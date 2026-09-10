import SwiftUI
import TokenBarCore

/// The usage stack. Leads with the chart, then the live-session trace and the
/// model breakdown.
///
/// Quota lives in its own lens as of 2026-08-17 — the window card and the
/// Agent-limits card answer one question at two scales, and keeping them here
/// while the window history sat elsewhere split one subject across two places.
struct OverviewView: View {
    let payload: UsagePayload
    /// The active tab's client slice (all present clients on Overview).
    let clientIds: [String]
    let stats: UsageStats
    let modelReport: ModelReport?
    /// Forwarded to ModelBreakdownCard; see its `loading` doc.
    var modelLoading = false
    let colors: ModelColorMap
    let trace: [TraceBucket]
    /// Set when this view shows a single client's slice.
    var singleClient: String?
    /// Dashboard year filter (nil = all time), forwarded to the chart card.
    var year: String?
    /// The user's tab-hidden set, passed in from the observing parent
    /// (PopoverView) so the dependency is explicit rather than an imperative
    /// `ClientRegistry.hiddenClients()` read in this body.
    var hidden: Set<String> = []
    /// Cross-subscription quota position, folded by the model. Nil means no
    /// subscription reported a usable window.
    var quotaSummary: QuotaSummary?
    /// Whether the first quota fetch has settled, so a nil summary can read as
    /// "nothing to report" rather than "still asking".
    var usageAttempted = true
    /// Quota readings per `"<clientId>|<cardId>"`, for the Agent-limits card's
    /// sparkline and recent-consumption indicator. Both tabs: the client tab
    /// draws the same card, and an empty map here silently blanks the indicator
    /// rather than saving anything.
    var windowCurves: [String: [QuotaSample]] = [:]
    var agentUsage: AgentUsagePayload?

    @AppStorage(OverviewCard.hiddenKey) private var hiddenCardsRaw = ""
    @AppStorage("tokenbar.limits.enabled") private var limitsEnabled = true

    var body: some View {
        VStack(spacing: 12) {
            // Quota cards moved to the Quota lens (2026-08-17): the window card
            // and the Agent-limits card answer the same question at two scales,
            // and having them here while the window history lived elsewhere
            // split one subject across two lenses. Overview keeps usage.
            ForEach(OverviewCard.visible(hiddenRaw: hiddenCardsRaw), id: \.self) { card in
                self.card(card)
            }
        }
    }

    /// One card, or nothing when it does not apply to this tab. A card absent
    /// because the surface has no use for it is not the same as one the user
    /// hid, and neither needs a placeholder.
    @ViewBuilder
    private func card(_ card: OverviewCard) -> some View {
        switch card {
        case .quotaSummary:
            // `limitsEnabled` too, not just the tab. Settings promises "Off
            // hides the Agent-limits quota card everywhere — the Overview
            // summary, every client's own tab, and this preview", and this is
            // that Overview summary; only the `.limits` branch below was
            // honouring it.
            if singleClient == nil, limitsEnabled {
                QuotaSummaryLine(
                    summary: quotaSummary, attempted: usageAttempted,
                    today: stats.perDayMap[Format.todayKey()].map {
                        (tokens: $0.tokens, cost: $0.cost)
                    })
            }
        case .limits:
            // Back on Overview as of 2026-08-17. Moving it to the Quota lens
            // took the most-looked-at card off the landing tab for everyone
            // rather than for anyone who asked; it belongs in both, as a glance
            // here and in full there. Hiding it is now the user's call.
            if limitsEnabled {
                AgentLimitsCard(
                    clients: clientIds, trace: trace, agentUsage: agentUsage,
                    usageAttempted: usageAttempted,
                    title: singleClient.map {
                        "%@ limits".localized(ClientRegistry.tabDisplayName($0))
                    } ?? "Agent limits",
                    note: singleClient == nil
                        ? "OAuth quota" : "Session / weekly / model limits",
                    restrict: singleClient != nil,
                    reorderable: singleClient == nil,
                    // Both tabs. This gate was a copy of the one in
                    // `PopoverView`, and removing that one left this one
                    // blanking the same indicator on the same lens — the rule
                    // was stated twice and reconciled once. The client tab's
                    // card carries the recent-consumption indicator too, and it
                    // is computed from these curves.
                    curves: windowCurves)
            }
        case .chart:
            chart
        case .trace:
            if singleClient == nil {
                UsageTraceCard(buckets: trace, windowSecs: 600, hidden: hidden)
            }
        case .models:
            ModelBreakdownCard(
                report: modelReport, clientIds: clientIds, colors: colors,
                title: singleClient.map {
                    "%@ models".localized(ClientRegistry.style($0).displayName)
                } ?? "Models by cost",
                loading: modelLoading)
        case .streaks:
            StreaksCard(streaks: stats.streaks)
        }
    }

    private var chart: some View {
        UsageChartCard(
            payload: payload, clientIds: clientIds, stats: stats, colors: colors,
            year: year)
    }
}
