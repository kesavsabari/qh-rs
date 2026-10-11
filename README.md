# qh

A coding agent for your terminal. It works with local models through Ollama or with cloud
models through an API key. It's a single 4 MB binary with nothing else to install.

It reads and edits your files, runs commands, browses the web, and can run several agents
side by side in tabs.

**Website: [kesavsabari.github.io/qh-rs](https://kesavsabari.github.io/qh-rs/) · Guide: [docs](https://kesavsabari.github.io/qh-rs/docs/)** (or run `qh docs`).

## Install

**Windows** (PowerShell):

```powershell
irm kesavsabari.github.io/qh | iex
```

**Linux / macOS**:

```sh
curl -fsSL kesavsabari.github.io/qh/sh | sh
```

Open a **new** terminal afterwards so `qh` is on your PATH.

**Updating.** qh looks for a new version once a day. When there is one, the bottom line says
`Update available! Run: qh update`. `qh update` (or `/update` inside qh) installs it. It only
installs releases signed with the qh release key, and never an older version. `qh --version`
shows what you run. `update_check = "off"` in config.toml stops the daily check. That check
fetches two small files from GitHub and sends nothing about you or your install. qh 0.1.3 and
older can't update themselves: run the install command again once.

## First run

Go to a project folder and type `qh`. On first start it asks how you want to run models:

- **Local (Ollama).** It's free and private, and it runs on your GPU. Install Ollama from
  <https://ollama.com/download> (on Windows: `winget install Ollama.Ollama`) and start it.
  Setup lists your models and can download `qwen3.5:9b`, the model qh is tuned on. That
  model needs about 7 GB of GPU memory.
- **Ollama cloud.** This needs the Ollama app but no big GPU and no API key: setup runs
  `ollama signin`, which opens your browser to log in to ollama.com (there's a free tier), then
  lets you pick a `…-cloud` model, which runs on their servers.
- **Cloud (API key).** No GPU is needed. Pick a provider, paste your key, and setup tests it
  and lets you search for a model. [OpenRouter](https://openrouter.ai/keys) is the easiest
  start: one key covers hundreds of models, and some of them are free.
  [Groq](https://console.groq.com/keys) has a free tier too.

You can run `qh setup` again at any time to change this.

## Using it

Type what you want in plain words:

```
fix the failing test in tests/test_api.py
add a --verbose flag to main.py
what does src/engine.rs do?
```

The agent asks before it edits a file or runs a command. Press **Shift+Tab** to change how
much it asks:

| mode | what it does |
|---|---|
| `ask` | asks before any change (default) |
| `auto-edit` | edits files freely, still asks before commands |
| `auto` | runs everything except dangerous commands (rm, sudo, git push…) |
| `plan` | read-only: it looks around and writes a plan, then you switch modes to let it act |

Useful keys and commands:

| | |
|---|---|
| `Esc` | stop the agent |
| `@file.py` | attach a file to your message |
| `/model` | switch model |
| `/undo` · `/diff` | undo the agent's file changes, or see them |
| `/new name` · `Ctrl+N` | open another agent in a new tab |
| `Alt+1..9` | switch tabs |
| `/compact` | shrink a long conversation |
| `/help` | everything else |
| `Ctrl+Q` | quit |

Run one task without the UI: `qh -p "explain this repo" -y`.

## Make it yours

All of these are optional.

- **Project instructions:** put a `QH.md` (or `AGENTS.md` / `CLAUDE.md`) in your project, and
  the agent follows it in every session there.
- **Your own commands:** `.qh/commands/review.md` becomes `/review`. In the file, `$ARGUMENTS`
  is replaced by whatever you type after the command.
- **Skills:** `.qh/skills/<name>/SKILL.md` holds instructions the agent loads only when a task
  needs them. Start the file with front matter containing `name:` and `description:`.
- **Config:** `config.toml` lives in `%APPDATA%\qwen-harness\` on Windows and
  `~/.config/qwen-harness/` elsewhere. Some examples:

```toml
mode = "auto-edit"

# run a command after every edit; exit code 2 from a pre_tool hook blocks the action
[[hooks]]
event = "post_tool"
match = "write_file|edit_file"
cmd = "cargo fmt"

# MCP servers: their tools become available to the agent
[mcp.github]
cmd = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_PERSONAL_ACCESS_TOKEN = "..." }
```

## Troubleshooting

- **"Ollama isn't answering":** start the Ollama app (or `ollama serve`), or run `qh setup`
  and pick a cloud model.
- **"rejected the API key":** run `qh login <provider>` to paste a new one.
- **Slow or forgetful:** your GPU may only fit a small context. `/tier` shows what was chosen,
  and a cloud model avoids the limit.
- **Windows commands:** qh uses Git Bash if it's installed, otherwise PowerShell.
