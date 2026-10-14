import AppKit
import SwiftUI

struct ContentView: View {
    @EnvironmentObject var ws: Workspace

    var body: some View {
        HStack(spacing: 0) {
            Sidebar()
            Rectangle().fill(Theme.rule).frame(width: 1)
            ZStack {
                Theme.paper
                if ws.qh == nil {
                    Missing()
                } else if let p = ws.current {
                    ProjectView(project: p).id(p.path)
                } else {
                    Empty()
                }
            }
        }
        .background(Theme.paper)
        .ignoresSafeArea()
    }
}

struct Sidebar: View {
    @EnvironmentObject var ws: Workspace

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .center) {
                HStack(alignment: .lastTextBaseline, spacing: 2) {
                    Text("qh").font(Theme.sans(26, medium: true)).kerning(-1.6)
                    Cursor(width: 2.5, height: 17)
                }
                .foregroundStyle(Theme.ink)
                Spacer()
                SpaceSwitch()
            }
            .padding(EdgeInsets(top: 44, leading: 6, bottom: 22, trailing: 0))

            Text(ws.space == "team" ? "team projects" : "personal projects").font(Theme.mono(12)).foregroundStyle(Theme.soft)
                .padding(.horizontal, 6).padding(.bottom, 8)
            Rectangle().fill(Theme.rule).frame(height: 1).padding(.bottom, 6)

            ScrollView {
                VStack(alignment: .leading, spacing: 1) {
                    let shown = ws.recent.filter { (ws.teamRepos[$0] != nil) == (ws.space == "team") }
                    ForEach(shown, id: \.self) { p in ProjectRow(path: p); SessionList(path: p) }
                    if shown.isEmpty {
                        Text(ws.space == "team"
                             ? "No team projects yet. Open a GitHub project and start a team from its team room (⌘⇧T)."
                             : "No personal projects yet: open a folder.")
                            .font(Theme.sans(12.5)).foregroundStyle(Theme.faint).padding(.horizontal, 8).padding(.top, 4)
                    }
                }
            }

            Rectangle().fill(Theme.rule).frame(height: 1).padding(.top, 10)
            Button(action: ws.pick) {
                HStack {
                    Text("+ open folder").foregroundStyle(Theme.ink)
                    Spacer()
                    Text("⌘O").foregroundStyle(Theme.faint)
                }
                .font(Theme.mono(12))
                .padding(.horizontal, 8).padding(.vertical, 10)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            Text(ws.version).font(Theme.mono(11)).foregroundStyle(Theme.faint).padding(.horizontal, 8).padding(.top, 4)
        }
        .padding(EdgeInsets(top: 0, leading: 14, bottom: 16, trailing: 14))
        .frame(width: 268)
        .frame(maxHeight: .infinity, alignment: .top)
        .background(Theme.side)
    }
}

struct ProjectRow: View {
    @EnvironmentObject var ws: Workspace
    let path: String
    @State private var over = false

    var body: some View {
        let on = ws.active == path
        let live = ws.projects.first { $0.path == path }
        Button { ws.open(path) } label: {
            HStack(spacing: 0) {
                VStack(alignment: .leading, spacing: 1) {
                    Text((path as NSString).lastPathComponent).font(Theme.sans(14, medium: true)).foregroundStyle(Theme.ink).lineLimit(1)
                    Text(ws.teamRepos[path] ?? Workspace.short(path)).font(Theme.mono(11)).foregroundStyle(Theme.faint).lineLimit(1).truncationMode(.head)
                }
                Spacer(minLength: 6)
                if let live { LiveDot(project: live) }
            }
            .padding(.horizontal, 8).padding(.vertical, 8)
            .background(RoundedRectangle(cornerRadius: 6).fill(on || over ? Theme.hover : .clear))
            .overlay(alignment: .leading) {
                if on { Rectangle().fill(Theme.accent).frame(width: 2).padding(.vertical, 10).offset(x: -14) }
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .onHover { over = $0 }
        .help(path)
        .contextMenu {
            Button("Show in Finder") { NSWorkspace.shared.selectFile(nil, inFileViewerRootedAtPath: path) }
            if let live { Button("Stop qh here") { ws.close(live) } }
            Button("Remove from list") { ws.forget(path) }
        }
    }
}

/// An open project's sessions under its row, newest first, like Claude Code desktop: the tabs
/// open now, then saved ones; click to switch to or resume one.
struct SessionList: View {
    @EnvironmentObject var ws: Workspace
    let path: String
    var body: some View {
        if let p = ws.projects.first(where: { $0.path == path }) { Sessions(project: p) }
    }
}

struct Sessions: View {
    @ObservedObject var project: Project
    @EnvironmentObject var ws: Workspace
    @State private var all = false

    var body: some View {
        let shown = all ? project.sessions : Array(project.sessions.prefix(8))
        VStack(alignment: .leading, spacing: 0) {
            ForEach(shown) { s in
                let tab = project.openSessions[s.id]
                let on = ws.active == project.path && tab != nil && tab == project.current?.name
                HStack(spacing: 6) {
                    Circle().fill(tab != nil ? Theme.accent : .clear).frame(width: 4, height: 4)
                    Text(s.title.isEmpty ? s.id : s.title).font(Theme.sans(12.5)).foregroundStyle(on ? Theme.ink : Theme.soft).lineLimit(1)
                    Spacer(minLength: 4)
                    Text(Self.ago(s.when)).font(Theme.mono(10)).foregroundStyle(Theme.faint)
                }
                .padding(.leading, 14).padding(.trailing, 8).padding(.vertical, 5)
                .background(RoundedRectangle(cornerRadius: 5).fill(on ? Theme.hover : .clear))
                .contentShape(Rectangle())
                .onTapGesture { ws.active = project.path; project.resume(s) }
                .help("\(s.messages) messages · \(s.id)")
            }
            if project.sessions.count > 8 {
                Button(all ? "fewer" : "all \(project.sessions.count)") { all.toggle() }
                    .buttonStyle(.plain).font(Theme.mono(11)).foregroundStyle(Theme.faint).padding(.leading, 24).padding(.vertical, 4)
            }
        }
        .padding(.bottom, 6)
    }

    static func ago(_ d: Date) -> String {
        let s = Int(Date().timeIntervalSince(d))
        if s < 60 { return "now" }
        if s < 3600 { return "\(s / 60)m" }
        if s < 86400 { return "\(s / 3600)h" }
        return "\(s / 86400)d"
    }
}

/// Personal ⇄ team, like Claude's Chat ⇄ Code switch: the sidebar shows one space at a time.
struct SpaceSwitch: View {
    @EnvironmentObject var ws: Workspace
    var body: some View {
        HStack(spacing: 2) {
            seg("person", "personal", "Personal projects (⌃Space flips)")
            seg("person.2", "team", "Team projects (⌃Space flips)")
        }
        .padding(2)
        .background(RoundedRectangle(cornerRadius: 8).fill(Theme.hover))
    }

    private func seg(_ icon: String, _ space: String, _ help: String) -> some View {
        let on = ws.space == space
        return Button { ws.setSpace(space) } label: {
            Image(systemName: icon).font(.system(size: 12, weight: .medium))
                .foregroundStyle(on ? Theme.ink : Theme.faint)
                .frame(width: 30, height: 24)
                .background(RoundedRectangle(cornerRadius: 6).fill(on ? Theme.paper : .clear))
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help(help)
    }
}

struct SectionLabel: View {
    let text: String
    let count: Int
    var body: some View {
        HStack {
            Text(text).font(Theme.mono(11)).foregroundStyle(Theme.faint)
            Spacer()
            Text("\(count)").font(Theme.mono(11)).foregroundStyle(Theme.faint)
        }
        .padding(.horizontal, 8).padding(.top, 4).padding(.bottom, 4)
    }
}

/// A project's dot in the sidebar: solid when something waits for you, pulsing while it works.
struct LiveDot: View {
    @ObservedObject var project: Project
    var body: some View {
        if project.waiting > 0 {
            Text("\(project.waiting)").font(Theme.mono(10)).foregroundStyle(Theme.paper)
                .padding(.horizontal, 5).background(Capsule().fill(Theme.accent))
        } else {
            Circle().fill(Theme.accent).frame(width: 6, height: 6)
        }
    }
}

struct Empty: View {
    @EnvironmentObject var ws: Workspace

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Mark(size: 210)
            Text("Open a project folder and qh starts there: sessions, team mode over qh-link, the sandbox, everything.")
                .font(Theme.sans(16)).foregroundStyle(Theme.soft).frame(maxWidth: 520, alignment: .leading)
                .padding(.top, 36).padding(.bottom, 26)
            Button(action: ws.pick) {
                Text("open a project").font(Theme.mono(13)).foregroundStyle(Theme.paper)
                    .padding(.horizontal, 20).padding(.vertical, 13).background(Theme.ink)
            }
            .buttonStyle(.plain)
            if !ws.recent.isEmpty {
                HStack(spacing: 22) {
                    ForEach(ws.recent.prefix(5), id: \.self) { p in
                        Button(Workspace.short(p)) { ws.open(p) }.buttonStyle(.plain).font(Theme.mono(12)).foregroundStyle(Theme.soft)
                    }
                }
                .padding(.top, 44)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .leading)
        .padding(.horizontal, 90)
    }
}

struct Missing: View {
    @State private var copied = false
    private let cmd = "curl -fsSL kesavsabari.github.io/qh/sh | sh"

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Mark(size: 210)
            Text("qh isn't installed on this Mac yet. Run this in Terminal, then reopen the app:")
                .font(Theme.sans(16)).foregroundStyle(Theme.soft).padding(.top, 36).padding(.bottom, 18)
            Text(cmd).font(Theme.mono(15)).foregroundStyle(Theme.ink).textSelection(.enabled).padding(.vertical, 16)
            Button {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(cmd, forType: .string)
                copied = true
            } label: {
                Text(copied ? "copied" : "copy").font(Theme.mono(13)).foregroundStyle(Theme.paper)
                    .padding(.horizontal, 20).padding(.vertical, 13).background(Theme.ink)
            }
            .buttonStyle(.plain)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .leading)
        .padding(.horizontal, 90)
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
