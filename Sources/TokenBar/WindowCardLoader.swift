import Foundation
import TokenBarCore

/// Everything `WindowUsageCard` needs, resolved once per load.
struct WindowCardData: Sendable {
    let clientId: String
    let windowLabel: String
    let resolution: WindowResolution
    let samples: [QuotaSample]
    /// Already filtered to this subscription's attributed usage, and already
    /// turned into bars and hit zones. Both are metric-free and O(messages),
    /// so they are computed once here rather than on every hover event — the
    /// per-body version pegged a CPU on a window holding a few thousand rows.
    let mine: [WindowMessage]
    let bars: [BarRect]
    let hits: [HitZone]
    /// Every window this client offers, so the card can switch between them.
    /// `cardId` is the stable identity; `label` is what the provider calls it.
    let candidates: [(cardId: String, label: String)]
    let cardId: String
    let nowMs: Int64
    /// Set when no session window could be found but one is expected to come
    /// back — a provider whose fetch failed. Vanishing silently in that case is
    /// indistinguishable from "you have no such subscription", which is the one
    /// reading the user cannot correct.
    var blockedBy: String?
}

/// Turns the selected quota window into card data: resolve the window, then —
/// and only then — ask the engine for the messages inside it.
enum WindowCardLoader {
    /// "<clientId>|<cardId>" — the same shape QuotaResolver uses for its own
    /// canonical selection, so the two cannot drift into different vocabularies.
    static let selectionKey = "tokenbar.window.card.selection"

    /// Only decides which window to show first when the user has not picked
    /// one: a session window is the question the card was built to answer.
    /// It no longer gates the scan — see the note in `load`.
    ///
    /// A dot-component match rather than a substring, so `weekly_scoped.fable.v1`
    /// cannot pass for a session window.
    ///
    /// Observed keys (2026-08-14, live payload): `session.v1`, `main.session.v1`,
    /// `weekly.v1`, `main.weekly.v1`, `weekly_scoped.fable.v1`, `chat.v1`,
    /// `billing.weekly.v1`, `additional.<hash>.primary.v1`.
    /// Identifies one window's curve/heatmap across every displayed client AND
    /// account. `"<clientId>|<cardId>"` alone collides the instant a second
    /// account of the same client offers an identically-carded window (both a
    /// "session.v1") — one account's curve would silently overwrite the
    /// other's in a shared dictionary keyed that way. The primary account
    /// keeps the exact two-part shape callers have always built by hand
    /// (`"\(clientId)|\(cardId)")`), so a plain client's curve key is
    /// unaffected; only an extra account's key grows a third segment.
    static func curveKey(clientId: String, accountKey: String?, cardId: String) -> String {
        AccountIdentity(clientId: clientId, accountKey: accountKey).windowKey(cardId: cardId)
    }

    static func isSessionClass(windowKey: String?) -> Bool {
        guard let windowKey else { return false }
        return windowKey.split(separator: ".").contains("session")
    }

    /// Within one agent. An explicit pick from the card's own buttons wins;
    /// otherwise prefer a session-class window — that is the question the card
    /// was built to answer — and fall back to the most depleted window there is.
    static func select(
        payload: AgentUsagePayload, clientId: String, chosen explicit: String? = nil
    ) -> (clientId: String, window: UsageWindow)? {
        // Primary account only: a client tab is a single detail card with a
        // single stored selection and no account component, so it can only
        // ever show one account's windows. An extra account has no tab of its
        // own — it surfaces as its own row in the Agent-limits overview.
        guard let agent = payload.agents.first(where: {
            $0.clientId == clientId && $0.accountKey == nil
        }), agent.error == nil
        else { return nil }
        return pick(windows: agent.uniqueCardWindows, clientId: clientId, chosen: explicit)
    }

    /// The same choice without the error guard.
    ///
    /// `select` refuses a client whose fetch failed, and must: presenting a
    /// stale window as the one running now is the misreading a user cannot
    /// correct. History is the opposite case. Those cycles are on disk, a
    /// rate-limited endpoint does not unwrite them, and the identity of the
    /// window they belong to is not time-sensitive — so refusing here reported
    /// "no earlier windows recorded" about a subscription with weeks of them.
    static func pickForHistory(
        payload: AgentUsagePayload, clientId: String, chosen explicit: String? = nil
    ) -> (clientId: String, window: UsageWindow)? {
        // Primary account only — see `select`.
        guard let agent = payload.agents.first(where: {
            $0.clientId == clientId && $0.accountKey == nil
        })
        else { return nil }
        return pick(windows: agent.uniqueCardWindows, clientId: clientId, chosen: explicit)
    }

    private static func pick(
        windows: [UsageWindow], clientId: String, chosen explicit: String?
    ) -> (clientId: String, window: UsageWindow)? {
        // A selection stored for a different agent is not stale state to clear,
        // it just does not apply here — each tab answers for its own client.
        if let explicit, let sep = explicit.firstIndex(of: "|"),
           String(explicit[explicit.startIndex..<sep]) == clientId,
           let window = windows.first(where: {
               $0.cardId == String(explicit[explicit.index(after: sep)...])
           }) {
            return (clientId, window)
        }
        if let session = windows.first(where: {
            isSessionClass(windowKey: $0.paceStatus.windowKey)
        }) {
            return (clientId, session)
        }
        let best = windows.filter { $0.remainingPercent.isFinite }
            .min { $0.remainingPercent < $1.remainingPercent }
        return best.map { (clientId, $0) }
    }

    static func resolution(
        window: UsageWindow, nowMs: Int64, firstUsageAfterReset: Int64?
    ) -> WindowResolution {
        let resetMs = window.resetsAt.flatMap(parseISO8601Ms)
        return WindowResolver.resolve(
            resetsAtMs: resetMs, durationMs: window.durationSeconds.map { $0 * 1000 },
            now: nowMs, firstUsageAfterReset: firstUsageAfterReset)
    }

    static func parseISO8601Ms(_ text: String) -> Int64? {
        let withFraction = ISO8601DateFormatter()
        withFraction.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        let plain = ISO8601DateFormatter()
        guard let date = withFraction.date(from: text) ?? plain.date(from: text)
        else { return nil }
        return Int64(date.timeIntervalSince1970 * 1000)
    }

    // MARK: - Stage 1: quota half, no scan

    /// Derives everything a scan is not needed for. Takes the payload as a
    /// value that is already in memory — it never fetches one, which is the
    /// whole reason this half is instant.
    static func quotaHalf(
        payload: AgentUsagePayload?, clientId: String, attempted: Bool,
        curve: (String, String?, String, UInt64) throws -> QuotaCurve?, nowMs: Int64
    ) -> WindowCardState {
        // `attempted` decides whether an absence is a wait or an answer. Both
        // guards used to return `.loading` either way — the first through a
        // ternary whose branches were identical, which is how it survived
        // review — so a quota fetch that kept failing left this card spinning
        // for ever while every other surface had learned to say so.
        guard let payload else {
            return attempted
                ? .blocked(clientId: clientId,
                           reason: "Quota could not be loaded.".localized)
                : .loading
        }
        // Primary account only — see `select`.
        guard let agent = payload.agents.first(where: {
            $0.clientId == clientId && $0.accountKey == nil
        })
        else {
            // Not "reported no windows" — this agent is not in the report at
            // all, and a client enabled a moment ago sits here until the next
            // poll. The wording has to be about the report, not about an answer
            // the agent never gave.
            return attempted
                ? .blocked(clientId: clientId,
                           reason: "Not in the latest quota report.".localized)
                : .loading
        }
        if let error = agent.error {
            return .blocked(clientId: clientId, reason: error)
        }
        // The third copy of the same rule, and the one the wording above was
        // written for: the agent IS in the report, has no error, and offers no
        // window the picker can settle on. The wire format allows an empty
        // `windows` array, so this is a real answer, not a wait.
        guard let selected = select(
            payload: payload, clientId: clientId,
            chosen: UserDefaults.standard.string(forKey: selectionKey))
        else {
            return attempted
                ? .blocked(clientId: clientId,
                           reason: "This agent reported no quota windows.".localized)
                : .loading
        }

        let window = selected.window
        let candidates = agent.uniqueCardWindows.map {
            (cardId: "\(clientId)|\($0.cardId)", label: $0.label)
        }
        let cardId = "\(clientId)|\(window.cardId)"

        // `nil` is "could not read", `[]` is "read fine, nothing in this
        // window". Only the second is a fact about the subscription, and only
        // the second may be stated. The engine fails the curve read closed on a
        // generation mismatch, and its bindings are replaced on every
        // publication — including ones the independent tray poller makes — so a
        // read issued against the generation this payload carries can expire
        // between the two. Rendering that as a terminal "no quota history" is
        // the same mistake as the one this file's `noQuotaHistory` comment
        // warns about, in the other direction.
        guard let samples = curveSamples(
            payload: payload, clientId: clientId, accountKey: nil, window: window,
            curve: curve, nowMs: nowMs)
        else { return .loading }
        guard !samples.isEmpty else {
            return .noQuotaHistory(
                clientId: clientId, windowLabel: window.label,
                candidates: candidates, cardId: cardId)
        }

        // No usage yet, so `.idle` here means "R is past and within one D" —
        // the scan may still turn it into `.inferred`.
        let resolved = resolution(window: window, nowMs: nowMs, firstUsageAfterReset: nil)
        var pending = false
        if case .idle = resolved { pending = true }

        return .quotaOnly(WindowQuotaHalf(
            clientId: clientId, cardId: cardId, windowLabel: window.label,
            candidates: candidates, resolution: resolved, samples: samples,
            resetMs: window.resetsAt.flatMap(parseISO8601Ms),
            durationMs: window.durationSeconds.map { $0 * 1000 },
            placementPending: pending, nowMs: nowMs,
            modelScope: window.modelScope), scanFailed: false)
    }

    /// The earliest interval start across every candidate window of every
    /// displayed agent. See `UnionScan` for why this one bound covers all
    /// three window states.
    static func unionStart(
        payload: AgentUsagePayload?, clients: [String], nowMs: Int64
    ) -> Int64? {
        guard let payload else { return nil }
        var earliest: Int64?
        for agent in payload.agents where agent.error == nil && clients.contains(agent.clientId) {
            for window in agent.uniqueCardWindows {
                guard let reset = window.resetsAt.flatMap(parseISO8601Ms),
                      let duration = window.durationSeconds.map({ $0 * 1000 })
                else { continue }
                // The same anchor-validity rule the card itself applies, and
                // the second place it is stated. An anchor more than one
                // duration old — the app was offline, or the provider is
                // reporting a long-dead cycle — resolves `.unavailable`, so
                // the card draws nothing for it; yet `reset - duration` still
                // dragged the union start back to wherever that dead reset
                // sat, and opening the tab paid for a scan of months of local
                // history to render a window that cannot display usage.
                //
                // `firstUsageAfterReset` is passed nil deliberately: both
                // `.unavailable` verdicts are decided before the resolver
                // consults usage, so the filter is exact without knowing the
                // usage the scan has not run yet.
                guard WindowResolver.resolve(
                    resetsAtMs: reset, durationMs: duration, now: nowMs,
                    firstUsageAfterReset: nil) != .unavailable
                else { continue }
                let start = reset - duration
                if earliest == nil || start < earliest! { earliest = start }
            }
        }
        return earliest
    }

    /// Stage 2, per client, from the one scan.
    static func usageHalf(
        quota: WindowQuotaHalf, scan: UnionScan, confirmed: [UsageAttribution.Record]
    ) -> (WindowQuotaHalf, WindowUsageHalf)? {
        // One predicate, used twice. The usage this card DISPLAYS is filtered
        // to messages declared against this subscription; the window it infers
        // was anchored to the earliest message in the whole union scan, so
        // another subscription's work could set this one's start — or invent an
        // active window for a subscription that has done nothing since its
        // reset. The chart would then begin before any usage it goes on to
        // count as this client's.
        func isMine(_ message: WindowMessage) -> Bool {
            UsageAttribution.resolve(
                client: message.client, provider: message.providerId,
                model: message.modelId, records: confirmed) == .assigned(quota.clientId)
        }

        // Refine `.idle` now that usage is known: the same resolver, this time
        // with the first usage after the reset.
        var resolved = quota.resolution
        if quota.placementPending, let reset = quota.resetMs {
            resolved = WindowResolver.resolve(
                resetsAtMs: reset, durationMs: quota.durationMs, now: quota.nowMs,
                firstUsageAfterReset: WindowResolver.firstUsageAfterReset(
                    messages: scan.slice(from: reset, to: quota.nowMs).filter(isMine),
                    resetMs: reset))
        }
        let settled = WindowQuotaHalf(
            clientId: quota.clientId, cardId: quota.cardId,
            windowLabel: quota.windowLabel, candidates: quota.candidates,
            resolution: resolved, samples: quota.samples,
            resetMs: quota.resetMs, durationMs: quota.durationMs,
            placementPending: false, nowMs: quota.nowMs,
            modelScope: quota.modelScope)

        guard let (start, end) = interval(resolved) else {
            return (settled, WindowUsageHalf(mine: [], bars: [], hits: []))
        }
        // A scan that starts after this window did cannot answer for it.
        guard scan.covers(start: start) else { return nil }

        // Two predicates, applied to different questions on purpose.
        //
        // `isMine` answers "is this the subscription's usage" and decides where
        // the window IS — it is what `firstUsageAfterReset` above consumes.
        // `inScope` answers "is this the model the allowance counts" and
        // decides only what the card DISPLAYS.
        //
        // The scope is deliberately NOT applied to placement. A scoped weekly
        // window is anchored by the provider's own reset, so narrowing the
        // placement probe buys no accuracy — while a scope join that matches
        // nothing would turn the card blank instead of merely empty, which
        // moves a naming mismatch from "we found no usage" to "there is no
        // window". The blast radius of the heuristic stays inside the totals.
        let subscription = scan.slice(from: start, to: min(end, quota.nowMs)).filter(isMine)
        let mine = QuotaHistoryFold.inScope(subscription, quota.modelScope)
        let geo = WindowCardGeometry.usageGeometry(
            windowStartMs: start, windowEndMs: end, nowMs: min(quota.nowMs, end),
            samples: quota.samples, messages: mine)
        return (settled, WindowUsageHalf(
            mine: mine, bars: geo.bars, hits: geo.hits,
            undatedCount: scan.undatedCount,
            scopeMatchedNothing: quota.modelScope != nil
                && mine.isEmpty && !subscription.isEmpty))
    }

    static func interval(_ s: WindowResolution) -> (start: Int64, end: Int64)? {
        switch s {
        case let .active(start, end), let .inferred(start, end): return (start, end)
        case .idle, .unavailable: return nil
        }
    }

    /// Quota readings inside the resolved interval. `sampledAt` is in SECONDS —
    /// the decoder subtracts `durationSeconds` from `resetAt` directly — while
    /// every window bound here is milliseconds. Comparing the two units gives
    /// an empty series that looks exactly like a missing data path.
    /// Every recorded reset cycle of the window this client currently shows,
    /// newest first. Reads the RAW curve rather than `curveSamples`, which
    /// bounds itself to the active window and would therefore return exactly
    /// the one cycle the history is not about.
    ///
    /// Returns nil when the reading could not be obtained, `[]` when it was
    /// obtained and there is no history — the same distinction, for the same
    /// reason, as `curveSamples`.
    static func cycles(
        payload: AgentUsagePayload?, clientId: String,
        curve read: (String, String?, String, UInt64) throws -> QuotaCurve?
    ) -> [QuotaCycle]? {
        // Split, not one guard: no payload is "could not be obtained" and must
        // be nil, while a payload that simply offers no history is `[]`. Folding
        // both into `[]` is what let the history card state "no earlier windows"
        // before the first fetch had returned.
        guard let payload else { return nil }
        guard let selected = pickForHistory(
                  payload: payload, clientId: clientId,
                  chosen: UserDefaults.standard.string(forKey: selectionKey)),
              let key = selected.window.paceStatus.windowKey,
              let generation = payload.publicationGeneration
        else { return [] }
        let attempt: QuotaCurve?
        // Primary account only — see `select`.
        do { attempt = try read(clientId, nil, key, generation) } catch { return nil }
        guard let curve = attempt else { return [] }
        // Capped: this list is what the history card draws AND what bounds the
        // union scan, through its oldest entry's `evidenceStartMs`.
        return QuotaHistoryFold.considered(QuotaHistoryFold.cycles(
            points: curve.points))
    }

    /// The model scope of one window, addressed the way every other surface
    /// addresses a window: by its `"<clientId>|<cardId>"`.
    ///
    /// The history rows and the equivalence estimate are keyed that way and
    /// have no `UsageWindow` in hand, so without this they would each re-derive
    /// the scope from the window key string — three parsers for one fact, which
    /// is how they drift.
    static func modelScope(payload: AgentUsagePayload?, cardId: String?) -> String? {
        guard let payload, let cardId,
              let separator = cardId.firstIndex(of: "|")
        else { return nil }
        let clientId = String(cardId[cardId.startIndex..<separator])
        let card = String(cardId[cardId.index(after: separator)...])
        return payload.agents
            .first { $0.clientId == clientId }?
            .uniqueCardWindows
            .first { $0.cardId == card }?
            .modelScope
    }

    /// The `"<clientId>|<cardId>"` the card is currently showing, or nil when
    /// nothing can be selected. One statement of "which window is on screen",
    /// so retention decisions elsewhere compare against the same answer the
    /// card itself resolves.
    static func selectedCardId(payload: AgentUsagePayload?, clientId: String) -> String? {
        guard let payload,
              let selected = select(
                  payload: payload, clientId: clientId,
                  chosen: UserDefaults.standard.string(forKey: selectionKey))
        else { return nil }
        return "\(clientId)|\(selected.window.cardId)"
    }

    /// The `"<clientId>|<cardId>"` whose history `cycles` above returns.
    ///
    /// Deliberately NOT `selectedCardId`: that one resolves through `select`,
    /// this and `cycles` resolve through `pickForHistory`, and the two differ
    /// on exactly the cases the history card is about — a window the live
    /// picker refuses still has a recorded history to draw. A surface keyed on
    /// this therefore cannot disagree with the list it is keying.
    ///
    /// It is the stored preference RESOLVED, which is the distinction that
    /// matters to a caller wanting to know whether the history changed
    /// underneath it. The raw `selectionKey` answers neither direction: a
    /// choice saved for another client leaves this client's window untouched,
    /// and a window disappearing from the payload changes this without the
    /// preference moving at all.
    static func historyCardId(payload: AgentUsagePayload?, clientId: String) -> String? {
        guard let payload,
              let selected = pickForHistory(
                  payload: payload, clientId: clientId,
                  chosen: UserDefaults.standard.string(forKey: selectionKey))
        else { return nil }
        return "\(clientId)|\(selected.window.cardId)"
    }

    /// Not private: `AgentLimitsCard`'s sparkline needs the same series for
    /// every window a client offers, not just the one the card selected. Same
    /// function so the two surfaces cannot disagree about which readings belong
    /// to a window.
    /// Returns `nil` when the reading could not be obtained at all, and `[]`
    /// when it was obtained and this window has none. Callers must not collapse
    /// the two: only the second says anything about the subscription.
    // No default: a caller that omits this reads the primary account's curve
    // and nothing reports the mistake, so the argument is required.
    static func curveSamples(
        payload: AgentUsagePayload, clientId: String, accountKey: String?,
        window: UsageWindow,
        curve read: (String, String?, String, UInt64) throws -> QuotaCurve?, nowMs: Int64
    ) -> [QuotaSample]? {
        // A window the payload cannot key, or a payload with no generation, is
        // a settled "nothing to read" rather than a failed read.
        guard let key = window.paceStatus.windowKey,
              let generation = payload.publicationGeneration
        else { return [] }
        // The read itself is the part that can fail transiently. `try?` would
        // flatten the throw and the legitimate nil into one value, which is
        // precisely the conflation this function exists to undo.
        let attempt: QuotaCurve?
        do { attempt = try read(clientId, accountKey, key, generation) } catch { return nil }
        guard let curve = attempt else { return [] }

        // Bounded by the window the provider anchors, not by a resolution that
        // does not exist yet at stage 1.
        guard let reset = window.resetsAt.flatMap(parseISO8601Ms),
              let duration = window.durationSeconds.map({ $0 * 1000 })
        else { return [] }
        let lo = (reset - duration) / 1000, hi = nowMs / 1000
        // NOT filtered by reset cycle, deliberately.
        //
        // A foreign-cycle sample would let the interpolation draw a fall to
        // zero and back — a refill that never happened — so a guard looks
        // obviously right. Measured 2026-08-16, it is not:
        //
        //   * No window admits two cycles. Under usage-triggered windows
        //     `R - D = t0` and the previous cycle ends at or before `t0`, so
        //     the time range already separates them. All four live series
        //     returned exactly one cycle.
        //   * Both attempts at a guard broke working cards. Comparing against
        //     the parsed `resetsAt` fails because the engine quantises
        //     `resetAt` to the minute (codex differed by 62s, grok by 19s);
        //     comparing against `activeResetAt` fails too — it does not mean
        //     the cycle these points belong to. Each version silently filtered
        //     every sample and the card fell to "no quota history".
        //
        // No guard for an unobserved case, twice implemented wrongly, each
        // time causing the failure that IS observed. If a mixed-cycle window
        // ever appears, the fix belongs where the cycle is known: the engine.
        let persisted = curve.points
            .filter { $0.sampledAt >= lo && $0.sampledAt <= hi }
            .sorted { $0.sampledAt < $1.sampledAt }
            .map { QuotaSample(atMs: $0.sampledAt * 1000, usedPercent: $0.usedPercent) }
        return persisted + liveReading(window: window, after: persisted.last, nowMs: nowMs)
    }

    /// The reading the payload is carrying right now, which the store may not
    /// have written yet.
    ///
    /// The history is a phase PROFILE, not a time series: it keeps one sample
    /// per 48th of a cycle, replacing within the bucket rather than appending.
    /// On a 5-hour session window that is a sample every six minutes and looks
    /// like a curve; on a 7-day weekly window it is one every 3.5 hours, so a
    /// freshly reset window shows a single point for hours while the headline
    /// above it moves 97% → 95%. The card was drawing only what had been
    /// persisted, and so reported "1 reading" about an app that had taken ten.
    ///
    /// Same admission rule as the engine's recorder — `0 < used <= 100` — so a
    /// window at full allowance does not plant a point at zero, and the same
    /// treatment as a bucket replacement: skipped when the newest persisted
    /// sample already carries this value, since drawing a flat segment to it
    /// would claim a measurement nobody took.
    private static func liveReading(
        window: UsageWindow, after last: QuotaSample?, nowMs: Int64
    ) -> [QuotaSample] {
        let used = window.usedPercent
        guard used > 0, used <= 100 else { return [] }
        if let last, last.atMs >= nowMs || abs(last.usedPercent - used) < 0.000_001 {
            return []
        }
        return [QuotaSample(atMs: nowMs, usedPercent: used)]
    }
}
