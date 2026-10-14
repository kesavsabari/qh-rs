import AppKit
import SwiftUI

struct FileChange: Identifiable {
    let path: String
    let root: String
    let before: String?
    let after: String?
    var id: String { path }
    var short: String { path.hasPrefix(root + "/") ? String(path.dropFirst(root.count + 1)) : path }
    var kind: String { before == nil ? "new" : after == nil ? "deleted" : "edited" }
}

/// One row of a diff: a kept line (both numbers), a removed one (old only), an added one (new only),
/// or a fold over unchanged lines.
struct DiffRow: Identifiable {
    enum Kind { case same, removed, added, fold }
    let id: Int
    let kind: Kind
    let old: Int?
    let new: Int?
    let text: String
}

enum Diff {
    /// Just the +/− counts, for the file list: no rows built.
    static func counts(_ before: String?, _ after: String?) -> (added: Int, removed: Int) {
        let a = (before ?? "").split(separator: "\n", omittingEmptySubsequences: false)
        let b = (after ?? "").split(separator: "\n", omittingEmptySubsequences: false)
        var add = 0, rem = 0
        for c in b.difference(from: a) { if case .insert = c { add += 1 } else { rem += 1 } }
        return (add, rem)
    }

    /// Line diff with 3 lines of context around each change; long unchanged runs fold.
    static func rows(_ before: String?, _ after: String?) -> [DiffRow] {
        let a = (before ?? "").split(separator: "\n", omittingEmptySubsequences: false).map(String.init)
        let b = (after ?? "").split(separator: "\n", omittingEmptySubsequences: false).map(String.init)
        let d = b.difference(from: a)
        var removed = Set<Int>(), inserted = Set<Int>()
        for c in d {
            switch c {
            case .remove(let o, _, _): removed.insert(o)
            case .insert(let o, _, _): inserted.insert(o)
            }
        }
        // walk both sides in order
        var all: [(DiffRow.Kind, Int?, Int?, String)] = []
        var i = 0, j = 0
        while i < a.count || j < b.count {
            if i < a.count && removed.contains(i) { all.append((.removed, i + 1, nil, a[i])); i += 1 }
            else if j < b.count && inserted.contains(j) { all.append((.added, nil, j + 1, b[j])); j += 1 }
            else if i < a.count && j < b.count { all.append((.same, i + 1, j + 1, a[i])); i += 1; j += 1 }
            else if i < a.count { all.append((.removed, i + 1, nil, a[i])); i += 1 }
            else { all.append((.added, nil, j + 1, b[j])); j += 1 }
        }
        let changed = all.indices.filter { all[$0].0 != .same }
        var keep = Set<Int>()
        for c in changed { for k in max(0, c - 3)...min(all.count - 1, c + 3) { keep.insert(k) } }
        var out: [DiffRow] = []
        var hidden = 0
        for (k, r) in all.enumerated() {
            if keep.contains(k) || changed.isEmpty {
                if hidden > 0 { out.append(DiffRow(id: out.count, kind: .fold, old: nil, new: nil, text: "⋯ \(hidden) unchanged lines")); hidden = 0 }
                out.append(DiffRow(id: out.count, kind: r.0, old: r.1, new: r.2, text: r.3))
            } else {
                hidden += 1
            }
        }
        if hidden > 0 { out.append(DiffRow(id: out.count, kind: .fold, old: nil, new: nil, text: "⋯ \(hidden) unchanged lines")) }
        return out
    }
}

/// Everything the agent changed this session: pick a file, read the diff, keep it or put it back.
struct ReviewView: View {
    @ObservedObject var project: Project
    @State private var sel: String?
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        let files = project.changes
        let cur = files.first { $0.path == sel } ?? files.first
        HStack(spacing: 0) {
            VStack(alignment: .leading, spacing: 0) {
                Text("changes").font(Theme.mono(12)).foregroundStyle(Theme.soft).padding(14)
                ScrollView {
                    VStack(alignment: .leading, spacing: 1) {
                        ForEach(files) { f in
                            let n = Diff.counts(f.before, f.after)
                            HStack(spacing: 6) {
                                Text(f.short).foregroundStyle(Theme.ink).lineLimit(1).truncationMode(.head)
                                Spacer(minLength: 4)
                                Text("+\(n.added)").foregroundStyle(Color(nsColor: .systemGreen))
                                Text("−\(n.removed)").foregroundStyle(Color(nsColor: .systemRed))
                            }
                            .font(Theme.mono(12))
                            .padding(.horizontal, 12).padding(.vertical, 7)
                            .background(cur?.path == f.path ? Theme.hover : .clear)
                            .contentShape(Rectangle())
                            .hand().onTapGesture { sel = f.path }
                        }
                    }
                }
                Spacer()
                if !files.isEmpty {
                    HStack {
                        Button("keep all") { files.forEach(project.accept) }
                        Button("undo all") { files.forEach(project.reject) }
                    }
                    .font(Theme.mono(12)).padding(12)
                }
            }
            .frame(width: 260)
            .background(Theme.side)
            Rectangle().fill(Theme.rule).frame(width: 1)
            VStack(spacing: 0) {
                if let f = cur {
                    HStack(spacing: 12) {
                        Text(f.short).font(Theme.mono(13)).foregroundStyle(Theme.ink).textSelection(.enabled)
                        Text(f.kind).font(Theme.mono(11)).foregroundStyle(Theme.faint)
                        Spacer()
                        Button("open") { NSWorkspace.shared.open(URL(fileURLWithPath: f.path)) }
                        Button("undo file") { project.reject(f) }
                        Button("keep") { project.accept(f) }.keyboardShortcut(.defaultAction)
                    }
                    .font(Theme.mono(12)).padding(12)
                    Rectangle().fill(Theme.rule).frame(height: 1)
                    let rows = Diff.rows(f.before, f.after)
                    ScrollView([.vertical, .horizontal]) {
                        LazyVStack(alignment: .leading, spacing: 0) {
                            ForEach(rows) { r in row(r) }
                        }
                        .padding(.vertical, 8)
                    }
                } else {
                    Text("nothing to review: no files changed this session").font(Theme.mono(12)).foregroundStyle(Theme.faint)
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                }
                Rectangle().fill(Theme.rule).frame(height: 1)
                HStack { Spacer(); Button("done") { dismiss() }.keyboardShortcut(.cancelAction) }.padding(10)
            }
        }
        .frame(minWidth: 900, minHeight: 600)
        .background(Theme.paper)
        .onAppear { project.loadChanges() }
    }

    private func row(_ r: DiffRow) -> some View {
        let bg: Color = r.kind == .added ? Color(nsColor: .systemGreen).opacity(0.13)
            : r.kind == .removed ? Color(nsColor: .systemRed).opacity(0.13) : .clear
        return HStack(spacing: 0) {
            Text(r.old.map(String.init) ?? "").frame(width: 44, alignment: .trailing).foregroundStyle(Theme.faint)
            Text(r.new.map(String.init) ?? "").frame(width: 44, alignment: .trailing).foregroundStyle(Theme.faint)
            Text(r.kind == .added ? " + " : r.kind == .removed ? " − " : "   ")
                .foregroundStyle(r.kind == .added ? Color(nsColor: .systemGreen) : Color(nsColor: .systemRed))
            Text(r.text.isEmpty ? " " : r.text).foregroundStyle(r.kind == .fold ? Theme.faint : Theme.ink)
                .fixedSize(horizontal: true, vertical: false)
            Spacer(minLength: 0)
        }
        .font(Theme.mono(12))
        .padding(.vertical, 1)
        .background(bg)
        .textSelection(.enabled)
    }
}
