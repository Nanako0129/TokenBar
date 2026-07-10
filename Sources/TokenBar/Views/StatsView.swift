import SwiftUI
import TokenBarCore

/// "Stats" lens, port of StatsView.tsx: the usage chart over a headline
/// summary — total spend, six metric cells, favorite model and best day —
/// distilled from the same stats and model report the other lenses use.
struct StatsView: View {
    let payload: UsagePayload
    /// The active tab's client slice.
    let clientIds: [String]
    let stats: UsageStats
    let modelReport: ModelReport?
    let colors: ModelColorMap
    /// Dashboard year filter (nil = all time), forwarded to the chart card.
    var year: String?

    private var favorite: ModelReportEntry? {
        let allow = Set(clientIds)
        return (modelReport?.entries ?? [])
            .filter { allow.contains($0.client) }
            .max { $0.cost < $1.cost }
    }

    var body: some View {
        VStack(spacing: 12) {
            UsageChartCard(
                payload: payload, clientIds: clientIds, stats: stats, colors: colors,
                year: year)
            summaryCard
        }
    }

    private var summaryCard: some View {
        VStack(alignment: .leading, spacing: 12) {
            (Text("Your total spending is ")
                + Text(Format.usd(stats.totalCost)).bold().foregroundStyle(Color(hex: "#22c55e")))
                .font(.system(size: 14))

            let metrics: [(label: String, value: String, accent: Bool)] = [
                ("Total tokens", Format.compactTokens(stats.totalTokens), false),
                ("Total spend", Format.usd(stats.totalCost), true),
                ("Active days", String(stats.activeDays), false),
                ("Avg / day", Format.usd(stats.averagePerDay), false),
                ("Current streak", "\(stats.streaks.current)d", false),
                ("Longest streak", "\(stats.streaks.longest)d", false),
            ]
            LazyVGrid(
                columns: Array(repeating: GridItem(.flexible(), alignment: .leading), count: 3),
                spacing: 10
            ) {
                ForEach(metrics, id: \.label) { m in
                    VStack(alignment: .leading, spacing: 2) {
                        Text(m.value)
                            .font(.system(size: 14, weight: .semibold).monospacedDigit())
                            .foregroundStyle(m.accent ? AnyShapeStyle(Color(hex: "#22c55e")) : AnyShapeStyle(.primary))
                        Text(m.label)
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                    }
                }
            }

            VStack(alignment: .leading, spacing: 5) {
                if let favorite {
                    HStack {
                        Text("Favorite model")
                            .foregroundStyle(.secondary)
                        Spacer()
                        HStack(spacing: 5) {
                            Circle()
                                .fill(Color(hex: colors.color(favorite.provider, favorite.model)))
                                .frame(width: 7, height: 7)
                            Text(favorite.model)
                                .lineLimit(1)
                                .truncationMode(.middle)
                        }
                    }
                    .font(.caption)
                }
                if let bestDay = stats.bestDay {
                    HStack {
                        Text("Best day")
                            .foregroundStyle(.secondary)
                        Spacer()
                        Text("\(Format.monthDay(bestDay.date)) · \(Format.usd(bestDay.cost))")
                    }
                    .font(.caption)
                }
            }
        }
        .padding(12)
        .glassCard()
    }
}
