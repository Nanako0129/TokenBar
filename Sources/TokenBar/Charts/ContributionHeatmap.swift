import SwiftUI
import TokenBarCore

// Flat, Sunday-first contribution heatmap. Reads the exact same
// `GridLayout` UsageChartCard builds for ContributionGraph3D (same
// `buildGrid(year:perDayMap:)` call over `stats.perDayMap`) — this is only a
// different renderer over identical data (FLAT-HEATMAP invariant 2), not a
// second aggregation path.

/// Visual parameters as named constants so a design-review pass only touches
/// one line (FLAT-HEATMAP invariant 9).
enum HeatmapLayout {
    static let cell: CGFloat = 11
    static let gap: CGFloat = 3
    static let cornerRadius: CGFloat = 2
    static let monthLabelHeight: CGFloat = 14
    static let step: CGFloat = cell + gap
    /// Round 6, FIX 2: extra trailing width reserved only when the LAST
    /// renderable column carries a month label. Labels are leading-aligned
    /// and wider than an 11pt cell, so a label landing in the very last
    /// column (any month's first week — happens roughly monthly) would
    /// otherwise have its trailing half clipped by the ScrollView once
    /// scrolled to its trailing edge. Sized for the widest localized month
    /// abbreviation, not just English "Dec" — zh-Hant's "12月" is wider.
    static let lastColumnLabelMargin: CGFloat = 28

    /// Left edge of the grid. The ring reaches `hoverRingReach` outward on
    /// every side, so the first column needs that much room before x = 0 or
    /// the ScrollView clips its highlight. `contentWidth` reserves the same
    /// at the trailing end — which is where it matters most, since the view
    /// opens scrolled to the most recent day.
    static var gridLeading: CGFloat { hoverRingReach }

    /// Top of the grid. Not `monthLabelHeight`: the content reserves
    /// `hoverRingReach` above the first row (and the same below the last) so a
    /// hover ring on an edge row is not cut off by the content frame. The
    /// month labels keep their own position, above this.
    static var gridTop: CGFloat { monthLabelHeight + hoverRingReach }

    /// One source of cell geometry for the draw loop, the hover ring, the hit
    /// test and the tooltip anchor, so none of them can drift from the others.
    static func rect(col: Int, row: Int) -> CGRect {
        CGRect(
            x: gridLeading + CGFloat(col) * step,
            y: gridTop + CGFloat(row) * step,
            width: cell, height: cell)
    }

    /// Hover ring, matching the bar chart's treatment of the hovered bar
    /// (`UsageChartCard`: a 1pt primary stroke plus a soft primary shadow).
    ///
    /// The glow radius is deliberately smaller than the bars' 3. A bar is tall
    /// and stands alone; a cell is 11pt with only `gap` (3pt) to its
    /// neighbours, so a 3pt halo reaches every surrounding cell and the
    /// highlight reads as a blob rather than an edge. 1.5 keeps it inside the
    /// gap. Scale this with `cell`/`gap`, not with the bars' value.
    static let hoverStrokeWidth: CGFloat = 1
    static let hoverStrokeOpacity: Double = 0.85
    static let hoverGlowRadius: CGFloat = 1.5
    static let hoverGlowOpacity: Double = 0.65

    /// The ring sits in the gap, not on the cell edge, and this is what makes
    /// it legible at all. A fixed `Color.primary` ring drawn on the cell has
    /// no constant backdrop: level 0 is `primary` at 0.05 and level 4 is a
    /// bright near-white blue, so in dark mode a white ring vanishes on the
    /// busiest days — the exact days worth pointing at. Pushed into the gap it
    /// meets the popover background instead, which is the same everywhere.
    ///
    /// This is also why the bar chart's ring works without any such trick:
    /// most of a bar ring's perimeter already runs along the chart background
    /// rather than across the bar's own fill. Same idea, different geometry.
    ///
    /// 1pt out of the 3pt gap, leaving the neighbouring cells untouched.
    static let hoverRingInset: CGFloat = -1

    /// How far the ring reaches beyond its cell: the outward inset, half the
    /// stroke straddling the path, and the glow. The content reserves this
    /// above the first row and below the last, otherwise a ring on an edge row
    /// is clipped by the content frame (the bottom row's ring landed 1pt past
    /// it, and the glow 1.5pt past that).
    static var hoverRingReach: CGFloat { -hoverRingInset + hoverStrokeWidth / 2 + hoverGlowRadius }

    /// Five-level intensity ramp. Thresholds ported from token-monitor's
    /// `heatmapIntensity()` (usageCharts.js:60-65: `>=0.75→4`, `>=0.5→3`,
    /// `>=0.25→2`, `>0→1`, else `0`); colors ported from its `.heat.lvl-*`
    /// rules (dashboard.css:98-102), a blue ramp climbing opacity 0→1.
    private static let levelFills: [Color] = [
        Color.primary.opacity(0.05),
        Color(red: 90 / 255, green: 170 / 255, blue: 255 / 255).opacity(0.22),
        Color(red: 120 / 255, green: 190 / 255, blue: 255 / 255).opacity(0.5),
        Color(red: 150 / 255, green: 210 / 255, blue: 255 / 255).opacity(0.82),
        Color(red: 180 / 255, green: 230 / 255, blue: 255 / 255),
    ]

    static func level(value: Double, max: Double) -> Int {
        guard max > 0, value > 0 else { return 0 }
        let ratio = value / max
        if ratio >= 0.75 { return 4 }
        if ratio >= 0.5 { return 3 }
        if ratio >= 0.25 { return 2 }
        return 1
    }

    static func fill(level: Int) -> Color {
        levelFills[max(0, min(level, levelFills.count - 1))]
    }
}

/// Month abbreviations for the header row above the grid. Reuses the same
/// "Jan"…"Dec" localization keys as `Format.monthDay` (see zh-Hant.lproj)
/// rather than reaching into Format's private array.
private let monthAbbrev = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun",
    "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
]

/// The full-year flat heatmap. Tokens/Price only — no Model/Agent stacking
/// (a single color-ramped cell has nowhere to stack).
struct ContributionHeatmap: View {
    let grid: TokenBarCore.GridLayout
    let metric: ChartMetric
    /// The year `grid` was built for — needed only to decide the future-day
    /// cutoff (round 2, item 3); `buildGrid` itself is untouched.
    let year: String

    /// The hovered cell's `col * 7 + row` index into `grid.cells` — not the
    /// `GridCell` value itself. Round 7: `DashboardModel.pollGraph()` re-
    /// fetches the graph payload every 60s while the popover stays open, so
    /// `grid` (and the tokens/cost a hovered date carries) can change under
    /// a mouse that never moves. Storing an index and re-resolving the cell
    /// fresh on every access (`hoverCell` below) — the same live-lookup shape
    /// `UsageChartCard`'s own `hoverIndex`-into-`bars` already uses for its
    /// 2D bar chart — keeps the tooltip's numbers current instead of frozen
    /// at whatever they were the instant hovering began.
    @State private var hoverIndex: Int?
    @State private var tooltipSize: CGSize = .zero
    /// The scrolling content's on-screen origin, tracked so the tooltip
    /// (rendered in an overlay *outside* the horizontal ScrollView, see
    /// `body`) can find a hovered cell's true screen position instead of one
    /// pinned to the clipped, scrolled-past content frame.
    @State private var contentOrigin: CGPoint = .zero
    @Environment(\.popoverScrollViewport) private var popoverScrollViewport

    private static let tooltipWidth: CGFloat = 170
    private static let contentID = "heatmap-content"

    // MARK: - Pure per-metric data (no Calendar/TimeZone/Date — ISODay only)

    /// Raw metric value for a cell. Internal (not `private`) so SelfTest can
    /// exercise it directly.
    static func value(_ cell: GridCell, metric: ChartMetric) -> Double {
        metric == .cost ? cell.cost : Double(cell.tokens)
    }

    /// "This day has data" per metric — FLAT-HEATMAP invariant 3: never
    /// `cell.active` (that's tokens-only; Grid.swift:49), because a day can
    /// carry `cost > 0` with `tokens == 0` (UsageStats.swift:105-110).
    static func hasData(_ cell: GridCell, metric: ChartMetric) -> Bool {
        cell.inYear && value(cell, metric: metric) > 0
    }

    /// Per-metric intensity denominator, computed here rather than reusing
    /// `GridLayout.maxTokens` (invariant 4 forbids modifying/extending
    /// `GridLayout.maxTokens`, not forbids simply not using it) — `maxTokens`
    /// only ever tracks tokens and never applies the future-day cutoff, so
    /// Price needs its own max regardless, and round 4's FIX 3 makes tokens
    /// take the same `cutoff`-filtered path: a hidden future cell (clock
    /// skew, an imported session dated past today) must not sit in the
    /// denominator any more than it's drawn or hoverable — the same
    /// `isRenderable` judgment used everywhere else a cell "counts".
    static func maxValue(_ grid: TokenBarCore.GridLayout, metric: ChartMetric, cutoff: String) -> Double {
        switch metric {
        case .tokens:
            return grid.cells.reduce(0.0) { isRenderable($1, cutoff: cutoff) ? max($0, Double($1.tokens)) : $0 }
        case .cost:
            return grid.cells.reduce(0.0) { isRenderable($1, cutoff: cutoff) ? max($0, $1.cost) : $0 }
        }
    }

    /// Round 2, item 3(a): the last day this heatmap should draw. Round 5:
    /// a single `min` over the ISO date strings (whose lexicographic order
    /// is chronological order) correctly covers all three year cases at
    /// once — a past year clips at its own Dec 31, the current year clips at
    /// today, and a FUTURE year (reachable if clock skew or an imported
    /// session put data there, making it show up in the year picker) also
    /// clips at today, i.e. renders nothing. The two-way `year ==
    /// currentYear ? today : "\(year)-12-31"` this replaced treated any
    /// non-current year as past, so a future year rendered its entire (blank
    /// data-wise but still drawn) 12 months — every day of it is, after all,
    /// still in the future. View-level only; `buildGrid` still produces the
    /// full padded year.
    static func cutoffDate(year: String, today: String) -> String {
        min("\(year)-12-31", today)
    }

    /// A cell is drawn/hoverable only if it's in-year and on or before the
    /// cutoff — i.e. never a future day in the current year.
    static func isRenderable(_ cell: GridCell, cutoff: String) -> Bool {
        cell.inYear && cell.date <= cutoff
    }

    /// Round 3: the last column that has any renderable cell. Layout width,
    /// month labels, and hit-testing all derive from this — never from
    /// `grid.cols` — so a future day (round 2's `isRenderable` correctly
    /// excludes it from drawing/hover, but round 2 left `grid.cols` driving
    /// the layout width, so cutoff-past columns still ate blank space and
    /// `scrollTo(.trailing)` landed past the actual last day) occupies no
    /// layout width at all. `isRenderable` stays the single judgment call;
    /// this only reduces it to a column number instead of inventing a
    /// separate "visible range" concept.
    static func lastRenderableCol(_ grid: TokenBarCore.GridLayout, cutoff: String) -> Int {
        grid.cells.filter { isRenderable($0, cutoff: cutoff) }.map(\.col).max() ?? -1
    }

    /// Round 5: canvas width for however many columns are actually
    /// renderable — 0 when nothing is (a future selected year now that
    /// `cutoffDate` clips it to today), never a negative width from naively
    /// computing `0 * step - gap`. Round 6, FIX 2: adds
    /// `HeatmapLayout.lastColumnLabelMargin` when (and only when) the last
    /// renderable column itself carries a month label, so that label isn't
    /// clipped at the trailing edge; every other case is unchanged. Static
    /// so SelfTest can exercise both edges directly instead of instantiating
    /// the view.
    static func contentWidth(visibleCols: Int, monthLabelCols: [(col: Int, label: String)]) -> CGFloat {
        guard visibleCols > 0 else { return 0 }
        // Derived from the last cell's own rect rather than recomputing the
        // column arithmetic, so "the content is wide enough for the trailing
        // ring" holds by construction instead of by two formulas agreeing.
        let base = HeatmapLayout.rect(col: visibleCols - 1, row: 0).maxX
            + HeatmapLayout.hoverRingReach
        let lastCol = visibleCols - 1
        return monthLabelCols.contains { $0.col == lastCol }
            ? base + HeatmapLayout.lastColumnLabelMargin
            : base
    }

    /// The columns that get a month-name header — the same `isRenderable`
    /// cutoff as the cell drawing loop, so a month past the cutoff gets no
    /// label any more than it gets cells. Static (not a private computed
    /// property) so SelfTest can exercise the exact production filter rather
    /// than a hand-rebuilt copy of it.
    static func monthLabelCols(grid: TokenBarCore.GridLayout, cutoff: String) -> [(col: Int, label: String)] {
        grid.cells
            .filter { isRenderable($0, cutoff: cutoff) && $0.date.hasSuffix("-01") }
            .compactMap { cell in
                let monthChars = cell.date.dropFirst(5).prefix(2)
                guard let month = Int(monthChars), (1...12).contains(month) else { return nil }
                return (cell.col, monthAbbrev[month - 1].localized)
            }
    }

    /// Round 6, FIX 3: whether a 1D offset within one column/row's `step`
    /// (cell + gap) stride lands inside the cell itself rather than the gap
    /// between cells. Gap coordinates used to fall through to
    /// `Int(offset / step)`'s floor division, which silently attributed them
    /// to the PRECEDING cell — putting the cursor on what looks like a blank
    /// gridline popped a tooltip naming the adjacent day instead. This is
    /// deliberately different from `UsageChartGeometry.barIndex`, whose doc
    /// comment says gap pixels attach to the left bar on purpose (a 1D bar
    /// chart, avoiding a dead zone between tall thin bars); this is a 2D
    /// discrete date grid where the gaps are visible gridlines and the
    /// tooltip names one specific date, so the gaps should be dead zones —
    /// `UsageChartGeometry` itself is untouched.
    static func withinCell(offset: CGFloat, step: CGFloat, cell: CGFloat) -> Bool {
        let local = offset.truncatingRemainder(dividingBy: step)
        return local >= 0 && local < cell
    }

    /// Round 7: whether a hover should be cleared given the content's old
    /// and new on-screen origin. True only when it actually moved.
    ///
    /// Rejected alternative: re-project the LAST known cursor point through
    /// the new origin and re-resolve whatever cell now sits under it. That
    /// needs a second piece of state (the last raw pointer position) and a
    /// second coordinate-math path alongside `cellAt`'s — one more thing to
    /// keep in sync and one more place a sign or offset error can hide.
    /// Clearing is simpler and, for a grid the user is actively scrolling,
    /// the reasonable behavior anyway: the tooltip disappears while the
    /// content moves and reappears the instant the pointer moves again
    /// (which `.onContinuousHover`'s `.active` phase already does).
    ///
    /// In production, `onGeometryChange`'s own `action` closure is already
    /// only invoked when its (`Equatable`) transformed value changes —
    /// verified against Apple's documentation for `onGeometryChange(for:
    /// of:action:)`, not assumed — so this is always true whenever that
    /// action fires at all. It's still factored out and applied explicitly
    /// (not left as an implicit platform guarantee) so SelfTest can verify
    /// the "unchanged origin never clears" half on its own, independent of
    /// that platform behavior.
    static func shouldClearHoverOnOriginChange(old: CGPoint, new: CGPoint) -> Bool {
        old != new
    }

    /// Round 2, item 1: the tooltip's anchor in the OUTER (non-scrolling)
    /// container's coordinate space, derived from the hovered cell's
    /// position in the scrolling content plus that content's current origin
    /// RELATIVE TO THE CONTAINER. This is what lets the tooltip escape the
    /// horizontal ScrollView's clip: an anchor pinned to the scrolled
    /// content's own local frame is exactly the bug being fixed.
    ///
    /// Round 8 (perf): simplified from a 3-argument
    /// `(cellCenter, contentOrigin, containerOrigin)` — `contentOrigin` is
    /// now measured directly in a coordinate space anchored to the
    /// container (see `body`'s `.coordinateSpace`), so it already IS
    /// "relative to the container"; the separate subtraction this used to
    /// do is now done for free by the coordinate space itself. The
    /// 3-argument form required tracking `contentOrigin` in `.global` space,
    /// which changes on every frame of an ANCESTOR's vertical scroll (not
    /// just this view's own horizontal one) even though the subtracted
    /// RESULT never does — see `body`'s `.coordinateSpace` comment for the
    /// full story.
    static func tooltipAnchor(cellCenter: CGPoint, contentOrigin: CGPoint) -> CGPoint {
        CGPoint(x: cellCenter.x + contentOrigin.x, y: cellCenter.y + contentOrigin.y)
    }

    /// Round 8 (perf): everything derived from `grid`/`metric`/`year`/today
    /// that's expensive to recompute — `Format.todayKey()` builds a
    /// `DateFormatter` per call (Format.swift:32-38, not touched here: a
    /// cached `static let` formatter would freeze `.timeZone = .current`,
    /// conflicting with issue #127's timezone semantics, out of scope for
    /// this PR), and `lastRenderableCol`/`monthLabelCols`/`maxValue` each
    /// walk every cell in `grid.cells` (~371 for a 53-column year). These
    /// used to be six-plus independent zero-argument computed properties,
    /// each re-deriving `cutoff` (and therefore rebuilding a
    /// `DateFormatter`) and re-walking the cell array on every access — and
    /// `contentWidth` read `monthLabelCols` a SECOND time on top of the
    /// canvas draw loop's own read. Computed exactly once per `body`
    /// evaluation via `makeRenderState()` and threaded down as a plain
    /// value instead. This is NOT a cache — nothing persists between
    /// renders, no `ObservableObject`, no stored property; it's the same
    /// work, just grouped into one pass instead of scattered redundant ones.
    private struct RenderState {
        let cutoff: String
        let visibleCols: Int
        let monthLabelCols: [(col: Int, label: String)]
        let metricMax: Double
        let contentWidth: CGFloat
    }

    private func makeRenderState() -> RenderState {
        let cutoff = Self.cutoffDate(year: year, today: Format.todayKey())
        let visibleCols = max(0, Self.lastRenderableCol(grid, cutoff: cutoff) + 1)
        let monthLabelCols = Self.monthLabelCols(grid: grid, cutoff: cutoff)
        let metricMax = Self.maxValue(grid, metric: metric, cutoff: cutoff)
        let contentWidth = Self.contentWidth(visibleCols: visibleCols, monthLabelCols: monthLabelCols)
        return RenderState(
            cutoff: cutoff, visibleCols: visibleCols, monthLabelCols: monthLabelCols,
            metricMax: metricMax, contentWidth: contentWidth)
    }

    /// The hovered cell, resolved fresh from the CURRENT `grid` on every
    /// access rather than a value snapshotted once at hover-set time — see
    /// `hoverIndex`'s doc comment. Re-applies `isRenderable`/`hasData` too,
    /// for the same reason: cheap, and consistent with `cellAt` treating
    /// them as the one live judgment call for "does this cell count".
    private func hoverCell(_ state: RenderState) -> GridCell? {
        guard let hoverIndex, grid.cells.indices.contains(hoverIndex) else { return nil }
        let cell = grid.cells[hoverIndex]
        guard Self.isRenderable(cell, cutoff: state.cutoff), Self.hasData(cell, metric: metric) else { return nil }
        return cell
    }

    private var gridHeight: CGFloat { 7 * HeatmapLayout.step - HeatmapLayout.gap }
    private var contentHeight: CGFloat {
        HeatmapLayout.gridTop + gridHeight + HeatmapLayout.hoverRingReach
    }

    /// Round 8 (perf): named coordinate space anchored to the OUTER
    /// (non-scrolling) container — the same view `outerGeo` describes.
    /// `canvasBody`'s `.onGeometryChange` measures the scrolling content's
    /// origin IN THIS SPACE instead of `.global`.
    ///
    /// Why: the dashboard's own vertical ScrollView is an ANCESTOR of this
    /// entire view. When it scrolls, this heatmap's `.global` position
    /// shifts every single frame — and so does the outer container's. Their
    /// DIFFERENCE (all `tooltipAnchor` ever needed) does not change; only
    /// the two individual `.global` values do. Tracking `.global` meant
    /// paying full price — a `contentOrigin` write, a `hoverIndex = nil`
    /// write (round 7's fix, correctly reacting to what looked like a
    /// change), and the resulting `body` re-evaluation (which used to
    /// rebuild ~8 `DateFormatter`s via `cutoff` and re-walk ~371 cells
    /// several times over) — on every frame of a scroll that has nothing to
    /// do with this view at all. Measuring in a space anchored to the
    /// container reports the already-subtracted, invariant difference
    /// directly, so a vertical ancestor scroll no longer changes the
    /// tracked value, writes no state, and triggers no re-render.
    private static let coordinateSpaceName = "heatmap-container"

    var body: some View {
        VStack(spacing: 0) {
            Spacer(minLength: 0)
            if grid.cells.isEmpty {
                Color.clear
            } else {
                let state = makeRenderState()
                GeometryReader { outerGeo in
                    ScrollViewReader { proxy in
                        ScrollView(.horizontal, showsIndicators: false) {
                            canvasBody(state)
                                .id(Self.contentID)
                        }
                        // Round 2, item 3(b): land on the most recent columns
                        // on open, GitHub-style, instead of Jan 1. Round 4,
                        // FIX 1: `onAppear` alone only fires once — changing
                        // the dashboard's year filter while already on the
                        // Heatmap tab updates this same view in place rather
                        // than re-inserting it, so a stale horizontal offset
                        // (e.g. left over from a wider current-year layout)
                        // stuck around after switching to a narrower or wider
                        // past year. `cutoff`, not `year`, is the trigger: it
                        // already covers both a year change AND a day
                        // rollover while the popover happens to stay open.
                        .onAppear { proxy.scrollTo(Self.contentID, anchor: .trailing) }
                        .onChange(of: state.cutoff) { _, _ in
                            proxy.scrollTo(Self.contentID, anchor: .trailing)
                        }
                    }
                    // Tooltip lives in the ScrollView's overlay, not its
                    // content — an overlay isn't clipped by the view it
                    // decorates, so this is what stops the tooltip being cut
                    // off at the heatmap's edge (round 2, item 1).
                    .overlay(alignment: .topLeading) {
                        if let cell = hoverCell(state) {
                            let anchor = Self.tooltipAnchor(
                                cellCenter: cellCenter(cell), contentOrigin: contentOrigin)
                            let measuredSize = tooltipSize == .zero
                                ? CGSize(width: Self.tooltipWidth, height: 60)
                                : tooltipSize
                            // Still `.global` here (unrelated to the
                            // scroll-perf fix above): `containerFrame` only
                            // needs to match `popoverScrollViewport`'s space
                            // for the viewport-clamp math, and reading it
                            // fresh at render time — rather than storing it
                            // via `@State`/`onGeometryChange` — costs nothing
                            // extra; it's not what was driving the
                            // re-render cascade.
                            let offset = PopoverTooltipPlacement.offset(
                                anchor: anchor,
                                tooltipSize: measuredSize,
                                containerFrame: outerGeo.frame(in: .global),
                                viewport: popoverScrollViewport)
                            tooltip(cell)
                                .offset(offset ?? .zero)
                        }
                    }
                }
                .coordinateSpace(name: Self.coordinateSpaceName)
                .frame(height: contentHeight)
            }
            Spacer(minLength: 0)
        }
    }

    private func canvasBody(_ state: RenderState) -> some View {
        ZStack(alignment: .topLeading) {
            Canvas { context, _ in
                for (col, label) in state.monthLabelCols {
                    context.draw(
                        Text(label).font(.caption2).foregroundStyle(.secondary),
                        at: CGPoint(
                            x: HeatmapLayout.gridLeading + CGFloat(col) * HeatmapLayout.step,
                            y: HeatmapLayout.monthLabelHeight / 2),
                        anchor: .leading)
                }
                for cell in grid.cells where Self.isRenderable(cell, cutoff: state.cutoff) {
                    let level = HeatmapLayout.level(value: Self.value(cell, metric: metric), max: state.metricMax)
                    let rect = HeatmapLayout.rect(col: cell.col, row: cell.row)
                    context.fill(
                        Path(roundedRect: rect, cornerRadius: HeatmapLayout.cornerRadius),
                        with: .color(HeatmapLayout.fill(level: level)))
                }
            }
            // Outside the Canvas on purpose, mirroring how the bar chart
            // draws its hovered-bar highlight: a ring drawn inside the
            // Canvas would make every hover move repaint all ~370 cells,
            // which is the cascade round 8 removed from the scroll path.
            if let cell = hoverCell(state) {
                let rect = HeatmapLayout.rect(col: cell.col, row: cell.row)
                    .insetBy(dx: HeatmapLayout.hoverRingInset, dy: HeatmapLayout.hoverRingInset)
                RoundedRectangle(
                    cornerRadius: HeatmapLayout.cornerRadius - HeatmapLayout.hoverRingInset)
                    .stroke(
                        Color.primary.opacity(HeatmapLayout.hoverStrokeOpacity),
                        lineWidth: HeatmapLayout.hoverStrokeWidth)
                    .frame(width: rect.width, height: rect.height)
                    .offset(x: rect.minX, y: rect.minY)
                    .shadow(
                        color: Color.primary.opacity(HeatmapLayout.hoverGlowOpacity),
                        radius: HeatmapLayout.hoverGlowRadius)
                    .allowsHitTesting(false)
            }
        }
        .frame(width: state.contentWidth, height: contentHeight)
        .contentShape(Rectangle())
        .onContinuousHover { phase in
            switch phase {
            case let .active(point):
                hoverIndex = Self.cellAt(point, grid: grid, visibleCols: state.visibleCols)
            case .ended:
                hoverIndex = nil
            }
        }
        // Round 7: clear the hover the instant the content's origin
        // (relative to the container — round 8, see `body`'s
        // `coordinateSpaceName`) actually moves (a redirected mouse-wheel
        // scroll or a trackpad pan on THIS grid) rather than leaving it
        // pinned to a cell that just scrolled out from under a stationary
        // cursor — that stale pin was the actual bug: the tooltip's anchor
        // incorporates `contentOrigin` (see `tooltipAnchor`), so it kept
        // following the content while scrolling, landing on screen at a
        // position the cursor was never over, sometimes naming a date that
        // had scrolled out of view entirely. `.active` hover events
        // re-populate it the moment the pointer moves again.
        .onGeometryChange(
            for: CGPoint.self, of: { $0.frame(in: .named(Self.coordinateSpaceName)).origin }
        ) { newOrigin in
            if Self.shouldClearHoverOnOriginChange(old: contentOrigin, new: newOrigin) {
                hoverIndex = nil
            }
            contentOrigin = newOrigin
        }
        // Round 2, item 2: let a plain vertical mouse wheel scroll this
        // horizontal grid (DashboardTabs.swift's pattern) — placed inside the
        // ScrollView's content, per HorizontalWheelScroll's own doc comment,
        // so `enclosingScrollView` resolves to this row's NSScrollView.
        .background(HorizontalWheelScroll())
    }

    // MARK: - Hit testing

    /// Resolves a screen point to a `grid.cells` index — pure geometry
    /// (bounds, the gap dead zone, `col * 7 + row` indexing). Whether that
    /// index is CURRENTLY a countable, renderable, data-bearing cell is
    /// `hoverCell`'s job, re-checked live on every access rather than baked
    /// in here at hover-set time (round 7).
    ///
    /// `grid.cells` is laid out `col * 7 + row` by `buildGrid` (Grid.swift's
    /// nested col/row loop), so direct indexing avoids a lookup dictionary;
    /// the col/row re-check guards against that ordering ever changing.
    /// Static (not `private`) so SelfTest can exercise the exact production
    /// resolver directly (FLAT-HEATMAP contract C3) instead of a hand-rebuilt
    /// copy of this coordinate math.
    static func cellAt(_ point: CGPoint, grid: TokenBarCore.GridLayout, visibleCols: Int) -> Int? {
        guard point.x >= HeatmapLayout.gridLeading, point.y >= HeatmapLayout.gridTop
        else { return nil }
        let localX = point.x - HeatmapLayout.gridLeading
        let localY = point.y - HeatmapLayout.gridTop
        // Reject gap coordinates before resolving an index — see
        // `withinCell`'s doc comment for why this differs from the bar chart.
        guard
            Self.withinCell(offset: localX, step: HeatmapLayout.step, cell: HeatmapLayout.cell),
            Self.withinCell(offset: localY, step: HeatmapLayout.step, cell: HeatmapLayout.cell)
        else { return nil }
        let row = Int(localY / HeatmapLayout.step)
        let col = Int(localX / HeatmapLayout.step)
        guard (0..<7).contains(row), (0..<visibleCols).contains(col) else { return nil }
        let index = col * 7 + row
        guard grid.cells.indices.contains(index) else { return nil }
        let cell = grid.cells[index]
        guard cell.col == col, cell.row == row else { return nil }
        return index
    }

    private func cellCenter(_ cell: GridCell) -> CGPoint {
        // Derived, not recomputed: one more copy of the row/col -> point math
        // is one more place for the tooltip to drift off the cell it describes.
        let rect = HeatmapLayout.rect(col: cell.col, row: cell.row)
        return CGPoint(x: rect.midX, y: rect.midY)
    }

    // MARK: - Tooltip

    private func tooltip(_ cell: GridCell) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(Format.monthDay(cell.date)).font(.caption.weight(.semibold))
            Text("%@ tokens".localized(Format.exactTokens(cell.tokens)))
                .font(.caption2)
                .foregroundStyle(.secondary)
            Text(Format.usd(cell.cost))
                .font(.caption2)
                .foregroundStyle(.secondary)
        }
        .padding(6)
        .frame(width: Self.tooltipWidth, alignment: .leading)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 8))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(.quaternary))
        .onGeometryChange(for: CGSize.self) { $0.size } action: { tooltipSize = $0 }
        .allowsHitTesting(false)
    }
}
