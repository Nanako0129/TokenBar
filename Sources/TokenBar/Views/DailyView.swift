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
    @State private var hover: HoverState?
    @State private var tooltipSize: CGSize = .zero
    @Environment(\.popoverScrollViewport) private var popoverScrollViewport

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
        var input: Int64
        var output: Int64
        var cacheRead: Int64
        var cacheWrite: Int64
        var reasoning: Int64
        var tokens: Int64
        var cost: Double
    }

    private struct HoverState {
        let slice: ModelSlice
        let point: CGPoint
    }

    private static let rowsSpace = "daily-model-rows"

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
                color: colors.color(cc.providerId, model),
                input: 0, output: 0, cacheRead: 0, cacheWrite: 0, reasoning: 0,
                tokens: 0, cost: 0)
            slot.input = slot.input.saturatingAdding(cc.tokens.input)
            slot.output = slot.output.saturatingAdding(cc.tokens.output)
            slot.cacheRead = slot.cacheRead.saturatingAdding(cc.tokens.cacheRead)
            slot.cacheWrite = slot.cacheWrite.saturatingAdding(cc.tokens.cacheWrite)
            slot.reasoning = slot.reasoning.saturatingAdding(cc.tokens.reasoning)
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
        .zIndex(hover == nil ? 0 : 1)
    }

    @ViewBuilder private func dayItem(_ row: DayRow) -> some View {
        let isOpen = openDate == row.date
        VStack(spacing: 4) {
            Button {
                hover = nil
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
                        let isHovered = hover?.slice.key == slice.key
                        HStack(spacing: 8) {
                            Circle()
                                .fill(Color(hex: slice.color))
                                .frame(width: 6, height: 6)
                                .overlay {
                                    Circle().stroke(
                                        Color.primary.opacity(isHovered ? 0.85 : 0),
                                        lineWidth: 1)
                                }
                                .shadow(
                                    color: Color.primary.opacity(isHovered ? 0.65 : 0),
                                    radius: isHovered ? 3 : 0)
                            Text(slice.model)
                                .font(.caption2)
                                .lineLimit(1)
                                .truncationMode(.middle)
                            Spacer()
                            Text(Format.compactTokens(slice.tokens))
                                .font(.caption2.monospacedDigit())
                                .foregroundStyle(.secondary)
                            Text(Format.usd(slice.cost))
                                .font(.caption2.monospacedDigit())
                                .frame(minWidth: 50, alignment: .trailing)
                        }
                        .contentShape(Rectangle())
                        .onContinuousHover(coordinateSpace: .named(Self.rowsSpace)) { phase in
                            switch phase {
                            case let .active(point):
                                hover = HoverState(slice: slice, point: point)
                            case .ended:
                                if hover?.slice.key == slice.key {
                                    hover = nil
                                }
                            }
                        }
                    }
                }
                .coordinateSpace(name: Self.rowsSpace)
                .overlay(alignment: .topLeading) {
                    if let hover {
                        GeometryReader { geo in
                            let measuredSize = tooltipSize == .zero
                                ? CGSize(width: ModelUsageTooltip.width, height: 120)
                                : tooltipSize
                            let offset = PopoverTooltipPlacement.offset(
                                anchor: hover.point,
                                tooltipSize: measuredSize,
                                containerFrame: geo.frame(in: .global),
                                viewport: popoverScrollViewport)
                            tooltip(hover.slice)
                                .offset(offset ?? .zero)
                        }
                        .allowsHitTesting(false)
                    }
                }
                .padding(.leading, 18)
                .padding(.bottom, 6)
            }
        }
        .zIndex(isOpen ? 1 : 0)
    }

    private func tooltip(_ slice: ModelSlice) -> some View {
        ModelUsageTooltip(
            model: slice.model,
            provider: slice.provider,
            context: nil,
            color: slice.color,
            input: slice.input,
            output: slice.output,
            cacheRead: slice.cacheRead,
            cacheWrite: slice.cacheWrite,
            reasoning: slice.reasoning,
            total: slice.tokens,
            cost: slice.cost,
            measuredSize: $tooltipSize)
    }
}
