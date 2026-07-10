import Foundation

// Contribution-graph payload (`UsagePayload` in the Tauri frontend's
// src/lib/types.ts). Keys match the Rust serde camelCase serialization exactly.

extension Int64 {
    /// Addition that clamps to `Int64.max`/`.min` instead of trapping on
    /// overflow. The Rust side clamps corrupt Antigravity token varints to
    /// `Int64.max` (M6 #766) and saturates its fold arithmetic (#822/#823), so
    /// a legitimately-clamped lane can carry `Int64.max`; the Swift-side re-sums
    /// (TokenBreakdown.total, the tray totals) run on the always-on menu bar and
    /// must not crash the app on such data. Normal (non-overflowing) values are
    /// byte-identical to `+`.
    public func saturatingAdding(_ other: Int64) -> Int64 {
        let (result, overflow) = addingReportingOverflow(other)
        guard overflow else { return result }
        return other > 0 ? .max : .min
    }
}

public struct TokenBreakdown: Decodable, Sendable {
    public let input: Int64
    public let output: Int64
    public let cacheRead: Int64
    public let cacheWrite: Int64
    public let reasoning: Int64

    /// Sum of every token lane — the single definition shared by the tray
    /// totals, DayBars, and UsageStats aggregations. Saturating so an
    /// `Int64.max`-clamped corrupt lane can't trap the always-on menu bar
    /// (matches the Rust-side saturation; see `Int64.saturatingAdding`).
    public var total: Int64 {
        input
            .saturatingAdding(output)
            .saturatingAdding(cacheRead)
            .saturatingAdding(cacheWrite)
            .saturatingAdding(reasoning)
    }
}

public struct ContributionClient: Decodable, Sendable {
    public let client: String
    public let modelId: String
    public let providerId: String
    public let tokens: TokenBreakdown
    public let cost: Double
    public let messages: Int
}

public struct Contribution: Decodable, Sendable {
    public struct Totals: Decodable, Sendable {
        public let tokens: Int64
        public let cost: Double
        public let messages: Int
    }

    public let date: String
    public let totals: Totals
    public let intensity: Int
    public let tokenBreakdown: TokenBreakdown
    public let clients: [ContributionClient]
}

public struct DateRange: Decodable, Sendable {
    public let start: String
    public let end: String
}

public struct YearMeta: Decodable, Sendable {
    public let year: String
    public let totalTokens: Int64
    public let totalCost: Double
    public let range: DateRange
}

public struct UsagePayload: Decodable, Sendable {
    public struct Meta: Decodable, Sendable {
        public let generatedAt: String
        public let version: String
        public let dateRange: DateRange
    }

    public struct Summary: Decodable, Sendable {
        public let totalTokens: Int64
        public let totalCost: Double
        public let totalDays: Int
        public let activeDays: Int
        public let averagePerDay: Double
        public let maxCostInSingleDay: Double
        public let clients: [String]
        public let models: [String]
    }

    public let meta: Meta
    public let summary: Summary
    public let years: [YearMeta]
    public let contributions: [Contribution]
}

/// Today/total token+cost figures for the menu-bar title, with the user's
/// hidden clients excluded.
public struct TrayTotals: Sendable {
    public let todayTokens: Int64
    public let todayCost: Double
    public let totalTokens: Int64
    public let totalCost: Double

    public init(todayTokens: Int64, todayCost: Double, totalTokens: Int64, totalCost: Double) {
        self.todayTokens = todayTokens
        self.todayCost = todayCost
        self.totalTokens = totalTokens
        self.totalCost = totalCost
    }
}

extension UsagePayload {
    /// The four tray-title figures with `hidden` client ids excluded. `today`
    /// is the local-timezone `YYYY-MM-DD` day key (tokscale-core's bucketing).
    ///
    /// An empty hidden set takes a fast path that reads `summary` and the
    /// today contribution totals directly, so the numbers are byte-identical
    /// to the pre-hide implementation (regression guard). With any client
    /// hidden, the figures are re-summed from the surviving per-client stripes.
    ///
    /// Clamp-granularity caveat: vendor/tokscale-core's aggregator clamps a
    /// day's `totals.tokens` with `.max(0)` at the aggregate level, while each
    /// per-client stripe lane clamps independently. With pathological negative
    /// token deltas the re-summed slow path can therefore differ slightly from
    /// `summary` — the day-level clamp is not reproducible from the stripes
    /// alone, so we do NOT try to. Smoke's `trayDrift` probe compares the two
    /// on real data every run to catch any vendor-sync regression.
    public func trayTotals(hidden: Set<String>, today: String) -> TrayTotals {
        if hidden.isEmpty {
            let todayEntry = contributions.last(where: { $0.date == today })
            return TrayTotals(
                todayTokens: todayEntry?.totals.tokens ?? 0,
                todayCost: todayEntry?.totals.cost ?? 0,
                totalTokens: summary.totalTokens,
                totalCost: summary.totalCost)
        }
        var totalTokens: Int64 = 0
        var totalCost = 0.0
        var todayTokens: Int64 = 0
        var todayCost = 0.0
        for c in contributions {
            let isToday = c.date == today
            for cc in c.clients where !hidden.contains(cc.client) {
                let sum = cc.tokens.total
                totalTokens = totalTokens.saturatingAdding(sum)
                totalCost += cc.cost
                if isToday {
                    todayTokens = todayTokens.saturatingAdding(sum)
                    todayCost += cc.cost
                }
            }
        }
        return TrayTotals(
            todayTokens: todayTokens, todayCost: todayCost,
            totalTokens: totalTokens, totalCost: totalCost)
    }
}
