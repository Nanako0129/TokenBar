import AppKit
import SwiftUI
import TokenBarCore

/// Card density: 'full' (with pace line) or 'classic' (compact), mirroring
/// LimitsLayout in settings.ts.
enum LimitsLayout: String, CaseIterable {
    case full, classic
}

/// OAuth quota cards per agent: usage-window bars with gauge colors, reset
/// text and a pace marker. Port of AgentLimitsCard.tsx. The pace mode, fill
/// direction and layout density read the same defaults the settings panel
/// (later phase) will edit.
struct AgentLimitsCard: View {
    /// Clients requested by the active tab.
    let clients: [String]
    let trace: [TraceBucket]
    let agentUsage: AgentUsagePayload?
    var title = "Agent limits"
    var note = "OAuth quota"
    /// When true, show only the passed `clients` (single-client view) instead
    /// of unioning in every agent that has a quota snapshot.
    var restrict = false
    /// When true, cards can be reordered by dragging their grip handle; the
    /// order persists to UserDefaults. Only the multi-agent overview opts in.
    var reorderable = false

    /// Bar fills by used (true) or remaining (false).
    @AppStorage("tokenbar.limits.asUsed") private var asUsed = false
    @AppStorage("tokenbar.limits.paceMode") private var paceModeRaw = PaceMode.historical.rawValue
    @AppStorage("tokenbar.limits.layout") private var layoutRaw = LimitsLayout.full.rawValue
    /// Saved drag order (shared with the "Client tabs (top bar)" order in Settings).
    /// Reordering providers in Settings → Client tabs now also reorders the
    /// quota cards shown in Overview → Agent limits (and vice-versa via drag).
    @AppStorage(ClientRegistry.tabOrderKey) private var orderRaw = ""
    /// Per-client Agent-limits visibility, independent of tab visibility.
    @AppStorage(ClientRegistry.limitsHiddenKey) private var limitsHiddenRaw = ""

    @State private var dragId: String?
    @State private var overId: String?
    @State private var cardFrames: [String: CGRect] = [:]

    private var paceMode: PaceMode { PaceMode(rawValue: paceModeRaw) ?? .historical }
    private var classic: Bool { LimitsLayout(rawValue: layoutRaw) ?? .full == .classic }

    /// Pure state presentation shared by every AgentLimitsCard consumer.
    enum PacePresentation {
        static let learningHistoryText = "Learning history · Linear estimate"
        static let learningDurationText = "Learning reset duration"
        static let linearText = "Linear"
        static let legacyText = "Pace unavailable · legacy data"

        static func statusText(
            state: UsagePaceState,
            reason: UsagePaceUnavailableReason?,
            mode: PaceMode
        ) -> String? {
            guard mode != .off else { return nil }
            switch state {
            case .learningHistory:
                return mode == .historical ? learningHistoryText : linearText
            case .learningDuration:
                return learningDurationText
            case .available:
                return mode == .linear ? linearText : nil
            case .unavailable:
                return unavailableText(reason)
            case .legacyMissing:
                return legacyText
            }
        }

        static func unavailableText(_ reason: UsagePaceUnavailableReason?) -> String {
            guard let reason else { return "Pace unavailable · unavailable reason" }
            switch reason {
            case .windowIdentity:
                return "Pace unavailable · unknown quota window"
            case .missingReset:
                return "Pace unavailable · missing reset"
            case .invalidEvidence:
                return "Pace unavailable · invalid quota data"
            case .accountScope:
                return "Pace unavailable · account identity unavailable"
            case .storeCapacity:
                return "Pace unavailable · history storage full"
            case .history:
                return "Pace unavailable · history unavailable"
            case .nonRecurring:
                return "Pace unavailable · non-recurring quota"
            }
        }

        static func isHistoricalDeficit(_ pace: UsagePace?) -> Bool {
            pace?.isHistoricalDeficit == true
        }
    }

    /// Placeholder window labels for agents we know carry quotas but have no
    /// snapshot yet (LIMIT_ROWS in the web card).
    private static let placeholderRows: [String: [String]] = [
        "codex": ["Session", "Weekly"],
        "claude": ["Session", "Weekly"],
        "gemini": ["Pro", "Flash"],
        "grok": ["Weekly"],
    ]

    /// Maps opencode subscription labels (from the backend) to the agent
    /// client ids whose quota cards represent them.
    private static let subLabelToId: [String: String] = [
        "Codex": "codex", "Claude": "claude", "Copilot": "copilot",
        "Gemini": "antigravity",
    ]

    /// Every client id that can show a row in the multi-agent Agent-limits
    /// card. Thin wrapper over `ClientRegistry.knownLimitsClients` (the one
    /// implementation) that supplies this card's placeholder-row keys, so the
    /// registry-level lists and the card agree on the universe.
    static func knownClientIds(agentUsage: AgentUsagePayload?, present: [String]) -> [String] {
        ClientRegistry.knownLimitsClients(
            present: present,
            quotaIds: (agentUsage?.agents ?? []).map(\.clientId),
            placeholders: Set(placeholderRows.keys))
    }

    private var snapshots: [String: AgentUsageSnapshot] {
        var dict = Dictionary(
            (agentUsage?.agents ?? []).map { ($0.clientId, $0) },
            uniquingKeysWith: { first, _ in first })
        // Antigravity CLI shares the Antigravity IDE's account and quota, so it
        // gets no snapshot of its own. In its single-client view, surface the
        // Antigravity snapshot under its id so the card still shows the quota.
        // Only in `restrict` mode — the overview already renders Antigravity's
        // own card, so aliasing there would duplicate it.
        if restrict, dict["antigravity-cli"] == nil, let shared = dict["antigravity"] {
            dict["antigravity-cli"] = shared
        }
        return dict
    }

    /// Clients whose live tail shows activity right now.
    private var liveClients: Set<String> {
        Set(
            trace.filter { $0.tokensPerMin > 0 }
                .map { Self.normalizeTraceClient($0.client) })
    }

    private var opencodeSubs: [String] { agentUsage?.opencodeSubscriptions ?? [] }

    /// opencode is a router with no quota of its own; its client view instead
    /// shows the cards of the subscriptions it's authed against.
    private var opencodeView: Bool { restrict && clients.contains("opencode") }

    private var baseClients: [String] {
        let snapshots = self.snapshots
        if opencodeView {
            return opencodeSubs
                .map { Self.subLabelToId[$0] ?? $0.lowercased() }
                .filter { snapshots[$0] != nil }
        }
        func known(_ id: String) -> Bool {
            Self.placeholderRows[id] != nil || snapshots[id] != nil
        }
        if restrict { return clients.filter(known) }
        var seen = Set<String>()
        var base = (clients.filter(known) + (agentUsage?.agents.map(\.clientId) ?? []))
            .filter { seen.insert($0).inserted }

        // Apply the same hide logic as Client tabs, plus the independent
        // per-client Agent-limits toggle: either one keeps a client out of
        // the multi-agent limits card.
        if reorderable {
            let limitsHidden = ClientRegistry.parseIdSet(limitsHiddenRaw)
            let hidden = ClientRegistry.hiddenClients().union(limitsHidden)
            base = base.filter { !hidden.contains($0) }
        }
        return base
    }

    /// Saved drag order applied; ids without a saved position keep their
    /// natural order at the end. Disabled in non-reorderable views. Reads the
    /// observed `orderRaw` so a drag re-sorts the cards reactively.
    private var visibleClients: [String] {
        reorderable ? ClientRegistry.orderedClients(baseClients, orderRaw: orderRaw) : baseClients
    }

    // The master `tokenbar.limits.enabled` gate lives at every call site
    // (OverviewView, SettingsWindowView) rather than inside `body`, so an
    // "off" card leaves no structural gap in its parent VStack.
    var body: some View {
        DashCard(title, trailing: { noteLabel }) {
            if opencodeView {
                integrationLine("↔ Routes through opencode")
            } else if !restrict && !opencodeSubs.isEmpty {
                integrationLine("opencode also taps: \(opencodeSubs.joined(separator: " · "))")
            }
            let visible = visibleClients
            if visible.isEmpty {
                Text(
                    opencodeView && !opencodeSubs.isEmpty
                        ? "Subscriptions: \(opencodeSubs.joined(separator: " · "))"
                        : "No supported agents yet"
                )
                .font(.caption)
                .foregroundStyle(.tertiary)
                .frame(maxWidth: .infinity, alignment: .center)
                .padding(.vertical, 8)
            } else {
                VStack(spacing: 12) {
                    ForEach(visible, id: \.self) { id in
                        agentSection(id, visible: visible)
                    }
                }
                .coordinateSpace(name: Self.dragSpace)
                .onPreferenceChange(CardFramesKey.self) { cardFrames = $0 }
            }
        }
    }

    private var noteLabel: some View {
        Text(note)
            .font(.caption2)
            .foregroundStyle(.tertiary)
    }

    private func integrationLine(_ text: String) -> some View {
        Text(text)
            .font(.caption2)
            .foregroundStyle(.secondary)
    }

    // MARK: - Drag reorder

    private static let dragSpace = "limits-cards"

    private struct CardFramesKey: PreferenceKey {
        static let defaultValue: [String: CGRect] = [:]
        static func reduce(value: inout [String: CGRect], nextValue: () -> [String: CGRect]) {
            value.merge(nextValue(), uniquingKeysWith: { $1 })
        }
    }

    /// Move `from` to the `to` card's slot, direction-aware. Delegates to the
    /// single `ClientRegistry.reorder` implementation (SelfTest asserts against
    /// this symbol; keeping the wrapper keeps those checks addressing the card).
    static func reorder(_ list: [String], from: String, to: String) -> [String] {
        ClientRegistry.reorder(list, from: from, to: to)
    }

    /// Which edge of a card the drop line sits on, matching the
    /// direction-aware insert.
    private func dropEdge(_ id: String, in visible: [String]) -> VerticalEdge? {
        guard let dragId, overId == id, dragId != id,
              let fromI = visible.firstIndex(of: dragId), let toI = visible.firstIndex(of: id)
        else { return nil }
        return fromI < toI ? .bottom : .top
    }

    private func dragGesture(for id: String, visible: [String]) -> some Gesture {
        DragGesture(minimumDistance: 2, coordinateSpace: .named(Self.dragSpace))
            .onChanged { value in
                dragId = id
                let over = cardFrames.first { $0.value.contains(value.location) }?.key
                overId = (over != nil && over != id) ? over : nil
            }
            .onEnded { _ in
                if let over = overId, over != id {
                    // `visible` is a subset of the shared tab order (it excludes
                    // hidden and non-quota clients). Merge the reordered subset
                    // back into the full saved order so off-screen ids keep
                    // their slots instead of being dropped from the key.
                    let full = ClientRegistry.parseIdList(orderRaw)
                    let merged = ClientRegistry.mergeReorder(
                        full: full, visible: visible, from: id, to: over)
                    orderRaw = merged.joined(separator: ",")
                }
                dragId = nil
                overId = nil
            }
    }

    // MARK: - Per-agent section

    @ViewBuilder private func agentSection(_ id: String, visible: [String]) -> some View {
        let style = ClientRegistry.style(id)
        let snapshot = snapshots[id]
        let uniqueWindows = snapshot?.uniqueCardWindows ?? []
        let isLive = liveClients.contains(id)
        let edge = dropEdge(id, in: visible)
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                if reorderable {
                    Text("⠿")
                        .font(.caption)
                        .foregroundStyle(dragId == id ? .primary : .tertiary)
                        .help("Drag to reorder")
                        .gesture(dragGesture(for: id, visible: visible))
                }
                AgentIconView(clientId: id, size: 14)
                Text(style.displayName)
                    .font(.caption.weight(.semibold))
                Spacer()
                statusBadge(snapshot: snapshot, isLive: isLive)
            }
            if snapshot?.source == "unconfigured" {
                setupPrompt()
            } else {
                if let detail = detailText(snapshot) {
                    Text(detail)
                        .font(.caption2)
                        .foregroundStyle(snapshot?.error != nil ? .red : .secondary)
                        .lineLimit(2)
                        .help(snapshot?.error ?? detail)
                }
                VStack(spacing: 8) {
                    if !uniqueWindows.isEmpty {
                        ForEach(uniqueWindows, id: \.cardId) { window in
                            windowRow(window, brand: style.color)
                                .id("\(id):\(window.cardId)")
                        }
                    } else {
                        ForEach(Self.placeholderRows[id] ?? ["Limit"], id: \.self) { label in
                            placeholderRow(label, brand: style.color)
                        }
                    }
                }
            }
        }
        .opacity(dragId == id ? 0.5 : 1)
        .overlay(alignment: edge == .top ? .top : .bottom) {
            if edge != nil {
                Rectangle()
                    .fill(Color.accentColor)
                    .frame(height: 2)
                    .offset(y: edge == .top ? -6 : 6)
            }
        }
        .background(
            GeometryReader { geo in
                Color.clear.preference(
                    key: CardFramesKey.self,
                    value: [id: geo.frame(in: .named(Self.dragSpace))])
            })
    }

    /// Keychain command that hands TokenBar a Claude setup-token when the
    /// automatic shell/env detection can't reach it (e.g. a plain `~/.zshrc`
    /// export a Finder-launched app never inherits).
    // `-U` updates the item if it already exists (so re-pasting after a wrong
    // token, or rotating the token, works instead of failing). `-w` is given last
    // with no value on purpose: `security(1)` then prompts for the token
    // interactively, so it never lands in shell history or process args.
    private static let claudeSetupCommand =
        #"security add-generic-password -U -a "$USER" -s tokenbar-claude-oauth-token -w"#

    /// Setup prompt shown for Claude when no credential is configured at all
    /// (source "unconfigured"), instead of a red "credentials not found" error.
    @ViewBuilder private func setupPrompt() -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Using a Claude `setup-token`? TokenBar auto-detects `CLAUDE_CODE_OAUTH_TOKEN` from your login shell. If limits don't appear, store the token in Keychain — run this, then paste the token at the prompt:")
                .font(.caption2)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            HStack(alignment: .top, spacing: 6) {
                Text(Self.claudeSetupCommand)
                    .font(.system(.caption2, design: .monospaced))
                    .textSelection(.enabled)
                    .lineLimit(3)
                    .truncationMode(.middle)
                    .padding(6)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(RoundedRectangle(cornerRadius: 6).fill(Color.primary.opacity(0.06)))
                Button {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(Self.claudeSetupCommand, forType: .string)
                } label: {
                    Image(systemName: "doc.on.doc").font(.caption2)
                }
                .buttonStyle(.borderless)
                .help("Copy command")
            }
        }
    }

    private func statusBadge(snapshot: AgentUsageSnapshot?, isLive: Bool) -> some View {
        let text: String
        var color: Color = .secondary
        if snapshot?.source == "unconfigured" {
            // Not set up yet -- neutral prompt, not an alarming red error.
            text = "Set up"
        } else if snapshot?.error != nil {
            text = "Error"
            color = .red
        } else if let snapshot, !snapshot.uniqueCardWindows.isEmpty {
            text = snapshot.source.uppercased()
        } else if isLive {
            text = "Live"
            color = .green
        } else {
            text = "No quota"
        }
        return Text(text)
            .font(.caption2.weight(.medium))
            .foregroundStyle(color)
    }

    private func detailText(_ snapshot: AgentUsageSnapshot?) -> String? {
        guard let snapshot else { return nil }
        if let error = snapshot.error { return error }
        let parts = [snapshot.identity?.email, snapshot.identity?.plan].compactMap(\.self)
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }

    // MARK: - Window rows

    /// A quota bar reads green when healthy, ambers under 25% left and reds
    /// under 10% (tokscale/codexbar Usage view). No quota signal → brand color.
    private func gaugeColor(remaining: Double?, brand: String) -> Color {
        guard let remaining else { return Color(hex: brand) }
        if remaining <= 10 { return Color(red: 0.937, green: 0.267, blue: 0.267) }
        if remaining <= 25 { return Color(red: 0.961, green: 0.620, blue: 0.043) }
        return Color(red: 0.133, green: 0.773, blue: 0.369)
    }

    @ViewBuilder private func windowRow(_ window: UsageWindow, brand: String) -> some View {
        let remaining = min(100, max(0, window.remainingPercent))
        let used = min(100, max(0, window.usedPercent))
        // Pace is suppressed entirely in the classic layout and when the user
        // turns it off; otherwise it follows the chosen mode.
        let pace = classic ? nil : UsagePace.compute(window: window, mode: paceMode)
        // The bar fills by used (counting up) or remaining (counting down)
        // per the setting; the pace marker sits on the same axis so it lines
        // up with the fill either way.
        let fill = asUsed ? used : remaining
        let leftLabel = asUsed
            ? "\(Int(used.rounded()))% used"
            : "\(Int(remaining.rounded()))% left"
        let gauge = gaugeColor(remaining: remaining, brand: brand)

        if classic {
            VStack(alignment: .leading, spacing: 3) {
                HStack {
                    Text(window.label)
                        .font(.caption2.weight(.medium))
                    Spacer()
                    Text(window.resetText ?? leftLabel)
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                }
                bar(fillPercent: fill, color: gauge, paceLeft: nil, paceIsDeficit: false)
                if window.resetText != nil {
                    Text(leftLabel)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
            }
        } else {
            VStack(alignment: .leading, spacing: 3) {
                HStack {
                    Text(window.label)
                        .font(.caption2.weight(.medium))
                    Spacer()
                    if let reset = window.resetText {
                        Text(reset)
                            .font(.caption2)
                            .foregroundStyle(.tertiary)
                    }
                }
                bar(
                    fillPercent: fill, color: gauge,
                    paceLeft: pace.map {
                        let left = asUsed ? $0.expectedUsedPercent : 100 - $0.expectedUsedPercent
                        return min(100, max(0, left))
                    },
                    paceIsDeficit: Self.PacePresentation.isHistoricalDeficit(pace))
                paceFooter(window: window, leftLabel: leftLabel, pace: pace)
            }
        }
    }

    @ViewBuilder private func paceFooter(
        window: UsageWindow, leftLabel: String, pace: UsagePace?
    ) -> some View {
        let status = Self.PacePresentation.statusText(
            state: window.paceStatus.state,
            reason: window.paceStatus.reason,
            mode: paceMode)
        // Historical ETA/lasts and risk are composed together so a visible
        // risk suppresses the generic "Lasts until reset" phrase when the
        // backend reports both.
        let projection = pace.map {
            UsagePace.presentation(window: window, mode: paceMode, pace: $0)
        }
        let projectionText = [projection?.etaText, projection?.riskText]
            .compactMap(\.self).joined(separator: " · ")
        let paceText = [status, pace?.label]
            .compactMap(\.self).joined(separator: " · ")

        if paceText.isEmpty {
            paceLeftLabel(leftLabel)
        } else if projectionText.isEmpty {
            HStack(spacing: 8) {
                paceLeftLabel(leftLabel)
                Spacer(minLength: 8)
                paceTextLabel(paceText, pace: pace)
            }
        } else {
            ViewThatFits(in: .horizontal) {
                HStack(spacing: 8) {
                    paceLeftLabel(leftLabel)
                        .fixedSize(horizontal: true, vertical: false)
                    Spacer(minLength: 8)
                    paceTextLabel("\(paceText) · \(projectionText)", pace: pace)
                        .lineLimit(1)
                        .fixedSize(horizontal: true, vertical: false)
                }

                VStack(alignment: .trailing, spacing: 1) {
                    HStack(spacing: 8) {
                        paceLeftLabel(leftLabel)
                        Spacer(minLength: 8)
                        paceTextLabel(paceText, pace: pace)
                    }
                    paceTextLabel(projectionText, pace: pace)
                        .multilineTextAlignment(.trailing)
                        .frame(maxWidth: .infinity, alignment: .trailing)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
        }
    }

    private func paceLeftLabel(_ text: String) -> some View {
        Text(text)
            .font(.caption2)
            .foregroundStyle(.secondary)
    }

    private func paceTextLabel(_ text: String, pace: UsagePace?) -> some View {
        Text(text)
            .font(.caption2)
            .foregroundStyle(
                Self.PacePresentation.isHistoricalDeficit(pace)
                    ? AnyShapeStyle(.orange) : AnyShapeStyle(.tertiary))
    }

    private func placeholderRow(_ label: String, brand: String) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack {
                Text(label)
                    .font(.caption2.weight(.medium))
                Spacer()
                if classic {
                    Text("No data")
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                }
            }
            bar(fillPercent: 0, color: Color(hex: brand), paceLeft: nil, paceIsDeficit: false)
            if !classic {
                Text("No data")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
            }
        }
    }

    private func bar(
        fillPercent: Double, color: Color, paceLeft: Double?, paceIsDeficit: Bool
    ) -> some View {
        GeometryReader { geo in
            ZStack(alignment: .leading) {
                Capsule()
                    .fill(.quaternary.opacity(0.6))
                    .frame(height: geo.size.height)
                Capsule()
                    .fill(color.opacity(0.85))
                    .frame(
                        width: geo.size.width * fillPercent / 100,
                        height: geo.size.height)
                if let paceLeft {
                    RoundedRectangle(cornerRadius: 0.75)
                        .fill(paceIsDeficit ? Color.orange : Color.secondary)
                        .frame(width: 1.5, height: geo.size.height + 4)
                        .offset(x: geo.size.width * paceLeft / 100 - 0.75)
                        .help("Expected \(Int((asUsed ? paceLeft : 100 - paceLeft).rounded()))% used by now")
                }
            }
        }
        .frame(height: 6)
    }

    /// The live tail reports raw client ids; quota snapshots use short ids.
    /// Layers this card's deliberate quota-attribution fold on top of the shared
    /// explicit aliases: after the registry's exact mappings, a generic `-cli`
    /// strip folds CLI variants onto their base client so e.g. `antigravity-cli`
    /// shares the `antigravity` quota snapshot. This generic fold is intentional
    /// HERE (quota grouping) and must NOT leak into the hidden-set deny-filters,
    /// which need `antigravity-cli` kept distinct — hence it lives in the card,
    /// not in `ClientRegistry.canonicalClient`.
    static func normalizeTraceClient(_ id: String) -> String {
        let canonical = ClientRegistry.canonicalClient(id)
        guard canonical == id else { return canonical } // an explicit alias applied
        return id.hasSuffix("-cli") ? String(id.dropLast(4)) : id
    }
}
