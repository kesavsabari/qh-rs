import SwiftUI

@main
struct QhApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) var delegate
    @StateObject private var ws: Workspace
    @AppStorage("appearance") private var look = "system"

    /// Light, dark, or follow the Mac: every Theme colour has both, so this is all it takes.
    static func apply(_ v: String) {
        NSApp.appearance = v == "light" ? NSAppearance(named: .aqua) : v == "dark" ? NSAppearance(named: .darkAqua) : nil
    }

    init() {
        Theme.registerFonts()
        _ws = StateObject(wrappedValue: Workspace())
    }

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environmentObject(ws)
                .frame(minWidth: 760, minHeight: 480)
        }
        .windowStyle(.hiddenTitleBar)
        .defaultSize(width: 1280, height: 820)
        .commands {
            CommandGroup(after: .toolbar) {
                Picker("Appearance", selection: $look) {
                    Text("System").tag("system")
                    Text("Light").tag("light")
                    Text("Dark").tag("dark")
                }
                .onChange(of: look) { _, v in QhApp.apply(v) }
                Button("Compact Sidebar") { ws.toggleCompact() }.keyboardShortcut("\\", modifiers: [.command, .control])
            }
            // settings live in the main window, not a separate one
            CommandGroup(replacing: .appSettings) {
                Button("Settings…") { ws.settings.toggle() }.keyboardShortcut(",")
            }
            CommandGroup(replacing: .newItem) {
                Button("Open Folder…") { ws.pick() }.keyboardShortcut("o")
                Button("Command Palette…") { ws.palette = true }.keyboardShortcut("k")
                Button("Review Changes") { ws.current?.loadChanges(); ws.current?.showReview = true }.keyboardShortcut("d").disabled(ws.current == nil)
                Button("Team Space") { ws.setSpace("team") }.keyboardShortcut("t", modifiers: [.command, .shift])
                Button("Inspector") { ws.toggleInspector() }.keyboardShortcut("i", modifiers: [.command, .option])
                Button("Background Tasks") { ws.current?.showTasks = true }.keyboardShortcut("b", modifiers: [.command, .shift]).disabled(ws.current == nil)
                Button("Replies Only") { ws.current?.repliesOnly.toggle() }.keyboardShortcut("r").disabled(ws.current == nil)
                Button("Models…") { ws.current?.showSetup = true }.keyboardShortcut("m", modifiers: [.command, .shift]).disabled(ws.current == nil)
                Button("New Tab") { ws.current?.newTab() }.keyboardShortcut("t").disabled(ws.current == nil)
                Divider()
                Button("Chat") { ws.current?.layout = "chat" }.keyboardShortcut("1", modifiers: .control).disabled(ws.current == nil)
                Button("Chat and Code") { ws.current?.layout = "split" }.keyboardShortcut("2", modifiers: .control).disabled(ws.current == nil)
                Button("Code") { ws.current?.layout = "code" }.keyboardShortcut("3", modifiers: .control).disabled(ws.current == nil)
            }
        }
        MenuBarExtra {
            MenuBarView().environmentObject(ws)
        } label: {
            MenuBarLabel().environmentObject(ws)
        }
        .menuBarExtraStyle(.window)
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ n: Notification) {
        QhApp.apply(UserDefaults.standard.string(forKey: "appearance") ?? "system")
        TrafficLights.start()
        Peek.start()
        // for checking the UI from another machine without screen-recording rights: when the
        // `debugSnapshot` default is on, a distributed "io.qh.snapshot" notification makes the app
        // draw its own window into /tmp/qh-window.png (only its own pixels, nothing else on screen)
        if UserDefaults.standard.bool(forKey: "debugSnapshot") {
            // and "io.qh.page" with a page name as its object: show that page (agent, requests, members…)
            DistributedNotificationCenter.default().addObserver(forName: .init("io.qh.page"), object: nil, queue: .main) { n in
                let page = n.object as? String ?? ""
                MainActor.assumeIsolated {
                    guard let ws = Notify.workspace else { return }
                    switch page {
                    case "agent": ws.teamPage = .agent
                    case "requests": ws.teamPage = .requests
                    case "members": ws.teamPage = .members
                    case "claims": ws.teamPage = .claims
                    case "activity": ws.teamPage = .activity
                    case "map": ws.teamPage = .map
                    case "personal", "team": ws.setSpace(page)
                    case "palette": ws.palette = true
                    default: ws.teamPage = .channel(page)
                    }
                }
            }
            // "io.qh.project" with a folder: open that project in Personal
            DistributedNotificationCenter.default().addObserver(forName: .init("io.qh.project"), object: nil, queue: .main) { n in
                let p = n.object as? String ?? ""
                MainActor.assumeIsolated { Notify.workspace?.setSpace("personal"); Notify.workspace?.open(p) }
            }
            // "io.qh.resume" with a session id: open it in the current project
            DistributedNotificationCenter.default().addObserver(forName: .init("io.qh.resume"), object: nil, queue: .main) { n in
                let id = n.object as? String ?? ""
                MainActor.assumeIsolated {
                    guard let p = Notify.workspace?.current else { return }
                    p.resume(p.sessions.first { $0.id == id } ?? SavedSession(id: id, title: "", messages: 0, when: Date()))
                }
            }
            // "io.qh.open" with "path[:line]": open it in the code view, as a click on it in the thread would
            DistributedNotificationCenter.default().addObserver(forName: .init("io.qh.open"), object: nil, queue: .main) { n in
                let p = n.object as? String ?? ""
                MainActor.assumeIsolated { Notify.workspace?.current?.openFile(p) }
            }
            // "io.qh.peek": open the peek panel, as ⌥Space would
            DistributedNotificationCenter.default().addObserver(forName: .init("io.qh.peek"), object: nil, queue: .main) { _ in
                MainActor.assumeIsolated { Peek.show() }
            }
            DistributedNotificationCenter.default().addObserver(forName: .init("io.qh.snapshot"), object: nil, queue: .main) { _ in
                MainActor.assumeIsolated {
                    guard let main = Peek.window ?? NSApp.windows.first(where: { $0.isVisible && $0.contentView != nil && $0.sheetParent == nil }), case let w = main.attachedSheet ?? main, let v = w.contentView?.superview ?? w.contentView,
                          let rep = v.bitmapImageRepForCachingDisplay(in: v.bounds) else { return }
                    v.cacheDisplay(in: v.bounds, to: rep)
                    try? rep.representation(using: .png, properties: [:])?.write(to: URL(fileURLWithPath: "/tmp/qh-window.png"))
                }
            }
        }
    }

    func applicationWillTerminate(_ n: Notification) {
        MainActor.assumeIsolated { Notify.workspace?.shutdown() }
    }

    // closing the last window quits, and quitting ends every qh the app started
    func applicationShouldTerminateAfterLastWindowClosed(_ app: NSApplication) -> Bool { true }
}

/// The traffic lights stay visible over the hidden title bar (he asked 2026-10-21; they used to hide
/// until hover). Re-applied at 10 Hz because SwiftUI can reset them on window changes.
@MainActor
enum TrafficLights {
    private static var shown: [ObjectIdentifier: Bool] = [:]

    static func start() {
        Timer.scheduledTimer(withTimeInterval: 0.1, repeats: true) { _ in
            MainActor.assumeIsolated {
                _ = NSEvent.mouseLocation
                for w in NSApp.windows where w.isVisible && w.styleMask.contains(.titled) && w.sheetParent == nil {
                    set(w, true)   // close / minimise / zoom always visible; the title bar stays hidden
                }
            }
        }
    }

    private static func set(_ w: NSWindow, _ show: Bool) {
        let id = ObjectIdentifier(w)
        guard shown[id] != show else { return }
        shown[id] = show
        let buttons = [NSWindow.ButtonType.closeButton, .miniaturizeButton, .zoomButton].compactMap { w.standardWindowButton($0) }
        NSAnimationContext.runAnimationGroup { c in
            c.duration = show ? 0.12 : 0.25
            for b in buttons { b.animator().alphaValue = show ? 1 : 0 }
        }
    }
}
