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
            } else if let tab = project.current {
                ThreadView(tab: tab, repliesOnly: project.repliesOnly)
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
        .background(Theme.paper)
        .sheet(item: $project.picker) { p in PickerSheet(picker: p, project: project) }
        .sheet(isPresented: $project.showReview) { ReviewView(project: project) }
        .sheet(isPresented: $project.showTasks) { TasksView(project: project) }
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
                .buttonStyle(.plain).help("New tab (⌘T)")
                Spacer()
                if !project.changes.isEmpty {
                    Button { project.loadChanges(); project.showReview = true } label: {
                        Text("\(project.changes.count) changed").font(Theme.mono(12)).foregroundStyle(Theme.accent)
                    }
                    .buttonStyle(.plain).padding(.bottom, 9).padding(.trailing, 10).help("Review changes (⌘D)")
                }
                Button { project.showTasks = true } label: {
                    Text("tasks").font(Theme.mono(12)).foregroundStyle(Theme.soft)
                }
                .buttonStyle(.plain).padding(.bottom, 9).padding(.trailing, 10).help("Background tasks (⌘⇧B)")
                Button { project.showSetup.toggle() } label: {
                    Text("models").font(Theme.mono(12)).foregroundStyle(project.showSetup ? Theme.ink : Theme.soft)
                }
                .buttonStyle(.plain).padding(.bottom, 9).padding(.trailing, 10).help("Connect models (⌘,)")
                ModePicker(project: project).padding(.bottom, 6)
            }
            .padding(.horizontal, 12)
            .frame(height: 42, alignment: .bottom)
            Rectangle().fill(Theme.rule).frame(height: 1)
        }
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
                .buttonStyle(.plain).help("Close tab (⌘W)")
            }
        }
        .padding(.leading, 12).padding(.trailing, 8)
        .frame(height: 34)
        .overlay(alignment: .bottom) { Rectangle().fill(on ? Theme.accent : .clear).frame(height: 2) }
        .contentShape(Rectangle())
        .onTapGesture { project.active = tab.name }
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

struct ModePicker: View {
    @ObservedObject var project: Project
    private let modes = ["ask", "auto-edit", "auto", "plan"]

    var body: some View {
        Menu {
            ForEach(modes, id: \.self) { m in
                Button { project.setMode(m) } label: { Text(m + (m == project.mode ? "  ✓" : "")) }
            }
        } label: {
            Text(project.mode.isEmpty ? "mode" : project.mode).font(Theme.mono(12)).foregroundStyle(Theme.soft)
        }
        .menuStyle(.borderlessButton)
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

    var body: some View {
        ScrollViewReader { sv in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 14) {
                    if tab.items.isEmpty {
                        Text("Ask qh anything about this project.").font(Theme.sans(15)).foregroundStyle(Theme.faint).padding(.top, 40)
                    }
                    ForEach(shown) { item in ItemView(item: item).equatable() }
                    if tab.status == "working" { Thinking() }
                    Color.clear.frame(height: 1).id("end")
                }
                .frame(maxWidth: 720, alignment: .leading)
                .padding(.horizontal, 24).padding(.vertical, 20)
                .frame(maxWidth: .infinity)
            }
            .onChange(of: tab.items.count) { _, _ in sv.scrollTo("end", anchor: .bottom) }
            .onChange(of: tab.status) { _, _ in sv.scrollTo("end", anchor: .bottom) }
            .onAppear { sv.scrollTo("end", anchor: .bottom) }
        }
    }
}

struct ItemView: View, Equatable {
    let item: Item
    static func == (a: ItemView, b: ItemView) -> Bool { a.item == b.item }

    var body: some View {
        switch item.kind {
        case .user(let t):
            // yours: on the right, in a tinted bubble with an orange edge, a clear break before each turn
            HStack {
                Spacer(minLength: 80)
                Text(t).font(Theme.sans(15)).foregroundStyle(Theme.ink).textSelection(.enabled)
                    .padding(.horizontal, 14).padding(.vertical, 10)
                    .background(RoundedRectangle(cornerRadius: 14, style: .continuous).fill(Theme.ink.opacity(0.07)))
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

struct DiffLine: View {
    let path: String, added: Int, removed: Int
    @EnvironmentObject var ws: Workspace
    var body: some View {
        Button { ws.current?.loadChanges(); ws.current?.showReview = true } label: {
            HStack(spacing: 10) {
                Text("edited").foregroundStyle(Theme.faint)
                Text(path).foregroundStyle(Theme.ink)
                Text("+\(added)").foregroundStyle(Color(nsColor: .systemGreen))
                Text("−\(removed)").foregroundStyle(Color(nsColor: .systemRed))
                Text("review").foregroundStyle(Theme.faint)
            }
            .font(Theme.mono(12))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help("Review this session's changes")
    }
}

struct ToolCard: View {
    let name: String
    let args: String
    let ok: Bool?
    let summary: String
    let output: String
    @State private var open = false

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Button { open.toggle() } label: {
                HStack(spacing: 8) {
                    Text(ok == nil ? "◐" : ok! ? "✓" : "✕").foregroundStyle(ok == false ? Color(nsColor: .systemRed) : Theme.accent)
                    Text(name).foregroundStyle(Theme.ink)
                    Text(args).foregroundStyle(Theme.soft).lineLimit(1).truncationMode(.middle)
                    Spacer(minLength: 0)
                    if !summary.isEmpty || !output.isEmpty { Text(open ? "▾" : "▸").foregroundStyle(Theme.faint) }
                }
                .font(Theme.mono(12))
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            if open {
                if !summary.isEmpty { Text(summary).font(Theme.mono(11.5)).foregroundStyle(Theme.soft).textSelection(.enabled).padding(.leading, 20) }
                if !output.isEmpty {
                    ScrollView {
                        Text(output).font(Theme.mono(11.5)).foregroundStyle(Theme.ink).textSelection(.enabled)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                    .frame(maxHeight: 280)
                    .padding(8).background(RoundedRectangle(cornerRadius: 5).fill(Theme.side))
                }
            }
        }
        .padding(.horizontal, 12).padding(.vertical, 8)
        .background(RoundedRectangle(cornerRadius: 10, style: .continuous).fill(Theme.ink.opacity(0.03)))
        .overlay(RoundedRectangle(cornerRadius: 10, style: .continuous).stroke(Theme.ink.opacity(0.08), lineWidth: 0.5))
    }
}

/// Assistant text: markdown inline styles, fenced code blocks as their own boxes.
struct Markdown: View {
    let text: String

    /// Styled text per block (paragraph run or code block), cached by the block's own text: while a
    /// reply streams, every finished block is a cache hit and only the one being written is parsed.
    private static let cache: NSCache<NSString, Box> = { let c = NSCache<NSString, Box>(); c.countLimit = 2000; return c }()
    final class Box { let v: AttributedString; init(_ v: AttributedString) { self.v = v } }

    enum Kind { case para, code, heading(Int), bullets }

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
                    ScrollView(.horizontal, showsIndicators: false) {
                        Text(b.text).font(Theme.mono(12.5)).foregroundStyle(Theme.ink).textSelection(.enabled).padding(12)
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
        (try? AttributedString(markdown: s, options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace))) ?? AttributedString(s)
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
        .buttonStyle(.plain)
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

/// A permission, inline above the composer: exactly what would run, and the three answers.
struct AskCard: View {
    let ask: Ask
    @ObservedObject var project: Project

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Text("qh wants to run").foregroundStyle(Theme.soft)
                Text(ask.tool).foregroundStyle(Theme.ink)
                if ask.guarded { Text("· crosses a safety line").foregroundStyle(Theme.accent) }
            }
            .font(Theme.mono(12))
            ScrollView {
                Text(ask.detail).font(Theme.mono(12)).foregroundStyle(Theme.ink).textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .frame(maxHeight: 220)
            .padding(10)
            .background(RoundedRectangle(cornerRadius: 6).fill(Theme.side))
            if !ask.warn.isEmpty { Text(ask.warn).font(Theme.mono(11.5)).foregroundStyle(Theme.accent) }
            HStack(spacing: 10) {
                Button("Allow") { project.answer(ask, "yes") }.keyboardShortcut(.return, modifiers: .command)
                if !ask.always.isEmpty {
                    Button("Always allow \(ask.always)") { project.answer(ask, "always") }
                }
                Button("Deny") { project.answer(ask, "no") }.keyboardShortcut(.delete, modifiers: .command)
                Spacer()
                Text("⌘⏎ allow · ⌘⌫ deny").font(Theme.mono(11)).foregroundStyle(Theme.faint)
            }
            .font(Theme.mono(12))
        }
        .padding(14)
        .background(RoundedRectangle(cornerRadius: 8).fill(Theme.paper))
        .overlay(RoundedRectangle(cornerRadius: 8).stroke(Theme.accent.opacity(0.7)))
        .frame(maxWidth: 820)
        .padding(.horizontal, 28).padding(.bottom, 10)
    }
}

/// ⏎ sends, ⇧⏎ new line, Esc stops a running turn. `/` offers commands, `@` project files,
/// ⌘V or a drop attaches images. When a command asks for an API key the field masks it.
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
                        .onTapGesture { text = s.insert; sel = 0 }
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
                                Button("×") { images.remove(at: i) }.buttonStyle(.plain)
                            }
                            .font(Theme.mono(11)).foregroundStyle(Theme.accent)
                        }
                    }
                }
                Group {
                    if let p = project.secret {
                        SecureField("paste your \(p) API key, ⏎ to save", text: $text).onSubmit(send)
                    } else {
                        TextField(tab.busy ? "qh is working — Esc to stop, or type to queue" : "Message qh   / commands   @ files",
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
                .onChange(of: text) { _, _ in sel = 0 }
                HStack(spacing: 14) {
                    Button { project.prompt("/model") } label: { Text(tab.model).font(Theme.mono(11)).foregroundStyle(Theme.faint) }
                        .buttonStyle(.plain).help("Switch model (/model)")
                    if tab.tokens >= 1000 { Text("\(tab.tokens / 1000)k tokens").font(Theme.mono(11)).foregroundStyle(Theme.faint) }
                    if let c = tab.cost, c > 0 { Text(String(format: "$%.3f", c)).font(Theme.mono(11)).foregroundStyle(Theme.faint) }
                    Spacer()
                    Button { attachPanel() } label: { Text("+ image").font(Theme.mono(11)).foregroundStyle(Theme.faint) }
                        .buttonStyle(.plain).help("Attach an image (or paste / drop one)")
                    if tab.busy {
                        Button("stop") { project.stop() }.buttonStyle(.plain).font(Theme.mono(12)).foregroundStyle(Theme.accent)
                    }
                    Button("send ⏎") { send() }.buttonStyle(.plain).font(Theme.mono(12))
                        .foregroundStyle(text.isEmpty && images.isEmpty ? Theme.faint : Theme.ink)
                        .disabled(text.isEmpty && images.isEmpty)
                }
            }
            .padding(14)
            .background(RoundedRectangle(cornerRadius: 14, style: .continuous).fill(Theme.side))
            .overlay(RoundedRectangle(cornerRadius: 14, style: .continuous)
                .stroke(dropping ? Theme.accent : focused ? Theme.accent.opacity(0.55) : Theme.ink.opacity(0.1), lineWidth: focused || dropping ? 1 : 0.5))
        }
        .frame(maxWidth: 720)
        .padding(.horizontal, 24).padding(.bottom, 18).padding(.top, 4)
        .onAppear { focused = true; if project.commands.isEmpty { project.loadCommands() } }
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
                            .onTapGesture { project.choose(picker, r.value) }
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
