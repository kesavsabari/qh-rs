import AppKit
import SwiftUI

/// The window: system split view (sidebar · content · inspector) like Mail, Linear and Slack, so
/// the sidebar gets the system material, collapse animation and traffic-light inset for free.
struct ContentView: View {
    @EnvironmentObject var ws: Workspace
    @State private var columns = NavigationSplitViewVisibility.all

    var body: some View {
        NavigationSplitView(columnVisibility: $columns) {
            SidebarView()
                .navigationSplitViewColumnWidth(min: 220, ideal: 260, max: 340)
        } detail: {
            DetailView()
                .background(Theme.paper)
                .inspector(isPresented: $ws.inspector) {
                    InspectorView().inspectorColumnWidth(min: 260, ideal: 300, max: 420)
                }
        }
        .navigationSplitViewStyle(.balanced)
        .toolbar {
            ToolbarItem(placement: .principal) {
                Text(ws.title).font(Theme.sans(13, medium: true)).foregroundStyle(Theme.ink).lineLimit(1)
            }
            ToolbarItem(placement: .primaryAction) {
                Button { withAnimation(.smooth(duration: 0.25)) { ws.toggleInspector() } } label: {
                    Image(systemName: "sidebar.right")
                }
                .help("Inspector (⌘⌥I)")
            }
        }
    }
}

// MARK: - sidebar

struct SidebarView: View {
    @EnvironmentObject var ws: Workspace

    var body: some View {
        Group {
            if ws.space == "team" { TeamSidebar() } else { PersonalSidebar() }
        }
        .safeAreaInset(edge: .top, spacing: 0) {
            SpaceSwitch().padding(.horizontal, 12).padding(.top, 4).padding(.bottom, 8)
        }
        .safeAreaInset(edge: .bottom, spacing: 0) {
            HStack {
                Button(action: ws.pick) {
                    Label("Open folder", systemImage: "plus").font(Theme.sans(12.5))
                }
                .buttonStyle(.plain).foregroundStyle(Theme.soft)
                Spacer()
                Text(ws.version.replacingOccurrences(of: "qh ", with: "v")).font(Theme.mono(11)).foregroundStyle(Theme.faint)
            }
            .padding(.horizontal, 14).padding(.vertical, 10)
        }
    }
}

/// Personal ⇄ team, like Claude's Chat ⇄ Code switch.
struct SpaceSwitch: View {
    @EnvironmentObject var ws: Workspace
    var body: some View {
        HStack(spacing: 2) {
            seg("person", "Personal", "personal")
            seg("person.2", "Team", "team")
        }
        .padding(2)
        .background(RoundedRectangle(cornerRadius: 8, style: .continuous).fill(Theme.ink.opacity(0.06)))
    }

    private func seg(_ icon: String, _ label: String, _ space: String) -> some View {
        let on = ws.space == space
        return Button { withAnimation(.snappy(duration: 0.2)) { ws.setSpace(space) } } label: {
            Label(label, systemImage: icon)
                .font(Theme.sans(12, medium: on))
                .foregroundStyle(on ? Theme.ink : Theme.soft)
                .frame(maxWidth: .infinity).frame(height: 24)
                .background(RoundedRectangle(cornerRadius: 6, style: .continuous).fill(on ? Theme.paper : .clear)
                    .shadow(color: .black.opacity(on ? 0.12 : 0), radius: 1, y: 0.5))
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help("\(label) projects (⌃Space)")
    }
}

/// A sidebar row: 28pt, neutral selection (never orange), 4% hover.
struct Row<Content: View>: View {
    let selected: Bool
    let action: () -> Void
    let content: Content
    @State private var over = false

    init(selected: Bool, action: @escaping () -> Void, @ViewBuilder label: () -> Content) {
        self.selected = selected
        self.action = action
        self.content = label()
    }

    var body: some View {
        content
            .font(Theme.sans(13))
            .padding(.horizontal, 8)
            .frame(minHeight: 28)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(RoundedRectangle(cornerRadius: 6, style: .continuous)
                .fill(Theme.ink.opacity(selected ? 0.09 : over ? 0.04 : 0)))
            .contentShape(Rectangle())
            .onTapGesture(perform: action)
            .onHover { h in withAnimation(.easeOut(duration: 0.12)) { over = h } }
    }
}

struct Header: View {
    let text: String
    var trailing: String = ""
    var body: some View {
        HStack {
            Text(text.uppercased()).font(Theme.sans(10.5, medium: true)).kerning(0.4).foregroundStyle(Theme.faint)
            Spacer()
            if !trailing.isEmpty { Text(trailing).font(Theme.mono(10.5)).foregroundStyle(Theme.faint) }
        }
        .padding(.horizontal, 8).padding(.top, 14).padding(.bottom, 4)
    }
}

struct PersonalSidebar: View {
    @EnvironmentObject var ws: Workspace

    var body: some View {
        let mine = ws.recent.filter { ws.teamRepos[$0] == nil }
        ScrollView {
            VStack(alignment: .leading, spacing: 1) {
                Header(text: "Projects")
                if mine.isEmpty {
                    EmptyHint(icon: "folder", text: "Open a folder to start.", hint: "⌘O")
                }
                ForEach(mine, id: \.self) { p in ProjectBlock(path: p) }
            }
            .padding(.horizontal, 8)
        }
    }
}

/// A project row with its sessions under it (when it's open), newest first.
struct ProjectBlock: View {
    @EnvironmentObject var ws: Workspace
    let path: String

    var body: some View {
        let live = ws.projects.first { $0.path == path }
        VStack(alignment: .leading, spacing: 1) {
            Row(selected: ws.active == path && live == nil, action: { ws.open(path) }) {
                HStack(spacing: 8) {
                    Image(systemName: live == nil ? "folder" : "folder.fill").font(.system(size: 12)).foregroundStyle(Theme.soft)
                    Text((path as NSString).lastPathComponent).font(Theme.sans(13, medium: true)).foregroundStyle(Theme.ink).lineLimit(1)
                    Spacer(minLength: 4)
                    if let live { LiveDot(project: live) }
                }
            }
            .contextMenu {
                Button("Show in Finder") { NSWorkspace.shared.selectFile(nil, inFileViewerRootedAtPath: path) }
                if let live { Button("Stop qh here") { ws.close(live) } }
                Button("Remove from list") { ws.forget(path) }
            }
            .help(path)
            if let live { SessionRows(project: live) }
        }
    }
}

struct SessionRows: View {
    @ObservedObject var project: Project
    @EnvironmentObject var ws: Workspace
    @State private var all = false

    var body: some View {
        let shown = all ? project.sessions : Array(project.sessions.prefix(8))
        ForEach(shown) { s in
            let tab = project.openSessions[s.id]
            let on = ws.active == project.path && tab != nil && tab == project.current?.name
            Row(selected: on, action: { ws.active = project.path; project.resume(s) }) {
                HStack(spacing: 8) {
                    Circle().fill(tab != nil ? Theme.accent : .clear).frame(width: 5, height: 5)
                    Text(s.title.isEmpty ? s.id : s.title).foregroundStyle(on ? Theme.ink : Theme.soft).lineLimit(1)
                    Spacer(minLength: 4)
                    Text(Sessions.ago(s.when)).font(Theme.mono(10.5)).foregroundStyle(Theme.faint)
                }
                .padding(.leading, 12)
            }
            .help("\(s.messages) messages · \(s.id)")
        }
        if project.sessions.count > 8 {
            Button(all ? "Show fewer" : "Show all \(project.sessions.count)") { all.toggle() }
                .buttonStyle(.plain).font(Theme.sans(11.5)).foregroundStyle(Theme.faint).padding(.leading, 28).padding(.vertical, 4)
        }
    }
}

enum Sessions {
    static func ago(_ d: Date) -> String {
        let s = Int(Date().timeIntervalSince(d))
        if s < 60 { return "now" }
        if s < 3600 { return "\(s / 60)m" }
        if s < 86400 { return "\(s / 3600)h" }
        return "\(s / 86400)d"
    }
}

/// The team space's sidebar, Slack-style: which team project, then channels, agents, inbox, team.
struct TeamSidebar: View {
    @EnvironmentObject var ws: Workspace

    var body: some View {
        let teams = ws.recent.filter { ws.teamRepos[$0] != nil }
        let cur = ws.teamProject.flatMap { p in teams.contains(p) ? p : nil } ?? teams.first
        VStack(spacing: 0) {
            if let cur {
                Menu {
                    ForEach(teams, id: \.self) { p in
                        Button(ws.teamRepos[p] ?? p) { ws.open(p) }
                    }
                } label: {
                    HStack(spacing: 8) {
                        RoundedRectangle(cornerRadius: 5, style: .continuous).fill(Theme.accent)
                            .frame(width: 22, height: 22)
                            .overlay(Text(String((cur as NSString).lastPathComponent.prefix(1)).uppercased())
                                .font(Theme.sans(11, medium: true)).foregroundStyle(.white))
                        VStack(alignment: .leading, spacing: 0) {
                            Text((cur as NSString).lastPathComponent).font(Theme.sans(13, medium: true)).foregroundStyle(Theme.ink)
                            Text(ws.teamRepos[cur] ?? "").font(Theme.mono(10.5)).foregroundStyle(Theme.faint)
                        }
                        Spacer()
                        Image(systemName: "chevron.up.chevron.down").font(.system(size: 10)).foregroundStyle(Theme.faint)
                    }
                    .padding(.horizontal, 8).padding(.vertical, 6)
                    .contentShape(Rectangle())
                }
                .menuStyle(.button).buttonStyle(.plain).menuIndicator(.hidden)
                .padding(.horizontal, 8)
                if let p = ws.projects.first(where: { $0.path == cur }) {
                    TeamNav(project: p)
                } else {
                    VStack { Spacer(); ProgressView().controlSize(.small); Spacer() }
                        .onAppear { ws.open(cur) }
                }
            } else {
                EmptyHint(icon: "person.2", text: "No team projects yet. Open a GitHub project, then run /team in it.", hint: "⌘O")
                Spacer()
            }
        }
    }
}

enum TeamPage: Hashable {
    case agent                  // your own agent: the normal chat
    case channel(String)
    case peer(String, String)   // a teammate's agent: login, session
    case requests, members, claims, activity
}

struct TeamNav: View {
    @ObservedObject var project: Project
    @EnvironmentObject var ws: Workspace
    private let tick = Timer.publish(every: 3, on: .main, in: .common).autoconnect()

    var body: some View {
        let t = project.team
        ScrollView {
            VStack(alignment: .leading, spacing: 1) {
                if let t, !t.running {
                    EmptyHint(icon: "antenna.radiowaves.left.and.right.slash", text: "Team mode isn't connected here.", hint: "/team on")
                }
                Header(text: "Channels")
                ForEach(t?.channels.filter { !$0.id.hasPrefix("@") } ?? []) { c in channel(c) }
                let dms = t?.channels.filter { $0.id.hasPrefix("@") } ?? []
                if !dms.isEmpty {
                    Header(text: "Direct messages")
                    ForEach(dms) { c in channel(c) }
                }
                Header(text: "Agents")
                page(.agent, icon: "sparkles", "Your agent", trailing: project.current?.busy == true ? "working" : "")
                ForEach(t?.agents ?? []) { a in
                    page(.peer(a.login, a.name), icon: "", avatar: a.login, "\(a.login) · \(a.name)", trailing: a.status)
                }
                Header(text: "Inbox")
                page(.requests, icon: "tray", "Requests", badge: t?.waiting.count ?? 0)
                Header(text: "Team")
                page(.members, icon: "person.2", "Members", trailing: "\(t?.people.filter(\.online).count ?? 0)/\(t?.people.count ?? 0)")
                page(.claims, icon: "lock.doc", "Claims")
                page(.activity, icon: "waveform.path.ecg", "Activity")
            }
            .padding(.horizontal, 8)
        }
        .onAppear { refresh() }
        .onReceive(tick) { _ in refresh() }
    }

    private func refresh() {
        if case .channel(let c) = ws.teamPage { project.loadTeam(c) } else { project.loadTeam("#team") }
        if case .peer(let l, let s) = ws.teamPage { project.loadTranscript(l, s) }
    }

    private func channel(_ c: TeamState.Channel) -> some View {
        let on = ws.teamPage == .channel(c.id)
        let dm = c.id.hasPrefix("@")
        return Row(selected: on, action: { ws.teamPage = .channel(c.id); project.loadTeam(c.id) }) {
            HStack(spacing: 8) {
                if dm {
                    Avatar(name: String(c.id.dropFirst()), size: 16).overlay(alignment: .bottomTrailing) {
                        Circle().fill(c.online ? Color(nsColor: .systemGreen) : Theme.faint).frame(width: 6, height: 6)
                            .overlay(Circle().stroke(Theme.side, lineWidth: 1)).offset(x: 2, y: 2)
                    }
                } else {
                    Image(systemName: "number").font(.system(size: 11)).foregroundStyle(Theme.soft).frame(width: 16)
                }
                Text(c.label.trimmingCharacters(in: CharacterSet(charactersIn: "#@")))
                    .font(Theme.sans(13, medium: c.unread > 0)).foregroundStyle(c.unread > 0 || on ? Theme.ink : Theme.soft)
                Spacer()
                if c.unread > 0 { Circle().fill(c.pinged ? Theme.accent : Theme.soft).frame(width: 6, height: 6) }
            }
        }
    }

    private func page(_ p: TeamPage, icon: String, avatar: String = "", _ label: String, trailing: String = "", badge: Int = 0) -> some View {
        Row(selected: ws.teamPage == p, action: {
            ws.teamPage = p
            if case .peer(let l, let s) = p { project.loadTranscript(l, s) }
        }) {
            HStack(spacing: 8) {
                if avatar.isEmpty {
                    Image(systemName: icon).font(.system(size: 11.5)).foregroundStyle(Theme.soft).frame(width: 16)
                        .symbolRenderingMode(.hierarchical)
                } else {
                    Avatar(name: avatar, size: 16)
                }
                Text(label).foregroundStyle(ws.teamPage == p ? Theme.ink : Theme.soft).lineLimit(1)
                Spacer(minLength: 4)
                if badge > 0 {
                    Text("\(badge)").font(Theme.mono(10.5)).foregroundStyle(.white)
                        .padding(.horizontal, 6).padding(.vertical, 1).background(Capsule().fill(Theme.accent))
                } else if !trailing.isEmpty {
                    Text(trailing).font(Theme.mono(10.5)).foregroundStyle(Theme.faint).lineLimit(1)
                }
            }
        }
    }
}

/// A project's mark in the sidebar: a count when something waits for you, a dot while it works.
struct LiveDot: View {
    @ObservedObject var project: Project
    var body: some View {
        if project.waiting > 0 {
            Text("\(project.waiting)").font(Theme.mono(10)).foregroundStyle(.white)
                .padding(.horizontal, 5).background(Capsule().fill(Theme.accent))
        } else if project.current?.busy == true {
            Circle().fill(Theme.accent).frame(width: 6, height: 6)
        }
    }
}

struct EmptyHint: View {
    let icon: String
    let text: String
    let hint: String
    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Image(systemName: icon).font(.system(size: 22, weight: .light)).foregroundStyle(Theme.faint)
            Text(text).font(Theme.sans(12.5)).foregroundStyle(Theme.soft).fixedSize(horizontal: false, vertical: true)
            Text(hint).font(Theme.mono(11)).foregroundStyle(Theme.faint)
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

// MARK: - detail

struct DetailView: View {
    @EnvironmentObject var ws: Workspace

    var body: some View {
        if ws.qh == nil {
            Missing()
        } else if ws.space == "team" {
            if let p = ws.projects.first(where: { $0.path == ws.teamProject }) {
                if ws.teamPage == .agent { ProjectView(project: p).id(p.path) } else { TeamPageView(project: p) }
            } else {
                Empty()
            }
        } else if let p = ws.personalProject {
            ProjectView(project: p).id(p.path)
        } else {
            Empty()
        }
    }
}

struct Empty: View {
    @EnvironmentObject var ws: Workspace

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Mark(size: 150)
            Text(ws.space == "team"
                 ? "Your team's agents ask each other, claim files, and see who's changing what, over qh-link."
                 : "Open a project folder and qh starts there: sessions, the sandbox, every command.")
                .font(Theme.sans(15)).foregroundStyle(Theme.soft).frame(maxWidth: 460, alignment: .leading)
                .padding(.top, 28).padding(.bottom, 22)
            Button(action: ws.pick) {
                Text("Open a project").font(Theme.sans(13, medium: true)).foregroundStyle(Theme.paper)
                    .padding(.horizontal, 16).padding(.vertical, 9)
                    .background(RoundedRectangle(cornerRadius: 8, style: .continuous).fill(Theme.ink))
            }
            .buttonStyle(.plain)
            Text("⌘O").font(Theme.mono(11)).foregroundStyle(Theme.faint).padding(.top, 10)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .leading)
        .padding(.horizontal, 72)
        .background(Theme.paper)
    }
}

struct Missing: View {
    @State private var copied = false
    private let cmd = "curl -fsSL kesavsabari.github.io/qh/sh | sh"

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Mark(size: 150)
            Text("qh isn't installed on this Mac yet. Run this in Terminal, then reopen the app:")
                .font(Theme.sans(15)).foregroundStyle(Theme.soft).padding(.top, 28).padding(.bottom, 14)
            Text(cmd).font(Theme.mono(13)).foregroundStyle(Theme.ink).textSelection(.enabled).padding(.vertical, 12)
            Button {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(cmd, forType: .string)
                copied = true
            } label: {
                Text(copied ? "Copied" : "Copy").font(Theme.sans(13, medium: true)).foregroundStyle(Theme.paper)
                    .padding(.horizontal, 16).padding(.vertical, 9)
                    .background(RoundedRectangle(cornerRadius: 8, style: .continuous).fill(Theme.ink))
            }
            .buttonStyle(.plain)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .leading)
        .padding(.horizontal, 72)
        .background(Theme.paper)
    }
}

/// The big "qh▌".
struct Mark: View {
    let size: CGFloat
    var body: some View {
        HStack(alignment: .lastTextBaseline, spacing: size * 0.05) {
            Text("qh").font(Theme.sans(size, medium: true)).kerning(-size * 0.075).foregroundStyle(Theme.ink)
            Cursor(width: size * 0.075, height: size * 0.62)
        }
    }
}

// MARK: - inspector

/// Right panel: for your agent, what it changed and where it stands; for a teammate's agent, what
/// it's working on and the files it holds.
struct InspectorView: View {
    @EnvironmentObject var ws: Workspace

    var body: some View {
        let p = ws.space == "team" ? ws.projects.first { $0.path == ws.teamProject } : ws.personalProject
        ScrollView {
            if let p {
                if ws.space == "team", case .peer(let login, let name) = ws.teamPage,
                   let a = p.team?.agents.first(where: { $0.login == login && $0.name == name }) {
                    PeerInspector(agent: a)
                } else {
                    AgentInspector(project: p)
                }
            } else {
                EmptyHint(icon: "sidebar.right", text: "Open a project to see its changes and context.", hint: "⌘O")
            }
        }
        .background(Theme.paper)
    }
}

struct AgentInspector: View {
    @ObservedObject var project: Project

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Header(text: "Session")
            if let t = project.current {
                KV("model", t.model)
                KV("mode", project.mode)
                KV("context", t.tokens > 0 ? "\(t.tokens / 1000)k tokens" : "—")
                if let c = t.cost, c > 0 { KV("cost", String(format: "$%.3f", c)) }
                KV("status", project.asks.isEmpty ? t.status : "waiting for you")
            }
            Header(text: "Changes", trailing: project.changes.isEmpty ? "" : "\(project.changes.count)")
            if project.changes.isEmpty {
                Text("No files changed this session.").font(Theme.sans(12.5)).foregroundStyle(Theme.faint).padding(.horizontal, 8)
            }
            ForEach(project.changes) { f in
                let n = Diff.counts(f.before, f.after)
                Row(selected: false, action: { project.showReview = true }) {
                    HStack(spacing: 6) {
                        Image(systemName: f.kind == "new" ? "doc.badge.plus" : f.kind == "deleted" ? "doc.badge.minus" : "doc.text")
                            .font(.system(size: 11)).foregroundStyle(Theme.soft)
                        Text(f.short).font(Theme.mono(11.5)).foregroundStyle(Theme.ink).lineLimit(1).truncationMode(.head)
                        Spacer(minLength: 4)
                        Text("+\(n.added)").font(Theme.mono(10.5)).foregroundStyle(Color(nsColor: .systemGreen))
                        Text("−\(n.removed)").font(Theme.mono(10.5)).foregroundStyle(Color(nsColor: .systemRed))
                    }
                }
            }
            if !project.changes.isEmpty {
                Button("Review all  ⌘D") { project.showReview = true }.buttonStyle(.plain)
                    .font(Theme.sans(12, medium: true)).foregroundStyle(Theme.accent).padding(.horizontal, 8).padding(.top, 6)
            }
        }
        .padding(10)
        .onAppear { project.loadChanges() }
    }
}

struct PeerInspector: View {
    let agent: TeamState.Agent
    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Header(text: "Agent")
            KV("owner", agent.login + (agent.online ? " · online" : ""))
            KV("session", agent.name)
            KV("status", agent.status)
            if !agent.branch.isEmpty { KV("branch", agent.branch) }
            if !agent.topic.isEmpty { KV("on", agent.topic) }
            if !agent.now.isEmpty { KV("now", agent.now) }
            if agent.waiting { KV("waiting", "on a teammate") }
            Header(text: "Editing", trailing: "\(agent.edits.count)")
            ForEach(agent.edits, id: \.file) { e in
                VStack(alignment: .leading, spacing: 1) {
                    Text(e.file).font(Theme.mono(11.5)).foregroundStyle(Theme.ink).lineLimit(1).truncationMode(.head)
                    if !e.functions.isEmpty {
                        Text(e.functions.joined(separator: ", ")).font(Theme.mono(10.5)).foregroundStyle(Theme.faint).lineLimit(1)
                    }
                }
                .padding(.horizontal, 8).padding(.vertical, 3)
            }
        }
        .padding(10)
    }
}

struct KV: View {
    let k: String, v: String
    init(_ k: String, _ v: String) { self.k = k; self.v = v }
    var body: some View {
        HStack(alignment: .firstTextBaseline) {
            Text(k).font(Theme.sans(12)).foregroundStyle(Theme.faint).frame(width: 64, alignment: .leading)
            Text(v.isEmpty ? "—" : v).font(Theme.mono(11.5)).foregroundStyle(Theme.ink).lineLimit(2).textSelection(.enabled)
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 8).padding(.vertical, 2)
    }
}
