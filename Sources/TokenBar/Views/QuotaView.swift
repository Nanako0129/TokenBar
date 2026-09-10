import SwiftUI
import TokenBarCore

/// The "Quota" lens: everything about a subscription's usage windows in one
/// place, rather than the current window on Overview and its history somewhere
/// else.
///
/// The two tabs answer different questions and so render different things.
/// Across all agents the question is comparative — which subscription is
/// tightest, which is burning fastest — and `AgentLimitsCard` already answers
/// it, one sparkline per window. On a single client the question is specific:
/// this window, then the windows before it.
struct QuotaView: View {
    /// The open client tab, or nil on the all-agent view.
    let singleClient: String?
    let clientIds: [String]
    let trace: [TraceBucket]
    let agentUsage: AgentUsagePayload?
    var usageAttempted = true
    /// Whether the stage-two usage scan settled with an error, so the history
    /// card can say so instead of spinning under rows that are already drawn.
    var scanFailed = false
    var curveUnreadable = false
    /// Quota readings per `"<clientId>|<cardId>"`, for the sparklines.
    var windowCurves: [String: [QuotaSample]] = [:]
    /// The selected window's card state on a single-client tab.
    var windowCard: WindowCardState?
    /// Recorded reset cycles of that window, newest first.
    var quotaCycles: [QuotaCycle] = []
    /// Those cycles joined to local usage; empty while the scan is out.
    var quotaHistory: [QuotaHistoryRow] = []
    /// Shared model palette, so a model keeps one colour across the app.
    var colors: ModelColorMap = ModelColorMap(entries: [])
    /// Daily spend stacked by declared subscription, for the all-agent view.
    /// Nil until the graph payload has been folded.
    /// Recorded-cycle strips for every displayed window.
    var windowSummaries: [QuotaWindowSummary] = []
    /// Weekday-by-hour consumption per window, keyed as the window's id.
    var heatmaps: [String: QuotaHeatmap] = [:]
    /// Which windows have a grid at all. Not derived from `windowSummaries`.
    var heatmapWindows: [QuotaHeatmapWindow] = []
    var equivalences: [String: WindowEquivalence.Row] = [:]
    var trend: SubscriptionTrend?

    @AppStorage("tokenbar.limits.enabled") private var limitsEnabled = true

    var body: some View {
        VStack(spacing: 12) {
            if let singleClient {
                if let windowCard {
                    // Above its siblings: a `zIndex` set inside the card orders
                    // that card's children, not the card among these.
                    WindowUsageCard(state: windowCard).zIndex(1)
                }
                if limitsEnabled {
                    AgentLimitsCard(
                        clients: clientIds, trace: trace, agentUsage: agentUsage,
                        usageAttempted: usageAttempted,
                        title: "%@ limits".localized(
                            ClientRegistry.tabDisplayName(singleClient)),
                        note: "Session / weekly / model limits",
                        restrict: true, curves: windowCurves)
                }
                QuotaHistoryCard(
                    clientId: singleClient, cycles: quotaCycles,
                    rows: quotaHistory, colors: colors, attempted: usageAttempted,
                    scanFailed: scanFailed, curveUnreadable: curveUnreadable)
            } else {
                // Trend first: it answers "where is my spend going" across
                // subscriptions, which the window-by-window card below cannot.
                SubscriptionTrendCard(trend: trend)
                QuotaHistoryStripCard(
                    summaries: windowSummaries, equivalences: equivalences,
                    attempted: usageAttempted)
                // After the strip, not before: the strip says how much each
                // window consumed, and this says when. "When" is only a
                // question once "how much" has an answer.
                QuotaHeatmapCard(
                    windows: heatmapWindows, heatmaps: heatmaps,
                    equivalences: equivalences, attempted: usageAttempted)
                if limitsEnabled {
                    AgentLimitsCard(
                    clients: clientIds, trace: trace, agentUsage: agentUsage,
                        usageAttempted: usageAttempted,
                        reorderable: true, curves: windowCurves)
                }
            }
        }
    }
}
