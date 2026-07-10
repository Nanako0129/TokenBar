import Foundation

// Dashboard stats derived from a UsagePayload, ported from the Tauri app's
// src/lib/stats.ts + streaks.ts. Day keys are local-timezone `YYYY-MM-DD`
// strings (tokscale-core's bucketing); day arithmetic uses a civil-date day
// number so no Calendar/timezone is involved.

public struct ISODay: Equatable, Sendable {
    /// Days since 1970-01-01 (Howard Hinnant's days_from_civil).
    public let number: Int

    public init?(_ iso: String) {
        let parts = iso.split(separator: "-").compactMap { Int($0) }
        guard parts.count == 3 else { return nil }
        let (y, m, d) = (parts[0], parts[1], parts[2])
        let yy = m <= 2 ? y - 1 : y
        let era = (yy >= 0 ? yy : yy - 399) / 400
        let yoe = yy - era * 400
        let doy = (153 * (m + (m > 2 ? -3 : 9)) + 2) / 5 + d - 1
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy
        number = era * 146097 + doe - 719468
    }

    public init(number: Int) { self.number = number }

    /// `YYYY-MM-DD` (civil_from_days inverse).
    public var iso: String {
        let z = number + 719468
        let era = (z >= 0 ? z : z - 146096) / 146097
        let doe = z - era * 146097
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100)
        let mp = (5 * doy + 2) / 153
        let d = doy - (153 * mp + 2) / 5 + 1
        let m = mp + (mp < 10 ? 3 : -9)
        let y = yoe + era * 400 + (m <= 2 ? 1 : 0)
        return String(format: "%04d-%02d-%02d", y, m, d)
    }
}

public struct PerDay: Sendable {
    public let date: String
    public let tokens: Int64
    public let cost: Double
    public let intensity: Int

    public init(date: String, tokens: Int64, cost: Double, intensity: Int) {
        self.date = date
        self.tokens = tokens
        self.cost = cost
        self.intensity = intensity
    }
}

public struct Streaks: Sendable {
    public let longest: Int
    public let current: Int
}

public struct UsageStats: Sendable {
    public let totalTokens: Int64
    public let totalCost: Double
    public let activeDays: Int
    public let bestDay: (date: String, cost: Double)?
    public let averagePerDay: Double
    public let dateRange: DateRange
    public let perDay: [PerDay]
    public let perDayMap: [String: PerDay]
    public let streaks: Streaks
    public let presentClients: [String]
    public let maxTokens: Int64

    /// Port of computeStats: aggregate per-day totals over `selectedClients`
    /// (empty set = nothing selected; pass all present clients for the
    /// all-agent view).
    public init(payload: UsagePayload, selectedClients: Set<String>) {
        var perDay: [PerDay] = []
        var perDayMap: [String: PerDay] = [:]
        var present = Set<String>()
        var totalTokens: Int64 = 0
        var totalCost = 0.0
        var bestDay: (date: String, cost: Double)?
        var maxTokens: Int64 = 0

        for c in payload.contributions {
            var dayTokens: Int64 = 0
            var dayCost = 0.0
            for cc in c.clients {
                present.insert(cc.client)
                guard selectedClients.contains(cc.client) else { continue }
                // Saturating: a corrupt Antigravity stripe can be Int64.max
                // (Rust-side #766 clamp), and the filtered Overview/Stats path
                // folds these here — a trapping `+=` would crash the dashboard.
                dayTokens = dayTokens.saturatingAdding(cc.tokens.total)
                dayCost += cc.cost
            }
            if dayTokens == 0 && dayCost == 0 { continue }
            let entry = PerDay(date: c.date, tokens: dayTokens, cost: dayCost, intensity: c.intensity)
            perDay.append(entry)
            perDayMap[c.date] = entry
            totalTokens = totalTokens.saturatingAdding(dayTokens)
            totalCost += dayCost
            if dayTokens > maxTokens { maxTokens = dayTokens }
            if bestDay == nil || dayCost > bestDay!.cost { bestDay = (c.date, dayCost) }
        }

        self.totalTokens = totalTokens
        self.totalCost = totalCost
        activeDays = perDay.count
        self.bestDay = bestDay
        averagePerDay = perDay.isEmpty ? 0 : totalCost / Double(perDay.count)
        // `payload.meta.dateRange` is the activity span across ALL clients in the
        // payload (aggregator min/max contribution date). When some present
        // client is filtered out (hidden), copying that unfiltered range lets a
        // hidden client whose activity extends past the visible clients' last
        // active day inject trailing empty days that reset/shorten the streaks.
        // Derive the effective range from the SELECTED clients' own activity in
        // that case, matching a payload in which the hidden clients never
        // existed. The all-present (nothing hidden) and empty (nothing active)
        // cases keep `meta.dateRange` — byte-identical to before.
        let filtering = !present.isSubset(of: selectedClients)
        let selectedDates = perDay.map(\.date)
        let effectiveRange: DateRange
        if filtering, let start = selectedDates.min(), let end = selectedDates.max() {
            effectiveRange = DateRange(start: start, end: end)
        } else {
            effectiveRange = payload.meta.dateRange
        }
        dateRange = effectiveRange
        self.perDay = perDay
        self.perDayMap = perDayMap
        streaks = Streaks.compute(
            perDayMap: perDayMap, rangeStart: effectiveRange.start, rangeEnd: effectiveRange.end)
        presentClients = present.sorted()
        self.maxTokens = maxTokens
    }
}

extension UsageStats {
    /// The set of `YYYY` years in which at least one NON-hidden client had
    /// activity (tokens or cost), derived from a payload's contributions. Used
    /// to drop from the year picker years that only hidden clients used. Only
    /// meaningful over an all-time payload (contributions spanning every year);
    /// callers fall back to the unfiltered known-year list when the payload is
    /// year-scoped.
    public static func yearsWithVisibleActivity(
        contributions: [Contribution], hidden: Set<String>
    ) -> Set<String> {
        var years = Set<String>()
        for c in contributions {
            let year = String(c.date.prefix(4))
            if years.contains(year) { continue }
            if c.clients.contains(where: {
                !hidden.contains($0.client) && ($0.tokens.total > 0 || $0.cost > 0)
            }) {
                years.insert(year)
            }
        }
        return years
    }
}

extension Streaks {
    /// Port of computeStreaks: walk every calendar day in the range; a day is
    /// active when it has tokens. Current counts back from the range end.
    public static func compute(
        perDayMap: [String: PerDay], rangeStart: String, rangeEnd: String
    ) -> Streaks {
        guard let start = ISODay(rangeStart), let end = ISODay(rangeEnd),
              end.number >= start.number
        else { return Streaks(longest: 0, current: 0) }

        var longest = 0
        var run = 0
        var current = 0
        for n in start.number...end.number {
            let active = (perDayMap[ISODay(number: n).iso]?.tokens ?? 0) > 0
            if active {
                run += 1
                longest = max(longest, run)
            } else {
                run = 0
            }
        }
        for n in stride(from: end.number, through: start.number, by: -1) {
            if (perDayMap[ISODay(number: n).iso]?.tokens ?? 0) > 0 { current += 1 } else { break }
        }
        return Streaks(longest: longest, current: current)
    }
}
