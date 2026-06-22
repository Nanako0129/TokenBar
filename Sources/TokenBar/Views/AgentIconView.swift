import AppKit
import SwiftUI
import TokenBarCore

/// Brand-icon disc for an agent, port of the clients.ts iconRaw/iconType
/// registry (the SVGs ship as bundle resources, rendered via NSImage's
/// native SVG support — the codexbar approach). 'mono' glyphs tint white
/// over the brand-color disc; 'full' icons carry their own design and fill
/// the disc as-is; agents without an icon keep the initial-letter disc.
struct AgentIconView: View {
    let clientId: String
    var size: CGFloat = 14

    private static let monoIds: Set<String> = [
        "claude", "gemini", "opencode", "copilot", "cursor", "amp", "pi",
        "kimi", "qwen", "warp",
    ]
    private static let fullIds: Set<String> = [
        "codex", "droid", "kilocode", "kilo", "synthetic", "codebuff",
        "antigravity", "kiro",
        // Official brand icons for the newer local clients (png/svg).
        "cline", "jcode", "micode", "gjc",
    ]

    /// Clients that share another client's brand icon. The Antigravity CLI is
    /// the same product family as the Antigravity IDE and uses its logo.
    private static let iconAliases: [String: String] = [
        "antigravity-cli": "antigravity",
    ]

    /// Full icons whose mark is dark and mostly transparent, so they need a
    /// light disc behind to stay visible on a dark popover.
    private static let lightBackedIds: Set<String> = ["cline"]

    /// Resolve the id whose `agent-icons/<id>.svg` should render for a client.
    private static func iconId(_ clientId: String) -> String {
        iconAliases[clientId] ?? clientId
    }

    @MainActor private static var cache: [String: NSImage] = [:]

    @MainActor private static func image(_ id: String) -> NSImage? {
        if let cached = cache[id] { return cached }
        guard monoIds.contains(id) || fullIds.contains(id) else { return nil }
        let bundle = Bundle.tokenBarResources
        // SVG (vector) preferred; some brand icons ship only as PNG.
        guard let url = bundle.url(
                forResource: id, withExtension: "svg", subdirectory: "agent-icons")
                ?? bundle.url(
                    forResource: id, withExtension: "png", subdirectory: "agent-icons"),
              let image = NSImage(contentsOf: url)
        else { return nil }
        cache[id] = image
        return image
    }

    var body: some View {
        let style = ClientRegistry.style(clientId)
        let iconId = Self.iconId(clientId)
        ZStack {
            if Self.fullIds.contains(iconId), let image = Self.image(iconId) {
                ZStack {
                    // Dark, mostly-transparent marks (e.g. Cline's robot) are
                    // invisible on a dark popover; back them with a light disc.
                    if Self.lightBackedIds.contains(iconId) {
                        Circle().fill(.white)
                    }
                    Image(nsImage: image)
                        .resizable()
                        .scaledToFit()
                        .clipShape(Circle())
                }
            } else {
                Circle().fill(Color(hex: style.color))
                if Self.monoIds.contains(iconId), let image = Self.image(iconId) {
                    Image(nsImage: image)
                        .renderingMode(.template)
                        .resizable()
                        .scaledToFit()
                        .frame(width: size * 0.64, height: size * 0.64)
                        .foregroundStyle(.white)
                } else {
                    Text(String(style.displayName.prefix(1)).uppercased())
                        .font(.system(size: size * 0.55, weight: .bold))
                        .foregroundStyle(.white)
                }
            }
        }
        .frame(width: size, height: size)
    }
}
