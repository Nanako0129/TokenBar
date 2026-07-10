import Foundation

/// Picks which quota window the menu bar displays. The selection string is
/// `"auto"` (the tightest window — lowest remaining percent — across every
/// agent) or `"<clientId>|<windowLabel>"` for an explicit pick.
public enum QuotaResolver {
    public static let auto = "auto"

    public static func selection(clientId: String, label: String) -> String {
        "\(clientId)|\(label)"
    }

    /// `excluding` is the set of client ids to skip in AUTO mode only (the
    /// user's tab-hidden ∪ limits-hidden clients) — so the menu-bar quota can't
    /// surface a client the popover hides. An EXPLICIT `clientId|window`
    /// selection is always honored, even for an excluded client (the user
    /// deliberately picked it as the tray source). Empty set = pre-hide
    /// behavior, byte-identical.
    public static func resolve(
        payload: AgentUsagePayload?, selection: String, excluding: Set<String> = []
    ) -> (clientId: String, window: UsageWindow)? {
        guard let payload else { return nil }
        if selection.isEmpty || selection == Self.auto {
            var best: (clientId: String, window: UsageWindow)?
            for agent in payload.agents
            where agent.error == nil && !excluding.contains(agent.clientId) {
                for window in agent.windows where window.remainingPercent.isFinite {
                    if best == nil || window.remainingPercent < best!.window.remainingPercent {
                        best = (agent.clientId, window)
                    }
                }
            }
            return best
        }
        let parts = selection.split(separator: "|", maxSplits: 1).map(String.init)
        guard parts.count == 2,
              let agent = payload.agents.first(where: { $0.clientId == parts[0] }),
              let window = agent.windows.first(where: { $0.label == parts[1] })
        else { return nil }
        return (agent.clientId, window)
    }
}
