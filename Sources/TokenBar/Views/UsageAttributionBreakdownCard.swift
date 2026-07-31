import SwiftUI
import TokenBarCore

/// API-list-price equivalent split by the user's confirmed subscription
/// declarations. The report carries the year it was loaded for; the subtitle
/// keeps that actual range visible without inventing a new window.
struct UsageAttributionBreakdownCard: View {
    let loadedModelReport: LoadedModelReport?
    let clientIds: [String]

    private var confirmed: [UsageAttribution.Record] {
        UsageAttribution.confirmed(defaults: .standard).records
    }

    static func rangeLabel(for loadedModelReport: LoadedModelReport?) -> String {
        loadedModelReport?.year ?? "All years"
    }

    var body: some View {
        let rows = loadedModelReport.map {
            // Use raw entries: modelLevelEntries folds providers and erases the
            // dimension the attribution declarations resolve against.
            UsageAttributionBreakdown.rows(
                entries: $0.report.entries, clientIds: clientIds, confirmed: confirmed)
        }

        DashCard(
            UsageAttributionBreakdown.Copy.title,
            subtitle: Self.rangeLabel(for: loadedModelReport)
        ) {
            if let rows {
                if rows.isEmpty {
                    Text(UsageAttributionBreakdown.Copy.noUsage.localized)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                } else {
                    if confirmed.isEmpty {
                        Text(UsageAttributionBreakdown.Copy.hint.localized)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    VStack(spacing: 6) {
                        ForEach(rows) { row in
                            breakdownRow(row)
                        }
                    }
                }
            } else {
                LoadingLine(title: "Loading…")
            }
        }
    }

    private func breakdownRow(_ row: UsageAttributionBreakdown.Row) -> some View {
        let excluded: Bool
        let label: String
        switch row.state {
        case let .assigned(target):
            excluded = false
            label = UsageAttributionSettings.Copy.assigned.localized(
                ClientRegistry.shortName(target))
        case .excluded:
            excluded = true
            label = UsageAttributionSettings.Copy.excluded.localized
        case .unassigned:
            excluded = false
            label = UsageAttributionSettings.Copy.unassigned.localized
        }

        return HStack(spacing: 8) {
            Text(label)
                .font(.caption.weight(excluded ? .semibold : .regular))
                .foregroundStyle(
                    excluded ? AnyShapeStyle(Color.primary) : AnyShapeStyle(.secondary))
                .lineLimit(1)
            Spacer(minLength: 8)
            Text(Format.compactTokens(row.tokens))
                .font(.caption.monospacedDigit())
            Text(Format.usd(row.cost))
                .font(.caption.monospacedDigit())
                .foregroundStyle(excluded ? Color.orange : Color(hex: "#22c55e"))
        }
        .padding(.vertical, 3)
        .padding(.horizontal, excluded ? 6 : 0)
        .background(
            excluded ? Color.orange.opacity(0.12) : Color.clear,
            in: RoundedRectangle(cornerRadius: 6))
    }
}
