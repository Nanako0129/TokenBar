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
        "claude", "gemini", "opencode", "copilot", "qwen",
    ]
    private static let fullIds: Set<String> = [
        "codex", "droid", "kilocode", "synthetic", "codebuff",
        "antigravity", "kiro", "cursor", "warp", "amp", "pi", "kimi",
        // Official brand icons for the newer local clients (png/svg).
        "cline", "jcode", "micode", "gjc", "grok",
        // Newly onboarded brand icons (png/svg).
        "hermes", "roocode", "mux", "crush", "goose", "zed", "trae", "openclaw",
    ]

    /// Clients that share another client's brand icon. The Antigravity CLI is
    /// the same product family as the Antigravity IDE and uses its logo; Kilo
    /// CLI and KiloCode are the same Kilo-Org brand.
    private static let iconAliases: [String: String] = [
        "antigravity-cli": "antigravity",
        "kilo": "kilocode",
    ]

    /// Full icons whose mark has no opaque background of its own (dark ink or
    /// light text on transparent), so they need a solid disc behind them to
    /// stay legible against the popover. Color chosen per-icon to match how
    /// the brand actually presents the mark (e.g. Cline/Hermes's marks are
    /// dark-on-light; Mux/Amp's marks are light-on-dark).
    private static let backgroundFills: [String: Color] = [
        "cline": .white,
        "hermes": .white,
        "mux": .black,
        "amp": .black,
    ]

    /// Full icons whose source art reaches the edges of its square canvas
    /// (antennae, ears, corners) and would be clipped by the circular mask at
    /// 100% scale — rendered slightly inset instead.
    private static let insetScale: [String: CGFloat] = [
        "cline": 0.82,
    ]

    /// Resolve the id whose `agent-icons/<id>.svg` should render for a client.
    private static func iconId(_ clientId: String) -> String {
        iconAliases[clientId] ?? clientId
    }

    /// The only IDs allowed to create an individual status item. Aliases share
    /// resources but remain separate client identities everywhere else.
    static var officialClientIDs: Set<String> {
        monoIds.union(fullIds).union(Set(iconAliases.keys))
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

    /// Returns only official IDs whose source asset really loads from the bundle.
    @MainActor static func availableOfficialClientIDs() -> Set<String> {
        officialClientIDs.filter { image(iconId($0)) != nil }
    }

    /// Render a static full-color status image once at each supported backing
    /// scale. The controller retains this image and does not re-render it.
    /// Returns nil for any client `availableOfficialClientIDs()` would exclude,
    /// so callers need no second eligibility check.
    @MainActor static func statusItemImage(
        clientId: String, size: CGFloat = 18
    ) -> NSImage? {
        guard availableOfficialClientIDs().contains(clientId) else { return nil }
        let logicalSize = NSSize(width: size, height: size)
        var representations: [NSBitmapImageRep] = []
        for scale in [CGFloat(1), CGFloat(2)] {
            let renderer = ImageRenderer(
                content: AgentIconView(clientId: clientId, size: size)
                    .frame(width: size, height: size))
            renderer.scale = scale
            renderer.proposedSize = ProposedViewSize(width: size, height: size)
            guard let cgImage = renderer.cgImage else { return nil }
            let representation = NSBitmapImageRep(cgImage: cgImage)
            representation.size = logicalSize
            representations.append(representation)
        }
        guard representations.count == 2 else { return nil }
        let result = NSImage(size: logicalSize)
        representations.forEach(result.addRepresentation)
        result.isTemplate = false
        result.accessibilityDescription = ClientRegistry.style(clientId).displayName
        return result
    }

    var body: some View {
        let style = ClientRegistry.style(clientId)
        let iconId = Self.iconId(clientId)
        ZStack {
            if Self.fullIds.contains(iconId), let image = Self.image(iconId) {
                ZStack {
                    // Marks with no opaque background of their own (dark ink
                    // or light text on transparent) are invisible on their
                    // own; back them with a solid disc in the brand's color.
                    if let fill = Self.backgroundFills[iconId] {
                        Circle().fill(fill)
                    }
                    let scale = Self.insetScale[iconId] ?? 1
                    Image(nsImage: image)
                        .resizable()
                        .scaledToFit()
                        .frame(width: size * scale, height: size * scale)
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
