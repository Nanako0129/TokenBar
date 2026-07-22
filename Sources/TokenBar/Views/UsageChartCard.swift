import SwiftUI
import TokenBarCore

/// Pure chart geometry shared by canvas, hover hit math, and selftests.
/// Kept outside `UsageChartCard` so tests do not hit View MainActor isolation.
enum UsageChartGeometry {
    static let chartHeight: CGFloat = 150
    static let gap: CGFloat = 3

    static func barFrame(index: Int, barWidth: CGFloat) -> CGRect {
        CGRect(
            x: CGFloat(index) * (barWidth + gap),
            y: 0,
            width: barWidth,
            height: chartHeight)
    }

    /// Floor-division hit index (pre-change). Gap pixels attach to the left
    /// bar; points past the last stride are out of range.
    static func barIndex(atX x: CGFloat, barWidth: CGFloat, count: Int) -> Int? {
        guard count > 0, barWidth > 0, x >= 0 else { return nil }
        let stride = barWidth + gap
        let index = Int(x / stride)
        return (0..<count).contains(index) ? index : nil
    }
}

/// The "Token Usage" card, port of UsageBarGraph2D.tsx: trailing-30-day
/// stacked bars (Model/Agent stacking, Tokens/Price metric, wrapping legend,
/// rich hover tooltip) toggling with the full-year 3D contribution grid.
struct UsageChartCard: View {
    let payload: UsagePayload
    /// Clients included in the stack (the active tab's slice).
    let clientIds: [String]
    let stats: UsageStats
    let colors: ModelColorMap
    /// Dashboard year filter; nil (all time) falls back to the current year
    /// for the 3D grid, which is inherently single-year.
    var year: String?

    @AppStorage("tokenbar.chart.stackBy") private var stackByRaw = StackBy.model.rawValue
    @AppStorage("tokenbar.chart.metric") private var metricRaw = ChartMetric.tokens.rawValue
    /// "2d" = trailing-30-day stacked bars, "3d" = full-year contribution grid.
    @AppStorage("tokenbar.chart.view") private var chartViewRaw = "2d"
    @State private var hoverIndex: Int?
    @State private var hoverPoint: CGPoint = .zero
    @State private var tooltipSize: CGSize = .zero
    @Environment(\.popoverScrollViewport) private var popoverScrollViewport

    private static let legendMax = 12

    private var stackBy: StackBy { StackBy(rawValue: stackByRaw) ?? .model }
    private var metric: ChartMetric { ChartMetric(rawValue: metricRaw) ?? .tokens }

    private var bars: [DayBar] {
        // Anchor the trailing window to the filtered stats' range end (selection-
        // derived; equals meta.dateRange.end when nothing is hidden) so a hidden
        // client's later activity can't shift visible activity out of the chart.
        DayBars.build(
            payload: payload, clientIds: clientIds, stackBy: stackBy,
            colors: colors, rangeEnd: stats.dateRange.end, endFallback: Format.todayKey())
    }

    private var is3D: Bool { chartViewRaw == "3d" }

    var body: some View {
        let bars = self.bars
        let legend = DayBars.legend(bars: bars, metric: metric)
        let showsTooltip = hoverIndex.map {
            bars.indices.contains($0) && !bars[$0].isEmpty
        } ?? false
        DashCard(
            "Token Usage",
            subtitle: is3D
                ? "Full year"
                : (stackBy == .model ? "Stacked by model" : "Stacked by agent"),
            trailing: { toggles }
        ) {
            togglesRow
            TokenUsageRow(stats: stats)
            if is3D {
                // Year grid over the same client slice; sized to match the 2D
                // legend + chart + axis block so the card doesn't jump.
                ContributionGraph3D(
                    grid: buildGrid(
                        year: year ?? String(Format.todayKey().prefix(4)),
                        perDayMap: stats.perDayMap)
                )
                .frame(height: 196)
            } else {
                legendView(legend)
                chart(bars)
                HStack {
                    axisLabel(bars.first?.date)
                    Spacer()
                    axisLabel(bars.last?.date)
                }
            }
        }
        .zIndex(showsTooltip ? 1 : 0)
    }

    // MARK: - Header toggles

    /// The 2D/3D view switch rides the header; the 2D-only group/metric
    /// toggles get their own slim row below — stacked in the header they made
    /// it three rows tall and left the card top mostly whitespace, and all
    /// three don't fit beside the title without wrapping.
    private var toggles: some View {
        picker(selection: $chartViewRaw, options: [("2d", "2D"), ("3d", "3D")])
    }

    @ViewBuilder private var togglesRow: some View {
        // Stacking and bar metric are 2D-only concepts — the 3D view is the
        // year heatmap (web hides these the same way).
        if !is3D {
            HStack(spacing: 4) {
                Spacer()
                picker(selection: $stackByRaw, options: [
                    (StackBy.model.rawValue, "Model"), (StackBy.agent.rawValue, "Agent"),
                ])
                picker(selection: $metricRaw, options: [
                    (ChartMetric.tokens.rawValue, "Tokens"), (ChartMetric.cost.rawValue, "Price"),
                ])
            }
        }
    }

    /// Compact two-option toggle, tighter than the native segmented picker.
    private func picker(selection: Binding<String>, options: [(String, String)]) -> some View {
        HStack(spacing: 2) {
            ForEach(options, id: \.0) { value, label in
                Button(label) { selection.wrappedValue = value }
                    .buttonStyle(.plain)
                    .lineLimit(1)
                    .fixedSize()
                    .font(.caption2.weight(selection.wrappedValue == value ? .semibold : .regular))
                    .foregroundStyle(selection.wrappedValue == value ? .primary : .secondary)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 2)
                    .background(
                        selection.wrappedValue == value
                            ? AnyShapeStyle(Color.primary.opacity(0.16))
                            : AnyShapeStyle(.clear),
                        in: RoundedRectangle(cornerRadius: 4))
            }
        }
        .padding(1)
        // Plain adaptive fill: these ride *inside* the card's glass, and
        // nesting glass effects renders as a murky dark blob.
        .background(Color.primary.opacity(0.07), in: RoundedRectangle(cornerRadius: 6))
    }

    // MARK: - Legend

    @ViewBuilder private func legendView(_ legend: [DaySegment]) -> some View {
        let shown = Array(legend.prefix(Self.legendMax))
        let hidden = legend.count - shown.count
        if !shown.isEmpty {
            FlowLayout(hSpacing: 8, vSpacing: 3) {
                ForEach(shown, id: \.key) { item in
                    HStack(spacing: 4) {
                        Circle().fill(Color(hex: item.color)).frame(width: 6, height: 6)
                        Text(item.label).lineLimit(1)
                    }
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                }
                if hidden > 0 {
                    Text("+\(hidden)")
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                }
            }
        }
    }

    // MARK: - Chart

    private func chart(_ bars: [DayBar]) -> some View {
        GeometryReader { geo in
            let width = geo.size.width
            let barWidth = (width - UsageChartGeometry.gap * CGFloat(max(bars.count - 1, 0)))
                / CGFloat(max(bars.count, 1))
            let maxValue = max(bars.map(barTotal).max() ?? 1, metric == .cost ? 0.000001 : 1)
            // Anchor X on the bar center (pre-change feel); Y tracks the
            // cursor so region-dodge can flip above/below inside the chart.
            let anchor: CGPoint = {
                guard let index = hoverIndex, bars.indices.contains(index) else {
                    return hoverPoint
                }
                let frame = UsageChartGeometry.barFrame(index: index, barWidth: barWidth)
                return CGPoint(x: frame.midX, y: hoverPoint.y)
            }()

            ZStack(alignment: .topLeading) {
                canvas(bars: bars, barWidth: barWidth, maxValue: maxValue)
                if let index = hoverIndex, bars.indices.contains(index),
                   !bars[index].isEmpty {
                    let bar = bars[index]
                    let total = barTotal(bar)
                    if total > 0 {
                        let frame = UsageChartGeometry.barFrame(index: index, barWidth: barWidth)
                        let height = CGFloat(total / maxValue) * (geo.size.height - 8)
                        RoundedRectangle(cornerRadius: min(2, height / 2))
                            .stroke(Color.primary.opacity(0.85), lineWidth: 1)
                            .frame(width: barWidth, height: height)
                            .position(x: frame.midX, y: geo.size.height - 1 - height / 2)
                            .shadow(color: Color.primary.opacity(0.65), radius: 3)
                            .allowsHitTesting(false)
                    }
                    let measuredSize = tooltipSize == .zero
                        ? CGSize(width: Self.tooltipWidth, height: 120)
                        : tooltipSize
                    let offset = PopoverTooltipPlacement.offset(
                        anchor: anchor,
                        tooltipSize: measuredSize,
                        containerFrame: geo.frame(in: .global),
                        viewport: popoverScrollViewport)
                    tooltip(bar)
                        .offset(offset ?? .zero)
                }
            }
            // One continuous hover on the chart (not per-bar) — smoother and
            // matches the pre-change dodge path. Gap pixels attach to the
            // left bar via floor division (see `UsageChartGeometry.barIndex`).
            .contentShape(Rectangle())
            .onContinuousHover { phase in
                switch phase {
                case let .active(point):
                    hoverIndex = UsageChartGeometry.barIndex(
                        atX: point.x, barWidth: barWidth, count: bars.count)
                    hoverPoint = point
                case .ended:
                    hoverIndex = nil
                }
            }
        }
        .frame(height: UsageChartGeometry.chartHeight)
    }

    private func canvas(bars: [DayBar], barWidth: CGFloat, maxValue: Double) -> some View {
        Canvas { context, size in
            let bottom = size.height - 1
            // Axis line.
            context.fill(
                Path(CGRect(x: 0, y: bottom, width: size.width, height: 1)),
                with: .color(.secondary.opacity(0.3)))

            for (index, bar) in bars.enumerated() {
                let frame = UsageChartGeometry.barFrame(index: index, barWidth: barWidth)
                let total = barTotal(bar)
                if total <= 0 {
                    context.fill(
                        Path(roundedRect: CGRect(x: frame.minX, y: bottom - 2, width: barWidth, height: 2),
                             cornerRadius: 1),
                        with: .color(.secondary.opacity(0.15)))
                    continue
                }
                let totalHeight = CGFloat(total / maxValue) * (size.height - 8)
                var y = bottom
                for segment in bar.segments {
                    let h = totalHeight * CGFloat(segValue(segment) / total)
                    guard h > 0 else { continue }
                    y -= h
                    context.fill(
                        Path(roundedRect: CGRect(x: frame.minX, y: y, width: barWidth, height: h),
                             cornerRadius: min(2, h / 2)),
                        with: .color(Color(hex: segment.color).opacity(0.86)))
                }
            }
        }
    }

    private func axisLabel(_ date: String?) -> some View {
        Text(date.map(Format.monthDay) ?? "")
            .font(.caption2)
            .foregroundStyle(.tertiary)
    }

    private func barTotal(_ bar: DayBar) -> Double {
        metric == .cost ? bar.totalCost : Double(bar.totalTokens)
    }

    private func segValue(_ segment: DaySegment) -> Double {
        metric == .cost ? segment.cost : Double(segment.tokens)
    }

    // MARK: - Tooltip

    private static let tooltipWidth: CGFloat = 210

    private func tooltip(_ bar: DayBar) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(Format.monthDay(bar.date))
                .font(.caption.weight(.semibold))
            HStack {
                Text("\(Format.exactTokens(bar.totalTokens)) tokens")
                Spacer()
                Text(Format.usd(bar.totalCost))
            }
            .font(.caption2)
            .foregroundStyle(.secondary)
            ForEach(
                bar.segments.sorted { $0.tokens > $1.tokens }.prefix(6), id: \.key
            ) { segment in
                HStack(spacing: 4) {
                    Circle().fill(Color(hex: segment.color)).frame(width: 5, height: 5)
                    Text(segment.label).lineLimit(1)
                    Spacer()
                    Text("\(Format.compactTokens(segment.tokens)) · \(Format.usd(segment.cost))")
                        .foregroundStyle(.secondary)
                }
                .font(.caption2)
            }
        }
        .padding(8)
        .frame(width: Self.tooltipWidth, alignment: .leading)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(.quaternary))
        .onGeometryChange(for: CGSize.self) { $0.size } action: { tooltipSize = $0 }
        .allowsHitTesting(false)
    }
}
