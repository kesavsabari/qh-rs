import AppKit
import SwiftUI

/// The folders you've opened, and the ones open now, each with its own `qh serve`.
@MainActor
final class Workspace: ObservableObject {
    @Published var recent: [String]
    @Published var projects: [Project] = []
    @Published var active: String?
    @Published var version = ""
    @Published var palette = false   // ⌘K
    @Published var waiting = 0   // permissions waiting, every project
    @Published var working = 0   // tabs running a turn, every project
    /// folder → GitHub repo, for the folders that are team projects (asked from qh itself)
    @Published var teamRepos: [String: String] = [:]
    private var teamsKnown = false   // until qh __team has answered once, don't pick a space from it
    /// which space the sidebar shows: "personal" or "team"
    @Published var space = UserDefaults.standard.string(forKey: "space") ?? "personal"
    @Published var inspector = UserDefaults.standard.bool(forKey: "inspector")
    @Published var teamPage: TeamPage = .channel("#team")

    /// the team project the team space shows: the open one if it's a team project, else the last
    var teamProject: String? {
        if let a = active, teamRepos[a] != nil { return a }
        return recent.first { r in teamRepos[r] != nil && projects.contains { $0.path == r } } ?? recent.first { teamRepos[$0] != nil }
    }

    /// the personal project on screen: the open one, or the last open personal one while a team
    /// project is active (each space keeps its own place)
    var personalProject: Project? {
        if let c = current, teamRepos[c.path] == nil { return c }
        return recent.lazy.compactMap { p in self.teamRepos[p] == nil ? self.projects.first { $0.path == p } : nil }.first
    }

    /// the toolbar's title: what the content column shows
    var title: String {
        if space == "team" {
            guard let p = teamProject else { return "Team" }
            return (p as NSString).lastPathComponent + (teamPage == .agent ? " · your agent" : "")
        }
        guard let p = personalProject else { return "qh" }
        return [p.name, p.current?.purpose.isEmpty == false ? p.current!.purpose : p.current?.name ?? ""].filter { !$0.isEmpty }.joined(separator: " · ")
    }
    let qh: String?
    private var keys: Any?

    init() {
        recent = UserDefaults.standard.stringArray(forKey: "projects") ?? []
        qh = Workspace.findQh()
        if let qh {
            Task.detached {
                let v = Workspace.run(qh, ["--version"])
                await MainActor.run { self.version = v }
            }
        }
        // ⌘O open · ⌘T new tab · ⌘W close the tab (not the window) · ⌘1–9 switch tabs · Esc stop
        keys = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] e in
            guard let self else { return e }
            let mods = e.modifierFlags.intersection(.deviceIndependentFlagsMask)
            if e.keyCode == 49, mods == .control { self.setSpace(self.space == "team" ? "personal" : "team"); return nil }
            if e.keyCode == 53, mods.isEmpty, let p = self.current, p.current?.busy == true { p.stop(); return nil }
            guard mods == .command, let k = e.charactersIgnoringModifiers else { return e }
            switch k {
            case "o": self.pick(); return nil
            case "k": self.palette.toggle(); return nil
            case "t": self.current?.newTab(); return self.current == nil ? e : nil
            case "w":
                guard let p = self.current, let t = p.current else { return e }
                if p.tabs.count > 1 { p.closeTab(t.name) } else { self.close(p) }
                return nil
            case "1"..."9":
                guard let p = self.current else { return e }
                let i = Int(k)! - 1
                if i < p.tabs.count { p.active = p.tabs[i].name; return nil }
                return e
            default: return e
            }
        }
        refreshTeams()
        NotificationCenter.default.addObserver(forName: NSApplication.didBecomeActiveNotification, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { self?.refreshTeams() }
        }
        Notify.workspace = self
        WorkspaceHolder.ws = self
        Notify.setup()
        // back where you were: the project open when the app last quit
        if let last = UserDefaults.standard.string(forKey: "lastOpen"), FileManager.default.fileExists(atPath: last) { open(last) }
    }

    var current: Project? { projects.first { $0.path == active } }

    /// Show one project's tab, in whichever space it lives (from a notification, the menu bar, ⌘K).
    func focus(_ path: String, tab: String? = nil) {
        if teamRepos[path] != nil { setSpace("team"); teamPage = .agent } else { setSpace("personal") }
        active = path
        if let tab, let p = current, p.tabs.contains(where: { $0.name == tab }) { p.active = tab }
    }

    func setSpace(_ s: String) {
        space = s
        UserDefaults.standard.set(s, forKey: "space")
    }

    func toggleInspector() {
        inspector.toggle()
        UserDefaults.standard.set(inspector, forKey: "inspector")
    }

    /// Which recent folders are team projects: `qh __team` answers with qh's own check.
    func refreshTeams() {
        guard let qh else { return }
        let paths = recent
        Task.detached {
            let out = Workspace.run(qh, ["__team"] + paths)
            let map = (try? JSONSerialization.jsonObject(with: Data(out.utf8)) as? [String: Any]) ?? [:]
            let teams = map.compactMapValues { $0 as? String }
            await MainActor.run {
                self.teamRepos = teams
                self.teamsKnown = true
                // a folder that just turned out to be a team project: show the team space
                if let a = self.active { self.setSpace(teams[a] != nil ? "team" : "personal") }
            }
        }
    }

    func open(_ path: String) {
        recent = Array(([path] + recent.filter { $0 != path }).prefix(14))
        UserDefaults.standard.set(recent, forKey: "projects")
        refreshTeams()
        if teamsKnown { setSpace(teamRepos[path] != nil ? "team" : "personal") }
        if !projects.contains(where: { $0.path == path }), let qh {
            projects.append(Project(path: path, qh: qh))
        }
        active = path
        UserDefaults.standard.set(path, forKey: "lastOpen")
    }

    func close(_ p: Project) {
        p.shutdown()
        projects.removeAll { $0.path == p.path }
        if active == p.path { active = projects.last?.path }
    }

    func shutdown() { projects.forEach { $0.shutdown() } }

    func forget(_ path: String) {
        recent.removeAll { $0 == path }
        UserDefaults.standard.set(recent, forKey: "projects")
    }

    func pick() {
        let p = NSOpenPanel()
        p.canChooseDirectories = true
        p.canChooseFiles = false
        p.allowsMultipleSelection = false
        p.prompt = "Open in qh"
        p.message = "Pick a project folder: qh starts there."
        if p.runModal() == .OK, let url = p.url { open(url.path) }
    }

    static var home: String { FileManager.default.homeDirectoryForCurrentUser.path }

    static func short(_ p: String) -> String { p.hasPrefix(home) ? "~" + p.dropFirst(home.count) : p }

    /// The install line puts qh in ~/.local/bin, which an app opened from the Dock doesn't have on its PATH.
    static func findQh() -> String? {
        var c = ["\(home)/.local/bin/qh", "/opt/homebrew/bin/qh", "/usr/local/bin/qh"]
        for d in (ProcessInfo.processInfo.environment["PATH"] ?? "").split(separator: ":") { c.append("\(d)/qh") }
        return c.first { FileManager.default.isExecutableFile(atPath: $0) }
    }

    nonisolated static func run(_ exe: String, _ args: [String]) -> String {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: exe)
        p.arguments = args
        let out = Pipe()
        p.standardOutput = out
        p.standardError = Pipe()
        do { try p.run() } catch { return "" }
        p.waitUntilExit()
        return String(data: out.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
    }
}
