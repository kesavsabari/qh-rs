import AppKit
import SwiftUI

/// The menu bar extra: every open project's agents and what they're doing, and permissions you
/// can answer without opening the window.
struct MenuBarView: View {
    @EnvironmentObject var ws: Workspace

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack {
                Text("qh").font(Theme.sans(13, medium: true)).foregroundStyle(Theme.ink)
                Spacer()
                Text(summary).font(Theme.mono(11)).foregroundStyle(Theme.faint)
            }
            .padding(.horizontal, 12).padding(.vertical, 10)
            Rectangle().fill(Theme.rule).frame(height: 1)
            if ws.projects.isEmpty {
                Text("No project open.").font(Theme.sans(12.5)).foregroundStyle(Theme.faint).padding(12)
            }
            ScrollView {
                VStack(alignment: .leading, spacing: 10) {
                    ForEach(ws.projects) { p in MenuProject(project: p) }
                }
                .padding(8)
            }
            .frame(maxHeight: 420)
            Rectangle().fill(Theme.rule).frame(height: 1)
            HStack {
                Button("Open qh") { NSApp.activate(ignoringOtherApps: true); NSApp.windows.first { $0.canBecomeMain }?.makeKeyAndOrderFront(nil) }
                Spacer()
                Button("Quit") { NSApp.terminate(nil) }
            }
            .buttonStyle(.hand).font(Theme.sans(12)).foregroundStyle(Theme.soft)
            .padding(.horizontal, 12).padding(.vertical, 9)
        }
        .frame(width: 340)
        .background(Theme.paper)
    }

    private var summary: String {
        return [ws.waiting > 0 ? "\(ws.waiting) waiting" : "", ws.working > 0 ? "\(ws.working) working" : ""].filter { !$0.isEmpty }.joined(separator: " · ").ifEmpty("all idle")
    }
}

/// The menu bar icon: a ring per running agent (up to 3), the first one orange when something
/// waits for you; a small dot when all is idle. Same drawing as the Windows tray icon.
struct MenuBarLabel: View {
    @EnvironmentObject var ws: Workspace
    var body: some View {
        Image(nsImage: MenuBarIcon.image(running: ws.working, waiting: ws.waiting))
    }
}

enum MenuBarIcon {
    static func image(running: Int, waiting: Int) -> NSImage {
        let n = min(3, max(running, waiting > 0 ? 1 : 0)), orange = waiting > 0 && n > 0
        let s: CGFloat = 16, k = s / 32   // laid out on the tray's 32px grid
        let img = NSImage(size: NSSize(width: s, height: s), flipped: false) { _ in
            // template: the system tints it; otherwise ink follows the menu bar's appearance
            let ink = orange ? NSColor.labelColor : NSColor.black
            if n == 0 {
                ink.setFill()
                NSBezierPath(ovalIn: NSRect(x: (16 - 4.5) * k, y: (16 - 4.5) * k, width: 9 * k, height: 9 * k)).fill()
                return true
            }
            let r: CGFloat = n == 1 ? 9 : n == 2 ? 6.5 : 4.6, gap = 2 * r + 2.5, x0 = 16 - CGFloat(n - 1) * gap / 2
            for i in 0..<n {
                let cx = x0 + CGFloat(i) * gap
                let ring = NSBezierPath(ovalIn: NSRect(x: (cx - r) * k, y: (16 - r) * k, width: 2 * r * k, height: 2 * r * k))
                ring.lineWidth = 2.6 * k
                (i == 0 && orange ? NSColor(srgbRed: 1, green: 0x4D / 255.0, blue: 0x1C / 255.0, alpha: 1) : ink).setStroke()
                ring.stroke()
            }
            return true
        }
        // a template adapts to light and dark menu bars; orange can't be a template
        img.isTemplate = !orange
        return img
    }
}

private struct MenuProject: View {
    @ObservedObject var project: Project
    @EnvironmentObject var ws: Workspace

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(project.name).font(Theme.sans(11, medium: true)).foregroundStyle(Theme.faint).padding(.horizontal, 6)
            ForEach(project.tabs) { t in MenuTab(project: project, tab: t) }
            ForEach(project.asks) { a in
                VStack(alignment: .leading, spacing: 6) {
                    Text("\(a.tab) wants to run \(a.tool)").font(Theme.sans(12, medium: true)).foregroundStyle(Theme.ink)
                    if !a.detail.isEmpty {
                        Text(a.detail).font(Theme.mono(11)).foregroundStyle(Theme.soft).lineLimit(3)
                    }
                    HStack(spacing: 8) {
                        Button("Allow") { project.answer(a, "yes") }.buttonStyle(.borderedProminent).tint(Theme.accent)
                        if !a.always.isEmpty && !a.guarded { Button("Always") { project.answer(a, "always") } }
                        Button("Deny") { project.answer(a, "no") }
                    }
                    .controlSize(.small)
                }
                .padding(8)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(RoundedRectangle(cornerRadius: 8).fill(Theme.accent.opacity(0.08)))
            }
        }
    }
}

private struct MenuTab: View {
    @ObservedObject var project: Project
    @ObservedObject var tab: ChatTab
    @EnvironmentObject var ws: Workspace

    var body: some View {
        Row(selected: false, action: {
            ws.focus(project.path, tab: tab.name)
            NSApp.activate(ignoringOtherApps: true)
        }) {
            HStack(spacing: 8) {
                Circle().fill(color).frame(width: 6, height: 6)
                Text(tab.purpose.isEmpty ? tab.name : tab.purpose).font(Theme.sans(12.5)).foregroundStyle(Theme.ink).lineLimit(1)
                Spacer(minLength: 4)
                Text(label).font(Theme.mono(10.5)).foregroundStyle(Theme.faint)
            }
        }
    }

    private var waiting: Bool { project.asks.contains { $0.tab == tab.name } }
    private var color: SwiftUI.Color { waiting ? Theme.accent : tab.busy ? SwiftUI.Color(nsColor: .systemGreen) : Theme.faint.opacity(0.5) }
    private var label: String { waiting ? "needs you" : tab.busy ? "working" : "idle" }
}

extension String {
    func ifEmpty(_ s: String) -> String { isEmpty ? s : self }
}
