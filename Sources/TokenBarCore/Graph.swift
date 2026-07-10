import Foundation

// Contribution-graph payload (`UsagePayload` in the Tauri frontend's
// src/lib/types.ts). Keys match the Rust serde camelCase serialization exactly.

public struct TokenBreakdown: Decodable, Sendable {
    public let input: Int64
    public let output: Int64
    public let cacheRead: Int64
    public let cacheWrite: Int64
    public let reasoning: Int64
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
                let t = cc.tokens
                let sum = t.input + t.output + t.cacheRead + t.cacheWrite + t.reasoning
                totalTokens += sum
                totalCost += cc.cost
                if isToday {
                    todayTokens += sum
                    todayCost += cc.cost
                }
            }
        }
        return TrayTotals(
            todayTokens: todayTokens, todayCost: todayCost,
            totalTokens: totalTokens, totalCost: totalCost)
    }
}
