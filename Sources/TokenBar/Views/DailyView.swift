import SwiftUI
import TokenBarCore

/// "Daily" lens, port of DailyView.tsx: one row per active day (most recent
/// first) with msgs / tokens / cost. Selecting a day drills into that day's
/// per-model split — the same provider-tinted breakdown the Models view uses,
/// scoped to the date.
struct DailyView: View {
    let payload: UsagePayload
    /// Restrict to these clients (strict membership). Empty = show nothing —
    /// consistent with the day rows and DayBars/UsageStats — so an all-hidden
    /// slice can't leak the drill-down.
    var clientIds: [String] = []
    let colors: ModelColorMap

    @State private var openDate: String?

    private struct DayRow {
        let date: String
        let tokens: Int64
        let cost: Double
        let messages: Int
        let contribution: Contribution
    }

    private struct ModelSlice {
        let key: String
        let model: String
        let provider: String
        let color: String
        var tokens: Int64
        var cost: Double
    }

    private static func tokenTotal(_ t: TokenBreakdown) -> Int64 {
        // Delegate to the shared saturating sum (was a plain-`+` 4th copy).
        t.total
    }

    private var rows: [DayRow] {
        let allow = Set(clientIds)
        return payload.contributions.compactMap { c -> DayRow? in
            var tokens: Int64 = 0
            var cost = 0.0
            var messages = 0
            for cc in c.clients {
                if !allow.contains(cc.client) { continue }
                tokens = tokens.saturatingAdding(Self.tokenTotal(cc.tokens))
                cost += cc.cost
                messages += cc.messages
            }
            guard tokens > 0 || cost > 0 else { return nil }
            return DayRow(date: c.date, tokens: tokens, cost: cost, messages: messages, contribution: c)
        }
        .sorted { $0.date > $1.date }
    }

    private func models(for c: Contribution) -> [ModelSlice] {
        let allow = Set(clientIds)
        var grouped: [String: ModelSlice] = [:]
        for cc in c.clients {
            if !allow.contains(cc.client) { continue }
            let tokens = Self.tokenTotal(cc.tokens)
            if tokens <= 0 && cc.cost <= 0 { continue }
            let model = cc.modelId.isEmpty ? "unknown" : cc.modelId
            let key = "\(model)|\(cc.providerId)"
            var slot = grouped[key] ?? ModelSlice(
                key: key, model: model, provider: cc.providerId,
                color: colors.color(cc.providerId, model), tokens: 0, cost: 0)
            slot.tokens = slot.tokens.saturatingAdding(tokens)
            slot.cost += cc.cost
            grouped[key] = slot
        }
        return grouped.values.sorted {
            $0.cost != $1.cost ? $0.cost > $1.cost : $0.tokens > $1.tokens
        }
    }

    var body: some View {
        let rows = self.rows
        DashCard(
            "Daily",
            trailing: {
                Text("\(rows.count) active day\(rows.count == 1 ? "" : "s")")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
        ) {
            if rows.isEmpty {
                Text("No usage in this range")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                VStack(spacing: 2) {
                    ForEach(rows, id: \.date) { row in
                        dayItem(row)
                    }
                }
            }
        }
    }

    @ViewBuilder private func dayItem(_ row: DayRow) -> some View {
        let isOpen = openDate == row.date
        VStack(spacing: 4) {
            Button {
                withAnimation(.easeOut(duration: 0.15)) {
                    openDate = isOpen ? nil : row.date
                }
            } label: {
                HStack(spacing: 8) {
                    Image(systemName: "chevron.right")
                        .font(.system(size: 8, weight: .semibold))
                        .foregroundStyle(.tertiary)
                        .rotationEffect(.degrees(isOpen ? 90 : 0))
                    Text(Format.monthDay(row.date))
                        .font(.caption)
                    Text("\(row.messages.formatted()) msgs")
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                    Spacer()
                    Text(Format.compactTokens(row.tokens))
                        .font(.caption.monospacedDigit())
                        .foregroundStyle(.secondary)
                    Text(Format.usd(row.cost))
                        .font(.caption.monospacedDigit())
                        .frame(minWidth: 56, alignment: .trailing)
                }
                .padding(.vertical, 4)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)

            if isOpen {
                VStack(spacing: 4) {
                    ForEach(models(for: row.contribution), id: \.key) { slice in
                        HStack(spacing: 8) {
                            Circle()
                                .fill(Color(hex: slice.color))
                                .frame(width: 6, height: 6)
                            Text(slice.model)
                                .font(.caption2)
                                .lineLimit(1)
                                .truncationMode(.middle)
                                .help("\(slice.model) · \(slice.provider)")
                            Spacer()
                            Text(Format.compactTokens(slice.tokens))
                                .font(.caption2.monospacedDigit())
                                .foregroundStyle(.secondary)
                            Text(Format.usd(slice.cost))
                                .font(.caption2.monospacedDigit())
                                .frame(minWidth: 50, alignment: .trailing)
                        }
                    }
                }
                .padding(.leading, 18)
                .padding(.bottom, 6)
            }
        }
    }
}
