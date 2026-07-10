import AppKit
import TokenBarCore

/// App bootstrap: accessory activation policy (menu-bar only, no Dock icon),
/// the status-item controller, and the tray-title refresh loop.
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    private static let defaultRefreshSecs = 300
    private static let intervalKey = "tokenbar.refresh.intervalMin"

    private var statusController: StatusItemController?
    private var trayAnimator: TrayAnimator?
    private var titleRefreshTask: Task<Void, Never>?
    private var defaultsObserver: NSObjectProtocol?
    // Last good fetches — a failed refresh keeps showing these.
    private var lastGraph: UsagePayload?
    private var lastRate: Double?
    // Forced-refresh clock. An instance property (not a Task-local) so a loop
    // restart to pick up a new interval does NOT reset it to distantPast and
    // force an immediate uncached re-parse — that turned every settings write
    // into a full log re-read (the CPU regression Codex/the review flagged).
    private var lastFullRefresh = Date.distantPast
    // The interval the refresh loop is currently running with. The defaults
    // observer compares against it so the loop is restarted only when the
    // interval actually changes, not on every unrelated UserDefaults write.
    private var refreshIntervalMin = AppDelegate.readIntervalMin()
    // Snapshot of the tab-hidden raw string. The defaults observer compares
    // against it so a visibility change fires ONE off-main filtered-rate fetch
    // (updating the tokens/min title + animated-icon speed at once) and only
    // then — never on the flood of unrelated writes. Same value-gating
    // discipline as refreshIntervalMin, to avoid the CPU-regression storm.
    private var lastHiddenRaw = UserDefaults.standard.string(forKey: ClientRegistry.tabHiddenKey) ?? ""

    private static func readIntervalMin() -> Int {
        max(1, UserDefaults.standard.object(forKey: intervalKey).flatMap { $0 as? Int } ?? 30)
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory)
        BetaMigration.runIfNeeded() // before anything reads defaults
        ClientRegistry.migrateLegacyOrderKey() // fold the old limits order into the shared tab order, likewise before reads
        // refreshIntervalMin's initializer ran at AppDelegate construction in
        // main.swift, BEFORE the migration above — re-read it now so a migrated
        // (non-default) data-refresh interval is honored this session instead of
        // staying at the pre-migration default until the next defaults write.
        refreshIntervalMin = AppDelegate.readIntervalMin()
        _ = UpdaterService.shared // arm Sparkle when bundled

        let controller = StatusItemController()
        statusController = controller
        let animator = TrayAnimator(controller: controller)
        trayAnimator = animator
        controller.quotaPayloadProvider = { [weak animator] in animator?.quota }
        // A fresh quota or rate fetch re-renders the title right away.
        animator.onQuotaUpdated = { [weak self] in self?.applyTitle() }
        animator.start()
        startTitleRefresh()

        // Re-render the title the moment any setting changes (tray mode, quota
        // source) — cheap, recomputes from cached data. The refresh LOOP is
        // restarted only when the data-refresh interval actually changes:
        // didChangeNotification carries no key and fires for every write
        // (popover height slider, active tab, year, quota cache…), so an
        // unconditional restart turned each of those into a forced full
        // log re-read. Gate on the interval value to avoid that storm.
        defaultsObserver = NotificationCenter.default.addObserver(
            forName: UserDefaults.didChangeNotification, object: nil, queue: .main
        ) { _ in
            MainActor.assumeIsolated {
                guard let self = NSApp.delegate as? AppDelegate else { return }
                self.applyTitle()
                let next = AppDelegate.readIntervalMin()
                if next != self.refreshIntervalMin {
                    self.refreshIntervalMin = next
                    self.titleRefreshTask?.cancel()
                    self.startTitleRefresh()
                }
                // A visibility change alters the FILTERED live rate, which
                // applyTitle above can't recompute (it reuses the cached rate).
                // Value-gate on the hidden set and refetch once when it changes.
                let hiddenRaw = UserDefaults.standard.string(forKey: ClientRegistry.tabHiddenKey) ?? ""
                if hiddenRaw != self.lastHiddenRaw {
                    self.lastHiddenRaw = hiddenRaw
                    self.refreshFilteredRate()
                }
            }
        }

        // Debug hooks: `--open-popover` shows the popover shortly after
        // launch, `--settings` the settings window — both screenshot aids.
        if CommandLine.arguments.contains("--open-popover") {
            DispatchQueue.main.asyncAfter(deadline: .now() + 1) {
                controller.showPopover()
            }
        }
        if CommandLine.arguments.contains("--settings") {
            DispatchQueue.main.asyncAfter(deadline: .now() + 1) {
                SettingsWindowController.shared.show()
            }
        }
        if CommandLine.arguments.contains("--icon-gallery") {
            DispatchQueue.main.asyncAfter(deadline: .now() + 1) {
                IconGalleryWindowController.show()
            }
        }
    }

    func applicationWillTerminate(_ notification: Notification) {
        titleRefreshTask?.cancel()
        trayAnimator?.stop()
        if let defaultsObserver { NotificationCenter.default.removeObserver(defaultsObserver) }
        // Remove the status item / close the popover so ControlCenter tears
        // the menu-bar item down cleanly (avoids the ~40s RunningBoard
        // "waiting on exit context" stall seen on the 2026-06-16 quit).
        statusController?.tearDown()
    }

    /// Compose the tray title from the cached data and the current settings.
    /// The rate prefers the animator's 30s-fresh value over lastRate (which
    /// is only updated on the 5-minute title-refresh cycle).
    private func applyTitle() {
        let mode = TrayMode.current
        let quotaRemaining = trayAnimator?.quotaRemaining
        let rate = trayAnimator?.tokensPerMinRate ?? lastRate
        statusController?.updateTitle(
            mode.title(graph: lastGraph, tokensPerMin: rate, quotaRemaining: quotaRemaining),
            color: mode.titleColor(quotaRemaining: quotaRemaining))
    }

    /// The hidden-tabs set changed: fetch the filtered live rate once, off the
    /// main actor, and push it into the animator (spin speed + cached rate),
    /// which re-renders the title via onQuotaUpdated. No loop restart, no graph
    /// refetch — mirrors the 30s poll's delivery so the tokens/min mode and the
    /// animated-icon speed react to a visibility toggle immediately.
    private func refreshFilteredRate() {
        // Reserve a fresh generation up front so this refetch wins over any 30s
        // poll fetch still in flight (which carries an older generation).
        guard let generation = trayAnimator?.nextRateGeneration() else { return }
        Task { [weak self] in
            let rate = try? await Task.detached(priority: .utility) {
                try LiveRate.current()
            }.value
            guard let self, let rate else { return }
            self.lastRate = rate
            self.trayAnimator?.applyRate(rate, generation: generation)
        }
    }

    /// Background graph refresh: serves the graph-based title modes (today's
    /// tokens/cost, total tokens/cost). The rate and quota title modes are
    /// covered by TrayAnimator's load/quota polling via onQuotaUpdated.
    /// A full log re-read (tb_refresh_graph) is forced every "Data refresh"
    /// interval from settings; between forced refreshes the cheap mtime-aware
    /// cached graph() path re-reads on the loop cadence (capped at
    /// defaultRefreshSecs so graph-mode titles stay fresh even when the user's
    /// interval is long).
    private func startTitleRefresh() {
        titleRefreshTask = Task { [weak self] in
            while !Task.isCancelled {
                guard let self else { break }
                let mode = TrayMode.current
                let intervalMin = self.refreshIntervalMin
                let forceRefresh = Date().timeIntervalSince(self.lastFullRefresh) >= Double(intervalMin) * 60
                let graph = try? await Task.detached(priority: .utility) {
                    forceRefresh ? try TBCore.refreshGraph() : try TBCore.graph()
                }.value
                if forceRefresh && graph != nil { self.lastFullRefresh = Date() }
                // Failed refreshes keep the last good numbers — the title
                // must never blank/zero out on a transient error.
                if let graph { self.lastGraph = graph }
                if mode == .tokensPerMin {
                    let rate = try? await Task.detached(priority: .utility) {
                        try LiveRate.current()
                    }.value
                    if let rate { self.lastRate = rate }
                }
                guard !Task.isCancelled else { break }
                self.applyTitle()
                // Wake at least as often as the force interval (so a short
                // interval is honored) but never sleep longer than the 5-min
                // cached-refresh cap (so graph titles don't lag a long interval).
                let sleepSecs = max(60, min(intervalMin * 60, Self.defaultRefreshSecs))
                try? await Task.sleep(for: .seconds(Double(sleepSecs)))
            }
        }
    }
}
