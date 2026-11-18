# Quick Command

A Herdr extension that turns plain language into a shell command, like ⌘K in Cursor's terminal. It works in the Herdr desktop app and in the `herdr` terminal multiplexer.

Press the key, type `list git branches`, press Enter. `git branch -a` appears at your shell prompt. It is typed, not run: you read it, edit it if needed, and press Enter yourself.

```
❯ list git branches
↵ send · esc close
```

## Install

Needs a Rust toolchain and the `claude` CLI (or another backend, below; the HTTP backend needs `curl`). There are two Herdr programs with different extension systems; install for the one you use.

```bash
git clone <this repo> quick-command && cd quick-command
cargo build --release
```

### Herdr desktop app (a mod)

```bash
mkdir ~/.config/herdr-app/mods/quick-command
cp mod/mod.json mod/main.js target/release/quick-command ~/.config/herdr-app/mods/quick-command/
herdr-app check ~/.config/herdr-app/mods/quick-command
```

Add the key to the `terminal` section of `~/.config/herdr-app/keys.json` (the app picks it up on save):

```json
"cmd-k": "mod.open:quick-command"
```

Once you edit `keys.json` the app treats the whole file as yours, so shortcuts added by later Herdr releases must be added by hand.

The mod pastes the command through the clipboard, so the command replaces whatever you had copied. It also starts the backend only when you press Enter, so it is a little slower than the terminal plugin below.

### `herdr` terminal multiplexer (a plugin)

Needs `herdr` 0.9.5 or later.

```bash
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

Settings live in `config.env`: in `~/.config/quick-command/` for the desktop app, or in the folder printed by `herdr plugin config-dir ooj.quick-command` for the terminal plugin. Environment variables with the same names override the file.

```
QC_BACKEND=openrouter
```

| `QC_BACKEND` | Uses | Notes |
|---|---|---|
| `claude` (default) | Your Claude Code subscription | Default model `haiku`. Works with no setup if `claude` is logged in. |
| `openrouter` | An API key, over HTTP | Fastest. Needs `OPENROUTER_API_KEY` or `QC_API_KEY`. Default model `anthropic/claude-haiku-5.5`. |
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

The command goes to stdout and the elapsed milliseconds to stderr. Set `QC_TYPING_MS=1500` to add pretend typing time during which the backend warms up, as it does in the terminal plugin's box.

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
