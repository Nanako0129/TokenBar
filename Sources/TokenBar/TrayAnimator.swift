import AppKit
import TokenBarCore

#if DEBUG
struct TrayAnimationCPUTestConfiguration {
    let style: String
    let animated: Bool
    let tokensPerMinute = 1_000_000.0

    static let current: TrayAnimationCPUTestConfiguration? = {
        let prefix = "--tray-animation-cpu-test="
        guard let argument = CommandLine.arguments.first(where: { $0.hasPrefix(prefix) })
        else { return nil }
        let value = String(argument.dropFirst(prefix.count))
        switch value {
        case "cat", "parrot":
            return TrayAnimationCPUTestConfiguration(style: value, animated: true)
        case "static":
            return TrayAnimationCPUTestConfiguration(style: "cat", animated: false)
        default:
            return nil
        }
    }()
}
#endif

/// RunCat-style menu-bar animation, port of src-tauri's animation.rs: the
/// cat (or parrot) spins faster as the live token rate climbs. Frame sets
/// come in dark/light pairs and follow the menu bar's effective appearance —
/// `anim-*` are white glyphs for a dark menu bar, `anim-*-light` black ones
/// for a light menu bar.
@MainActor
final class TrayAnimator {
    static let animateKey = "tokenbar.tray.animate"
    static let styleKey = "tokenbar.tray.animationStyle"

    static let quotaSourceKey = "tokenbar.quota.source"

    private weak var controller: StatusItemController?
    private let source: any UsageDataSource
#if DEBUG
    private let cpuTest = TrayAnimationCPUTestConfiguration.current
#endif
    /// Frame sets keyed by "<style>|<dark|light>".
    private let frames: [String: [NSImage]]
    private var loadTask: Task<Void, Never>?
    private var quotaTask: Task<Void, Never>?
    private var presentedAnimationKey: String?
    private var isStopped = true
    /// RunCat load signal in [0, 100]: tokens/min ÷ 10K, so 1M tok/min = 100.
    private var load: Double = 0
    /// Latest snapshot returned by this poller. A newer payload accepted by a
    /// dashboard or Settings poll wins through the shared publication state.
    private var polledQuota: AgentUsagePayload?
    var quota: AgentUsagePayload? { Self.publishedQuota(polledQuota) }

    static func publishedQuota(_ polledQuota: AgentUsagePayload?) -> AgentUsagePayload? {
        if let polledQuota, polledQuota.publicationGeneration == nil { return polledQuota }
        return AgentUsagePublicationCoordinator.latestPayload ?? polledQuota
    }
    /// Fired after every successful quota fetch (title refresh hook).
    var onQuotaUpdated: (() -> Void)?

    init(
        controller: StatusItemController,
        source: any UsageDataSource = UsageDataSources.current
    ) {
        self.controller = controller
        self.source = source
        self.cachedQuotaRemaining = source.allowsQuotaCachePersistence
            ? UserDefaults.standard.object(forKey: Self.lastRemainingKey) as? Double
            : nil
        var sets: [String: [NSImage]] = [:]
        for (style, dir) in [("cat", "anim-cat2"), ("parrot", "anim-parrot")] {
            sets["\(style)|dark"] = Self.loadFrames(directory: dir)
            sets["\(style)|light"] = Self.loadFrames(directory: "\(dir)-light")
        }
        frames = sets
    }

    /// The bar's frame box. Art is fitted into it, never stretched to it.
    nonisolated static let frameBox = NSSize(width: 18, height: 18)

    /// PNG frames sorted by name (frame-00 … frame-NN), sized for the bar.
    /// Internal so the settings window's menu-bar mock can render the same
    /// frame sets.
    static func loadFrames(directory: String) -> [NSImage] {
        let urls = Bundle.tokenBarResources.urls(
            forResourcesWithExtension: "png", subdirectory: directory) ?? []
        return urls
            .sorted { $0.lastPathComponent < $1.lastPathComponent }
            .compactMap { url in
                guard let image = NSImage(contentsOf: url) else { return nil }
                image.size = barSize(for: image)
                return image
            }
    }

    /// `frameBox`-bounded logical size preserving the art's own aspect ratio.
    ///
    /// This used to assign `frameBox` unconditionally, which is only correct
    /// for square art. `anim-parrot` is 48x36, so forcing 18x18 stretched it
    /// vertically — and only on the paths that render `NSImage.size`:
    /// `button.image` (what static tray mode and the Settings preview show).
    /// The animation itself was never affected, because
    /// `StatusItemAnimationSurface.rasterizedFrame` fits by the *pixel*
    /// dimensions of the representation and ignores the logical size, so the
    /// two paths disagreed about the same asset.
    ///
    /// Pixel data is deliberately untouched: the raster path reads
    /// `representations`, so re-drawing frames here would change what it
    /// sees. Only the logical size is corrected.
    nonisolated static func barSize(for image: NSImage) -> NSSize {
        let pixels = image.representations
            .max(by: { $0.pixelsWide * $0.pixelsHigh < $1.pixelsWide * $1.pixelsHigh })
            .map { NSSize(width: $0.pixelsWide, height: $0.pixelsHigh) } ?? image.size
        guard pixels.width > 0, pixels.height > 0 else { return frameBox }
        let fit = min(frameBox.width / pixels.width, frameBox.height / pixels.height)
        return NSSize(width: pixels.width * fit, height: pixels.height * fit)
    }

    private var defaultsObserver: NSObjectProtocol?
    /// Snapshot of the icon-affecting defaults the observer reacts to. The
    /// global didChangeNotification carries no key and fires for every write
    /// (popover height, active tab, year, quota cache…), so we compare this
    /// signature and act only when an icon setting actually changed —
    /// otherwise an unrelated write would needlessly re-render the gauge and
    /// tear down + restart the animation loop on every keystroke.
    private var iconSettingsSignature = ""

    static func currentIconSignature(defaults d: UserDefaults = .standard) -> String {
        let style = d.string(forKey: styleKey) ?? ""
        let animate = d.object(forKey: animateKey).map { String(describing: $0) } ?? ""
        let quotaSource = d.string(forKey: quotaSourceKey) ?? ""
        let lastRemaining = d.object(forKey: lastRemainingKey).map { String(describing: $0) } ?? ""
        let iconColor = d.string(forKey: IconColoring.storageKey) ?? ""
        // The Auto gauge value now depends on the exclusion set, so a hide
        // toggle must re-render (and re-resolve) the gauge — renderGaugeIcon
        // reads `quotaRemaining`, which resolves with the live exclusion.
        // Without these keys the icon kept the excluded client's % until the
        // 30s gauge loop / next quota poll. Value-gated: one re-render on an
        // actual change, unrelated writes stay free.
        let tabHidden = d.string(forKey: ClientRegistry.tabHiddenKey) ?? ""
        let limitsHidden = d.string(forKey: ClientRegistry.limitsHiddenKey) ?? ""
        return [style, animate, quotaSource, lastRemaining, iconColor, tabHidden, limitsHidden]
            .joined(separator: "|")
    }

    func start() {
        isStopped = false
#if DEBUG
        if let cpuTest {
            load = Self.animationLoad(tokensPerMinute: cpuTest.tokensPerMinute)
            tokensPerMinRate = cpuTest.tokensPerMinute
            refreshIcon()
            reportCPUTestReady(cpuTest)
            return
        }
#endif
        iconSettingsSignature = Self.currentIconSignature()
        controller?.setAppearanceChangeHandler { [weak self] in
            self?.refreshIcon()
        }
        defaultsObserver = NotificationCenter.default.addObserver(
            forName: UserDefaults.didChangeNotification, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self else { return }
                let next = Self.currentIconSignature()
                guard next != self.iconSettingsSignature else { return }
                self.iconSettingsSignature = next
                if let payload = self.quota {
                    self.reconcileQuotaRemaining(with: payload)
                }
                self.refreshIcon()
            }
        }
        refreshIcon()
        startLoadPolling()
        startQuotaPolling()
    }

    func stop() {
        isStopped = true
        loadTask?.cancel()
        quotaTask?.cancel()
        loadTask = nil
        quotaTask = nil
        if let defaultsObserver { NotificationCenter.default.removeObserver(defaultsObserver) }
        defaultsObserver = nil
        controller?.setAppearanceChangeHandler(nil)
        controller?.stopTrayAnimation()
        presentedAnimationKey = nil
    }

    private var currentStyle: String {
#if DEBUG
        if let cpuTest { return cpuTest.style }
#endif
        return UserDefaults.standard.string(forKey: Self.styleKey) ?? "cat"
    }

    /// Draws the current gauge style immediately (no-op for cat/parrot).
    private func renderGaugeIcon() {
        let style = currentStyle
        guard let gaugeStyle = QuotaIconStyle(rawValue: style) else { return }
        let coloring = IconColoring(
            rawValue: UserDefaults.standard.string(forKey: IconColoring.storageKey) ?? ""
        ) ?? .warningOnly
        presentedAnimationKey = nil
        controller?.setStaticIcon(
            TrayIcons.image(
                style: gaugeStyle, remaining: quotaRemaining,
                dark: controller?.isDarkAppearance ?? true,
                coloring: coloring),
            isTemplate: false)
    }

    /// Internal so the settings window's preview can use the same last-good
    /// reading before its own quota fetch lands.
    nonisolated static let lastRemainingKey = "tokenbar.quota.lastRemaining"

    /// Remaining percent reconciled from the most recent successful outer
    /// payload. Live mode seeds this from UserDefaults so an outer FFI failure
    /// before any new payload preserves the last-good reading; demo mode keeps
    /// only the process-local synthetic value.
    private var cachedQuotaRemaining: Double?

    /// The selected quota window's remaining percent. A missing outer payload
    /// may use the last-good scalar; a successful payload must resolve a fresh,
    /// finite value from its own windows.
    var quotaRemaining: Double? {
        let persistedSelection = UserDefaults.standard.string(forKey: Self.quotaSourceKey)
            ?? QuotaResolver.auto
        return QuotaSelectionPolicy.resolveRemainingPercent(
            payload: quota,
            persistedSelection: persistedSelection,
            excluding: ClientRegistry.quotaExcludedClients(),
            cachedRemaining: cachedQuotaRemaining)
    }

    /// Reconcile a successful payload with the scalar cache. A missing outer
    /// payload returns the cache without touching defaults; nil defaults keep
    /// demo mode process-local.
    nonisolated static func applyQuotaRemaining(
        payload: AgentUsagePayload?,
        persistedSelection: String,
        excluding: Set<String>,
        cachedRemaining: Double?,
        defaults: UserDefaults?
    ) -> Double? {
        guard payload != nil else { return cachedRemaining }
        let remaining = QuotaSelectionPolicy.resolveRemainingPercent(
            payload: payload,
            persistedSelection: persistedSelection,
            excluding: excluding,
            cachedRemaining: cachedRemaining)
        if let remaining {
            defaults?.set(remaining, forKey: Self.lastRemainingKey)
        } else {
            defaults?.removeObject(forKey: Self.lastRemainingKey)
        }
        return remaining
    }

    private func reconcileQuotaRemaining(with payload: AgentUsagePayload) {
        let defaults = UserDefaults.standard
        cachedQuotaRemaining = Self.applyQuotaRemaining(
            payload: payload,
            persistedSelection: defaults.string(forKey: Self.quotaSourceKey)
                ?? QuotaResolver.auto,
            excluding: ClientRegistry.quotaExcludedClients(),
            cachedRemaining: cachedQuotaRemaining,
            defaults: source.allowsQuotaCachePersistence ? defaults : nil)
    }

    /// Persist a proven legacy-label migration after the payload's scalar state
    /// has been reconciled. Demo mode remains entirely process-local.
    private func persistQuotaSelectionMigration(for payload: AgentUsagePayload) {
        guard source.allowsQuotaCachePersistence else { return }
        let defaults = UserDefaults.standard
        let persistedSelection = defaults.string(forKey: Self.quotaSourceKey)
            ?? QuotaResolver.auto
        if let migrated = QuotaSelectionPolicy.migrationToPersist(
            payload: payload, persistedSelection: persistedSelection)
        {
            defaults.set(migrated, forKey: Self.quotaSourceKey)
        }
    }

    static func applyQuotaPayload(
        _ candidate: AgentUsagePayload,
        store: (AgentUsagePayload) -> Void,
        reconcile: (AgentUsagePayload) -> Void,
        persistSelection: (AgentUsagePayload) -> Void,
        render: () -> Void,
        notify: () -> Void
    ) {
        let payload = AgentUsagePublicationCoordinator.resolve(candidate)
        store(payload)
        reconcile(payload)
        persistSelection(payload)
        render()
        notify()
    }

    private func currentFrames() -> [NSImage] {
        let dark = controller?.isDarkAppearance ?? true
        return frames["\(currentStyle)|\(dark ? "dark" : "light")"]
            ?? frames["cat|dark"] ?? []
    }

    private var animateEnabled: Bool {
#if DEBUG
        if let cpuTest { return cpuTest.animated }
#endif
        return UserDefaults.standard.object(forKey: Self.animateKey) == nil
            || UserDefaults.standard.bool(forKey: Self.animateKey)
    }

    nonisolated static func animationLoad(tokensPerMinute: Double) -> Double {
        min(max(0, tokensPerMinute) / 10_000.0, 100.0)
    }

    nonisolated static func animationIntervalMilliseconds(load: Double) -> Int {
        Int(500.0 / max(1.0, load / 5.0))
    }

    nonisolated static func animationLayerSpeed(load: Double) -> Double {
        500.0 / Double(animationIntervalMilliseconds(load: load))
    }

    nonisolated static func effectiveAnimationFPS(load: Double) -> Double {
        2.0 * animationLayerSpeed(load: load)
    }

    nonisolated static func baseAnimationDuration(frameCount: Int) -> Double {
        Double(frameCount) / 2.0
    }

    private var animationSpeed: Float {
        Float(Self.animationLayerSpeed(load: load))
    }

    private func refreshIcon() {
        guard !isStopped else { return }
        let style = currentStyle
        if QuotaIconStyle(rawValue: style) != nil {
            renderGaugeIcon()
            return
        }

        let dark = controller?.isDarkAppearance ?? true
        let frameKey = "\(style)|\(dark ? "dark" : "light")"
        let set = frames[frameKey] ?? frames["cat|dark"] ?? []
        guard let first = set.first else {
            controller?.setAnimatedFrames([], speed: animationSpeed)
            return
        }

        guard animateEnabled else {
            presentedAnimationKey = nil
            controller?.setStaticIcon(first, isTemplate: true)
            return
        }

        if presentedAnimationKey != frameKey {
            presentedAnimationKey = frameKey
            controller?.setAnimatedFrames(set, speed: animationSpeed)
        } else {
            controller?.setAnimationSpeed(animationSpeed)
        }
    }

    private func updateAnimationSpeedIfPresented() {
        guard QuotaIconStyle(rawValue: currentStyle) == nil, animateEnabled,
              presentedAnimationKey != nil
        else { return }
        controller?.setAnimationSpeed(animationSpeed)
    }

#if DEBUG
    private func reportCPUTestReady(_ test: TrayAnimationCPUTestConfiguration) {
        let frameCount = currentFrames().count
        let duration = Self.baseAnimationDuration(frameCount: frameCount)
        let speed = test.animated ? Self.animationLayerSpeed(load: load) : 0
        let fps = test.animated ? Self.effectiveAnimationFPS(load: load) : 0
        print(String(
            format: "TRAY_CPU_TEST_READY style=%@ animated=%@ frames=%d base_duration=%.3f speed=%.3f fps=%.3f",
            test.style, test.animated.description, frameCount, duration, speed, fps))
        fflush(stdout)
    }
#endif

    /// OAuth quota fetch is network-bound (~30s worst case across four
    /// providers), so refresh on a 5-minute cadence — quota windows move
    /// slowly and the popover has its own faster loop while open.
    private func startQuotaPolling() {
        quotaTask = Task { [weak self] in
            while !Task.isCancelled {
                guard let source = self?.source else { break }
                // Read before the fetch, like the popover's poll: the fetch is
                // network-bound and owns most of the cycle, so a registry change
                // lands during it far more often than during the sleep.
                let registryEpoch = ClaudeExtraRoots.RegistryChange.epoch
                let payload = try? await source.agentUsage()
                guard let self, !Task.isCancelled else { break }
                // A payload built for the previous account set must not be
                // applied. `AgentUsageThrottle.invalidate()` deliberately hands
                // an in-flight result to the waiter that asked for it rather
                // than failing it — the caller asked a question and gets an
                // answer — but this loop's answer becomes the Auto gauge, the
                // coordinator state and a persisted scalar, and the correction
                // is a whole network round away. Dropping it costs one cycle;
                // applying it shows the old account set for that round.
                //
                // The epoch, not the payload's contents: nothing in the payload
                // says which registry produced it.
                guard ClaudeExtraRoots.RegistryChange.epoch == registryEpoch else { continue }
                if let payload {
                    Self.applyQuotaPayload(
                        payload,
                        store: { self.polledQuota = $0 },
                        reconcile: { self.reconcileQuotaRemaining(with: $0) },
                        persistSelection: { self.persistQuotaSelectionMigration(for: $0) },
                        render: { self.renderGaugeIcon() },
                        notify: { self.onQuotaUpdated?() })
                }
                // Interruptible. This poll is the Auto gauge's only source, and
                // it is the one poll that runs with no window open — the launch
                // race and every change made while the popover is closed reach
                // the tray through here and nowhere else. A plain 300-second
                // sleep meant an added account could be missing from the gauge,
                // or a removed one still counted in it, for five minutes.
                await ClaudeExtraRoots.RegistryChange.sleep(upTo: 300, since: registryEpoch)
            }
        }
    }

    /// The raw tokens/min value from the last load poll — exposed so the
    /// tray title can display it without its own FFI call.
    private(set) var tokensPerMinRate: Double?

    // Monotonic rate-fetch generation. Every rate fetch reserves a token at
    // START via `nextRateGeneration()`; `applyRate` discards a result whose
    // token is older than the last applied one. This stops a slow 30s-poll
    // fetch (unfiltered rate) that was in flight during a hide toggle from
    // landing AFTER — and clobbering — the observer's fresh filtered refetch.
    private var rateGeneration = 0
    private var lastAppliedRateGen = 0

    /// Reserve the next rate-fetch generation token (call on the main actor,
    /// before launching the detached fetch).
    func nextRateGeneration() -> Int {
        rateGeneration += 1
        return rateGeneration
    }

    /// Apply a freshly-fetched live rate to the spin speed and cached rate, and
    /// re-render the title. Shared by the 30s poll and the immediate refresh
    /// AppDelegate kicks when the hidden-tabs set changes (so the filtered rate
    /// updates without waiting for the next poll tick). `generation` must be the
    /// token reserved at that fetch's start; a stale (superseded) result is
    /// dropped.
    func applyRate(_ rate: Double, generation: Int) {
        guard !isStopped, generation >= lastAppliedRateGen else { return }
        lastAppliedRateGen = generation
        load = Self.animationLoad(tokensPerMinute: rate)
        tokensPerMinRate = rate
        updateAnimationSpeedIfPresented()
        onQuotaUpdated?()
    }

    /// Poll the live rate to feed the spin speed. 30s cadence balances
    /// animation responsiveness against the rayon wakeup cost of each FFI
    /// call (the staticlib's mtime check wakes the entire rayon pool).
    private func startLoadPolling() {
        loadTask = Task { [weak self] in
            while !Task.isCancelled {
                guard let gen = self?.nextRateGeneration(), let source = self?.source else { break }
                let rate = try? await LiveRate.current(source: source)
                guard let self, !Task.isCancelled else { break }
                if let rate { self.applyRate(rate, generation: gen) }
                try? await Task.sleep(for: .seconds(30))
            }
        }
    }
}
