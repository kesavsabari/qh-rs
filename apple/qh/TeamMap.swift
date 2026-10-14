import AppKit
import SwiftUI

/// The whole codebase as a treemap (folders as boxes, files as tiles sized by bytes) with every
/// agent's live work painted on it: a teammate's agent editing a file fills it with their colour
/// and rings it, files they hold get a tint, your own agent's changes show in orange. Hover for who
/// and what, click to open the file.
struct MapPage: View {
    @ObservedObject var project: Project
    @EnvironmentObject var ws: Workspace
    @State private var sizes: [String: Int] = [:]
    @State private var hover: Tile?
    @State private var focus: String?   // a login: only their work is lit

    var body: some View {
        let people = presence
        VStack(spacing: 0) {
            PageHeader(title: "Map", subtitle: subtitle(people)) {
                HStack(spacing: 10) {
                    ForEach(legend(people), id: \.self) { who in
                        Button { focus = focus == who ? nil : who } label: {
                            HStack(spacing: 5) {
                                RoundedRectangle(cornerRadius: 3).fill(Self.color(who, me: me)).frame(width: 10, height: 10)
                                Text(who == me ? "you" : who).font(Theme.sans(12)).foregroundStyle(focus == nil || focus == who ? Theme.ink : Theme.faint)
                            }
                        }
                        .buttonStyle(.plain).help("Show only \(who == me ? "your" : who + "'s") work")
                    }
                }
            }
            GeometryReader { geo in
                let tiles = Self.layout(project.tree, sizes, CGRect(origin: .zero, size: geo.size).insetBy(dx: 12, dy: 12))
                ZStack(alignment: .topLeading) {
                    Canvas { ctx, _ in draw(ctx, tiles, people) }
                    if let h = hover { tip(h, people[h.path] ?? [], in: geo.size) }
                }
                .contentShape(Rectangle())
                .onContinuousHover { phase in
                    if case .active(let p) = phase { hover = tiles.last { $0.rect.contains(p) } } else { hover = nil }
                }
                .onTapGesture {
                    guard let h = hover, !h.isDir else { return }
                    ws.teamPage = .agent
                    project.openFile(h.path)
                }
            }
            .background(Theme.paper)
        }
        .task(id: project.files.count) { sizes = await Self.measure(project.path, project.files) }
    }

    private var me: String { project.team?.me ?? "" }

    // MARK: who is where

    struct Mark { let who: String; let editing: Bool; let what: String }

    /// file → the agents on it. Teammates' edits and held files from the team state; yours from
    /// this session's changes.
    private var presence: [String: [Mark]] {
        var out: [String: [Mark]] = [:]
        for a in project.team?.agents ?? [] where a.login != me {
            for e in a.edits {
                out[e.file, default: []].append(Mark(who: a.login, editing: true,
                    what: "\(a.login) · \(a.name) is editing" + (e.functions.isEmpty ? "" : " " + e.functions.joined(separator: ", "))))
            }
            for f in a.files where !a.edits.contains(where: { $0.file == f }) {
                out[f, default: []].append(Mark(who: a.login, editing: false, what: "\(a.login) · \(a.name) holds it"))
            }
        }
        for c in project.changes { out[c.short, default: []].append(Mark(who: me, editing: true, what: "your agent changed it")) }
        return out
    }

    private func legend(_ p: [String: [Mark]]) -> [String] {
        Array(Set(p.values.flatMap { $0.map(\.who) })).sorted { $0 == me ? true : $1 == me ? false : $0 < $1 }
    }

    private func subtitle(_ p: [String: [Mark]]) -> String {
        let busy = p.filter { $0.value.contains(where: \.editing) }.count
        return "\(project.files.count) files" + (busy > 0 ? " · \(busy) being edited" : " · nobody editing")
    }

    static func color(_ who: String, me: String) -> SwiftUI.Color {
        if who == me { return Theme.accent }
        let hue = Double(who.unicodeScalars.reduce(0) { ($0 * 31 + Int($1.value)) % 360 }) / 360   // same hue as their avatar
        return SwiftUI.Color(hue: hue, saturation: 0.55, brightness: 0.75)
    }

    // MARK: drawing

    private func draw(_ ctx: GraphicsContext, _ tiles: [Tile], _ people: [String: [Mark]]) {
        // which folders hold live work, so their outline shows it from far out
        var hot: [String: String] = [:]
        for (f, ms) in people { if let m = ms.first(where: { focus == nil || $0.who == focus }) {
            var d = (f as NSString).deletingLastPathComponent
            while !d.isEmpty && d != "." { hot[d] = hot[d] ?? m.who; d = (d as NSString).deletingLastPathComponent }
        } }
        for t in tiles {
            let r = RoundedRectangle(cornerRadius: t.isDir ? 5 : 3, style: .continuous).path(in: t.rect)
            if t.isDir {
                ctx.fill(r, with: .color(Theme.ink.opacity(0.035)))
                ctx.stroke(r, with: .color(hot[t.path].map { Self.color($0, me: me) } ?? Theme.ink.opacity(0.12)), lineWidth: hot[t.path] == nil ? 0.5 : 1.2)
                if t.rect.width > 50 && t.rect.height > 22 {
                    ctx.draw(Text(t.name).font(Theme.mono(10)).foregroundStyle(Theme.soft), at: CGPoint(x: t.rect.minX + 6, y: t.rect.minY + 8), anchor: .leading)
                }
                continue
            }
            let marks = (people[t.path] ?? []).filter { focus == nil || $0.who == focus }
            if let m = marks.first(where: \.editing) ?? marks.first {
                let c = Self.color(m.who, me: me)
                ctx.fill(r, with: .color(c.opacity(m.editing ? 0.85 : 0.3)))
                if m.editing { ctx.stroke(RoundedRectangle(cornerRadius: 4).path(in: t.rect.insetBy(dx: -1.5, dy: -1.5)), with: .color(c), lineWidth: 1.5) }
            } else {
                ctx.fill(r, with: .color(Theme.ink.opacity(hover?.path == t.path ? 0.16 : 0.07)))
            }
            if t.rect.width > 60 && t.rect.height > 16 {
                ctx.draw(Text(t.name).font(Theme.mono(9.5)).foregroundStyle(marks.contains(where: \.editing) ? .white : Theme.soft),
                         in: t.rect.insetBy(dx: 4, dy: 3))
            }
        }
    }

    private func tip(_ t: Tile, _ marks: [Mark], in size: CGSize) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(t.path.isEmpty ? project.name : t.path).font(Theme.mono(11.5)).foregroundStyle(Theme.ink)
            if !t.isDir, let b = sizes[t.path] { Text(ByteCountFormatter.string(fromByteCount: Int64(b), countStyle: .file)).font(Theme.mono(10.5)).foregroundStyle(Theme.faint) }
            ForEach(Array(marks.enumerated()), id: \.offset) { _, m in
                HStack(spacing: 5) {
                    Circle().fill(Self.color(m.who, me: me)).frame(width: 7, height: 7)
                    Text(m.what).font(Theme.sans(12)).foregroundStyle(Theme.ink)
                }
            }
            if !t.isDir { Text("click to open").font(Theme.sans(10.5)).foregroundStyle(Theme.faint) }
        }
        .padding(8)
        .background(RoundedRectangle(cornerRadius: 8).fill(Theme.paper).shadow(color: .black.opacity(0.15), radius: 6, y: 2))
        .offset(x: min(t.rect.minX + 8, size.width - 280), y: min(t.rect.maxY + 6, size.height - 90))
        .allowsHitTesting(false)
    }

    // MARK: layout

    struct Tile { let path: String; let name: String; let isDir: Bool; let rect: CGRect }

    /// File sizes off the main thread; a file that can't be read counts as small, not missing.
    static func measure(_ root: String, _ files: [String]) async -> [String: Int] {
        await Task.detached(priority: .utility) {
            var out: [String: Int] = [:]
            for f in files {
                let s = (try? FileManager.default.attributesOfItem(atPath: root + "/" + f)[.size] as? Int) ?? 0
                out[f] = max(s ?? 0, 256)
            }
            return out
        }.value
    }

    /// Squarified treemap. A folder too small to show its insides stays one box.
    static func layout(_ root: FileNode, _ sizes: [String: Int], _ rect: CGRect) -> [Tile] {
        var memo: [String: Double] = [:]
        func weight(_ n: FileNode) -> Double {
            if let w = memo[n.path] { return w }
            let w = n.isDir ? n.children.reduce(0) { $0 + weight($1) } : Double(sizes[n.path] ?? 1024)
            memo[n.path] = w
            return w
        }
        var out: [Tile] = []
        func place(_ kids: [FileNode], in r: CGRect) {
            let items = kids.map { ($0, weight($0)) }.filter { $0.1 > 0 }.sorted { $0.1 > $1.1 }
            let total = items.reduce(0) { $0 + $1.1 }
            guard total > 0, r.width > 2, r.height > 2 else { return }
            let scale = Double(r.width * r.height) / total
            var rest = r, i = 0
            while i < items.count {
                let short = Double(min(rest.width, rest.height))
                var row: [(FileNode, Double)] = [], best = Double.infinity
                while i < items.count {
                    let next = row + [(items[i].0, items[i].1 * scale)]
                    let sum = next.reduce(0) { $0 + $1.1 }, mx = next.map(\.1).max()!, mn = next.map(\.1).min()!
                    let worst = max(short * short * mx / (sum * sum), sum * sum / (short * short * mn))
                    if worst > best { break }
                    best = worst; row = next; i += 1
                }
                let sum = row.reduce(0) { $0 + $1.1 }
                let horizontal = rest.width >= rest.height           // the row runs along the short side
                let thick = CGFloat(sum / short)
                var off: CGFloat = 0
                for (n, a) in row {
                    let len = CGFloat(a) / thick
                    let cell = horizontal ? CGRect(x: rest.minX, y: rest.minY + off, width: thick, height: len)
                                          : CGRect(x: rest.minX + off, y: rest.minY, width: len, height: thick)
                    off += len
                    emit(n, cell.insetBy(dx: 1.5, dy: 1.5))
                }
                rest = horizontal ? CGRect(x: rest.minX + thick, y: rest.minY, width: rest.width - thick, height: rest.height)
                                  : CGRect(x: rest.minX, y: rest.minY + thick, width: rest.width, height: rest.height - thick)
            }
        }
        func emit(_ n: FileNode, _ r: CGRect) {
            guard r.width > 1, r.height > 1 else { return }
            out.append(Tile(path: n.path, name: n.name, isDir: n.isDir, rect: r))
            if n.isDir, r.width > 36, r.height > 30 { place(n.children, in: CGRect(x: r.minX + 3, y: r.minY + 16, width: r.width - 6, height: r.height - 19)) }
        }
        place(root.children, in: rect)
        return out
    }
}
