import SwiftUI
import TokenBarCore

/// The classic TokenBar dashboard stack. The all-agent overview leads with
/// the usage chart, lists every agent's limits and carries the live-session
/// trace; a single-client tab leads with that client's limits instead.
struct OverviewView: View {
    let payload: UsagePayload
    /// The active tab's client slice (all present clients on Overview).
    let clientIds: [String]
    let stats: UsageStats
    let modelReport: ModelReport?
    let colors: ModelColorMap
    let trace: [TraceBucket]
    let agentUsage: AgentUsagePayload?
    /// Set when this view shows a single client's slice.
    var singleClient: String?
    /// Dashboard year filter (nil = all time), forwarded to the chart card.
    var year: String?
    /// The user's tab-hidden set, passed in from the observing parent
    /// (PopoverView) so the dependency is explicit rather than an imperative
    /// `ClientRegistry.hiddenClients()` read in this body.
    var hidden: Set<String> = []

    /// Master switch: off hides the Agent-limits card everywhere.
    @AppStorage("tokenbar.limits.enabled") private var limitsEnabled = true
    /// Per-client Agent-limits visibility, independent of tab visibility.
    @AppStorage(ClientRegistry.limitsHiddenKey) private var limitsHiddenRaw = ""

    private var hiddenLimits: Set<String> {
        ClientRegistry.parseIdSet(limitsHiddenRaw)
    }

    var body: some View {
        VStack(spacing: 12) {
            if let singleClient {
                let name = ClientRegistry.style(singleClient).displayName
                if limitsEnabled && !hiddenLimits.contains(singleClient) {
                    AgentLimitsCard(
                        clients: [singleClient], trace: trace, agentUsage: agentUsage,
                        title: "\(name) limits", note: "Session / weekly / model limits",
                        restrict: true)
                }
                chart
                ModelBreakdownCard(
                    report: modelReport, clientIds: clientIds, colors: colors,
                    title: "\(name) models")
            } else {
                chart
                if limitsEnabled {
                    AgentLimitsCard(
                        clients: clientIds, trace: trace, agentUsage: agentUsage,
                        reorderable: true)
                }
                UsageTraceCard(buckets: trace, windowSecs: 600, hidden: hidden)
                ModelBreakdownCard(
                    report: modelReport, clientIds: clientIds, colors: colors)
            }
            StreaksCard(streaks: stats.streaks)
        }
    }

    private var chart: some View {
        UsageChartCard(
            payload: payload, clientIds: clientIds, stats: stats, colors: colors,
            year: year)
    }
}
