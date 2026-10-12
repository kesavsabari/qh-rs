# Changelog

Each version starts with a one-line summary; the website shows it once the version is no longer recent.

## 0.1.8 — 2026-10-12

Teammates connect from any network, cloud models get their full window, and edits show as diffs.

- **Teammates connect from any network.** When two machines can't reach each other directly
  (a phone hotspot, a strict home router), qh-link now goes through qh's own relay instead of
  falling back to git. Everything stays end-to-end encrypted; the relay only forwards packets.
  Set `relay` to use your own instead.
- **Settings live in `~/.config/qh`** (`%APPDATA%\qh` on Windows), and sessions in
  `~/.local/state/qh`. qh moves your old `qwen-harness` folders over on first start; logins,
  sessions and settings come along.
- **Cloud models get their whole window.** A cloud model runs on the provider's machines, so qh no
  longer caps it at 64k: it uses the model's full context window and still compacts at 88%. qh
  asks the provider for the window when it isn't known yet. Every request resends the
  conversation, so long sessions use more of your plan; set `cloud_ctx` to cap it again.
- **Edits show as diffs.** File edits appear in red and green as they happen, like Claude Code;
  Ctrl+O folds or unfolds the latest one. Other tool calls stay folded to one line.
- **See when it's thinking, and how hard.** The input box says `✻ thinking…` or `… working`.
  `/think low|medium|high` sets the effort on ChatGPT, OpenAI, OpenRouter, Anthropic and gpt-oss.
- **A cleaner screen.** The chat and input sit in bordered panes like the grid view; your
  messages have a bar down the side and long pastes fold after 6 lines; `/compact` updates the
  token count straight away.

## 0.1.7 — 2026-10-12

The safety update: secrets and anything outside the project ask first, and commands run in a sandbox on Linux and macOS.

- **Secrets ask first, in every mode.** Reading `~/.ssh`, `~/.aws`, `.env` files, keys, cloud and
  git credentials, browser profiles or qh's own logins asks you, even in `auto`. `grep` and the
  other search tools skip those files.
- **Writes stay in the project.** The file tools write freely in the project and temp. Anything
  else asks, even in `auto`, and so do files that run code later: `.git/hooks`, `.git/config`,
  shell rc files, `.envrc`, `.qh/` and qh's config. Symlinks and hard links can't get around it.
- **Commands run in a sandbox.** They can write only in the project, temp and package caches, can't
  read secrets, and don't get variables holding tokens and keys. Linux uses Landlock and seccomp
  (plus private network and mount namespaces where the kernel allows them) and macOS uses Seatbelt.
  Nothing to install. On Windows commands aren't sandboxed yet (the guards above still apply);
  `/sandbox` says so.
- **Network through an allowlist.** Package registries and git hosts just work; any other host
  asks first, even in `auto`.
- When the sandbox blocks something a command really needs, the agent can ask to rerun it outside.
  You're asked every time.
- `/sandbox` shows what's in force. `[sandbox]` in the config adds folders, hosts or variables, or
  turns it off.
- A helper's answer that arrives mid-turn is taken in by that turn, not answered twice; a hang when
  two sessions waited on each other is fixed; and a missing file's error says where it looked.
- `/mode auto` autocompletes; Python is found on macOS (`python3`); build folders (any marked with
  `CACHEDIR.TAG`) stay out of the project map.
- Python projects with bare `test_` functions get `pytest` as their test command, since
  `unittest` would find nothing and still say OK.
- Roles that qh named from your first request stay out of the system prompt.
- Mac key hints show ⌃ for Control.

## 0.1.6 — 2026-10-11

The browser no longer gets stuck, a quieter screen, and keys that work on a Mac.

- **The browser no longer gets stuck.** If qh lost track of its Chrome, every later start opened
  a blank tab in it and failed with "browser did not start". qh now reconnects to its own
  Chrome, opens a new tab if every window was closed, and shuts down one it gives up on.
- A page with no console errors or failed requests now says so, so the model knows it checked.
- Python folders with plain `test_*.py` files and no `pyproject.toml` get a test command
  (`python3 -m unittest`).
- **A quieter screen:** no tokens line after every reply (the status bar has it), text wraps at
  100 columns on wide screens, a short key hint line (`/help` lists all keys), and the status
  bar only mentions the profile and thinking when they differ from usual.
- The model knows sessions are saved and that `qh --resume` reopens them.
- **Keys that work on a Mac.** Shift+←/→ switches tabs (macOS takes Ctrl+←/→ for desktops),
  Option+←/→ moves a word instead of typing `b` or `f`, and Shift+↑/↓ scrolls (no PgUp/PgDn on
  a Mac keyboard). Alt+←/→ and Alt+1..9 still work where the terminal sends them.

## 0.1.5 — 2026-10-11

Failed edits point at the right lines, no going in circles, rate limits handled, and a browser that reports what's broken.

- **Failed edits point at the right lines.** When an edit's text isn't in the file, the error
  shows the closest lines with their numbers, so the model can copy them on its next try. When
  the text appears more than once, the error says on which lines.
- **No more going in circles.** If the model makes the same call three times with the same
  result, qh tells it to change approach. On the fifth time it stops and asks you.
- **The model knows the project at start:** the git branch, the uncommitted files, and the
  build and test commands from `Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod` or
  `Makefile`.
- **The browser reports what's broken.** Console errors, uncaught exceptions and failed or
  4xx/5xx requests come back with every page the agent looks at.
- **Rate limits (HTTP 429).** qh waits as long as the provider asks, switches to
  `fallback_model` if you set one, and otherwise says plainly what to do.
- **Switching networks no longer breaks requests.** After changing WiFi or joining a hotspot,
  the first request used to fail ("Can't assign requested address" on macOS). In 0.1.4 this can
  also break `qh update`: restart qh first.
- **`/tier`** shows the window of the tab's own model. On Apple Silicon it reads the memory the
  GPU may use, instead of saying "no NVIDIA GPU". ChatGPT models use their real context size.
- **`/team create` in an empty folder** makes a first commit, so the push no longer fails.
  Running it again finishes a create that stopped half-way.
- Tool descriptions state their limits: the 120 s command timeout and cut-off output.

## 0.1.4 — 2026-10-11

Self-updates from signed releases, ChatGPT Go plans, and direct encrypted lines between teammates.

- `qh update` and `/update`, with signed releases and a daily update check. Run `qh --version`
  to see which version you have.
- ChatGPT Go plans work, through the same sign-in the Codex CLI uses.
- Team mode: direct encrypted lines between teammates, the team room, and agents asking each
  other's agents. Board folders are signed and vouched for by GitHub.

## 0.1.3 — 2026-10-10

Team mode with an encrypted board kept in your own repo, and the docs website.

- **Team mode.** See which teammates are online and what their agents are doing, through the
  project's own git host. The board is encrypted so only the team can read it. Start with
  `qh team create` or `/team create`, and teammates run `qh join`.
- **Teammates' sessions:** read them and send them messages.
- **Simpler team create:** two questions and one confirm; `/team private` and `/team public`.
- **A website and docs:** the homepage, the guide at /docs, and `/docs` opens it from qh.
- Typed `/commands` show in the transcript, and rejected API keys are no longer saved.
- MIT license.

## 0.1.2 — 2026-10-10

Sign in with your ChatGPT plan, coding-plan providers, and pickers for providers and models.

- **Sign in with ChatGPT** and use your plan's allowance instead of an API key.
- **Coding plans:** GLM (Z.ai), Kimi Code and MiniMax.
- **Pickers:** choose a provider, and models grouped by provider.
- **Slash-command autocomplete:** Enter runs the highlighted command.

## 0.1.1 — 2026-10-10

A friendlier first run: tick any mix of model sources, search models, and sign in instead of pasting keys.

- **Interactive setup:** tick any mix of sources (local, Ollama cloud, API keys) and search the
  model list.
- **Ollama cloud** through `ollama signin`, with no API key.
- **OpenRouter** sign-in in the browser.
- Shorter install commands, and a README.

## 0.1.0 — 2026-10-10

The first release: a terminal coding agent for local and cloud models, with several sessions side by side.

- **A coding agent in your terminal** for Ollama models and OpenAI-compatible providers:
  OpenRouter, OpenAI, Anthropic, Groq, DeepSeek and Mistral.
- **Tools:** read, write and edit files, run commands, grep, rename symbols, find references,
  fetch web pages, a todo list, memory, and a browser.
- **Several sessions as tabs** that message each other and hand work around, with a live map.
- **Modes** from ask to auto, and `/undo` and `/diff` for the agent's changes.
- **Sized to your GPU:** the context window fits your graphics memory (`/tier`), and long
  conversations are compacted.
- MCP servers, hooks, skills and your own `/commands`.
- A setup wizard, and on Windows commands run in Git Bash when it's installed.
