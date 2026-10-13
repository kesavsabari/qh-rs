import AppKit
import SwiftUI

/// The projects you've opened and the tabs running qh in them.
@MainActor
final class Workspace: ObservableObject {
    @Published var projects: [String]
    @Published var tabs: [QhTab] = []
    @Published var active: UUID?
    @Published var version = ""
    let qh: String?
    private var keys: Any?
    private var look: NSKeyValueObservation?

    init() {
        projects = UserDefaults.standard.stringArray(forKey: "projects") ?? []
        qh = Workspace.findQh()
        if let qh {
            Task.detached {
                let v = Workspace.run(qh, ["--version"])
                await MainActor.run { self.version = v }
            }
        }
        // ⌘O open · ⌘W close the tab (not the window) · ⌘1–9 switch tabs
        keys = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] e in
            guard let self, e.modifierFlags.intersection(.deviceIndependentFlagsMask) == .command, let k = e.charactersIgnoringModifiers else { return e }
            switch k {
            case "o": self.pick(); return nil
            case "w" where self.current != nil: self.close(self.current!); return nil
            case "1"..."9":
                let i = Int(k)! - 1
                if i < self.tabs.count { self.show(self.tabs[i]); return nil }
                return e
            default: return e
            }
        }
        // light ⇄ dark: repaint the terminals
        look = NSApplication.shared.observe(\.effectiveAppearance) { [weak self] _, _ in
            Task { @MainActor in self?.tabs.forEach { Theme.paint($0.view) } }
        }
    }

    var current: QhTab? { tabs.first { $0.id == active } }

    func open(_ path: String) {
        if let t = tabs.first(where: { $0.path == path && !$0.done }) { return show(t) }
        guard let qh else { return }
        projects = [path] + projects.filter { $0 != path }
        projects = Array(projects.prefix(14))
        UserDefaults.standard.set(projects, forKey: "projects")
        let t = QhTab(path: path, qh: qh)
        tabs.append(t)
        show(t)
    }

    func show(_ t: QhTab) {
        active = t.id
        DispatchQueue.main.async { t.view.window?.makeFirstResponder(t.view) }
    }

    func close(_ t: QhTab) {
        t.stop()
        tabs.removeAll { $0.id == t.id }
        if active == t.id { active = tabs.last?.id }
    }

    func forget(_ path: String) {
        projects.removeAll { $0 == path }
        UserDefaults.standard.set(projects, forKey: "projects")
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
