import AppKit
import UserNotifications

/// Tells you when qh needs you and you're looking elsewhere: a notification you can answer from
/// (Allow / Always / Deny) or click to jump to the tab, and the Dock badge counts permissions
/// waiting across every project.
@MainActor
enum Notify {
    static weak var workspace: Workspace?
    private static let delegate = Delegate()

    static func setup() {
        let c = UNUserNotificationCenter.current()
        c.delegate = delegate
        let allow = UNNotificationAction(identifier: "yes", title: "Allow")
        let always = UNNotificationAction(identifier: "always", title: "Always Allow")
        let deny = UNNotificationAction(identifier: "no", title: "Deny", options: [.destructive])
        c.setNotificationCategories([
            UNNotificationCategory(identifier: "ask", actions: [allow, always, deny], intentIdentifiers: []),
            UNNotificationCategory(identifier: "ask-once", actions: [allow, deny], intentIdentifiers: []),
        ])
        c.requestAuthorization(options: [.alert, .sound, .badge]) { _, _ in }
    }

    static func ask(_ p: Project, _ a: Ask) {
        let body = a.detail.isEmpty ? "qh wants to run \(a.tool)" : "\(a.tool): \(a.detail.prefix(160))"
        post("\(p.name) · \(a.tab) needs you", body, id: askID(p, a.id),
             category: a.always.isEmpty || a.guarded ? "ask-once" : "ask", info: ["path": p.path, "tab": a.tab, "ask": a.id])
    }

    /// The ask was answered (here, in another window, or it timed out): its notification goes.
    static func clear(_ p: Project, _ id: Int) {
        UNUserNotificationCenter.current().removeDeliveredNotifications(withIdentifiers: [askID(p, id)])
    }

    static func done(project p: Project, tab: String) {
        post("\(p.name) · \(tab) is done", "qh finished its turn", id: UUID().uuidString, category: "", info: ["path": p.path, "tab": tab])
    }

    private static func askID(_ p: Project, _ id: Int) -> String { "ask:\(p.path):\(id)" }

    private static func post(_ title: String, _ body: String, id: String, category: String, info: [String: Any]) {
        guard !NSApp.isActive else { return }
        let c = UNMutableNotificationContent()
        c.title = title
        c.body = body
        c.sound = .default
        c.categoryIdentifier = category
        c.userInfo = info
        UNUserNotificationCenter.current().add(UNNotificationRequest(identifier: id, content: c, trigger: nil))
    }

    static func badge() {
        guard let ws = workspace else { return }
        let n = ws.projects.reduce(0) { $0 + $1.waiting }
        let busy = ws.projects.reduce(0) { $0 + $1.tabs.filter(\.busy).count }
        NSApp.dockTile.badgeLabel = n > 0 ? "\(n)" : nil
        if ws.waiting != n { ws.waiting = n }   // the menu bar label follows these
        if ws.working != busy { ws.working = busy }
    }

    /// A button on the notification answers the ask; a click on it opens that tab.
    fileprivate static func respond(_ action: String, _ info: [AnyHashable: Any]) {
        guard let ws = workspace, let path = info["path"] as? String,
              let p = ws.projects.first(where: { $0.path == path }) else { return }
        let tab = info["tab"] as? String ?? ""
        if let id = info["ask"] as? Int, ["yes", "always", "no"].contains(action) {
            if let a = p.asks.first(where: { $0.id == id }) { p.answer(a, action) }
            return
        }
        ws.focus(path, tab: tab)
        NSApp.activate(ignoringOtherApps: true)
    }

    private final class Delegate: NSObject, UNUserNotificationCenterDelegate {
        func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse,
                                    withCompletionHandler done: @escaping () -> Void) {
            let action = response.actionIdentifier
            let info = response.notification.request.content.userInfo
            DispatchQueue.main.async { MainActor.assumeIsolated { Notify.respond(action, info) }; done() }
        }
    }
}
