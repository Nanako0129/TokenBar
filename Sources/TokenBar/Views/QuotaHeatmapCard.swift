import SwiftUI
import TokenBarCore

/// When the allowance actually gets spent, as a weekday-by-hour grid.
///
/// One window at a time, chosen from the picker. Summing windows was considered
/// and rejected: a client's session window and its weekly window are consumed
/// by the same messages, so adding them counts the same work twice, and
/// percentage points of two different subscriptions are not the same quantity
/// to begin with.
struct QuotaHeatmapCard: View {
    /// Every window that has a grid, heaviest first — NOT the strip card's
    /// summaries. Those exist only for windows with completed history, so a
    /// window whose only movement is in the cycle now running had a grid the
    /// picker could not reach, and the card said nothing was recorded.
    let windows: [QuotaHeatmapWindow]
    /// Keyed as `QuotaHeatmapWindow.id`.
    let heatmaps: [String: QuotaHeatmap]
    /// The per-window equivalence, keyed the same way. What turns a percentage
    /// into money — and the only reason this card can show money at all
    /// without a scan.
    var equivalences: [String: WindowEquivalence.Row] = [:]
    var attempted = true

    @AppStorage("tokenbar.heatmap.window") private var selectedRaw = ""
    @State private var cardFrame: CGRect = .zero
    @State private var hover: (weekday: Int, hour: Int)?
    @State private var hoverAnchorInCard: CGPoint = .zero
    @State private var tooltipSize: CGSize = .zero
    @Environment(\.popoverScrollViewport) private var viewport

    private static let rowHeight: CGFloat = 13
    private static let cellGap: CGFloat = 1
    private static let labelWidth: CGFloat = 18
    private static let tooltipWidth: CGFloat = 186
    /// Faintest and strongest fill for a slot that carries anything. An
    /// occupied slot never drops to invisible: "a little" and "nothing" are
    /// different answers and the grid has to keep them apart.
    private static let floorOpacity: Double = 0.14
    private static let peakOpacity: Double = 0.92
    /// Below this share of the peak a slot is drawn at the floor rather than
    /// proportionally, so a grid with one dominant hour still shows its quiet
    /// activity instead of 167 blank cells.
    private static let floorShare: Double = 0.06

    private var selected: QuotaHeatmapWindow? {
        // The list already excludes empty grids and leads with the heaviest, so
        // the fallback is simply "the first one" rather than a search for one
        // with something to draw.
        windows.first { $0.id == selectedRaw } ?? windows.first
    }

    private var grid: QuotaHeatmap? { selected.flatMap { heatmaps[$0.id] } }

    var body: some View {
        DashCard("When the allowance goes", subtitle: subtitle) {
            if windows.count > 1 { picker }
        } content: {
            if let grid, !grid.isEmpty {
                heatmap(grid)
                hourAxis
                footnote(grid)
            } else if let grid, grid.hasMovement {
                // Movement WAS recorded — every pair of readings was simply too
                // far apart to place it on a grid, which leaves `total` at zero.
                // Falling through to "nothing recorded yet" stated the opposite
                // of the truth, and hid the one line that explains it.
                Text("%@%% consumed, but every pair of readings was too far apart to place on the grid. It fills in as sampling gets denser."
                    .localized(Self.percent(grid.unplacedPercent)))
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
            } else if attempted {
                Text("No allowance movement recorded yet. It accumulates as TokenBar runs.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
            } else {
                LoadingLine(title: "Reading quota history…")
            }
        }
        .overlay(alignment: .topLeading) { tooltipLayer }
        .onGeometryChange(for: CGRect.self) { $0.frame(in: .global) } action: { cardFrame = $0 }
        .zIndex(hover == nil ? 0 : 1)
    }

    private var subtitle: String? {
        guard let grid, !grid.isEmpty else { return nil }
        return "%@ days observed".localized(String(grid.observedDays))
    }

    /// A menu rather than a segmented control: window names are long enough
    /// that four of them do not fit across the popover, and this card is not
    /// the place to abbreviate a subscription's own label.
    private var picker: some View {
        Menu {
            ForEach(windows) { window in
                Button {
                    selectedRaw = window.id
                } label: {
                    Text(verbatim: Self.label(window))
                }
            }
        } label: {
            HStack(spacing: 3) {
                Text(verbatim: selected.map(Self.label) ?? "")
                    .lineLimit(1)
                Image(systemName: "chevron.up.chevron.down")
                    .font(.system(size: 6))
            }
            // Below the subtitle's `.caption` and well below the 13pt title:
            // this is a control label, not content. Weight matters as much as
            // size here — dropping it to secondary is what stops it reading as
            // a second heading beside the real one.
            .font(.system(size: 8))
            .foregroundStyle(.secondary)
        }
        .menuStyle(.borderlessButton)
        .controlSize(.mini)
        .fixedSize()
    }

    /// The picker's row and current-selection text, qualified by account the
    /// same way `QuotaSummaryLine.tightestName` is — see its doc comment for
    /// why the client's display name alone leaves two accounts' windows
    /// indistinguishable in the menu.
    ///
    /// `static`, and a named function rather than composed inline, so `M3-n`
    /// can assert the string directly.
    static func label(_ window: QuotaHeatmapWindow) -> String {
        let style = ClientRegistry.style(window.clientId)
        let account = AccountIdentity(
            clientId: window.clientId, accountKey: window.accountKey
        ).accountLabel
        let name = [style.displayName, account].compactMap(\.self).joined(separator: " ")
        return "\(name) · \(window.windowLabel.localized)"
    }

    private func heatmap(_ grid: QuotaHeatmap) -> some View {
        GeometryReader { proxy in
            let width = proxy.size.width - Self.labelWidth
            let cellWidth = max(1, (width - 23 * Self.cellGap) / 24)
            HStack(spacing: 0) {
                VStack(spacing: Self.cellGap) {
                    ForEach(0..<7, id: \.self) { weekday in
                        Text(Self.weekdayLabels[weekday].localized)
                            .font(.system(size: 8))
                            .foregroundStyle(.tertiary)
                            .frame(width: Self.labelWidth,
                                   height: Self.rowHeight, alignment: .leading)
                    }
                }
                Canvas { context, size in
                    for weekday in 0..<7 {
                        for hour in 0..<24 {
                            let value = grid.cells[weekday][hour]
                            let rect = CGRect(
                                x: CGFloat(hour) * (cellWidth + Self.cellGap),
                                y: CGFloat(weekday) * (Self.rowHeight + Self.cellGap),
                                width: cellWidth, height: Self.rowHeight)
                            let isHovered = hover?.weekday == weekday && hover?.hour == hour
                            context.fill(
                                Path(roundedRect: rect, cornerRadius: 2),
                                with: .color(fill(value, peak: grid.peak,
                                                  hovered: isHovered)))
                        }
                    }
                    _ = size
                }
                .contentShape(Rectangle())
                .onContinuousHover { phase in
                    switch phase {
                    case let .active(point):
                        let hour = Int(point.x / (cellWidth + Self.cellGap))
                        let weekday = Int(point.y / (Self.rowHeight + Self.cellGap))
                        guard (0..<24).contains(hour), (0..<7).contains(weekday)
                        else { hover = nil; break }
                        hover = (weekday, hour)
                        let canvas = proxy.frame(in: .global)
                        hoverAnchorInCard = CGPoint(
                            x: CGFloat(hour) * (cellWidth + Self.cellGap) + cellWidth
                                + Self.labelWidth + canvas.minX - cardFrame.minX,
                            y: CGFloat(weekday) * (Self.rowHeight + Self.cellGap)
                                + Self.rowHeight / 2 + canvas.minY - cardFrame.minY)
                    case .ended:
                        hover = nil
                    }
                }
            }
        }
        .frame(height: 7 * Self.rowHeight + 6 * Self.cellGap)
        .panelCrossfade(selectedRaw)
    }

    /// Occupied slots ramp between a floor and the peak; an empty one gets a
    /// near-invisible plate so the grid reads as a grid rather than as floating
    /// squares.
    private func fill(_ value: Double, peak: Double, hovered: Bool) -> Color {
        guard value > 0, peak > 0 else {
            return .primary.opacity(hovered ? 0.16 : 0.05)
        }
        let share = value / peak
        let opacity = share <= Self.floorShare
            ? Self.floorOpacity
            : Self.floorOpacity
                + (Self.peakOpacity - Self.floorOpacity)
                * ((share - Self.floorShare) / (1 - Self.floorShare))
        return .accentColor.opacity(hovered ? min(1, opacity + 0.25) : opacity)
    }

    private static let weekdayLabels =
        ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]

    private var hourAxis: some View {
        HStack(spacing: 0) {
            Spacer().frame(width: Self.labelWidth)
            ForEach([0, 6, 12, 18], id: \.self) { hour in
                Text(verbatim: "\(hour)")
                    .frame(maxWidth: .infinity, alignment: .leading)
                if hour == 18 { Spacer(minLength: 0) }
            }
        }
        .font(.system(size: 8))
        .foregroundStyle(.tertiary)
    }

    @ViewBuilder
    private func footnote(_ grid: QuotaHeatmap) -> some View {
        if grid.unplacedPercent >= 1 {
            // Stated, not swallowed: consumption we could not place in time is
            // consumption the grid is not showing, and a reader comparing the
            // grid's total against the strip card above would otherwise find a
            // gap with no explanation.
            Text("%@%% consumed between readings too far apart to place"
                .localized(String(Int(grid.unplacedPercent.rounded()))))
                .font(.system(size: 9))
                .foregroundStyle(.tertiary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    @ViewBuilder
    private var tooltipLayer: some View {
        if let hover, let grid, let selected, cardFrame != .zero {
            let value = grid.cells[hover.weekday][hover.hour]
            VStack(alignment: .leading, spacing: 3) {
                Text(verbatim: Self.weekdayLabels[hover.weekday].localized
                     + String(format: " %02d:00", hover.hour))
                    .font(.caption2.weight(.semibold))
                if value <= 0 {
                    Text("No allowance consumed in this slot")
                        .font(.system(size: 9))
                        .foregroundStyle(.tertiary)
                } else {
                    // Points, not "% of the allowance". A slot accumulates
                    // across every recorded cycle, so with enough history it
                    // passes 100 and the percentage has no denominator left to
                    // be a percentage of. What the number honestly is: how many
                    // percentage points of allowance were spent in this slot,
                    // summed over the window's whole recorded history.
                    Text("%@ allowance-points spent here".localized(Self.percent(value)))
                        .font(.caption2)
                    equivalent(value, row: equivalences[selected.id])
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
                        ? CGSize(width: Self.tooltipWidth, height: 84) : tooltipSize,
                    containerFrame: cardFrame, viewport: viewport) ?? .zero)
            .allowsHitTesting(false)
        }
    }

    /// Converted from the window's own equivalence rather than measured.
    ///
    /// The measured route would need a message scan over the whole recorded
    /// history, which this project has already paid 60 seconds for once. The
    /// equivalence is derived from attributed spend against quota movement, so
    /// the figures below are attributed too — they are an estimate, and they
    /// say so, carrying the same error the history card publishes.
    @ViewBuilder
    private func equivalent(_ percent: Double, row: WindowEquivalence.Row?) -> some View {
        if case let .tokensOnly(tokensPerTenth, errorPercent) = row {
            // The fold produces this deliberately when the models carry no
            // price; matching only `.ratio` sent it to "not enough history",
            // which is false — the history is sufficient and the token estimate
            // is the part of the answer that exists.
            let share = percent / 10
            Text(verbatim: "≈ " + Format.compactTokens(
                WindowEquivalence.clamped((Double(tokensPerTenth) * share).rounded())))
                .font(.caption2)
                .foregroundStyle(.secondary)
            Text("unpriced models, ±%@%%".localized(String(errorPercent)))
                .font(.system(size: 9))
                .foregroundStyle(.tertiary)
                .fixedSize(horizontal: false, vertical: true)
        } else if case let .costOnly(costPerTenth, errorPercent) = row {
            // The mirror of `.tokensOnly`, and it fell through to "not enough
            // history" for the same wrong reason: the history IS sufficient,
            // and the money estimate is the part of the answer that exists.
            // `usdOrBelowCent`, not `money`: this branch has no token count
            // to pair — that is what makes it cost-only — and `.costOnly` is
            // constructed only from positive cost, so the exact-zero case
            // cannot arise. What can is sub-cent: the slot's share of a tenth
            // shrinks without bound, a 0.05-point cell still draws, and an
            // ordinary `costPerTenth` scaled by it lands under half a cent.
            Text(verbatim: "~ " + Format.usdOrBelowCent(costPerTenth * (percent / 10)))
                .font(.caption2)
                .foregroundStyle(.secondary)
            Text("tokens unavailable, ±%@%%".localized(String(errorPercent)))
                .font(.system(size: 9))
                .foregroundStyle(.tertiary)
                .fixedSize(horizontal: false, vertical: true)
        } else if case let .ratio(tokensPerTenth, costPerTenth, errorPercent) = row {
            let share = percent / 10
            // Clamped: the ratio itself can already be saturated, and a cell
            // above 10% scales it further, past what `Int64(_:)` accepts.
            Text(verbatim: "≈ " + Format.compactTokens(
                WindowEquivalence.clamped((Double(tokensPerTenth) * share).rounded())))
                .font(.caption2)
                .foregroundStyle(.secondary)
            Text("~ %@ API-equivalent, ±%@%%".localized(
                Format.usdOrBelowCent(costPerTenth * share), String(errorPercent)))
                .font(.system(size: 9))
                .foregroundStyle(.tertiary)
                .fixedSize(horizontal: false, vertical: true)
        } else if case let .spread(lowPerTenth, highPerTenth, lowCost, highCost) = row {
            // The third sibling of the two branches above, missed when they were
            // written. `.spread` is a measured answer — the cycles disagree, so
            // the fold reports a range instead of a point — and the strip card
            // prints exactly that range for the same row. Falling through made
            // one card say "not enough history" about a window the card beside
            // it was quantifying.
            let share = percent / 10
            Text(verbatim: "≈ "
                + Format.compactTokens(
                    WindowEquivalence.clamped((Double(lowPerTenth) * share).rounded()))
                + "–"
                + Format.compactTokens(
                    WindowEquivalence.clamped((Double(highPerTenth) * share).rounded())))
                .font(.caption2)
                .foregroundStyle(.secondary)
            Text("~ %@ – %@ API-equivalent".localized(
                Format.usdOrBelowCent(lowCost * share),
                Format.usdOrBelowCent(highCost * share)))
                .font(.system(size: 9))
                .foregroundStyle(.tertiary)
                .fixedSize(horizontal: false, vertical: true)
        } else {
            Text(Self.noFigureReason(row).localized)
                .font(.system(size: 9))
                .foregroundStyle(.tertiary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    /// Why there is no figure, stated per case rather than blamed on history.
    ///
    /// "Not enough history" used to cover everything that reached here, and it
    /// is a false sentence for three of them: `.insufficient`, `.notMoved` and
    /// `.unaccounted` all have sufficient history and fail for a different
    /// reason each. That is the same defect as sending `.spread` here — copy
    /// answering a narrower question than the state it describes — with the
    /// difference that no number is being hidden, only a wrong reason given.
    static func noFigureReason(_ row: WindowEquivalence.Row?) -> String {
        switch row {
        case .undeclared:
            return "Classify your usage in Settings to see what this window is worth"
        case .notMoved:
            return "The allowance did not move in this window"
        case .insufficient:
            return "The allowance moved too little to convert reliably"
        case .unaccounted:
            return "The allowance moved, but no usage was recorded on this machine"
        default:
            return "Not enough history to convert this window to a figure"
        }
    }

    /// One decimal below 10, none above: a 0.4% slot and an idle one must not
    /// both render as "0%".
    static func percent(_ value: Double) -> String {
        value < 10
            ? String(format: "%.1f", value)
            : String(Int(value.rounded()))
    }
}
