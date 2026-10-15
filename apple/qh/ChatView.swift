import AppKit
import UniformTypeIdentifiers
import SwiftUI

/// A project: its tabs on top, the active tab's thread, permissions waiting, and the composer.
struct ProjectView: View {
    @ObservedObject var project: Project

    var body: some View {
        VStack(spacing: 0) {
            ChatTabBar(project: project)
            if project.state.hasPrefix("closed") {
                Banner(text: String(project.state.dropFirst(8)), retry: nil)
            }
            if project.showSetup {
                SetupView(project: project)
            } else if project.layout == "code" {
                CodeBrowser(project: project)
            } else if project.layout == "split" {
                HSplitView {
                    chat.frame(minWidth: 380)
                    CodePane(project: project).frame(minWidth: 360)
                }
            } else {
                chat
            }
        }
        .background(Theme.card)
        .sheet(item: $project.picker) { p in PickerSheet(picker: p, project: project) }
        .sheet(isPresented: $project.showReview) { ReviewView(project: project) }
        .sheet(isPresented: $project.showTasks) { TasksView(project: project) }
        // links in the thread point at this project's files
        .onAppear { if !project.tree.children.isEmpty { PathLinks.use(project.fileSet, root: project.path) } }
    }
}

extension ProjectView {
    /// The thread, waiting permissions and the composer: the chat column in every layout.
    @ViewBuilder var chat: some View {
        VStack(spacing: 0) {
            if let tab = project.current {
                ThreadView(tab: tab, repliesOnly: project.repliesOnly)
                    .environment(\.openURL, OpenURLAction { url in
                        guard url.scheme == "qh-file" else { return .systemAction }
                        // relative paths make opaque URLs (qh-file:a.py) whose url.path is empty
                        let c = URLComponents(url: url, resolvingAgainstBaseURL: false)
                        let line = c?.queryItems?.first { $0.name == "line" }?.value
                        project.openFile(c?.path ?? url.path, line: line.flatMap { Int($0) })
                        return .handled
                    })
                ForEach(project.asks.filter { $0.tab == tab.name }) { a in AskCard(ask: a, project: project) }
                Composer(project: project, tab: tab)
            } else {
                VStack(spacing: 10) {
                    ProgressView().controlSize(.small)
                    Text(project.state == "starting" ? "starting qh in \(project.name)…" : "no tabs")
                        .font(Theme.mono(12)).foregroundStyle(Theme.soft)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
    }
}

struct ChatTabBar: View {
    @ObservedObject var project: Project

    var body: some View {
        VStack(spacing: 0) {
            HStack(alignment: .bottom, spacing: 2) {
                ForEach(project.tabs) { t in ChatTabButton(tab: t, project: project) }
                Button { project.newTab() } label: {
                    Text("+").font(Theme.mono(14)).foregroundStyle(Theme.faint).frame(width: 30, height: 34).contentShape(Rectangle())
                }
                .buttonStyle(.hand).help("New tab (⌘T)")
                Text(project.name).font(Theme.mono(12)).foregroundStyle(Theme.faint).lineLimit(1)
                    .padding(.leading, 8).padding(.bottom, 9)
                Spacer()
                LayoutSwitch(layout: $project.layout).padding(.bottom, 5).padding(.trailing, 12)
                if !project.changes.isEmpty {
                    Button { project.loadChanges(); project.showReview = true } label: {
                        Text("\(project.changes.count) changed").font(Theme.mono(12)).foregroundStyle(Theme.accent)
                    }
                    .buttonStyle(.hand).padding(.bottom, 9).padding(.trailing, 10).help("Review changes (⌘D)")
                }
                Button { project.showTasks = true } label: {
                    Text("tasks").font(Theme.mono(12)).foregroundStyle(Theme.soft)
                }
                .buttonStyle(.hand).padding(.bottom, 9).padding(.trailing, 10).help("Background tasks (⌘⇧B)")
                Button { project.showSetup.toggle() } label: {
                    Text("models").font(Theme.mono(12)).foregroundStyle(project.showSetup ? Theme.ink : Theme.soft)
                }
                .buttonStyle(.hand).padding(.bottom, 9).padding(.trailing, 10).help("Connect models (⌘,)")
            }
            .padding(.horizontal, 12)
            .frame(height: 44, alignment: .bottom)
        }
    }
}

/// Chat | Split | Code, as a quiet pill group.
struct LayoutSwitch: View {
    @Binding var layout: String
    var body: some View {
        HStack(spacing: 2) {
            seg("Chat", "chat")
            seg("Split", "split")
            seg("Code", "code")
        }
        .padding(2)
        .background(RoundedRectangle(cornerRadius: 8, style: .continuous).fill(Theme.tile))
        .help("Chat, chat and code side by side, or code (⌃1 ⌃2 ⌃3)")
    }

    private func seg(_ label: String, _ tag: String) -> some View {
        let on = layout == tag
        return Button { layout = tag } label: {
            Text(label).font(Theme.sans(12)).foregroundStyle(on ? Theme.ink : Theme.soft)
                .padding(.horizontal, 10).frame(height: 24)
                .background(RoundedRectangle(cornerRadius: 6, style: .continuous).fill(on ? Theme.card : .clear)
                    .shadow(color: .black.opacity(on ? 0.08 : 0), radius: 1, y: 0.5))
                .contentShape(Rectangle())
        }
        .buttonStyle(.hand)
    }
}

struct ChatTabButton: View {
    @ObservedObject var tab: ChatTab
    @ObservedObject var project: Project

    var body: some View {
        let on = tab.name == project.current?.name
        HStack(spacing: 8) {
            StatusDot(status: tab.status, asking: project.asks.contains { $0.tab == tab.name })
            Text(tab.purpose.isEmpty ? tab.name : tab.purpose).font(Theme.mono(12)).foregroundStyle(on ? Theme.ink : Theme.soft).lineLimit(1)
            if project.tabs.count > 1 {
                Button { project.closeTab(tab.name) } label: {
                    Text("×").font(Theme.mono(13)).foregroundStyle(Theme.faint).frame(width: 18, height: 18).contentShape(Rectangle())
                }
                .buttonStyle(.hand).help("Close tab (⌘W)")
            }
        }
        .padding(.leading, 12).padding(.trailing, 8)
        .frame(height: 28)
        .background(RoundedRectangle(cornerRadius: 8, style: .continuous).fill(on ? Theme.tile : .clear))
        .padding(.bottom, 5)
        .contentShape(Rectangle())
        .hand().onTapGesture { project.active = tab.name }
    }
}

/// thinking = pulsing orange, waiting for you = solid orange, idle = faint.
struct StatusDot: View {
    let status: String
    let asking: Bool
    @State private var pulse = false

    var body: some View {
        Circle()
            .fill(asking || status == "working" ? Theme.accent : Theme.faint.opacity(0.5))
            .frame(width: 6, height: 6)
            .opacity(status == "working" && !asking ? (pulse ? 0.25 : 1) : 1)
            .animation(status == "working" ? .easeInOut(duration: 0.7).repeatForever() : .default, value: pulse)
            .onAppear { pulse = true }
            .help(asking ? "waiting for you" : status)
    }
}

/// Thinking and its effort, as /think sets it: off, on (the model's default), low, medium, high.
struct EffortPicker: View {
    @ObservedObject var project: Project
    @ObservedObject var tab: ChatTab
    private let levels = ["off", "on", "low", "medium", "high"]

    var body: some View {
        let cur = !tab.think ? "off" : tab.effort.isEmpty ? "on" : tab.effort
        Menu {
            ForEach(levels, id: \.self) { l in
                Button { project.prompt("/think \(l)") } label: { Text(l + (l == cur ? "  ✓" : "")) }
            }
        } label: {
            Text(cur == "off" ? "no thinking" : cur == "on" ? "thinking" : "\(cur) effort").font(Theme.mono(11)).foregroundStyle(Theme.faint)
        }
        .menuStyle(.borderlessButton).menuIndicator(.hidden)
        .fixedSize()
        .help("Thinking effort (/think)")
    }
}

struct ModePicker: View {
    @ObservedObject var project: Project
    var small = false
    private let modes = ["ask", "auto-edit", "auto", "plan"]

    var body: some View {
        Menu {
            ForEach(modes, id: \.self) { m in
                Button { project.setMode(m) } label: { Text(m + (m == project.mode ? "  ✓" : "")) }
            }
        } label: {
            Text(project.mode.isEmpty ? "mode" : project.mode).font(Theme.mono(small ? 11 : 12)).foregroundStyle(small ? Theme.faint : Theme.soft)
        }
        .menuStyle(.borderlessButton).menuIndicator(small ? .hidden : .visible)
        .fixedSize()
        .help("Mode: what qh may do without asking")
    }
}

struct ThreadView: View {
    @ObservedObject var tab: ChatTab
    var repliesOnly = false

    /// Replies only (⌘R): your messages and the agent's answers; tool calls, diffs and notes fold away.
    private var shown: [Item] {
        guard repliesOnly else { return tab.items }
        return tab.items.filter { if case .user = $0.kind { return true }; if case .reply = $0.kind { return true }; return false }
    }

    @State private var pinned = true

    var body: some View {
        ScrollViewReader { sv in
            ScrollView {
                // not lazy: a lazy stack guesses row heights and corrects them as you scroll, which
                // made the whole thread jump
                VStack(alignment: .leading, spacing: 14) {
                    let blocks = Activity.blocks(shown)
                    ForEach(Array(blocks.enumerated()), id: \.element.id) { i, b in
                        switch b {
                        case .item(let item): ItemView(item: item).equatable()
                        case .run(let run): ToolRun(items: run, live: tab.busy && i == blocks.count - 1)
                        }
                    }
                    if tab.status == "working" { ThinkingRow(tab: tab) }
                    Color.clear.frame(height: 1).id("end")
                }
                .frame(maxWidth: 700, alignment: .leading)
                .padding(.horizontal, 24).padding(.vertical, 20)
                .frame(maxWidth: .infinity)
            }
            .defaultScrollAnchor(.bottom)
            .atBottom($pinned)
            // follow new output only while you're at the bottom: scrolled up to read, you stay put
            .onChange(of: tab.items.count) { _, _ in if pinned { DispatchQueue.main.async { sv.scrollTo("end", anchor: .bottom) } } }
            .onChange(of: tab.status) { _, _ in if pinned { DispatchQueue.main.async { sv.scrollTo("end", anchor: .bottom) } } }
        }
        .overlay(alignment: .top) {
            // outside the scroll view: it's anchored to the bottom, and this belongs at the top
            if tab.items.isEmpty {
                Text("Ask qh anything about this project.").font(Theme.sans(15)).foregroundStyle(Theme.faint)
                    .frame(maxWidth: 720, alignment: .leading).padding(.horizontal, 24).padding(.top, 60).frame(maxWidth: .infinity)
                    .allowsHitTesting(false)
            }
        }
        .id(ObjectIdentifier(tab))   // a fresh scroll view per tab, never one left at another session's offset
    }
}

struct ItemView: View, Equatable {
    let item: Item
    static func == (a: ItemView, b: ItemView) -> Bool { a.item == b.item }

    var body: some View {
        switch item.kind {
        case .user(let t):
            // yours: on the right, in a grey bubble, a clear break before each turn
            HStack {
                Spacer(minLength: 80)
                Text(t).font(Theme.sans(15)).foregroundStyle(Theme.ink).textSelection(.enabled)
                    .padding(.horizontal, 14).padding(.vertical, 10)
                    .background(RoundedRectangle(cornerRadius: 14, style: .continuous).fill(Theme.tile))
            }
            .padding(.top, 18)
        case .reply(let t):
            Markdown(text: t)
        case .tool(let name, let args, let ok, let summary, let output):
            ToolCard(name: name, args: args, ok: ok, summary: summary, output: output)
        case .diff(let path, let added, let removed):
            DiffLine(path: path, added: added, removed: removed)
        case .notice(let t):
            Text(t).font(Theme.mono(12)).foregroundStyle(Theme.faint).textSelection(.enabled)
        case .error(let t):
            Text(t).font(Theme.mono(12)).foregroundStyle(Color(nsColor: .systemRed)).textSelection(.enabled)
        }
    }
}

/// An edit, as a file card: the path, the counts, and Review (keep or undo per file there).
struct DiffLine: View {
    let path: String, added: Int, removed: Int
    @EnvironmentObject var ws: Workspace
    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: "doc").font(.system(size: 12)).foregroundStyle(Theme.soft)
            Text(path).font(Theme.mono(13)).foregroundStyle(Theme.ink).lineLimit(1).truncationMode(.middle)
                .underline(PathLinks.resolve(path) != nil, color: Theme.faint)
                .hand().onTapGesture { ws.current?.openFile(path) }
            Spacer(minLength: 8)
            Text("+\(added) −\(removed)").font(Theme.mono(12)).foregroundStyle(Theme.soft)
            Button { ws.current?.loadChanges(); ws.current?.showReview = true } label: {
                Text("Review").font(Theme.sans(12)).foregroundStyle(Theme.card)
                    .padding(.horizontal, 12).frame(height: 28)
                    .background(RoundedRectangle(cornerRadius: 7, style: .continuous).fill(Theme.ink))
            }
            .buttonStyle(.hand).help("Keep or undo this session's changes (⌘D)")
        }
        .padding(.horizontal, 12).padding(.vertical, 8)
        .overlay(RoundedRectangle(cornerRadius: 10, style: .continuous).stroke(Theme.line, lineWidth: 1))
    }
}

struct ToolCard: View {
    let name: String
    let args: String
    let ok: Bool?
    let summary: String
    let output: String
    @State private var open = false
    @EnvironmentObject var ws: Workspace

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Button { open.toggle() } label: {
                HStack(spacing: 8) {
                    Text(ok == nil ? "◐" : ok! ? "✓" : "✕").foregroundStyle(ok == nil ? Theme.accent : ok! ? Theme.faint : Color(nsColor: .systemRed))
                    Text(name).foregroundStyle(Theme.soft)
                    Text(args).foregroundStyle(Theme.faint).lineLimit(1).truncationMode(.middle)
                        .underline(PathLinks.resolve(args) != nil, color: Theme.faint)
                        .hand().onTapGesture { if PathLinks.resolve(args) != nil { ws.current?.openFile(args) } else { open.toggle() } }
                    Spacer(minLength: 0)
                    if !summary.isEmpty || !output.isEmpty { Text(open ? "▾" : "▸").foregroundStyle(Theme.faint) }
                }
                .font(Theme.mono(12))
                .contentShape(Rectangle())
            }
            .buttonStyle(.hand)
            if open {
                if !summary.isEmpty { Text(summary).font(Theme.mono(11.5)).foregroundStyle(Theme.soft).textSelection(.enabled).padding(.leading, 20) }
                if !output.isEmpty {
                    ScrollView([.vertical, .horizontal]) {
                        Text(output).font(Theme.mono(11.5)).foregroundStyle(Theme.ink).textSelection(.enabled)
                            .fixedSize().frame(maxWidth: .infinity, alignment: .leading)
                    }
                    .frame(maxHeight: 280)
                    .padding(8).background(RoundedRectangle(cornerRadius: 5).fill(Theme.side))
                }
            }
        }
        .padding(.vertical, 1)
    }
}

/// Assistant text: markdown inline styles, fenced code blocks as their own boxes.
struct Markdown: View {
    let text: String

    /// Styled text per block (paragraph run or code block), cached by the block's own text: while a
    /// reply streams, every finished block is a cache hit and only the one being written is parsed.
    private static let cache: NSCache<NSString, Box> = { let c = NSCache<NSString, Box>(); c.countLimit = 2000; return c }()
    final class Box { let v: AttributedString; init(_ v: AttributedString) { self.v = v } }

    enum Kind { case para, code, heading(Int), bullets, table }

    private var parsed: [(kind: Kind, text: String, styled: AttributedString)] {
        Self.blocks(text).map { b in
            if case .code = b.kind { return (b.kind, b.text, AttributedString()) }
            let key = b.text as NSString
            if let hit = Self.cache.object(forKey: key) { return (b.kind, b.text, hit.v) }
            let v = Self.inline(b.text)
            Self.cache.setObject(Box(v), forKey: key)
            return (b.kind, b.text, v)
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            ForEach(Array(parsed.enumerated()), id: \.offset) { _, b in
                switch b.kind {
                case .code:
                    ScrollView(.horizontal) {
                        Text(b.text).font(Theme.mono(12.5)).foregroundStyle(Theme.ink).textSelection(.enabled).fixedSize().padding(12)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                    .background(RoundedRectangle(cornerRadius: 6).fill(Theme.side))
                    .overlay(alignment: .topTrailing) { CopyButton(text: b.text).padding(6) }
                case .heading(let n):
                    Text(b.styled).font(Theme.sans(n <= 1 ? 20 : n == 2 ? 17 : 15.5, medium: true)).foregroundStyle(Theme.ink)
                        .textSelection(.enabled).padding(.top, 6)
                case .bullets:
                    VStack(alignment: .leading, spacing: 5) {
                        ForEach(Array(b.text.split(separator: "\n").enumerated()), id: \.offset) { _, line in
                            HStack(alignment: .firstTextBaseline, spacing: 8) {
                                Text(Self.marker(line)).font(Theme.sans(15)).foregroundStyle(Theme.accent)
                                Text(Self.inline(Self.stripMarker(line))).font(Theme.sans(15)).foregroundStyle(Theme.ink).lineSpacing(3)
                                    .textSelection(.enabled)
                            }
                            .padding(.leading, CGFloat(line.prefix { $0 == " " }.count) * 6)
                        }
                    }
                case .table:
                    TableBlock(text: b.text)
                case .para:
                    Text(b.styled).font(Theme.sans(15)).foregroundStyle(Theme.ink).lineSpacing(3).textSelection(.enabled)
                }
            }
        }
    }

    static func isBullet(_ l: Substring) -> Bool {
        let t = l.drop { $0 == " " }
        if t.hasPrefix("- ") || t.hasPrefix("* ") || t.hasPrefix("• ") { return true }
        let digits = t.prefix { $0.isNumber }
        return !digits.isEmpty && t.dropFirst(digits.count).hasPrefix(". ")
    }
    static func marker(_ l: Substring) -> String {
        let t = l.drop { $0 == " " }
        let digits = t.prefix { $0.isNumber }
        return digits.isEmpty ? "•" : digits + "."
    }
    static func stripMarker(_ l: Substring) -> String {
        let t = l.drop { $0 == " " }
        let digits = t.prefix { $0.isNumber }
        return String(digits.isEmpty ? t.dropFirst(2) : t.dropFirst(digits.count + 2))
    }

    /// Code fences, # headings, runs of list lines, and paragraphs (split at blank lines).
    static func blocks(_ s: String) -> [(kind: Kind, text: String)] {
        var out: [(Kind, String)] = []
        var cur: [Substring] = [], curKind: Kind = .para, inCode = false
        func push() {
            let t = cur.joined(separator: "\n")
            if !t.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty { out.append((inCode ? .code : curKind, t)) }
            cur = []
            curKind = .para
        }
        for line in s.split(separator: "\n", omittingEmptySubsequences: false) {
            let trimmed = line.trimmingCharacters(in: .whitespaces)
            if trimmed.hasPrefix("```") { push(); inCode.toggle(); continue }
            if inCode { cur.append(line); continue }
            if trimmed.isEmpty { push(); continue }
            if trimmed.hasPrefix("#") {
                let level = trimmed.prefix { $0 == "#" }.count
                if level <= 6, trimmed.dropFirst(level).hasPrefix(" ") {
                    push()
                    out.append((.heading(level), String(trimmed.dropFirst(level + 1))))
                    continue
                }
            }
            if trimmed.hasPrefix("|") {
                if case .table = curKind {} else { push(); curKind = .table }
                cur.append(line)
                continue
            }
            if case .table = curKind { push() }
            let bullet = isBullet(line)
            if bullet, case .para = curKind, !cur.isEmpty { push() }
            if !bullet, case .bullets = curKind { push() }
            if bullet { curKind = .bullets }
            cur.append(line)
        }
        push()
        return out
    }

    static func inline(_ s: String) -> AttributedString {
        let a = (try? AttributedString(markdown: s, options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace))) ?? AttributedString(s)
        return PathLinks.link(a) { PathLinks.resolve($0) != nil }
    }
    static func clearCache() { cache.removeAllObjects() }
}

/// A markdown table: header row bold, hairline rules, columns sized to content, scrolls sideways if wide.
struct TableBlock: View {
    let text: String

    private var rows: [[String]] {
        text.split(separator: "\n").map { line in
            var t = line.trimmingCharacters(in: .whitespaces)
            if t.hasPrefix("|") { t.removeFirst() }
            if t.hasSuffix("|") { t.removeLast() }
            return t.split(separator: "|", omittingEmptySubsequences: false).map { $0.trimmingCharacters(in: .whitespaces) }
        }
        // the |---|:--:| line only says where the header ends
        .filter { r in !r.allSatisfy { c in !c.isEmpty && c.allSatisfy { "-: ".contains($0) } } }
    }

    var body: some View {
        let r = rows
        let cols = max(r.map(\.count).max() ?? 1, 1)
        // columns share the transcript's width: rows then wrap and grow to fit instead of overlapping
        // (a sideways scroll measured them at infinite width)
        let first: CGFloat = cols > 1 ? 130 : 700
        let rest = cols > 1 ? (700 - first - CGFloat(cols - 1) * 16) / CGFloat(cols - 1) : 0
        Grid(alignment: .topLeading, horizontalSpacing: 16, verticalSpacing: 0) {
            ForEach(Array(r.enumerated()), id: \.offset) { i, row in
                GridRow {
                    ForEach(0..<cols, id: \.self) { c in
                        Text(Markdown.inline(c < row.count ? row[c] : ""))
                            .font(Theme.sans(13.5, medium: i == 0 || c == 0)).foregroundStyle(Theme.ink)
                            .lineSpacing(2)
                            .fixedSize(horizontal: false, vertical: true)
                            .frame(width: c == 0 ? first : rest, alignment: .leading)
                            .textSelection(.enabled)
                            .padding(.vertical, 8)
                    }
                }
                if i < r.count - 1 {
                    Rectangle().fill(Theme.ink.opacity(i == 0 ? 0.18 : 0.08)).frame(height: 0.5).gridCellColumns(cols)
                }
            }
        }
    }
}

struct CopyButton: View {
    let text: String
    @State private var done = false
    var body: some View {
        Button {
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(text, forType: .string)
            done = true
        } label: { Text(done ? "copied" : "copy").font(Theme.mono(11)).foregroundStyle(Theme.faint) }
        .buttonStyle(.hand)
    }
}

struct Thinking: View {
    var body: some View {
        HStack(spacing: 6) {
            Cursor(width: 7, height: 14)
            Text("working").font(Theme.mono(12)).foregroundStyle(Theme.faint)
        }
    }
}

/// A permission, inline above the composer: a grey card with exactly what would run and the
/// three answers. Orange only when it crosses a safety line.
struct AskCard: View {
    let ask: Ask
    @ObservedObject var project: Project

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Text("Run \(ask.tool)?").font(Theme.sans(14, medium: true)).foregroundStyle(Theme.ink)
                if ask.guarded { Text("crosses a safety line").font(Theme.mono(12)).foregroundStyle(Theme.accent) }
            }
            ScrollView {
                Text(ask.detail).font(Theme.mono(12.5)).foregroundStyle(Theme.soft).textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .frame(maxHeight: 220)
            .fixedSize(horizontal: false, vertical: true)
            if !ask.warn.isEmpty { Text(ask.warn).font(Theme.mono(11.5)).foregroundStyle(Theme.accent) }
            HStack(spacing: 6) {
                AskButton(title: "Allow once", key: "⌘⏎", fill: Theme.ink, text: Theme.card) { project.answer(ask, "yes") }
                    .keyboardShortcut(.return, modifiers: .command)
                if !ask.always.isEmpty && !ask.guarded {
                    AskButton(title: "Always for \(ask.always)", fill: Theme.card, text: Theme.ink) { project.answer(ask, "always") }
                }
                AskButton(title: "Deny", key: "⌘⌫", fill: .clear, text: Theme.soft) { project.answer(ask, "no") }
                    .keyboardShortcut(.delete, modifiers: .command)
                Spacer(minLength: 0)
            }
        }
        .padding(14)
        .background(RoundedRectangle(cornerRadius: 12, style: .continuous).fill(Theme.tile))
        .overlay(RoundedRectangle(cornerRadius: 12, style: .continuous).stroke(ask.guarded ? Theme.accent.opacity(0.7) : .clear))
        .frame(maxWidth: 700)
        .padding(.horizontal, 24).padding(.bottom, 10)
    }
}

struct AskButton: View {
    let title: String
    var key = ""
    let fill: SwiftUI.Color
    let text: SwiftUI.Color
    let action: () -> Void
    var body: some View {
        Button(action: action) {
            HStack(spacing: 6) {
                Text(title).lineLimit(1)
                if !key.isEmpty { Text(key).font(Theme.mono(11)).opacity(0.6) }
            }
            .font(Theme.sans(13)).foregroundStyle(text)
            .padding(.horizontal, 14).frame(height: 32)
            .background(RoundedRectangle(cornerRadius: 8, style: .continuous).fill(fill))
            .contentShape(Rectangle())
        }
        .buttonStyle(.hand)
    }
}

struct Composer: View {
    @ObservedObject var project: Project
    @ObservedObject var tab: ChatTab
    @State private var text = ""
    @State private var images: [(name: String, b64: String)] = []
    @State private var sel = 0
    @State private var dropping = false
    @FocusState private var focused: Bool

    /// What the popup offers for the word being typed: commands after a leading /, files after @.
    private var suggestions: [(insert: String, label: String, about: String)] {
        if project.secret != nil { return [] }
        if text.hasPrefix("/") && !text.contains(" ") {
            return project.commands.filter { $0.cmd.hasPrefix(text) }.prefix(9).map { ($0.cmd + " ", $0.cmd, $0.about) }
        }
        if let at = text.split(separator: " ", omittingEmptySubsequences: false).last, at.hasPrefix("@") {
            let q = at.dropFirst().lowercased()
            let hits = project.files.lazy.filter { q.isEmpty || $0.lowercased().contains(q) }.prefix(9)
            return hits.map { f in (String(text.dropLast(at.count)) + "@" + f + " ", f, "") }
        }
        return []
    }

    var body: some View {
        let sugg = suggestions
        VStack(alignment: .leading, spacing: 6) {
            if !sugg.isEmpty {
                VStack(alignment: .leading, spacing: 0) {
                    ForEach(Array(sugg.enumerated()), id: \.offset) { i, s in
                        HStack(spacing: 12) {
                            Text(s.label).foregroundStyle(Theme.ink)
                            Text(s.about).foregroundStyle(Theme.faint).lineLimit(1)
                            Spacer(minLength: 0)
                        }
                        .font(Theme.mono(12)).padding(.horizontal, 10).padding(.vertical, 5)
                        .background(i == min(sel, sugg.count - 1) ? Theme.hover : .clear)
                        .contentShape(Rectangle())
                        .hand().onTapGesture { text = s.insert; sel = 0 }
                    }
                }
                .padding(.vertical, 4)
                .background(RoundedRectangle(cornerRadius: 8).fill(Theme.side))
                .overlay(RoundedRectangle(cornerRadius: 8).stroke(Theme.rule))
            }
            VStack(alignment: .leading, spacing: 8) {
                if !images.isEmpty {
                    HStack(spacing: 8) {
                        ForEach(Array(images.enumerated()), id: \.offset) { i, im in
                            HStack(spacing: 4) {
                                Text("▣ " + im.name).lineLimit(1)
                                Button("×") { images.remove(at: i) }.buttonStyle(.hand)
                            }
                            .font(Theme.mono(11)).foregroundStyle(Theme.accent)
                        }
                    }
                }
                Group {
                    if let p = project.secret {
                        SecureField("paste your \(p) API key, ⏎ to save", text: $text).onSubmit(send)
                    } else {
                        TextField(tab.busy ? "qh is working — Esc to stop, or type to queue" : "Ask anything · / commands · @ files",
                                  text: $text, axis: .vertical)
                            .lineLimit(1...10)
                    }
                }
                .textFieldStyle(.plain)
                .font(Theme.sans(15))
                .focused($focused)
                .onKeyPress(.return, phases: .down) { k in
                    if k.modifiers.contains(.shift) { return .ignored }
                    if !sugg.isEmpty { text = sugg[min(sel, sugg.count - 1)].insert; sel = 0; return .handled }
                    send()
                    return .handled
                }
                .onKeyPress(.tab) {
                    guard !sugg.isEmpty else { return .ignored }
                    text = sugg[min(sel, sugg.count - 1)].insert; sel = 0
                    return .handled
                }
                .onKeyPress(.downArrow) { guard !sugg.isEmpty else { return .ignored }; sel = min(sel + 1, sugg.count - 1); return .handled }
                .onKeyPress(.upArrow) { guard !sugg.isEmpty else { return .ignored }; sel = max(sel - 1, 0); return .handled }
                .onKeyPress(.escape) {
                    // Esc closes the popup first, then stops a running turn
                    if !sugg.isEmpty { text += " "; return .handled }
                    guard tab.busy else { return .ignored }
                    project.stop()
                    return .handled
                }
                .onChange(of: text) { _, _ in sel = 0 }
                HStack(spacing: 14) {
                    Button { attachPanel() } label: { Image(systemName: "plus").font(.system(size: 13)).foregroundStyle(Theme.soft).frame(width: 24, height: 24) }
                        .buttonStyle(.hand).help("Attach an image (or paste / drop one)").accessibilityLabel("Attach")
                    Button { project.prompt("/model") } label: { Text(tab.model).font(Theme.mono(11)).foregroundStyle(Theme.faint) }
                        .buttonStyle(.hand).help("Switch model (/model)")
                    EffortPicker(project: project, tab: tab)
                    ModePicker(project: project, small: true)
                    if tab.tokens >= 1000 { Text("\(tab.tokens / 1000)k tokens").font(Theme.mono(11)).foregroundStyle(Theme.faint) }
                    if let c = tab.cost, c > 0 { Text(String(format: "$%.3f", c)).font(Theme.mono(11)).foregroundStyle(Theme.faint) }
                    Spacer()
                    if tab.busy {
                        Button("stop") { project.stop() }.buttonStyle(.hand).font(Theme.mono(12)).foregroundStyle(Theme.accent)
                    }
                    let empty = text.isEmpty && images.isEmpty
                    Button { send() } label: {
                        Image(systemName: "arrow.up").font(.system(size: 13, weight: .semibold)).foregroundStyle(Theme.card)
                            .frame(width: 30, height: 30)
                            .background(RoundedRectangle(cornerRadius: 9, style: .continuous).fill(empty ? Theme.faint : Theme.ink))
                    }
                    .buttonStyle(.hand).disabled(empty).help("Send (⏎)").accessibilityLabel("Send")
                }
            }
            .padding(.horizontal, 14).padding(.top, 14).padding(.bottom, 8)
            .background(RoundedRectangle(cornerRadius: 14, style: .continuous).fill(Theme.card)
                .shadow(color: .black.opacity(0.06), radius: 9, y: 4))
            .overlay(RoundedRectangle(cornerRadius: 14, style: .continuous)
                .stroke(dropping ? Theme.accent : Theme.line, lineWidth: 1))
        }
        .frame(maxWidth: 700)
        .padding(.horizontal, 24).padding(.bottom, 18).padding(.top, 4)
        .onAppear { focused = true; if project.commands.isEmpty { project.loadCommands() } }
        .onChange(of: project.draft) { _, d in
            if let d { text = d; focused = true; project.draft = nil }
        }
        .onDrop(of: [.fileURL, .image], isTargeted: $dropping) { providers in
            for p in providers {
                _ = p.loadObject(ofClass: URL.self) { url, _ in
                    if let url { Task { @MainActor in add(url) } }
                }
            }
            return true
        }
        .onPasteCommand(of: [.png, .tiff, .fileURL]) { _ in pasteImage() }
    }

    private func send() {
        let t = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !t.isEmpty || !images.isEmpty else { return }
        project.prompt(t, images: images.map(\.b64))
        text = ""
        images = []
    }

    private func add(_ url: URL) {
        let ok = ["png", "jpg", "jpeg", "gif", "webp", "heic", "tiff"].contains(url.pathExtension.lowercased())
        if ok, let img = NSImage(contentsOf: url), let b = Self.png(img) {
            images.append((url.lastPathComponent, b))
        } else {
            // a dropped non-image file becomes an @ mention
            let rel = url.path.hasPrefix(project.path + "/") ? String(url.path.dropFirst(project.path.count + 1)) : url.path
            text += (text.isEmpty || text.hasSuffix(" ") ? "" : " ") + "@" + rel + " "
        }
    }

    private func pasteImage() {
        let pb = NSPasteboard.general
        if let urls = pb.readObjects(forClasses: [NSURL.self]) as? [URL], !urls.isEmpty { urls.forEach(add); return }
        if let img = NSImage(pasteboard: pb), let b = Self.png(img) { images.append(("pasted image", b)) }
    }

    private func attachPanel() {
        let p = NSOpenPanel()
        p.allowedContentTypes = [.image]
        p.allowsMultipleSelection = true
        if p.runModal() == .OK { p.urls.forEach(add) }
    }

    static func png(_ img: NSImage) -> String? {
        guard let t = img.tiffRepresentation, let rep = NSBitmapImageRep(data: t),
              let d = rep.representation(using: .png, properties: [:]) else { return nil }
        return d.base64EncodedString()
    }
}

/// /model, /provider and every other list a command opens: type to filter, ⏎ picks.
struct PickerSheet: View {
    let picker: PickerState
    @ObservedObject var project: Project
    @State private var filter = ""
    @State private var sel = 0

    var body: some View {
        let rows = picker.rows.filter { filter.isEmpty || $0.label.localizedCaseInsensitiveContains(filter) || $0.value.localizedCaseInsensitiveContains(filter) }
        VStack(alignment: .leading, spacing: 10) {
            Text(picker.title).font(Theme.sans(17, medium: true)).foregroundStyle(Theme.ink)
            TextField("type to filter \(picker.rows.count)", text: $filter).textFieldStyle(.plain).font(Theme.mono(13))
                .padding(9).background(RoundedRectangle(cornerRadius: 6).fill(Theme.side))
                .onSubmit { if let r = rows[safe: sel] { project.choose(picker, r.value) } }
                .onKeyPress(.downArrow) { sel = min(sel + 1, max(rows.count - 1, 0)); return .handled }
                .onKeyPress(.upArrow) { sel = max(sel - 1, 0); return .handled }
                .onChange(of: filter) { _, _ in sel = 0 }
            ScrollViewReader { sv in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 0) {
                        ForEach(Array(rows.enumerated()), id: \.element.id) { i, r in
                            if !r.group.isEmpty && (i == 0 || rows[i - 1].group != r.group) {
                                Text(r.group).font(Theme.mono(11)).foregroundStyle(Theme.faint).padding(.top, 10).padding(.bottom, 3).padding(.horizontal, 8)
                            }
                            HStack {
                                Text(r.label).foregroundStyle(Theme.ink)
                                Spacer()
                                if r.value == picker.current { Text("current").foregroundStyle(Theme.accent) }
                            }
                            .font(Theme.mono(12.5)).padding(.horizontal, 8).padding(.vertical, 6)
                            .background(RoundedRectangle(cornerRadius: 5).fill(i == sel ? Theme.hover : .clear))
                            .contentShape(Rectangle())
                            .hand().onTapGesture { project.choose(picker, r.value) }
                            .id(i)
                        }
                    }
                }
                .onChange(of: sel) { _, v in sv.scrollTo(v) }
            }
            HStack {
                Spacer()
                Button("cancel") { project.picker = nil }.keyboardShortcut(.cancelAction)
            }
        }
        .padding(20)
        .frame(width: 520, height: 480)
        .background(Theme.paper)
    }
}

extension Array {
    subscript(safe i: Int) -> Element? { indices.contains(i) ? self[i] : nil }
}

struct Banner: View {
    let text: String
    let retry: (() -> Void)?
    var body: some View {
        Text(text.isEmpty ? "qh stopped" : text).font(Theme.mono(12)).foregroundStyle(Theme.accent)
            .frame(maxWidth: .infinity, alignment: .leading).padding(.horizontal, 28).padding(.vertical, 8)
            .background(Theme.side)
    }
}

extension View {
    /// Keeps `pinned` true while the scroll view sits at (or near) its bottom.
    @ViewBuilder func atBottom(_ pinned: Binding<Bool>) -> some View {
        if #available(macOS 15.0, *) {
            onScrollGeometryChange(for: Bool.self) { g in
                g.contentOffset.y + g.containerSize.height >= g.contentSize.height - 60
            } action: { _, v in pinned.wrappedValue = v }
        } else {
            self
        }
    }
}
