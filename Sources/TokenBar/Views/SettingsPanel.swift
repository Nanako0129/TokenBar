import AppKit
import SwiftUI
import TokenBarCore

/// In-popover settings, port of SettingsPanel.tsx. Every control binds the
/// same UserDefaults keys the cards/tray read live. Autostart, tray animation
/// and the updater arrive with their subsystems in later phases.
struct SettingsPanel: View {
    enum Page: String, CaseIterable, Identifiable {
        case menuBar = "Menu bar"
        case dashboard = "Dashboard"
        case general = "General"
        case about = "About"

        var id: Self { self }

        var localizedTitle: String { rawValue.localized }

        var symbolName: String {
            switch self {
            case .menuBar: "menubar.rectangle"
            case .dashboard: "chart.bar.xaxis"
            case .general: "gearshape"
            case .about: "info.circle"
            }
        }
    }

    var page: Page = .menuBar

    /// For the quota-source picker (the windows currently known).
    var agentUsage: AgentUsagePayload?

    /// Present clients (used for the client tabs reorder/hide UI).
    var presentClients: [String] = []

    /// True while either initial request is still in flight. An empty client list
    /// is otherwise indistinguishable from "still loading", and the first seconds
    /// after opening Settings would claim there are no eligible clients. The
    /// caller derives this from request lifecycle, not payload presence — a failed
    /// fetch leaves the payload nil forever.
    var isLoading = false

    @AppStorage(TrayMode.storageKey) private var trayModeRaw = TrayMode.todayTokens.rawValue
    @AppStorage(TrayAnimator.animateKey) private var animateTray = true
    @AppStorage(TrayAnimator.styleKey) private var animationStyle = "cat"
    @AppStorage(IconColoring.storageKey) private var iconColoringRaw = IconColoring.warningOnly.rawValue
    @AppStorage(TrayAnimator.quotaSourceKey) private var quotaSource = QuotaResolver.auto
    @AppStorage(ClientTray.enabledKey) private var individualEnabledRaw = ""
    @AppStorage(ClientTray.selectionsKey) private var individualSelectionsRaw = "{}"
    @AppStorage("tokenbar.updates.beta") private var betaUpdates = false
    /// Loaded once per panel appearance without blocking SwiftUI body creation.
    @State private var autostartEnabled = false
    /// Set only after the service accepts a user mutation, so a slower initial
    /// read cannot overwrite committed state while a failed mutation still lets
    /// that authoritative read settle the switch.
    @State private var autostartMutationCommitted = false
    @AppStorage("tokenbar.limits.enabled") private var limitsEnabled = true
    @AppStorage("tokenbar.views.hidden") private var hiddenViewsRaw = ""
    @AppStorage("tokenbar.limits.asUsed") private var limitsAsUsed = false
    @AppStorage("tokenbar.limits.paceMode") private var paceModeRaw = PaceMode.historical.rawValue
    @AppStorage("tokenbar.limits.layout") private var layoutRaw = LimitsLayout.full.rawValue
    @AppStorage("tokenbar.trace.detailed") private var detailedTrace = false
    @AppStorage("tokenbar.refresh.intervalMin") private var refreshIntervalMin = 30
    @AppStorage(AppLanguage.storageKey) private var languageRaw = AppLanguage.system.rawValue
    @State private var showLanguageRestartPrompt = false
    /// 0 = auto (≈60% of the screen). The popover's drag handle writes the
    /// same key, so the two stay in sync.
    @AppStorage(PopoverChrome.heightKey) private var popoverHeight = 0.0

    // New for tabs improvement
    @AppStorage(ClientRegistry.tabOrderKey) private var tabsOrderRaw = ""
    @AppStorage(ClientRegistry.tabHiddenKey) private var tabsHiddenRaw = ""
    /// Per-client Agent-limits visibility, independent of tab visibility.
    @AppStorage(ClientRegistry.limitsHiddenKey) private var limitsHiddenRaw = ""

    // Drag state for client tabs reorder (scoped to this panel)
    @State private var tabsDragId: String?
    @State private var tabsOverId: String?
    @State private var tabsCardFrames: [String: CGRect] = [:]

    private static let tabsDragSpace = "client-tabs-order"

    private struct TabsCardFramesKey: PreferenceKey {
        static let defaultValue: [String: CGRect] = [:]
        static func reduce(value: inout [String: CGRect], nextValue: () -> [String: CGRect]) {
            value.merge(nextValue(), uniquingKeysWith: { $1 })
        }
    }

    static let refreshIntervalOptions = [1, 5, 15, 30, 60]

    /// First-wins dedup of two id lists, preserving order (a's entries first,
    /// then b's not already seen). Used to build the client-tabs universe from
    /// present clients ∪ quota-card clients.
    private static func orderedUnion(_ a: [String], _ b: [String]) -> [String] {
        var seen = Set<String>()
        return (a + b).filter { seen.insert($0).inserted }
    }

    // MARK: - Client tabs drag reorder helpers (adapted from AgentLimitsCard)

    private func dropEdge(for id: String, in orderList: [String]) -> VerticalEdge? {
        guard let dragId = tabsDragId,
              tabsOverId == id,
              dragId != id,
              let fromI = orderList.firstIndex(of: dragId),
              let toI = orderList.firstIndex(of: id)
        else { return nil }
        return fromI < toI ? .bottom : .top
    }

    private func dragGestureForTab(id: String, orderList: [String]) -> some Gesture {
        DragGesture(minimumDistance: 2, coordinateSpace: .named(Self.tabsDragSpace))
            .onChanged { value in
                tabsDragId = id
                let over = tabsCardFrames.first { $0.value.contains(value.location) }?.key
                tabsOverId = (over != nil && over != id) ? over : nil
            }
            .onEnded { _ in
                if let over = tabsOverId, over != id {
                    let next = ClientRegistry.reorder(orderList, from: id, to: over)
                    tabsOrderRaw = next.joined(separator: ",")
                }
                tabsDragId = nil
                tabsOverId = nil
            }
    }

    var body: some View {
        // Computed once and shared by the two sections below (Agent limits +
        // Client tabs), instead of re-deriving `knownClientIds` per section.
        // `orderRaw:` overloads keep both lists reactive to a drag/reorder.
        let knownIds = AgentLimitsCard.knownClientIds(
            agentUsage: agentUsage, present: presentClients)
        // Agent-limits management universe: only clients that can actually
        // render a quota card (placeholder rows or a live snapshot).
        let limitOrdered = ClientRegistry.orderedClients(knownIds, orderRaw: tabsOrderRaw)
        // Client-tabs universe: every client that can be a top tab (present)
        // OR a quota card (knownIds — e.g. quota-only Antigravity), so both
        // orderings are managed from one list. Mirrors displayClients' source.
        let presentSet = Set(presentClients)
        let tabsUniverse = ClientRegistry.orderedClients(
            Self.orderedUnion(presentClients, knownIds), orderRaw: tabsOrderRaw)

        VStack(alignment: .leading, spacing: 14) {
            switch page {
            case .menuBar:
                menuBarPage()
            case .dashboard:
                dashboardPage(
                    limitOrdered: limitOrdered,
                    presentSet: presentSet,
                    tabsUniverse: tabsUniverse)
            case .general:
                generalPage()
            case .about:
                aboutPage()
            }
        }
        .task {
            guard AutostartService.isAvailable else { return }
            let enabled = await AutostartService.readEnabled()
            // The query takes ~0.5-0.9s. If the user flipped the switch while it
            // was in flight, `setEnabled` has already changed the service and
            // this result is stale — applying it would leave the control showing
            // the opposite of the real state until Settings is reopened.
            guard !Task.isCancelled, !autostartMutationCommitted else { return }
            autostartEnabled = enabled
        }
        .alert("Restart TokenBar?", isPresented: $showLanguageRestartPrompt) {
            Button("Later", role: .cancel) {}
            Button("Restart Now") { AppRelauncher.relaunch() }
        } message: {
            Text("Restart TokenBar to apply the new language.")
        }
    }

    @ViewBuilder
    private func menuBarPage() -> some View {
        section("Menubar title") {
            radioGroup(
                selection: $trayModeRaw,
                options: TrayMode.allCases.map { ($0.rawValue, $0.label) })
        }

        section("Menubar icon") {
            radioGroup(
                selection: $animationStyle,
                options: [("cat", "Spinning cat"), ("parrot", "Party parrot")]
                    + QuotaIconStyle.allCases.map { ($0.rawValue, $0.label) })
            if isAnimatedStyle {
                toggleRow("Animate based on token usage", isOn: $animateTray)
                hint("Spins faster as the live token rate climbs (idle 2 fps, 1M tokens/min tops out at 40 fps).")
            } else {
                radioGroup(
                    selection: $iconColoringRaw,
                    options: IconColoring.allCases.map { ($0.rawValue, $0.label) })
                hint("Gauge icons drain as the selected quota window empties. \"Color on warning only\" stays monochrome until under 25% left (amber) and 10% (red), like the battery icon.")
            }
        }

        section("Quota source") {
            quotaSourcePicker()
            hint("Feeds the gauge icons and the \"Quota left\" title. Auto follows whichever window is closest to running out.")
        }

        individualItemsSection()
    }

    @ViewBuilder
    private func individualItemsSection() -> some View {
        let rows = ClientTray.settingsRows(
            presentClients: presentClients,
            payload: agentUsage,
            enabled: ClientTray.parseEnabledRaw(individualEnabledRaw),
            selections: ClientTray.parseSelectionsRaw(individualSelectionsRaw),
            hidden: ClientRegistry.parseIdSet(tabsHiddenRaw),
            orderRaw: tabsOrderRaw,
            officialClients: AgentIconView.availableOfficialClientIDs())

        section("Individual items") {
            hint("Keep the main TokenBar item. Optional client items show each client's selected quota window; Auto chooses the tightest healthy window for that client.")
            if rows.isEmpty, isLoading {
                HStack(spacing: 7) {
                    ProgressView()
                        .controlSize(.small)
                        .frame(width: 14, height: 14)
                    Text("Looking for eligible clients…".localized)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            } else if rows.isEmpty {
                Text("No eligible individual clients yet.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                VStack(spacing: 1) {
                    ForEach(rows) { row in
                        individualItemRow(row)
                    }
                }
                .glassCard(cornerRadius: 8)
            }
        }
    }

    private func individualItemRow(_ row: ClientTray.SettingsRow) -> some View {
        VStack(alignment: .leading, spacing: 7) {
            HStack(spacing: 7) {
                AgentIconView(clientId: row.clientId, size: 16)
                    .accessibilityHidden(true)
                Text(row.displayName)
                    .font(.caption)
                    .accessibilityHidden(true)
                Spacer()
                Text(row.valueText)
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)
                    .accessibilityLabel(row.accessibilityLabel)
                Toggle("", isOn: Binding(
                    get: { row.isEnabled },
                    set: { next in
                        if let raw = ClientTray.enabledRaw(
                            updating: individualEnabledRaw,
                            clientId: row.clientId,
                            enabled: next)
                        {
                            individualEnabledRaw = raw
                        }
                    }))
                    .toggleStyle(.switch)
                    .controlSize(.mini)
                    .labelsHidden()
                    .accessibilityLabel("Show %@ individual item".localized(row.displayName))
            }

            if row.isEnabled {
                HStack {
                    Text("Window")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                    Spacer()
                    Picker("", selection: Binding(
                        get: { row.selection },
                        set: { next in
                            if let raw = ClientTray.selectionsRaw(
                                updating: individualSelectionsRaw,
                                clientId: row.clientId,
                                selection: next)
                            {
                                individualSelectionsRaw = raw
                            }
                        })) {
                        ForEach(row.options) { option in
                            Text(option.label.localized)
                                .tag(option.tag)
                                .disabled(!option.isEnabled)
                        }
                    }
                    .labelsHidden()
                    .accessibilityLabel("Quota window for %@".localized(row.displayName))
                    .pickerStyle(.menu)
                    .frame(maxWidth: 210)
                }
            }

            if let statusHint = row.statusHint {
                Text(statusHint.localized)
                    .font(.caption2)
                    .foregroundStyle(row.status == .errorExplicit ? .secondary : .tertiary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 8)
    }

    @ViewBuilder
    private func dashboardPage(
        limitOrdered: [String],
        presentSet: Set<String>,
        tabsUniverse: [String]
    ) -> some View {
        section("Agent limits") {
            toggleRow("Show Agent limits card", isOn: $limitsEnabled)
            hint("Off hides the Agent-limits quota card everywhere — the Overview summary, every client's own tab, and this preview. Cost/token data is unaffected.")

            if limitsEnabled {
                toggleRow("Show as used", isOn: $limitsAsUsed)
                hint("On, bars count up as quota is used; off, they count down to what's left. The color always warns as quota runs low.")
                radioGroup(
                    selection: $layoutRaw,
                    options: LimitsLayout.allCases.map { ($0.rawValue, "Layout: \($0.rawValue.capitalized)") })
                hint("Full is the wide card with the pace line; Classic is the original compact layout without pace.")
                if LimitsLayout(rawValue: layoutRaw) != .classic {
                    radioGroup(
                        selection: $paceModeRaw,
                        options: PaceMode.allCases.map { ($0.rawValue, "Pace: \($0.rawValue.capitalized)") })
                    hint("The deficit/reserve marker. Historical learns each quota window's usage pattern; during learning, the Linear estimate is labeled; Linear uses the exact reset duration; Off hides the marker.")
                }

                if !limitOrdered.isEmpty {
                    let limitsHiddenSet = ClientRegistry.parseIdSet(limitsHiddenRaw)
                    // A tab hidden below always hides its quota card too — the
                    // toggle here reflects that (off + disabled) rather than
                    // offering a state the card can never actually reach.
                    let tabHiddenSet = ClientRegistry.parseIdSet(tabsHiddenRaw)
                    Divider()
                    VStack(spacing: 1) {
                        ForEach(limitOrdered, id: \.self) { id in
                            let tabHidden = tabHiddenSet.contains(id)
                            HStack {
                                HStack(spacing: 6) {
                                    AgentIconView(clientId: id, size: 14)
                                    Text(ClientRegistry.shortName(id))
                                        .font(.caption)
                                }
                                Spacer()
                                Toggle("", isOn: Binding(
                                    get: { !tabHidden && !limitsHiddenSet.contains(id) },
                                    set: { show in
                                        var hidden = limitsHiddenSet
                                        if show {
                                            hidden.remove(id)
                                        } else {
                                            hidden.insert(id)
                                        }
                                        limitsHiddenRaw = hidden.sorted().joined(separator: ",")
                                    }
                                ))
                                .disabled(tabHidden)
                                .toggleStyle(.switch)
                                .controlSize(.mini)
                                .labelsHidden()
                            }
                            .padding(.horizontal, 10)
                            .padding(.vertical, 7)
                            .opacity(tabHidden ? 0.5 : 1)
                        }
                    }
                    .glassCard(cornerRadius: 8)
                    hint("Hides only that client's quota card here and on its own tab — the tab and its cost/token data stay visible. Useful for accounts with no OAuth quota (e.g. Claude Console). Grayed out when the tab itself is hidden below, since a hidden tab always hides its quota card too.")
                }
            }
        }

        section("View tabs") {
            let hiddenViews = ClientRegistry.parseIdSet(hiddenViewsRaw)
            VStack(spacing: 1) {
                ForEach(AppView.toggleable, id: \.self) { view in
                    HStack {
                        Text(view.label)
                            .font(.caption)
                        Spacer()
                        Toggle("", isOn: Binding(
                            get: { !hiddenViews.contains(view.rawValue) },
                            set: { show in
                                var hidden = hiddenViews
                                if show { hidden.remove(view.rawValue) } else { hidden.insert(view.rawValue) }
                                hiddenViewsRaw = hidden.sorted().joined(separator: ",")
                            }
                        ))
                        .toggleStyle(.switch)
                        .controlSize(.mini)
                        .labelsHidden()
                    }
                    .padding(.horizontal, 10)
                    .padding(.vertical, 7)
                }
            }
            .glassCard(cornerRadius: 8)
            hint("Off removes a tab from the popover's tab row. Cost/token data is unaffected.")
        }

        section("Client tabs (top bar)") {
            let hiddenSet = ClientRegistry.parseIdSet(tabsHiddenRaw)

            if tabsUniverse.isEmpty {
                Text("No clients with usage data yet.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                VStack(alignment: .leading, spacing: 6) {
                    Text("Drag to set the order used by both the top tabs and the quota cards — or drag a tab directly in the top bar. The switch shows/hides a client's top tab (hiding also drops its quota card).")
                        .font(.caption2)
                        .foregroundStyle(.secondary)

                    VStack(spacing: 1) {
                        ForEach(tabsUniverse, id: \.self) { id in
                            let isVisible = !hiddenSet.contains(id)
                            // Only present clients can be top tabs, so only
                            // they get the show/hide switch. Quota-only ids
                            // (e.g. Antigravity — OAuth quota, no local
                            // sessions) appear solely to order their quota
                            // card, so they show a caption instead.
                            let canTab = presentSet.contains(id)
                            HStack(spacing: 8) {
                                // Drag handle - always shown for every provider
                                Text("⠿")
                                    .font(.caption)
                                    .foregroundStyle(tabsDragId == id ? .primary : .tertiary)
                                    .help("Drag to reorder")
                                    .gesture(dragGestureForTab(id: id, orderList: tabsUniverse))

                                AgentIconView(clientId: id, size: 14)
                                Text(ClientRegistry.shortName(id))
                                    .font(.caption)

                                if !canTab {
                                    Text("(quota card only)")
                                        .font(.caption2)
                                        .foregroundStyle(.tertiary)
                                }

                                Spacer()

                                if canTab {
                                    Toggle("", isOn: Binding(
                                        get: { isVisible },
                                        set: { show in
                                            var hidden = hiddenSet
                                            if show {
                                                hidden.remove(id)
                                            } else {
                                                hidden.insert(id)
                                            }
                                            tabsHiddenRaw = hidden.sorted().joined(separator: ",")
                                        }
                                    ))
                                    .toggleStyle(.switch)
                                    .controlSize(.mini)
                                    .labelsHidden()
                                }
                            }
                            .padding(.horizontal, 10)
                            .padding(.vertical, 7)
                            .opacity(tabsDragId == id ? 0.5 : 1)
                            .overlay(alignment: dropEdge(for: id, in: tabsUniverse) == .top ? .top : .bottom) {
                                if let edge = dropEdge(for: id, in: tabsUniverse) {
                                    Rectangle()
                                        .fill(Color.accentColor)
                                        .frame(height: 2)
                                        .offset(y: edge == .top ? -3 : 3)
                                }
                            }
                            .background(
                                GeometryReader { geo in
                                    Color.clear.preference(
                                        key: TabsCardFramesKey.self,
                                        value: [id: geo.frame(in: .named(Self.tabsDragSpace))])
                                })
                        }
                    }
                    .coordinateSpace(name: Self.tabsDragSpace)
                    .onPreferenceChange(TabsCardFramesKey.self) { tabsCardFrames = $0 }
                    .glassCard(cornerRadius: 8)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            hint("Present clients have a switch to show/hide their top tab — hiding also removes that client's quota card. Quota-only clients (OAuth quota, no local sessions, e.g. Antigravity) have no tab, so they appear here only to order their quota card. Drag order applies to both top tabs and quota cards.")
        }

        section("Live trace") {
            toggleRow("Split by agent / model", isOn: $detailedTrace)
            hint("Affects the live-session card only: on, each agent & model gets its own row; off, rows collapse to one per app.")
        }

        section("Popover size") {
            VStack(alignment: .leading, spacing: 8) {
                HStack {
                    Text("Height")
                        .font(.caption)
                    Spacer()
                    Text("\(Int(popoverHeightBinding.wrappedValue.rounded())) pt")
                        .font(.caption.monospacedDigit())
                        .foregroundStyle(.secondary)
                    if popoverHeight > 0 {
                        Button("Auto") { popoverHeight = 0 }
                            .controlSize(.mini)
                            .buttonStyle(.plain)
                            .font(.caption2)
                            .foregroundStyle(.tint)
                            .help("Fit the height to the screen automatically")
                    }
                }
                Slider(
                    value: popoverHeightBinding,
                    in: Double(PopoverChrome.minHeight)...popoverHeightMax,
                    step: 10)
                    .controlSize(.small)
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 8)
            .glassCard(cornerRadius: 8)
            hint("Or drag the handle at the bottom edge of the popover. Width is fixed; \"Auto\" fits about 60% of your screen height.")
        }
    }

    @ViewBuilder
    private func generalPage() -> some View {
        if AutostartService.isAvailable {
            section("Startup") {
                toggleRow(
                    "Launch at login",
                    isOn: Binding(
                        get: { autostartEnabled },
                        set: { next in
                            if AutostartService.setEnabled(next) {
                                autostartMutationCommitted = true
                                autostartEnabled = next
                            }
                        }))
            }
        }

        section("Data refresh") {
            radioGroup(
                selection: Binding(
                    get: { String(refreshIntervalMin) },
                    set: { refreshIntervalMin = Int($0) ?? 30 }),
                options: Self.refreshIntervalOptions.map {
                    (String($0), $0 == 60 ? "Every hour" : "Every %lld min".localized($0))
                })
            hint("How often the tray re-reads your logs. The dashboard refreshes when the popover opens; live tokens/min updates every few seconds regardless.")
        }

        section("Language") {
            radioGroup(
                selection: Binding(
                    get: { languageRaw },
                    set: { next in
                        guard AppLanguage.requiresRelaunch(
                            from: languageRaw, to: next)
                        else { return }
                        languageRaw = next
                        AppLanguage(rawValue: next)?.apply()
                        showLanguageRestartPrompt = true
                    }),
                options: AppLanguage.allCases.map { ($0.rawValue, $0.label) })
            hint("Takes effect the next time TokenBar starts.")
        }
    }

    @ViewBuilder
    private func aboutPage() -> some View {
        section("About") {
            row("Version") {
                Text(AppInfo.version)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            if UpdaterService.isAvailable {
                row("Check for updates") {
                    Button("Check Now") { UpdaterService.shared.checkForUpdates() }
                        .controlSize(.small)
                }
                row("Receive beta updates") {
                    Toggle("", isOn: $betaUpdates)
                        .toggleStyle(.switch)
                        .controlSize(.mini)
                        .labelsHidden()
                }
            }
            hint("TokenBar began as a fork of tokcat by handlecusion. Parsing & pricing come from tokscale by Junho Yeo; the menu-bar patterns reference CodexBar by Peter Steinberger; the running cat traces back to RunCat by Takuto Nakamura. MIT licensed.")
        }
    }

    private var isAnimatedStyle: Bool {
        animationStyle == "cat" || animationStyle == "parrot"
    }

    /// Shows the resolved auto height while 0 (auto), the chosen value once set.
    private var popoverHeightBinding: Binding<Double> {
        Binding(
            get: {
                popoverHeight > 0
                    ? popoverHeight
                    : Double(PopoverChrome.autoHeight(
                        visibleHeight: NSScreen.main?.visibleFrame.height ?? 900))
            },
            set: { popoverHeight = $0 })
    }

    /// Slider ceiling: the screen the settings window is on (the controller
    /// re-clamps to the popover's actual screen on open anyway).
    private var popoverHeightMax: Double {
        Double(max(700, (NSScreen.main?.visibleFrame.height ?? 1000) - 24))
    }

    @ViewBuilder
    private func quotaSourcePicker() -> some View {
        let canonical = QuotaResolver.canonicalSelection(
            payload: agentUsage, selection: quotaSource)
        let selectedClientId = quotaClientId(from: canonical)
        let agents = (agentUsage?.agents ?? []).filter {
            $0.error == nil && !$0.uniqueCardWindows.isEmpty
        }
        let availableClientIds = agents.map(\.clientId)
        let clientIds = selectedClientId.map {
            availableClientIds.contains($0) ? availableClientIds : availableClientIds + [$0]
        } ?? availableClientIds
        let selectedAgent = selectedClientId.flatMap { selectedId in
            agents.first { $0.clientId == selectedId }
        }

        row("Agent") {
            Picker("", selection: Binding(
                get: { selectedClientId ?? QuotaResolver.auto },
                set: { next in
                    if next == QuotaResolver.auto {
                        quotaSource = QuotaResolver.auto
                    } else if next != selectedClientId,
                              let agent = agents.first(where: { $0.clientId == next }),
                              let window = agent.uniqueCardWindows.first
                    {
                        quotaSource = QuotaResolver.selection(
                            clientId: agent.clientId, cardId: window.cardId)
                    }
                }))
            {
                Text("Auto (tightest window)".localized)
                    .tag(QuotaResolver.auto)
                ForEach(clientIds, id: \.self) { clientId in
                    Text(ClientRegistry.style(clientId).displayName)
                        .tag(clientId)
                }
            }
            .labelsHidden()
            .pickerStyle(.menu)
            .frame(maxWidth: 190)
        }

        if let selectedClientId {
            row("Window") {
                if let selectedAgent {
                    let availableSelections = Set(selectedAgent.uniqueCardWindows.map {
                        QuotaResolver.selection(
                            clientId: selectedClientId, cardId: $0.cardId)
                    })
                    Picker("", selection: Binding(
                        get: { canonical },
                        set: { next in
                            quotaSource = QuotaResolver.canonicalSelection(
                                payload: agentUsage, selection: next)
                        }))
                    {
                        ForEach(selectedAgent.uniqueCardWindows, id: \.cardId) { window in
                            Text(window.label.localized)
                                .tag(QuotaResolver.selection(
                                    clientId: selectedClientId, cardId: window.cardId))
                        }
                        if !availableSelections.contains(canonical) {
                            Text("Unavailable selection".localized)
                                .tag(canonical)
                                .disabled(true)
                        }
                    }
                    .labelsHidden()
                    .pickerStyle(.menu)
                    .frame(maxWidth: 190)
                } else {
                    Text("—")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
        }
    }

    private func quotaClientId(from selection: String) -> String? {
        guard selection != QuotaResolver.auto else { return nil }
        return selection.split(
            separator: "|", maxSplits: 1, omittingEmptySubsequences: false
        ).first.map(String.init)
    }

    // MARK: - Building blocks

    private func section(_ label: String, @ViewBuilder content: () -> some View) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(label.localized.uppercased())
                .font(.caption2.weight(.semibold))
                .foregroundStyle(.tertiary)
            content()
        }
    }

    private func row(_ label: String, @ViewBuilder trailing: () -> some View) -> some View {
        HStack {
            Text(label.localized)
                .font(.caption)
            Spacer()
            trailing()
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 7)
        .glassCard(cornerRadius: 8)
    }

    private func toggleRow(_ label: String, isOn: Binding<Bool>) -> some View {
        row(label) {
            Toggle("", isOn: isOn)
                .toggleStyle(.switch)
                .controlSize(.mini)
                .labelsHidden()
        }
    }

    private func radioGroup(
        selection: Binding<String>, options: [(value: String, label: String)]
    ) -> some View {
        VStack(spacing: 1) {
            ForEach(options, id: \.value) { option in
                radioOption(
                    selection: selection,
                    value: option.value,
                    label: option.label)
            }
        }
        .glassCard(cornerRadius: 8)
    }

    private func radioOption(
        selection: Binding<String>, value: String, label: String
    ) -> some View {
        Button {
            selection.wrappedValue = value
        } label: {
            HStack {
                Text(label.localized)
                    .font(.caption)
                Spacer()
                if selection.wrappedValue == value {
                    Image(systemName: "checkmark")
                        .font(.caption2.weight(.bold))
                        .foregroundStyle(Color.accentColor)
                }
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 7)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    private func hint(_ text: String) -> some View {
        Text(text.localized)
            .font(.caption2)
            .foregroundStyle(.tertiary)
            .fixedSize(horizontal: false, vertical: true)
    }
}

/// Build/version info. The bare SwiftPM executable has no bundle, so the
/// version is a constant until Phase 9 wraps it in a .app with an Info.plist.
enum AppInfo {
    static var version: String {
        Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "dev"
    }

    /// Read from the bundle rather than hard-coded, so a rename carries into the
    /// UI with the Info.plist instead of leaving a stale name behind.
    static var name: String {
        Bundle.main.infoDictionary?["CFBundleName"] as? String ?? "TokenBar"
    }
}
