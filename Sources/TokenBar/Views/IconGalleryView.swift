import SwiftUI
import TokenBarCore

/// Debug-only grid of every registered client's icon, rendered through the
/// real AgentIconView / NSImage(contentsOf:) path (the one that caught the
/// Antigravity CoreSVG mask+blur bug) rather than a browser mockup. Not
/// wired to any real usage data — `--icon-gallery` opens it standalone so
/// the whole roster can be screenshotted without needing local session
/// fixtures for every client.
struct IconGalleryView: View {
    private static let ids = [
        "claude", "gemini", "opencode", "copilot", "qwen",
        "codex", "droid", "kilocode", "kilo", "synthetic", "kiro", "codebuff",
        "cursor", "warp", "amp", "pi", "kimi",
        "cline", "jcode", "micode", "gjc",
        "antigravity", "antigravity-cli",
        "hermes", "roocode", "mux", "crush", "goose", "zed", "trae", "openclaw",
    ]

    private let columns = [GridItem(.adaptive(minimum: 110), spacing: 16)]

    var body: some View {
        ScrollView {
            LazyVGrid(columns: columns, spacing: 20) {
                ForEach(Self.ids, id: \.self) { id in
                    VStack(spacing: 6) {
                        AgentIconView(clientId: id, size: 48)
                        AgentIconView(clientId: id, size: 14)
                        Text(id)
                            .font(.caption2.monospaced())
                            .lineLimit(1)
                        Text(ClientRegistry.style(id).displayName)
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                    }
                    .frame(width: 110)
                }
            }
            .padding(20)
        }
        .frame(width: 900, height: 820)
        .background(PopoverBackdrop().ignoresSafeArea())
    }
}
