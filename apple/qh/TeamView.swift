import SwiftUI

struct TeamState {
    struct Person: Identifiable { var id: String { login }; let login: String; let online: Bool; let status: String; let sessions: [String]; let role: String }
    struct Channel: Identifiable { let id: String; let label: String; let unread: Int; let pinged: Bool; let online: Bool }
    struct Waiting: Identifiable { var id: String { "\(from)#\(num)" }; let from: String; let num: Int; let session: String; let text: String; let question: Bool }
    struct Msg: Identifiable { let id = UUID(); let who: String; let when: String; let text: String; let ping: Bool }
    struct Edit { let file: String; let functions: [String] }
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
        agents = (e["agents"] as? [[String: Any]] ?? []).map { a in
            Agent(login: a["login"] as? String ?? "", name: a["name"] as? String ?? "", status: a["status"] as? String ?? "",
                  topic: a["topic"] as? String ?? "", now: a["now"] as? String ?? "", branch: a["branch"] as? String ?? "",
                  online: a["online"] as? Bool ?? false, waiting: a["waiting"] as? Bool ?? false,
                  files: a["files"] as? [String] ?? [],
                  edits: (a["edits"] as? [[String: Any]] ?? []).map { Edit(file: $0["file"] as? String ?? "", functions: $0["functions"] as? [String] ?? []) })
        }
    }
}

/// A page title bar inside the content column: title, one line under it, actions on the right.
struct PageHeader<Trailing: View>: View {
    let title: String
    let subtitle: String
    @ViewBuilder var trailing: Trailing
    var body: some View {
        VStack(spacing: 0) {
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                Text(title).font(Theme.sans(17, medium: true)).foregroundStyle(Theme.ink)
                Text(subtitle).font(Theme.sans(12.5)).foregroundStyle(Theme.faint).lineLimit(1)
                Spacer()
                trailing
            }
            .padding(.horizontal, 24).padding(.vertical, 14)
            Rectangle().fill(Theme.ink.opacity(0.08)).frame(height: 0.5)
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
            case .requests: RequestsPage(project: project, waiting: t?.waiting ?? [])
            case .members: MembersPage(people: t?.people ?? [], me: t?.me ?? "")
            case .claims: ClaimsPage(agents: t?.agents ?? [])
            case .activity: ActivityPage(rows: t?.activity ?? [])
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
                       subtitle: chan == "#team" ? "everyone in \(t?.repo ?? "the team")" : chan.hasPrefix("@") ? "direct message" : "group")
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
                                Text(m.text).font(Theme.sans(14)).foregroundStyle(Theme.ink).lineSpacing(3).textSelection(.enabled)
                                    .padding(.leading, m.ping ? 8 : 0)
                                    .overlay(alignment: .leading) { if m.ping { Rectangle().fill(Theme.accent).frame(width: 2) } }
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
            HStack(spacing: 8) {
                TextField("Message \(chan)", text: $text, axis: .vertical).textFieldStyle(.plain).font(Theme.sans(14)).lineLimit(1...6)
                    .onSubmit(send)
                Button(action: send) { Image(systemName: "arrow.up").font(.system(size: 12, weight: .semibold)).foregroundStyle(.white)
                    .frame(width: 26, height: 26).background(Circle().fill(text.isEmpty ? Theme.faint : Theme.accent)) }
                    .buttonStyle(.plain).disabled(text.isEmpty)
            }
            .padding(12)
            .background(RoundedRectangle(cornerRadius: 14, style: .continuous).fill(Theme.side))
            .overlay(RoundedRectangle(cornerRadius: 14, style: .continuous).stroke(Theme.ink.opacity(0.08), lineWidth: 0.5))
            .frame(maxWidth: 720)
            .padding(.horizontal, 24).padding(.bottom, 18)
        }
    }

    private func send() {
        let s = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !s.isEmpty else { return }
        project.teamSend(chan, s)
        text = ""
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
                    .buttonStyle(.plain).font(Theme.sans(12.5, medium: true)).foregroundStyle(Theme.accent)
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

    var body: some View {
        VStack(spacing: 0) {
            PageHeader(title: "Requests", subtitle: waiting.isEmpty ? "nothing waiting" : "\(waiting.count) waiting for you")
            ScrollView {
                LazyVStack(spacing: 10) {
                    if waiting.isEmpty {
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
                        .background(RoundedRectangle(cornerRadius: 10, style: .continuous).fill(Theme.side))
                        .overlay(RoundedRectangle(cornerRadius: 10, style: .continuous).stroke(Theme.ink.opacity(0.08), lineWidth: 0.5))
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

struct PillButton: View {
    let title: String
    var primary = false
    let action: () -> Void
    var body: some View {
        Button(action: action) {
            Text(title).font(Theme.sans(12.5, medium: true))
                .foregroundStyle(primary ? .white : Theme.ink)
                .padding(.horizontal, 12).padding(.vertical, 6)
                .background(RoundedRectangle(cornerRadius: 7, style: .continuous).fill(primary ? Theme.accent : Theme.ink.opacity(0.07)))
        }
        .buttonStyle(.plain)
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
