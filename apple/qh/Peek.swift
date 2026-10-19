import AppKit
import Carbon.HIToolbox
import SwiftUI

// ⌥Space from anywhere: a small floating field that sends one prompt to the current project's
// current tab without bringing the main window forward. Carbon's hotkey needs no Accessibility.
@MainActor
enum Peek {
    private static var ref: EventHotKeyRef?
    private static var handler: EventHandlerRef?
    private static var panel: NSPanel?

    static func start() {
        sync()
        NotificationCenter.default.addObserver(forName: UserDefaults.didChangeNotification, object: nil, queue: .main) { _ in
            MainActor.assumeIsolated { sync() }
        }
    }

    // register or drop the hotkey to match the "peekHotkey" setting (default on)
    private static func sync() {
        let on = UserDefaults.standard.object(forKey: "peekHotkey") as? Bool ?? true
        if on, ref == nil {
            if handler == nil {
                var spec = EventTypeSpec(eventClass: OSType(kEventClassKeyboard), eventKind: UInt32(kEventHotKeyPressed))
                InstallEventHandler(GetApplicationEventTarget(), { _, _, _ in
                    DispatchQueue.main.async { MainActor.assumeIsolated { Peek.toggle() } }
                    return noErr
                }, 1, &spec, nil, &handler)
            }
            RegisterEventHotKey(UInt32(kVK_Space), UInt32(optionKey), EventHotKeyID(signature: 0x7168706B, id: 1), GetApplicationEventTarget(), 0, &ref)
        } else if !on, let r = ref {
            UnregisterEventHotKey(r)
            ref = nil
            close()
        }
    }

    static func toggle() { panel?.isVisible == true ? close() : show() }

    static func show() {
        let size = NSSize(width: 560, height: 110)
        let p = panel ?? {
            let p = PeekPanel(contentRect: NSRect(origin: .zero, size: size), styleMask: [.nonactivatingPanel, .borderless], backing: .buffered, defer: false)
            p.level = .floating
            p.isFloatingPanel = true
            p.hidesOnDeactivate = false
            p.isOpaque = false
            p.backgroundColor = .clear
            p.hasShadow = true
            p.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
            panel = p
            return p
        }()
        p.contentView = NSHostingView(rootView: PeekField())
        let mouse = NSEvent.mouseLocation
        let f = (NSScreen.screens.first { $0.frame.contains(mouse) } ?? NSScreen.main)?.visibleFrame ?? .zero
        p.setFrameOrigin(NSPoint(x: f.midX - size.width / 2, y: f.midY - size.height / 2 + f.height / 6))
        p.makeKeyAndOrderFront(nil)
    }

    static func close() { panel?.orderOut(nil) }

    static func send(_ text: String) {
        Notify.workspace?.current?.prompt(text)
        close()
    }

    static var window: NSWindow? { panel?.isVisible == true ? panel : nil }
}

// borderless panels refuse key by default; the field needs it
private final class PeekPanel: NSPanel {
    override var canBecomeKey: Bool { true }
}

private struct PeekField: View {
    @State private var text = ""
    @FocusState private var focused: Bool
    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            TextField("Ask the current tab…", text: $text)
                .textFieldStyle(.plain)
                .font(Theme.sans(18))
                .foregroundStyle(Theme.ink)
                .focused($focused)
                .onSubmit { if !text.trimmingCharacters(in: .whitespaces).isEmpty { Peek.send(text) } }
                .onExitCommand { Peek.close() }
            HStack(spacing: 6) {
                Circle().fill(Theme.accent).frame(width: 6, height: 6)
                Text(MainActor.assumeIsolated { Notify.workspace?.current?.name } ?? "No project open")
                    .font(Theme.sans(11.5)).foregroundStyle(Theme.soft)
                Spacer()
                Text("↩ send   esc close").font(Theme.sans(11.5)).foregroundStyle(Theme.faint)
            }
        }
        .padding(.horizontal, 22)
        .frame(width: 560, height: 110)
        .background(RoundedRectangle(cornerRadius: 14).fill(Theme.card))
        .overlay(RoundedRectangle(cornerRadius: 14).stroke(Theme.line))
        .onAppear { focused = true }
    }
}
