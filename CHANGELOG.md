# Changelog

## 0.1.5 — 2026-10-11

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

- `qh update` and `/update`, with signed releases and a daily update check. Run `qh --version`
  to see which version you have.
- ChatGPT Go plans work, through the same sign-in the Codex CLI uses.
- Team mode: direct encrypted lines between teammates, the team room, and agents asking each
  other's agents. Board folders are signed and vouched for by GitHub.
