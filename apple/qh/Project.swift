import Foundation
import SwiftUI

/// One thing in a tab's thread.
struct Item: Identifiable {
    enum Kind {
        case user(String)
        case reply(String)
        case tool(name: String, args: String, ok: Bool?, summary: String)
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

    func prompt(_ text: String) {
        let t = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !t.isEmpty, let tab = current else { return }
        tab.items.append(Item(kind: .user(t)))
        tab.status = "working"
        client.send(["type": "prompt", "tab": tab.name, "text": t])
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

    // MARK: down

    private func handle(_ e: [String: Any]) {
        let type = e["type"] as? String ?? ""
        let tabName = e["tab"] as? String ?? "main"
        switch type {
        case "snapshot":
            state = "live"
            mode = e["mode"] as? String ?? ""
            tabs = []
            for t in e["tabs"] as? [[String: Any]] ?? [] {
                let tab = self.tab(t["name"] as? String ?? "main")
                update(tab, t)
                for m in t["messages"] as? [[String: Any]] ?? [] {
                    let text = m["text"] as? String ?? ""
                    if m["role"] as? String == "user" {
                        if !text.isEmpty { tab.items.append(Item(kind: .user(text))) }
                    } else {
                        if !text.isEmpty { tab.items.append(Item(kind: .reply(text))) }
                        for name in m["tools"] as? [String] ?? [] {
                            tab.items.append(Item(kind: .tool(name: name, args: "", ok: true, summary: "")))
                        }
                    }
                }
            }
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
            tab(tabName).items.append(Item(kind: .tool(name: e["name"] as? String ?? "", args: Self.brief(e["args"]), ok: nil, summary: "")))
        case "tool_result":
            let t = tab(tabName)
            if let i = t.items.lastIndex(where: { if case .tool(_, _, nil, _) = $0.kind { return true }; return false }),
               case .tool(let n, let a, _, _) = t.items[i].kind {
                t.items[i].kind = .tool(name: n, args: a, ok: e["ok"] as? Bool ?? true, summary: e["summary"] as? String ?? "")
            }
        case "diff":
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
