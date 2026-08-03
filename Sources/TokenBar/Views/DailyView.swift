import SwiftUI
import TokenBarCore

/// Fold the shared hourly report into the calendar keys used by Daily and
/// Monthly. Hour keys are local YYYY-MM-DD HH:00; malformed slots are
/// ignored before the saturating turn-count fold.
enum TurnCountBuckets {
    static func byDay(_ report: HourlyReport?) -> [String: Int64] {
        fold(report) { String($0.prefix(10)) }
    }

    static func byMonth(_ report: HourlyReport?) -> [String: Int64] {
        fold(report) { String($0.prefix(7)) }
    }

    static func scope(_ clientIds: [String]) -> String? {
        let names = clientIds.map(ClientRegistry.shortName)
        switch names.count {
        case 0: return nil
        case 1: return "Turns · %@ only".localized(names[0])
        default: return "Turns · %@ + %@ only".localized(names[0], names[1])
        }
    }

    static func showsLoading(
        report: HourlyReport?, requestInFlight: Bool, clientIds: [String]
    ) -> Bool {
        report == nil && requestInFlight && !clientIds.isEmpty
    }

    private static func fold(
        _ report: HourlyReport?, key: (String) -> String
    ) -> [String: Int64] {
        var result: [String: Int64] = [:]
        for entry in report?.entries ?? [] {
            guard let valid = validHour(entry.hour), entry.turnCount >= 0 else { continue }
            result[key(valid)] = (result[key(valid)] ?? 0)
                .saturatingAdding(Int64(entry.turnCount))
        }
        return result
    }

    private static func validHour(_ raw: String) -> String? {
        let bytes = Array(raw.utf8)
        guard bytes.count == 16,
              bytes[4] == 45, bytes[7] == 45, bytes[10] == 32,
              bytes[13] == 58, bytes[14] == 48, bytes[15] == 48
        else { return nil }
        for index in [0, 1, 2, 3, 5, 6, 8, 9, 11, 12] {
            guard bytes[index] >= 48, bytes[index] <= 57 else { return nil }
        }
        let year = Int(raw.prefix(4)) ?? 0
        let month = Int(raw.dropFirst(5).prefix(2)) ?? 0
        let day = Int(raw.dropFirst(8).prefix(2)) ?? 0
        let hour = Int(raw.dropFirst(11).prefix(2)) ?? 0
        guard year > 0, (1...12).contains(month), (0...23).contains(hour) else {
            return nil
        }
        let leap = year.isMultiple(of: 4) && (!year.isMultiple(of: 100) || year.isMultiple(of: 400))
        let daysInMonth = [31, leap ? 29 : 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][month - 1]
        guard (1...daysInMonth).contains(day) else { return nil }
        return raw
    }
}

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
    let hourlyReport: HourlyReport?
    var turnClientIds: [String] = []
    var turnsLoading = false
    let colors: ModelColorMap

    @State private var openDate: String?
    @State private var hover: HoverState?
    @State private var tooltipSize: CGSize = .zero
    @Environment(\.popoverScrollViewport) private var popoverScrollViewport

    struct DayRow {
        let date: String
        let tokens: Int64
        let cost: Double
        let messages: Int
        let turns: Int64?
        let contribution: Contribution
    }

    struct ModelSlice {
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

    var rows: [DayRow] {
        let allow = Set(clientIds)
        let turnsByDay = TurnCountBuckets.byDay(hourlyReport)
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
            guard tokens > 0 || cost > 0 || messages > 0 else { return nil }
            let turns = hourlyReport == nil ? nil : turnsByDay[c.date] ?? 0
            return DayRow(
                date: c.date, tokens: tokens, cost: cost, messages: messages,
                turns: turns, contribution: c)
        }
        .sorted { $0.date > $1.date }
    }

    func models(for c: Contribution) -> [ModelSlice] {
        let allow = Set(clientIds)
        var grouped: [String: ModelSlice] = [:]
        for cc in c.clients {
            if !allow.contains(cc.client) { continue }
            let tokens = Self.tokenTotal(cc.tokens)
            if tokens <= 0 && cc.cost <= 0 && cc.messages <= 0 { continue }
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
            subtitle: hourlyReport == nil ? nil : TurnCountBuckets.scope(turnClientIds),
            trailing: {
                Text((rows.count == 1 ? "%lld active day" : "%lld active days")
                    .localized(rows.count))
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
                    Text("%@ msgs".localized(row.messages.formatted()))
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                    if let turns = row.turns {
                        Text("%@ turns".localized(turns.formatted()))
                            .font(.caption2)
                            .foregroundStyle(.tertiary)
                    } else if TurnCountBuckets.showsLoading(
                        report: hourlyReport, requestInFlight: turnsLoading,
                        clientIds: turnClientIds)
                    {
                        ProgressView()
                            .controlSize(.mini)
                            .frame(width: 10, height: 10)
                            .accessibilityLabel("Loading…")
                    }
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
