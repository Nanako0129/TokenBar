import AppKit
import SwiftUI

// The build floor, stated where it actually binds rather than only in prose.
//
// `if #available(macOS 26.0, *)` below guards Liquid Glass at RUNTIME. The
// symbols inside it still have to exist at COMPILE time, and they ship in the
// macOS 26 SDK, so an older Xcode cannot type-check this file no matter what
// the availability check says. Swift 6.2 is the compiler that carries that SDK.
//
// Without this, an older toolchain produces a wall of "cannot find
// 'GlassEffectContainer' in scope" — errors that name the symbol and not the
// cause, and that invite a fix which compiles the Liquid Glass surfaces out
// (#343). That fix builds and runs; it just silently ships an app without a
// feature the app has a preference for. Failing here instead makes the floor
// impossible to meet by accident and impossible to miss.
//
// Deliberately not `swift-tools-version: 6.2` in Package.swift, which would
// also refuse an older toolchain: that setting additionally moves language-mode
// and manifest defaults, which is a larger change than stating a requirement.
#if !compiler(>=6.2)
#error("Syrtis requires Swift 6.2 or newer (Xcode 26+). Liquid Glass is compiled against the macOS 26 SDK; an older toolchain cannot build this file, and removing those surfaces to make it compile would ship the app without them.")
#endif

/// Reusable backdrop for popover/panel content. Uses Liquid Glass on
/// macOS 26+ and falls back to an `NSVisualEffectView` (.popover material,
/// behindWindow) on older systems. Apply with `.background(GlassBackground())`.
///
/// Layering note: NSPopover already draws its own vibrant chrome behind the
/// content view. We deliberately apply the material to the *content* (this
/// view) rather than trying to strip the popover's frame view — a clear glass
/// layer over the system chrome reads as one surface, while hacking the
/// popover's private background view is fragile across OS releases.
struct GlassBackground: View {
    var cornerRadius: CGFloat = 0

    var body: some View {
        if #available(macOS 26.0, *) {
            GlassEffectContainer {
                Rectangle()
                    .fill(.clear)
                    .glassEffect(.regular, in: .rect(cornerRadius: cornerRadius))
            }
        } else {
            VisualEffectBackground(material: .popover)
                .clipShape(RoundedRectangle(cornerRadius: cornerRadius))
        }
    }
}

/// Popover root backdrop: HUD-grade translucency sampling what's behind the
/// window (the .popover chrome reads nearly opaque in dark mode, burying the
/// wallpaper blur that makes Liquid Glass cards come alive).
struct PopoverBackdrop: View {
    @Environment(\.inGlassPanel) private var inGlassPanel

    var body: some View {
        if inGlassPanel {
            Color.clear // the panel's own .regular glass is the backdrop
        } else {
            VisualEffectBackground(material: .hudWindow)
        }
    }
}

/// AppKit visual-effect bridge.
struct VisualEffectBackground: NSViewRepresentable {
    let material: NSVisualEffectView.Material

    func makeNSView(context: Context) -> NSVisualEffectView {
        let view = NSVisualEffectView()
        view.material = material
        view.blendingMode = .behindWindow
        view.state = .active
        return view
    }

    func updateNSView(_ view: NSVisualEffectView, context: Context) {
        view.material = material
    }
}
