import Foundation

// Stacked-bar series for the Token Usage chart, ported from the grouping logic
// in the Tauri app's src/components/UsageBarGraph2D.tsx. UI-free so the
// builder is unit-testable; the SwiftUI chart just renders the result.

public enum StackBy: String, Sendable, CaseIterable {
    case model
    case agent
}

/// Whether bar length encodes token count or spend.
public enum ChartMetric: String, Sendable, CaseIterable {
    case tokens
    case cost
}

public struct DaySegment: Sendable {
    public let key: String
    public let label: String
    /// Hex color (provider shade for model stacking, brand color for agent).
    public let color: String
    public var tokens: Int64
    public var cost: Double
}

public struct DayBar: Sendable {
    public let date: String
    public var totalTokens: Int64 { segments.reduce(0) { $0.saturatingAdding($1.tokens) } }
    public var totalCost: Double { segments.reduce(0) { $0 + $1.cost } }
    public var segments: [DaySegment]
    public var isEmpty: Bool { segments.isEmpty }
}

public enum DayBars {
    public static let window = 30

    /// Build the trailing `window`-day series ending at `rangeEnd` (today, via
    /// `endFallback`, when absent). Days outside the data render as empty bars.
    ///
    /// `rangeEnd` must be the SELECTED clients' range end (`stats.dateRange.end`,
    /// selection-derived), NOT the unfiltered `payload.meta.dateRange.end`: a
    /// hidden client whose activity extends past the visible clients' last day
    /// would otherwise shift the trailing window forward and push visible
    /// activity off the chart while the range-filtered headline stats disagree.
    /// When nothing is hidden the two are equal, so the window is unchanged.
    public static func build(
        payload: UsagePayload,
        clientIds: [String],
        stackBy: StackBy,
        colors: ModelColorMap,
        rangeEnd: String,
        endFallback: String
    ) -> [DayBar] {
        let allowed = Set(clientIds)
        var byDate: [String: DayBar] = [:]
        for contribution in payload.contributions {
            let day = dayBar(contribution, allowed: allowed, stackBy: stackBy, colors: colors)
            if day.totalTokens > 0 || day.totalCost > 0 { byDate[day.date] = day }
        }

        let end = rangeEnd.isEmpty ? endFallback : rangeEnd
        guard let endDay = ISODay(end) else { return [] }
        return (0..<window).map { i in
            let date = ISODay(number: endDay.number - (window - 1) + i).iso
            return byDate[date] ?? DayBar(date: date, segments: [])
        }
    }

    static func dayBar(
        _ contribution: Contribution, allowed: Set<String>, stackBy: StackBy,
        colors: ModelColorMap
    ) -> DayBar {
        // Group each day either by model (tokscale-style provider shades) or
        // by agent/client (brand colors). Color + label follow the mode.
        var grouped: [String: DaySegment] = [:]
        for client in contribution.clients {
            guard allowed.contains(client.client) else { continue }
            let tokens = client.tokens.total
            if tokens <= 0 && client.cost <= 0 { continue }
            let model = client.modelId.isEmpty ? "unknown" : client.modelId
            let key = stackBy == .model ? model : client.client
            var slot = grouped[key] ?? {
                switch stackBy {
                case .model:
                    return DaySegment(
                        key: key, label: model,
                        color: colors.color(client.providerId, model), tokens: 0, cost: 0)
                case .agent:
                    return DaySegment(
                        key: key, label: ClientRegistry.shortName(client.client),
                        color: ClientRegistry.style(client.client).color, tokens: 0, cost: 0)
                }
            }()
            slot.tokens = slot.tokens.saturatingAdding(tokens)
            slot.cost += client.cost
            grouped[key] = slot
        }
        // Stable stacking order across days: sort by key.
        return DayBar(
            date: contribution.date,
            segments: grouped.values.sorted { $0.key < $1.key })
    }

    /// Aggregate every segment across the visible window for the legend,
    /// heaviest-first by the active metric.
    public static func legend(bars: [DayBar], metric: ChartMetric) -> [DaySegment] {
        var agg: [String: DaySegment] = [:]
        for bar in bars {
            for seg in bar.segments {
                var slot = agg[seg.key]
                    ?? DaySegment(key: seg.key, label: seg.label, color: seg.color, tokens: 0, cost: 0)
                slot.tokens = slot.tokens.saturatingAdding(seg.tokens)
                slot.cost += seg.cost
                agg[seg.key] = slot
            }
        }
        return agg.values.sorted {
            metric == .cost ? $0.cost > $1.cost : $0.tokens > $1.tokens
        }
    }
}
