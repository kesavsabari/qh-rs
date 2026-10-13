import AppKit
import CoreText
import SwiftUI
import SwiftTerm

/// The qh design: paper and ink, one signal orange, Geist. Light and dark follow the Mac.
enum Theme {
    static let paper = dyn(0xF6F6F3, 0x0D0D0C)
    static let side = dyn(0xEFEFEA, 0x121211)
    static let ink = dyn(0x0E0E0D, 0xECEBE6)
    static let soft = dyn(0x6B6B66, 0x9C9B94)
    static let faint = dyn(0x9A9A94, 0x6F6E68)
    static let rule = dyn(0xDCDCD5, 0x262624)
    static let hover = dyn(0xE6E6E0, 0x1C1C1A)
    static let accent = dyn(0xFF4D1C, 0xFF5A2A)

    static func sans(_ size: CGFloat, medium: Bool = false) -> Font { .custom(medium ? "Geist-Medium" : "Geist-Regular", size: size) }
    static func mono(_ size: CGFloat, medium: Bool = false) -> Font { .custom(medium ? "GeistMono-Medium" : "GeistMono-Regular", size: size) }
    static func monoNS(_ size: CGFloat) -> NSFont { NSFont(name: "GeistMono-Regular", size: size) ?? .monospacedSystemFont(ofSize: size, weight: .regular) }

    static var isDark: Bool { NSApplication.shared.effectiveAppearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua }

    /// The fonts ship inside the app.
    static func registerFonts() {
        for url in Bundle.main.urls(forResourcesWithExtension: "ttf", subdirectory: nil) ?? [] {
            CTFontManagerRegisterFontsForURL(url as CFURL, .process, nil)
        }
    }

    private static func dyn(_ light: UInt32, _ dark: UInt32) -> Color {
        Color(NSColor(name: nil) { ap in
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

/// The blinking orange cursor of the qh mark.
struct Cursor: View {
    var width: CGFloat
    var height: CGFloat
    var body: some View {
        TimelineView(.periodic(from: .now, by: 0.55)) { ctx in
            Rectangle()
                .fill(Theme.accent)
                .frame(width: width, height: height)
                .opacity(Int(ctx.date.timeIntervalSinceReferenceDate / 0.55) % 2 == 0 ? 1 : 0)
        }
    }
}
