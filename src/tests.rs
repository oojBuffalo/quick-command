use super::*;
use crossterm::event::{KeyCode, KeyModifiers};

fn cfg(backend: &str) -> Config {
    Config {
        backend: backend.into(),
        model: None,
        command: None,
        base_url: DEFAULT_BASE_URL.into(),
        api_key: None,
    }
}

fn has_pair(argv: &[String], a: &str, b: &str) -> bool {
    argv.windows(2).any(|w| w[0] == a && w[1] == b)
}

#[test]
fn env_file_parsing() {
    let m = parse_env("# comment\nQC_BACKEND=claude\nexport QC_MODEL=\"haiku\"\n\nQC_COMMAND='llm -m x'\nnoequals\n");
    assert_eq!(m["QC_BACKEND"], "claude");
    assert_eq!(m["QC_MODEL"], "haiku");
    assert_eq!(m["QC_COMMAND"], "llm -m x");
    assert_eq!(m.len(), 3);
}

#[test]
fn config_env_overrides_file_and_defaults() {
    let file = parse_env("QC_BACKEND=codex\nOPENROUTER_API_KEY=filekey\n");
    let c = load_config(&file, &|k| (k == "QC_BACKEND").then(|| "grok".to_string()));
    assert_eq!(c.backend, "grok");
    assert_eq!(c.api_key.as_deref(), Some("filekey"));
    assert_eq!(c.base_url, DEFAULT_BASE_URL);
    let d = load_config(&HashMap::new(), &|_| None);
    assert_eq!(d.backend, "openrouter");
    assert!(d.api_key.is_none() && d.model.is_none());
}

#[test]
fn sanitize_accepts_one_clean_line() {
    assert_eq!(sanitize("git branch -a\n").unwrap(), "git branch -a");
    assert_eq!(sanitize("```bash\ngit branch -a\n```").unwrap(), "git branch -a");
    assert_eq!(sanitize("`git branch -a`").unwrap(), "git branch -a");
    assert_eq!(sanitize("$ ls -la").unwrap(), "ls -la");
    // backticks that are part of the command stay
    assert_eq!(sanitize("echo `date` `whoami`").unwrap(), "echo `date` `whoami`");
}

#[test]
fn sanitize_strips_control_characters() {
    // ESC, BEL, a C1 control (0x9b) and tab must never reach the terminal
    assert_eq!(sanitize("ls\x1b[31m -la\x07\u{9b}").unwrap(), "ls[31m -la");
    assert_eq!(sanitize("ls\t-la").unwrap(), "ls -la");
}

#[test]
fn sanitize_rejects_multiple_lines_and_empty() {
    assert!(sanitize("ls\nrm -rf /").is_err());
    assert!(sanitize("ls\rrm -rf /").is_err());
    assert!(sanitize("```\nls\npwd\n```").is_err());
    assert!(sanitize("  \n ").is_err());
    assert!(sanitize("\x1b\x07").is_err());
}

#[test]
fn presets_always_disable_tools() {
    let claude = preset(&cfg("claude"), "SYS").unwrap();
    assert!(has_pair(&claude.argv, "--tools", ""));
    assert!(has_pair(&claude.argv, "--system-prompt", "SYS"));
    assert!(has_pair(&claude.argv, "--model", "haiku"));
    assert_eq!(claude.wire, Wire::ClaudeJson);

    let grok = preset(&cfg("grok"), "SYS").unwrap();
    assert!(has_pair(&grok.argv, "--tools", ""));
    assert!(grok.argv.iter().any(|a| a == "--disable-web-search"));
    assert!(grok.argv.iter().any(|a| a == "{request}"));

    let codex = preset(&cfg("codex"), "SYS").unwrap();
    assert!(has_pair(&codex.argv, "--sandbox", "read-only"));
    assert_eq!(codex.wire, Wire::Plain);
}

#[test]
fn preset_model_override_and_custom_command() {
    let mut c = cfg("claude");
    c.model = Some("sonnet".into());
    assert!(has_pair(&preset(&c, "S").unwrap().argv, "--model", "sonnet"));

    let mut c = cfg("codex");
    c.model = Some("gpt-x".into());
    assert!(has_pair(&preset(&c, "S").unwrap().argv, "-m", "gpt-x"));

    let mut c = cfg("command");
    assert!(preset(&c, "S").is_err());
    c.command = Some("llm -m fast {prompt}".into());
    assert_eq!(preset(&c, "S").unwrap().argv, ["llm", "-m", "fast", "{prompt}"]);

    assert!(preset(&cfg("nope"), "S").is_err());
}

#[test]
fn fill_replaces_whole_tokens_only() {
    let argv: Vec<String> = ["x", "{prompt}", "{request}", "a{request}"].iter().map(|s| s.to_string()).collect();
    // a request that itself contains a placeholder must not be expanded again
    let out = fill(&argv, "FULL", "say {prompt}");
    assert_eq!(out, ["x", "FULL", "say {prompt}", "a{request}"]);
}

#[test]
fn claude_wire_format() {
    let line = payload(&cfg("claude"), Wire::ClaudeJson, "SYS", "list \"files\"");
    let v: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(v["message"]["content"], "list \"files\"");
    assert!(line.ends_with('\n'));

    let out = "{\"type\":\"system\"}\nnot json\n{\"type\":\"result\",\"is_error\":false,\"result\":\"ls -la\"}\n";
    assert_eq!(parse_output(Wire::ClaudeJson, out).unwrap(), "ls -la");
    let bad = "{\"type\":\"result\",\"is_error\":true,\"result\":\"Not logged in\"}";
    assert_eq!(parse_output(Wire::ClaudeJson, bad).unwrap_err(), "Not logged in");
    assert!(parse_output(Wire::ClaudeJson, "").is_err());
}

#[test]
fn claude_answer_is_taken_without_waiting_for_exit() {
    assert!(is_final(Wire::ClaudeJson, "{\"type\":\"result\",\"result\":\"ls\"}\n"));
    assert!(!is_final(Wire::ClaudeJson, "{\"type\":\"system\"}\n"));
    assert!(!is_final(Wire::ClaudeJson, "not json\n"));
    // plain backends are read to the end
    assert!(!is_final(Wire::Plain, "{\"type\":\"result\"}\n"));
}

#[test]
fn plain_wire_format() {
    let p = payload(&cfg("codex"), Wire::Plain, "SYS", "list files");
    assert!(p.starts_with("SYS") && p.contains("list files"));
    assert_eq!(parse_output(Wire::Plain, "ls\n").unwrap(), "ls\n");
}

#[test]
fn http_body_shape() {
    let b = http_body(&cfg("openrouter"), "SYS", "list files");
    assert_eq!(b["model"], DEFAULT_HTTP_MODEL);
    assert_eq!(b["messages"][0]["content"], "SYS");
    assert_eq!(b["messages"][1]["content"], "list files");
    assert_eq!(b["reasoning_effort"], "none");
    assert_eq!(b["provider"]["sort"], "latency");

    let mut c = cfg("openrouter");
    c.base_url = "http://localhost:11434/v1".into();
    c.model = Some("qwen".into());
    let b = http_body(&c, "SYS", "x");
    assert_eq!(b["model"], "qwen");
    // OpenRouter-only fields are not sent to other servers
    assert!(b.get("provider").is_none() && b.get("reasoning_effort").is_none());
}

#[test]
fn openrouter_preset_goes_through_curl_stdin() {
    // no key: refuse before spawning anything
    assert!(preset(&cfg("openrouter"), "S").is_err());
    let mut c = cfg("openrouter");
    c.api_key = Some("sk-secret".into());
    let p = preset(&c, "S").unwrap();
    assert_eq!(p.wire, Wire::Http);
    assert_eq!(p.argv[0], "curl");
    // the key must never appear on the command line (visible in `ps`)
    assert!(!p.argv.iter().any(|a| a.contains("sk-secret")));

    let conf = curl_config(&c, "SYS", "say \"hi\"\\now");
    assert!(conf.contains("url = \"https://openrouter.ai/api/v1/chat/completions\"\n"));
    assert!(conf.contains("header = \"Authorization: Bearer sk-secret\"\n"));
    // data line round-trips through curl's quoting back to the JSON body
    let data = conf.lines().find_map(|l| l.strip_prefix("data = ")).unwrap();
    let unquoted = data[1..data.len() - 1].replace("\\\"", "\"").replace("\\\\", "\\");
    assert_eq!(serde_json::from_str::<Value>(&unquoted).unwrap(), http_body(&c, "SYS", "say \"hi\"\\now"));

    // a local server needs no key and gets no Authorization header
    let mut l = cfg("openrouter");
    l.base_url = "http://localhost:11434/v1/".into();
    assert!(preset(&l, "S").is_ok());
    let conf = curl_config(&l, "SYS", "x");
    assert!(conf.contains("url = \"http://localhost:11434/v1/chat/completions\"\n"));
    assert!(!conf.contains("Authorization"));
}

#[test]
fn http_response_parsing() {
    let ok = r#"{"choices":[{"message":{"role":"assistant","content":"git branch -a"}}]}"#;
    assert_eq!(parse_http(ok).unwrap(), "git branch -a");
    let err = r#"{"error":{"message":"No auth credentials found","code":401}}"#;
    assert_eq!(parse_http(err).unwrap_err(), "No auth credentials found");
    assert!(parse_http("<html>").is_err());
    assert!(parse_http("{}").is_err());
}

#[test]
fn system_prompt_carries_context() {
    let p = system_prompt("macos", "zsh", Some("/tmp/proj"), Some("main"));
    assert!(p.contains("macOS") && p.contains("BSD"));
    assert!(p.contains("zsh") && p.contains("/tmp/proj") && p.contains("main"));
    let q = system_prompt("linux", "bash", None, None);
    assert!(q.contains("Linux") && q.contains("not a git repository"));
}

#[test]
fn line_editing() {
    let mut s: Vec<char> = Vec::new();
    let mut cur = 0;
    for c in "list  git branches".chars() {
        assert_eq!(edit(&mut s, &mut cur, KeyCode::Char(c), KeyModifiers::NONE), Action::None);
    }
    edit(&mut s, &mut cur, KeyCode::Char('w'), KeyModifiers::CONTROL);
    assert_eq!(s.iter().collect::<String>(), "list  git ");
    edit(&mut s, &mut cur, KeyCode::Backspace, KeyModifiers::NONE);
    edit(&mut s, &mut cur, KeyCode::Home, KeyModifiers::NONE);
    edit(&mut s, &mut cur, KeyCode::Char('X'), KeyModifiers::SHIFT);
    assert_eq!(s.iter().collect::<String>(), "Xlist  git");
    assert_eq!(cur, 1);
    edit(&mut s, &mut cur, KeyCode::Char('u'), KeyModifiers::CONTROL);
    assert_eq!(s.iter().collect::<String>(), "list  git");
    assert_eq!(edit(&mut s, &mut cur, KeyCode::Enter, KeyModifiers::NONE), Action::Submit);
    assert_eq!(edit(&mut s, &mut cur, KeyCode::Esc, KeyModifiers::NONE), Action::Quit);
    assert_eq!(edit(&mut s, &mut cur, KeyCode::Char('c'), KeyModifiers::CONTROL), Action::Quit);
}
