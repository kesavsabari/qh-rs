import AppKit
import SwiftUI

/// A file to show in the code view, and the line to land on.
struct CodeTarget: Identifiable {
    let path: String   // relative to the project, or absolute outside it
    let line: Int?
    var id: String { path + ":" + String(line ?? 0) }
}

/// The project's files as a tree: folders first, each sorted. Built once per file list.
final class FileNode: Identifiable {
    let name: String
    let path: String          // relative; a folder's has no trailing slash
    var children: [FileNode]  // empty for a file
    let isDir: Bool
    var id: String { path }
    init(name: String, path: String, isDir: Bool) { self.name = name; self.path = path; self.isDir = isDir; children = [] }

    static func build(_ files: [String]) -> FileNode {
        let root = FileNode(name: "", path: "", isDir: true)
        var dirs: [String: FileNode] = ["": root]
        for f in files {
            var parent = root
            let parts = f.split(separator: "/")
            for (i, p) in parts.enumerated() {
                let sub = parts[0...i].joined(separator: "/")
                if i == parts.count - 1 {
                    parent.children.append(FileNode(name: String(p), path: sub, isDir: false))
                } else if let d = dirs[sub] {
                    parent = d
                } else {
                    let d = FileNode(name: String(p), path: sub, isDir: true)
                    parent.children.append(d)
                    dirs[sub] = d
                    parent = d
                }
            }
        }
        func sort(_ n: FileNode) {
            n.children.sort { a, b in a.isDir != b.isDir ? a.isDir : a.name.localizedStandardCompare(b.name) == .orderedAscending }
            n.children.filter(\.isDir).forEach(sort)
        }
        sort(root)
        return root
    }
}

/// The inspector's file tree: what git sees (so .gitignore holds), folders fold, a click opens the code view.
struct FileTree: View {
    @ObservedObject var project: Project
    @State private var open: Set<String> = []
    @State private var filter = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 1) {
            TextField("filter files", text: $filter).textFieldStyle(.plain).font(Theme.mono(11.5))
                .padding(.horizontal, 8).padding(.vertical, 5)
                .background(RoundedRectangle(cornerRadius: 6).fill(Theme.side)).padding(.horizontal, 6).padding(.bottom, 4)
            if !filter.isEmpty {
                let q = filter.lowercased()
                ForEach(project.files.lazy.filter { $0.lowercased().contains(q) }.prefix(200), id: \.self) { f in
                    fileRow(name: f, path: f, depth: 0)
                }
            } else {
                ForEach(rows, id: \.node.id) { r in
                    if r.node.isDir {
                        Row(selected: false, action: { toggle(r.node.path) }) {
                            HStack(spacing: 5) {
                                Image(systemName: open.contains(r.node.path) ? "chevron.down" : "chevron.right")
                                    .font(.system(size: 8.5, weight: .semibold)).foregroundStyle(Theme.faint).frame(width: 10)
                                Image(systemName: "folder").font(.system(size: 11)).foregroundStyle(Theme.soft)
                                Text(r.node.name).font(Theme.mono(11.5)).foregroundStyle(Theme.ink).lineLimit(1)
                            }
                            .padding(.leading, CGFloat(r.depth) * 12)
                        }
                    } else {
                        fileRow(name: r.node.name, path: r.node.path, depth: r.depth)
                    }
                }
            }
            if project.files.isEmpty {
                Text("No files.").font(Theme.sans(12.5)).foregroundStyle(Theme.faint).padding(.horizontal, 8)
            }
        }
    }

    private func fileRow(name: String, path: String, depth: Int) -> some View {
        let changed = project.changes.contains { $0.short == path }
        return Row(selected: project.code?.path == path, action: { project.openFile(path) }) {
            HStack(spacing: 5) {
                Color.clear.frame(width: 10, height: 1)
                Image(systemName: "doc.text").font(.system(size: 10.5)).foregroundStyle(Theme.faint)
                Text(name).font(Theme.mono(11.5)).foregroundStyle(changed ? Theme.accent : Theme.ink).lineLimit(1).truncationMode(.head)
            }
            .padding(.leading, CGFloat(depth) * 12)
        }
        .contextMenu {
            Button("Open in your editor") { Editor.open(project.absolute(path), line: nil) }
            Button("Reveal in Finder") { NSWorkspace.shared.selectFile(project.absolute(path), inFileViewerRootedAtPath: "") }
        }
        .help(path)
    }

    private func toggle(_ p: String) { if open.contains(p) { open.remove(p) } else { open.insert(p) } }

    /// The rows showing now: every node whose folders are all open.
    private var rows: [(node: FileNode, depth: Int)] {
        var out: [(FileNode, Int)] = []
        func walk(_ n: FileNode, _ d: Int) {
            for c in n.children {
                out.append((c, d))
                if c.isDir && open.contains(c.path) { walk(c, d + 1) }
            }
        }
        walk(project.tree, 0)
        return out
    }
}

/// "Open in your editor": $VISUAL/$EDITOR when it names an editor app we know (with the line), else the
/// file's default app.
enum Editor {
    static func open(_ path: String, line: Int?) {
        let env = ProcessInfo.processInfo.environment
        let name = ((env["VISUAL"] ?? env["EDITOR"] ?? "").split(separator: " ").first.map(String.init) ?? "") as NSString
        // CLI name → (app, how it takes a line)
        let known: [String: (app: String, arg: (String, Int) -> [String])] = [
            "code": ("Visual Studio Code", { ["-g", "\($0):\($1)"] }),
            "cursor": ("Cursor", { ["-g", "\($0):\($1)"] }),
            "zed": ("Zed", { ["\($0):\($1)"] }),
            "subl": ("Sublime Text", { ["\($0):\($1)"] }),
        ]
        if let k = known[name.lastPathComponent],
           let app = NSWorkspace.shared.urlForApplication(withBundleIdentifier: bundle(k.app)) ?? findApp(k.app) {
            // the editor's own CLI takes a line; the app GUI launch from Finder has no PATH, so look in the usual places
            let cli = name.isAbsolutePath ? [name as String]
                : ["/usr/local/bin", "/opt/homebrew/bin", NSHomeDirectory() + "/.local/bin"].map { $0 + "/" + (name as String) }
            if let line, let cli = cli.first(where: { FileManager.default.isExecutableFile(atPath: $0) }) {
                let p = Process()
                p.executableURL = URL(fileURLWithPath: cli)
                p.arguments = k.arg(path, line)
                try? p.run()
                return
            }
            NSWorkspace.shared.open([URL(fileURLWithPath: path)], withApplicationAt: app, configuration: NSWorkspace.OpenConfiguration())
            return
        }
        NSWorkspace.shared.open(URL(fileURLWithPath: path))
    }

    private static func bundle(_ app: String) -> String {
        ["Visual Studio Code": "com.microsoft.VSCode", "Cursor": "com.todesktop.230313mzl4w4u92",
         "Zed": "dev.zed.Zed", "Sublime Text": "com.sublimetext.4"][app] ?? ""
    }
    private static func findApp(_ app: String) -> URL? {
        let u = URL(fileURLWithPath: "/Applications/\(app).app")
        return FileManager.default.fileExists(atPath: u.path) ? u : nil
    }
}

/// Split layout's right side: the open file, or how to open one.
struct CodePane: View {
    @ObservedObject var project: Project
    var body: some View {
        if let t = project.code {
            CodeView(target: t, project: project)
        } else {
            VStack(spacing: 8) {
                Text("No file open").font(Theme.sans(13, medium: true)).foregroundStyle(Theme.soft)
                Text("Click a path in the chat, or pick one in Code (⌃3).").font(Theme.sans(12)).foregroundStyle(Theme.faint)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity).background(Theme.paper)
        }
    }
}

/// Code layout: the project's files on the left, the open file on the right.
struct CodeBrowser: View {
    @ObservedObject var project: Project
    var body: some View {
        HSplitView {
            ScrollView { FileTree(project: project).padding(8) }
                .frame(minWidth: 200, idealWidth: 260, maxWidth: 420).background(Theme.side)
            CodePane(project: project).frame(minWidth: 400)
        }
        .onAppear { if project.tree.children.isEmpty { project.loadFiles() } }
    }
}

/// Read-only code with line numbers and syntax colour; lands on the line it was opened at.
struct CodeView: View {
    let target: CodeTarget
    @ObservedObject var project: Project
    @State private var lines: [String] = []
    @State private var note = ""
    @State private var reloads = 0          // bumps when the file changes on disk
    @State private var watch: FileWatch?
    @State private var landed = ""          // the target we've already scrolled to
    @State private var width: CGFloat = 0   // widest line: a lazy stack only measures the rows on screen
    @AppStorage("codeSize") private var codeSize = 12.0   // Settings → Appearance

    var body: some View {
        let abs = project.absolute(target.path)
        let lang = Syntax.lang(for: abs)
        VStack(spacing: 0) {
            HStack(spacing: 12) {
                Text(target.path).font(Theme.mono(13)).foregroundStyle(Theme.ink).textSelection(.enabled).lineLimit(1).truncationMode(.head)
                if let l = target.line { Text("line \(l)").font(Theme.mono(11)).foregroundStyle(Theme.faint) }
                Text("read-only").font(Theme.mono(11)).foregroundStyle(Theme.faint)
                Spacer()
                Button("Reveal in Finder") { NSWorkspace.shared.selectFile(abs, inFileViewerRootedAtPath: "") }
                Button("Open in your editor") { Editor.open(abs, line: target.line) }.keyboardShortcut("e")
                if project.layout == "split" {
                    Button { project.layout = "chat" } label: { Image(systemName: "xmark") }.buttonStyle(.hand)
                        .foregroundStyle(Theme.soft).help("Close the code pane")
                }
            }
            .font(Theme.mono(12)).padding(12)
            Rectangle().fill(Theme.rule).frame(height: 1)
            if !note.isEmpty {
                Text(note).font(Theme.mono(12)).foregroundStyle(Theme.faint).frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                ScrollViewReader { sv in
                    ScrollView([.vertical, .horizontal]) {
                        LazyVStack(alignment: .leading, spacing: 0) {
                            ForEach(Array(lines.enumerated()), id: \.offset) { i, l in
                                HStack(spacing: 0) {
                                    Text("\(i + 1)").frame(width: 48, alignment: .trailing).foregroundStyle(Theme.faint).padding(.trailing, 14)
                                    Text(Syntax.color(l, lang)).fixedSize(horizontal: true, vertical: false)
                                    Spacer(minLength: 0)
                                }
                                .font(Theme.mono(codeSize))
                                .padding(.vertical, 1)
                                .background(target.line == i + 1 ? Theme.accent.opacity(0.14) : .clear)
                                .id(i + 1)
                            }
                        }
                        .frame(minWidth: width, alignment: .leading)
                        .textSelection(.enabled)
                        .padding(.vertical, 8)
                    }
                    .defaultScrollAnchor(.topLeading)
                    .onChange(of: lines.count) { _, _ in
                        // land on the line once per file opened; a live reload keeps your place
                        guard landed != target.id else { return }
                        landed = target.id
                        if let l = target.line { DispatchQueue.main.async { sv.scrollTo(l, anchor: UnitPoint(x: 0, y: 0.5)) } }
                    }
                }
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Theme.paper)
        .task(id: "\(target.id)#\(reloads)#\(codeSize)") { load(abs) }
        .onAppear { watch = FileWatch(abs) { reloads += 1 } }
        .onChange(of: target.id) { _, _ in watch = FileWatch(project.absolute(target.path)) { reloads += 1 } }
    }

    private func load(_ abs: String) {
        note = ""
        guard let d = FileManager.default.contents(atPath: abs) else { note = "can't read \(target.path)"; return }
        if d.count > 4_000_000 { note = "too big to show here (\(d.count / 1_000_000) MB): open it in your editor"; return }
        if d.prefix(8000).contains(0) { note = "binary file: open it in your editor"; return }
        let s = String(decoding: d, as: UTF8.self)
        lines = s.split(separator: "\n", omittingEmptySubsequences: false).map { String($0.replacingOccurrences(of: "\t", with: "    ")) }
        if lines.last == "" { lines.removeLast() }
        let cw = ("M" as NSString).size(withAttributes: [.font: Theme.monoNS(codeSize)]).width
        width = 62 + CGFloat(lines.map(\.count).max() ?? 0) * cw + 24
    }
}

/// A small line-at-a-time highlighter: comments, strings, numbers and keywords. Block comments
/// aren't tracked across lines; this is for reading, not an IDE.
enum Syntax {
    struct Lang { let line: [String]; let keywords: Set<String> }

    static func lang(for path: String) -> Lang? {
        let ext = (path as NSString).pathExtension.lowercased()
        let c = ["if", "else", "for", "while", "return", "break", "continue", "switch", "case", "default", "do", "const", "static", "struct", "enum", "true", "false", "null"]
        let table: [([String], [String], [String])] = [
            (["rs"], ["//"], c + ["fn", "let", "mut", "pub", "use", "mod", "impl", "trait", "match", "loop", "in", "self", "Self", "crate", "super", "where", "as", "ref", "move", "async", "await", "dyn", "type", "unsafe", "Some", "None", "Ok", "Err"]),
            (["swift"], ["//"], c + ["func", "let", "var", "import", "class", "protocol", "extension", "guard", "in", "self", "Self", "init", "private", "public", "static", "some", "any", "nil", "try", "throws", "async", "await", "final", "override", "where", "case"]),
            (["js", "jsx", "ts", "tsx", "mjs", "cjs"], ["//"], c + ["function", "let", "var", "import", "export", "from", "class", "new", "this", "async", "await", "typeof", "interface", "type", "undefined", "of", "in", "try", "catch", "throw"]),
            (["go"], ["//"], c + ["func", "var", "package", "import", "type", "interface", "go", "defer", "range", "map", "chan", "nil", "select"]),
            (["c", "h", "cc", "cpp", "hpp", "m", "mm", "java", "kt", "cs"], ["//"], c + ["int", "char", "void", "unsigned", "long", "float", "double", "class", "public", "private", "new", "this", "include", "define", "fun", "val", "var", "import", "package"]),
            (["py"], ["#"], ["def", "class", "import", "from", "return", "if", "elif", "else", "for", "while", "in", "not", "and", "or", "is", "None", "True", "False", "with", "as", "try", "except", "finally", "raise", "lambda", "yield", "self", "pass", "async", "await"]),
            (["sh", "bash", "zsh", "fish"], ["#"], ["if", "then", "else", "elif", "fi", "for", "do", "done", "while", "case", "esac", "function", "return", "local", "export", "in"]),
            (["rb"], ["#"], ["def", "end", "class", "module", "if", "else", "elsif", "unless", "do", "return", "nil", "true", "false", "self", "require"]),
            (["toml", "yml", "yaml", "conf", "ini"], ["#"], ["true", "false"]),
            (["json"], [], ["true", "false", "null"]),
            (["sql"], ["--"], ["select", "from", "where", "insert", "update", "delete", "create", "table", "join", "on", "and", "or", "not", "null", "SELECT", "FROM", "WHERE", "INSERT", "UPDATE", "DELETE", "CREATE", "TABLE", "JOIN", "ON", "AND", "OR", "NOT", "NULL"]),
        ]
        for (exts, line, kw) in table where exts.contains(ext) { return Lang(line: line, keywords: Set(kw)) }
        return nil
    }

    static let keyword = SwiftUI.Color(nsColor: NSColor(name: nil) { ap in
        Theme.ns(ap.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? 0xFF7A52 : 0xD23C0E) })
    static let string = SwiftUI.Color(nsColor: NSColor(name: nil) { ap in
        Theme.ns(ap.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? 0x8FD18A : 0x2F7D3A) })
    static let number = SwiftUI.Color(nsColor: NSColor(name: nil) { ap in
        Theme.ns(ap.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? 0x9AA0FF : 0x373FF5) })

    static func color(_ line: String, _ lang: Lang?) -> AttributedString {
        var out = AttributedString()
        func add(_ s: Substring, _ c: SwiftUI.Color) { var a = AttributedString(String(s)); a.foregroundColor = c; out += a }
        guard let lang else { add(Substring(line.isEmpty ? " " : line), Theme.ink); return out }
        var i = line.startIndex
        while i < line.endIndex {
            let rest = line[i...]
            if let m = lang.line.first(where: { rest.hasPrefix($0) }) {
                _ = m
                add(rest, Theme.faint)
                break
            }
            let ch = line[i]
            if ch == "\"" || ch == "'" || ch == "`" {
                var j = line.index(after: i)
                while j < line.endIndex && line[j] != ch {
                    if line[j] == "\\" { j = line.index(after: j); if j == line.endIndex { break } }
                    j = line.index(after: j)
                }
                if j < line.endIndex { j = line.index(after: j) }
                add(line[i..<j], string)
                i = j
                continue
            }
            if ch.isLetter || ch == "_" {
                let j = rest.firstIndex { !($0.isLetter || $0.isNumber || $0 == "_") } ?? line.endIndex
                let w = line[i..<j]
                add(w, lang.keywords.contains(String(w)) ? keyword : Theme.ink)
                i = j
                continue
            }
            if ch.isNumber {
                let j = rest.firstIndex { !($0.isNumber || $0.isLetter || $0 == "." || $0 == "_") } ?? line.endIndex
                add(line[i..<j], number)
                i = j
                continue
            }
            let j = rest.firstIndex { $0.isLetter || $0.isNumber || $0 == "_" || $0 == "\"" || $0 == "'" || $0 == "`" || $0 == "/" || $0 == "#" || $0 == "-" } ?? line.endIndex
            let end = j == i ? line.index(after: i) : j
            add(line[i..<end], Theme.ink)
            i = end
        }
        if out.characters.isEmpty { out = AttributedString(" ") }
        return out
    }
}

/// Paths in the thread become links: `qh-file:` URLs the thread opens in the code view.
enum PathLinks {
    /// a path-looking word: has a dot extension or a slash, optional :line
    nonisolated(unsafe) static var files: Set<String> = []
    nonisolated(unsafe) static var root = ""
    nonisolated(unsafe) static var byName: [String: [String]] = [:]

    /// The project whose thread is drawn: its files decide what's a link.
    static func use(_ f: Set<String>, root r: String) {
        guard f != files || r != root else { return }
        files = f
        root = r
        byName = Dictionary(grouping: f, by: { ($0 as NSString).lastPathComponent })
        Markdown.clearCache()
    }

    static func resolve(_ p: String) -> String? { resolve(p, files: files, root: root) }

    /// A path as the agent wrote it → the project file it means: exact, absolute inside the project,
    /// or a tail of exactly one file's path ("ChatView.swift", "qh/ChatView.swift").
    static func resolve(_ p: String, files: Set<String>, root: String) -> String? {
        var p = p.trimmingCharacters(in: .whitespaces)
        if let c = p.lastIndex(of: ":"), Int(p[p.index(after: c)...]) != nil { p = String(p[..<c]) }
        if p.hasPrefix("./") { p.removeFirst(2) }
        if !root.isEmpty, p.hasPrefix(root + "/") { p = String(p.dropFirst(root.count + 1)) }
        if p.isEmpty || files.isEmpty { return nil }
        if files.contains(p) { return p }
        let name = (p as NSString).lastPathComponent
        let pool = files == Self.files ? (byName[name] ?? []) : files.filter { ($0 as NSString).lastPathComponent == name }
        let hits = pool.filter { $0.hasSuffix("/" + p) }
        return hits.count == 1 ? hits[0] : nil
    }

    nonisolated(unsafe) static let re = try! NSRegularExpression(pattern: #"(?<![\w/.~-])((?:[\w.~-]+/)*[\w.-]+\.[A-Za-z0-9]{1,8})(?::(\d+))?(?![\w/])"#)

    static func link(_ s: AttributedString, exists: (String) -> Bool) -> AttributedString {
        var out = s
        let plain = String(s.characters)
        let ns = plain as NSString
        for m in re.matches(in: plain, range: NSRange(location: 0, length: ns.length)).reversed() {
            let path = ns.substring(with: m.range(at: 1))
            guard exists(path), let r = Range(m.range, in: plain),
                  let lo = AttributedString.Index(r.lowerBound, within: out), let hi = AttributedString.Index(r.upperBound, within: out) else { continue }
            if out[lo..<hi].link != nil { continue }
            let line = m.range(at: 2).location != NSNotFound ? ns.substring(with: m.range(at: 2)) : ""
            var c = URLComponents()
            c.scheme = "qh-file"
            c.path = path
            if !line.isEmpty { c.queryItems = [URLQueryItem(name: "line", value: line)] }
            out[lo..<hi].link = c.url
            out[lo..<hi].underlineStyle = .single
        }
        return out
    }
}
