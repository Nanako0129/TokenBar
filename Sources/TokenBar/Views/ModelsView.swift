import SwiftUI
import TokenBarCore

/// "Models by cost" lens, port of ModelsView.tsx (itself adapted from
/// tokscale's TUI models view). One row per model sorted by cost:
/// provider-tinted dot + name + share of total cost, a dim In·Out·CR·CW token
/// split, and the model's totals with cost in green. No row cap — it scrolls.
struct ModelsView: View {
    let report: ModelReport?
    /// Restrict rows to these clients; empty = show everything.
    var clientIds: [String] = []
    let colors: ModelColorMap

    private static let kinds: [(label: String, pick: (ModelReportEntry) -> Int64)] = [
        ("In", { $0.input }),
        ("Out", { $0.output }),
        ("CR", { $0.cacheRead }),
        ("CW", { $0.cacheWrite }),
    ]

    var body: some View {
        let allow = Set(clientIds)
        let rows = (report?.entries ?? [])
            .filter { allow.contains($0.client) }
            .sorted { $0.cost != $1.cost ? $0.cost > $1.cost : $0.total > $1.total }
        let totalCost = rows.reduce(0) { $0 + $1.cost }
        let totalTokens = rows.reduce(Int64(0)) { $0.saturatingAdding($1.total) }

        DashCard(
            "Models by cost",
            trailing: {
                VStack(alignment: .trailing, spacing: 1) {
                    Text("\(rows.count) model\(rows.count == 1 ? "" : "s") · \(Format.compactTokens(totalTokens)) · \(Format.usd(totalCost))")
                        .foregroundStyle(.secondary)
                    if let updatedAt = report?.pricingUpdatedAt {
                        Text("Prices updated \(Format.relativeTime(updatedAt))")
                            .foregroundStyle(.tertiary)
                            .help("LiteLLM pricing data; refreshes automatically about once an hour")
                    }
                }
                .font(.caption2)
            }
        ) {
            if rows.isEmpty {
                Text("No model usage in this range")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                VStack(spacing: 10) {
                    ForEach(rows, id: \.rowID) { entry in
                        row(entry, totalCost: totalCost)
                    }
                }
            }
        }
    }

    private func row(_ entry: ModelReportEntry, totalCost: Double) -> some View {
        let share = totalCost > 0 ? entry.cost / totalCost * 100 : 0
        return HStack(spacing: 8) {
            Circle()
                .fill(Color(hex: colors.color(entry.provider, entry.model)))
                .frame(width: 8, height: 8)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(entry.model)
                        .font(.caption)
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .help("\(entry.model) · \(ClientRegistry.style(entry.client).displayName)")
                    Text(String(format: "%.1f%%", share))
                        .font(.caption2.monospacedDigit())
                        .foregroundStyle(.tertiary)
                }
                HStack(spacing: 8) {
                    ForEach(Self.kinds, id: \.label) { kind in
                        (Text(kind.label + " ").foregroundStyle(.tertiary)
                            + Text(Format.compactTokens(kind.pick(entry))))
                            .font(.caption2.monospacedDigit())
                            .foregroundStyle(.secondary)
                    }
                }
            }
            Spacer(minLength: 8)
            VStack(alignment: .trailing, spacing: 2) {
                Text(Format.compactTokens(entry.total))
                    .font(.caption.monospacedDigit())
                Text(Format.usd(entry.cost))
                    .font(.caption2.monospacedDigit())
                    .foregroundStyle(Color(hex: "#22c55e"))
            }
        }
    }
}
