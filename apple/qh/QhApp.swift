import SwiftUI

@main
struct QhApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) var delegate
    @StateObject private var ws: Workspace

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
            CommandGroup(replacing: .newItem) {
                Button("Open Folder…") { ws.pick() }.keyboardShortcut("o")
                Button("Review Changes") { ws.current?.loadChanges(); ws.current?.showReview = true }.keyboardShortcut("d").disabled(ws.current == nil)
                Button("Team Room") { ws.current?.showTeam = true }.keyboardShortcut("t", modifiers: [.command, .shift]).disabled(ws.current == nil)
                Button("Background Tasks") { ws.current?.showTasks = true }.keyboardShortcut("b", modifiers: [.command, .shift]).disabled(ws.current == nil)
                Button("Replies Only") { ws.current?.repliesOnly.toggle() }.keyboardShortcut("r").disabled(ws.current == nil)
                Button("Models…") { ws.current?.showSetup = true }.keyboardShortcut(",").disabled(ws.current == nil)
                Button("New Tab") { ws.current?.newTab() }.keyboardShortcut("t").disabled(ws.current == nil)
            }
        }
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationWillTerminate(_ n: Notification) {
        MainActor.assumeIsolated { Notify.workspace?.shutdown() }
    }

    // closing the last window quits, and quitting ends every qh the app started
    func applicationShouldTerminateAfterLastWindowClosed(_ app: NSApplication) -> Bool { true }
}
