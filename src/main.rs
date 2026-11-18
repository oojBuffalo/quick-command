//! quick-command: natural language to shell command popup for Herdr.
//!
//! Every backend is "spawn a process, write the request to its stdin (or argv),
//! read the answer from stdout": the subscription CLIs directly, HTTP via curl.

use std::collections::HashMap;
use std::env;
use std::fs::{self, DirBuilder, File};
use std::os::unix::fs::DirBuilderExt;
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{cursor, execute, queue};
use serde_json::{json, Value};

#[cfg(test)]
mod tests;

const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
const DEFAULT_HTTP_MODEL: &str = "anthropic/claude-haiku-5.5";
const TIMEOUT: Duration = Duration::from_secs(20);
// used when the `herdr` plugin host does not hand us its own folders
const STATE_DIR: &str = ".local/state/quick-command";
const CONFIG_DIR: &str = ".config/quick-command";

// ---------------------------------------------------------------- config

#[derive(Clone)]
struct Config {
    backend: String,
    model: Option<String>,
    command: Option<String>,
    base_url: String,
    api_key: Option<String>,
}

/// Parses `KEY=VALUE` lines; `#` comments, `export ` prefixes and quotes are allowed.
fn parse_env(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let line = line.strip_prefix("export ").unwrap_or(line);
            if line.starts_with('#') {
                return None;
            }
            let (k, v) = line.split_once('=')?;
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(v);
            Some((k.trim().to_string(), v.to_string()))
        })
        .collect()
}

/// Environment wins over the config file; empty values count as unset.
fn load_config(file: &HashMap<String, String>, env: &dyn Fn(&str) -> Option<String>) -> Config {
    let get = |k: &str| env(k).or_else(|| file.get(k).cloned()).filter(|v| !v.is_empty());
    Config {
        backend: get("QC_BACKEND").unwrap_or_else(|| "claude".into()),
        model: get("QC_MODEL"),
        command: get("QC_COMMAND"),
        base_url: get("QC_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.into()),
        api_key: get("QC_API_KEY").or_else(|| get("OPENROUTER_API_KEY")),
    }
}

fn config() -> Config {
    let file = dir_or_home(env::var("HERDR_PLUGIN_CONFIG_DIR").ok(), env::var("HOME").ok(), CONFIG_DIR)
        .and_then(|d| fs::read_to_string(d.join("config.env")).ok())
        .map(|t| parse_env(&t))
        .unwrap_or_default();
    load_config(&file, &|k| env::var(k).ok())
}

// ---------------------------------------------------------------- prompt

fn system_prompt(os: &str, shell: &str, cwd: Option<&str>, branch: Option<&str>) -> String {
    let os = match os {
        "macos" => "macOS (BSD userland, not GNU)",
        "linux" => "Linux (GNU userland)",
        other => other,
    };
    let git = match branch {
        Some(b) => format!("git repository on branch {b}"),
        None => "not a git repository".to_string(),
    };
    format!(
        "You translate a request into one shell command.\n\
         Environment: {os}; shell: {shell}; working directory: {cwd}; {git}.\n\
         Rules:\n\
         - Reply with exactly one line that contains only the command. No markdown, no backticks, no explanation.\n\
         - Use only tools and flags that exist on this operating system.\n\
         - Join several steps with && or pipes so the reply stays on one line.\n\
         - If the request cannot be done with a command, reply with one line that starts with \"# \" and says why.\n\
         Examples:\n\
         list git branches -> git branch -a\n\
         show disk usage of this folder -> du -sh .\n\
         find files larger than 100MB -> find . -type f -size +100M",
        cwd = cwd.unwrap_or("unknown"),
    )
}

fn build_system(cwd: Option<&str>) -> String {
    let shell = env::var("SHELL").ok();
    let shell = shell.as_deref().and_then(|s| s.rsplit('/').next()).unwrap_or("sh");
    let branch = cwd.and_then(|dir| {
        let out = Command::new("git")
            .args(["-C", dir, "rev-parse", "--abbrev-ref", "HEAD"])
            .stderr(Stdio::null())
            .output()
            .ok()?;
        let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && !name.is_empty()).then_some(name)
    });
    system_prompt(env::consts::OS, shell, cwd, branch.as_deref())
}

// ---------------------------------------------------------------- output safety

/// Characters that act on the terminal or change how text is shown without being seen:
/// controls, zero-width characters, and the bidi overrides that can reorder a line.
fn is_invisible(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{ad}' | '\u{61c}' | '\u{180e}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}' | '\u{feff}' | '\u{fff9}'..='\u{fffb}' | '\u{e0000}'..='\u{e007f}')
}

/// Text with every kind of whitespace turned into a plain space and invisible characters removed.
fn plain(text: &str) -> String {
    text.chars().map(|c| if c.is_whitespace() { ' ' } else { c }).filter(|c| !is_invisible(*c)).collect()
}

/// Turns a model reply into one line that is safe to type into a terminal.
///
/// This is the trust boundary: the result becomes keystrokes. Control characters
/// (Enter, Escape, ...) are removed, and anything but a single line is refused.
fn sanitize(raw: &str) -> Result<String, String> {
    let all: Vec<&str> = raw.split(['\n', '\r', '\u{85}', '\u{2028}', '\u{2029}']).map(str::trim).filter(|l| !l.is_empty()).collect();
    let lines: &[&str] = match all.iter().position(|l| l.starts_with("```")) {
        Some(open) => {
            let rest = &all[open + 1..];
            let close = rest.iter().position(|l| l.starts_with("```")).unwrap_or(rest.len());
            &rest[..close]
        }
        None => &all,
    };
    let line = match lines {
        [] => return Err("the model returned nothing".into()),
        [one] => *one,
        _ => return Err("the model returned more than one line".into()),
    };
    let line = line.strip_prefix("$ ").unwrap_or(line);
    let wrapped = line.len() > 1 && line.starts_with('`') && line.ends_with('`') && line.matches('`').count() == 2;
    let line = if wrapped { &line[1..line.len() - 1] } else { line };
    let clean = plain(line);
    let clean = clean.trim();
    if clean.is_empty() {
        return Err("the model returned nothing".into());
    }
    Ok(clean.to_string())
}

// ---------------------------------------------------------------- backends

#[derive(Debug, PartialEq, Clone, Copy)]
enum Wire {
    /// prompt text in, answer text out
    Plain,
    /// Claude Code stream-json in and out (lets the process start before the request exists)
    ClaudeJson,
    /// curl config in (URL, headers, body), chat-completion JSON out
    Http,
}

struct Preset {
    argv: Vec<String>,
    wire: Wire,
}

/// Command line for the chosen backend. Every agent CLI is started with its tools off,
/// so a request can only ever produce text.
fn preset(cfg: &Config, system: &str) -> Result<Preset, String> {
    let model = cfg.model.as_deref();
    let (mut argv, wire): (Vec<&str>, Wire) = match cfg.backend.as_str() {
        "claude" => (
            vec![
                "claude", "-p", "--model", model.unwrap_or("haiku"),
                "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
                "--tools", "", "--strict-mcp-config", "--setting-sources", "",
                "--disable-slash-commands", "--no-session-persistence", "--system-prompt", system,
            ],
            Wire::ClaudeJson,
        ),
        "codex" => (
            vec![
                "codex", "exec", "--ephemeral", "--skip-git-repo-check",
                "--sandbox", "read-only", "-c", "model_reasoning_effort=low",
            ],
            Wire::Plain,
        ),
        "grok" => (
            vec![
                "grok", "-p", "{request}", "--output-format", "plain", "--tools", "",
                "--disable-web-search", "--max-turns", "1", "--effort", "low",
                "--system-prompt-override", system,
            ],
            Wire::Plain,
        ),
        "openrouter" => {
            if cfg.api_key.is_none() && cfg.base_url.contains("openrouter.ai") {
                return Err("no API key: set OPENROUTER_API_KEY or QC_API_KEY".into());
            }
            // URL, key and body go in on stdin so the key never shows up in `ps`
            // -q (must be first): ignore ~/.curlrc
            (vec!["curl", "-q", "-sS", "--max-time", "20", "-K", "-"], Wire::Http)
        }
        "command" => {
            let cmd = cfg.command.as_deref().ok_or("QC_BACKEND=command needs QC_COMMAND")?;
            (cmd.split_whitespace().collect(), Wire::Plain)
        }
        other => return Err(format!("unknown QC_BACKEND \"{other}\" (claude, codex, grok, openrouter, command)")),
    };
    match (cfg.backend.as_str(), model) {
        ("codex", Some(m)) | ("grok", Some(m)) => argv.extend(["-m", m]),
        _ => {}
    }
    if cfg.backend == "codex" {
        argv.push("-"); // read the prompt from stdin
    }
    if argv.is_empty() {
        return Err("QC_COMMAND is empty".into());
    }
    Ok(Preset { argv: argv.into_iter().map(String::from).collect(), wire })
}

/// Replaces the whole-argument placeholders `{prompt}` (rules + request) and `{request}`.
fn fill(argv: &[String], full: &str, request: &str) -> Vec<String> {
    // a leading space keeps text that starts with '-' from being read as a flag
    let value = |s: &str| if s.starts_with('-') { format!(" {s}") } else { s.to_string() };
    argv.iter()
        .map(|a| match a.as_str() {
            "{prompt}" => value(full),
            "{request}" => value(request),
            _ => a.clone(),
        })
        .collect()
}

fn takes_request_in_argv(argv: &[String]) -> bool {
    argv.iter().any(|a| a == "{prompt}" || a == "{request}")
}

fn full_prompt(system: &str, request: &str) -> String {
    format!("{system}\n\nRequest: {request}\n")
}

fn http_body(cfg: &Config, system: &str, request: &str) -> Value {
    let mut body = json!({
        "model": cfg.model.as_deref().unwrap_or(DEFAULT_HTTP_MODEL),
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": request},
        ],
        "max_tokens": 512,
    });
    if cfg.base_url.contains("openrouter.ai") {
        body["reasoning_effort"] = json!("none");
        body["provider"] = json!({"sort": "latency"});
    }
    body
}

/// curl config file (read with `-K -`): quoted values use `\\` and `\"` escapes.
fn curl_config(cfg: &Config, system: &str, request: &str) -> String {
    let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let mut conf = format!("url = {}\nheader = {}\n", quote(&url), quote("Content-Type: application/json"));
    if let Some(key) = &cfg.api_key {
        conf += &format!("header = {}\n", quote(&format!("Authorization: Bearer {key}")));
    }
    conf + &format!("data = {}\n", quote(&http_body(cfg, system, request).to_string()))
}

/// What is written to the backend's stdin.
fn payload(cfg: &Config, wire: Wire, system: &str, request: &str) -> String {
    match wire {
        Wire::Plain => full_prompt(system, request),
        Wire::ClaudeJson => json!({"type": "user", "message": {"role": "user", "content": request}}).to_string() + "\n",
        Wire::Http => curl_config(cfg, system, request),
    }
}

fn parse_http(text: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(text).map_err(|_| "unreadable response from the API".to_string())?;
    if let Some(msg) = v["error"]["message"].as_str() {
        return Err(msg.to_string());
    }
    v["choices"][0]["message"]["content"].as_str().map(String::from).ok_or_else(|| "the API returned no answer".into())
}

/// Extracts the answer text from the backend's stdout.
fn parse_output(wire: Wire, out: &str) -> Result<String, String> {
    match wire {
        Wire::Plain => Ok(out.to_string()),
        Wire::Http => parse_http(out),
        Wire::ClaudeJson => {
            let result = out
                .lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .find(|v| v["type"] == "result")
                .ok_or("no result from claude")?;
            let text = result["result"].as_str().unwrap_or_default().to_string();
            if result["is_error"] == true { Err(text) } else { Ok(text) }
        }
    }
}

/// True when `line` already holds the whole answer, so there is no need to wait for
/// the process to exit (Claude Code takes about half a second to shut down).
fn is_final(wire: Wire, line: &str) -> bool {
    wire == Wire::ClaudeJson && serde_json::from_str::<Value>(line).map_or(false, |v| v["type"] == "result")
}

/// Where backends run and their stderr is logged: Herdr's per-plugin state directory,
/// else a private one under the home directory. Never a shared temp directory, where
/// another user could plant a link at the log's path or files for an agent CLI to read.
fn dir_or_home(herdr: Option<String>, home: Option<String>, under_home: &str) -> Option<PathBuf> {
    let set = |v: Option<String>| v.filter(|s| !s.is_empty()).map(PathBuf::from);
    set(herdr).or_else(|| set(home).map(|h| h.join(under_home)))
}

fn workdir() -> Result<PathBuf, String> {
    let dir = dir_or_home(env::var("HERDR_PLUGIN_STATE_DIR").ok(), env::var("HOME").ok(), STATE_DIR)
        .ok_or("no state directory: HOME is not set")?;
    DirBuilder::new().recursive(true).mode(0o700).create(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    Ok(dir)
}

/// One request against one backend process.
struct Runner {
    cfg: Config,
    system: String,
    preset: Preset,
    workdir: PathBuf,
    child: Option<Child>,
}

impl Runner {
    /// Builds the command and, when the request is delivered on stdin, starts the
    /// process right away so its startup overlaps with the user typing.
    fn new(cfg: Config, system: String) -> Result<Runner, String> {
        Runner::in_dir(cfg, system, workdir()?)
    }

    fn in_dir(cfg: Config, system: String, workdir: PathBuf) -> Result<Runner, String> {
        let preset = preset(&cfg, &system)?;
        let mut runner = Runner { cfg, system, preset, workdir, child: None };
        if !takes_request_in_argv(&runner.preset.argv) {
            runner.child = Some(runner.spawn("")?);
        }
        Ok(runner)
    }

    fn stderr_log(&self) -> PathBuf {
        self.workdir.join("last-stderr.log")
    }

    fn spawn(&self, request: &str) -> Result<Child, String> {
        let argv = fill(&self.preset.argv, &full_prompt(&self.system, request), request);
        let stdin = if takes_request_in_argv(&self.preset.argv) { Stdio::null() } else { Stdio::piped() };
        let stderr = File::create(self.stderr_log()).map(Stdio::from).unwrap_or_else(|_| Stdio::null());
        Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(&self.workdir)
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", argv[0]))
    }

    /// Hands the request to the process; the returned channel yields its stdout.
    fn start(&mut self, request: &str) -> Result<Receiver<String>, String> {
        let alive = matches!(self.child.as_mut().map(|c| c.try_wait()), Some(Ok(None)));
        if !alive {
            self.kill();
            self.child = Some(self.spawn(request)?);
        }
        let text = payload(&self.cfg, self.preset.wire, &self.system, request);
        let child = self.child.as_mut().expect("spawned above");
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes()).map_err(|e| format!("cannot send the request: {e}"))?;
        } // dropped here: closing stdin tells the process the request is complete
        let mut stdout = BufReader::new(child.stdout.take().ok_or("backend has no output")?);
        let wire = self.preset.wire;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut out = String::new();
            loop {
                let seen = out.len();
                match stdout.read_line(&mut out) {
                    Ok(n) if n > 0 && !is_final(wire, &out[seen..]) => {}
                    _ => break, // end of output, a read error, or the answer line
                }
            }
            let _ = tx.send(out);
        });
        Ok(rx)
    }

    /// Returns the sanitized command and stops the process.
    fn finish(&mut self, raw: &str) -> Result<String, String> {
        let answer = parse_output(self.preset.wire, raw).and_then(|t| sanitize(&t));
        if answer.is_ok() {
            self.kill(); // the answer is in; no reason to wait for a slow shutdown
            return answer;
        }
        let ok = self.child.take().map(|mut c| c.wait().map(|s| s.success()).unwrap_or(false)).unwrap_or(false);
        if ok {
            return answer;
        }
        let log = fs::read_to_string(self.stderr_log()).unwrap_or_default();
        let reason = log.lines().rev().find(|l| !l.trim().is_empty()).map(String::from);
        Err(format!("{} failed: {}", self.preset.argv[0], reason.unwrap_or_else(|| answer.unwrap_err())))
    }

    // ponytail: kills the direct child only. A wrapper's own child (codex runs under
    // node) may finish its request in the background. Kill the process group if that matters.
    fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// No exit path, including an early `?` return, may leave a backend process running.
impl Drop for Runner {
    fn drop(&mut self) {
        self.kill();
    }
}

// ---------------------------------------------------------------- herdr

struct Target {
    pane: Option<String>,
    cwd: Option<String>,
}

/// The pane underneath the popup, from Herdr's context JSON. `QC_TARGET_PANE` and
/// `QC_TARGET_CWD` override it, which is how the box is tested as an ordinary pane.
fn target() -> Target {
    let ctx: Value = env::var("HERDR_PLUGIN_CONTEXT_JSON").ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
    let pick = |var: &str, key: &str| {
        env::var(var).ok().filter(|v| !v.is_empty()).or_else(|| ctx[key].as_str().map(String::from))
    };
    Target { pane: pick("QC_TARGET_PANE", "focused_pane_id"), cwd: pick("QC_TARGET_CWD", "focused_pane_cwd") }
}

/// Types the command at the pane's prompt without submitting it.
fn insert(pane: &str, command: &str) -> Result<(), String> {
    let herdr = env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into());
    let out = Command::new(herdr).args(["pane", "send-text", pane, command]).output().map_err(|e| format!("cannot run herdr: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("herdr could not type the command: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

// ---------------------------------------------------------------- input box

#[derive(Debug, PartialEq)]
enum Action {
    None,
    Submit,
    Quit,
}

fn is_quit(code: KeyCode, mods: KeyModifiers) -> bool {
    code == KeyCode::Esc || (code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL))
}

/// Applies one key press to the input line.
fn edit(input: &mut Vec<char>, cursor: &mut usize, code: KeyCode, mods: KeyModifiers) -> Action {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    match code {
        _ if is_quit(code, mods) => return Action::Quit,
        KeyCode::Enter => return Action::Submit,
        KeyCode::Home => *cursor = 0,
        KeyCode::Char('a') if ctrl => *cursor = 0,
        KeyCode::End => *cursor = input.len(),
        KeyCode::Char('e') if ctrl => *cursor = input.len(),
        KeyCode::Char('u') if ctrl => {
            input.drain(..*cursor);
            *cursor = 0;
        }
        KeyCode::Char('w') if ctrl => {
            let mut i = *cursor;
            while i > 0 && input[i - 1] == ' ' {
                i -= 1;
            }
            while i > 0 && input[i - 1] != ' ' {
                i -= 1;
            }
            input.drain(i..*cursor);
            *cursor = i;
        }
        KeyCode::Left => *cursor = cursor.saturating_sub(1),
        KeyCode::Right => *cursor = (*cursor + 1).min(input.len()),
        KeyCode::Backspace if *cursor > 0 => {
            *cursor -= 1;
            input.remove(*cursor);
        }
        KeyCode::Delete if *cursor < input.len() => {
            input.remove(*cursor);
        }
        KeyCode::Char(c) if !ctrl && !mods.contains(KeyModifiers::ALT) => {
            input.insert(*cursor, c);
            *cursor += 1;
        }
        _ => {}
    }
    Action::None
}

fn clip(text: &str, width: usize) -> String {
    text.chars().filter(|c| !c.is_control()).take(width).collect()
}

// ponytail: assumes one terminal cell per character; wide (CJK, emoji) input will misplace the cursor.
fn draw(out: &mut impl Write, input: &[char], cursor: usize, error: Option<&str>, waiting: Option<Instant>) -> io::Result<()> {
    let (w, h) = terminal::size().unwrap_or((60, 2));
    let width = (w as usize).max(8);
    let status = match (waiting, error) {
        (Some(since), _) => {
            let frames: Vec<char> = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".chars().collect();
            let frame = frames[(since.elapsed().as_millis() / 80) as usize % frames.len()];
            Some((Color::Cyan, format!("{frame} generating…  esc cancels")))
        }
        (None, Some(e)) => Some((Color::Red, e.to_string())),
        (None, None) => None,
    };
    queue!(out, cursor::MoveTo(0, 0), Clear(ClearType::All))?;
    // with a single row, a status message takes the place of the input line
    let input_visible = h >= 2 || status.is_none();
    let room = width - 3; // "❯ " plus one spare cell for the cursor
    let start = cursor.saturating_sub(room);
    if input_visible {
        queue!(out, SetForegroundColor(Color::Cyan), Print("❯ "), ResetColor)?;
        if input.is_empty() {
            queue!(out, SetAttribute(Attribute::Dim), Print(clip("Command instructions", room)), SetAttribute(Attribute::Reset))?;
        } else {
            queue!(out, Print(input.iter().skip(start).take(room).collect::<String>()))?;
        }
    }
    let status_row = if h >= 2 { 1 } else { 0 };
    match status {
        Some((color, text)) => queue!(out, cursor::MoveTo(0, status_row), SetForegroundColor(color), Print(clip(&text, width - 1)), ResetColor)?,
        None if h >= 2 => queue!(out, cursor::MoveTo(0, 1), SetAttribute(Attribute::Dim), Print(clip("↵ send · esc close", width - 1)), SetAttribute(Attribute::Reset))?,
        None => {}
    }
    if waiting.is_none() && input_visible {
        queue!(out, cursor::MoveTo((2 + cursor - start) as u16, 0), cursor::Show)?;
    } else {
        queue!(out, cursor::Hide)?;
    }
    out.flush()
}

/// Runs the box. `Ok(Some(cmd))` means "print it" (no pane to type into).
fn ui(out: &mut impl Write, tgt: &Target) -> io::Result<Option<String>> {
    let (mut input, mut cursor): (Vec<char>, usize) = (Vec::new(), 0);
    let mut error: Option<String> = None;
    let mut waiting: Option<(Receiver<String>, Instant)> = None;

    // draw first so the box is there at once; context and backend start-up come after
    draw(out, &input, cursor, None, None)?;
    let cfg = config();
    let system = build_system(tgt.cwd.as_deref());
    let mut runner = match Runner::new(cfg.clone(), system.clone()) {
        Ok(r) => Some(r),
        Err(e) => {
            error = Some(e);
            None
        }
    };
    let mut dirty = true;
    loop {
        if dirty || waiting.is_some() {
            draw(out, &input, cursor, error.as_deref(), waiting.as_ref().map(|w| w.1))?;
            dirty = false;
        }
        let done = waiting.as_ref().and_then(|(rx, since)| match rx.try_recv() {
            Ok(raw) => Some(runner.as_mut().map_or(Err("backend is gone".to_string()), |r| r.finish(&raw))),
            Err(TryRecvError::Disconnected) => Some(Err("backend stopped unexpectedly".to_string())),
            Err(TryRecvError::Empty) if since.elapsed() > TIMEOUT => Some(Err("timed out".to_string())),
            Err(TryRecvError::Empty) => None,
        });
        if let Some(result) = done {
            waiting = None;
            if let Some(mut r) = runner.take() {
                r.kill();
            }
            let typed = result.and_then(|cmd| match &tgt.pane {
                Some(pane) => insert(pane, &cmd).map(|_| None),
                None => Ok(Some(cmd)),
            });
            match typed {
                Ok(print) => return Ok(print),
                Err(e) => {
                    error = Some(e);
                    runner = Runner::new(cfg.clone(), system.clone()).ok(); // ready for a retry
                    dirty = true;
                    continue;
                }
            }
        }
        if !event::poll(Duration::from_millis(if waiting.is_some() { 80 } else { 250 }))? {
            continue;
        }
        dirty = true;
        let action = match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press && waiting.is_some() => {
                if is_quit(k.code, k.modifiers) { Action::Quit } else { Action::None }
            }
            Event::Key(k) if k.kind == KeyEventKind::Press => {
                error = None;
                edit(&mut input, &mut cursor, k.code, k.modifiers)
            }
            Event::Paste(text) if waiting.is_none() => {
                for c in plain(&text).chars() {
                    input.insert(cursor, c);
                    cursor += 1;
                }
                Action::None
            }
            _ => Action::None,
        };
        match action {
            Action::None => {}
            Action::Quit => {
                if let Some(mut r) = runner.take() {
                    r.kill();
                }
                return Ok(None);
            }
            Action::Submit => {
                let request: String = input.iter().collect();
                let request = request.trim();
                if request.is_empty() {
                    continue;
                }
                if runner.is_none() {
                    match Runner::new(cfg.clone(), system.clone()) {
                        Ok(r) => runner = Some(r),
                        Err(e) => {
                            error = Some(e);
                            continue;
                        }
                    }
                }
                match runner.as_mut().expect("set above").start(request) {
                    Ok(rx) => waiting = Some((rx, Instant::now())),
                    Err(e) => {
                        error = Some(e);
                        if let Some(mut r) = runner.take() {
                            r.kill();
                        }
                    }
                }
            }
        }
    }
}

fn popup() -> i32 {
    let tgt = target();
    let mut out = io::stdout();
    if terminal::enable_raw_mode().is_err() {
        eprintln!("quick-command: needs a terminal (or use --once \"<instruction>\")");
        return 1;
    }
    let _ = execute!(out, EnableBracketedPaste);
    let result = ui(&mut out, &tgt);
    let _ = execute!(out, DisableBracketedPaste, ResetColor, cursor::Show, cursor::MoveTo(0, 0), Clear(ClearType::All));
    let _ = terminal::disable_raw_mode();
    match result {
        Ok(Some(command)) => {
            println!("{command}");
            0
        }
        Ok(None) => 0,
        Err(e) => {
            eprintln!("quick-command: {e}");
            1
        }
    }
}

// ---------------------------------------------------------------- --once

/// One request without the box: prints the command, and the time from "Enter" to
/// answer on stderr. This is what the desktop-app mod runs. `QC_TYPING_MS` (default 0)
/// stands in for the user typing while the backend warms up, to time the popup's early start.
fn once(request: &str) -> Result<(String, u128), String> {
    let cwd = env::current_dir().ok().map(|p| p.display().to_string());
    let mut runner = Runner::new(config(), build_system(cwd.as_deref()))?;
    let typing = env::var("QC_TYPING_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    thread::sleep(Duration::from_millis(typing));
    let entered = Instant::now();
    let rx = runner.start(request)?;
    let raw = rx.recv_timeout(TIMEOUT).map_err(|_| {
        runner.kill();
        "timed out".to_string()
    })?;
    let command = runner.finish(&raw)?;
    Ok((command, entered.elapsed().as_millis()))
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        None => popup(),
        Some("--once") if args.len() > 1 => match once(&args[1..].join(" ")) {
            Ok((command, ms)) => {
                println!("{command}");
                eprintln!("{ms} ms");
                0
            }
            Err(e) => {
                eprintln!("error: {e}");
                1
            }
        },
        _ => {
            eprintln!("usage: quick-command [--once <instruction>]");
            2
        }
    };
    std::process::exit(code);
}
