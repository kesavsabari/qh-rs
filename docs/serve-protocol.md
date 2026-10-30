# `qh serve` protocol

The contract between `qh serve` (src/serve.rs) and its clients: the Mac app (apple/), the Windows
app (windows/) and the Linux app (linux/). The VS Code extension (vscode/) still drives qh in a
terminal and does not speak this protocol yet.

**Protocol 1.0** (since qh 0.2.4). Everything below is written from the code, not from intent; if
the two disagree, the code is the bug.

## Versioning

- `PROTOCOL` (major, now `1`) and `PROTOCOL_MINOR` (now `0`) in src/serve.rs.
- **Minor** goes up for additions an old client can ignore: a new event, a new command, a new
  optional field. Clients must ignore event types and fields they don't know.
- **Major** goes up for anything that breaks a client: a removed or renamed event, command or
  field, a changed meaning or type. A client and server with different majors refuse each other.
- Renaming, removing or retyping anything in this file means a major bump and a client release.

## Transport

- One server per project folder. Socket and token live at `state_dir/serve/<sha256(path)[..16]>.sock`
  and `.token` (folder 0700, files 0600; the server refuses a folder that isn't private to the user).
- A Unix stream socket; on Windows an AF_UNIX one (Windows 10 1803+), same bytes.
- UTF-8 JSON, one object per line (`\n`), both directions. Every object has a `type`.
- Unparseable lines from a client are skipped silently.

## Starting

Run `qh serve` in the project folder. Its first JSON stdout line says where to connect (a login
shell may print other lines first; skip to the JSON one):

| line | meaning |
|---|---|
| `{"type":"listening","socket":…,"token_file":…,"protocol":1,"protocol_minor":0,"pid":…}` | this process is the server; the client may stop it when done |
| `{"type":"running","socket":…,"token_file":…,"protocol":1,"protocol_minor":0}` | another server already answers for the folder; this process exits 0 |

qh before 0.2.4 sent `protocol` without `protocol_minor`; a missing `protocol` means major 1. A client seeing another major refuses here, before connecting,
and stops the server if it started it (`listening`).

## Handshake

1. Client reads the token from `token_file` (trimmed) and connects.
2. Client's first line: `{"type":"hello","token":"…","protocol":1}`.
   `protocol` is the client's major. Omitting it (pre-0.2.4 clients) is accepted as 1.
3. Server answers one of:
   - wrong type or token: `{"type":"error","message":"bad hello or token"}`, then closes.
   - other major (or a non-integer `protocol`):
     `{"type":"error","re":"hello","code":"protocol","protocol":<server major>,"message":"this qh speaks serve protocol 1, the app speaks 2; update qh"}`,
     then closes. Clients show `message` as the reason the connection ended.
   - accepted: a `snapshot`, then one `confirm_pending` per open confirm, then the live stream.
     Snapshot and joining the broadcast happen under one lock, so no event falls between them.

## Common rules

- Commands take an optional `tab` (default `"main"`).
- A command that fails gets `{"type":"error","message":…,"re":<command type>}` sent to that client
  only. Commands that work answer nothing directly; their results are broadcast to every client.
- Server events are broadcast to every connected client unless marked *(this client)*.
- Events about a tab carry `tab`.

## Shared shapes

**tab** (in `snapshot`, `tabs`, `history`): `name`, `status` (`idle`, …), `model` (`provider:model`
or bare), `provider`, `effort`, `think`, `purpose` (label), `cwd`, `tokens`. With history
(`snapshot`, `history` only): `messages: [{role: "user"|"assistant", text, tools: [name]|null}]`.

## Commands (client → server)

| type | fields | effect / answer |
|---|---|---|
| `hello` | `token`, `protocol` | first line only; see Handshake |
| `snapshot` | | a fresh `snapshot` *(this client)* |
| `prompt` | `text`, `images?: [string]` | exactly what Enter does in the TUI (text or `/command`); needs text or images. Broadcasts any `notice` / `error` / `picker` it produces, then `tabs` |
| `commands` | | `commands` |
| `answer` | `id`, `answer: "yes"\|"always"\|other=no` | answers a `confirm`; `always` only counts when the confirm had a rule. Broadcasts `confirm_closed` |
| `stop` | | interrupts the tab's turn |
| `mode` | `mode` | sets the hub mode (`plan`, `auto-edit`, …); `tabs` follows. Leaving for plan remembers the old mode |
| `model` | `model` (`provider:id`) | sets the tab's model; `tabs` follows |
| `new_tab` | `name?` | adds a tab (default `tab2`, `tab3`, …); `tabs` follows |
| `close_tab` | | closes the tab; `tabs` follows |
| `sessions` | | `sessions` (and, once per server, titles old sessions in the background, then `sessions` again) |
| `resume` | `id` | opens a saved session in a new tab (`resumed`, `resumed2`, …): `tabs`, `history`, `active`. Already open: just `active` |
| `delete_session` | `id` | deletes a saved session and its checkpoints (closes its tab, unless it is the last); `tabs`, `sessions` |
| `files` | | `files` |
| `activity` | | `activity` |
| `tasks` | | `tasks` |
| `changes` | | `changes` |
| `accept` / `reject` | `path` | keeps / reverts one changed file; `changes` |
| `checkpoints` | | `checkpoints` |
| `rewind` | `turn`, `chat?: bool` | restores files to before user message `turn`; with `chat`, truncates the conversation too (`history`). Then `rewound`, `checkpoints`, `changes`, `tabs`. Refused while the tab is busy |
| `plan` | | `plan` |
| `plan_approve` | `edits?` | leaves plan mode (back to the remembered mode) and sends "Go ahead with the plan." plus edits |
| `memory` | | `memory` |
| `memory_save` | `text` | writes the project's NOTES.md; `memory` |
| `fix_verify` | | sends the tab's last failed check back to the model to fix |
| `inline_edit` | `path` (project-relative), `start`, `end` (byte offsets), `instruction` | streams `inline_delta`, then `inline_result` or an `error` with `re:"inline_edit"` and `id` |
| `inline_apply` | `id` | writes the inline result (refused if the selection changed since); `inline_applied`, `changes` |
| `inline_discard` | `id` | drops a pending inline result |
| `team` | `chan?` (default `#team`) | `team`; from then on `team` is pushed on every team change (debounced 250 ms) |
| `team_send` | `chan?`, `text` | posts to a team channel; `team` |
| `team_transcript` | `login`, `session` | `team_transcript` |
| `setup` / `setup_state` | | `setup` *(this client)* |
| `setup_key` | `provider`, `key` | tests and saves an API key: `setup_result` (`data.models` = count), then `setup` *(this client)* |
| `setup_chatgpt` | | ChatGPT sign-in: `setup_progress` lines, then `setup_result` (`data.account`), `setup` *(this client)* |
| `setup_models` | | `setup_result` with `data.models: [{id, from}]`, then `setup` *(this client)* |
| `setup_default` | `model` | sets the default model, moves tabs to it: `setup_result` (`data.model`), `setup`, `tabs` |

`setup_*` commands run off the client's reader thread and answer only that client. Any other
`type` gets `error` "unknown command …" (`unknown setup step …` as a failed `setup_result` for `setup_*`).

## Events (server → client)

### Connection and tabs
| type | fields |
|---|---|
| `snapshot` | `protocol`, `protocol_minor`, `version` (qh), `mode`, `project`, `tabs: [tab with messages]`, `needs_setup` |
| `tabs` | `mode`, `tabs: [tab without messages]`; on any tab or hub change |
| `history` | a tab object with `messages` plus `type`; after `resume` and `rewind` with `chat` |
| `active` | `tab`: the client should show this tab |
| `error` | `message`, `re?` (command that failed), `code?` (`"protocol"` on a refused hello), `id?`, `tab?` |

### A turn
| type | fields |
|---|---|
| `text` | `tab`, `delta`: streamed assistant text |
| `tool_call` | `tab`, `name`, `args` |
| `tool_result` | `tab`, `name`, `ok`, `summary` (first line), `output` (first 8000 chars) |
| `diff` | `tab`, `path`, `added`, `removed`: after a successful file-writing tool |
| `notice` | `tab`, `message` |
| `verify` | `tab`, `cmd`, `ok`, `secs`, `tail` |
| `compact` | `tab`, `stage`, `before`, `after` |
| `log` | `tab`, `message` |
| `turn_done` | `tab`, `tokens` |
| `plan_ready` | `tab`: a turn ended in plan mode |
| `picker` | `tab`, `title`, `cmd`, `current`, `items: [{value, label, group}]`: a `/command` wants a choice; answer with a `prompt` of `cmd value` |

### Confirms
| type | fields |
|---|---|
| `confirm` | `tab`, `id`, `tool`, `args`, `cwd`, `warn`, `guard`, `always` (the rule "always" would save; empty = no always) |
| `confirm_pending` | `id`, `always`: right after the snapshot, for confirms opened before this client joined *(this client)* |
| `confirm_closed` | `id`: answered by some client |

### Answers to commands
| type | fields |
|---|---|
| `commands` | `tab`, `commands: [{cmd, about, sub?}]` |
| `sessions` | `sessions: [{id, title, messages, when, path}]`, `open: [{tab, id}]` |
| `files` | `tab`, `root`, `files: [relative path]` |
| `activity` | `tab`, `items` (activity log rows) |
| `tasks` | `tab`, `text` |
| `changes` | `tab`, `root`, `files: [{path, before, after}]` |
| `checkpoints` | `tab`, `items: [{turn, prompt, files}]` |
| `rewound` | `tab`, `turn`, `restored`, `prompt` (the removed user message when `chat`, else empty) |
| `plan` | `tab`, `items: [{text, status: "done"\|"active"\|"pending"}]`; also after every `todo` tool result |
| `memory` | `path`, `text`, `instructions: [{path, text, readonly: true}]` |
| `inline_delta` | `id`, `tab`, `delta` |
| `inline_result` | `id`, `tab`, `path`, `start`, `end`, `before`, `after` |
| `inline_applied` | `id`, `tab`, `path` |
| `team` | `running: false`, or `running: true`, `me`, `repo`, `status`, `people`, `channels`, `waiting`, `activity`, `chan`, `messages`, `agents`, `team_push: true` |
| `team_transcript` | `login`, `session`, `rows: [{role, text}]` |
| `setup` | `needed`, `sources: [{id, label, kind: "local"\|"key"\|"signin", connected, …}]`, `default` |
| `setup_result` | `step`, `ok`, `data?` or `error?` |
| `setup_progress` | `message` |
