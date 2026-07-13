import TokenBarCore

/// Shared quota-selection policy for the tray and Settings preview. Live mode
/// preserves an explicit persisted selection exactly; demo mode may locally
/// substitute Auto when a dynamic provider label is absent from its fixture.
enum QuotaSelectionPolicy {
    static func effectiveSelection(
        payload: AgentUsagePayload?,
        persistedSelection: String,
        excluding: Set<String>,
        fallbackUnknownExplicit: Bool
    ) -> String {
        guard fallbackUnknownExplicit,
              !persistedSelection.isEmpty,
              persistedSelection != QuotaResolver.auto,
              QuotaResolver.resolve(
                  payload: payload, selection: persistedSelection, excluding: excluding) == nil
        else {
            return persistedSelection
        }
        return QuotaResolver.auto
    }

    static func resolve(
        payload: AgentUsagePayload?,
        persistedSelection: String,
        excluding: Set<String>,
        fallbackUnknownExplicit: Bool
    ) -> (clientId: String, window: UsageWindow)? {
        let selection = effectiveSelection(
            payload: payload,
            persistedSelection: persistedSelection,
            excluding: excluding,
            fallbackUnknownExplicit: fallbackUnknownExplicit)
        return QuotaResolver.resolve(
            payload: payload, selection: selection, excluding: excluding)
    }
}
