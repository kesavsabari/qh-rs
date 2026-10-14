import SwiftUI

struct TeamState {
    struct Person: Identifiable { var id: String { login }; let login: String; let online: Bool; let status: String; let sessions: [String]; let role: String }
    struct Channel: Identifiable { let id: String; let label: String; let unread: Int; let pinged: Bool; let online: Bool }
    struct Waiting: Identifiable { var id: String { "\(from)#\(num)" }; let from: String; let num: Int; let session: String; let text: String; let question: Bool }
    struct Msg: Identifiable { let id = UUID(); let who: String; let when: String; let text: String; let ping: Bool }

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
    }
}

/// The team room (⌘T in the TUI is Ctrl+T): teammates and their agents, requests waiting for you,
/// channels, and activity. Refreshes while open.
struct TeamView: View {
    @ObservedObject var project: Project
    @State private var chan = "#team"
    @State private var text = ""
    @Environment(\.dismiss) private var dismiss
    private let tick = Timer.publish(every: 3, on: .main, in: .common).autoconnect()

    var body: some View {
        VStack(spacing: 0) {
            if let t = project.team, t.running {
                HStack(spacing: 0) {
                    side(t)
                    Rectangle().fill(Theme.rule).frame(width: 1)
                    room(t)
                }
            } else {
                VStack(alignment: .leading, spacing: 14) {
                    Text("No team in this project yet").font(Theme.sans(20, medium: true)).foregroundStyle(Theme.ink)
                    Text("A team shares one GitHub repo: your agents ask each other, claim files, and see who's changing what over qh-link.")
                        .font(Theme.sans(14)).foregroundStyle(Theme.soft).frame(maxWidth: 480, alignment: .leading)
                    HStack(spacing: 10) {
                        Button("start a team here") { project.prompt("/team"); dismiss() }
                        Text("or in Terminal: qh team create  ·  qh join <repo>").font(Theme.mono(11)).foregroundStyle(Theme.faint)
                    }
                }
                .padding(40).frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
            }
            Rectangle().fill(Theme.rule).frame(height: 1)
            HStack {
                Text(project.team?.status ?? "").font(Theme.mono(11)).foregroundStyle(Theme.faint).lineLimit(1)
                Spacer()
                Button("done") { dismiss() }.keyboardShortcut(.cancelAction)
            }
            .padding(10)
        }
        .frame(minWidth: 920, minHeight: 620)
        .background(Theme.paper)
        .onAppear { project.loadTeam(chan) }
        .onReceive(tick) { _ in project.loadTeam(chan) }
    }

    private func side(_ t: TeamState) -> some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 4) {
                Text(t.repo).font(Theme.mono(12)).foregroundStyle(Theme.ink).padding(.bottom, 8)
                label("people")
                ForEach(t.people) { p in
                    HStack(spacing: 8) {
                        Circle().fill(p.online ? Theme.accent : Theme.faint.opacity(0.4)).frame(width: 6, height: 6)
                        VStack(alignment: .leading, spacing: 1) {
                            Text(p.login + (p.role == "member" ? "" : "  · \(p.role)")).foregroundStyle(Theme.ink)
                            Text(p.sessions.isEmpty ? p.status : p.sessions.joined(separator: ", ")).foregroundStyle(Theme.faint).lineLimit(1)
                        }
                    }
                    .font(Theme.mono(12)).padding(.vertical, 3)
                }
                label("channels").padding(.top, 14)
                ForEach(t.channels) { c in
                    HStack {
                        Text(c.label).foregroundStyle(c.id == chan ? Theme.ink : Theme.soft)
                        Spacer()
                        if c.unread > 0 { Text("\(c.unread)").foregroundStyle(c.pinged ? Theme.accent : Theme.faint) }
                    }
                    .font(Theme.mono(12)).padding(.horizontal, 6).padding(.vertical, 5)
                    .background(RoundedRectangle(cornerRadius: 5).fill(c.id == chan ? Theme.hover : .clear))
                    .contentShape(Rectangle())
                    .onTapGesture { chan = c.id; project.loadTeam(c.id) }
                }
                if !t.activity.isEmpty {
                    label("activity").padding(.top, 14)
                    ForEach(Array(t.activity.prefix(15).enumerated()), id: \.offset) { _, a in
                        Text("\(a.ago)  \(a.text)").font(Theme.mono(11)).foregroundStyle(Theme.faint).lineLimit(2)
                    }
                }
            }
            .padding(14)
        }
        .frame(width: 280)
        .background(Theme.side)
    }

    private func room(_ t: TeamState) -> some View {
        VStack(spacing: 0) {
            ForEach(t.waiting) { w in
                VStack(alignment: .leading, spacing: 8) {
                    Text("\(w.from) \(w.question ? "asks your agent" : "wants your agent to")").font(Theme.mono(12)).foregroundStyle(Theme.soft)
                    Text(w.text).font(Theme.sans(14)).foregroundStyle(Theme.ink).textSelection(.enabled)
                    HStack(spacing: 10) {
                        Button("accept") { project.prompt("/team answer \(w.from) \(w.num) once"); project.loadTeam(chan) }
                        Button("always from \(w.from)") { project.prompt("/team answer \(w.from) \(w.num) always"); project.loadTeam(chan) }
                        Button("decline") { project.prompt("/team answer \(w.from) \(w.num) deny"); project.loadTeam(chan) }
                    }
                    .font(Theme.mono(12))
                }
                .padding(12)
                .frame(maxWidth: .infinity, alignment: .leading)
                .overlay(RoundedRectangle(cornerRadius: 8).stroke(Theme.accent.opacity(0.7)))
                .padding(.horizontal, 16).padding(.top, 12)
            }
            ScrollViewReader { sv in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 8) {
                        if t.messages.isEmpty { Text("no messages in \(t.chan) yet").font(Theme.mono(12)).foregroundStyle(Theme.faint) }
                        ForEach(t.messages) { m in
                            VStack(alignment: .leading, spacing: 2) {
                                HStack(spacing: 8) {
                                    Text(m.who).foregroundStyle(m.who == "you" ? Theme.soft : Theme.ink)
                                    Text(m.when).foregroundStyle(Theme.faint)
                                }
                                .font(Theme.mono(11))
                                Text(m.text).font(Theme.sans(14)).foregroundStyle(m.ping ? Theme.accent : Theme.ink).textSelection(.enabled)
                            }
                        }
                        Color.clear.frame(height: 1).id("end")
                    }
                    .padding(16)
                }
                .onChange(of: t.messages.count) { _, _ in sv.scrollTo("end") }
                .onAppear { sv.scrollTo("end") }
            }
            HStack(spacing: 8) {
                TextField("message \(chan)", text: $text).textFieldStyle(.plain).font(Theme.sans(14))
                    .padding(10).background(RoundedRectangle(cornerRadius: 8).fill(Theme.side))
                    .onSubmit(send)
                Button("send", action: send).disabled(text.isEmpty)
            }
            .padding(12)
        }
    }

    private func label(_ s: String) -> some View {
        Text(s).font(Theme.mono(11)).foregroundStyle(Theme.faint).padding(.bottom, 2)
    }

    private func send() {
        let t = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !t.isEmpty else { return }
        project.teamSend(chan, t)
        text = ""
    }
}

/// Background tasks (dev servers, watchers) this tab started: /tasks, refreshed while open.
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
