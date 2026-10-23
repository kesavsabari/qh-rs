import SwiftUI

struct TeamState {
    struct Person: Identifiable { var id: String { login }; let login: String; let online: Bool; let status: String; let sessions: [String]; let role: String }
    struct Channel: Identifiable { let id: String; let label: String; let unread: Int; let pinged: Bool; let online: Bool }
    struct Waiting: Identifiable { var id: String { "\(from)#\(num)" }; let from: String; let num: Int; let session: String; let text: String; let question: Bool }
    struct Msg: Identifiable { let id = UUID(); let who: String; let when: String; let text: String; let ping: Bool }
    struct Edit { let file: String; let functions: [String]; var ago: Int = 0; var live: Bool { ago < 120 } }
    /// A teammate's push handed to you to decide: the reviewer's reason and the diff.
    struct Hold: Identifiable { var id: String { from + "/" + head }; let from: String; let head: String; let text: String; let ago: String; let decided: String? }
    struct Agent: Identifiable {
        var id: String { login + "/" + name }
        let login: String, name: String, status: String, topic: String, now: String, branch: String
        let online: Bool, waiting: Bool
        let files: [String]
        let edits: [Edit]
    }

    let running: Bool
    let me: String
    let repo: String
    let status: String
    let people: [Person]
    let channels: [Channel]
    let waiting: [Waiting]
    let activity: [(ago: String, text: String)]
    let chan: String
    let messages: [Msg]
    let agents: [Agent]
    let holds: [Hold]
    /// requests and held pushes still waiting on you
    var needs: Int { waiting.count + holds.filter { $0.decided == nil }.count }

    init(_ e: [String: Any]) {
        running = e["running"] as? Bool ?? false
        me = e["me"] as? String ?? ""
        repo = e["repo"] as? String ?? ""
        status = e["status"] as? String ?? ""
        people = (e["people"] as? [[String: Any]] ?? []).map {
            Person(login: $0["login"] as? String ?? "", online: $0["online"] as? Bool ?? false, status: $0["status"] as? String ?? "",
                   sessions: $0["sessions"] as? [String] ?? [], role: $0["role"] as? String ?? "")
        }
        channels = (e["channels"] as? [[String: Any]] ?? []).map {
            Channel(id: $0["id"] as? String ?? "", label: $0["label"] as? String ?? "", unread: $0["unread"] as? Int ?? 0,
                    pinged: $0["pinged"] as? Bool ?? false, online: $0["online"] as? Bool ?? false)
        }
        waiting = (e["waiting"] as? [[String: Any]] ?? []).map {
            Waiting(from: $0["from"] as? String ?? "", num: $0["id"] as? Int ?? 0, session: $0["session"] as? String ?? "",
                    text: $0["text"] as? String ?? "", question: $0["question"] as? Bool ?? false)
        }
        activity = (e["activity"] as? [[String: Any]] ?? []).map { ($0["ago"] as? String ?? "", $0["text"] as? String ?? "") }
        chan = e["chan"] as? String ?? "#team"
        messages = (e["messages"] as? [[String: Any]] ?? []).map {
            Msg(who: $0["who"] as? String ?? "", when: $0["when"] as? String ?? "", text: $0["text"] as? String ?? "", ping: $0["ping"] as? Bool ?? false)
        }
        holds = (e["holds"] as? [[String: Any]] ?? []).map {
            Hold(from: $0["from"] as? String ?? "", head: $0["head"] as? String ?? "", text: $0["text"] as? String ?? "",
                 ago: $0["ago"] as? String ?? "", decided: $0["decided"] as? String)
        }
        agents = (e["agents"] as? [[String: Any]] ?? []).map { a in
            Agent(login: a["login"] as? String ?? "", name: a["name"] as? String ?? "", status: a["status"] as? String ?? "",
                  topic: a["topic"] as? String ?? "", now: a["now"] as? String ?? "", branch: a["branch"] as? String ?? "",
                  online: a["online"] as? Bool ?? false, waiting: a["waiting"] as? Bool ?? false,
                  files: a["files"] as? [String] ?? [],
                  edits: (a["edits"] as? [[String: Any]] ?? []).map { Edit(file: $0["file"] as? String ?? "", functions: $0["functions"] as? [String] ?? [], ago: $0["ago"] as? Int ?? 0) })
        }
    }
}

/// A page title inside the card: big title, one quiet line beside it, actions on the right. No rule.
struct PageHeader<Trailing: View>: View {
    let title: String
    let subtitle: String
    @ViewBuilder var trailing: Trailing
    var body: some View {
        VStack(spacing: 0) {
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                Text(title).font(Theme.sans(20, medium: true)).foregroundStyle(Theme.ink)
                Text(subtitle).font(Theme.sans(13)).foregroundStyle(Theme.soft).lineLimit(1)
                Spacer()
                trailing
            }
            .padding(.horizontal, 24).padding(.top, 20).padding(.bottom, 8)
        }
    }
}

extension PageHeader where Trailing == EmptyView {
    init(title: String, subtitle: String) { self.init(title: title, subtitle: subtitle) { EmptyView() } }
}

/// The team space's content column for everything but your own agent.
struct TeamPageView: View {
    @ObservedObject var project: Project
    @EnvironmentObject var ws: Workspace

    var body: some View {
        let t = project.team
        Group {
            switch ws.teamPage {
            case .channel(let id): ChannelPage(project: project, chan: id)
            case .peer(let login, let name): PeerPage(project: project, login: login, name: name)
            case .requests: RequestsPage(project: project, waiting: t?.waiting ?? [], holds: t?.holds ?? [])
            case .members: MembersPage(people: t?.people ?? [], me: t?.me ?? "")
            case .claims: ClaimsPage(agents: t?.agents ?? [])
            case .activity: ActivityPage(rows: t?.activity ?? [])
            case .map: MapPage(project: project)
            case .agent: EmptyView()
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .background(Theme.paper)
    }
}

struct ChannelPage: View {
    @ObservedObject var project: Project
    let chan: String
    @State private var text = ""

    var body: some View {
        let t = project.team
        let msgs = t?.chan == chan ? t?.messages ?? [] : []
        VStack(spacing: 0) {
            PageHeader(title: chan.hasPrefix("@") ? String(chan.dropFirst()) : chan,
                       subtitle: chan == "#team" ? "everyone in \(t?.repo ?? "the team")" : chan.hasPrefix("@") ? "direct message" : "group") {
                if chan != "#team" && !msgs.isEmpty {
                    Button("Clear") { project.teamClear(chan) }.buttonStyle(.hand).font(Theme.sans(12.5)).foregroundStyle(Theme.soft)
                        .help("Hide everything in this thread so far, for you only; they keep their copy")
                }
            }
            ScrollViewReader { sv in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 14) {
                    if msgs.isEmpty {
                        EmptyHint(icon: "bubble.left.and.bubble.right", text: "No messages in \(chan) yet. Say hi.", hint: "⏎ sends").padding(.top, 20)
                    }
                    ForEach(msgs) { m in
                        HStack(alignment: .top, spacing: 10) {
                            Avatar(name: m.who == "you" ? (t?.me ?? "you") : m.who)
                            VStack(alignment: .leading, spacing: 2) {
                                HStack(spacing: 8) {
                                    Text(m.who).font(Theme.sans(13, medium: true)).foregroundStyle(Theme.ink)
                                    Text(m.when).font(Theme.mono(10.5)).foregroundStyle(Theme.faint)
                                }
                                if let c = ReviewNote(m.text) {
                                    ReviewNoteCard(note: c)
                                } else {
                                    Text(Mentions.render(m.text, project)).font(Theme.sans(14)).foregroundStyle(Theme.ink).lineSpacing(3).textSelection(.enabled)
                                        .padding(.leading, m.ping ? 8 : 0)
                                        .overlay(alignment: .leading) { if m.ping { Rectangle().fill(Theme.accent).frame(width: 2) } }
                                }
                            }
                        }
                    }
                    Color.clear.frame(height: 1).id("end")
                }
                .frame(maxWidth: 720, alignment: .leading)
                .padding(24)
                .frame(maxWidth: .infinity)
            }
            .onChange(of: msgs.count) { _, _ in sv.scrollTo("end", anchor: .bottom) }
            .onAppear { sv.scrollTo("end", anchor: .bottom) }
            }
            let sugg = suggestions
            if !sugg.isEmpty {
                VStack(alignment: .leading, spacing: 0) {
                    ForEach(sugg, id: \.self) { f in
                        Label(f, systemImage: "doc.text").font(Theme.mono(12)).foregroundStyle(Theme.ink)
                            .padding(.horizontal, 10).padding(.vertical, 5)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .contentShape(Rectangle())
                            .hand().onTapGesture { insert(f) }
                    }
                }
                .padding(.vertical, 4)
                .background(RoundedRectangle(cornerRadius: 8).fill(Theme.side))
                .overlay(RoundedRectangle(cornerRadius: 8).stroke(Theme.rule))
                .frame(maxWidth: 720).padding(.horizontal, 24).padding(.bottom, 6)
            }
            HStack(spacing: 8) {
                TextField("Message \(chan)   @ to mention a file", text: $text, axis: .vertical).textFieldStyle(.plain).font(Theme.sans(14)).lineLimit(1...6)
                    .onSubmit(send)
                Button(action: send) { Image(systemName: "arrow.up").font(.system(size: 12, weight: .semibold)).foregroundStyle(.white)
                    .frame(width: 26, height: 26).background(Circle().fill(text.isEmpty ? Theme.faint : Theme.accent)) }
                    .buttonStyle(.hand).disabled(text.isEmpty)
            }
            .padding(12)
            .background(RoundedRectangle(cornerRadius: 14, style: .continuous).fill(Theme.side))
            .overlay(RoundedRectangle(cornerRadius: 14, style: .continuous).stroke(Theme.ink.opacity(0.08), lineWidth: 0.5))
            .frame(maxWidth: 720)
            .padding(.horizontal, 24).padding(.bottom, 18)
        }
        .onAppear { project.loadHeat() }
        // debug: "io.qh.type" with text as its object fills the composer, for snapshots
        .onReceive(DistributedNotificationCenter.default().publisher(for: .init("io.qh.type"))) { n in
            if UserDefaults.standard.bool(forKey: "debugSnapshot") { text = n.object as? String ?? "" }
        }
    }

    private var suggestions: [String] {
        guard let q = mentionQuery else { return [] }
        return FileRank.rank(project.files, query: q, heat: heat).filter { project.fileSet.contains($0) }
    }

    /// The words typed after the last @ ("@auth ctrl" → "auth ctrl"), until it's a finished mention.
    private var mentionQuery: String? {
        guard let at = text.lastIndex(of: "@"), at == text.startIndex || text[text.index(before: at)] == " " else { return nil }
        let q = String(text[text.index(after: at)...])
        let first = q.split(separator: " ", omittingEmptySubsequences: false).first.map(String.init) ?? ""
        if project.fileSet.contains(first.split(separator: ":").first.map(String.init) ?? "") { return nil }
        if q.split(separator: " ").count > 3 { return nil }
        if (project.team?.people ?? []).contains(where: { $0.login.lowercased() == first.lowercased() }) { return nil } // @teammate ping
        return q
    }

    /// git history + what this chat and your teammates are on right now
    private var heat: [String: Double] {
        var h = project.gitHeat
        for c in project.changes { h[c.short, default: 0] += 6 }                 // your agent's edits this session
        for m in project.team?.messages ?? [] {                                   // mentioned in this channel
            for w in m.text.split(separator: " ") where w.hasPrefix("@") {
                let f = String(w.dropFirst()).split(separator: ":").first.map(String.init) ?? ""
                if project.fileSet.contains(f) { h[f, default: 0] += 5 }
            }
        }
        for a in project.team?.agents ?? [] {                                     // teammates' live files
            for f in a.files + a.edits.map(\.file) { h[f, default: 0] += 3 }
        }
        return h
    }

    private func insert(_ f: String) {
        guard let at = text.lastIndex(of: "@") else { return }
        text = String(text[..<at]) + "@" + f + " "
    }

    private func send() {
        if let q = mentionQuery, !q.isEmpty, !q.contains(" "), let f = suggestions.first { insert(f); return }
        let s = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !s.isEmpty else { return }
        project.teamSend(chan, s)
        text = ""
    }
}

/// @path mentions in a message: a file of this project turns into a link that opens it here.
enum Mentions {
    @MainActor static func render(_ text: String, _ project: Project) -> AttributedString {
        var out = AttributedString()
        for (i, w) in text.split(separator: " ", omittingEmptySubsequences: false).enumerated() {
            if i > 0 { out += AttributedString(" ") }
            let core = w.trimmingCharacters(in: CharacterSet(charactersIn: ".,;:!?)"))
            let path = core.hasPrefix("@") ? String(core.dropFirst()).split(separator: ":").first.map(String.init) ?? "" : ""
            if !path.isEmpty, project.fileSet.contains(path) {
                var a = AttributedString(String(w))
                a.link = URL(fileURLWithPath: project.path + "/" + path)
                a.foregroundColor = Theme.accent
                a.font = Theme.mono(13)
                out += a
            } else {
                out += AttributedString(String(w))
            }
        }
        return out
    }
}

struct Avatar: View {
    let name: String
    var size: CGFloat = 28
    var body: some View {
        // a stable colour per login (hashValue changes every launch)
        let hue = Double(name.unicodeScalars.reduce(0) { ($0 * 31 + Int($1.value)) % 360 }) / 360
        RoundedRectangle(cornerRadius: size / 4, style: .continuous)
            .fill(Color(hue: hue, saturation: 0.3, brightness: 0.55))
            .frame(width: size, height: size)
            .overlay(Text(String(name.prefix(1)).uppercased()).font(Theme.sans(size * 0.43, medium: true)).foregroundStyle(.white))
    }
}

/// A teammate's agent: its recent conversation, read-only, refreshed while open.
struct PeerPage: View {
    @ObservedObject var project: Project
    let login: String
    let name: String

    var body: some View {
        let a = project.team?.agents.first { $0.login == login && $0.name == name }
        let rows = project.transcript.key == "\(login)/\(name)" ? project.transcript.rows : []
        VStack(spacing: 0) {
            PageHeader(title: "\(login) · \(name)", subtitle: [a?.status ?? "", a?.topic ?? ""].filter { !$0.isEmpty }.joined(separator: " · ")) {
                Button("Message \(login)") { WorkspaceHolder.ws?.teamPage = .channel("@" + login) }
                    .buttonStyle(.hand).font(Theme.sans(12.5, medium: true)).foregroundStyle(Theme.accent)
            }
            ScrollViewReader { sv in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 14) {
                    if rows.isEmpty {
                        EmptyHint(icon: "text.bubble", text: "Nothing from this agent yet. Its conversation shows here as it works.", hint: "read-only").padding(.top, 20)
                    }
                    ForEach(Array(rows.enumerated()), id: \.offset) { _, r in
                        if r.role == "user" {
                            HStack { Spacer(minLength: 80); Text(r.text).font(Theme.sans(14)).foregroundStyle(Theme.ink).textSelection(.enabled)
                                .padding(.horizontal, 14).padding(.vertical, 10)
                                .background(RoundedRectangle(cornerRadius: 12, style: .continuous).fill(Theme.ink.opacity(0.06))) }
                        } else {
                            Markdown(text: r.text)
                        }
                    }
                    Color.clear.frame(height: 1).id("end")
                }
                .frame(maxWidth: 720, alignment: .leading)
                .padding(24)
                .frame(maxWidth: .infinity)
            }
            .onChange(of: rows.count) { _, _ in sv.scrollTo("end", anchor: .bottom) }
            .onAppear { sv.scrollTo("end", anchor: .bottom) }
            }
        }
    }
}

/// Linear-style inbox: each request a card, accept / always / decline.
struct RequestsPage: View {
    @ObservedObject var project: Project
    let waiting: [TeamState.Waiting]
    var holds: [TeamState.Hold] = []

    var body: some View {
        let n = waiting.count + holds.filter { $0.decided == nil }.count
        VStack(spacing: 0) {
            PageHeader(title: "Requests", subtitle: n == 0 ? "nothing waiting" : "\(n) waiting for you")
            ScrollView {
                LazyVStack(spacing: 10) {
                    ForEach(holds) { h in HoldCard(project: project, hold: h) }
                    if waiting.isEmpty && holds.isEmpty {
                        EmptyHint(icon: "tray", text: "When a teammate's agent asks yours for something, it waits here for your OK.", hint: "inbox zero").padding(.top, 20)
                    }
                    ForEach(waiting) { w in
                        VStack(alignment: .leading, spacing: 10) {
                            HStack(spacing: 10) {
                                Avatar(name: w.from)
                                VStack(alignment: .leading, spacing: 1) {
                                    Text(w.from).font(Theme.sans(13, medium: true)).foregroundStyle(Theme.ink)
                                    Text(w.question ? "asks your agent" : "wants your agent to do this").font(Theme.sans(12)).foregroundStyle(Theme.faint)
                                }
                            }
                            Text(w.text).font(Theme.sans(14)).foregroundStyle(Theme.ink).lineSpacing(3).textSelection(.enabled)
                            HStack(spacing: 8) {
                                PillButton(title: "Accept", primary: true) { answer(w, "once") }
                                PillButton(title: "Always from \(w.from)") { answer(w, "always") }
                                PillButton(title: "Decline") { answer(w, "deny") }
                            }
                        }
                        .padding(16)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(RoundedRectangle(cornerRadius: 12, style: .continuous).fill(Theme.side))
                    }
                }
                .frame(maxWidth: 720)
                .padding(24)
                .frame(maxWidth: .infinity)
            }
        }
    }

    private func answer(_ w: TeamState.Waiting, _ a: String) {
        project.prompt("/team answer \(w.from) \(w.num) \(a)")
        project.loadTeam("#team")
    }
}

/// A push a teammate's qh held and handed to you: why, the change itself, and your call.
struct HoldCard: View {
    @ObservedObject var project: Project
    @EnvironmentObject var ws: Workspace
    let hold: TeamState.Hold
    @State private var sending = false
    @State private var note = ""

    var body: some View {
        let parts = split(hold.text)
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 10) {
                Avatar(name: hold.from)
                VStack(alignment: .leading, spacing: 1) {
                    Text(hold.from).font(Theme.sans(13, medium: true)).foregroundStyle(Theme.ink)
                    Text(hold.decided.map { "push \($0) · \(hold.ago)" } ?? "push held for you to decide · \(hold.ago)")
                        .font(Theme.sans(12)).foregroundStyle(hold.decided == nil ? Theme.accent : Theme.faint)
                }
                Spacer()
                Text(String(hold.head.prefix(7))).font(Theme.mono(11)).foregroundStyle(Theme.faint)
            }
            Text(parts.reason).font(Theme.sans(14)).foregroundStyle(Theme.ink).lineSpacing(3).textSelection(.enabled)
            if !parts.diff.isEmpty {
                VStack(alignment: .leading, spacing: 0) {
                    ForEach(Array(parts.diff.enumerated()), id: \.offset) { _, l in
                        Text(l).font(Theme.mono(12)).lineLimit(1)
                            .foregroundStyle(l.hasPrefix("@@") ? Theme.faint : l.hasPrefix("+") ? Color.green : l.hasPrefix("-") ? Color.red : Theme.ink)
                    }
                }
                .padding(10).frame(maxWidth: .infinity, alignment: .leading)
                .background(RoundedRectangle(cornerRadius: 8, style: .continuous).fill(Theme.paper))
                .textSelection(.enabled)
            }
            if hold.decided == nil {
                if sending {
                    TextField("What must change before it goes?", text: $note, axis: .vertical).textFieldStyle(.plain).font(Theme.sans(14))
                        .padding(10).background(RoundedRectangle(cornerRadius: 8).fill(Theme.paper))
                        .onSubmit(sendBack)
                    HStack(spacing: 8) {
                        PillButton(title: "Send back", primary: true, action: sendBack)
                        PillButton(title: "Cancel") { sending = false }
                    }
                } else {
                    HStack(spacing: 8) {
                        PillButton(title: "Approve push", primary: true) { project.teamHold(hold, approve: true, note: "") }
                        PillButton(title: "Send back…") { sending = true }
                        PillButton(title: "Talk it through") { ws.teamPage = .channel("@" + hold.from); project.loadTeam("@" + hold.from) }
                    }
                }
            }
        }
        .padding(16)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: 12, style: .continuous).fill(Theme.side))
        .opacity(hold.decided == nil ? 1 : 0.6)
    }

    private func sendBack() {
        let n = note.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !n.isEmpty else { return }
        project.teamHold(hold, approve: false, note: n)
        sending = false
    }

    /// The message is the reason, then a blank line, then the diff lines.
    private func split(_ t: String) -> (reason: String, diff: [String]) {
        let lines = t.components(separatedBy: "\n")
        guard let i = lines.firstIndex(where: { $0.hasPrefix("@@") || $0.hasPrefix("+++ ") }) else { return (t, []) }
        return (lines[..<i].joined(separator: "\n").trimmingCharacters(in: .whitespacesAndNewlines), Array(lines[i...]))
    }
}

/// A machine message in a DM (review hand-off, `#qh-hold`, `#qh-decide`) read as a card, not a paragraph:
/// who wants what, the verdict, one short line per point, the change folded away.
struct ReviewNote {
    struct Point: Hashable { let loc: String; let text: String }
    let title: String
    let verdict: String
    let points: [Point]
    let files: [String]
    let diff: [String]
    let commit: String
    let isHold: Bool

    init?(_ t: String) {
        let lines = t.components(separatedBy: "\n")
        let tag = lines.last { $0.hasPrefix("#qh-hold ") || $0.hasPrefix("#qh-decide ") }
        guard t.hasPrefix("[review]") || tag != nil else { return nil }
        let words = tag?.split(separator: " ").map(String.init) ?? []
        commit = words.count > 1 ? String(words[1].prefix(7)) : ""
        isHold = words.first == "#qh-hold"
        let body = lines.filter { !$0.hasPrefix("#qh-") }
        let d = body.firstIndex { $0.hasPrefix("+++ ") || $0.hasPrefix("@@") || $0.hasPrefix("diff --git") } ?? body.count
        diff = Array(body[d...]).filter { !$0.isEmpty }
        files = diff.compactMap { $0.hasPrefix("+++ b/") ? String($0.dropFirst(6)) : nil }
        var r = body[..<d].joined(separator: " ").trimmingCharacters(in: .whitespacesAndNewlines)
        if r.hasPrefix("[review]") { r = String(r.dropFirst(8)).trimmingCharacters(in: .whitespaces) }
        var why = r
        var v = ""
        for k in ["CHANGES", "HANDOFF", "OK"] {
            if let range = r.range(of: k + " —") { v = k; why = String(r[range.upperBound...]); break }
        }
        if words.first == "#qh-decide" {
            let ok = words.count > 2 && words[2] == "approve"
            verdict = ok ? "Approved" : "Sent back"
            title = ok ? "Push approved" : "Push sent back"
        } else {
            verdict = ["CHANGES": "Changes needed", "HANDOFF": "Needs your call", "OK": "Looks good"][v] ?? ""
            let who = r.components(separatedBy: " wants to push").first.map { $0.trimmingCharacters(in: .whitespaces) } ?? ""
            title = r.contains(" wants to push") && !who.isEmpty ? "\(who) wants to push" : "Review"
        }
        points = ReviewNote.split(why)
    }

    /// "`shop.py:12`: dividing… keep `/ 100`. `STAFF90` and…" → one point per sentence, the leading file:line pulled out.
    static func split(_ s: String) -> [Point] {
        var out: [String] = []
        var cur = ""
        var tick = false
        for ch in s {
            if ch == "`" { tick.toggle() }
            cur.append(ch)
            if !tick && (ch == "." || ch == ";") { out.append(cur); cur = "" }
        }
        out.append(cur)
        return out.compactMap { raw in
            var t = raw.trimmingCharacters(in: CharacterSet.whitespaces.union(CharacterSet(charactersIn: ".;")))
            guard t.count > 2 else { return nil }
            var loc = ""
            if let m = t.range(of: #"^`?[\w./-]+\.\w+(:\d+(-\d+)?)?`?:\s*"#, options: .regularExpression) {
                loc = t[m].trimmingCharacters(in: CharacterSet(charactersIn: "`: "))
                t = String(t[m.upperBound...])
            }
            return Point(loc: loc, text: t.prefix(1).uppercased() + t.dropFirst())
        }
    }
}

struct ReviewNoteCard: View {
    let note: ReviewNote
    @EnvironmentObject var ws: Workspace
    @State private var showDiff = false

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Text(note.title).font(Theme.sans(14, medium: true)).foregroundStyle(Theme.ink)
                if !note.verdict.isEmpty {
                    Text(note.verdict).font(Theme.sans(11.5, medium: true))
                        .foregroundStyle(note.verdict == "Approved" || note.verdict == "Looks good" ? Theme.soft : Theme.accent)
                        .padding(.horizontal, 8).padding(.vertical, 2)
                        .background(Capsule().fill((note.verdict == "Approved" || note.verdict == "Looks good" ? Theme.soft : Theme.accent).opacity(0.12)))
                }
                Spacer(minLength: 8)
                if !note.commit.isEmpty { Text(note.commit).font(Theme.mono(11)).foregroundStyle(Theme.faint) }
            }
            if !note.files.isEmpty {
                Text(note.files.joined(separator: "  ·  ")).font(Theme.mono(11.5)).foregroundStyle(Theme.soft)
            }
            if !note.points.isEmpty {
                VStack(alignment: .leading, spacing: 8) {
                    ForEach(note.points, id: \.self) { p in
                        HStack(alignment: .firstTextBaseline, spacing: 8) {
                            Circle().fill(Theme.faint).frame(width: 4, height: 4).offset(y: -3)
                            VStack(alignment: .leading, spacing: 2) {
                                if !p.loc.isEmpty { Text(p.loc).font(Theme.mono(11)).foregroundStyle(Theme.faint) }
                                Text(Markdown.inline(p.text)).font(Theme.sans(13.5)).foregroundStyle(Theme.ink).lineSpacing(2).textSelection(.enabled)
                            }
                        }
                    }
                }
            }
            HStack(spacing: 14) {
                if !note.diff.isEmpty {
                    Button(showDiff ? "Hide change" : "Show change") { withAnimation(.easeOut(duration: 0.15)) { showDiff.toggle() } }
                        .buttonStyle(.hand).font(Theme.sans(12.5)).foregroundStyle(Theme.soft)
                }
                if note.isHold {
                    Button("Decide in Requests") { ws.teamPage = .requests }
                        .buttonStyle(.hand).font(Theme.sans(12.5, medium: true)).foregroundStyle(Theme.accent)
                }
            }
            if showDiff {
                VStack(alignment: .leading, spacing: 0) {
                    ForEach(Array(note.diff.filter { !$0.hasPrefix("+++") && !$0.hasPrefix("---") }.enumerated()), id: \.offset) { _, l in
                        Text(l).font(Theme.mono(12)).lineLimit(1)
                            .foregroundStyle(l.hasPrefix("@@") ? Theme.faint : l.hasPrefix("+") ? Color.green : l.hasPrefix("-") ? Color.red : Theme.ink)
                    }
                }
                .padding(10).frame(maxWidth: .infinity, alignment: .leading)
                .background(RoundedRectangle(cornerRadius: 8, style: .continuous).fill(Theme.paper))
                .textSelection(.enabled)
            }
        }
        .padding(16)
        .frame(maxWidth: 560, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: 12, style: .continuous).fill(Theme.side))
    }
}

struct PillButton: View {
    let title: String
    var primary = false
    let action: () -> Void
    var body: some View {
        Button(action: action) {
            // ink for the main answer, a raised card for the rest: orange stays for live things
            Text(title).font(Theme.sans(13))
                .foregroundStyle(primary ? Theme.paper : Theme.ink)
                .padding(.horizontal, 14).frame(height: 32)
                .background(RoundedRectangle(cornerRadius: 8, style: .continuous).fill(primary ? Theme.ink : Theme.paper))
        }
        .buttonStyle(.hand)
    }
}

struct MembersPage: View {
    let people: [TeamState.Person]
    let me: String
    var body: some View {
        VStack(spacing: 0) {
            PageHeader(title: "Members", subtitle: "\(people.filter(\.online).count) online of \(people.count)")
            ScrollView {
                LazyVStack(spacing: 0) {
                    ForEach(people) { p in
                        HStack(spacing: 12) {
                            Avatar(name: p.login).overlay(alignment: .bottomTrailing) {
                                Circle().fill(p.online ? Color(nsColor: .systemGreen) : Theme.faint).frame(width: 8, height: 8)
                                    .overlay(Circle().stroke(Theme.paper, lineWidth: 1.5)).offset(x: 2, y: 2)
                            }
                            VStack(alignment: .leading, spacing: 1) {
                                Text(p.login + (p.login == me ? "  (you)" : "")).font(Theme.sans(13, medium: true)).foregroundStyle(Theme.ink)
                                Text(p.sessions.isEmpty ? p.status : p.sessions.joined(separator: ", ")).font(Theme.sans(12)).foregroundStyle(Theme.faint).lineLimit(1)
                            }
                            Spacer()
                            Text(p.role).font(Theme.mono(11)).foregroundStyle(Theme.soft)
                                .padding(.horizontal, 8).padding(.vertical, 2).background(Capsule().fill(Theme.ink.opacity(0.06)))
                        }
                        .padding(.horizontal, 12).frame(height: 52)
                        Rectangle().fill(Theme.ink.opacity(0.06)).frame(height: 0.5).padding(.leading, 52)
                    }
                }
                .frame(maxWidth: 720)
                .padding(24)
                .frame(maxWidth: .infinity)
            }
        }
    }
}

/// Who's editing which files right now: collisions are visible before they happen.
struct ClaimsPage: View {
    let agents: [TeamState.Agent]
    var body: some View {
        let busy = agents.filter { !$0.edits.isEmpty }
        VStack(spacing: 0) {
            PageHeader(title: "Claims", subtitle: "files teammates' agents are changing")
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 18) {
                    if busy.isEmpty {
                        EmptyHint(icon: "lock.open", text: "No one's editing anything right now.", hint: "edits show here live").padding(.top, 20)
                    }
                    ForEach(busy) { a in
                        VStack(alignment: .leading, spacing: 6) {
                            HStack(spacing: 8) {
                                Avatar(name: a.login)
                                Text("\(a.login) · \(a.name)").font(Theme.sans(13, medium: true)).foregroundStyle(Theme.ink)
                                if !a.branch.isEmpty { Text(a.branch).font(Theme.mono(11)).foregroundStyle(Theme.faint) }
                            }
                            ForEach(a.edits, id: \.file) { e in
                                HStack(spacing: 8) {
                                    Image(systemName: "lock.fill").font(.system(size: 9)).foregroundStyle(Theme.accent)
                                    Text(e.file).font(Theme.mono(12)).foregroundStyle(Theme.ink)
                                    if !e.functions.isEmpty { Text(e.functions.joined(separator: ", ")).font(Theme.mono(11)).foregroundStyle(Theme.faint).lineLimit(1) }
                                }
                                .padding(.leading, 38)
                            }
                        }
                    }
                }
                .frame(maxWidth: 720, alignment: .leading)
                .padding(24)
                .frame(maxWidth: .infinity)
            }
        }
    }
}

struct ActivityPage: View {
    let rows: [(ago: String, text: String)]
    var body: some View {
        VStack(spacing: 0) {
            PageHeader(title: "Activity", subtitle: "what happened in the team")
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    if rows.isEmpty { EmptyHint(icon: "waveform.path.ecg", text: "Quiet so far.", hint: "").padding(.top, 20) }
                    ForEach(Array(rows.enumerated()), id: \.offset) { _, r in
                        HStack(alignment: .firstTextBaseline, spacing: 14) {
                            Text(r.ago).font(Theme.mono(11)).foregroundStyle(Theme.faint).frame(width: 56, alignment: .trailing)
                            Circle().fill(Theme.ink.opacity(0.25)).frame(width: 5, height: 5)
                            Text(r.text).font(Theme.sans(13.5)).foregroundStyle(Theme.ink).textSelection(.enabled)
                        }
                        .padding(.vertical, 7)
                    }
                }
                .frame(maxWidth: 720, alignment: .leading)
                .padding(24)
                .frame(maxWidth: .infinity)
            }
        }
    }
}

/// For pages that need to switch the team page without carrying the workspace down.
@MainActor
enum WorkspaceHolder {
    static weak var ws: Workspace?
}

struct TasksView: View {
    @ObservedObject var project: Project
    @Environment(\.dismiss) private var dismiss
    private let tick = Timer.publish(every: 2, on: .main, in: .common).autoconnect()

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("background tasks").font(Theme.sans(17, medium: true)).foregroundStyle(Theme.ink)
            ScrollView {
                Text(project.tasks.isEmpty ? "none running" : project.tasks).font(Theme.mono(12)).foregroundStyle(Theme.ink)
                    .textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
            }
            Text("stop one: /tasks stop <id>").font(Theme.mono(11)).foregroundStyle(Theme.faint)
            HStack { Spacer(); Button("done") { dismiss() }.keyboardShortcut(.cancelAction) }
        }
        .padding(20)
        .frame(width: 620, height: 420)
        .background(Theme.paper)
        .onAppear { project.loadTasks() }
        .onReceive(tick) { _ in project.loadTasks() }
    }
}
