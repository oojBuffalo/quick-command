# Quick Command

A Herdr plugin that turns plain language into a shell command, like ⌘K in Cursor's terminal.

Press the key, type `list git branches`, press Enter. `git branch -a` appears at your shell prompt. It is typed, not run: you read it, edit it if needed, and press Enter yourself.

```
❯ list git branches
↵ send · esc close
```

## Install

Needs Herdr 0.9.5 or later, a Rust toolchain, and `curl`.

```bash
git clone <this repo> quick-command && cd quick-command
cargo build --release
herdr plugin link "$PWD"
```

Bind a key in `~/.config/herdr/config.toml`, then reload the config (`prefix+shift+r`):

```toml
[[keys.command]]
key = "cmd+k"
type = "plugin_action"
command = "ooj.quick-command.open"
description = "quick command"
```

Most macOS terminals keep ⌘K for themselves (clear screen). In Ghostty, release it so it reaches Herdr:

```
keybind = super+k=unbind
```

If your terminal cannot pass ⌘K through, pick another key, for example `key = "prefix+k"`.

## Choose a backend

Settings live in `config.env` inside the folder printed by `herdr plugin config-dir ooj.quick-command`. Environment variables with the same names override the file.

```
QC_BACKEND=claude
```

| `QC_BACKEND` | Uses | Notes |
|---|---|---|
| `openrouter` (default) | An API key, over HTTP | Fastest. Needs `OPENROUTER_API_KEY` or `QC_API_KEY`. Default model `anthropic/claude-haiku-5.5`. |
| `claude` | Your Claude Code subscription | Default model `haiku`. |
| `codex` | Your ChatGPT/Codex subscription | |
| `grok` | Your Grok subscription | |
| `command` | Any program you name in `QC_COMMAND` | See below. |

Other settings:

| Setting | Meaning |
|---|---|
| `QC_MODEL` | Model name, passed to whichever backend is selected. |
| `QC_BASE_URL` | For `openrouter`: any OpenAI-compatible endpoint, such as `https://api.openai.com/v1` or a local Ollama at `http://localhost:11434/v1`. No key is required for non-OpenRouter URLs. |
| `QC_API_KEY` | API key for `openrouter`. Falls back to `OPENROUTER_API_KEY`. |
| `QC_COMMAND` | For `command`: the program and its arguments, split on spaces. |

### Custom command

The program receives the rules plus your request on stdin and must print one command on stdout:

```
QC_BACKEND=command
QC_COMMAND=llm -m some-fast-model
```

If the program wants the text as an argument instead, use `{prompt}` (rules plus request) or `{request}` (request only) as a whole argument:

```
QC_COMMAND=gemini -p {prompt}
```

## Speed

Time from pressing Enter to the command appearing, measured on one Mac on 2026-11-11 with a handful of runs each. Treat these as rough.

| Backend | Enter to command |
|---|---|
| `openrouter`, free `nvidia/nemotron-3.5-lightning:free` | about 0.5–1 s (two runs) |
| `claude` (Haiku) | about 1–4 s, typically about 2.3 s |
| `grok` | about 7–10 s |
| `codex` | about 11 s |

The subscription CLIs are full agent programs, and nearly all of their time is the model's own response time on that route. Two things are done to keep them as quick as they can be: the CLI is started the moment the box opens, so its start-up overlaps with your typing, and the answer is taken as soon as it arrives rather than waiting for the CLI to shut down.

Time your own setup without Herdr:

```bash
QC_BACKEND=claude target/release/quick-command --once "list git branches"
```

The command goes to stdout and the elapsed milliseconds to stderr. `QC_TYPING_MS` (default 1500) is the pretend typing time during which the backend warms up.

## What is sent to the model

Your request, plus: operating system, shell name, the pane's working directory, and the git branch if there is one. Nothing from the screen or scrollback is sent.

## Safety

- The command is typed at the prompt and never run for you.
- Model output is reduced to a single line with every control character removed, so a reply cannot press Enter or send escape sequences. Zero-width and text-direction characters are removed too, so the line you read is the line the shell gets. A reply with more than one line is refused.
- The agent CLIs are started with their tools switched off (`claude --tools ""`, `grok --tools "" --disable-web-search`, `codex --sandbox read-only`) and in an empty folder, so a request can only produce text.
- The API key is handed to `curl` on stdin, not on its command line.

## Limits

- One command line per request. Multi-line scripts are refused.
- Wide characters (CJK, emoji) in the input box misplace the cursor.
- Pressing Esc stops the backend's main process; a helper process it started (Codex runs under Node) may finish its request in the background.
- If the focused pane is not at a shell prompt, the text is still typed there.

## Development

```bash
cargo test
```
