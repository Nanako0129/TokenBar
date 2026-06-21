import Foundation

// Client (agent) display registry, ported from the Tauri app's
// src/lib/clients.ts. SVG icons are not ported yet (later phase); this carries
// the display name + brand disc color used by chart legends and model rows.

public struct ClientStyle: Sendable {
    public let id: String
    public let displayName: String
    /// Brand disc color, hex.
    public let color: String
}

public enum ClientRegistry {
    static let entries: [String: (displayName: String, color: String)] = [
        "claude": ("Claude Code", "#d97706"),
        "openclaw": ("OpenClaw", "#dc2626"),
        "gemini": ("Gemini CLI", "#60a5fa"),
        "opencode": ("OpenCode", "#1f2937"),
        "codex": ("Codex CLI", "#9ca3af"),
        "copilot": ("Copilot CLI", "#1f2937"),
        "cursor": ("Cursor IDE", "#0ea5e9"),
        "amp": ("Amp", "#10b981"),
        "droid": ("Droid", "#22c55e"),
        "hermes": ("Hermes", "#a78bfa"),
        "pi": ("Pi", "#f472b6"),
        "kimi": ("Kimi CLI", "#fbbf24"),
        "qwen": ("Qwen CLI", "#7c3aed"),
        "roocode": ("Roo Code", "#ef4444"),
        "kilocode": ("KiloCode", "#f97316"),
        "kilo": ("Kilo CLI", "#f59e0b"),
        "mux": ("Mux", "#06b6d4"),
        "crush": ("Crush", "#ec4899"),
        "synthetic": ("Synthetic", "#64748b"),
        "goose": ("Goose", "#14b8a6"),
        "codebuff": ("Codebuff", "#8b5cf6"),
        "antigravity": ("Antigravity", "#3b82f6"),
        "zed": ("Zed", "#084fff"),
        "kiro": ("Kiro", "#9046ff"),
        "trae": ("Trae", "#ef4444"),
        "warp": ("Warp", "#01a4ff"),
        "cline": ("Cline", "#5b8def"),
        "antigravity-cli": ("Antigravity CLI", "#6366f1"),
        "jcode": ("Jcode", "#84cc16"),
        "micode": ("MiMo Code", "#fb923c"),
    ]

    public static func style(_ id: String) -> ClientStyle {
        if let entry = entries[id] {
            return ClientStyle(id: id, displayName: entry.displayName, color: entry.color)
        }
        // Fallback: title-case the id, neutral grey disc.
        let displayName = id.prefix(1).uppercased() + id.dropFirst()
        return ClientStyle(id: id, displayName: displayName, color: "#6b7280")
    }

    /// Display name with the trailing form-factor word dropped, as the chart
    /// legend does ("Claude Code" → "Claude").
    public static func shortName(_ id: String) -> String {
        let name = style(id).displayName
        let registeredNames = Set(entries.values.map { $0.displayName })
        for suffix in [" CLI", " Code", " IDE"] where name.hasSuffix(suffix) {
            let base = String(name.dropLast(suffix.count))
            // Don't collapse onto a base that is itself another client's full
            // name — e.g. "Antigravity CLI" must stay distinct from the IDE
            // client "Antigravity".
            if !registeredNames.contains(base) {
                return base
            }
        }
        return name
    }
}
