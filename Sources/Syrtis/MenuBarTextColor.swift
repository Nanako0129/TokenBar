import AppKit
import SwiftUI

/// Shared by the live status items and Settings previews. Only opaque sRGB
/// values are persisted; automatic mode preserves each item's native policy.
enum MenuBarTextColor: String, CaseIterable {
    case automatic, custom

    static let storageKey = "tokenbar.tray.textColor.mode"
    static let customColorKey = "tokenbar.tray.textColor.hex"
    static let defaultHex = "#21C55E"
    static let warningColorKey = "tokenbar.tray.textColor.warning.hex"
    static let criticalColorKey = "tokenbar.tray.textColor.critical.hex"

    static let presets: [(name: String, hex: String)] = [
        ("Black", "#000000"), ("Dark gray", "#52525B"),
        ("Light gray", "#A1A1AA"), ("White", "#FFFFFF"),
        ("Red", "#EF4444"), ("Orange", "#F97316"),
        ("Amber", "#F59E0B"), ("Yellow", "#FACC15"),
        ("Lime", "#84CC16"), ("Green", "#21C55E"),
        ("Emerald", "#10B981"), ("Cyan", "#06B6D4"),
        ("Blue", "#3B82F6"), ("Indigo", "#6366F1"),
        ("Purple", "#A855F7"), ("Pink", "#EC4899"),
    ]

    var label: String {
        switch self {
        case .automatic: "Automatic"
        case .custom: "Custom"
        }
    }

    static func resolve(
        automatic: NSColor?, quotaRemaining: Double? = nil, defaults: UserDefaults = .standard
    ) -> NSColor? {
        let level = quotaRemaining.map(QuotaColorLevel.init(remaining:)) ?? .normal
        return resolve(automatic: automatic,
            modeRaw: defaults.string(forKey: storageKey) ?? MenuBarTextColor.automatic.rawValue,
            hex: defaults.string(forKey: level.customColorKey) ?? level.defaultHex)
    }

    static func resolve(automatic: NSColor?, modeRaw: String, hex: String) -> NSColor? {
        guard modeRaw == custom.rawValue else { return automatic }
        return color(hex: hex) ?? automatic
    }

    static func color(hex: String) -> NSColor? {
        guard let normalized = normalizedHex(hex) else { return nil }
        return NSColor(Color(hex: normalized))
    }

    /// The existing Color(hex:) consumes trusted palette values; validate
    /// editable text before passing it through that shared renderer.
    static func normalizedHex(_ input: String) -> String? {
        let trimmed = input.trimmingCharacters(in: .whitespacesAndNewlines)
        let digits = trimmed.hasPrefix("#") ? String(trimmed.dropFirst()) : trimmed
        guard digits.utf8.count == 6,
              digits.utf8.allSatisfy({ (48...57).contains($0) || (65...70).contains($0) || (97...102).contains($0) })
        else { return nil }
        return "#" + digits.uppercased()
    }

    static func hex(color: NSColor) -> String? {
        guard let rgb = color.usingColorSpace(.sRGB) else { return nil }
        let components = [rgb.redComponent, rgb.greenComponent, rgb.blueComponent]
        guard components.allSatisfy(\.isFinite) else { return nil }
        let bytes = components.map { Int((min(1, max(0, $0)) * 255).rounded()) }
        return String(format: "#%02X%02X%02X", bytes[0], bytes[1], bytes[2])
    }
}

extension QuotaColorLevel {
    var customColorKey: String {
        switch self {
        // Preserve the existing single-color preference as the normal color.
        case .normal: MenuBarTextColor.customColorKey
        case .warning: MenuBarTextColor.warningColorKey
        case .critical: MenuBarTextColor.criticalColorKey
        }
    }

    var defaultHex: String {
        switch self {
        case .normal: MenuBarTextColor.defaultHex
        case .warning: "#F59E0B"
        case .critical: "#EF4444"
        }
    }

    var label: String {
        switch self {
        case .normal: "Normal"
        case .warning: "Low"
        case .critical: "Very low"
        }
    }

    var hint: String {
        switch self {
        case .normal: "More than 25% remaining, other text, or unavailable quota"
        case .warning: "More than 10% and up to 25% remaining"
        case .critical: "10% or less remaining"
        }
    }
}
