import SwiftUI
import TokenBarCore

/// Daily spend stacked by the subscription the user declared it against.
///
/// The existing usage chart buckets by CLIENT — which tool ran the work. On
/// this user's data the two answers invert: by client, `claude` carried $4,583
/// over a week and `codex` $510; by declaration it is `codex` $4,411 and
/// `claude` $682. So this is not a restyled copy of that chart, it answers a
/// question no existing view answers.
struct SubscriptionTrendCard: View {
    /// Nil means the attributed daily series has not been published yet, which
    /// is NOT the same as an empty range. Basing the empty copy on the quota
    /// fetch instead — a different subsystem with a different lifecycle — made
    /// the card announce "no usage recorded" while its own data was still on
    /// the way and the header spinner was still turning.
    let trend: SubscriptionTrend?

    /// Cost or tokens. Cost leads because the API-equivalent figure is what the
    /// rest of this lens is denominated in; tokens are one tap away because a
    /// subscription question is really a token question with a price attached.
    @AppStorage("tokenbar.trend.metric") private var metricRaw = Metric.cost.rawValue

    /// The card's frame, and the hovered day's anchor translated into it. The
    /// tooltip is clamped to this frame and laid out in it — two different
    /// coordinate spaces there is the bug the window card took three attempts
    /// to fix.
    @State private var cardFrame: CGRect = .zero
    @State private var hoverIndex: Int?
    @State private var hoverAnchorInCard: CGPoint = .zero
    @State private var tooltipSize: CGSize = .zero
    @Environment(\.popoverScrollViewport) private var viewport

    private static let tooltipWidth: CGFloat = 176

    enum Metric: String, CaseIterable {
        case cost, tokens

        /// "Price", matching `UsageChartCard`'s toggle. Two cards in the same
        /// scroll calling one concept by two names is a name the reader has to
        /// reconcile.
        var label: String { self == .cost ? "Price" : "Tokens" }
    }

    private var metric: Metric { Metric(rawValue: metricRaw) ?? .cost }

    /// The stacking and listing order for the SELECTED metric, resolved in one
    /// place. Cost and tokens do not rank subscriptions the same way, and the
    /// cost order was applied to both views — so under Tokens the largest token
    /// consumer could sit behind smaller bands and fall outside the legend's
    /// `prefix(4)`, under a tooltip that says largest-first. Four consumers
    /// read this: the stacking loop, the tooltip list, the legend, and the
    /// overflow count.
    private func ordered(_ trend: SubscriptionTrend) -> [String] {
        trend.targets(byTokens: metric == .tokens)
    }

    private static let chartHeight: CGFloat = 78
    /// Wider than a hairline: whitespace between columns is what stops
    /// fourteen filled bars reading as one mass.
    private static let columnGap: CGFloat = 4
    /// A day with usage always draws at least this, so a quiet-but-worked day
    /// stays distinguishable from an idle one. Below it the column vanishes and
    /// the chart claims nothing happened.
    private static let minimumInk: CGFloat = 1.5
    /// Resting fill for every band. See `barHoverOpacity` for why this value.
    private static let barOpacity: Double = 0.50
    /// Hover lifts the whole column to the strip card's own 0.95, so "the one
    /// you point at comes forward" is the same gesture with the same number.
    ///
    /// Flat, not a gradient: every other fill in the app is flat, and a
    /// gradient here was actively misleading. Its start and end points are each
    /// segment's own top and bottom, so a multi-subscription column fades
    /// within every band and jumps back at each boundary — a vertical intensity
    /// the data does not have. The resting value sits below the usage chart's
    /// 0.86 because these columns are ~23pt wide and that alpha over this area
    /// is a solid block, and above the strip's 0.32 because hue is data here:
    /// each band IS a subscription, and low alpha collapses brand hues into
    /// each other over the popover material.
    private static let barHoverOpacity: Double = 0.95

    var body: some View {
        DashCard("Daily by subscription", subtitle: subtitle) {
            SegmentedPicker(
                selection: Binding(get: { metric }, set: { metricRaw = $0.rawValue }),
                options: Metric.allCases.map { (value: $0, label: $0.label) })
        } content: {
            switch Self.state(trend: trend, metric: metric) {
            case .chart:
                if let trend {
                    chart(trend)
                        .panelChartRegrow(metricRaw)
                    axis(trend)
                    legend(trend)
                    undeclaredHint(trend)
                }
            case .metricUnavailable:
                placeholder(Text(
                    (metric == .cost
                        ? "Usage recorded, but none of it is priced."
                        : "Usage recorded, but it carries no token counts.").localized)
                    .font(.caption)
                    .foregroundStyle(.secondary))
            case .noUsage:
                placeholder(Text("No usage recorded in this range.".localized)
                    .font(.caption)
                    .foregroundStyle(.secondary))
            case .loading:
                placeholder(LoadingLine(title: "Reading daily usage…"))
            }
        }
        .overlay(alignment: .topLeading) { tooltipLayer }
        .onGeometryChange(for: CGRect.self) { $0.frame(in: .global) } action: { cardFrame = $0 }
        .zIndex(hoverIndex == nil ? 0 : 1)
        .panelSwitchAnimation(metricRaw)
    }

    private var peak: Double {
        guard let trend else { return 0 }
        return metric == .cost ? trend.peakCost : Double(trend.peakTokens)
    }

    enum ContentState: Equatable {
        case loading
        case chart
        /// Usage was recorded, but the SELECTED metric has none of it.
        case metricUnavailable
        case noUsage
    }

    /// Which of the four things this card can show.
    ///
    /// Lifted out of `body` because the defect it exists to prevent lives in
    /// the branch order, and a condition written inside a SwiftUI view cannot
    /// be asserted on. `peak` is metric-specific, so testing it alone reported
    /// the whole range as empty whenever the other metric held everything —
    /// unpriced models in Price view, cost-carrying rows without token counts
    /// in Tokens view. The range is a fact about the data; only the copy is a
    /// fact about the toggle.
    static func state(trend: SubscriptionTrend?, metric: Metric) -> ContentState {
        guard let trend else { return .loading }
        guard !trend.days.isEmpty, trend.peakCost > 0 || trend.peakTokens > 0
        else { return .noUsage }
        let selected = metric == .cost ? trend.peakCost : Double(trend.peakTokens)
        return selected > 0 ? .chart : .metricUnavailable
    }

    private func placeholder(_ content: some View) -> some View {
        content
            .frame(maxWidth: .infinity, alignment: .leading)
            .frame(height: Self.chartHeight + 22, alignment: .topLeading)
    }

    private var subtitle: String? {
        guard let trend, let first = trend.days.first?.date else { return nil }
        return "since %@".localized(Format.monthDay(first))
    }

    private func value(_ bucket: SubscriptionTrend.Bucket) -> Double {
        metric == .cost ? bucket.cost : Double(bucket.tokens)
    }

    private func total(_ day: SubscriptionTrend.Day) -> Double {
        metric == .cost ? day.totalCost : Double(day.totalTokens)
    }

    /// One column per calendar day, stacked bottom-up in the fold's order so
    /// the largest payer is always the base and the bands do not reshuffle
    /// between refreshes.
    private func chart(_ trend: SubscriptionTrend) -> some View {
        GeometryReader { proxy in
            let width = proxy.size.width
            let columnWidth = max(
                1, (width - CGFloat(trend.days.count - 1) * Self.columnGap)
                    / CGFloat(max(trend.days.count, 1)))
            Canvas { context, size in
                // Kept even though the hovered column now lights up: a day
                // with no usage draws no column, and this band is then the only
                // thing that says which day the tooltip is describing.
                if let hoverIndex, trend.days.indices.contains(hoverIndex) {
                    let x = CGFloat(hoverIndex) * (columnWidth + Self.columnGap)
                    context.fill(
                        Path(CGRect(x: x - Self.columnGap / 2, y: 0,
                                    width: columnWidth + Self.columnGap, height: size.height)),
                        with: .color(.primary.opacity(0.07)))
                }
                for (index, day) in trend.days.enumerated() {
                    let dayTotal = total(day)
                    guard dayTotal > 0 else { continue }
                    let alpha = hoverIndex == index
                        ? Self.barHoverOpacity : Self.barOpacity
                    let x = CGFloat(index) * (columnWidth + Self.columnGap)
                    let fullHeight = max(
                        Self.minimumInk, size.height * CGFloat(dayTotal / peak))
                    var cursor = size.height
                    for target in ordered(trend) {
                        guard let bucket = day.byTarget[target] else { continue }
                        let share = value(bucket) / dayTotal
                        let segment = fullHeight * CGFloat(share)
                        guard segment > 0 else { continue }
                        cursor -= segment
                        context.fill(
                            // Rounded like `UsageChartCard`'s segments. Square
                            // blocks read as pasted-on next to the rest of the
                            // app's charts, whatever their opacity.
                            Path(roundedRect: CGRect(
                                    x: x, y: cursor, width: columnWidth, height: segment),
                                 cornerRadius: min(2, segment / 2)),
                            with: .color(color(target).opacity(alpha)))
                    }
                }
            }
            .contentShape(Rectangle())
            .onContinuousHover { phase in
                switch phase {
                case let .active(point):
                    let pitch = columnWidth + Self.columnGap
                    let index = pitch > 0 ? Int(point.x / pitch) : 0
                    guard trend.days.indices.contains(index) else { hoverIndex = nil; break }
                    hoverIndex = index
                    let chart = proxy.frame(in: .global)
                    hoverAnchorInCard = CGPoint(
                        x: CGFloat(index) * pitch + columnWidth + chart.minX - cardFrame.minX,
                        y: point.y + chart.minY - cardFrame.minY)
                case .ended:
                    hoverIndex = nil
                }
            }
        }
        .frame(height: Self.chartHeight)
    }

    @ViewBuilder
    private var tooltipLayer: some View {
        if let hoverIndex, let trend, trend.days.indices.contains(hoverIndex),
           cardFrame != .zero
        {
            let day = trend.days[hoverIndex]
            VStack(alignment: .leading, spacing: 4) {
                Text(Format.monthDay(day.date))
                    .font(.caption.weight(.semibold))
                if day.isEmpty {
                    Text("No usage this day")
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                } else {
                    // Largest first, matching the legend and `UsageChartCard`'s
                    // tooltip. Note the stack itself is largest-at-the-bottom,
                    // so this list is the reverse of the column's top-to-bottom
                    // order — consistency across tooltips wins over matching the
                    // one column being pointed at.
                    ForEach(ordered(trend), id: \.self) { target in
                        if let bucket = day.byTarget[target] {
                            HStack(spacing: 4) {
                                RoundedRectangle(cornerRadius: 1.5)
                                    .fill(color(target))
                                    .frame(width: 6, height: 6)
                                Text(name(target))
                                Spacer(minLength: 6)
                                Text(verbatim: metric == .cost
                                     ? Format.money(
                                        tokens: bucket.tokens, cost: bucket.cost)
                                     : Format.tokens(
                                        tokens: bucket.tokens, cost: bucket.cost))
                                    .foregroundStyle(.secondary)
                            }
                            .font(.caption2)
                        }
                    }
                    Divider().opacity(0.4)
                    HStack {
                        Text("Total")
                        Spacer()
                        // Both branches carry the same rule now. Only the
                        // cost side had it, so flipping the toggle turned a
                        // dash into a measured "0" for the same cost-only row.
                        Text(verbatim: metric == .cost
                             ? Format.money(
                                tokens: day.totalTokens, cost: day.totalCost)
                             : Format.tokens(
                                tokens: day.totalTokens, cost: day.totalCost))
                    }
                    .font(.caption2.weight(.medium))
                }
            }
            .padding(8)
            .frame(width: Self.tooltipWidth, alignment: .leading)
            .tooltipSurface()
            .onGeometryChange(for: CGSize.self) { $0.size } action: { tooltipSize = $0 }
            .offset(
                PopoverTooltipPlacement.offset(
                    anchor: hoverAnchorInCard,
                    tooltipSize: tooltipSize == .zero
                        ? CGSize(width: Self.tooltipWidth, height: 110) : tooltipSize,
                    containerFrame: cardFrame, viewport: viewport) ?? .zero)
            .allowsHitTesting(false)
        }
    }

    /// Ends only. At this width a label per column is unreadable, and the shape
    /// is what the card is for.
    private func axis(_ trend: SubscriptionTrend) -> some View {
        HStack {
            Text(verbatim: trend.days.first.map { Format.monthDay($0.date) } ?? "")
            Spacer()
            Text(verbatim: trend.days.last.map { Format.monthDay($0.date) } ?? "")
        }
        .font(.caption2)
        .foregroundStyle(.tertiary)
    }

    private func legend(_ trend: SubscriptionTrend) -> some View {
        HStack(spacing: 10) {
            ForEach(ordered(trend).prefix(4), id: \.self) { target in
                HStack(spacing: 4) {
                    RoundedRectangle(cornerRadius: 1.5)
                        .fill(color(target))
                        .frame(width: 6, height: 6)
                    Text(name(target))
                }
            }
            // Same overflow marker as `UsageChartCard`: without it a fifth
            // subscription draws a band no legend entry accounts for.
            if ordered(trend).count > 4 {
                Text(verbatim: "+\(ordered(trend).count - 4)")
                    .foregroundStyle(.tertiary)
            }
            Spacer()
        }
        .font(.caption2)
        .foregroundStyle(.secondary)
        .lineLimit(1)
    }

    /// Shown when every band is the unclassified one.
    ///
    /// The chart is not wrong in that state — the totals are real — but it is
    /// answering "how much did you spend" with a single grey block while
    /// claiming to answer "on which subscription". Most users have never opened
    /// that Settings page, so without this the card silently presents its least
    /// useful form as its normal one.
    @ViewBuilder
    private func undeclaredHint(_ trend: SubscriptionTrend) -> some View {
        if trend.targets == [SubscriptionTrendFold.unassignedTarget] {
            Text("Nothing is classified yet. Settings › Usage attribution splits this by subscription.")
                .font(.system(size: 9))
                .foregroundStyle(.tertiary)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    /// Subscription brand colours, so a band here matches that subscription
    /// everywhere else. Unclassified usage is deliberately grey rather than
    /// given a brand: it does not belong to anyone yet, and colouring it like a
    /// subscription would assert what the user has not declared.
    private func color(_ target: String) -> Color {
        // Solid grey, not `.secondary.opacity(0.45)`: that alpha multiplied
        // with the bar's own and put unclassified spend at an effective 0.22,
        // invisible at hover too, while the legend swatch drew at full 0.45 and
        // matched nothing on the chart. The fold deliberately keeps this band
        // so the totals agree; hiding it by alpha undoes that.
        target == SubscriptionTrendFold.unassignedTarget
            ? Color(hex: "#8e8e93")
            : Color(hex: ClientRegistry.style(target).color)
    }

    private func name(_ target: String) -> String {
        target == SubscriptionTrendFold.unassignedTarget
            ? "Unclassified".localized
            : ClientRegistry.style(target).displayName
    }
}
