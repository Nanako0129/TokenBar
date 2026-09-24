import SwiftUI
import TokenBarCore

/// Every window's recorded cycles as a strip, so the current one can be read
/// against the ones before it.
///
/// The bars are on a fixed 0...100 with the ceiling drawn, which is what makes
/// the honest headline possible: on this data the tallest bar in eighteen
/// recorded sessions is 58%, and the ceiling has never been reached. A gauge
/// showing "79% left" says the same thing far less usefully, because it cannot
/// say whether 79% is normal.
struct QuotaHistoryStripCard: View {
    let summaries: [QuotaWindowSummary]
    /// Per-window equivalence, keyed as the summary's `id`. Absent means either
    /// the window cannot support an estimate or the scan has not landed — the
    /// strip above never waits on it either way.
    var equivalences: [String: WindowEquivalence.Row] = [:]
    var attempted = true

    private static let stripHeight: CGFloat = 26
    private static let barGap: CGFloat = 1.5
    /// A recorded cycle always draws something. A cycle that consumed 1% is not
    /// the same as one that was never recorded, and a bar rounding to zero
    /// height would make them identical.
    private static let minimumInk: CGFloat = 1
    private static let tooltipWidth: CGFloat = 168

    @State private var cardFrame: CGRect = .zero
    /// Which window and which bar in it. Kept together so a stale window id can
    /// never pair with a live bar index.
    @State private var hover: (window: String, index: Int)?
    @State private var hoverAnchorInCard: CGPoint = .zero
    @State private var tooltipSize: CGSize = .zero
    @Environment(\.popoverScrollViewport) private var viewport

    var body: some View {
        DashCard("Past windows", subtitle: subtitle) {
            if !summaries.isEmpty {
                VStack(spacing: 10) {
                    ForEach(summaries) { summary in
                        row(summary)
                    }
                }
            } else if attempted {
                Text("No completed windows recorded yet. They accumulate as TokenBar runs.")
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

    @ViewBuilder
    private var tooltipLayer: some View {
        if let hover, cardFrame != .zero,
           let summary = summaries.first(where: { $0.id == hover.window }),
           summary.recent.indices.contains(hover.index)
        {
            let value = summary.recent[hover.index]
            // The strip is oldest-to-newest, so counting back from the end is
            // what turns a bar into "how many windows ago".
            let ago = summary.recent.count - 1 - hover.index
            VStack(alignment: .leading, spacing: 3) {
                Text(verbatim: Self.rowLabel(summary))
                    .font(.caption2.weight(.semibold))
                Text(ago == 0 ? "Most recent window".localized
                              : "%@ windows ago".localized(String(ago)))
                    .font(.system(size: 9))
                    .foregroundStyle(.tertiary)
                Text("Consumed %@%% of the allowance".localized(
                    String(Int(value.rounded()))))
                    .font(.caption2)
                if summary.recentPeaks.indices.contains(hover.index),
                   summary.recentPeaks[hover.index] >= QuotaOverviewFold.exhaustedPercent {
                    Text("Ran out")
                        .font(.system(size: 9))
                        .foregroundStyle(.orange)
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
                        ? CGSize(width: Self.tooltipWidth, height: 72) : tooltipSize,
                    containerFrame: cardFrame, viewport: viewport) ?? .zero)
            .allowsHitTesting(false)
        }
    }

    private var subtitle: String? {
        guard !summaries.isEmpty else { return nil }
        return summaries.allSatisfy(\.neverExhausted) ? "never exhausted".localized : nil
    }

    /// The client name and window label a row (and its tooltip) both show,
    /// qualified by account the same way `QuotaSummaryLine.tightestName` is:
    /// two accounts of one client can carry identically-carded history, so
    /// the client's display name alone leaves their rows indistinguishable.
    ///
    /// A named function, not composed inline in `row`/`tooltipLayer`, so
    /// `M3-n` can assert the string directly.
    static func rowLabel(_ summary: QuotaWindowSummary) -> String {
        let style = ClientRegistry.style(summary.clientId)
        let account = AccountIdentity(
            clientId: summary.clientId, accountKey: summary.accountKey
        ).accountLabel
        let name = [style.displayName, account].compactMap(\.self).joined(separator: " ")
        return "\(name) · \(summary.windowLabel.localized)"
    }

    @ViewBuilder
    private func row(_ summary: QuotaWindowSummary) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: 5) {
                AgentIconView(clientId: summary.clientId, size: 11)
                Text(verbatim: Self.rowLabel(summary))
                    .font(.caption2)
                    .lineLimit(1)
                Spacer(minLength: 4)
                Text("%@ windows".localized(String(summary.cycleCount)))
                    .font(.system(size: 9))
                    .foregroundStyle(.tertiary)
            }
            strip(summary)
            Text(headline(summary))
                .font(.system(size: 9))
                .foregroundStyle(.tertiary)
            if let row = equivalences[summary.id] {
                Text(WindowEquivalence.text(
                    row, tokens: Format.compactTokens, money: Format.usdOrBelowCent))
                    .font(.system(size: 9))
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    /// Fixed 0...100 with the ceiling drawn as a dotted rule. Rescaling to the
    /// tallest bar would make a run of 2% cycles look like a run of 60% ones,
    /// which is the one comparison this strip exists to support.
    private func strip(_ summary: QuotaWindowSummary) -> some View {
        GeometryReader { proxy in
            let width = proxy.size.width
            let count = max(summary.recent.count, 1)
            let barWidth = max(
                1, (width - CGFloat(count - 1) * Self.barGap) / CGFloat(count))
            Canvas { context, size in
                var ceiling = Path()
                ceiling.move(to: CGPoint(x: 0, y: 0.5))
                ceiling.addLine(to: CGPoint(x: size.width, y: 0.5))
                context.stroke(
                    ceiling, with: .color(.secondary.opacity(0.35)),
                    style: StrokeStyle(lineWidth: 1, dash: [2, 3]))

                for (index, value) in summary.recent.enumerated() {
                    let height = max(
                        Self.minimumInk, size.height * CGFloat(min(1, value / 100)))
                    let isLatest = index == summary.recent.count - 1
                    let isHovered = hover?.window == summary.id && hover?.index == index
                    context.fill(
                        Path(CGRect(
                            x: CGFloat(index) * (barWidth + Self.barGap),
                            y: size.height - height,
                            width: barWidth, height: height)),
                        // The newest cycle is the one being asked about; the
                        // rest are context, so they recede.
                        with: .color(.accentColor.opacity(
                            isHovered ? 0.95 : (isLatest ? 0.86 : 0.32))))
                }
            }
            .contentShape(Rectangle())
            .onContinuousHover { phase in
                switch phase {
                case let .active(point):
                    let pitch = barWidth + Self.barGap
                    let index = pitch > 0 ? Int(point.x / pitch) : 0
                    guard summary.recent.indices.contains(index) else { hover = nil; break }
                    hover = (summary.id, index)
                    let strip = proxy.frame(in: .global)
                    hoverAnchorInCard = CGPoint(
                        x: CGFloat(index) * pitch + barWidth + strip.minX - cardFrame.minX,
                        y: point.y + strip.minY - cardFrame.minY)
                case .ended:
                    hover = nil
                }
            }
        }
        .frame(height: Self.stripHeight)
    }

    private func headline(_ summary: QuotaWindowSummary) -> String {
        let peak = String(Int(summary.peakPercent.rounded()))
        // "Peaked at", not "heaviest": this is the highest READING, while the
        // bars draw consumption, and the two differ whenever the app started
        // watching a cycle partway through.
        return summary.neverExhausted
            ? "Peaked at %@%% · never ran out".localized(peak)
            : "Peaked at %@%% · ran out at least once".localized(peak)
    }
}
