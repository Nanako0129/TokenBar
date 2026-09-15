import SwiftUI
import TokenBarCore

/// The windows before this one: how much allowance each consumed, and what was
/// spent inside it.
///
/// Both halves are required. Measured on live data, a `claude/session.v1`
/// window moved 1% while 308M tokens and $535 of API-equivalent value were
/// recorded in the same five hours, almost all of it charged to a different
/// subscription. The quota column alone would say "a quiet window"; the spend
/// column alone would say "an expensive one". Neither is the answer.
struct QuotaHistoryCard: View {
    let clientId: String
    /// Newest first. Present as soon as the persisted curve is read, which is
    /// milliseconds — the rows render with their quota bars immediately.
    let cycles: [QuotaCycle]
    /// The join to local usage. Empty while the scan is out; a row then shows
    /// its quota and a placeholder where the numbers will land, rather than the
    /// whole card waiting.
    let rows: [QuotaHistoryRow]
    /// The app-wide model palette. Shared so a model is the same colour here as
    /// in the model breakdown and the usage chart.
    var colors: ModelColorMap = ModelColorMap(entries: [])
    /// Whether the first quota fetch has settled. `cycles` derives from the
    /// live payload, and the quota payload is never persisted, so on every cold
    /// start an empty array here means "still asking" — not "nothing recorded".
    /// Without this the card announced no history while the window card
    /// directly above it was still showing its own spinner.
    var attempted = true
    /// Whether the usage scan settled with an error. Same distinction the
    /// window card makes: an expanded row with no usage half is either still
    /// scanning or done and failed, and only one of those is a spinner.
    var scanFailed = false
    /// Whether the persisted curve could not be read. Distinct from having no
    /// history: both leave `cycles` empty after the fetch has settled, and only
    /// one of them is the card's fault to report as an absence.
    var curveUnreadable = false

    /// Observed, not read once: `span` above is accumulated only for messages
    /// assigned to THIS subscription, so with nothing declared every cycle
    /// arrives at zero and the fold would call that "none of it recorded on
    /// this machine". Reading `UserDefaults` in a computed property would not
    /// be a view dependency, so a classification saved in Settings would leave
    /// this line stale until something unrelated rebuilt the body.
    @AppStorage(UsageAttribution.confirmedKey) private var attributionRaw = ""

    @State private var expanded: Int64?

    /// How many rows are drawn right now. Grows by `visibleRows` per press of
    /// the footer button and never shrinks, which is the property the two
    /// aggregates below depend on — see `shownRows`.
    ///
    /// Per window, not per card: `QuotaView` gives this view an identity of
    /// `WindowCardLoader.historyCardId`, so picking a different window builds a
    /// new card and this returns to its opening value along with `expanded`.
    /// Without that, switching Session to Weekly kept the reader's expansion —
    /// same view identity, an entirely different history underneath it — and
    /// landed them on a window already grown to 36 rows with no "Show more"
    /// beneath it and both aggregates folded over the inherited count.
    @State private var shownCount = QuotaHistoryCard.visibleRows

    /// Below this the cycle was barely witnessed and its consumption figure is
    /// not evidence about the window — the app simply was not running for most
    /// of it. Shown, but marked.
    private static let thinObservation = 0.5
    private static let barWidth: CGFloat = 62
    private static let barHeight: CGFloat = 4
    private static let barGap: CGFloat = 2
    /// How many rows the card opens with, and how many each press of the
    /// footer button adds. The ceiling is not here: `cycles` arrives already
    /// capped at `QuotaHistoryFold.consideredCycles` by `WindowCardLoader`, so
    /// pressing until the button disappears shows everything the fold admits
    /// and reaching further is an engine change, not a display one.
    ///
    /// Growing the list rather than paging it or nesting a scroller: the two
    /// aggregates on this card — the equivalence line and the usage bar's
    /// scale — are computed over the rows on screen, so a pager would hand the
    /// same history a different ratio on every page. And the whole dashboard
    /// already sits in one vertical `ScrollView`, which a second same-axis
    /// scroller inside the card would fight for the wheel.
    ///
    /// Not private: `QuotaHistoryFold.consideredCycles` has to stay at or above
    /// it, and the selftest asserts that. A cap below this number would draw
    /// fewer rows than this card intends without anything saying so.
    static let visibleRows = 12

    private var byCycle: [Int64: QuotaHistoryRow] {
        Dictionary(rows.map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
    }

    var body: some View {
        DashCard("Window history", subtitle: subtitle) {
            if cycles.isEmpty, !attempted {
                LoadingLine(title: "Reading quota history…")
            } else if cycles.isEmpty, curveUnreadable {
                Text("Quota history could not be read. It will be retried.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
            } else if cycles.isEmpty {
                Text("No earlier windows recorded yet. They accumulate as TokenBar runs.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .frame(maxWidth: .infinity, alignment: .leading)
            } else {
                let joined = byCycle
                let drawn = shownCycles
                equivalence
                VStack(spacing: 0) {
                    ForEach(drawn, id: \.resetAtMs) { cycle in
                        row(cycle, row: joined[cycle.resetAtMs])
                        if cycle.resetAtMs != drawn.last?.resetAtMs {
                            Divider().opacity(0.4)
                        }
                    }
                }
                showMore
                footnote
            }
        }
    }

    /// The cycles drawn as rows. One statement of it, because three things read
    /// it: the row list, the equivalence, and the usage bar's scale.
    private var shownCycles: [QuotaCycle] {
        Array(cycles.prefix(shownCount))
    }

    private var subtitle: String? {
        guard !cycles.isEmpty else { return nil }
        return "%@ windows".localized(String(shownCycles.count))
    }

    /// How many rows the next press would add: the step, or what is left if
    /// that is less. Zero means the button is not drawn at all — a "Show 0
    /// more" that did nothing is the failure this clamps away, and it is
    /// reachable whenever the admitted cycle count is not a multiple of the
    /// step.
    static func moreCount(total: Int, shown: Int) -> Int {
        max(0, min(total - shown, visibleRows))
    }

    /// The grow control. Absent once every admitted cycle is on screen, which
    /// is also how the card says there is no more history rather than leaving a
    /// button that would do nothing.
    @ViewBuilder
    private var showMore: some View {
        let more = Self.moreCount(total: cycles.count, shown: shownCount)
        if more > 0 {
            Button {
                shownCount += Self.visibleRows
            } label: {
                Text("Show %@ more".localized(String(more)))
                    .font(.caption2)
            }
            .buttonStyle(.link)
            .padding(.top, 6)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    @ViewBuilder
    private func row(_ cycle: QuotaCycle, row: QuotaHistoryRow?) -> some View {
        let isOpen = expanded == cycle.resetAtMs
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 8) {
                Image(systemName: isOpen ? "chevron.down" : "chevron.right")
                    .font(.system(size: 8, weight: .semibold))
                    .foregroundStyle(.tertiary)
                    .frame(width: 8)
                Text(Format.windowStamp(ms: cycle.startMs))
                    .font(.caption2.monospacedDigit())
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .fixedSize()
                bars(cycle, row: row)
                Text(verbatim: "\(Int(cycle.usedPercent.rounded()))%")
                    .font(.caption2.monospacedDigit())
                    .frame(width: 30, alignment: .trailing)
                Spacer(minLength: 4)
                if let row {
                    // The mirror of the money rule beside it: cost-only usage
                    // assigned here has no token count, and printing "0" states
                    // a measurement nobody took. Same dash, same reason.
                    Text(Format.tokens(tokens: row.mineTokens, cost: row.mineCost))
                        .font(.caption2.monospacedDigit())
                        .foregroundStyle(.secondary)
                    // A dash, not "$0.00", when tokens were recorded and none
                    // of them carried a price. This is a fixed-width column, so
                    // the alternative to a placeholder is a false total — the
                    // same missing-metric-as-a-measured-zero the other-line
                    // below avoids, in the column above it.
                    Text(Format.money(tokens: row.mineTokens, cost: row.mineCost))
                        .font(.caption2.monospacedDigit())
                        .frame(width: 54, alignment: .trailing)
                } else {
                    Text(verbatim: "·")
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                        .frame(width: 54, alignment: .trailing)
                }
            }
            .contentShape(Rectangle())
            .onTapGesture { expanded = isOpen ? nil : cycle.resetAtMs }
            if isOpen { detail(cycle, row: row) }
        }
        .padding(.vertical, 5)
    }

    /// Two bars, stacked and deliberately NOT sharing a scale.
    ///
    /// Quota and tokens are not proportional. Measured on live data, one window
    /// moved 1% of the allowance while carrying 18.0M attributed tokens and
    /// another moved 9% carrying 22.8M — a 5x difference in tokens per point.
    /// Putting them on one bar, or on one axis, would draw a relationship that
    /// is not there. Adjacent and separately scaled, the mismatch between the
    /// two lengths is legible instead of hidden, and that mismatch is the thing
    /// this card exists to expose.
    private func bars(_ cycle: QuotaCycle, row: QuotaHistoryRow?) -> some View {
        VStack(alignment: .leading, spacing: Self.barGap) {
            // Fixed 0...100, matching the window card: the point of a history is
            // comparing cycles to each other and to the ceiling, and rescaling
            // to the largest row would make a 3% window and a 58% one look
            // alike.
            ZStack(alignment: .leading) {
                Capsule().fill(.quaternary.opacity(0.6))
                Capsule()
                    .fill(Color.accentColor.opacity(
                        cycle.observedFraction < Self.thinObservation ? 0.35 : 0.85))
                    .frame(width: Self.barWidth * min(1, cycle.usedPercent / 100))
            }
            .frame(width: Self.barWidth, height: Self.barHeight)

            // Relative to the heaviest window on screen, segmented by model in
            // the app's shared palette. An empty strip is a real answer: this
            // window consumed allowance but nothing was declared as this
            // subscription's.
            usageBar(row)
        }
    }

    /// The rows actually on screen.
    ///
    /// One statement of it. The question is asked three times — the `ForEach`
    /// draws `shownCycles`, the equivalence accumulates over the same set, and
    /// the usage bar scales against it — and had three answers, the scale's
    /// being "every retained row". A hidden older cycle with the largest total
    /// then set the scale and made every visible bar short, which is precisely
    /// the comparison the bar claims to be making.
    ///
    /// Pressing Show more therefore moves both aggregates, and that is the
    /// intended reading: they describe what is on screen. It is also why the
    /// row count only ever grows — a control that could shrink it would make
    /// the equivalence oscillate between two answers for one history.
    private var shownRows: [QuotaHistoryRow] {
        let shown = Set(shownCycles.map(\.resetAtMs))
        return rows.filter { shown.contains($0.id) }
    }

    /// The largest attributed token count among the visible rows, and therefore
    /// the usage bar's full width. Nil when nothing has been scanned yet.
    private var usageScale: Int64? {
        let peak = shownRows.map(\.mineTokens).max() ?? 0
        return peak > 0 ? peak : nil
    }

    @ViewBuilder
    private func usageBar(_ row: QuotaHistoryRow?) -> some View {
        ZStack(alignment: .leading) {
            Capsule().fill(.quaternary.opacity(0.35))
            if let row, let scale = usageScale, row.mineTokens > 0 {
                let full = Self.barWidth * min(1, Double(row.mineTokens) / Double(scale))
                HStack(spacing: 0) {
                    ForEach(row.models) { model in
                        Rectangle()
                            .fill(Color(hex: colors.color(model.providerId, model.modelId)))
                            .frame(width: full * Double(model.tokens) / Double(row.mineTokens))
                    }
                }
                .frame(width: full, alignment: .leading)
                .clipShape(Capsule())
            }
        }
        .frame(width: Self.barWidth, height: Self.barHeight)
    }

    @ViewBuilder
    private func detail(_ cycle: QuotaCycle, row: QuotaHistoryRow?) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            // The full interval lives here rather than in the row: it is the
            // one place with room for it, and it is what a reader opens a row
            // to confirm.
            Text(Format.clockRange(fromMs: cycle.startMs, toMs: cycle.resetAtMs))
                .font(.system(size: 9).monospacedDigit())
                .foregroundStyle(.tertiary)
            if cycle.observedFraction < Self.thinObservation {
                Text("TokenBar observed %@%% of this window, so its usage figure is a floor."
                    .localized(String(Int((cycle.observedFraction * 100).rounded()))))
                    .font(.system(size: 9))
                    .foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if let row {
                ForEach(row.models.prefix(4)) { model in
                    HStack(spacing: 6) {
                        Text(model.modelId)
                            .lineLimit(1)
                            .truncationMode(.middle)
                        Spacer(minLength: 4)
                        // The same pair rule as the collapsed row above. The
                        // total learned the dash and these did not, so expanding
                        // a cost-only model contradicted the line it expanded.
                        Text(Format.tokens(tokens: model.tokens, cost: model.cost))
                            .foregroundStyle(.secondary)
                        Text(Format.money(tokens: model.tokens, cost: model.cost))
                            .frame(width: 54, alignment: .trailing)
                    }
                    .font(.system(size: 10).monospacedDigit())
                }
                if row.models.isEmpty {
                    Text("Nothing in this window was charged to this subscription.")
                        .font(.system(size: 10))
                        .foregroundStyle(.tertiary)
                }
                // The line that explains a flat bar. Without it a 1% window
                // holding $535 of work looks like a quiet afternoon.
                // Tokens OR cost. A supported cost-only source — priced usage
                // whose transcript carries no token counts — suppressed this
                // line entirely, so a window full of another subscription's
                // spend read as one with nothing else in it. The same reason
                // the equivalence grew a `costOnly` row.
                if row.otherTokens > 0 || row.otherCost > 0 {
                    // One-value phrasing whenever one side is absent, in EITHER
                    // direction. The first version handled only the cost-only
                    // case and left its mirror printing "2.1M · $0.00" for
                    // unpriced models — the same missing-metric-as-measured-zero
                    // this branch exists to avoid, surviving on the other side
                    // of its own condition.
                    Text({
                        // The lead names what the bucket actually holds. It
                        // mixes three attribution states — declared elsewhere,
                        // declared excluded, and never classified — and calling
                        // all of it "other subscriptions" is a claim the user
                        // never made. On a setup with no declarations at all it
                        // is a claim about every message on the machine.
                        //
                        // Excluded is its own state, not a shade of
                        // unclassified. The user who excluded a source already
                        // answered the question this line asks; calling it
                        // unclassified reports their decision back to them as
                        // an outstanding one. Exhaustive over the seven
                        // non-empty combinations, so no combination is
                        // described by a phrase that does not cover it.
                        let lead: String
                        if row.otherHasUnattributed {
                            lead = row.otherHasAssigned || row.otherHasExcluded
                                ? "Other and unclassified usage in the same hours:"
                                : "Unclassified usage in the same hours:"
                        } else if row.otherHasExcluded {
                            lead = row.otherHasAssigned
                                ? "Other and excluded usage in the same hours:"
                                : "Excluded usage in the same hours:"
                        } else {
                            lead = "Other subscriptions in the same hours:"
                        }
                        let value = row.otherTokens > 0 && row.otherCost > 0
                            ? "%@ · %@".localized(
                                Format.compactTokens(row.otherTokens),
                                Format.usdOrBelowCent(row.otherCost))
                            : (row.otherTokens > 0
                                ? Format.compactTokens(row.otherTokens)
                                : Format.usdOrBelowCent(row.otherCost))
                        return "%@ %@".localized(lead.localized, value)
                    }())
                        .font(.system(size: 9))
                        .foregroundStyle(.tertiary)
                        .padding(.top, 1)
                }
            } else if scanFailed {
                Text("Local usage could not be read.".localized)
                    .font(.system(size: 10))
                    .foregroundStyle(.tertiary)
            } else {
                LoadingLine(title: "Reading local usage…")
                    .scaleEffect(0.85, anchor: .leading)
            }
        }
        .padding(.leading, 16)
        .padding(.top, 2)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    /// What a tenth of this window's allowance has been worth, taken across
    /// every cycle on screen rather than one.
    ///
    /// Placed above the rows because it is the summary of them: a single row's
    /// ratio is dominated by the 1-point reading quantisation, and the whole
    /// reason to accumulate is that the individual figures cannot be trusted.
    @ViewBuilder
    private var equivalence: some View {
        let counted = shownRows
        if !counted.isEmpty {
            Text(WindowEquivalence.text(
                WindowEquivalence.aggregate(
                    subscription: clientId,
                    records: UsageAttribution.parseRaw(attributionRaw).records,
                    cycles: counted.map {
                        WindowEquivalence.Cycle(
                            deltaPercent: $0.cycle.usedPercent,
                            spanTokens: $0.spanTokens, spanCost: $0.spanCost,
                            observedFraction: $0.cycle.observedFraction,
                            risingRuns: $0.cycle.risingRuns)
                    }),
                tokens: Format.compactTokens, money: Format.usdOrBelowCent))
                .font(.caption2)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.bottom, 2)
        }
    }

    private var footnote: some View {
        Text("Amounts are API list-price equivalents for usage you declared as %@ — not what the subscription charged."
            .localized(ClientRegistry.style(clientId).displayName))
            .font(.system(size: 9))
            .foregroundStyle(.tertiary)
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.top, 4)
    }
}
