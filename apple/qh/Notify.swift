import AppKit
import UserNotifications

/// Tells you when qh needs you and you're looking elsewhere: a notification, and the Dock badge
/// counts permissions waiting across every project.
@MainActor
enum Notify {
    static weak var workspace: Workspace?

    static func setup() {
        UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound, .badge]) { _, _ in }
    }

    static func ask(project: String, tab: String, tool: String) {
        post("\(project) · \(tab) needs you", "qh wants to run \(tool)")
    }

    static func done(project: String, tab: String) {
        post("\(project) · \(tab) is done", "qh finished its turn")
    }

    private static func post(_ title: String, _ body: String) {
        guard !NSApp.isActive else { return }
        let c = UNMutableNotificationContent()
        c.title = title
        c.body = body
        c.sound = .default
        UNUserNotificationCenter.current().add(UNNotificationRequest(identifier: UUID().uuidString, content: c, trigger: nil))
    }

    static func badge() {
        let n = workspace?.projects.reduce(0) { $0 + $1.waiting } ?? 0
        NSApp.dockTile.badgeLabel = n > 0 ? "\(n)" : nil
    }
}
