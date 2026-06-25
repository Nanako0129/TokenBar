import Foundation
import Observation
import TokenBarCore

/// The set of analysis lenses, echoing tokscale's TUI tabs. The client tab
/// (Overview/Claude/Codex…, later phase) filters *which* data; this picks
/// *how* it is broken down. The two compose.
enum AppView: String, CaseIterable {
    case overview, models, daily, hourly, stats, agents

    var label: String { rawValue.prefix(1).uppercased() + rawValue.dropFirst() }
}

/// Snapshot of the model's essential state, captured on each successful
/// load so a fresh DashboardModel can start in `.ready` state instead of
/// flashing "Loading usage…" every time the popover reopens.
///
/// Only `hourly`/`agents` are excluded: they are the lazy lenses that
/// `ensureData(for:)` refetches *solely when nil*, so restoring stale non-nil
/// values would strand them on a previous year's slice (Codex P2-1/P2-3)
/// until the 60s pollGraph tick. `agentUsage`/`trace` are NOT lazy lenses —
/// their pollers (`pollAgentUsage`/`pollTrace`) fetch-first and overwrite
/// unconditionally — so caching them is staleness-free and keeps the Overview
/// tab's live/quota cards populated on reopen instead of flashing placeholders.
private struct DashboardSnapshot {
    let payload: UsagePayload
    let stats: UsageStats
    let modelReport: ModelReport?
    let colors: ModelColorMap
    let knownYears: [String]
    let year: String?
    let agentUsage: AgentUsagePayload?
    let trace: [TraceBucket]
}

/// Shared dashboard data for every lens. Base data (graph + model report)
/// loads when the popover opens; the hourly/agents reports load lazily the
/// first time their lens becomes active, mirroring the Tauri app's
/// empty-year short-circuit hooks.
@MainActor @Observable final class DashboardModel {
    /// Survives the model's deallocation so the next PopoverView starts with
    /// cached data instead of `.loading`. A deliberate process-lifetime cache
    /// (one COW-shared value snapshot, never invalidated). Every model may
    /// *read* it on init, but only the popover's model *writes* it (gated by
    /// `cachesSnapshot`): SettingsWindowView's independent DashboardModel runs
    /// the same poll loops on a year frozen at its own init, so letting it
    /// write here would clobber the snapshot with the settings model's stale
    /// year and re-introduce the reopen flash. TODO: the cleaner end-state is
    /// StatusItemController owning one long-lived DashboardModel injected via
    /// `.environment`, with the poll loops started/stopped explicitly on
    /// popover open/close — that deletes this static, DashboardSnapshot, and
    /// the year guard while preserving the Phase B CPU win.
    private static var lastSnapshot: DashboardSnapshot?
    /// Whether this model owns the shared `lastSnapshot` (true only for the
    /// popover's model, whose teardown/rebuild is what the cache speeds up).
    private let cachesSnapshot: Bool
    enum Phase {
        case loading
        case ready
        case failed(String)
    }

    private(set) var phase: Phase
    private static let yearKey = "tokenbar.dashboard.year"

    /// Resolve the active year filter: the `--year=` debug flag wins, else the
    /// persisted selection. Shared by `init()`'s snapshot guard and the `year`
    /// property initializer so the two can never drift (the guard MUST compute
    /// the same value the property does, or it would mis-classify a consistent
    /// snapshot as stale). nil = all time.
    private static func resolveYear() -> String? {
        CommandLine.arguments
            .first(where: { $0.hasPrefix("--year=") })
            .map { String($0.dropFirst("--year=".count)) }
            ?? UserDefaults.standard.string(forKey: yearKey)
    }

    /// `cachesSnapshot` = true only for the popover's model (PopoverView), the
    /// one whose per-open teardown/rebuild the cache exists to speed up; the
    /// settings window passes false so it never writes the shared snapshot.
    init(cachesSnapshot: Bool = false) {
        self.cachesSnapshot = cachesSnapshot
        // Guard snapshot restore on year-consistency: if the user changed the
        // year filter after the snapshot was written (e.g. setYear() persisted
        // the new year but reload() failed before apply() ran), the cached
        // payload is for the wrong slice — fall through to .loading so load()
        // fetches fresh. resolveYear() is a static (no `self`); @Observable
        // turns `year` into a computed accessor that touches self, so it is not
        // readable here until `phase` is set — hence the shared static helper,
        // which mirrors the `year` property initializer exactly.
        if let snap = Self.lastSnapshot, snap.year == Self.resolveYear() {
            payload = snap.payload
            stats = snap.stats
            modelReport = snap.modelReport
            colors = snap.colors
            knownYears = snap.knownYears
            agentUsage = snap.agentUsage
            trace = snap.trace
            phase = .ready
        } else {
            phase = .loading
        }
    }

    /// Year filter for every lens (HeaderBar's year select in the Tauri app);
    /// nil = all time. Persisted so the selection survives the popover's
    /// rootView teardown/rebuild cycle.
    /// `--year=<yyyy>` preselects a year (debug/screenshot aid).
    private(set) var year: String? = DashboardModel.resolveYear()
    /// Union of `payload.years` across loads — a year-filtered payload only
    /// reports the selected year, so remember the rest for the picker.
    private(set) var knownYears: [String] = []
    private(set) var payload: UsagePayload?
    private(set) var stats: UsageStats?
    private(set) var modelReport: ModelReport?
    private(set) var colors = ModelColorMap(report: nil)
    private(set) var hourly: HourlyReport?
    private(set) var agents: AgentsReport?
    private(set) var agentUsage: AgentUsagePayload?
    private(set) var trace: [TraceBucket] = []

    /// TBCore is blocking — every fetch hops off the main actor.
    func load() async {
        do {
            let year = self.year
            async let payloadTask = Task.detached(priority: .userInitiated) {
                try TBCore.graph(year: year)
            }.value
            async let reportTask = Task.detached(priority: .userInitiated) {
                try? TBCore.modelReport(year: year)
            }.value
            let payload = try await payloadTask
            let report = await reportTask
            // The year may have changed while we were off-actor (the user can
            // open the year menu during the initial load); drop a stale slice
            // so apply() never tags the new year — and the static snapshot —
            // with the old year's payload. Mirrors reload()/pollGraph().
            guard self.year == year else { return }
            apply(payload: payload, report: report)
        } catch {
            // Keep showing stale data over an error screen when a previous
            // load succeeded — a transient failure must not blank the UI.
            if payload == nil {
                phase = .failed("Failed to load usage: \(error)")
            }
        }
    }

    private(set) var refreshing = false

    /// Manual refresh: force a full log re-read (bypassing the staticlib's
    /// 30s cache) and drop the lazy per-lens reports so they re-fetch.
    func refresh() async {
        guard !refreshing else { return }
        refreshing = true
        defer { refreshing = false }
        await reload(force: true)
    }

    /// Switch the year filter and re-fetch every lens for the new slice.
    /// Served from the staticlib's per-year cache when fresh, so flipping
    /// back to a recent year is instant.
    func setYear(_ newYear: String?) async {
        guard newYear != year, !refreshing else { return }
        year = newYear
        UserDefaults.standard.set(newYear, forKey: Self.yearKey)
        refreshing = true
        defer { refreshing = false }
        await reload(force: false)
    }

    private func reload(force: Bool) async {
        let year = self.year
        async let payloadTask = Task.detached(priority: .userInitiated) {
            force ? try TBCore.refreshGraph(year: year) : try TBCore.graph(year: year)
        }.value
        async let reportTask = Task.detached(priority: .userInitiated) {
            try? TBCore.modelReport(year: year)
        }.value
        guard let payload = try? await payloadTask else { return }
        let report = await reportTask
        apply(payload: payload, report: report)
        // If apply() cleared a now-empty year filter, it spawned its own
        // unfiltered reload that re-fetches the lazy lenses for the new (nil)
        // year — skip the stale-`year` re-fetch here, or an empty year-filtered
        // hourly/agents could land after it and blank those lenses.
        guard self.year == year else { return }
        // Re-fetch the lazy lenses that were already loaded.
        if hourly != nil {
            hourly = await Task.detached(priority: .userInitiated) {
                try? TBCore.hourlyReport(year: year)
            }.value
        }
        if agents != nil {
            agents = await Task.detached(priority: .userInitiated) {
                try? TBCore.agentsReport(year: year)
            }.value
        }
    }

    private func apply(payload: UsagePayload, report: ModelReport?) {
        // A year-filtered payload reports only the selected year (empty if that
        // year has no data). Validate the filter against THIS fresh payload —
        // not the knownYears union, which never drops a year once seen — so a
        // selected year whose logs were deleted/moved (even while the popover
        // stays open) clears instead of stranding the dashboard on an empty
        // slice. Re-fetch unfiltered so all data shows immediately.
        if let year, !payload.years.contains(where: { $0.year == year }) {
            self.year = nil
            UserDefaults.standard.removeObject(forKey: Self.yearKey)
            Task { [weak self] in await self?.reload(force: false) }
            return
        }
        self.payload = payload
        stats = UsageStats(payload: payload, selectedClients: Set(payload.summary.clients))
        modelReport = report
        colors = ModelColorMap(report: report)
        knownYears = Set(knownYears + payload.years.map(\.year)).sorted(by: >)
        phase = .ready
        cacheSnapshot()
    }

    /// Capture the full restore cache from the current state. Called ONLY from
    /// apply(), where the year-scoped payload/stats and `year` are set together
    /// and `year` has been validated against the payload — so the snapshot's
    /// `year` always matches the slice its `payload` holds. No-op unless this
    /// model owns the cache and a base payload has loaded.
    private func cacheSnapshot() {
        guard cachesSnapshot, let payload, let stats else { return }
        Self.lastSnapshot = DashboardSnapshot(
            payload: payload, stats: stats, modelReport: modelReport,
            colors: colors, knownYears: knownYears, year: year,
            agentUsage: agentUsage, trace: trace)
    }

    /// Refresh only the live, year-independent fields (agentUsage/trace) of the
    /// existing snapshot from their pollers, keeping the payload/year pair that
    /// apply() last wrote. The pollers run outside apply() and must NOT
    /// re-capture payload/year: self.year can momentarily disagree with
    /// self.payload mid year-switch (setYear flips year before reload's apply
    /// lands) or after the empty-year auto-clear, and writing that pair would
    /// mis-tag a stale payload with a changed year that the init guard can't
    /// catch. Preserving snap.payload/snap.year keeps the cache consistent.
    /// No-op until apply() has written a base snapshot.
    private func refreshSnapshotLiveData() {
        guard cachesSnapshot, let snap = Self.lastSnapshot else { return }
        Self.lastSnapshot = DashboardSnapshot(
            payload: snap.payload, stats: snap.stats, modelReport: snap.modelReport,
            colors: snap.colors, knownYears: snap.knownYears, year: snap.year,
            agentUsage: agentUsage, trace: trace)
    }

    /// Periodically re-derive every loaded lens so the popover advances while
    /// it stays open. StatusItemController tears down and rebuilds PopoverView
    /// on each open/close cycle, so `.task { load() }` runs on every open and
    /// this loop is cancelled on close — but while open, without this loop the
    /// overview bars never pick up today's usage until a manual Refresh. Uses
    /// the non-forced graph() path: the staticlib's mtime-aware cache makes
    /// idle ticks cheap and only re-aggregates when logs actually change.
    /// Keeps stale data on error (only assigns on success).
    func pollGraph() async {
        while !Task.isCancelled {
            // Sleep first: load()'s initial fetch already covers t=0.
            try? await Task.sleep(for: .seconds(60))
            if Task.isCancelled { break }
            // Don't race an in-flight manual Refresh or year switch.
            guard !refreshing else { continue }
            let year = self.year
            async let payloadTask = Task.detached(priority: .utility) {
                try? TBCore.graph(year: year)
            }.value
            async let reportTask = Task.detached(priority: .utility) {
                try? TBCore.modelReport(year: year)
            }.value
            let fetched = await payloadTask
            let report = await reportTask
            if Task.isCancelled { break }
            // The year may have changed while we were off-actor; drop a stale
            // slice so the chart never flickers to the wrong year.
            guard self.year == year, let payload = fetched else { continue }
            apply(payload: payload, report: report)
            // apply() may have cleared a now-empty year filter and spawned an
            // unfiltered reload; skip the stale-`year` lazy re-fetch so it
            // can't blank Hourly/Agents with empty year-filtered reports.
            guard self.year == year else { continue }
            // Re-fetch the lazy lenses that were already loaded (mirrors reload).
            if hourly != nil {
                hourly = await Task.detached(priority: .utility) {
                    try? TBCore.hourlyReport(year: year)
                }.value
            }
            if agents != nil {
                agents = await Task.detached(priority: .utility) {
                    try? TBCore.agentsReport(year: year)
                }.value
            }
        }
    }

    /// Poll the OAuth quota snapshots while the popover is open. The fetch is
    /// network-bound (up to ~30s when a provider hangs), so failures keep the
    /// previous payload; per-provider errors live inside each snapshot.
    func pollAgentUsage() async {
        while !Task.isCancelled {
            let payload = try? await Task.detached(priority: .utility) {
                try TBCore.agentUsage()
            }.value
            if Task.isCancelled { break }
            if let payload {
                agentUsage = payload
                refreshSnapshotLiveData() // keep the reopen cache's quota cards current
            }
            try? await Task.sleep(for: .seconds(60))
        }
    }

    /// Poll the live tail (10-minute window) — drives the limits card's
    /// "Live" badge now and the trace card in a later phase. The staticlib
    /// re-parses at most every 10s, so this matches its cadence.
    func pollTrace() async {
        while !Task.isCancelled {
            let buckets = try? await Task.detached(priority: .utility) {
                try TBCore.usageTrace(windowSecs: 600)
            }.value
            if Task.isCancelled { break }
            if let buckets {
                trace = buckets
                refreshSnapshotLiveData() // keep the reopen cache's live trace current
            }
            try? await Task.sleep(for: .seconds(10))
        }
    }

    /// Fetch the lazy per-lens reports on first activation — and, because
    /// PopoverView's `.task` is keyed on the year too, again for the active
    /// lens after a year switch. Re-checks the year after the off-actor fetch
    /// (mirrors load()/reload()/pollGraph()): a year change mid-fetch drops the
    /// stale slice instead of stranding the previous year's report on the lens,
    /// and the keyed `.task` re-fires to fetch the new year while the report is
    /// still nil (reload()'s lazy re-fetch only covers an already-loaded lens).
    func ensureData(for view: AppView) async {
        let year = self.year
        switch view {
        case .hourly where hourly == nil:
            let report = await Task.detached(priority: .userInitiated) {
                try? TBCore.hourlyReport(year: year)
            }.value
            guard self.year == year else { return }
            hourly = report
        case .agents where agents == nil:
            let report = await Task.detached(priority: .userInitiated) {
                try? TBCore.agentsReport(year: year)
            }.value
            guard self.year == year else { return }
            agents = report
        default:
            break
        }
    }
}
