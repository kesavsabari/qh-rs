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
        .windowToolbarStyle(.unified(showsTitle: false))
        .defaultSize(width: 1280, height: 820)
        .commands {
            CommandGroup(after: .toolbar) {
                Picker("Appearance", selection: $look) {
                    Text("System").tag("system")
                    Text("Light").tag("light")
                    Text("Dark").tag("dark")
                }
                .onChange(of: look) { _, v in QhApp.apply(v) }
            }
            CommandGroup(replacing: .newItem) {
                Button("Open Folder…") { ws.pick() }.keyboardShortcut("o")
                Button("Review Changes") { ws.current?.loadChanges(); ws.current?.showReview = true }.keyboardShortcut("d").disabled(ws.current == nil)
                Button("Team Space") { ws.setSpace("team") }.keyboardShortcut("t", modifiers: [.command, .shift])
                Button("Inspector") { ws.toggleInspector() }.keyboardShortcut("i", modifiers: [.command, .option])
                Button("Background Tasks") { ws.current?.showTasks = true }.keyboardShortcut("b", modifiers: [.command, .shift]).disabled(ws.current == nil)
                Button("Replies Only") { ws.current?.repliesOnly.toggle() }.keyboardShortcut("r").disabled(ws.current == nil)
                Button("Models…") { ws.current?.showSetup = true }.keyboardShortcut(",").disabled(ws.current == nil)
                Button("New Tab") { ws.current?.newTab() }.keyboardShortcut("t").disabled(ws.current == nil)
            }
        }
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ n: Notification) {
        QhApp.apply(UserDefaults.standard.string(forKey: "appearance") ?? "system")
    }

    func applicationWillTerminate(_ n: Notification) {
        MainActor.assumeIsolated { Notify.workspace?.shutdown() }
    }

    // closing the last window quits, and quitting ends every qh the app started
    func applicationShouldTerminateAfterLastWindowClosed(_ app: NSApplication) -> Bool { true }
}
