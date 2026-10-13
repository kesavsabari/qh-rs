import AppKit
import SwiftTerm
import SwiftUI

/// One project tab: qh running in a real terminal (SwiftTerm), started through your login shell
/// so the commands qh runs find what your own terminal finds (git, node, cargo…).
@MainActor
final class QhTab: NSObject, ObservableObject, Identifiable, LocalProcessTerminalViewDelegate {
    let id = UUID()
    let path: String
    var name: String { (path as NSString).lastPathComponent }
    @Published var done = false
    let view: LocalProcessTerminalView

    init(path: String, qh: String) {
        self.path = path
        view = LocalProcessTerminalView(frame: NSRect(x: 0, y: 0, width: 900, height: 600))
        super.init()
        view.processDelegate = self
        view.font = Theme.monoNS(13.5)
        view.optionAsMetaKey = true
        Theme.paint(view)

        var env = ProcessInfo.processInfo.environment
        env["TERM"] = "xterm-256color"
        env["COLORTERM"] = "truecolor"
        env["LANG"] = env["LANG"] ?? "en_US.UTF-8"
        env["QH_BIN"] = qh
        env["QH_CWD"] = path
        env["QH_EDITOR"] = "desktop"
        let shell = env["SHELL"] ?? "/bin/zsh"
        view.startProcess(
            executable: shell,
            args: ["-l", "-c", "cd \"$QH_CWD\" && exec \"$QH_BIN\""],
            environment: env.map { "\($0.key)=\($0.value)" },
            execName: "-" + (shell as NSString).lastPathComponent
        )
    }

    func stop() {
        if !done { view.process?.terminate() }
    }

    // LocalProcessTerminalViewDelegate
    nonisolated func sizeChanged(source: LocalProcessTerminalView, newCols: Int, newRows: Int) {}
    nonisolated func setTerminalTitle(source: LocalProcessTerminalView, title: String) {}
    nonisolated func hostCurrentDirectoryUpdate(source: TerminalView, directory: String?) {}
    nonisolated func processTerminated(source: TerminalView, exitCode: Int32?) {
        Task { @MainActor in
            self.done = true
            self.view.feed(text: "\r\n\u{1b}[90m  qh ended. Close this tab, or open the project again.\u{1b}[0m\r\n")
        }
    }
}

/// Hosts a tab's terminal view in SwiftUI. The view lives as long as the tab, so switching
/// tabs never restarts qh.
struct TerminalHost: NSViewRepresentable {
    let tab: QhTab
    func makeNSView(context: Context) -> LocalProcessTerminalView { tab.view }
    func updateNSView(_ v: LocalProcessTerminalView, context: Context) {}
}
