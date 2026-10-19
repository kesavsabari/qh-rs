import AppKit
import CoreText
import SwiftUI
import SwiftTerm

/// The qh design: paper and ink, one signal orange, Geist. Light and dark follow the Mac.
enum Theme {
    // the Zen layout: paper is the floating card, side the grey insets inside it
    static let paper = dyn(0xFBFBF9, 0x1D1D1B)
    static let side = dyn(0xF0EFEA, 0x262624)
    static let ink = dyn(0x0E0E0D, 0xECEBE6)
    static let soft = dyn(0x6B6B66, 0x9C9B94)
    static let faint = dyn(0x9A9A94, 0x6F6E68)
    static let rule = dyn(0xDCDCD5, 0x262624)
    static let hover = dyn(0xE6E6E0, 0x1C1C1A)
    static let accent = dyn(0xFF4D1C, 0xFF5A2A)
    // chrome sits on a warm ground, the content floats on it as a card
    static let ground = dyn(0xECEBE6, 0x141413)
    static let card = dyn(0xFBFBF9, 0x1D1D1B)
    static let tile = dyn(0xF0EFEA, 0x262624)
    static let line = dyn(0xE3E2DC, 0x2A2A28)

    static func sans(_ size: CGFloat, medium: Bool = false) -> Font { .custom(medium ? "Geist-Medium" : "Geist-Regular", size: size) }
    static func mono(_ size: CGFloat, medium: Bool = false) -> Font { .custom(medium ? "GeistMono-Medium" : "GeistMono-Regular", size: size) }
    /// "openrouter:anthropic/claude-opus-5.5" → "claude-opus-5.5"; a local "qwen3.5:9b" stays whole.
    /// Only the provider qh says the tab uses is cut, so a model's own tag never is.
    static func shortModel(_ m: String, provider: String) -> String {
        let rest = !provider.isEmpty && m.hasPrefix(provider + ":") ? String(m.dropFirst(provider.count + 1)) : m
        let s = rest.split(separator: "/").last.map(String.init) ?? rest
        return s.isEmpty ? m : s
    }
    static func monoNS(_ size: CGFloat) -> NSFont { NSFont(name: "GeistMono-Regular", size: size) ?? .monospacedSystemFont(ofSize: size, weight: .regular) }

    static var isDark: Bool { NSApplication.shared.effectiveAppearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua }

    /// The fonts ship inside the app.
    static func registerFonts() {
        for url in Bundle.main.urls(forResourcesWithExtension: "ttf", subdirectory: nil) ?? [] {
            CTFontManagerRegisterFontsForURL(url as CFURL, .process, nil)
        }
    }

    private static func dyn(_ light: UInt32, _ dark: UInt32) -> SwiftUI.Color {
        SwiftUI.Color(NSColor(name: nil) { ap in
            ns(ap.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? dark : light)
        })
    }

    static func ns(_ hex: UInt32, alpha: CGFloat = 1) -> NSColor {
        NSColor(srgbRed: CGFloat((hex >> 16) & 0xFF) / 255, green: CGFloat((hex >> 8) & 0xFF) / 255, blue: CGFloat(hex & 0xFF) / 255, alpha: alpha)
    }

    /// qh's terminal colours re-mapped to the design: its cyan accent becomes signal orange, team magenta a deep blue.
    static func paint(_ v: TerminalView) {
        let dark = isDark
        let ansi: [UInt32] = dark
            ? [0x0E0E0D, 0xFF6B5E, 0x8FD18A, 0xE6B450, 0x7C83FF, 0x7C83FF, 0xFF5A2A, 0xC4C3BC,
               0x6F6E68, 0xFF8A7F, 0xA9E0A4, 0xF0C674, 0x9AA0FF, 0x9AA0FF, 0xFF7A52, 0xFFFFFF]
            : [0x0E0E0D, 0xC4342B, 0x2F7D3A, 0x8A5D00, 0x373FF5, 0x373FF5, 0xE8400F, 0x3D3D39,
               0x9A9A94, 0xD9473D, 0x3A9A48, 0xA06C00, 0x5A61FF, 0x5A61FF, 0xFF5A2A, 0x0E0E0D]
        v.installColors(ansi.map { h in
            SwiftTerm.Color(red: UInt16((h >> 16) & 0xFF) * 257, green: UInt16((h >> 8) & 0xFF) * 257, blue: UInt16(h & 0xFF) * 257)
        })
        v.nativeBackgroundColor = ns(dark ? 0x0D0D0C : 0xF6F6F3)
        v.nativeForegroundColor = ns(dark ? 0xECEBE6 : 0x0E0E0D)
        v.caretColor = ns(dark ? 0xFF5A2A : 0xFF4D1C)
        v.selectedTextBackgroundColor = ns(dark ? 0xFF5A2A : 0xFF4D1C, alpha: 0.28)
    }
}

/// The blinking orange cursor of the qh mark. The blink is a Core Animation layer animation, so
/// it runs in the window server: the app does no work per blink (a SwiftUI timer redrew the view
/// twice a second and kept the app at a few % CPU while idle).
struct Cursor: NSViewRepresentable {
    var width: CGFloat
    var height: CGFloat

    func makeNSView(context: Context) -> BlinkView { BlinkView() }
    func updateNSView(_ v: BlinkView, context: Context) {}
    func sizeThatFits(_ p: ProposedViewSize, nsView: BlinkView, context: Context) -> CGSize? { CGSize(width: width, height: height) }

    final class BlinkView: NSView {
        override init(frame: NSRect) {
            super.init(frame: frame)
            wantsLayer = true
            let blink = CAKeyframeAnimation(keyPath: "opacity")
            blink.values = [1, 1, 0, 0]
            blink.keyTimes = [0, 0.5, 0.5, 1]
            blink.duration = 1.1
            blink.repeatCount = .infinity
            layer?.add(blink, forKey: "blink")
        }
        required init?(coder: NSCoder) { nil }
        override func updateLayer() {
            let dark = effectiveAppearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
            layer?.backgroundColor = Theme.ns(dark ? 0xFF5A2A : 0xFF4D1C).cgColor
        }
        override var wantsUpdateLayer: Bool { true }
        override func viewDidChangeEffectiveAppearance() { needsDisplay = true }
    }
}

/// Pointing-hand cursor over anything clickable.
struct Hand: ViewModifier {
    func body(content: Content) -> some View {
        content.onContinuousHover { phase in
            switch phase {
            case .active: NSCursor.pointingHand.set()
            case .ended: NSCursor.arrow.set()
            }
        }
    }
}

extension View {
    func hand() -> some View { modifier(Hand()) }
}

/// `.plain` with the pointing hand.
struct HandButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label.opacity(configuration.isPressed ? 0.6 : 1).hand()
    }
}

extension ButtonStyle where Self == HandButtonStyle {
    static var hand: HandButtonStyle { HandButtonStyle() }
}
