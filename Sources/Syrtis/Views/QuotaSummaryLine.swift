import SwiftUI
import TokenBarCore

/// The all-agent Overview's opening statement: which subscription is closest to
/// running out, and whether anything else needs looking at.
///
/// Prose rather than another card. Since 2026-08-17 the limits card sits in
/// this lens again, directly below, so this line and that card do overlap on
/// "which subscription is tightest" — deliberately: one sentence is faster to
/// read than ten bars are to scan, and the reader who wants the bars has them
/// immediately underneath. Both are individually hideable from Overview, which
/// is what makes the overlap the user's call rather than ours.
struct QuotaSummaryLine: View {
    let summary: QuotaSummary?
    /// False while the first quota fetch is outstanding. `summary == nil` alone
    /// cannot tell "still asking" from "asked and nothing reported a window",
    /// and only the second may be stated.
    var attempted = true
    /// Today's totals, or nil on a day with no recorded usage yet.
    var today: (tokens: Int64, cost: Double)?

    /// Label column width. Fixed so the three rows align, and wide enough for
    /// the longest label in both shipped languages.
    private static let labelWidth: CGFloat = 62

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            if let summary {
                tightestRow(summary)
                // "Every measured window", not "every window". The count
                // guards against speaking with nothing checked, but it cannot
                // make the sentence true of the windows that were skipped: with
                // four windows and one measurable, the old wording vouched for
                // three nobody looked at. Scoping the claim to what was
                // measured is honest at any coverage, and dropping the line
                // whenever coverage is partial would hide it most of the time —
                // the historical basis is per window and often still learning.
                //
                // Gated on `paceCheckedWindows` as well as on there being
                // other windows: with pace off, or while the historical basis
                // is still learning, `compute` returns nil for every window and
                // `burning` is nil because nothing was asked, not because
                // nothing was wrong. Printing the reassurance then states a
                // guarantee that was never evaluated.
                //
                // A slot that is empty whenever nothing is wrong teaches the
                // user to ignore it, and on this data it is empty nearly
                // always — every window measured runs 3.5 to 68 points under
                // its expected line. Inverting it costs nothing and turns the
                // most reliable fact here into something worth reading.
                if let burning = summary.burning {
                    burnRow(burning)
                } else if summary.otherWindows > 0, summary.paceCheckedWindows > 0 {
                    row(label: "Pace", tint: .secondary) {
                        Text("Every measured window is under its expected pace")
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
                if let today {
                    row(label: "Today", tint: .secondary) {
                        // `money`, not `usdOrBelowCent`: the latter handles
                        // only the sub-cent half. A day of entirely unpriced
                        // models sums to exactly zero, so "5.2K tokens · $0.00"
                        // — the case the first version of this comment claimed
                        // to have fixed — survived it untouched.
                        Text(verbatim: "%@ tokens".localized(
                            Format.compactTokens(today.tokens)) + " · "
                            + Format.money(tokens: today.tokens, cost: today.cost))
                            .font(.caption)
                    }
                }
            } else if attempted {
                Text("No subscription is reporting a usage window right now.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                LoadingLine(title: "Checking agent limits…")
            }
        }
        .fixedSize(horizontal: false, vertical: true)
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(12)
        .glassCard()
    }

    /// A label and its value, stacked rather than run together in one sentence.
    ///
    /// The flowing version wrapped mid-unit — "3 小時 23" then "分 後重置" — and
    /// there is no way to forbid that for a paragraph on a narrow popover:
    /// Chinese breaks between any two characters. Splitting each statement into
    /// short fragments keeps every one of them under a line, which is a layout
    /// the width cannot break rather than a plea that it will not.
    @ViewBuilder
    private func row(
        label: String, tint: HierarchicalShapeStyle, @ViewBuilder value: () -> some View
    ) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Text(label.localized)
                .font(.caption2)
                .foregroundStyle(tint)
                .frame(width: Self.labelWidth, alignment: .leading)
                .fixedSize()
            VStack(alignment: .leading, spacing: 1) { value() }
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    /// Who the tightest window belongs to, qualified by account when the client
    /// has more than one.
    ///
    /// This line answers "of everything you are subscribed to, what is closest
    /// to running out". With two accounts of one client, the client's display
    /// name alone cannot answer it — two subscriptions offering the same card,
    /// or sitting on the same plan, rendered an identical line, so the reader
    /// learned which client is about to run out but not which subscription.
    ///
    /// A named function rather than a `let` inside the body so the composed
    /// string is assertable: `M3-k` covers the label's derivation, but a gate
    /// that stops at the derivation cannot see this line drop it again.
    static func tightestName(_ summary: QuotaSummary) -> String {
        let style = ClientRegistry.style(summary.tightestClient)
        let account = AccountIdentity(
            clientId: summary.tightestClient, accountKey: summary.tightestAccountKey
        ).accountLabel
        return [style.displayName, account].compactMap(\.self).joined(separator: " ")
    }

    private func tightestRow(_ summary: QuotaSummary) -> some View {
        let name = Self.tightestName(summary)
        // Reset text comes from the same helper the quota cards use, so the
        // countdown here cannot drift from the one shown next to the bar.
        let reset = summary.resetsAt.flatMap { UsagePace.resetText(for: $0) }
        return row(label: "Tightest", tint: .secondary) {
            Text(verbatim: "\(name) · \(summary.tightestLabel.localized)")
                .font(.system(size: 13, weight: .semibold))
                .lineLimit(1)
                .minimumScaleFactor(0.8)
            Text(verbatim: [
                "%lld%% left".localized(Int(summary.remainingPercent.rounded())),
                reset,
            ].compactMap(\.self).joined(separator: " · "))
                .font(.caption)
                .foregroundStyle(.secondary)
            if summary.otherWindows > 0 {
                Text(othersText(summary))
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
            }
        }
    }

    /// Who is burning fastest, qualified by account the same way
    /// `tightestName` is — see that function's doc comment for why a bare
    /// client name cannot answer this with two accounts of one client.
    ///
    /// A named function for the same reason `tightestName` is one: `M3-n`
    /// asserts the composed string directly rather than only the view body.
    static func burnName(_ burning: BurnWarning) -> String {
        let style = ClientRegistry.style(burning.clientId)
        let account = AccountIdentity(
            clientId: burning.clientId, accountKey: burning.accountKey
        ).accountLabel
        return [style.displayName, account].compactMap(\.self).joined(separator: " ")
    }

    /// The rate line. Amber rather than red: being ahead of schedule is worth
    /// naming, but it is a projection, and `AgentLimitsCard` owns the actual
    /// warning thresholds.
    private func burnRow(_ burning: BurnWarning) -> some View {
        let name = Self.burnName(burning)
        let tail = [burning.riskText, burning.etaText].compactMap(\.self).first
        return row(label: "Burning fastest", tint: .secondary) {
            Text(verbatim: "\(name) · \(burning.label.localized)")
                .font(.caption.weight(.medium))
                .lineLimit(1)
                .minimumScaleFactor(0.8)
            Text(verbatim: [
                "%lld%% ahead of pace".localized(Int(burning.aheadPercent.rounded())),
                tail,
            ].compactMap(\.self).joined(separator: " · "))
                .font(.caption2)
                .foregroundStyle(.orange)
        }
    }

    private func othersText(_ summary: QuotaSummary) -> String {
        if summary.allOthersComfortable {
            return "%lld other windows, all above %lld%%".localized(
                summary.otherWindows, Int(QuotaSummaryFold.comfortablePercent))
        }
        return "%lld of %lld other windows are below %lld%%".localized(
            summary.otherWindows - summary.othersComfortable,
            summary.otherWindows,
            Int(QuotaSummaryFold.comfortablePercent))
    }
}
