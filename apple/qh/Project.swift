import Foundation
import SwiftUI

/// One thing in a tab's thread.
struct Item: Identifiable {
    enum Kind {
        case user(String)
        case reply(String)
        case tool(name: String, args: String, ok: Bool?, summary: String, output: String)
        case diff(path: String, added: Int, removed: Int)
        case notice(String)
        case error(String)
    }
    let id = UUID()
    var kind: Kind
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
}

@MainActor
final class ChatTab: ObservableObject, Identifiable {
    let name: String
    var id: String { name }
    @Published var items: [Item] = []
    @Published var status = "idle"
    @Published var model = ""
    @Published var purpose = ""
    @Published var tokens = 0
    @Published var cost: Double?
    var busy: Bool { status == "working" || status == "confirm" }

    init(name: String) { self.name = name }

    /// Streaming text joins the reply it belongs to; anything else in between starts a new one.
    func append(text: String) {
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
    @Published var asks: [Ask] = []
    @Published var state: String = "starting"   // starting | live | closed: <why>
    @Published var setup: SetupState?
    @Published var picker: PickerState?
    @Published var changes: [FileChange] = []
    @Published var team: TeamState?
    @Published var sessions: [SavedSession] = []
    @Published var openSessions: [String: String] = [:]   // session id → tab showing it
    @Published var tasks = ""
    @Published var showTeam = false
    @Published var showTasks = false
    @Published var repliesOnly = false
    @Published var showReview = false
    @Published var secret: String?          // provider whose API key the composer is asking for
    @Published var commands: [(cmd: String, about: String)] = []
    lazy var files: [String] = Self.listFiles(path)
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

    func loadChanges() { client.send(["type": "changes", "tab": current?.name ?? "main"]) }
    func accept(_ f: FileChange) { client.send(["type": "accept", "tab": current?.name ?? "main", "path": f.path]) }
    func reject(_ f: FileChange) { client.send(["type": "reject", "tab": current?.name ?? "main", "path": f.path]) }
    func loadSessions() { client.send(["type": "sessions"]) }
    func resume(_ s: SavedSession) {
        if let t = openSessions[s.id] { active = t; return }
        client.send(["type": "resume", "id": s.id])
    }
    func loadTeam(_ chan: String) { client.send(["type": "team", "chan": chan]) }
    func teamSend(_ chan: String, _ text: String) { client.send(["type": "team_send", "chan": chan, "text": text]) }
    func loadTasks() { client.send(["type": "tasks", "tab": current?.name ?? "main"]) }
    func loadCommands() { client.send(["type": "commands", "tab": current?.name ?? "main"]) }

    /// Project files for @ mentions: skips what's never worth mentioning, stops at 20k.
    nonisolated static func listFiles(_ root: String) -> [String] {
        let skip: Set<String> = [".git", "node_modules", "target", "build", ".build", "dist", ".next", "DerivedData", "__pycache__", ".venv"]
        var out: [String] = []
        let base = URL(fileURLWithPath: root)
        guard let e = FileManager.default.enumerator(at: base, includingPropertiesForKeys: [.isDirectoryKey], options: [.skipsHiddenFiles]) else { return [] }
        for case let u as URL in e {
            if skip.contains(u.lastPathComponent) { e.skipDescendants(); continue }
            if (try? u.resourceValues(forKeys: [.isDirectoryKey]).isDirectory) == true { continue }
            out.append(String(u.path.dropFirst(base.path.count + 1)))
            if out.count >= 20000 { break }
        }
        return out
    }

    func answer(_ a: Ask, _ choice: String) {
        asks.removeAll { $0.id == a.id }
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
        switch type {
        case "history":
            fill(e)
        case "sessions":
            sessions = (e["sessions"] as? [[String: Any]] ?? []).map {
                SavedSession(id: $0["id"] as? String ?? "", title: $0["title"] as? String ?? "",
                             messages: $0["messages"] as? Int ?? 0, when: Date(timeIntervalSince1970: $0["when"] as? Double ?? 0))
            }
            var open: [String: String] = [:]
            for o in e["open"] as? [[String: Any]] ?? [] { if let id = o["id"] as? String, let t = o["tab"] as? String { open[id] = t } }
            openSessions = open
        case "team":
            team = TeamState(e)
        case "tasks":
            tasks = e["text"] as? String ?? ""
        case "changes":
            let root = e["root"] as? String ?? path
            changes = (e["files"] as? [[String: Any]] ?? []).map {
                FileChange(path: $0["path"] as? String ?? "", root: root, before: $0["before"] as? String, after: $0["after"] as? String)
            }
        case "picker":
            picker = PickerState(e)
        case "secret":
            secret = e["provider"] as? String
        case "active":
            if let a = e["tab"] as? String, tabs.contains(where: { $0.name == a }) { active = a }
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
            tabs.removeAll { !names.contains($0.name) }
            if !names.contains(active) { active = names.first ?? "main" }
        case "text":
            tab(tabName).append(text: e["delta"] as? String ?? "")
        case "tool_call":
            tab(tabName).items.append(Item(kind: .tool(name: e["name"] as? String ?? "", args: Self.brief(e["args"]), ok: nil, summary: "", output: "")))
        case "tool_result":
            let t = tab(tabName)
            if let i = t.items.lastIndex(where: { if case .tool(_, _, nil, _, _) = $0.kind { return true }; return false }),
               case .tool(let n, let a, _, _, _) = t.items[i].kind {
                t.items[i].kind = .tool(name: n, args: a, ok: e["ok"] as? Bool ?? true, summary: e["summary"] as? String ?? "",
                                        output: e["output"] as? String ?? "")
            }
        case "diff":
            loadChanges()
            tab(tabName).items.append(Item(kind: .diff(path: e["path"] as? String ?? "", added: e["added"] as? Int ?? 0, removed: e["removed"] as? Int ?? 0)))
        case "notice", "compact":
            let msg = e["message"] as? String ?? "compacted the conversation"
            tab(tabName).items.append(Item(kind: .notice(msg)))
        case "error":
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
                            guarded: e["guard"] as? Bool ?? false, always: e["always"] as? String ?? ""))
            Notify.ask(project: name, tab: tabName, tool: e["tool"] as? String ?? "")
        case "confirm_closed":
            let id = e["id"] as? Int ?? -1
            asks.removeAll { $0.id == id }
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
        tab.purpose = t["purpose"] as? String ?? tab.purpose
        tab.tokens = t["tokens"] as? Int ?? tab.tokens
        if was && !tab.busy { Notify.done(project: name, tab: tab.name) }
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
    struct Row: Identifiable { let id = UUID(); let value: String; let label: String; let group: String }
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
