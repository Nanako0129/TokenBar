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

    /// The two non-subscription buckets each get their own tint because they
    /// mean opposite things: `excluded` is money actually charged, `unassigned`
    /// is work left for the user. Sharing one colour, or leaving `unassigned`
    /// bare, would read as "nothing to do here".
    private enum RowTint {
        case assigned
        case excluded
        case unassigned

        var fill: Color {
            switch self {
            case .assigned: return .clear
            case .excluded: return Color.orange.opacity(0.12)
            case .unassigned: return Color(hex: "#3b82f6").opacity(0.12)
            }
        }

        var amount: Color {
            switch self {
            case .assigned, .unassigned: return Color(hex: "#22c55e")
            case .excluded: return .orange
            }
        }

        var label: AnyShapeStyle {
            self == .assigned ? AnyShapeStyle(.secondary) : AnyShapeStyle(Color.primary)
        }
    }

    private func breakdownRow(_ row: UsageAttributionBreakdown.Row) -> some View {
        let tint: RowTint
        let label: String
        switch row.state {
        case let .assigned(target):
            tint = .assigned
            label = UsageAttributionSettings.Copy.assigned.localized(
                ClientRegistry.shortName(target))
        case .excluded:
            tint = .excluded
            label = UsageAttributionSettings.Copy.excluded.localized
        case .unassigned:
            tint = .unassigned
            label = UsageAttributionSettings.Copy.unassigned.localized
        }

        return HStack(spacing: 8) {
            Text(label)
                .font(.caption.weight(tint == .assigned ? .regular : .semibold))
                .foregroundStyle(tint.label)
                .lineLimit(1)
            Spacer(minLength: 8)
            // Both figure columns reserve a minimum width and align trailing,
            // so short values pad with blank space instead of sliding their
            // column's left edge per row. minWidth rather than a fixed width:
            // an unusually long total still grows instead of clipping.
            Text(Format.compactTokens(row.tokens))
                .font(.caption.monospacedDigit())
                .frame(minWidth: 52, alignment: .trailing)
            Text(Format.usd(row.cost))
                .font(.caption.monospacedDigit())
                .foregroundStyle(tint.amount)
                .frame(minWidth: 76, alignment: .trailing)
        }
        // Every row carries the same inset so a tinted row's text and figures
        // stay on the same left and right edges as the untinted ones; padding
        // only the tinted rows visibly shortened them at both ends.
        .padding(.vertical, 3)
        .padding(.horizontal, 6)
        .background(tint.fill, in: RoundedRectangle(cornerRadius: 6))
    }
}
