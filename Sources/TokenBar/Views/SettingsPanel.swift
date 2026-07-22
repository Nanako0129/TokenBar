import AppKit
import SwiftUI
import TokenBarCore

/// In-popover settings, port of SettingsPanel.tsx. Every control binds the
/// same UserDefaults keys the cards/tray read live. Autostart, tray animation
/// and the updater arrive with their subsystems in later phases.
struct SettingsPanel: View {
    /// For the quota-source picker (the windows currently known).
    var agentUsage: AgentUsagePayload?

    /// Present clients (used for the client tabs reorder/hide UI).
    var presentClients: [String] = []

    @AppStorage(TrayMode.storageKey) private var trayModeRaw = TrayMode.todayTokens.rawValue
    @AppStorage(TrayAnimator.animateKey) private var animateTray = true
    @AppStorage(TrayAnimator.styleKey) private var animationStyle = "cat"
    @AppStorage(IconColoring.storageKey) private var iconColoringRaw = IconColoring.warningOnly.rawValue
    @AppStorage(TrayAnimator.quotaSourceKey) private var quotaSource = QuotaResolver.auto
    @AppStorage("tokenbar.updates.beta") private var betaUpdates = false
    /// Mirrors SMAppService's actual state (read once per panel appearance).
    @State private var autostartEnabled = AutostartService.isAvailable && AutostartService.isEnabled
    @AppStorage("tokenbar.limits.enabled") private var limitsEnabled = true
    @AppStorage("tokenbar.views.hidden") private var hiddenViewsRaw = ""
    @AppStorage("tokenbar.limits.asUsed") private var limitsAsUsed = false
    @AppStorage("tokenbar.limits.paceMode") private var paceModeRaw = PaceMode.historical.rawValue
    @AppStorage("tokenbar.limits.layout") private var layoutRaw = LimitsLayout.full.rawValue
    @AppStorage("tokenbar.trace.detailed") private var detailedTrace = false
    @AppStorage("tokenbar.refresh.intervalMin") private var refreshIntervalMin = 30
    /// 0 = auto (≈60% of the screen). The popover's drag handle writes the
    /// same key, so the two stay in sync.
    @AppStorage(PopoverChrome.heightKey) private var popoverHeight = 0.0
    @AppStorage(WarpSourcePreference.externalPathKey) private var warpExternalPath = ""
    @State private var warpBearer = ""
    @State private var warpStatus: WarpStatus?
    @State private var warpBusy = false
    @State private var warpError: String?

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

        return VStack(alignment: .leading, spacing: 14) {
            section("Menubar title") {
                radioGroup(
                    selection: $trayModeRaw,
                    options: TrayMode.allCases.map { ($0.rawValue, $0.label) })
            }

            if AutostartService.isAvailable {
                section("Startup") {
                    toggleRow(
                        "Launch at login",
                        isOn: Binding(
                            get: { autostartEnabled },
                            set: { next in
                                if AutostartService.setEnabled(next) {
                                    autostartEnabled = next
                                }
                            }))
                }
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
                radioGroup(
                    selection: Binding(
                        get: {
                            QuotaResolver.canonicalSelection(
                                payload: agentUsage, selection: quotaSource)
                        },
                        set: { next in
                            quotaSource = QuotaResolver.canonicalSelection(
                                payload: agentUsage, selection: next)
                        }),
                    options: quotaSourceOptions)
                hint("Feeds the gauge icons and the \"Quota left\" title. Auto follows whichever window is closest to running out.")
            }

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
                        Text("Drag to set the order used by both the top tabs and the quota cards. The switch shows/hides a client's top tab (hiding also drops its quota card).")
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

            section("Warp local reporting") {
                VStack(alignment: .leading, spacing: 8) {
                    HStack {
                        Text(warpStatusLabel)
                            .font(.caption)
                            .foregroundStyle(warpStatus?.active == true ? .primary : .secondary)
                        Spacer()
                        if warpBusy {
                            ProgressView().controlSize(.small)
                        }
                    }

                    SecureField("Warp bearer token", text: $warpBearer)
                        .textFieldStyle(.roundedBorder)
                        .disabled(warpBusy || warpStatus?.supported == false)

                    HStack {
                        Button("Connect & Sync") { connectWarpBearer() }
                            .disabled(
                                warpBusy || warpStatus?.supported == false
                                    || warpBearer.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                        Button("Choose usage.json") { chooseWarpExternalFile() }
                            .disabled(warpBusy || warpStatus?.supported == false)
                        Spacer()
                        if warpStatus?.mode == "app" {
                            Button("Sync Now") { refreshWarp() }
                                .disabled(warpBusy)
                        }
                        if warpStatus?.active == true {
                            Button("Disconnect") { clearWarp() }
                                .disabled(warpBusy)
                        }
                    }
                    .controlSize(.small)

                    if let warpError {
                        Text(warpError)
                            .font(.caption2)
                            .foregroundStyle(.red)
                    }
                }
                .padding(.horizontal, 10)
                .padding(.vertical, 8)
                .glassCard(cornerRadius: 8)
                hint("Bearer mode keeps the token only in process memory. External mode reads only the exact selected usage.json and never reads or manages its credentials.")
            }

            section("Data refresh") {
                radioGroup(
                    selection: Binding(
                        get: { String(refreshIntervalMin) },
                        set: { refreshIntervalMin = Int($0) ?? 30 }),
                    options: Self.refreshIntervalOptions.map {
                        (String($0), $0 == 60 ? "Every hour" : "Every \($0) min")
                    })
                hint("How often the tray re-reads your logs. The dashboard refreshes when the popover opens; live tokens/min updates every few seconds regardless.")
            }

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
        .task { await loadWarpStatus() }
    }

    private var warpStatusLabel: String {
        guard let status = warpStatus else { return "Checking availability…" }
        guard status.supported else { return "Unsupported on this platform" }
        guard status.active else { return "Not configured" }
        let requests = status.requestsUsed.map(String.init) ?? "—"
        let limit = status.requestLimit.map(String.init) ?? "—"
        let stale = status.stale ? " · stale" : ""
        return status.mode == "app"
            ? "Bearer · \(requests) / \(limit) requests\(stale)"
            : "External usage.json · \(requests) requests"
    }

    @MainActor
    private func loadWarpStatus() async {
        guard !warpBusy else { return }
        warpBusy = true
        defer { warpBusy = false }
        do {
            let path = warpExternalPath
            let result = try await Task.detached(priority: .utility) {
                let status = try TBCore.warpStatus()
                if status.supported && !status.active && !path.isEmpty {
                    return try TBCore.warpRestoreExternalUsage(path: path)
                }
                return status
            }.value
            warpStatus = result
            warpError = result.errorCode.map(warpErrorMessage)
        } catch {
            warpError = warpErrorMessage(error)
            warpStatus = try? await Task.detached(priority: .utility) {
                try TBCore.warpStatus()
            }.value
        }
    }

    private func connectWarpBearer() {
        let bearer = warpBearer
        warpBusy = true
        warpError = nil
        Task { @MainActor in
            defer { warpBusy = false }
            do {
                warpStatus = try await Task.detached(priority: .utility) {
                    try TBCore.warpSetBearer(bearer)
                }.value
                warpBearer = ""
                warpExternalPath = ""
            } catch {
                warpBearer = ""
                warpError = warpErrorMessage(error)
                warpStatus = try? await Task.detached(priority: .utility) {
                    try TBCore.warpStatus()
                }.value
            }
        }
    }

    private func refreshWarp() {
        warpBusy = true
        warpError = nil
        Task { @MainActor in
            defer { warpBusy = false }
            do {
                warpStatus = try await Task.detached(priority: .utility) {
                    try TBCore.warpRefresh()
                }.value
            } catch {
                warpError = warpErrorMessage(error)
                warpStatus = try? await Task.detached(priority: .utility) {
                    try TBCore.warpStatus()
                }.value
            }
        }
    }

    private func chooseWarpExternalFile() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        panel.prompt = "Use usage.json"
        if !warpExternalPath.isEmpty {
            panel.directoryURL = URL(fileURLWithPath: warpExternalPath).deletingLastPathComponent()
        }
        guard panel.runModal() == .OK, let url = panel.url else { return }
        guard url.lastPathComponent == "usage.json" else {
            warpError = warpErrorMessage("warp_invalid_external_path")
            return
        }
        let path = url.path
        warpBearer = ""
        warpBusy = true
        warpError = nil
        Task { @MainActor in
            defer { warpBusy = false }
            do {
                warpStatus = try await Task.detached(priority: .utility) {
                    try TBCore.warpSetExternalUsage(path: path)
                }.value
                warpExternalPath = path
                warpBearer = ""
            } catch {
                warpError = warpErrorMessage(error)
            }
        }
    }

    private func clearWarp() {
        warpBusy = true
        warpError = nil
        Task { @MainActor in
            defer { warpBusy = false }
            do {
                warpStatus = try await Task.detached(priority: .utility) {
                    try TBCore.warpClear()
                }.value
                warpBearer = ""
                warpExternalPath = ""
            } catch {
                warpError = warpErrorMessage(error)
                warpStatus = try? await Task.detached(priority: .utility) {
                    try TBCore.warpStatus()
                }.value
                if warpStatus?.active != true {
                    warpBearer = ""
                    warpExternalPath = ""
                }
            }
        }
    }

    private func warpErrorMessage(_ error: Error) -> String {
        guard case let TBCoreError.bridge(code) = error else {
            return "Warp reporting failed."
        }
        return warpErrorMessage(code)
    }

    private func warpErrorMessage(_ code: String) -> String {
        switch code {
        case "warp_unsupported_platform": return "Warp reporting is unavailable on this platform."
        case "warp_invalid_bearer": return "Enter a valid Warp bearer token."
        case "warp_no_active_credential": return "No Warp bearer is active."
        case "warp_retry_cooldown": return "Warp sync is cooling down after a failed request."
        case "warp_unauthorized": return "Warp rejected this bearer. Enter a fresh token."
        case "warp_forbidden": return "Warp denied access; the last same-account data is retained."
        case "warp_rate_limited": return "Warp rate-limited sync; retry is delayed for at least five minutes."
        case "warp_timeout", "warp_transport", "warp_remote_server": return "Warp is temporarily unavailable; the last same-account data is retained."
        case "warp_graphql_error", "warp_decode_failed", "warp_response_too_large", "warp_invalid_usage": return "Warp returned an unusable usage response."
        case "warp_invalid_external_path": return "Choose the exact external file named usage.json."
        case "warp_scope_mismatch": return "The cached Warp account does not match the active source."
        case "warp_storage_failed": return "Warp's owner-only local cache could not be verified or saved."
        default: return "Warp reporting failed."
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

    /// Auto + every window the latest quota snapshot knows about.
    private var quotaSourceOptions: [(String, String)] {
        var options = [(QuotaResolver.auto, "Auto (tightest window)")]
        for agent in agentUsage?.agents ?? [] where agent.error == nil {
            let name = ClientRegistry.style(agent.clientId).displayName
            for window in agent.uniqueCardWindows {
                options.append(
                    (QuotaResolver.selection(clientId: agent.clientId, cardId: window.cardId),
                     "\(name) · \(window.label)"))
            }
        }
        return options
    }

    // MARK: - Building blocks

    private func section(_ label: String, @ViewBuilder content: () -> some View) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(label.uppercased())
                .font(.caption2.weight(.semibold))
                .foregroundStyle(.tertiary)
            content()
        }
    }

    private func row(_ label: String, @ViewBuilder trailing: () -> some View) -> some View {
        HStack {
            Text(label)
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
                Button {
                    selection.wrappedValue = option.value
                } label: {
                    HStack {
                        Text(option.label)
                            .font(.caption)
                        Spacer()
                        if selection.wrappedValue == option.value {
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
        }
        .glassCard(cornerRadius: 8)
    }

    private func hint(_ text: String) -> some View {
        Text(text)
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
}
