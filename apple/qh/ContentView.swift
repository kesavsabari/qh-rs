import AppKit
import SwiftUI

/// The window: system split view (sidebar · content · inspector) like Mail, Linear and Slack, so
/// the sidebar gets the system material, collapse animation and traffic-light inset for free.
struct ContentView: View {
    @EnvironmentObject var ws: Workspace
    @State private var columns = NavigationSplitViewVisibility.all

    @Environment(\.accessibilityReduceMotion) private var still

    var body: some View {
        Group {
            if ws.compact {
                // compact: a 56pt rail of tiles on the ground, the card takes the rest
                HStack(spacing: 0) {
                    CompactSidebar().frame(width: 56)
                    card.padding(.leading, -8)
                }
                .background(Theme.ground)
                .transition(.opacity)
            } else {
                split.transition(.opacity)
            }
        }
        .animation(still ? nil : .smooth, value: ws.compact)
        .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { ws.fitCompact($0) }
        .sheet(isPresented: $ws.palette) { Palette().environmentObject(ws) }
        .toolbar(.hidden, for: .windowToolbar)
    }

    private var card: some View {
        // Zen: no chrome around the work, just the chat floating as a card on the ground
        DetailView()
            .background(Theme.card, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
            .overlay(RoundedRectangle(cornerRadius: 12, style: .continuous).stroke(Theme.line, lineWidth: 0.5))
            .padding(8)
            .background(Theme.ground)
            // no title strip: the card starts at the top edge, like the Windows app
            .ignoresSafeArea(.container, edges: .top)
            .inspector(isPresented: $ws.inspector) {
                InspectorView().inspectorColumnWidth(min: 260, ideal: 300, max: 420)
            }
    }

    private var split: some View {
        NavigationSplitView(columnVisibility: $columns) {
            SidebarView()
                .navigationSplitViewColumnWidth(min: 220, ideal: 260, max: 340)
                // the system sidebar button drew as an empty circle; ⌃⌘S and View ▸ Hide Sidebar still work
                .toolbar(removing: .sidebarToggle)
        } detail: {
            card
        }
        .navigationSplitViewStyle(.balanced)
    }
}

/// Compact mode (⌃⌘\): the sidebar as a rail of small project tiles, space dots and the gear.
struct CompactSidebar: View {
    @EnvironmentObject var ws: Workspace

    var body: some View {
        let team = ws.space == "team"
        let mine = ws.recent.filter { (ws.teamRepos[$0] != nil) == team }
        let cur = ws.active
        VStack(spacing: 6) {
            ScrollView(showsIndicators: false) {
                VStack(spacing: 6) {
                    ForEach(mine, id: \.self) { p in CompactTile(path: p, on: cur == p && !ws.settings) }
                    Button(action: ws.pick) { Image(systemName: "plus").font(.system(size: 12)) }
                        .buttonStyle(.hand).foregroundStyle(Theme.soft).frame(width: 40, height: 32).help("Open folder (⌘O)")
                }
                .padding(.top, 2)
            }
            SpaceSwitch()
            Button { ws.settings.toggle() } label: { Image(systemName: "gearshape").font(.system(size: 12.5)) }
                .buttonStyle(.hand).foregroundStyle(ws.settings ? Theme.ink : Theme.soft).help("Settings (⌘,) · \(ws.version)")
                .padding(.bottom, 10)
        }
        // clear of the traffic lights, which fade in over the rail's top on hover
        .padding(.top, 40)
        .frame(maxHeight: .infinity, alignment: .top)
        .ignoresSafeArea(.container, edges: .top)
        .background(Theme.ground)
    }
}

/// A rail tile: the project's two initials, raised on the card colour when it's the one open.
struct CompactTile: View {
    @EnvironmentObject var ws: Workspace
    let path: String
    let on: Bool
    @State private var over = false

    static func initials(_ name: String) -> String {
        let words = name.split { " -_.".contains($0) }.filter { !$0.isEmpty }
        let s = words.count > 1 ? String(words[0].prefix(1) + words[1].prefix(1)) : String(name.prefix(2))
        return s.prefix(1).uppercased() + s.dropFirst().lowercased()
    }

    var body: some View {
        let live = ws.projects.first { $0.path == path }
        let name = (path as NSString).lastPathComponent
        Button { ws.open(path) } label: {
            Text(Self.initials(name))
                .font(Theme.sans(12, medium: on))
                .foregroundStyle(on ? Theme.ink : Theme.soft)
                .frame(width: 40, height: 36)
                .background(RoundedRectangle(cornerRadius: 9, style: .continuous)
                    .fill(on ? Theme.card : over ? Theme.hover : Theme.tile))
                .shadow(color: .black.opacity(on ? 0.08 : 0), radius: 3, y: 1)
                .overlay(alignment: .topTrailing) { if let live { LiveDot(project: live).scaleEffect(0.8).offset(x: 3, y: -3) } }
                .contentShape(Rectangle())
        }
        .buttonStyle(.hand)
        .onHover { h in withAnimation(.easeOut(duration: 0.12)) { over = h } }
        .help(name)
        .accessibilityLabel(name)
        .contextMenu {
            Button("Show in Finder") { NSWorkspace.shared.selectFile(nil, inFileViewerRootedAtPath: path) }
            if let live { Button("Stop qh here") { ws.close(live) } }
            Button("Remove from list") { ws.forget(path) }
        }
    }
}

// MARK: - sidebar

struct SidebarView: View {
    @EnvironmentObject var ws: Workspace

    var body: some View {
        Group {
            if ws.settings { SettingsNav() } else if ws.space == "team" { TeamSidebar() } else { PersonalSidebar() }
        }
        .safeAreaInset(edge: .bottom, spacing: 0) {
            HStack(spacing: 4) {
                SpaceSwitch()
                Spacer()
                Button(action: ws.pick) { Image(systemName: "folder.badge.plus").font(.system(size: 12.5)) }
                    .buttonStyle(.hand).foregroundStyle(Theme.soft).help("Open folder (⌘O)")
                Button { ws.settings.toggle() } label: { Image(systemName: "gearshape").font(.system(size: 12.5)) }
                    .buttonStyle(.hand).foregroundStyle(ws.settings ? Theme.ink : Theme.soft).help("Settings (⌘,) · \(ws.version)")
            }
            .padding(.horizontal, 14).padding(.vertical, 10)
        }
        // just clear of the traffic lights (they only fade in on hover), not a whole title bar down
        .padding(.top, 26)
        .ignoresSafeArea(.container, edges: .top)
        .background(Theme.ground)
    }
}

/// Personal ⇄ team as Zen's workspace dots: the current one is a bigger ink dot.
struct SpaceSwitch: View {
    @EnvironmentObject var ws: Workspace
    var body: some View {
        HStack(spacing: 0) {
            dot("Personal", "personal")
            dot("Team", "team")
        }
    }

    private func dot(_ label: String, _ space: String) -> some View {
        let on = ws.space == space
        return Button { withAnimation(.snappy(duration: 0.2)) { ws.setSpace(space) } } label: {
            Circle().fill(on ? Theme.ink : Theme.soft.opacity(0.5))
                .frame(width: on ? 8 : 6, height: on ? 8 : 6)
                .frame(width: 26, height: 26).contentShape(Rectangle())
        }
        .buttonStyle(.hand)
        .help("\(label) projects (⌃Space)")
        .accessibilityLabel("\(label) space")
    }
}

/// Work | Messages, inside the team space: chat gets its own sidebar.
struct TeamTabs: View {
    let messages: Bool
    let unread: Int
    let pick: (Bool) -> Void
    var body: some View {
        HStack(spacing: 2) {
            seg("Work", false)
            seg(unread > 0 ? "Messages \(unread)" : "Messages", true)
        }
        .padding(2)
        .background(RoundedRectangle(cornerRadius: 8, style: .continuous).fill(Theme.ink.opacity(0.07)))
    }

    private func seg(_ label: String, _ m: Bool) -> some View {
        let on = messages == m
        return Button { withAnimation(.snappy(duration: 0.2)) { pick(m) } } label: {
            Text(label)
                .font(Theme.sans(12, medium: on))
                .foregroundStyle(on ? Theme.ink : m && unread > 0 ? Theme.accent : Theme.soft)
                .frame(maxWidth: .infinity).frame(height: 22)
                .background(RoundedRectangle(cornerRadius: 6, style: .continuous).fill(on ? Theme.paper : .clear)
                    .shadow(color: .black.opacity(on ? 0.12 : 0), radius: 1, y: 0.5))
                .contentShape(Rectangle())
        }
        .buttonStyle(.hand)
    }
}

/// A sidebar row: a Zen tab, the selected one a raised card pill (never orange), 4% hover.
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
            .frame(minHeight: 32)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(RoundedRectangle(cornerRadius: 9, style: .continuous)
                .fill(selected ? Theme.card : Theme.ink.opacity(over ? 0.04 : 0))
                .shadow(color: .black.opacity(selected ? 0.07 : 0), radius: 1.5, y: 1))
            .contentShape(Rectangle())
            .hand().onTapGesture(perform: action)
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

/// Zen's layout: pinned projects as tiles on top (its Essentials), then the open project's
/// sessions as vertical tabs.
struct PersonalSidebar: View {
    @EnvironmentObject var ws: Workspace

    var body: some View {
        let mine = ws.recent.filter { ws.teamRepos[$0] == nil }
        let cur = ws.personalProject
        // the header sits above the scroll view: a ScrollView first in the sidebar column drew nothing at all
        VStack(alignment: .leading, spacing: 0) {
            if mine.isEmpty {
                EmptyHint(icon: "folder", text: "Open a folder to start.", hint: "⌘O").padding(.horizontal, 8)
            } else {
                LazyVGrid(columns: Array(repeating: GridItem(.flexible(), spacing: 6), count: 3), spacing: 6) {
                    ForEach(mine, id: \.self) { p in ProjectTile(path: p, on: cur?.path == p) }
                }
                .padding(.horizontal, 12).padding(.top, 4)
            }
            if let cur {
                Row(selected: false, action: { cur.newTab() }) {
                    HStack(spacing: 10) {
                        Image(systemName: "plus").font(.system(size: 12))
                        Text("New chat")
                        Spacer()
                        Text("⌘T").font(Theme.mono(11))
                    }
                    .foregroundStyle(Theme.soft)
                }
                .padding(.horizontal, 8).padding(.top, 12)
                Header(text: cur.name).padding(.horizontal, 8)
                ScrollView {
                    VStack(alignment: .leading, spacing: 2) { SessionRows(project: cur) }
                        .padding(.horizontal, 8)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
            }
            Spacer(minLength: 0)
        }
    }
}

/// An Essentials tile: the project's name, raised on the card colour when it's the one open.
struct ProjectTile: View {
    @EnvironmentObject var ws: Workspace
    let path: String
    let on: Bool
    @State private var over = false

    var body: some View {
        let live = ws.projects.first { $0.path == path }
        let name = (path as NSString).lastPathComponent
        Button { ws.open(path) } label: {
            Text(name)
                .font(Theme.sans(12, medium: on))
                .lineLimit(1).truncationMode(.tail).padding(.horizontal, 6)
                .foregroundStyle(on ? Theme.ink : Theme.soft)
                .frame(maxWidth: .infinity).frame(height: 44)
                .background(RoundedRectangle(cornerRadius: 10, style: .continuous)
                    .fill(on ? Theme.card : over ? Theme.hover : Theme.tile))
                .shadow(color: .black.opacity(on ? 0.08 : 0), radius: 3, y: 1)
                .overlay(alignment: .topTrailing) { if let live { LiveDot(project: live).padding(6) } }
                .contentShape(Rectangle())
        }
        .buttonStyle(.hand)
        .onHover { h in withAnimation(.easeOut(duration: 0.12)) { over = h } }
        .help(Workspace.short(path))
        .accessibilityLabel(name)
        .contextMenu {
            Button("Show in Finder") { NSWorkspace.shared.selectFile(nil, inFileViewerRootedAtPath: path) }
            if let live { Button("Stop qh here") { ws.close(live) } }
            Button("Remove from list") { ws.forget(path) }
        }
    }
}

struct SessionRows: View {
    @ObservedObject var project: Project
    @EnvironmentObject var ws: Workspace
    @State private var all = false
    @State private var deleting: SavedSession?

    var body: some View {
        let shown = all ? project.sessions : Array(project.sessions.prefix(8))
        ForEach(shown) { s in
            let tab = project.openSessions[s.id]
            let here = ws.space == "personal" || ws.teamPage == .agent
            let on = here && ws.active == project.path && tab != nil && tab == project.current?.name
            Row(selected: on, action: {
                ws.active = project.path
                if ws.space == "team" { ws.teamPage = .agent }
                project.resume(s)
            }) {
                HStack(spacing: 8) {
                    Circle().fill(tab != nil ? Theme.accent : .clear).frame(width: 5, height: 5)
                    Text(s.title.isEmpty ? s.id : s.title).foregroundStyle(on ? Theme.ink : Theme.soft).lineLimit(1)
                    Spacer(minLength: 4)
                    Text(Sessions.ago(s.when)).font(Theme.mono(10.5)).foregroundStyle(Theme.faint)
                }
            }
            .help("\(s.messages) messages · \(s.id)")
            .contextMenu { Button("Delete Session…", role: .destructive) { deleting = s } }
        }
        Color.clear.frame(height: 0)
            .confirmationDialog("Delete this session?", isPresented: Binding(get: { deleting != nil }, set: { if !$0 { deleting = nil } }),
                                presenting: deleting) { s in
                Button("Delete", role: .destructive) { project.deleteSession(s) }
                Button("Cancel", role: .cancel) {}
            } message: { s in
                Text("“\(s.title.isEmpty ? s.id : s.title)” and its \(s.messages) messages are removed for good, with its checkpoints. Files in your project aren't touched.")
            }
        if project.sessions.count > 8 {
            Button(all ? "Show fewer" : "Show all \(project.sessions.count)") { all.toggle() }
                .buttonStyle(.hand).font(Theme.sans(11.5)).foregroundStyle(Theme.faint).padding(.leading, 21).padding(.vertical, 4)
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
                // team projects as Essentials tiles, like the personal space
                LazyVGrid(columns: Array(repeating: GridItem(.flexible(), spacing: 6), count: 3), spacing: 6) {
                    ForEach(teams, id: \.self) { p in ProjectTile(path: p, on: p == cur).help(ws.teamRepos[p] ?? p) }
                }
                .padding(.horizontal, 12).padding(.top, 4).padding(.bottom, 6)
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
    case requests, members, claims, activity, map
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
                let unread = t?.channels.reduce(0) { $0 + $1.unread } ?? 0
                TeamTabs(messages: chatting, unread: unread) { m in
                    if m != chatting { ws.teamPage = m ? .channel("#team") : .agent }
                }
                .padding(.vertical, 6)
                if chatting {
                    Header(text: "Channels")
                    ForEach(t?.channels.filter { !$0.id.hasPrefix("@") } ?? []) { c in channel(c) }
                    let dms = t?.channels.filter { $0.id.hasPrefix("@") } ?? []
                    if !dms.isEmpty {
                        Header(text: "Direct messages")
                        ForEach(dms) { c in channel(c) }
                    }
                } else {
                    // what needs you first, then your own work, then the team
                    if let n = t?.waiting.count, n > 0 {
                        Header(text: "Needs you")
                        page(.requests, icon: "tray.full", "Requests", badge: n)
                    }
                    Header(text: "Your agent")
                    page(.agent, icon: "sparkles", "Agent",
                         trailing: project.current?.busy == true ? "working" : "")
                    SessionRows(project: project)
                    if let agents = t?.agents, !agents.isEmpty {
                        Header(text: "Teammates' agents")
                        ForEach(agents) { a in
                            page(.peer(a.login, a.name), icon: "", avatar: a.login, "\(a.login) · \(a.name)", trailing: a.status)
                        }
                    }
                    Header(text: "Team")
                    if (t?.waiting.count ?? 0) == 0 { page(.requests, icon: "tray", "Requests") }
                    page(.members, icon: "person.2", "Members", trailing: "\(t?.people.filter(\.online).count ?? 0)/\(t?.people.count ?? 0)")
                    page(.map, icon: "square.grid.3x3.square", "Map")
                    page(.claims, icon: "lock.doc", "Claims")
                    page(.activity, icon: "waveform.path.ecg", "Activity")
                }
            }
            .padding(.horizontal, 8)
        }
        .onAppear { refresh() }
        // a qh that pushes team changes only needs a slow safety poll (every 30 s), an older one every 3 s
        .onReceive(tick) { d in
            let push = ws.projects.first { $0.path == ws.teamProject }?.teamPush == true
            if !push || Int(d.timeIntervalSince1970) % 30 < 3 { refresh() }
        }
    }

    private var chatting: Bool { if case .channel = ws.teamPage { return true }; return false }

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
        } else if ws.settings {
            SettingsView()
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
            Mark(size: 96)
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
            .buttonStyle(.hand)
            Text("⌘O").font(Theme.mono(11)).foregroundStyle(Theme.faint).padding(.top, 10)
            let recent = ws.recent.filter { (ws.teamRepos[$0] != nil) == (ws.space == "team") }.prefix(5)
            if !recent.isEmpty {
                Header(text: "Recent").padding(.top, 28)
                ForEach(Array(recent), id: \.self) { p in
                    Row(selected: false, action: { ws.open(p) }) {
                        HStack(spacing: 12) {
                            Text((p as NSString).lastPathComponent).font(Theme.sans(14, medium: true)).foregroundStyle(Theme.ink)
                            Text(Workspace.short(p)).font(Theme.mono(12)).foregroundStyle(Theme.soft).lineLimit(1)
                            Spacer()
                        }
                        .frame(height: 40)
                    }
                }
            }
        }
        .frame(maxWidth: 460, alignment: .leading)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Theme.card)
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
            .buttonStyle(.hand)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .leading)
        .padding(.horizontal, 72)
        .background(Theme.card)
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
                Button("Review all  ⌘D") { project.showReview = true }.buttonStyle(.hand)
                    .font(Theme.sans(12, medium: true)).foregroundStyle(Theme.accent).padding(.horizontal, 8).padding(.top, 6)
            }
            CheckpointList(project: project)
            Header(text: "Files", trailing: project.files.isEmpty ? "" : "\(project.files.count)")
            FileTree(project: project)
        }
        .padding(10)
        .onAppear { project.loadChanges(); project.loadFiles() }
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
