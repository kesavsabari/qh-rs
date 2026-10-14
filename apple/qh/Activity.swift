import SwiftUI

/// What the thread draws: a message, or a run of consecutive tool calls shown as one line.
enum Block: Identifiable {
    case item(Item)
    case run([Item])
    var id: UUID {
        switch self {
        case .item(let i): return i.id
        case .run(let r): return r[0].id
        }
    }
}

enum Activity {
    /// Consecutive tool calls fold into one run. Edits stay their own lines: you want to see them.
    static func blocks(_ items: [Item]) -> [Block] {
        var out: [Block] = []
        var run: [Item] = []
        func close() { if !run.isEmpty { out.append(.run(run)); run = [] } }
        for i in items {
            if case .tool = i.kind { run.append(i) } else { close(); out.append(.item(i)) }
        }
        close()
        return out
    }

    enum Category: Int, CaseIterable { case edit, explore, search, browse, run, delegate, other }

    static func category(_ tool: String) -> Category {
        switch tool {
        case "edit_file", "write_file", "rename_symbol", "apply_patch": return .edit
        case "read_file", "list_files", "list_dir", "glob", "grep", "find_files", "search_code", "past_sessions": return .explore
        case "web_search": return .search
        case "web_fetch", "browser_open", "browser_navigate", "browse", "fetch_url": return .browse
        case "bash", "run", "shell": return .run
        case "spawn", "send_message", "ask", "ask_team": return .delegate
        default: return tool.hasPrefix("browser") ? .browse : .other
        }
    }

    private static let copy: [Category: (one: String, many: String, past: String, now: String)] = [
        .edit: ("file", "files", "Edited", "Editing"),
        .explore: ("file", "files", "Explored", "Exploring"),
        .search: ("search", "searches", "Searched", "Searching"),
        .browse: ("page", "pages", "Opened", "Opening"),
        .run: ("command", "commands", "Ran", "Running"),
        .delegate: ("message", "messages", "Sent", "Sending"),
        .other: ("tool", "tools", "Used", "Using"),
    ]

    /// "Explored 3 files, ran 2 commands" (live: "Exploring 3 files, running 2 commands").
    static func summary(_ run: [Item], live: Bool) -> String {
        var counts: [Category: Int] = [:]
        for i in run { if case .tool(let n, _, _, _, _) = i.kind { counts[category(n), default: 0] += 1 } }
        let parts = Category.allCases.compactMap { c -> String? in
            guard let n = counts[c], let w = copy[c] else { return nil }
            return "\(live ? w.now : w.past) \(n) \(n == 1 ? w.one : w.many)"
        }
        guard let first = parts.first else { return live ? "Working" : "Done" }
        return ([first] + parts.dropFirst().map { $0.prefix(1).lowercased() + $0.dropFirst() }).joined(separator: ", ")
    }

    /// One call as a line: the verb and what it touched.
    static func line(_ name: String, _ args: String) -> String {
        let verb: String
        switch name {
        case "read_file": verb = "Read"
        case "edit_file": verb = "Edit"
        case "write_file": verb = "Write"
        case "bash": verb = "Run"
        case "grep", "search_code": verb = "Search"
        case "glob", "list_files", "list_dir", "find_files": verb = "List"
        case "web_search": verb = "Search the web"
        case "web_fetch": verb = "Fetch"
        default: verb = name.replacingOccurrences(of: "_", with: " ").capitalized
        }
        return args.isEmpty ? verb : "\(verb) \(args)"
    }

    static func seconds(_ s: TimeInterval) -> String {
        if s < 1 { return "<1s" }
        let n = Int(s.rounded())
        return n < 60 ? "\(n)s" : "\(n / 60)m \(n % 60)s"
    }
}

/// Text with a light band sweeping across it, for anything still in progress. The sweep is a
/// SwiftUI repeat-forever animation of one offset, interpolated by the render server.
struct Shimmer: ViewModifier {
    let on: Bool
    @State private var x: CGFloat = -1

    func body(content: Content) -> some View {
        if on {
            content
                .overlay {
                    GeometryReader { g in
                        LinearGradient(colors: [.clear, Theme.paper.opacity(0.85), .clear], startPoint: .leading, endPoint: .trailing)
                            .frame(width: g.size.width * 0.45)
                            .offset(x: x * g.size.width * 1.45)
                    }
                    .mask(content)
                    .allowsHitTesting(false)
                }
                .onAppear { withAnimation(.linear(duration: 1.6).repeatForever(autoreverses: false)) { x = 1 } }
        } else {
            content
        }
    }
}

extension View {
    func shimmer(_ on: Bool = true) -> some View { modifier(Shimmer(on: on)) }
}

/// A run of tool calls: one grey line ("Explored 3 files · 4s"); while live it shimmers, counts
/// up, and a one-line ticker shows the latest call. Click to list every call.
struct ToolRun: View {
    let items: [Item]
    let live: Bool
    @State private var open = false

    var body: some View {
        let start = items.first?.at ?? Date()
        let end = items.compactMap(\.end).max()
        let failed = items.contains { if case .tool(_, _, false, _, _) = $0.kind { return true }; return false }
        VStack(alignment: .leading, spacing: 4) {
            Button { withAnimation(.snappy(duration: 0.2)) { open.toggle() } } label: {
                HStack(spacing: 6) {
                    Image(systemName: "chevron.right").font(.system(size: 9, weight: .semibold))
                        .rotationEffect(.degrees(open ? 90 : 0)).foregroundStyle(Theme.faint)
                    Text(Activity.summary(items, live: live)).font(Theme.sans(13)).foregroundStyle(Theme.soft).lineLimit(1)
                        .shimmer(live)
                    if failed { Image(systemName: "exclamationmark.circle").font(.system(size: 11)).foregroundStyle(Color(nsColor: .systemRed)) }
                    if live {
                        TimelineView(.periodic(from: .now, by: 1)) { ctx in
                            Text(Activity.seconds(ctx.date.timeIntervalSince(start))).font(Theme.mono(11)).foregroundStyle(Theme.faint)
                        }
                    } else if let end {
                        Text(Activity.seconds(end.timeIntervalSince(start))).font(Theme.mono(11)).foregroundStyle(Theme.faint)
                    }
                    Spacer(minLength: 0)
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.hand)
            if open {
                VStack(alignment: .leading, spacing: 2) {
                    ForEach(items) { i in
                        if case .tool(let n, let a, let ok, let summary, let output) = i.kind {
                            ToolLine(name: n, args: a, ok: ok, summary: summary, output: output)
                        }
                    }
                }
                .padding(.leading, 15)
                .transition(.opacity)
            } else if live, let last = items.last, case .tool(let n, let a, _, _, _) = last.kind {
                // the ticker: the newest call rolls in from below, the old one leaves upward
                Text(Activity.line(n, a)).font(Theme.mono(11.5)).foregroundStyle(Theme.faint).lineLimit(1).truncationMode(.middle)
                    .id(last.id)
                    .transition(.asymmetric(insertion: .move(edge: .bottom).combined(with: .opacity),
                                            removal: .move(edge: .top).combined(with: .opacity)))
                    .padding(.leading, 15)
                    .frame(height: 16, alignment: .leading)
                    .clipped()
                    .animation(.snappy(duration: 0.25), value: last.id)
            }
        }
    }
}

/// One call inside an opened run: verb + target, its result on click.
struct ToolLine: View {
    let name: String, args: String
    let ok: Bool?
    let summary: String, output: String
    @State private var open = false

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Button { open.toggle() } label: {
                HStack(spacing: 6) {
                    Image(systemName: ok == nil ? "circle.dotted" : ok! ? "checkmark" : "xmark")
                        .font(.system(size: 9, weight: .bold))
                        .foregroundStyle(ok == false ? Color(nsColor: .systemRed) : Theme.faint)
                        .frame(width: 12)
                    Text(Activity.line(name, args)).font(Theme.mono(11.5)).foregroundStyle(Theme.soft).lineLimit(1).truncationMode(.middle)
                    Spacer(minLength: 0)
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.hand)
            if open && !(summary.isEmpty && output.isEmpty) {
                ScrollView {
                    Text(output.isEmpty ? summary : output).font(Theme.mono(11)).foregroundStyle(Theme.ink).textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                .frame(maxHeight: 240)
                .padding(8)
                .background(RoundedRectangle(cornerRadius: 8, style: .continuous).fill(Theme.ink.opacity(0.04)))
                .padding(.leading, 18)
            }
        }
    }
}

/// While the agent is quiet for more than 2s mid-turn: a pulsing square, "Thinking", and how
/// long it's been, so a long pause never looks like a hang.
struct ThinkingRow: View {
    @ObservedObject var tab: ChatTab
    @State private var pulse = false

    var body: some View {
        TimelineView(.periodic(from: .now, by: 1)) { ctx in
            let quiet = ctx.date.timeIntervalSince(tab.lastActivity)
            if quiet >= 2 {
                HStack(spacing: 8) {
                    RoundedRectangle(cornerRadius: 2, style: .continuous).fill(Theme.accent)
                        .frame(width: 9, height: 9)
                        .opacity(pulse ? 0.35 : 1)
                        .animation(.easeInOut(duration: 0.8).repeatForever(), value: pulse)
                        .onAppear { pulse = true }
                    Text("Thinking").font(Theme.sans(13)).foregroundStyle(Theme.soft).shimmer()
                    Text(Activity.seconds(quiet)).font(Theme.mono(11)).foregroundStyle(Theme.faint)
                }
                .transition(.opacity)
            }
        }
    }
}
