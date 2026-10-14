import AppKit
import SwiftUI

/// ⌘K: one search over everything you can do or open — actions, / commands, files, sessions and
/// projects. Type to filter (letters in order, so "rvw" finds Review), ↑↓ to move, ⏎ to run.
struct Palette: View {
    @EnvironmentObject var ws: Workspace
    @Environment(\.dismiss) private var dismiss
    @State private var query = ""
    @State private var sel = 0
    @FocusState private var focused: Bool

    struct Entry: Identifiable {
        let id: String
        let kind: String     // action, command, file, session, project
        let title: String
        let detail: String
        let run: @MainActor () -> Void
    }

    var body: some View {
        let hits = Self.rank(entries, query)
        VStack(spacing: 0) {
            HStack(spacing: 8) {
                Image(systemName: "magnifyingglass").foregroundStyle(Theme.faint)
                TextField("Search actions, commands, files, sessions…", text: $query)
                    .textFieldStyle(.plain).font(Theme.sans(15)).focused($focused)
                    .onSubmit { if hits.indices.contains(sel) { go(hits[sel]) } }
                    .onKeyPress(.downArrow) { sel = min(sel + 1, max(hits.count - 1, 0)); return .handled }
                    .onKeyPress(.upArrow) { sel = max(sel - 1, 0); return .handled }
                    .onKeyPress(.escape) { dismiss(); return .handled }
            }
            .padding(14)
            Rectangle().fill(Theme.rule).frame(height: 1)
            ScrollViewReader { sv in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 1) {
                        ForEach(Array(hits.enumerated()), id: \.element.id) { i, e in
                            HStack(spacing: 10) {
                                Image(systemName: Self.icon(e.kind)).font(.system(size: 12)).foregroundStyle(Theme.soft).frame(width: 16)
                                Text(e.title).font(e.kind == "file" || e.kind == "command" ? Theme.mono(12.5) : Theme.sans(13))
                                    .foregroundStyle(Theme.ink).lineLimit(1).truncationMode(.middle)
                                Spacer(minLength: 8)
                                Text(e.detail).font(Theme.mono(11)).foregroundStyle(Theme.faint).lineLimit(1)
                            }
                            .padding(.horizontal, 12).frame(height: 32)
                            .background(RoundedRectangle(cornerRadius: 6).fill(i == sel ? Theme.accent.opacity(0.14) : .clear))
                            .contentShape(Rectangle())
                            .hand().onTapGesture { go(e) }
                            .id(i)
                        }
                        if hits.isEmpty {
                            Text("Nothing matches.").font(Theme.sans(12.5)).foregroundStyle(Theme.faint).padding(14)
                        }
                    }
                    .padding(6)
                }
                .onChange(of: sel) { _, v in sv.scrollTo(v) }
            }
            .frame(height: 360)
        }
        .frame(width: 620)
        .background(Theme.paper)
        .onAppear { focused = true }
        .onChange(of: query) { _, _ in sel = 0 }
    }

    private func go(_ e: Entry) {
        dismiss()
        DispatchQueue.main.async { MainActor.assumeIsolated { e.run() } }
    }

    private var entries: [Entry] {
        var out: [Entry] = []
        let p = ws.current
        func act(_ t: String, _ key: String, _ run: @escaping @MainActor () -> Void) { out.append(Entry(id: "a:" + t, kind: "action", title: t, detail: key, run: run)) }
        if let p {
            act("New Tab", "⌘T") { p.newTab() }
            act("Chat", "⌃1") { p.layout = "chat" }
            act("Chat and Code", "⌃2") { p.layout = "split" }
            act("Code", "⌃3") { p.layout = "code" }
            act("Review Changes", "⌘D") { p.loadChanges(); p.showReview = true }
            act("Background Tasks", "⌘⇧B") { p.showTasks = true }
            act("Replies Only", "⌘R") { p.repliesOnly.toggle() }
            act("Connect Models", "⌘,") { p.showSetup = true }
            if p.current?.busy == true { act("Stop", "esc") { p.stop() } }
            for t in p.tabs where t.name != p.active {
                act("Switch to \(t.purpose.isEmpty ? t.name : t.purpose)", "tab") { p.active = t.name }
            }
        }
        act("Open Folder…", "⌘O") { ws.pick() }
        act("Inspector", "⌘⌥I") { ws.toggleInspector() }
        act(ws.space == "team" ? "Personal Space" : "Team Space", "⌃space") { ws.setSpace(ws.space == "team" ? "personal" : "team") }
        if let p {
            for c in p.commands { out.append(Entry(id: "c:" + c.cmd, kind: "command", title: c.cmd, detail: c.about) { p.draft = c.cmd + " "; p.layout = p.layout == "code" ? "chat" : p.layout }) }
            for s in p.sessions.prefix(40) {
                out.append(Entry(id: "s:" + s.id, kind: "session", title: s.title.isEmpty ? s.id : s.title, detail: Sessions.ago(s.when)) { p.resume(s) })
            }
        }
        for r in ws.recent where r != p?.path {
            out.append(Entry(id: "p:" + r, kind: "project", title: (r as NSString).lastPathComponent, detail: Workspace.short(r)) { ws.open(r) })
        }
        // files last and only when typing: a big repo would bury everything else
        if let p, !query.isEmpty {
            for f in p.files { out.append(Entry(id: "f:" + f, kind: "file", title: f, detail: "") { p.openFile(f) }) }
        }
        return out
    }

    static func icon(_ kind: String) -> String {
        switch kind {
        case "command": return "slash.circle"
        case "file": return "doc.text"
        case "session": return "bubble.left"
        case "project": return "folder"
        default: return "bolt"
        }
    }

    /// Letters in order; earlier, tighter and word-start matches rank higher, and a file's name
    /// counts more than its folder. Empty query: everything but files, in order.
    static func rank(_ all: [Entry], _ q: String) -> [Entry] {
        let q = q.lowercased().filter { $0 != " " }
        if q.isEmpty { return Array(all.prefix(60)) }
        var scored: [(Int, Int, Entry)] = []
        for (i, e) in all.enumerated() {
            let t = e.title.lowercased()
            guard var s = score(t, q) else { continue }
            if e.kind == "file", let name = t.split(separator: "/").last, let n = score(String(name), q) { s = max(s, n + 20) }
            if e.kind == "action" || e.kind == "command" { s += 5 }
            scored.append((s, i, e))
        }
        return scored.sorted { $0.0 != $1.0 ? $0.0 > $1.0 : $0.1 < $1.1 }.prefix(60).map(\.2)
    }

    static func score(_ t: String, _ q: String) -> Int? {
        if t.hasPrefix(q) { return 100 - min(t.count, 40) / 4 }
        if let r = t.range(of: q) { return 70 - t.distance(from: t.startIndex, to: r.lowerBound) / 2 }
        var s = 40, ti = t.startIndex, prev: String.Index?
        for c in q {
            guard let f = t[ti...].firstIndex(of: c) else { return nil }
            if let pv = prev, t.index(after: pv) == f { s += 2 }
            if f == t.startIndex || " /_-.".contains(t[t.index(before: f)]) { s += 3 }
            prev = f; ti = t.index(after: f)
        }
        return s - t.count / 10
    }
}
