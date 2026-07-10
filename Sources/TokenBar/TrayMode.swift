import AppKit
import Foundation
import TokenBarCore

/// What the status-item text shows next to the icon, mirroring TrayMode +
/// computeTrayTitle in the Tauri app's settings.ts.
enum TrayMode: String, CaseIterable {
    case todayTokens = "today_tokens"
    case todayCost = "today_cost"
    case totalTokens = "total_tokens"
    case totalCost = "total_cost"
    case tokensPerMin = "tokens_per_min"
    case quotaLeft = "quota_left"
    case hidden

    static let storageKey = "tokenbar.tray.mode"

    static var current: TrayMode {
        UserDefaults.standard.string(forKey: storageKey)
            .flatMap(TrayMode.init(rawValue:)) ?? .todayTokens
    }

    var label: String {
        switch self {
        case .todayTokens: return "Today's tokens (50M)"
        case .todayCost: return "Today's cost ($5.20)"
        case .totalTokens: return "Total tokens (1.5B)"
        case .totalCost: return "Total cost ($889)"
        case .tokensPerMin: return "Tokens / min (12.4K/m)"
        case .quotaLeft: return "Quota left (57%)"
        case .hidden: return "Icon only"
        }
    }

    /// The tray title for this mode ("" = icon only). `tokensPerMin` feeds
    /// the live-rate mode; `quotaRemaining` the quota mode.
    func title(graph: UsagePayload?, tokensPerMin: Double?, quotaRemaining: Double? = nil) -> String {
        if self == .quotaLeft {
            guard let quotaRemaining else { return "—%" }
            return "\(Int(min(100, max(0, quotaRemaining)).rounded()))%"
        }
        guard self != .hidden, let graph else { return "" }
        // Hidden clients must not appear in any tray figure. An empty hidden
        // set is byte-identical to the pre-hide summary/today totals.
        let totals = graph.trayTotals(
            hidden: ClientRegistry.hiddenClients(), today: Format.todayKey())
        switch self {
        case .todayTokens:
            return Format.compactTokens(totals.todayTokens)
        case .todayCost:
            return Format.usd(totals.todayCost)
        case .totalTokens:
            return Format.compactTokens(totals.totalTokens)
        case .totalCost:
            return Format.usd(totals.totalCost)
        case .tokensPerMin:
            guard let tokensPerMin else { return "—/m" }
            return "\(Format.compactTokens(Int64(max(0, tokensPerMin).rounded())))/m"
        case .quotaLeft, .hidden:
            return ""
        }
    }

    /// Gauge color for the quota title (nil = default label color).
    func titleColor(quotaRemaining: Double?) -> NSColor? {
        guard self == .quotaLeft, let quotaRemaining else { return nil }
        return TrayIcons.gaugeColor(remaining: quotaRemaining)
    }
}
