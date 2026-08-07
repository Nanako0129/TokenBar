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
    /// Interaction turns for this day, keyed by exact client id. Rust counts
    /// these in the same fold that produces the day's tokens and messages, so a
    /// day/month view no longer runs a second full report just for turns.
    /// Absent on a payload from an engine that predates the field, and absent
    /// for a day with no recorded turns — both decode as an empty map.
    public let turnsByClient: [String: Int64]?

    /// Turns for `clients`, summed. Empty selection yields nil rather than 0 so
    /// a caller can tell "no clients selected" from "selected clients had none",
    /// matching how the lenses treat an all-hidden slice everywhere else.
    public func turns(for clients: [String]) -> Int64? {
        guard !clients.isEmpty else { return nil }
        let counts = turnsByClient ?? [:]
        return clients.reduce(Int64(0)) { $0.saturatingAdding(counts[$1] ?? 0) }
    }
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
    /// Today's visible client with the most tokens, or nil when today has no
    /// visible stripe. Client **id** only — display labels and the outbound
    /// allowlist belong to the app layer, not to this cross-platform surface.
    ///
    /// Windows port obligation: TokenBar.Core is a file-by-file port of this
    /// module (docs/knowledge/verification.md "Cross-port fixture cross-check"),
    /// so this field must be mirrored there on the next sync. The four figures
    /// above are unchanged, so the existing cross-check cases are unaffected.
    public let todayTopClient: String?

    public init(
        todayTokens: Int64, todayCost: Double, totalTokens: Int64, totalCost: Double,
        todayTopClient: String?
    ) {
        self.todayTokens = todayTokens
        self.todayCost = todayCost
        self.totalTokens = totalTokens
        self.totalCost = totalCost
        self.todayTopClient = todayTopClient
    }
}

extension UsagePayload {
    /// The one membership test both stages use, so the published top client can
    /// never disagree with the figures beside it.
    static func survives(_ id: String, hidden: Set<String>, only: Set<String>?) -> Bool {
        (only?.contains(id) ?? true) && !hidden.contains(id)
    }

    /// Fold per-client×model×provider stripes into per-client totals and return
    /// the id with the most tokens. `hidden` uses the same membership test as
    /// `trayTotals`, with no normalization, so both agree by construction.
    ///
    /// Two-stage on purpose: taking the max over raw stripes would pick the
    /// largest single model stripe, not the busiest client — a client that
    /// spreads its usage across several models would lose to a smaller
    /// single-model one. This id is displayed publicly, so picking the wrong one
    /// ships wrong data.
    ///
    /// Deterministic tie-break (a wobbling label would flip between publishes):
    /// tokens, then higher cost, then lexicographically smallest id. The fold
    /// walks the keys in sorted order because `Dictionary`'s own iteration order
    /// is unspecified (and per-process seeded).
    /// `only`, when non-nil, is a POSITIVE allowlist: a stripe survives when
    /// `id ∈ only && id ∉ hidden`. Both conditions must hold, which is what
    /// makes "a selection cannot defeat hiding" structural rather than guarded.
    ///
    /// Positive rather than a complement, and that is not a style choice. The
    /// obvious `hidden ∪ (allIds − {selected})` is wrong: the aggregator emits
    /// ids that are not in `allIds` — `cc-mirror/<name>`, and any agent
    /// detected before the registry catches up — so a complement cannot
    /// subtract what it does not know about and those ids survive the filter.
    /// The published figure would be "the selected agent plus every
    /// unregistered client".
    public static func topVisibleClient(
        in clients: [ContributionClient], hidden: Set<String>, only: Set<String>? = nil
    ) -> String? {
        var byClient: [String: (tokens: Int64, cost: Double)] = [:]
        for cc in clients where survives(cc.client, hidden: hidden, only: only) {
            let prev = byClient[cc.client] ?? (0, 0)
            byClient[cc.client] = (prev.tokens.saturatingAdding(cc.tokens.total), prev.cost + cc.cost)
        }
        var best: (id: String, tokens: Int64, cost: Double)?
        for id in byClient.keys.sorted() {
            let entry = byClient[id]!
            guard let current = best else {
                best = (id, entry.tokens, entry.cost)
                continue
            }
            // Strict `>` only: on a full tie the earlier (lexicographically
            // smaller) id already in `best` wins.
            if entry.tokens > current.tokens
                || (entry.tokens == current.tokens && entry.cost > current.cost) {
                best = (id, entry.tokens, entry.cost)
            }
        }
        return best?.id
    }

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
    public func trayTotals(
        hidden: Set<String>, today: String, only: Set<String>? = nil
    ) -> TrayTotals {
        let todayEntry = contributions.last(where: { $0.date == today })
        // Shared by both paths so the published top client can never disagree
        // with the figures next to it (same stripes, same membership test).
        let todayTopClient = UsagePayload.topVisibleClient(
            in: todayEntry?.clients ?? [], hidden: hidden, only: only)
        // `only == nil` as well as an empty hidden set: the fast path reads
        // `summary`, which is unfiltered, so a positive filter must never take
        // it. Callers that pass no `only` keep the byte-identical fast path.
        if hidden.isEmpty && only == nil {
            return TrayTotals(
                todayTokens: todayEntry?.totals.tokens ?? 0,
                todayCost: todayEntry?.totals.cost ?? 0,
                totalTokens: summary.totalTokens,
                totalCost: summary.totalCost,
                todayTopClient: todayTopClient)
        }
        var totalTokens: Int64 = 0
        var totalCost = 0.0
        var todayTokens: Int64 = 0
        var todayCost = 0.0
        for c in contributions {
            let isToday = c.date == today
            for cc in c.clients where UsagePayload.survives(cc.client, hidden: hidden, only: only) {
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
            totalTokens: totalTokens, totalCost: totalCost,
            todayTopClient: todayTopClient)
    }
}
