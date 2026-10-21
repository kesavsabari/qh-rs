import Foundation
import SwiftUI

/// One thing in a tab's thread.
struct Item: Identifiable, Equatable {
    enum Kind: Equatable {
        case user(String)
        case reply(String)
        case tool(name: String, args: String, ok: Bool?, summary: String, output: String)
        case diff(path: String, added: Int, removed: Int)
        case notice(String)
        case error(String)
        case verify(cmd: String, ok: Bool, secs: Double, tail: String)
    }
    let id = UUID()
    var kind: Kind
    var at = Date()       // when it appeared
    var end: Date?        // a tool call: when its result came back
}

/// A permission qh is waiting on.
struct Ask: Identifiable {
    let id: Int
    let tab: String
    let tool: String
    let detail: String
    let warn: String
    let guarded: Bool
    let always: String
    var args: [String: Any] = [:]
}

@MainActor
final class ChatTab: ObservableObject, Identifiable {
    let name: String
    var id: String { name }
    @Published var items: [Item] = []
    @Published var status = "idle"
    @Published var model = ""
    @Published var provider = ""
    @Published var purpose = ""
    @Published var tokens = 0
    @Published var cost: Double?
    @Published var think = false
    @Published var effort = ""
    @Published var plan: [PlanItem] = []
    @Published var planReady = false        // plan mode finished a turn: the plan waits for approval
    @Published var lastActivity = Date()   // the thread last showed something: the quiet timer starts here
    var busy: Bool { status == "working" || status == "confirm" }

    init(name: String) { self.name = name }

    /// Streamed tokens wait here and land together, about 15 times a second, so a fast model
    /// redraws the thread a few dozen times a second instead of once per token.
    private var pending = ""
    private var flushing = false

    func stream(_ text: String) {
        pending += text
        guard !flushing else { return }
        flushing = true
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.066) { [weak self] in
            MainActor.assumeIsolated { self?.flush() }
        }
    }

    func flush() {
        flushing = false
        guard !pending.isEmpty else { return }
        let t = pending
        pending = ""
        append(text: t)
    }

    /// Streaming text joins the reply it belongs to; anything else in between starts a new one.
    func append(text: String) {
        lastActivity = Date()
        if case .reply(let t)? = items.last?.kind {
            items[items.count - 1].kind = .reply(t + text)
        } else {
            items.append(Item(kind: .reply(text)))
        }
    }
}

/// A project folder with its `qh serve`: tabs, mode, and permissions waiting for you.
@MainActor
final class Project: ObservableObject, Identifiable {
    let path: String
    var id: String { path }
    var name: String { (path as NSString).lastPathComponent }
    @Published var tabs: [ChatTab] = []
    @Published var active = "main"
    @Published var mode = ""
    /// qh serve pushes team changes itself (snapshot's team_push): no fast polling needed
    var teamPush = false
    @Published var asks: [Ask] = []
    @Published var state: String = "starting"   // starting | live | closed: <why>
    @Published var setup: SetupState?
    @Published var picker: PickerState?
    @Published var changes: [FileChange] = []
    @Published var checkpoints: [Checkpoint] = []
    @Published var draft: String?   // text to put back in the composer (after a rewind)
    @Published var team: TeamState?
    @Published var transcript: (key: String, rows: [(role: String, text: String)]) = ("", [])
    @Published var sessions: [SavedSession] = []
    @Published var openSessions: [String: String] = [:]   // session id → tab showing it
    @Published var tasks = ""
    @Published var showTeam = false
    @Published var showTasks = false
    @Published var repliesOnly = false
    @Published var showReview = false
    @Published var showMemory = false
    @Published var showActivity = false
    @Published var memory: MemoryState?
    @Published var activity: [ActivityEntry] = []
    @Published var inline: InlineEdit?
    @Published var secret: String?          // provider whose API key the composer is asking for
    @Published var commands: [(cmd: String, about: String)] = []
    var files: [String] = []        // from `qh serve` (follows .gitignore), filled right after connecting
    var fileSet: Set<String> = []
    /// the file tree (from `qh serve`'s list, which follows .gitignore) and the file the code view shows
    @Published var tree = FileNode(name: "", path: "", isDir: true)
    @Published var code: CodeTarget?
    /// main area: "chat", "split" (chat and code side by side) or "code" (tree and file)
    @Published var layout = UserDefaults.standard.string(forKey: "defaultLayout") ?? "chat"
    /// files added or removed anywhere in the project refresh the tree
    private lazy var folderWatch = FolderWatch(path) { [weak self] in self?.loadFiles() }
    func loadFiles() { client.send(["type": "files", "tab": current?.name ?? "main"]) }
    func absolute(_ rel: String) -> String { rel.hasPrefix("/") ? rel : path + "/" + rel }
    /// Opens a path from anywhere (the tree, a link in the thread, a tool card) in the code view.
    func openFile(_ p: String, line: Int? = nil) {
        var rel = p.hasPrefix(path + "/") ? String(p.dropFirst(path.count + 1)) : p
        var line = line
        if line == nil, let c = rel.lastIndex(of: ":"), let n = Int(rel[rel.index(after: c)...]) { line = n; rel = String(rel[..<c]) }
        let found = PathLinks.resolve(rel, files: fileSet, root: path) ?? rel
        guard FileManager.default.fileExists(atPath: absolute(found)) else { return }
        code = CodeTarget(path: found, line: line)
        if layout == "chat" { layout = "split" }
    }
    /// how much each file matters to you, from git history (filled in the background)
    var gitHeat: [String: Double] = [:]
    private var heatLoaded = false
    func loadHeat() {
        guard !heatLoaded else { return }
        heatLoaded = true
        let root = path
        Task.detached {
            let h = FileRank.gitHeat(root)
            await MainActor.run { self.gitHeat = h }
        }
    }
    @Published var models: [ModelChoice] = []
    @Published var showSetup = false
    @Published var setupBusy = false
    @Published var setupMessage: String?
    @Published var setupError = false
    private let client: ServeClient

    init(path: String, qh: String) {
        self.path = path
        client = ServeClient(path: path, qh: qh)
        client.onEvent = { [weak self] in self?.handle($0) }
        client.onClose = { [weak self] why in self?.state = "closed: \(why)" }
        client.start()
    }

    var current: ChatTab? { tabs.first { $0.name == active } ?? tabs.first }
    var waiting: Int { asks.count }

    func tab(_ name: String) -> ChatTab {
        if let t = tabs.first(where: { $0.name == name }) { return t }
        let t = ChatTab(name: name)
        tabs.append(t)
        return t
    }

    // MARK: up

    func prompt(_ text: String, images: [String] = []) {
        let t = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !t.isEmpty || !images.isEmpty, let tab = current else { return }
        if secret != nil {
            secret = nil
            tab.items.append(Item(kind: .notice("key entered (hidden)")))
        } else if t.hasPrefix("/") {
            tab.items.append(Item(kind: .notice("› " + t)))
        } else {
            tab.items.append(Item(kind: .user(t + (images.isEmpty ? "" : "  [+\(images.count) image\(images.count == 1 ? "" : "s")]"))))
            tab.status = "working"
        }
        var msg: [String: Any] = ["type": "prompt", "tab": tab.name, "text": t]
        if !images.isEmpty { msg["images"] = images }
        client.send(msg)
    }

    func choose(_ p: PickerState, _ value: String) {
        picker = nil
        prompt("\(p.cmd) \(value)")
    }

    func loadChanges() {
        client.send(["type": "changes", "tab": current?.name ?? "main"])
        client.send(["type": "checkpoints", "tab": current?.name ?? "main"])
    }
    /// Files back to before message `turn`; with `chat`, the conversation from there goes too.
    func rewind(_ turn: Int, chat: Bool) {
        client.send(["type": "rewind", "tab": current?.name ?? "main", "turn": turn, "chat": chat])
    }
    func accept(_ f: FileChange) { client.send(["type": "accept", "tab": current?.name ?? "main", "path": f.path]) }
    func reject(_ f: FileChange) { client.send(["type": "reject", "tab": current?.name ?? "main", "path": f.path]) }
    func loadSessions() { client.send(["type": "sessions"]) }
    func deleteSession(_ s: SavedSession) { client.send(["type": "delete_session", "id": s.id]) }
    func resume(_ s: SavedSession) {
        if let t = openSessions[s.id], tabs.contains(where: { $0.name == t }) { active = t; return }
        client.send(["type": "resume", "id": s.id])
    }
    func loadTranscript(_ login: String, _ session: String) {
        client.send(["type": "team_transcript", "login": login, "session": session])
    }
    func loadTeam(_ chan: String) { client.send(["type": "team", "chan": chan]) }
    func teamSend(_ chan: String, _ text: String) { client.send(["type": "team_send", "chan": chan, "text": text]) }
    func teamHold(_ h: TeamState.Hold, approve: Bool, note: String) {
        client.send(["type": "team_hold", "from": h.from, "head": h.head, "approve": approve, "note": note])
    }
    func teamClear(_ chan: String) { client.send(["type": "team_clear", "chan": chan]) }
    func loadTasks() { client.send(["type": "tasks", "tab": current?.name ?? "main"]) }
    func fixVerify(_ tab: String) { client.send(["type": "fix_verify", "tab": tab]) }
    func approvePlan(_ tab: ChatTab, edits: String) {
        tab.planReady = false
        tab.status = "working"
        client.send(["type": "plan_approve", "tab": tab.name, "edits": edits])
    }
    func loadMemory() { client.send(["type": "memory", "tab": current?.name ?? "main"]) }
    func saveMemory(_ text: String) { client.send(["type": "memory_save", "tab": current?.name ?? "main", "text": text]) }
    func loadActivity() { client.send(["type": "activity", "tab": current?.name ?? "main"]) }
    /// Ask the model to rewrite lines `start...end` (1-based) of `path`; the result comes back to keep or discard.
    func inlineEdit(path: String, start: Int, end: Int, instruction: String) {
        let tab = current?.name ?? "main"
        inline = InlineEdit(tab: tab, path: path, start: start, end: end, instruction: instruction)
        client.send(["type": "inline_edit", "tab": tab, "path": path, "start": start, "end": end, "instruction": instruction])
    }
    func inlineKeep() {
        guard let i = inline, let id = i.id else { return }
        inline = nil
        client.send(["type": "inline_apply", "id": id])
    }
    func inlineDiscard() {
        if let id = inline?.id { client.send(["type": "inline_discard", "id": id]) }
        inline = nil
    }
    func loadCommands() { client.send(["type": "commands", "tab": current?.name ?? "main"]) }

    func answer(_ a: Ask, _ choice: String) {
        asks.removeAll { $0.id == a.id }
        Notify.clear(self, a.id)
        Notify.badge()
        client.send(["type": "answer", "id": a.id, "answer": choice])
    }

    func stop() { if let t = current { client.send(["type": "stop", "tab": t.name]) } }
    func setMode(_ m: String) { client.send(["type": "mode", "mode": m]) }
    func setModel(_ m: String) { if let t = current { client.send(["type": "model", "tab": t.name, "model": m]) } }
    func newTab() { client.send(["type": "new_tab"]) }
    func closeTab(_ name: String) { client.send(["type": "close_tab", "tab": name]) }
    func shutdown() { client.stop() }

    func loadSetup() {
        client.send(["type": "setup_state"])
        client.send(["type": "setup_models"])
    }
    func connectKey(_ provider: String, _ key: String) {
        setupBusy = true; setupError = false; setupMessage = "testing the \(provider) key…"
        client.send(["type": "setup_key", "provider": provider, "key": key])
    }
    func signInChatGPT() {
        setupBusy = true; setupError = false; setupMessage = "opening your browser to sign in with ChatGPT…"
        client.send(["type": "setup_chatgpt"])
    }
    func setDefault(_ model: String) { client.send(["type": "setup_default", "model": model]) }

    // MARK: down

    private func handle(_ e: [String: Any]) {
        let type = e["type"] as? String ?? ""
        let tabName = e["tab"] as? String ?? "main"
        if type != "text", let t = tabs.first(where: { $0.name == tabName }) {
            t.flush()
            if ["tool_call", "tool_result", "diff", "notice"].contains(type) { t.lastActivity = Date() }
        }
        switch type {
        case "history":
            fill(e)
            loadSessions()   // so the sidebar marks the session you just opened
        case "sessions":
            sessions = (e["sessions"] as? [[String: Any]] ?? []).map {
                SavedSession(id: $0["id"] as? String ?? "", title: $0["title"] as? String ?? "",
                             messages: $0["messages"] as? Int ?? 0, when: Date(timeIntervalSince1970: $0["when"] as? Double ?? 0))
            }
            var open: [String: String] = [:]
            for o in e["open"] as? [[String: Any]] ?? [] { if let id = o["id"] as? String, let t = o["tab"] as? String { open[id] = t } }
            openSessions = open
        case "team_transcript":
            let key = "\(e["login"] as? String ?? "")/\(e["session"] as? String ?? "")"
            transcript = (key, (e["rows"] as? [[String: Any]] ?? []).map { ($0["role"] as? String ?? "", $0["text"] as? String ?? "") })
        case "team":
            team = TeamState(e)
        case "files":
            let list = e["files"] as? [String] ?? []
            files = list
            fileSet = Set(list)
            tree = FileNode.build(list)
            _ = folderWatch
            PathLinks.use(fileSet, root: path)
        case "tasks":
            tasks = e["text"] as? String ?? ""
        case "changes":
            let root = e["root"] as? String ?? path
            changes = (e["files"] as? [[String: Any]] ?? []).map {
                FileChange(path: $0["path"] as? String ?? "", root: root, before: $0["before"] as? String, after: $0["after"] as? String)
            }
        case "checkpoints":
            if tabName == (current?.name ?? "main") {
                checkpoints = (e["items"] as? [[String: Any]] ?? []).map {
                    Checkpoint(turn: $0["turn"] as? Int ?? 0, prompt: $0["prompt"] as? String ?? "", files: $0["files"] as? [String] ?? [])
                }
            }
        case "rewound":
            let names = e["restored"] as? [String] ?? []
            tab(tabName).items.append(Item(kind: .notice(names.isEmpty ? "rewound: no files to restore" : "rewound: restored \(names.joined(separator: ", "))")))
            if let p = e["prompt"] as? String, !p.isEmpty { draft = p }
        case "picker":
            picker = PickerState(e)
        case "secret":
            secret = e["provider"] as? String
        case "active":
            if let a = e["tab"] as? String, tabs.contains(where: { $0.name == a }) { active = a }
            loadSessions()
        case "commands":
            commands = (e["commands"] as? [[String: Any]] ?? []).filter { $0["sub"] == nil }
                .map { ($0["cmd"] as? String ?? "", $0["about"] as? String ?? "") }
        case "setup":
            setup = SetupState(e)
            if setup?.needed == true { showSetup = true }
        case "setup_progress":
            setupMessage = e["message"] as? String
        case "setup_result":
            setupBusy = false
            let step = e["step"] as? String ?? ""
            let ok = e["ok"] as? Bool ?? false
            setupError = !ok
            let data = e["data"] as? [String: Any] ?? [:]
            if !ok { setupMessage = e["error"] as? String ?? "that didn't work" }
            else if step == "models" {
                models = (data["models"] as? [[String: Any]] ?? []).map { ModelChoice(id: $0["id"] as? String ?? "", from: $0["from"] as? String ?? "") }
                if !models.isEmpty, setupMessage == nil, setup?.needed == true { setupMessage = "pick a default model below" }
            } else if step == "default" {
                setupMessage = "default model: \(data["model"] as? String ?? "")"
            } else {
                setupMessage = step == "chatgpt" ? "signed in as \(data["account"] as? String ?? "")" : "connected: \(data["models"] as? Int ?? 0) models"
                client.send(["type": "setup_models"])
            }
        case "snapshot":
            state = "live"
            teamPush = e["team_push"] as? Bool ?? false
            loadFiles()
            if e["needs_setup"] as? Bool == true { showSetup = true; loadSetup() }
            mode = e["mode"] as? String ?? ""
            tabs = []
            for t in e["tabs"] as? [[String: Any]] ?? [] { fill(t) }
            loadSessions()
            if !tabs.contains(where: { $0.name == active }) { active = tabs.first?.name ?? "main" }
        case "tabs":
            mode = e["mode"] as? String ?? mode
            let list = e["tabs"] as? [[String: Any]] ?? []
            let names = list.compactMap { $0["name"] as? String }
            for t in list { update(tab(t["name"] as? String ?? ""), t) }
            let before = tabs.count
            tabs.removeAll { !names.contains($0.name) }
            if tabs.count != before { loadSessions() }   // a closed tab frees its session in the sidebar
            if !names.contains(active) { active = names.first ?? "main" }
        case "text":
            tab(tabName).stream(e["delta"] as? String ?? "")
        case "tool_call":
            tab(tabName).items.append(Item(kind: .tool(name: e["name"] as? String ?? "", args: Self.brief(e["args"]), ok: nil, summary: "", output: "")))
        case "tool_result":
            let t = tab(tabName)
            if let i = t.items.lastIndex(where: { if case .tool(_, _, nil, _, _) = $0.kind { return true }; return false }),
               case .tool(let n, let a, _, _, _) = t.items[i].kind {
                t.items[i].kind = .tool(name: n, args: a, ok: e["ok"] as? Bool ?? true, summary: e["summary"] as? String ?? "",
                                        output: e["output"] as? String ?? "")
                t.items[i].end = Date()
            }
        case "diff":
            loadChanges()
            tab(tabName).items.append(Item(kind: .diff(path: e["path"] as? String ?? "", added: e["added"] as? Int ?? 0, removed: e["removed"] as? Int ?? 0)))
        case "notice", "compact":
            let msg = e["message"] as? String ?? "compacted the conversation"
            tab(tabName).items.append(Item(kind: .notice(msg)))
        case "verify":
            tab(tabName).items.append(Item(kind: .verify(cmd: e["cmd"] as? String ?? "", ok: e["ok"] as? Bool ?? false,
                                                          secs: e["secs"] as? Double ?? 0, tail: e["tail"] as? String ?? "")))
        case "plan":
            let t = tab(tabName)
            t.plan = (e["items"] as? [[String: Any]] ?? []).map { PlanItem(text: $0["text"] as? String ?? "", status: $0["status"] as? String ?? "pending") }
            if t.plan.isEmpty { t.planReady = false }
        case "plan_ready":
            tab(tabName).planReady = true
        case "memory":
            memory = MemoryState(path: e["path"] as? String ?? "", text: e["text"] as? String ?? "",
                                 instructions: (e["instructions"] as? [[String: Any]] ?? []).map { ($0["path"] as? String ?? "", $0["text"] as? String ?? "") })
        case "activity":
            if tabName == (current?.name ?? "main") {
                activity = (e["items"] as? [[String: Any]] ?? []).map(ActivityEntry.init).reversed()
            }
        case "inline_delta":
            if let i = inline, i.id == nil || i.id == e["id"] as? Int {
                inline?.id = e["id"] as? Int
                inline?.preview += e["delta"] as? String ?? ""
            }
        case "inline_result":
            if inline != nil {
                inline?.id = e["id"] as? Int
                inline?.before = e["before"] as? String ?? ""
                inline?.after = e["after"] as? String ?? ""
                inline?.done = true
            }
        case "inline_applied":
            tab(tabName).items.append(Item(kind: .notice("inline edit kept: \(e["path"] as? String ?? "")")))
        case "error":
            if e["re"] as? String == "inline_edit", inline != nil { inline?.error = e["message"] as? String ?? "error"; break }
            let msg = e["message"] as? String ?? "error"
            if e["re"] != nil { tab(current?.name ?? "main").items.append(Item(kind: .error(msg))) }
            else { tab(tabName).items.append(Item(kind: .error(msg))) }
        case "usage":
            let t = tab(tabName)
            t.cost = e["cost"] as? Double
        case "turn_done":
            tab(tabName).tokens = e["tokens"] as? Int ?? 0
            loadSessions()
        case "confirm":
            asks.append(Ask(id: e["id"] as? Int ?? -1, tab: tabName, tool: e["tool"] as? String ?? "",
                            detail: Self.detail(e["tool"] as? String ?? "", e["args"]), warn: e["warn"] as? String ?? "",
                            guarded: e["guard"] as? Bool ?? false, always: e["always"] as? String ?? "",
                            args: e["args"] as? [String: Any] ?? [:]))
            if let a = asks.last { Notify.ask(self, a) }
        case "confirm_closed":
            let id = e["id"] as? Int ?? -1
            asks.removeAll { $0.id == id }
            Notify.clear(self, id)
        default:
            break
        }
        Notify.badge()
    }

    /// A tab from the server with its saved history (snapshot, or a resumed session).
    private func fill(_ t: [String: Any]) {
        let tab = self.tab(t["name"] as? String ?? "main")
        update(tab, t)
        tab.items = []
        for m in t["messages"] as? [[String: Any]] ?? [] {
            let text = m["text"] as? String ?? ""
            if m["role"] as? String == "user" {
                if !text.isEmpty { tab.items.append(Item(kind: .user(text))) }
            } else {
                if !text.isEmpty { tab.items.append(Item(kind: .reply(text))) }
                for name in m["tools"] as? [String] ?? [] {
                    tab.items.append(Item(kind: .tool(name: name, args: "", ok: true, summary: "", output: "")))
                }
            }
        }
    }

    private func update(_ tab: ChatTab, _ t: [String: Any]) {
        let was = tab.busy
        tab.status = t["status"] as? String ?? tab.status
        tab.model = t["model"] as? String ?? tab.model
        tab.provider = t["provider"] as? String ?? tab.provider
        tab.purpose = t["purpose"] as? String ?? tab.purpose
        tab.tokens = t["tokens"] as? Int ?? tab.tokens
        tab.think = t["think"] as? Bool ?? tab.think
        tab.effort = t["effort"] as? String ?? tab.effort
        if was && !tab.busy { Notify.done(project: self, tab: tab.name) }
    }

    /// The one line a tool card shows: the path, command, or query, not the whole JSON.
    static func brief(_ a: Any?) -> String {
        guard let d = a as? [String: Any] else { return "" }
        for k in ["cmd", "path", "pattern", "query", "url", "host", "name"] {
            if let s = d[k] as? String { return s }
        }
        return (try? String(data: JSONSerialization.data(withJSONObject: d), encoding: .utf8)) ?? ""
    }

    /// What an approval card shows in full: the command, or the edit.
    static func detail(_ tool: String, _ a: Any?) -> String {
        guard let d = a as? [String: Any] else { return "" }
        if let c = d["cmd"] as? String { return c }
        if let p = d["path"] as? String {
            if let old = d["old"] as? String, let new = d["new"] as? String {
                let minus = old.split(separator: "\n", omittingEmptySubsequences: false).map { "- " + $0 }
                let plus = new.split(separator: "\n", omittingEmptySubsequences: false).map { "+ " + $0 }
                return ([p] + minus + plus).joined(separator: "\n")
            }
            if let c = d["content"] as? String { return p + "\n" + String(c.prefix(4000)) }
            return p
        }
        if let h = d["host"] as? String { return h }
        return brief(a)
    }
}

/// A list a /command offers (/model, /provider, /merge…): choosing runs `cmd value`.
struct PickerState: Identifiable {
    struct Row: Identifiable { let value: String; let label: String; let group: String; var id: String { group + "\u{1}" + value } }
    let id = UUID()
    let title: String
    let cmd: String
    let current: String
    let rows: [Row]

    init(_ e: [String: Any]) {
        title = e["title"] as? String ?? ""
        cmd = e["cmd"] as? String ?? ""
        current = e["current"] as? String ?? ""
        rows = (e["items"] as? [[String: Any]] ?? []).map {
            Row(value: $0["value"] as? String ?? "", label: $0["label"] as? String ?? "", group: $0["group"] as? String ?? "")
        }
    }
}

struct SavedSession: Identifiable {
    let id: String
    let title: String
    let messages: Int
    let when: Date
}
