import Foundation

/// Picks which quota window the menu bar displays. The canonical selection
/// string is `"auto"` or `"<clientId>|<cardId>"`. A legacy label in the
/// second component is migrated when the current payload makes it unique.
public enum QuotaResolver {
    public static let auto = "auto"

    /// Builds the canonical persisted selection for one quota card.
    public static func selection(clientId: String, cardId: String) -> String {
        "\(clientId)|\(cardId)"
    }

    /// Canonicalizes a persisted selection against the current payload.
    ///
    /// Empty and `auto` selections normalize to `auto`. Before a payload is
    /// available, a well-formed explicit selection is preserved so a refresh
    /// cannot erase an otherwise valid persisted choice. Once a payload exists,
    /// exact card IDs win and a unique legacy label is migrated. A well-formed
    /// unmatched selection remains explicit: the payload can be partial during a
    /// provider failure, so silently changing it to Auto would show another
    /// provider instead of letting callers retain the selected source's last-good
    /// value.
    ///
    /// `accountKey` picks WHICH account of `parsed.clientId` this selection
    /// belongs to — nil (the default) is the primary account, which is what
    /// every existing single-string persisted selection means and must keep
    /// meaning. A card ID string alone cannot name the account: two accounts
    /// of the same client can both offer a window called `session.v1`, so an
    /// extra account's selection is kept in its OWN storage key by the caller
    /// and resolved by passing that account's key here explicitly.
    public static func canonicalSelection(
        payload: AgentUsagePayload?, selection: String, accountKey: String? = nil
    ) -> String {
        guard let parsed = parseExplicitSelection(selection) else { return auto }
        guard let payload else { return selection }
        guard let agent = payload.agents.first(where: {
            $0.clientId == parsed.clientId && $0.accountKey == accountKey
        }) else {
            return selection
        }

        // The RAW card view: a persisted pre-v3 selection holds the label the
        // provider sent, and whether that label was ambiguous is a fact about
        // what the provider sent. `uniqueCardWindows` qualifies a repeated
        // label with its window's duration, which can leave exactly one window
        // still carrying the raw text and migrate a selection that used to
        // match two. Card IDs are identical in both views, so the value this
        // returns is unchanged for every selection that was already unique.
        let windows = agent.rawCardWindows
        if let exact = windows.first(where: { $0.cardId == parsed.value }) {
            return Self.selection(clientId: agent.clientId, cardId: exact.cardId)
        }

        let labelMatches = windows.filter { $0.label == parsed.value }
        guard labelMatches.count == 1, let migrated = labelMatches.first else { return selection }
        return Self.selection(clientId: agent.clientId, cardId: migrated.cardId)
    }

    /// `excluding` is the set of client ids to skip in AUTO mode only (the
    /// user's tab-hidden ∪ limits-hidden clients) — so the menu-bar quota can't
    /// surface a client the popover hides. An EXPLICIT `clientId|cardId`
    /// selection is always honored, even for an excluded client (the user
    /// deliberately picked it as the tray source). Empty set = pre-hide
    /// behavior, byte-identical.
    /// `accountKey` is the account this call resolves an EXPLICIT selection
    /// for — see `canonicalSelection`. It plays no part in AUTO mode, which
    /// already considers every account of every client and returns whichever
    /// one it picks.
    public static func resolve(
        payload: AgentUsagePayload?, selection: String, accountKey: String? = nil,
        excluding: Set<String> = []
    ) -> (clientId: String, accountKey: String?, window: UsageWindow)? {
        guard let payload else { return nil }
        let canonical = canonicalSelection(
            payload: payload, selection: selection, accountKey: accountKey)
        if canonical == Self.auto {
            guard let best = autoCandidate(payload: payload, excluding: excluding) else {
                return nil
            }
            return (best.identity.clientId, best.identity.accountKey, best.window)
        }

        guard let parsed = parseExplicitSelection(canonical),
              let agent = payload.agents.first(where: {
                  $0.clientId == parsed.clientId && $0.accountKey == accountKey
              }),
              let window = agent.uniqueCardWindows.first(where: { $0.cardId == parsed.value })
        else { return nil }
        return (agent.clientId, agent.accountKey, window)
    }

    /// True when `resolve` returned nil ONLY because the exclusion removed every
    /// otherwise-resolvable auto candidate (there IS a healthy window, but all
    /// of them belong to excluded clients). Lets a caller distinguish "all
    /// candidates hidden" from "no payload / fetch failed / no healthy window":
    /// in the former it must suppress a stale cache fallback (the hidden
    /// client's last reading) rather than keep showing it. Only meaningful for
    /// the auto/empty selection — an explicit pick ignores the exclusion, so
    /// this returns false for it (and for an empty exclusion or no payload).
    public static func excludedAllCandidates(
        payload: AgentUsagePayload?, selection: String, excluding: Set<String>
    ) -> Bool {
        guard !excluding.isEmpty else { return false }
        guard let payload,
              canonicalSelection(payload: payload, selection: selection) == Self.auto
        else { return false }
        guard autoCandidate(payload: payload, excluding: []) != nil else { return false }
        return autoCandidate(payload: payload, excluding: excluding) == nil
    }

    private static func parseExplicitSelection(
        _ raw: String
    ) -> (clientId: String, value: String)? {
        guard !raw.isEmpty, raw != auto else { return nil }
        let parts = raw.split(
            separator: "|", maxSplits: 1, omittingEmptySubsequences: false)
        guard parts.count == 2 else { return nil }
        let clientId = String(parts[0])
        let value = String(parts[1])
        guard !clientId.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        else { return nil }
        return (clientId, value)
    }

    /// Considers every account of every client — `payload.agents` is a flat
    /// list of snapshots, not deduplicated by client id, so a second Claude
    /// account is just another entry the same loop already visits — and
    /// returns whichever single window is tightest across all of them.
    private static func autoCandidate(
        payload: AgentUsagePayload, excluding: Set<String>
    ) -> (identity: AccountIdentity, window: UsageWindow)? {
        var best: (identity: AccountIdentity, window: UsageWindow)?
        for agent in payload.agents
        where agent.error == nil && !excluding.contains(agent.clientId) {
            // Rust omits malformed percentage readings before serialization.
            // Do not reject every `.invalidEvidence` pace status here: a reset or
            // duration error can coexist with a valid remaining-percentage gauge.
            for window in agent.uniqueCardWindows where window.remainingPercent.isFinite {
                if best == nil || window.remainingPercent < best!.window.remainingPercent {
                    best = (agent.accountIdentity, window)
                }
            }
        }
        return best
    }
}
