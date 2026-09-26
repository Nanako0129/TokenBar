import SwiftUI
import AppKit

extension Color {
    /// Brand accent, resolved per appearance. Two values rather than one: the
    /// dark tone has too little contrast on a light surface and the light tone
    /// goes muddy on a dark one.
    ///
    /// Light is the naming essay's masthead value. Dark is the landing page's
    /// dark teal (#56B7A6). An earlier dark value pulled the teal down to 22%
    /// saturation (#6DA298) so it would not compete with the live-rate dot a
    /// few points to its right; on the glass header the user found that too
    /// faint to read (2026-09-26), so readability wins here. Re-check both
    /// against the screenshot wallpaper before shipping.
    static let syrtisAccent = Color(nsColor: NSColor(name: nil) { appearance in
        appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
            ? NSColor(srgbRed: 0x56 / 255.0, green: 0xB7 / 255.0, blue: 0xA6 / 255.0, alpha: 1)
            : NSColor(srgbRed: 0x1B / 255.0, green: 0x5E / 255.0, blue: 0x56 / 255.0, alpha: 1)
    })
}

extension Font {
    /// The wordmark face, mirroring the naming essay's stack.
    ///
    /// Resolved explicitly rather than handed to `Font.custom` as a list,
    /// because a missing face makes `Font.custom` fall back to the system
    /// *sans* without saying so — the serif would vanish and nothing would
    /// report it.
    static func syrtisWordmark(size: CGFloat) -> Font {
        for name in ["Iowan Old Style", "Palatino"] where NSFont(name: name, size: size) != nil {
            return .custom(name, size: size)
        }
        return .system(size: size, design: .serif)
    }
}

/// The Syrtis sediment mark as a flat monochrome glyph — the app-icon artwork
/// without the squircle, for in-app use (popover header, Settings sidebar).
/// Tinted by the current foreground style.
///
/// Backed by the vector `brandmark.pdf` rather than redrawn in `Canvas`: the
/// logo is a potrace vectorisation with several hundred segments, so a
/// hand-maintained second copy of it would drift from the shipped icon the
/// first time either is touched. One artwork, two consumers.
///
/// That artwork is the sediment disc alone. The watch star was dropped from the
/// mark, and with it the reason the disc sat left of centre — the offset existed
/// to balance the star's weight in the upper right. Without the star the disc is
/// centred on all four sides.
///
/// The image is loaded as a template so only its alpha is used and the tint
/// follows the surrounding style, the same way the old parametric mark took
/// `.secondary`. `Bundle.tokenBarResources` is the SwiftPM resource bundle, so
/// this resolves for both `swift run` and the assembled `.app`.
struct BrandMark: View {
    /// Loaded once: `NSImage` PDF rasterisation is not free and the mark is
    /// drawn on every popover layout pass.
    private static let image: NSImage? = {
        guard let url = Bundle.tokenBarResources.url(forResource: "brandmark", withExtension: "pdf"),
              let img = NSImage(contentsOf: url)
        else { return nil }
        img.isTemplate = true
        return img
    }()

    var body: some View {
        if let image = Self.image {
            Image(nsImage: image)
                .resizable()
                .renderingMode(.template)
                .aspectRatio(contentMode: .fit)
                .foregroundStyle(.secondary)
        } else {
            // An unbundled build with no resource bundle still lays out the
            // header correctly rather than collapsing the frame to zero.
            Color.clear
        }
    }
}
