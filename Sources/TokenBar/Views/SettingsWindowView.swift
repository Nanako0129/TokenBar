import AppKit
import SwiftUI
import TokenBarCore

/// Standalone settings window: the settings form on the left, a live preview
/// column on the right. Every control writes UserDefaults and every preview
/// piece reads the same keys (plus the real menu bar reacts anyway), so
/// changes reflect instantly without touching the popover's transient
/// behavior.
struct SettingsWindowView: View {
    // Default cachesSnapshot: false — this window's model must never write the
    // popover's restore snapshot (its `year` is frozen at init; clobbering the
    // cache with it would re-introduce the reopen flash).
    @State private var model = DashboardModel()
    @State private var tokensPerMin: Double?

    /// Master switch: off hides the preview's Agent-limits card too.
    @AppStorage("tokenbar.limits.enabled") private var limitsEnabled = true
    /// Observed so the preview's tab list re-derives the instant the user
    /// toggles visibility or reorders in the left panel, instead of lagging a
    /// poller tick behind.
    @AppStorage(ClientRegistry.tabHiddenKey) private var tabsHiddenRaw = ""
    @AppStorage(ClientRegistry.tabOrderKey) private var tabsOrderRaw = ""

    var body: some View {
        HStack(spacing: 0) {
            ScrollView {
                SettingsPanel(agentUsage: model.agentUsage, presentClients: model.stats?.presentClients ?? [])
                    .padding(14)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(OverlayScrollerEnforcer())
            }
            .scrollIndicators(.never)
            .frame(width: 354)
            Divider()
            ScrollView {
                previewColumn
                    .padding(14)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(OverlayScrollerEnforcer())
            }
            .scrollIndicators(.never)
            .frame(width: 330)
        }
        .frame(width: 685, height: 580)
        .background(PopoverBackdrop().ignoresSafeArea())
        .task { await model.load() }
        .task { await model.pollAgentUsage() }
        .task { await model.pollTrace() }
        .task { await model.pollGraph() }
        .task { await pollTokensPerMin() }
    }

    // MARK: - Preview column

    private var previewColumn: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Live preview — settings apply immediately.")
                .font(.caption2)
                .foregroundStyle(.tertiary)

            section("Menu bar") {
                VStack(spacing: 6) {
                    MenuBarMock(
                        dark: true, graph: model.payload,
                        tokensPerMin: tokensPerMin, agentUsage: model.agentUsage)
                    MenuBarMock(
                        dark: false, graph: model.payload,
                        tokensPerMin: tokensPerMin, agentUsage: model.agentUsage)
                }
            }

            if limitsEnabled {
                section("Agent limits card") {
                    let displayClients = ClientRegistry.displayClients(
                        present: model.stats?.presentClients ?? [],
                        hiddenRaw: tabsHiddenRaw, orderRaw: tabsOrderRaw)
                    AgentLimitsCard(
                        clients: displayClients,
                        trace: model.trace, agentUsage: model.agentUsage,
                        reorderable: true)
                }
            }

            section("Live session card") {
                UsageTraceCard(
                    buckets: model.trace, windowSecs: 600,
                    clientIds: ClientRegistry.displayClients(present: model.stats?.presentClients ?? []))
            }
        }
    }

    private func section(_ label: String, @ViewBuilder content: () -> some View) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(label.uppercased())
                .font(.caption2.weight(.semibold))
                .foregroundStyle(.tertiary)
            content()
        }
    }

    /// Same cadence as the popover: the staticlib tail re-parses at most
    /// every 10s. Feeds the rate tray mode and the preview's spin speed.
    private func pollTokensPerMin() async {
        while !Task.isCancelled {
            let rate = try? await Task.detached(priority: .utility) {
                try LiveRate.current()
            }.value
            if Task.isCancelled { break }
            tokensPerMin = rate
            try? await Task.sleep(for: .seconds(10))
        }
    }
}

/// One mock menu-bar strip (dark or light) with the TokenBar status item
/// rendered from the same inputs the real one uses: TrayIcons gauges or the
/// cat/parrot frame sets, plus TrayMode's title over live data.
private struct MenuBarMock: View {
    let dark: Bool
    let graph: UsagePayload?
    let tokensPerMin: Double?
    let agentUsage: AgentUsagePayload?

    @AppStorage(TrayMode.storageKey) private var trayModeRaw = TrayMode.todayTokens.rawValue
    @AppStorage(TrayAnimator.styleKey) private var animationStyle = "cat"
    @AppStorage(TrayAnimator.animateKey) private var animateTray = true
    @AppStorage(IconColoring.storageKey) private var iconColoringRaw = IconColoring.warningOnly.rawValue
    @AppStorage(TrayAnimator.quotaSourceKey) private var quotaSource = QuotaResolver.auto

    var body: some View {
        let mode = TrayMode(rawValue: trayModeRaw) ?? .todayTokens
        let remaining = quotaRemaining
        let title = mode.title(
            graph: graph, tokensPerMin: tokensPerMin, quotaRemaining: remaining)
        let ink: Color = dark ? .white : .black

        HStack(spacing: 10) {
            Text(dark ? "Dark" : "Light")
                .font(.caption2)
                .foregroundStyle(ink.opacity(0.4))
            Spacer()
            // The TokenBar status item, hover-highlighted to stand out.
            HStack(spacing: title.isEmpty ? 0 : 4) {
                icon(remaining: remaining)
                if !title.isEmpty {
                    Text(title)
                        .font(.system(size: 12).monospacedDigit())
                        .foregroundStyle(
                            mode.titleColor(quotaRemaining: remaining)
                                .map(Color.init(nsColor:)) ?? ink)
                }
            }
            .padding(.horizontal, 7)
            .padding(.vertical, 3)
            .background(
                (dark ? Color.white : Color.black).opacity(0.16),
                in: RoundedRectangle(cornerRadius: 5))
            Image(systemName: "wifi")
                .font(.system(size: 11))
                .foregroundStyle(ink.opacity(0.5))
            Text(Self.clock)
                .font(.system(size: 12))
                .foregroundStyle(ink.opacity(0.5))
        }
        .padding(.horizontal, 10)
        .frame(height: 27)
        .background(
            dark ? Color(white: 0.13) : Color(white: 0.93),
            in: RoundedRectangle(cornerRadius: 7))
        .overlay(
            RoundedRectangle(cornerRadius: 7)
                .strokeBorder(Color.primary.opacity(0.12), lineWidth: 1))
    }

    /// Mirrors TrayAnimator.quotaRemaining minus the write-back: live resolve
    /// first, then the persisted last-good reading.
    private var quotaRemaining: Double? {
        QuotaResolver.resolve(payload: agentUsage, selection: quotaSource)?
            .window.remainingPercent
            ?? UserDefaults.standard.object(forKey: TrayAnimator.lastRemainingKey) as? Double
    }

    @ViewBuilder
    private func icon(remaining: Double?) -> some View {
        if let gauge = QuotaIconStyle(rawValue: animationStyle) {
            let coloring = IconColoring(rawValue: iconColoringRaw) ?? .warningOnly
            Image(nsImage: TrayIcons.image(
                style: gauge, remaining: remaining, dark: dark, coloring: coloring))
        } else {
            let frames = PreviewFrames.frames(style: animationStyle, dark: dark)
            if frames.isEmpty {
                Image(systemName: "chart.bar.fill")
                    .font(.system(size: 12))
            } else if animateTray {
                let interval = frameInterval
                TimelineView(.periodic(from: .now, by: interval)) { timeline in
                    let index = Int(
                        timeline.date.timeIntervalSinceReferenceDate / interval)
                        % frames.count
                    Image(nsImage: frames[index])
                }
            } else {
                Image(nsImage: frames[0])
            }
        }
    }

    /// animation.rs pacing, same as TrayAnimator: idle 2 fps, 1M tok/min
    /// tops out at 40 fps.
    private var frameInterval: TimeInterval {
        let load = min((tokensPerMin ?? 0) / 10_000.0, 100.0)
        return 0.5 / max(1.0, load / 5.0)
    }

    private static let clock = Date.now.formatted(date: .omitted, time: .shortened)
}

/// Cat/parrot frame sets for the mock strips, loaded once per
/// (style, appearance) from the same bundle directories TrayAnimator uses.
@MainActor
private enum PreviewFrames {
    private static var cache: [String: [NSImage]] = [:]

    static func frames(style: String, dark: Bool) -> [NSImage] {
        let directory =
            (style == "parrot" ? "anim-parrot" : "anim-cat2") + (dark ? "" : "-light")
        if let hit = cache[directory] { return hit }
        let loaded = TrayAnimator.loadFrames(directory: directory)
        cache[directory] = loaded
        return loaded
    }
}
