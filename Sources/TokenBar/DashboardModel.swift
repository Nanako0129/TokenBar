import Foundation
import Observation
import TokenBarCore

/// The set of analysis lenses, echoing tokscale's TUI tabs. The client tab
/// (Overview/Claude/Codex…, later phase) filters *which* data; this picks
/// *how* it is broken down. The two compose.
enum AppView: String, CaseIterable {
    /// `quota` sits second because it answers the question `overview` raises:
    /// the summary says which subscription is tightest, this lens is where that
    /// subscription's window and its past windows live. Position is product
    /// order, not an implementation detail — the tab row renders declaration
    /// order, and `SelfTest` pins it.
    case overview, quota, models, monthly, daily, hourly, stats, agents

    /// Title-cased id, then looked up: the English label doubles as the
    /// translation key, while `rawValue` stays the persisted id.
    var label: String { (rawValue.prefix(1).uppercased() + rawValue.dropFirst()).localized }

    /// Lenses the user can individually hide via Settings. Overview and
    /// Models are fixed anchors — Overview is the fallback target for every
    /// hidden lens (see `effective`), so it can never itself be hidden.
    static let toggleable: [AppView] = allCases.filter { $0 != .overview && $0 != .models }

    /// Lenses shown in the tab row, given the persisted hidden-set raw
    /// string. Same comma-separated-ids shape `ClientRegistry` uses for
    /// hidden client tabs — `ClientRegistry.parseIdSet` is reused verbatim,
    /// it's a generic CSV-id parser, not client-specific in implementation.
    /// Only `toggleable` lenses can ever actually be hidden — even a
    /// tampered raw string (e.g. a manually edited UserDefaults value)
    /// can't hide Overview or Models, since Overview must always remain the
    /// guaranteed fallback target (see `effective`).
    static func visible(hiddenRaw: String) -> [AppView] {
        let hidden = ClientRegistry.parseIdSet(hiddenRaw)
        return allCases.filter { !toggleable.contains($0) || !hidden.contains($0.rawValue) }
    }

    /// The view to actually render/label this frame. A hidden lens never
    /// survives — not even for the one frame before `resetViewIfHidden()`
    /// persists the correction — because a transient popover can reopen with
    /// a brand-new view instance whose `onChange` has nothing to compare
    /// against (see StatusItemController's `.transient` behavior). Same
    /// defensive shape as `lensContent`'s inline `singleClient` check for a
    /// just-hidden client tab. Guarded to `toggleable` lenses for the same
    /// tamper-resistance reason as `visible`.
    static func effective(_ view: AppView, hiddenRaw: String) -> AppView {
        guard toggleable.contains(view) else { return view }
        return ClientRegistry.parseIdSet(hiddenRaw).contains(view.rawValue) ? .overview : view
    }
}

/// Value-only apply guard for generated agent-usage payloads. Legacy/demo
/// payloads omit the generation and pass through without changing this state.
struct AgentUsagePublicationState {
    private var latestGeneration: UInt64?
    private var latestPayload: AgentUsagePayload?

    var latest: AgentUsagePayload? { latestPayload }

    mutating func resolve(_ candidate: AgentUsagePayload) -> AgentUsagePayload {
        guard let generation = candidate.publicationGeneration else { return candidate }
        if let latestGeneration, generation < latestGeneration {
            return latestPayload ?? candidate
        }
        latestGeneration = generation
        latestPayload = candidate
        return candidate
    }
}

/// Process-lifetime MainActor state shared by every UI consumer, including the
/// dashboard models and the independent tray poller.
@MainActor
enum AgentUsagePublicationCoordinator {
    private static var state = AgentUsagePublicationState()

    static var latestPayload: AgentUsagePayload? { state.latest }

    static func resolve(_ candidate: AgentUsagePayload) -> AgentUsagePayload {
        state.resolve(candidate)
    }

    /// Test seam only: back to the state of a process that has published
    /// nothing.
    ///
    /// This floor is process-wide and monotone, so a case that publishes a high
    /// generation silently rejects every later case's payload and hands it the
    /// earlier one instead — the later case then measures a fixture it never
    /// built, and fails for a reason nothing in it points at. That is not
    /// hypothetical: `M3-p` needs generations above every other fixture's to be
    /// published at all, and without this reset it broke `M3-f`, two hundred
    /// lines further down, by doing so.
    static func resetForTesting() {
        state = AgentUsagePublicationState()
    }
}

/// What a graph commit actually did. `GraphFetchOutcome.committed` used to be
/// reported unconditionally, even when `apply()`'s empty-year branch left the
/// filter cleared and a reload spawned without committing anything — so a
/// caller reading `.committed` as "the slice settled" was wrong exactly then.
enum ApplyResult: Equatable {
    /// The payload committed; `payload`/`stats`/`acceptedPayloadYear` moved.
    case applied
    /// The selected year was absent from this payload; the filter cleared and
    /// an unfiltered reload was spawned. Nothing committed here.
    case redirectedToAllYears
    /// The expected-year guard failed (a stale fetch for a year the model has
    /// since moved away from). Nothing committed, nothing spawned.
    case rejected
}

/// Which trigger owns a background graph fetch, for the header's freshness
/// indicator. `.manual` deliberately renders through the EXISTING
/// `refreshButton` spinner instead of a second one — see
/// `PopoverView.header`/`refreshButton`.
enum RefreshKind: Equatable {
    case initial, poll, yearSwitch, manual
}

/// A graph fetch currently in flight, keyed on the same `graphFetchToken`
/// ownership rule `commit`/`graphFetchFailed` already follow: only the fetch
/// that owns the token it was created with may clear it, so an overtaken
/// fetch's completion can never clear a NEWER request's indicator.
struct BackgroundRefresh: Equatable {
    let token: Int
    let kind: RefreshKind
}

/// Retained once a restore (memory or disk) leaves the dashboard showing data
/// that has not yet been confirmed current by a live fetch — restored numbers
/// with nothing on screen saying so are just stale numbers. Cleared only by
/// the first ACCEPTED commit (`ApplyResult.applied`); a redirect or a
/// rejection settles nothing, so the age stays displayed.
struct RestoredSnapshot: Equatable {
    let savedAt: Date
    var failed: Bool
}

/// Snapshot of the model's essential state, captured on each successful
/// load so a fresh DashboardModel can start in `.ready` state instead of
/// flashing "Loading usage…" every time the popover reopens.
///
/// `hourly` is excluded because it has its own year/client-keyed process cache;
/// `agents` remains uncached. `agentUsage`/`trace` are NOT lazy lenses — their
/// pollers (`pollAgentUsage`/`pollTrace`) fetch-first and overwrite
/// unconditionally — so caching them is staleness-free and keeps the Overview
/// tab's live/quota cards populated on reopen instead of flashing placeholders.
private struct DashboardSnapshot {
    let payload: UsagePayload
    let stats: UsageStats
    /// When `apply()` committed this `payload` — NOT when this struct itself
    /// was built, so `publishModel()`/`refreshSnapshotLiveData()` republishing
    /// the cache for an unrelated reason (a model landing, a trace poll) never
    /// makes the graph look freshly captured. Feeds the header's restored-age
    /// indicator on a same-process reopen.
    let payloadCapturedAt: Date
    let modelReport: ModelReport?
    /// The payload generation the cached `modelReport` was fetched for. It can
    /// legitimately lag `payload.meta.generatedAt`: a poll may commit a newer
    /// graph while the user sits on a lens that does not show models. Storing
    /// the model's own generation keeps the restore honest, so a lagging report
    /// is re-requested instead of being mistaken for current.
    let modelGeneratedAt: String?
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
    private struct HourlyCacheKey: Hashable {
        let year: String
        let clients: Set<String>
    }

    /// The (year, graph generation) pair a model report belongs to. Comparing
    /// the whole pair is what lets an in-flight scan be reused for a re-entry
    /// on the same slice while still being superseded by a genuinely newer one.
    private struct ModelSliceIdentity: Equatable {
        let year: String
        let generation: String
    }

    /// Survives the model's deallocation so the next PopoverView starts with
    /// cached data instead of `.loading`. A deliberate process-lifetime cache
    /// (one COW-shared value snapshot, never invalidated). Every model may
    /// *read* it on init, but only the most recently initialized popover model
    /// may write it: an older instance can outlive its view while an FFI scan
    /// finishes, and must not roll back the next popover's fresher snapshot.
    private static var lastSnapshot: DashboardSnapshot?

    /// The union scan, held across popover reopens like `lastSnapshot` is.
    /// `@State` on PopoverView does not survive the view being rebuilt, so an
    /// instance property here would make every reopen pay the full scan again
    /// — which is exactly what the staging was meant to stop being visible.
    ///
    /// In memory only, never on disk: it holds local usage rows, and the disk
    /// envelope structurally refuses that class of data.
    /// One scan per account, keyed by `accountKey` — nil for the primary,
    /// spelled `""` here because a dictionary cannot key on nil.
    ///
    /// Per account rather than one shared scan: a quota window belongs to one
    /// account, and a scan that spans accounts folds another account's work
    /// into the allowance this one reports (issue #258). The engine narrows
    /// each scan to its account's roots, so selecting the right entry here is
    /// the whole of the Swift side's job.
    private static var lastUnionScans: [String: UnionScan] = [:]
    private static var lastSnapshotOwner: ObjectIdentifier?
    /// Reopen cache for the expensive hourly fold. Multiple slices coexist so
    /// Daily/Monthly's Codex+Claude report cannot evict Hourly's all-client one.
    // ponytail: FIFO at eight slices bounds memory; use LRU only if churn shows misses.
    private static let hourlyCacheLimit = 8
    private static var hourlyCache: [HourlyCacheKey: HourlyReport] = [:]
    private static var hourlyCacheOrder: [HourlyCacheKey] = []

    /// Drop every process-wide cache holding scan-derived data.
    ///
    /// Called when the extra-scan-root registry is replaced. The engine clears
    /// its own caches there and the setter's contract is that the next report
    /// picks up the new roots — but these live on THIS side of the FFI, and
    /// each serves its answer without asking the engine: the union scan for
    /// `unionScanMaxAge`, the hourly fold and the reopen snapshot until
    /// something evicts them. A root added in Settings would otherwise be
    /// missing from the quota cards, or a removed one still counted, for as
    /// long as the Swift copy outlived the change the engine had already
    /// applied.
    @MainActor
    static func invalidateScanDerivedCaches() {
        // Supersede in-flight loads, not just completed values. A suspended
        // `refreshWindowUsage` snapshotted its range before the roots changed
        // and the FFI returns its pre-change result regardless, so clearing
        // alone leaves the old task free to repopulate what was just cleared.
        // Advancing the token makes its own `windowScanToken == scanToken`
        // guard drop it — the guard already existed, it was simply never told
        // that a root change also overtakes a scan.
        windowScanToken &+= 1
        lastUnionScans.removeAll()
        lastSnapshot = nil
        lastSnapshotOwner = nil
        // The PERSISTED snapshot needs no clearing here, and deliberately gets
        // none. It records the root set it was built under, so the restore in
        // `init` rejects it by comparison — which also covers the routes a
        // clear cannot reach: the process dying between the edit and the
        // clear, and the registry being changed while the app is not running.
        // Clearing it here as well would only turn a rejected restore into a
        // missing one.
        hourlyCache = [:]
        hourlyCacheOrder = []
        // The fourth one, and it does not live on this type. The first version
        // of this function cleared the three statics it could see from here and
        // missed the attributed series' own reopen cache, which republishes
        // before awaiting a fresh graph — so the subscription trend kept
        // showing a removed root. "Every process-wide cache holding
        // scan-derived data" is the rule; being declared elsewhere is not an
        // exemption from it.
        AttributedSeriesModel.invalidateRowCache()
    }
    /// Whether this model participates in the shared `lastSnapshot` cache.
    /// The newest participating instance becomes its sole writer.
    private let cachesSnapshot: Bool
    private let source: any UsageDataSource
    /// The exact build this process ships as, or nil for anything that is not
    /// the shipping bundle (including every `swift run` invocation — demo,
    /// smoke, selftest, icon-gallery among them). A disk snapshot is read or
    /// written only when this is non-nil.
    private let buildIdentity: BuildIdentity?
    /// Resolved from the injected `snapshotDirectory` autoclosure ONLY when
    /// `cachesSnapshot && buildIdentity != nil` — never merely evaluated by
    /// default. `SnapshotStore.defaultDirectory()` is itself real
    /// `FileManager` work, and the isolation contract this app relies on
    /// (never touch the production location outside the shipping bundle) is
    /// that it is never even RESOLVED off that identity, not only never
    /// written to.
    private let resolvedSnapshotDirectory: URL?
    enum Phase {
        case loading
        case ready
        case failed(String)
    }

    private(set) var phase: Phase
    private static let yearKey = "tokenbar.dashboard.year"

    /// Empty is the explicit identity for an all-time slice; nil below means
    /// that no accepted payload/report exists yet, so it cannot masquerade as
    /// all-time.
    private static func identityYear(_ year: String?) -> String { year ?? "" }

    /// Resolve the active year filter: the `--year=` debug flag wins, else the
    /// persisted selection. Used as `init()`'s default so the snapshot guard and
    /// the model's `year` can never drift (the guard MUST compare the same value
    /// the model fetches, or it would mis-classify a consistent snapshot as stale).
    /// Callers that own process-wide settings may explicitly pass nil for all time.
    private static func resolveYear() -> String? {
        CommandLine.arguments
            .first(where: { $0.hasPrefix("--year=") })
            .map { String($0.dropFirst("--year=".count)) }
            ?? UserDefaults.standard.string(forKey: yearKey)
    }

    private static let snapshotDecoder = JSONDecoder()

    /// `cachesSnapshot` = true only for the popover's model (PopoverView), the
    /// one whose per-open teardown/rebuild the cache exists to speed up; the
    /// settings window passes false so it never writes the shared snapshot,
    /// and never reads the disk one either.
    ///
    /// `buildIdentity`/`snapshotDirectory` are injectable so a hermetic test
    /// can drive the disk path without ever touching the production location
    /// — production always takes the defaults. `snapshotDirectory` is an
    /// `@autoclosure` specifically so it is not evaluated unless
    /// `cachesSnapshot && buildIdentity != nil`; see `resolvedSnapshotDirectory`.
    init(
        cachesSnapshot: Bool = false,
        source: any UsageDataSource = UsageDataSources.current,
        initialYear: String? = DashboardModel.resolveYear(),
        buildIdentity: BuildIdentity? = BuildIdentity.shipping(),
        snapshotDirectory: @autoclosure () -> URL? = SnapshotStore.defaultDirectory()
    ) {
        self.cachesSnapshot = cachesSnapshot
        self.source = source
        self.year = initialYear
        self.buildIdentity = buildIdentity
        self.resolvedSnapshotDirectory =
            (cachesSnapshot && buildIdentity != nil) ? snapshotDirectory() : nil

        // Guard snapshot restore on year-consistency: if the user changed the
        // year filter after the snapshot was written (e.g. setYear() persisted
        // the new year but reload() failed before apply() ran), the cached
        // payload is for the wrong slice — fall through to .loading so load()
        // fetches fresh. Settings passes nil explicitly because its client-item
        // controls must use the same all-time graph universe as AppDelegate.
        if let snap = Self.lastSnapshot, snap.year == initialYear {
            payload = snap.payload
            stats = snap.stats
            modelReport = snap.modelReport
            colors = snap.colors
            knownYears = snap.knownYears
            acceptedPayloadYear = Self.identityYear(initialYear)
            // Restore the model's own slice identity alongside it, or the first
            // model-dependent lens would re-request a report the snapshot
            // already carries and flash a loading state on every reopen. Use
            // the model's recorded generation rather than the payload's: when
            // it lags, the lens SHOULD re-request instead of treating a stale
            // report as current.
            var modelCurrent = false
            if snap.modelReport != nil {
                modelYear = Self.identityYear(initialYear)
                modelPayloadGeneratedAt = snap.modelGeneratedAt
                modelCurrent = snap.modelGeneratedAt == snap.payload.meta.generatedAt
            }
            agentUsage = snap.agentUsage.map {
                AgentUsagePublicationCoordinator.resolve($0)
            }
            trace = snap.trace
            phase = .ready
            restoredSnapshot = RestoredSnapshot(savedAt: snap.payloadCapturedAt, failed: false)
            // Installed ONLY when the restored model report is absent or lags
            // its payload — a restore whose model is already current for the
            // committed generation never has anything for the gate to guard
            // (`ensureModelReport` returns at its own identity check).
            if !modelCurrent { restoreGatePending = true }
        } else if cachesSnapshot, let identity = buildIdentity,
                  let directory = resolvedSnapshotDirectory,
                  let bytes = SnapshotStore.readBytes(in: directory),
                  let envelope = try? Self.snapshotDecoder.decode(SnapshotEnvelope.self, from: bytes),
                  SnapshotStore.validate(
                      envelope, expectedYear: initialYear, identity: identity,
                      scanRoots: ClaudeExtraRoots.appliedPayloadJSON)
        {
            // The model report is never persisted (see SnapshotEnvelope's doc
            // comment), so a disk restore ALWAYS leaves modelReport/modelYear/
            // modelPayloadGeneratedAt nil and the gate is always installed.
            payload = envelope.payload
            stats = UsageStats(
                payload: envelope.payload, selectedClients: Set(envelope.payload.summary.clients))
            knownYears = envelope.knownYears
            acceptedPayloadYear = Self.identityYear(initialYear)
            phase = .ready
            restoredSnapshot = RestoredSnapshot(savedAt: envelope.savedAt, failed: false)
            restoreGatePending = true
        } else {
            phase = .loading
        }
        if cachesSnapshot {
            Self.lastSnapshotOwner = ObjectIdentifier(self)
        }
    }

    /// Year filter for every lens (HeaderBar's year select in the Tauri app);
    /// nil = all time. Persisted so the selection survives the popover's
    /// rootView teardown/rebuild cycle.
    /// `--year=<yyyy>` preselects a year (debug/screenshot aid).
    private(set) var year: String?
    /// Union of `payload.years` across loads — a year-filtered payload only
    /// reports the selected year, so remember the rest for the picker.
    private(set) var knownYears: [String] = []
    private(set) var payload: UsagePayload?
    private(set) var stats: UsageStats?
    private(set) var modelReport: ModelReport?
    private(set) var colors = ModelColorMap(report: nil)
    /// The graph fetch currently running, for the header's freshness
    /// indicator. Distinct from `graphLoadTask`: this is presentation state
    /// (token + trigger kind) rather than the task itself, and is cleared by
    /// ownership the same way `commit`/`graphFetchFailed` are — see
    /// `gatedGraph`.
    private(set) var backgroundRefresh: BackgroundRefresh?
    /// Set whenever `init` restored a payload (memory or disk) that has not
    /// yet been confirmed current by a live fetch. Cleared only by the first
    /// accepted commit — see `RestoredSnapshot`'s doc comment.
    private(set) var restoredSnapshot: RestoredSnapshot?
    private(set) var hourly: HourlyReport?
    /// True while a model-report request is in flight. Model-dependent cards
    /// must distinguish this from a completed request that genuinely found
    /// nothing, or a deferred model reads as "no usage" during startup.
    private(set) var modelLoading = false
    private(set) var agents: AgentsReport?
    private(set) var agentUsage: AgentUsagePayload?
    /// True once the first `pollAgentUsage()` attempt has finished, whether it
    /// succeeded or not. Lets a view show a terminal state instead of waiting on
    /// a payload that may never arrive.
    private(set) var agentUsageAttempted = false
    private(set) var trace: [TraceBucket] = []

    // Memo for the hidden-client Overview slice: lensContent re-evals on every
    // ~10s trace poll, and re-aggregating UsageStats (incl. Streaks' full-range
    // double pass) each time is wasteful. Keyed on the payload's generatedAt
    // plus the selected set, so it recomputes only when either changes.
    // @ObservationIgnored: pure derived cache, never a view dependency, so
    // reading/writing it during a view update triggers no observation churn.
    @ObservationIgnored private var statsMemoGeneratedAt: String?
    @ObservationIgnored private var statsMemoSelected: Set<String>?
    @ObservationIgnored private var statsMemoValue: UsageStats?

    // The client selection each lazy report was last fetched for. Hourly/agents
    // buckets fold all clients into mixed totals, so the slice is now applied at
    // the FFI (accurate per-client totals); these track it so a tab switch or a
    // hide toggle refetches the right slice instead of serving another tab's.
    // nil = never fetched. Set-valued so a reorder (same members) is not a
    // refetch. Background refreshes (reload/pollGraph) reuse the stored slice.
    @ObservationIgnored private var hourlyClients: Set<String>?
    @ObservationIgnored private var agentsClients: Set<String>?
    /// Identity of the slice the current `modelReport` was fetched for. The
    /// year keeps a previous year's report off a newly-filtered dashboard; the
    /// payload generation is what makes the request idempotent per graph commit,
    /// so a model-dependent lens re-requests only when the graph actually moves.
    /// Readable because the attribution breakdown states the range its figures
    /// cover, and that has to be the range the report was fetched for rather
    /// than the currently selected year — a failed reload can move the selection
    /// while this report stands, and labelling stale rows with the new year is
    /// exactly the misreading the feature exists to prevent.
    @ObservationIgnored private(set) var modelYear: String?
    @ObservationIgnored private var modelPayloadGeneratedAt: String?
    @ObservationIgnored private var modelRequestToken = 0
    /// The slice a model scan is currently running for, used to coalesce
    /// re-entry. Nil when nothing is in flight.
    @ObservationIgnored private var modelInFlight: ModelSliceIdentity?
    /// The unstructured task carrying the in-flight scan. Held so a re-entrant
    /// caller can await the same work instead of starting its own, and so the
    /// scan can be cancelled when its slice stops being displayed.
    @ObservationIgnored private var modelTask: Task<ModelReport?, Never>?
    /// Whether a model-dependent lens has asked for the report in this slice.
    /// Only then is a missing report worth retrying on the poll — a session that
    /// never leaves Daily must not pay for a scan it does not render.
    @ObservationIgnored private var modelWanted = false
    /// The graph fetch currently running, held so a deferred model request
    /// waits for it instead of racing it. EVERY graph fetch installs it — the
    /// initial load, a manual refresh and the 60s poll alike.
    ///
    /// A restored snapshot makes the dashboard renderable before `load()` has
    /// fetched anything, so the model task's key is already a real value on the
    /// very first body evaluation and the task fires at once. The "no model
    /// scan until a payload commits" property therefore held only for a
    /// first-ever open; every reopen whose snapshot lacked a current model
    /// report put both full scans back on the same bounded pool.
    ///
    /// Refresh and poll need the same gate for a different reason: the model
    /// task is keyed on the LENS as well as the committed slice, so opening
    /// Overview (or expanding a row) mid-refresh raises a request even though
    /// the slice key has not moved and no payload has committed.
    @ObservationIgnored private var graphLoadTask: Task<GraphFetchOutcome, Error>?

    /// What a gated fetch DID, which is not what it returned. A superseded
    /// fetch holds a perfectly good payload and commits nothing, so a caller
    /// that reads its completion as a settled slice acts on state the newer
    /// fetch is about to replace — the poll would start its model and lazy
    /// reports beside a graph scan still running, back on the bounded pool.
    /// Returning the payload made those two cases indistinguishable; naming
    /// them is what removes the question.
    private enum GraphFetchOutcome {
        case committed(ApplyResult)
        case superseded
    }

    /// Installed by `init` only when it restored a payload whose model report
    /// is absent or not current for it. Awaited by `ensureModelReport` before
    /// it reads ANY committed state — without that, a model-task-first
    /// ordering finds the restored payload already "committed" (year matches,
    /// non-nil) and a nil `graphLoadTask` (because `load()` has not even
    /// called `gatedGraph` yet), and would scan against a payload nobody has
    /// confirmed live. Fulfilled from INSIDE the gated task in `gatedGraph`,
    /// on every exit — success, failure, and superseded alike — so task
    /// cancellation on the caller's side can never strand it, and so
    /// `reload()`/`pollGraph()` fulfil it too if either runs before `load()`.
    @ObservationIgnored private var restoreGatePending = false
    @ObservationIgnored private var restoreGateContinuations: [CheckedContinuation<Void, Never>] = []

    private func waitForRestoreGate() async {
        guard restoreGatePending else { return }
        await withCheckedContinuation { continuation in
            // Re-checked here (not just by the guard above) because both
            // hops are MainActor-only: this is what makes "no window where
            // fulfillment and the wait cross" structural rather than assumed.
            if restoreGatePending {
                restoreGateContinuations.append(continuation)
            } else {
                continuation.resume()
            }
        }
    }

    private func fulfillRestoreGate() {
        guard restoreGatePending else { return }
        restoreGatePending = false
        let waiting = restoreGateContinuations
        restoreGateContinuations = []
        waiting.forEach { $0.resume() }
    }

    /// Monotonic capture sequence for the disk writer, taken on the MAIN
    /// actor before handing off to the detached encode — see `submitDiskCapture`.
    private static var nextCaptureSequence = 0

    /// Runs a graph fetch under that gate and commits it INSIDE the gated task,
    /// so the gate opens on the commit rather than on the fetch.
    ///
    /// Ordering, not just exclusion, is what the model request needs. With only
    /// the fetch gated, the waiter and the fetch's owner resume from the same
    /// task in an unspecified order: the waiter could go first, read the
    /// pre-commit payload, and scan for a generation that `apply()` was about
    /// to supersede — two full scans, the contention this split exists to
    /// remove.
    ///
    /// In practice the owner registers on the task first and does commit first
    /// — the fetch-gated shape was mutated back in and no assertion moved. So
    /// this removes a reliance on an unspecified ordering, not a reproduced
    /// defect, and no test discriminates the two shapes. It also closes the
    /// matching window where the slot still holds a finished task because the
    /// owner has yet to resume.
    ///
    /// The lazy-lens re-fetches stay outside: the model does not depend on
    /// them, and holding the gate across them would make it wait for nothing.
    /// `commit` runs only on success, and runs on this actor — its callers
    /// keep their own year guard, but not `Task.isCancelled`, which inside an
    /// unstructured task no longer describes the caller. `apply()` re-checks
    /// the year itself, and a commit that lands after a popover close only
    /// leaves a fresher reopen snapshot behind.
    /// `commit` receives the payload AND the scan roots that were installed
    /// when this fetch was issued.
    ///
    /// The roots must travel with the request rather than be read at capture
    /// time. The FFI returns a scan that started before a registry replace even
    /// after the replace has landed, so stamping a snapshot with the roots
    /// current at the moment of capture labels pre-change data as post-change —
    /// and the persisted-snapshot check then accepts it on the next launch,
    /// which is exactly the acceptance the fingerprint was added to withhold.
    /// Reading it here, before the fetch, is the only point at which the value
    /// is known to describe the scan.
    private func gatedGraph(
        kind: RefreshKind,
        fetch: @escaping () async throws -> UsagePayload,
        commit: @escaping (UsagePayload) -> ApplyResult
    ) async throws -> GraphFetchOutcome {
        let requestRoots = ClaudeExtraRoots.appliedPayloadJSON
        graphFetchToken += 1
        let token = graphFetchToken
        // Ownership-cleared, exactly like `graphFetchFailed` below: an
        // overtaken fetch's completion must not clear a NEWER request's
        // indicator, so every clear site compares the token it was given
        // against the CURRENT `backgroundRefresh`, not against its own copy.
        backgroundRefresh = BackgroundRefresh(token: token, kind: kind)
        let task = Task { () throws -> GraphFetchOutcome in
            do {
                let payload = try await fetch()
                // Same ownership rule as the failure path below, and for the
                // same reason: an overtaken fetch must not touch displayed
                // state. Two same-year fetches can overlap — a manual Refresh
                // started while `load()` or a poll is still running — and the
                // year guards cannot separate them, so an older result landing
                // second would roll the dashboard and the reopen snapshot back
                // to its payload and clear the newer fetch's failure state.
                // (The rollback itself predates the split; guarding only the
                // error path was this file's own asymmetry.)
                guard self.graphFetchToken == token else {
                    self.clearBackgroundRefresh(owner: token)
                    self.fulfillRestoreGate()
                    return .superseded
                }
                // Assigned rather than passed through `commit`: the closure
                // is the caller's, and threading a value it never mentions
                // through three call sites bought nothing over setting it on
                // the line before. Both statements are on the main actor with
                // no suspension between them, so `apply` cannot observe a
                // different fetch's value.
                self.payloadScanRoots = requestRoots
                let result = commit(payload)
                self.clearBackgroundRefresh(owner: token)
                self.fulfillRestoreGate()
                return .committed(result)
            } catch {
                // Inside the task for the same reason `commit` is: a waiter
                // that resumes the instant the gate opens must not read this
                // before the failure is recorded. Only the newest fetch may
                // record it — an overtaken one describes a slice that is no
                // longer displayed, exactly as above.
                if self.graphFetchToken == token {
                    self.graphFetchFailed = true
                    self.restoredSnapshot?.failed = true
                }
                self.clearBackgroundRefresh(owner: token)
                self.fulfillRestoreGate()
                throw error
            }
        }
        graphLoadTask = task
        defer { if graphLoadTask == task { graphLoadTask = nil } }
        return try await task.value
    }

    private func clearBackgroundRefresh(owner token: Int) {
        guard backgroundRefresh?.token == token else { return }
        backgroundRefresh = nil
    }
    /// Identity of the last payload/hourly report that actually committed.
    /// These are separate from `year`: a newer request may complete in the
    /// inverse order, and the presentation accessors must stay fail-closed.
    @ObservationIgnored private var acceptedPayloadYear: String?
    @ObservationIgnored private var hourlyYear: String?
    @ObservationIgnored private var hourlyRequestToken = 0

    /// UsageStats for a client slice, with hidden clients already removed from
    /// `selected`. Returns the precomputed full `stats` when the slice covers
    /// every present client (the common no-hidden case — no recompute); other-
    /// wise returns a memoized instance, recomputing only when the payload or
    /// the selected set changes. Call site: PopoverView.lensContent.
    func stats(selecting selected: Set<String>) -> UsageStats? {
        guard let payload, let stats else { return nil }
        if selected == Set(stats.presentClients) { return stats }
        if statsMemoGeneratedAt == payload.meta.generatedAt,
           statsMemoSelected == selected, let memo = statsMemoValue {
            return memo
        }
        let computed = UsageStats(payload: payload, selectedClients: selected)
        statsMemoGeneratedAt = payload.meta.generatedAt
        statsMemoSelected = selected
        statsMemoValue = computed
        return computed
    }

    /// The Hourly lens can render a matching report as soon as it completes;
    /// unlike Daily/Monthly it intentionally does not wait for the graph
    /// payload, preserving the existing hourly behavior.
    func hourlyReport(for clients: [String]) -> HourlyReport? {
        guard let hourly,
              hourlyClients == Set(clients),
              hourlyYear == Self.identityYear(year)
        else { return nil }
        return hourly
    }


    /// Identity of the slice whose payload is actually committed and rendering.
    ///
    /// Distinct from `year`, which moves the instant the user picks a filter —
    /// before the payload catches up. Keying the model task on `year` meant the
    /// id changed at the moment of intent and then stayed put when the new
    /// payload landed, so a slice whose generation matched the previous one
    /// never re-fired the task and no model request followed. Two slices can
    /// share a generation: an all-years payload and a current-year payload are
    /// both dated today.
    var committedSliceKey: String {
        "\(acceptedPayloadYear ?? "-")|\(payload?.meta.generatedAt ?? "")"
    }

    /// The source owns the blocking FFI hop in live mode; demo mode returns
    /// synthetic values through the same async contract.
    ///
    /// Graph-first: the model report is NOT fetched here. Both are blocking FFI
    /// scans that share one bounded Rayon pool, so issuing them together made
    /// each pay for the other's contention — measured on a real corpus, graph
    /// alone returned in 1.4s warm / 27s cold, but concurrently with the model
    /// (and the lazy hourly) it took 7.2s / 67s to reach the same first paint
    /// without finishing any sooner overall. The dashboard only needs the graph
    /// to render, so model-dependent lenses request it afterwards through
    /// `ensureModelData(for:)`, keyed on the committed payload generation.
    func load() async {
        do {
            let year = self.year
            _ = try await gatedGraph(kind: .initial) { [source] in
                try await source.graph(year: year, priority: .userInitiated)
            } commit: { payload in
                // The year may have changed while we were off-actor (the user
                // can open the year menu during the initial load); drop a stale
                // slice so apply() never tags the new year — and the static
                // snapshot — with the old year's payload. Mirrors
                // reload()/pollGraph().
                guard self.year == year else { return .rejected }
                return self.apply(payload: payload, expectedYear: year)
            }
        } catch {
            // Keep showing stale data over an error screen when a previous
            // load succeeded — a transient failure must not blank the UI.
            if payload == nil {
                phase = .failed("Failed to load usage: %@".localized(String(describing: error)))
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
        await reload(force: true, kind: .manual)
    }

    /// A scan-root change, which must supersede whatever is in flight.
    ///
    /// Deliberately NOT `refresh()`. That method returns at `guard !refreshing`
    /// when a manual refresh or an earlier root apply is still running — and
    /// then it never enters `gatedGraph`, never advances `graphFetchToken`, and
    /// the older scan's old-root payload commits after the invalidation. The
    /// case the guard exists for (a user pressing Refresh twice) is the case
    /// this must not obey: two quick Settings edits are exactly when the
    /// superseding matters.
    ///
    /// It also leaves `refreshing` alone rather than setting it, so it cannot
    /// clear a flag a concurrent manual refresh owns.
    func reloadForRootChange() async {
        await reload(force: true, kind: .manual)
    }

    /// Switch the year filter and re-fetch every lens for the new slice.
    /// Served from the staticlib's per-year cache when fresh, so flipping
    /// back to a recent year is instant.
    func setYear(_ newYear: String?) async {
        guard newYear != year, !refreshing else { return }
        year = newYear
        invalidateHourly()
        invalidateModel()
        UserDefaults.standard.set(newYear, forKey: Self.yearKey)
        refreshing = true
        defer { refreshing = false }
        await reload(force: false, kind: .yearSwitch)
    }

    /// Auto-clear a year filter scoped to a year that only hidden clients used.
    /// The best-effort year picker can't drop such a year while it is the active
    /// selection (the payload is year-scoped then), so a dashboard already
    /// stranded on it — or one where the user just hid the year's only client —
    /// would show an empty slice. When the CURRENT year-scoped payload has no
    /// visible (non-hidden) stripe, fall back to All years via `setYear(nil)`,
    /// which reuses the existing year-clear discipline (persist + reload with
    /// the stale-year guards). No-op on All-years, before data loads, or when
    /// any visible activity exists. Reactive: PopoverView calls this on a hide
    /// toggle and on payload load.
    func clearYearIfHiddenOnly(hidden: Set<String>) async {
        guard year != nil, let payload, !refreshing else { return }
        if !UsageStats.hasVisibleActivity(contributions: payload.contributions, hidden: hidden) {
            await setYear(nil)
        }
    }

    private func beginHourlyRequest() -> Int {
        hourlyRequestToken += 1
        return hourlyRequestToken
    }

    private func beginModelRequest() -> Int {
        modelRequestToken += 1
        return modelRequestToken
    }

    /// Drop a model report that belongs to a slice the dashboard no longer
    /// shows. Also cancels any in-flight request so its late completion cannot
    /// publish the previous year's models.
    private func invalidateModel() {
        modelReport = nil
        colors = ModelColorMap(report: nil)
        modelYear = nil
        modelPayloadGeneratedAt = nil
        modelLoading = false
        // Release the coalescing slot too: the in-flight scan belongs to the
        // slice being discarded, and leaving its identity set would let it
        // block a legitimate request should the user return to that slice.
        modelInFlight = nil
        modelTask?.cancel()
        modelTask = nil
        modelWanted = false
        // Every caller discards the model because a new slice is arriving, so
        // the honest state is "not known yet", not "none". Setting it here
        // rather than relying on a task to enter and set it is what survives a
        // key that does not change until the payload commits.
        modelLoading = true
        _ = beginModelRequest()
    }

    private func publishModel(_ report: ModelReport, year: String?, generation: String) {
        modelReport = report
        colors = ModelColorMap(report: report)
        modelYear = Self.identityYear(year)
        modelPayloadGeneratedAt = generation
        cacheSnapshot()
    }

    private func invalidateHourly() {
        hourly = nil
        hourlyClients = nil
        hourlyYear = nil
        _ = beginHourlyRequest()
    }

    private func publishHourly(
        _ report: HourlyReport, year: String?, clients: Set<String>
    ) {
        let yearKey = Self.identityYear(year)
        hourly = report
        hourlyClients = clients
        hourlyYear = yearKey
        if cachesSnapshot {
            let cacheKey = HourlyCacheKey(year: yearKey, clients: clients)
            if Self.hourlyCache[cacheKey] == nil {
                Self.hourlyCacheOrder.append(cacheKey)
                if Self.hourlyCacheOrder.count > Self.hourlyCacheLimit {
                    Self.hourlyCache.removeValue(forKey: Self.hourlyCacheOrder.removeFirst())
                }
            }
            Self.hourlyCache[cacheKey] = report
        }
    }

    private func reload(force: Bool, kind: RefreshKind) async {
        let year = self.year
        // Graph-first here too: the model is refreshed after the graph commits,
        // and only when a lens had already loaded it (mirrors hourly/agents).
        let outcome: GraphFetchOutcome
        do {
            outcome = try await gatedGraph(kind: kind) { [source] in
                force
                    ? try await source.refreshGraph(year: year, priority: .userInitiated)
                    : try await source.graph(year: year, priority: .userInitiated)
            } commit: { payload in
                guard self.year == year else { return .rejected }
                return self.apply(payload: payload, expectedYear: year)
            }
        } catch {
            // A model that has never reached `.ready` must still settle. apply()
            // spawns this reload for an emptied year filter and returns BEFORE
            // setting `.ready`, so a failure here would otherwise strand phase on
            // `.loading` forever — the dashboard spins and Settings keeps
            // "looking for clients". Once ready, keep the stale-data-over-error
            // behavior a manual refresh relies on.
            if case .loading = phase {
                phase = .failed("Failed to load usage: %@".localized(String(describing: error)))
            }
            return
        }
        // A superseded fetch settled nothing: the newer one owns this slice and
        // re-fetches these lenses itself. Driving them here would put an hourly
        // and an agents scan beside a graph scan still running.
        guard case .committed = outcome else { return }
        // If apply() cleared a now-empty year filter, it spawned its own
        // unfiltered reload that re-fetches the lazy lenses for the new (nil)
        // year — skip the stale-`year` re-fetch here, or an empty year-filtered
        // hourly/agents could land after it and blank those lenses.
        guard self.year == year, !Task.isCancelled else { return }
        // Heal a stale model report that a lens already asked for, just as the
        // poll does. The shared seam owns the wanted/stale checks and coalesces
        // PopoverView's generation-keyed refire onto the same in-flight scan.
        await retryModelIfStale(priority: .userInitiated)
        // The retry may suspend while cancellation or a year change retires this
        // reload; do not issue lazy work for the slice it no longer owns.
        guard self.year == year, !Task.isCancelled else { return }
        // Re-fetch the lazy lenses that were already loaded, keeping the slice
        // they were last fetched for (an ordered array of the stored Set — the
        // FFI filter is membership-based, so order is irrelevant). Re-check the
        // stored slice AFTER the await: a tab switch during the fetch commits a
        // new slice via ensureData, and the slice-keyed `.task` won't refetch
        // (its key already records the new tab), so a stale overwrite here would
        // strand the wrong slice on the lens.
        if hourly != nil {
            let captured = hourlyClients
            let requestToken = beginHourlyRequest()
            let report = try? await source.hourlyReport(
                year: year, clients: captured.map(Array.init), priority: .userInitiated)
            if !Task.isCancelled,
               self.year == year,
               self.hourlyClients == captured,
               self.hourlyYear == Self.identityYear(year),
               hourlyRequestToken == requestToken,
               let report,
               let captured
            {
                publishHourly(report, year: year, clients: captured)
            }
        }
        if agents != nil {
            let captured = agentsClients
            let report = try? await source.agentsReport(
                year: year, clients: captured.map(Array.init), priority: .userInitiated)
            if self.year == year, self.agentsClients == captured { agents = report }
        }
    }

    /// Commit a graph payload and make the dashboard renderable. The model
    /// report is owned separately (`ensureModelData`/`publishModel`) so a graph
    /// commit never has to wait for it, and a still-loading model never blanks
    /// the cards that already have last-good data.
    ///
    /// Returns what actually happened — see `ApplyResult`. Only `.applied`
    /// clears `restoredSnapshot`: a redirect or a rejection settles nothing,
    /// so restored data stays flagged as not-yet-confirmed.
    @discardableResult
    private func apply(payload: UsagePayload, expectedYear: String? = nil) -> ApplyResult {
        guard expectedYear == nil || self.year == expectedYear else { return .rejected }
        // A year-filtered payload reports only the selected year (empty if that
        // year has no data). Validate the filter against THIS fresh payload —
        // not the knownYears union, which never drops a year once seen — so a
        // selected year whose logs were deleted/moved (even while the popover
        // stays open) clears instead of stranding the dashboard on an empty
        // slice. Re-fetch unfiltered so all data shows immediately.
        if let year, !payload.years.contains(where: { $0.year == year }) {
            invalidateHourly()
            invalidateModel()
            acceptedPayloadYear = nil
            self.year = nil
            UserDefaults.standard.removeObject(forKey: Self.yearKey)
            Task { [weak self] in await self?.reload(force: false, kind: .yearSwitch) }
            return .redirectedToAllYears
        }
        // A model report fetched for a different year describes a slice this
        // payload no longer shows, so drop it rather than render it beside the
        // new graph; the model-dependent lens re-requests for the new slice.
        if modelReport != nil, modelYear != Self.identityYear(year) {
            invalidateModel()
        }
        self.payload = payload
        acceptedPayloadYear = Self.identityYear(year)
        graphFetchFailed = false
        stats = UsageStats(payload: payload, selectedClients: Set(payload.summary.clients))
        knownYears = Set(knownYears + payload.years.map(\.year)).sorted(by: >)
        phase = .ready
        payloadCapturedAt = Date()
        restoredSnapshot = nil
        cacheSnapshot()
        submitDiskCapture()
        return .applied
    }

    /// When `apply()` last committed a payload — the timestamp `cacheSnapshot()`
    /// stamps into `DashboardSnapshot.payloadCapturedAt`. Deliberately NOT
    /// touched by `publishModel()`/`refreshSnapshotLiveData()`, which republish
    /// the cache for reasons unrelated to the graph moving.
    @ObservationIgnored private var payloadCapturedAt = Date()

    /// The scan roots the CURRENT payload was fetched under, captured before
    /// its request went out rather than when the snapshot is written. Seeded
    /// with the roots at construction so a capture that somehow precedes any
    /// apply() records this process's actual set rather than an empty one.
    /// The scan roots the CURRENT payload was fetched under, captured before
    /// its request went out rather than when the snapshot is written.
    @ObservationIgnored private var payloadScanRoots = ClaudeExtraRoots.appliedPayloadJSON

    /// The roots stamped into the snapshot for the committed payload.
    var payloadScanRootsForTesting: String { payloadScanRoots }

    private var ownsLastSnapshot: Bool {
        cachesSnapshot && Self.lastSnapshotOwner == ObjectIdentifier(self)
    }

    private func replaceLastSnapshot(_ snapshot: DashboardSnapshot) {
        guard ownsLastSnapshot else { return }
        Self.lastSnapshot = snapshot
    }

    /// Capture the full restore cache from the current state. `apply()` first
    /// commits the validated payload/year pair; `publishModel()` may then add a
    /// report for that same slice. No-op unless this is the newest popover model
    /// and a base payload has loaded.
    private func cacheSnapshot() {
        guard cachesSnapshot, let payload, let stats else { return }
        replaceLastSnapshot(DashboardSnapshot(
            payload: payload, stats: stats, payloadCapturedAt: payloadCapturedAt,
            modelReport: modelReport,
            modelGeneratedAt: modelPayloadGeneratedAt,
            colors: colors, knownYears: knownYears, year: year,
            agentUsage: agentUsage, trace: trace))
    }

    /// Submit the DISK capture. Deliberately separate from `cacheSnapshot()`
    /// (the in-memory reopen cache, which both `apply()` and `publishModel()`
    /// write): only a graph commit reaches this, so a model landing on its own
    /// never triggers a disk write, and Settings' model (`cachesSnapshot ==
    /// false`) never resolves a directory to write to at all.
    private func submitDiskCapture() {
        guard cachesSnapshot, let identity = buildIdentity,
              let directory = resolvedSnapshotDirectory,
              let envelope = snapshotEnvelope(identity: identity)
        else { return }
        Self.nextCaptureSequence += 1
        let sequence = Self.nextCaptureSequence
        Task.detached(priority: .utility) {
            await SnapshotWriter.shared.submit(sequence: sequence, envelope: envelope, directory: directory)
        }
    }

    /// The envelope `submitDiskCapture` would write, built in one place so a
    /// self-test asserts the value that actually reaches disk.
    ///
    /// Not merely a refactor: an assertion on `payloadScanRoots` passes while
    /// the stamp beside it reads the CURRENT roots instead, which is the very
    /// substitution this field exists to prevent. The property is about what is
    /// written, so the test has to see what is written.
    func snapshotEnvelope(identity: BuildIdentity) -> SnapshotEnvelope? {
        guard let payload else { return nil }
        return SnapshotEnvelope(
            snapshotSchemaVersion: SnapshotEnvelope.schemaVersion,
            bundleIdentifier: identity.bundleIdentifier,
            shortVersion: identity.shortVersion,
            buildNumber: identity.buildNumber,
            savedAt: Date(),
            selectedYear: year,
            payload: payload,
            knownYears: knownYears,
            scanRoots: payloadScanRoots)
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
        replaceLastSnapshot(DashboardSnapshot(
            payload: snap.payload, stats: snap.stats, payloadCapturedAt: snap.payloadCapturedAt,
            modelReport: snap.modelReport,
            modelGeneratedAt: snap.modelGeneratedAt,
            colors: snap.colors, knownYears: snap.knownYears, year: snap.year,
            agentUsage: agentUsage, trace: trace))
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
            let fetched = try? await gatedGraph(kind: .poll) { [source] in
                try await source.graph(year: year, priority: .utility)
            } commit: { payload in
                // The year may have changed while we were off-actor; drop a
                // stale slice so the chart never flickers to the wrong year.
                guard self.year == year else { return .rejected }
                return self.apply(payload: payload, expectedYear: year)
            }
            if Task.isCancelled { break }
            // Same rule as reload: a superseded poll settled nothing, so its
            // model retry and lazy re-fetches would run beside the fetch that
            // overtook it.
            guard self.year == year, case .committed = fetched else { continue }
            // apply() may have cleared a now-empty year filter and spawned an
            // unfiltered reload; skip the stale-`year` lazy re-fetch so it
            // can't blank Hourly/Agents with empty year-filtered reports.
            guard self.year == year, !Task.isCancelled else { continue }
            // Retry a model report a lens asked for and does not have for the
            // committed slice. Before the graph/model split this poll re-fetched
            // it unconditionally, so a transient failure self-healed within 60s;
            // deferring the fetch removed that path. See `retryModelIfStale`
            // for why its condition is `modelWanted` and nothing else.
            await retryModelIfStale(priority: .utility)
            guard self.year == year, !Task.isCancelled else { continue }
            // Re-fetch the lazy lenses that were already loaded (mirrors reload),
            // keeping each one's last-fetched client slice.
            // Re-check the stored slice after the await (see reload()): a tab
            // switch mid-fetch must not let this background refresh overwrite
            // the fresh slice with the stale one.
            if hourly != nil {
                let captured = hourlyClients
                let requestToken = beginHourlyRequest()
                let report = try? await source.hourlyReport(
                    year: year, clients: captured.map(Array.init), priority: .utility)
                if !Task.isCancelled,
                   self.year == year,
                   self.hourlyClients == captured,
                   self.hourlyYear == Self.identityYear(year),
                   hourlyRequestToken == requestToken,
                   let report,
                   let captured
                {
                    publishHourly(report, year: year, clients: captured)
                }
            }
            if agents != nil {
                let captured = agentsClients
                let report = try? await source.agentsReport(
                    year: year, clients: captured.map(Array.init), priority: .utility)
                if self.year == year, self.agentsClients == captured { agents = report }
            }
        }
    }

    /// Card state per client. Never absent while an agent tab is open — the
    /// loading case is a state, not a nil.
    var windowCards: [String: WindowCardState] = [:]

    /// The scan for one account, restored from the shared cache so a reopen
    /// renders bars immediately instead of spinning.
    ///
    /// `accountKey` is the card's own — `nil` is the primary, which is what
    /// every client but an extra Claude account has. There is deliberately no
    /// accessor that returns "the scan", because there is no longer one scan
    /// and a caller reaching for it would silently get the primary's.
    private func unionScan(for accountKey: String?) -> UnionScan? {
        Self.lastUnionScans[Self.scanSlot(accountKey)]
    }

    private func setUnionScan(_ scan: UnionScan?, for accountKey: String?) {
        Self.lastUnionScans[Self.scanSlot(accountKey)] = scan
    }

    /// Dictionary key for an account. The empty string is the primary, which no
    /// real config directory can collide with: `claude_config_dirs` refuses an
    /// empty path, and Settings refuses one too.
    static func scanSlot(_ accountKey: String?) -> String { accountKey ?? "" }

    /// The account a window CARD is about, which is always the primary.
    ///
    /// Not a decision made here: `WindowCardLoader` selects
    /// `accountKey == nil` in `select`, `pickForHistory` and `quotaHalf`, so a
    /// client tab can only ever show the primary's windows. Named rather than
    /// written as a bare `nil` at four call sites, so the day an extra account
    /// gets a tab there is one place that has to change and a grep that finds
    /// it.
    static let cardAccountKey: String? = nil

    /// How long a scan is served before being refreshed. Matched to the
    /// engine's own oneshot age so the two layers do not disagree about what
    /// counts as fresh.
    private static let unionScanMaxAge: TimeInterval = 30

    /// Which agent tabs may need a card. Drives the union range.
    var windowCardClients: [String] = []

    /// Quota readings for EVERY window each client offers, keyed
    /// `"<clientId>|<cardId>"` — the same identity the card's candidates use.
    ///
    /// `windowCards` above holds only the selected window, which is all the
    /// detail card needs. The Agent-limits sparkline draws one line per row, so
    /// it needs the others too. Each entry is a ~2ms read of an already
    /// persisted file, so filling all of them stays inside the "instant" half.
    var windowCurves: [String: [QuotaSample]] = [:]

    /// Stage 1. Synchronous and local: reads the in-memory payload and the
    /// persisted quota curve. Deliberately NOT async and NOT driven from the
    /// quota poll — the whole point is that it does not wait on the network.
    func refreshWindowQuotaHalves() {
        let now = Int64(Date().timeIntervalSince1970 * 1000)
        // Deliberately NOT `try?`: swallowing the error here is what let a
        // transient generation expiry be reported to the user as "this window
        // has no recorded quota history".
        let readCurve: (String, String?, String, UInt64) throws -> QuotaCurve? = {
            [source] c, account, key, gen in
            try source.quotaCurveSync(
                clientId: c, accountKey: account, windowKey: key, generation: gen)
        }
        if let payload = agentUsage {
            for agent in payload.agents where windowCardClients.contains(agent.clientId) {
                for window in agent.uniqueCardWindows {
                    // A failed read keeps whatever the row already had. Blanking
                    // it would drop a drawn sparkline to a bar for one refresh
                    // and put it back on the next — a flicker that says nothing.
                    guard let samples = WindowCardLoader.curveSamples(
                        payload: payload, clientId: agent.clientId, accountKey: agent.accountKey,
                        window: window, curve: readCurve, nowMs: now)
                    else { continue }
                    windowCurves[WindowCardLoader.curveKey(
                        clientId: agent.clientId, accountKey: agent.accountKey,
                        cardId: window.cardId)] = samples
                }
            }
        }
        for clientId in windowCardClients {
            let state = WindowCardLoader.quotaHalf(
                payload: agentUsage, clientId: clientId,
                attempted: agentUsageAttempted,
                curve: readCurve, nowMs: now)
            // Fold in a usage half we already hold, so a stage-1 refresh does
            // not throw away a completed scan and blink back to loading.
            // The scan's AGE, not just its existence. A covering scan that has
            // aged past `unionScanMaxAge` and whose replacement then threw left
            // this branch winning over the failure branch below, so the card
            // returned to `.ready` with arbitrarily stale totals instead of
            // saying the refresh failed. Freshness is what `.ready` claims.
            if case let .quotaOnly(q, _) = state, let scan = unionScan(for: Self.cardAccountKey),
               Date().timeIntervalSince(scan.capturedAt) < Self.unionScanMaxAge
                   || !windowScanFailed(for: clientId),
               let (settled, usage) = WindowCardLoader.usageHalf(
                   quota: q, scan: scan, confirmed: UsageAttribution.confirmed().records) {
                windowCards[clientId] = .ready(settled, usage)
            } else if case let .quotaOnly(q, _) = state,
                      windowScanFailed(for: clientId) {
                // The loader cannot know this — it never runs the scan. Carried
                // in here so the card can say the scan failed instead of
                // spinning under a chart that is already drawn.
                windowCards[clientId] = .quotaOnly(q, scanFailed: true)
            } else if case .loading = state,
                      let held = windowCards[clientId]?.cardId,
                      held == WindowCardLoader.selectedCardId(
                          payload: agentUsage, clientId: clientId) {
                // A curve read that throws is a transient generation expiry —
                // `quotaHalf` answers `.loading`, which is right as an answer
                // and wrong as a replacement. Overwriting sent a drawn card
                // back to "Waiting for quota…" until a later refresh, which is
                // the flicker the summaries block a few lines down already
                // refuses to produce. Keep what the row had; the next refresh
                // publishes the new state.
                //
                // Only when the held card is about the window now selected:
                // two selections share a client, so retaining by client alone
                // left the chart and totals on the window the user had just
                // navigated away from while the picker highlighted the new one.
                continue
            } else {
                windowCards[clientId] = state
            }
        }

        // Strip summaries cover every displayed client. They read curves only,
        // so the cost is `windowCardClients.count` file reads, not a scan.
        //
        // Skipped entirely when no client is on screen, for the same reason the
        // curve loop above keeps what a failed read already had: `collected`
        // would be empty because there was nothing to collect FROM, and
        // publishing that empty as the answer makes the strip card state "no
        // completed windows recorded yet". `windowCardClients` is assigned from
        // `displayClients`, which arrives with graph data, so it is briefly
        // empty whenever the top-level view changes — which is exactly when the
        // card was seen blanking and coming back.
        // An empty client set has two causes. `displayClients` arrives with
        // graph data, so before that it is empty transiently — publishing then
        // overwrote a populated strip with nothing on every top-level view
        // change. But the user hiding their last visible client is ALSO an
        // empty set, and a permanent one: skipping publication there left the
        // strip and the grid showing a client that is no longer on screen.
        // `stats` is the graph's own arrival signal, so it separates the two.
        if windowCardClients.isEmpty, stats != nil {
            quotaWindowSummaries = []
            quotaHeatmaps = [:]
            quotaHeatmapWindows = []
            qualifyingCycles = [:]
        }
        if let payload = agentUsage, !windowCardClients.isEmpty {
            var collected: [(clientId: String, accountKey: String?, cardId: String, label: String,
                             cycles: [QuotaCycle])] = []
            var heatmaps: [String: QuotaHeatmap] = [:]
            var heatmapWindows: [QuotaHeatmapWindow] = []
            // A THROWN read is a transient generation expiry, which happens
            // when another publication lands while these synchronous reads run.
            // Skipping that window and publishing the rest would drop it from
            // the strip and the heatmap until some later refresh — the same
            // "absent because we could not ask" mistake, arriving through a
            // partial result rather than an empty one. A successful nil is
            // still genuinely no history and still skips.
            var readFailed = false
            for agent in payload.agents where windowCardClients.contains(agent.clientId) {
                for window in agent.uniqueCardWindows {
                    guard let key = window.paceStatus.windowKey,
                          let generation = payload.publicationGeneration
                    else { continue }
                    let attempt: QuotaCurve?
                    do { attempt = try readCurve(agent.clientId, agent.accountKey, key, generation) }
                    catch { readFailed = true; continue }
                    guard let curve = attempt else { continue }
                    let points = curve.points
                    let grid = QuotaHeatmapFold.build(points: points)
                    // See `WindowCardLoader.curveKey`: two accounts of the
                    // same client can offer identically-carded windows, so the
                    // plain "clientId|cardId" key would let one overwrite the
                    // other's grid.
                    heatmaps[WindowCardLoader.curveKey(
                        clientId: agent.clientId, accountKey: agent.accountKey,
                        cardId: window.cardId)] = grid
                    // Unplaced-only windows stay in the picker. `total` is
                    // zero when every reading pair straddles more than six
                    // hours, but the allowance still moved; dropping the window
                    // made the card report "nothing recorded yet" and put the
                    // line that explains it out of reach.
                    if grid.hasMovement {
                        heatmapWindows.append(QuotaHeatmapWindow(
                            clientId: agent.clientId, accountKey: agent.accountKey,
                            cardId: window.cardId,
                            windowLabel: window.label, total: grid.total))
                    }
                    collected.append((
                        clientId: agent.clientId, accountKey: agent.accountKey,
                        cardId: window.cardId,
                        label: window.label,
                        cycles: QuotaHistoryFold.cycles(
                            points: points)))
                }
            }
            // Skip this publication rather than replace a complete set with a
            // partial one. Deliberately not an early `return`: the per-client
            // cycles below are read through a different path and have their own
            // error handling, and suppressing them here would trade one stale
            // surface for another.
            if !readFailed {
                quotaWindowSummaries = QuotaOverviewFold.summaries(windows: collected)
                quotaHeatmaps = heatmaps
                quotaHeatmapWindows = heatmapWindows.sorted { $0.total > $1.total }
                qualifyingCycles = Dictionary(
                    uniqueKeysWithValues: collected.compactMap {
                        window -> (String, QualifyingWindow)? in
                        // Capped BEFORE admitting, which is what the probe
                        // sweep measured and what actually bounds the scan:
                        // capping the admitted count instead would let 32
                        // admitted cycles span a hundred recorded ones.
                        // `collected` keeps the full list for the lifetime
                        // summaries below.
                        let admitted = QuotaHistoryFold.considered(window.cycles).filter {
                            WindowEquivalence.deltaQualifies($0.usedPercent, runs: $0.risingRuns)
                                && $0.observedFraction >= WindowEquivalence.minimumObservedFraction
                        }
                        guard admitted.count >= WindowEquivalence.minimumCycles else { return nil }
                        // Same key as the grids and the row ids — see
                        // `AccountIdentity.windowKey`. Two accounts of one
                        // client offering the same window would otherwise
                        // collide here, and `uniqueKeysWithValues` traps on a
                        // duplicate key rather than reporting it.
                        return (
                            AccountIdentity(
                                clientId: window.clientId, accountKey: window.accountKey
                            ).windowKey(cardId: window.cardId),
                            QualifyingWindow(
                                accountKey: window.accountKey, cycles: admitted)
                        )
                    })
            }
        }

        // Cycles follow the scan's client, not every displayed one: the history
        // is a per-subscription list and only the open tab shows it.
        //
        // The client the published cycles belong to is tracked, because "keep
        // what we had" is only safe while the question has not changed. A
        // transient read failure on B used to leave A's cycles published, and
        // `QuotaView` then drew them under B's name — one subscription's
        // history labelled as another's, which is worse than an empty card and
        // indistinguishable from a correct one.
        if let client = windowUsageClient {
            let selected = WindowCardLoader.selectedCardId(
                payload: agentUsage, clientId: client)
            if let cycles = WindowCardLoader.cycles(
                payload: agentUsage, clientId: client, curve: readCurve)
            {
                quotaCycles = cycles
                quotaCyclesCardId = selected
                quotaCurveUnreadable = false
                rebuildQuotaHistory()
            } else {
                // The read failed. Whether this branch clears or retains is
                // only about a window we already had — on a FIRST failure there
                // is nothing to clear, and the two are indistinguishable in
                // state. What the card needs is the fact that the read failed,
                // which no amount of rearranging here provides: it decides on
                // `cycles.isEmpty` and `attempted`, and an unread curve is
                // empty-and-attempted exactly like a window with no history.
                quotaCurveUnreadable = true
                if quotaCyclesCardId != selected || selected == nil {
                    // Could not read, and what we hold is about a different
                    // window — another client's, or another window of this one.
                    quotaCycles = []
                    quotaHistory = []
                    quotaCyclesCardId = nil
                }
            }
        } else {
            quotaCycles = []
            quotaHistory = []
            quotaCyclesCardId = nil
            quotaCurveUnreadable = false
        }
    }

    /// Joins the cycles to the scan, whenever either changes. Cheap enough to
    /// redo rather than track: the fold is one sorted pass over the scan.
    /// Joins each qualifying window's admitted cycles to the scan.
    ///
    /// Numerators come from each cycle's OBSERVED span, not its whole window:
    /// the delta only describes the interval between two readings, and counting
    /// usage from a stretch nobody sampled against movement nobody saw is the
    /// misalignment that cost 8 points of spread on live data.
    private func rebuildQuotaEquivalences() {
        var built: [String: WindowEquivalence.Row] = [:]
        let confirmed = UsageAttribution.confirmed().records
        for (key, window) in qualifyingCycles {
            let cycles = window.cycles
            // Each row reads ITS OWN account's scan. One shared scan is what
            // issue #258 reports: every row divided every account's usage by
            // its own quota movement, which overstated the primary's estimate
            // by 1.40x and the extra account's by 3.51x on the reporting
            // machine, because the numerator was the same for both.
            guard let scan = unionScan(for: window.accountKey),
                  let client = key.split(separator: "|").first.map(String.init),
                  let oldest = cycles.last, scan.covers(start: oldest.evidenceStartMs)
            else { continue }
            // `declared` carries the empty-declaration case into the fold,
            // which is where the difference between "nothing recorded" and
            // "nothing classified" is defined.
            // The span rule and its numerators come from the same fold the
            // history card uses, sorted once and sliced per cycle. The filter
            // that used to live here re-walked the whole scan per cycle on the
            // main actor.
            let spans = QuotaHistoryFold.spans(
                cycles: cycles, messages: scan.messages, subscription: client,
                // `key` IS the window's `"<clientId>|<cardId>"`, so each
                // estimate is narrowed to its own window's scope rather than to
                // whichever one the card happens to be showing.
                modelScope: WindowCardLoader.modelScope(
                    payload: agentUsage, cardId: key),
                confirmed: confirmed)
            built[key] = WindowEquivalence.aggregate(
                declared: !confirmed.isEmpty,
                cycles: zip(cycles, spans).map { cycle, span in
                    WindowEquivalence.Cycle(
                        deltaPercent: cycle.usedPercent, spanTokens: span.tokens,
                        spanCost: span.cost, observedFraction: cycle.observedFraction,
                        risingRuns: cycle.risingRuns)
                })
        }
        quotaEquivalences = built
    }

    private func rebuildQuotaHistory() {
        guard let client = windowUsageClient,
              let scan = unionScan(for: Self.cardAccountKey),
              let oldest = quotaCycles.last, scan.covers(start: oldest.evidenceStartMs)
        else {
            quotaHistory = []
            return
        }
        quotaHistory = QuotaHistoryFold.rows(
            cycles: quotaCycles, messages: scan.messages,
            // The attribution target for a subscription's own quota is that
            // subscription's client id: the window came from the provider that
            // bills it. A row whose usage was declared elsewhere is exactly
            // what the "other" column exists to show.
            subscription: client,
            // The window these cycles belong to, not the client's default: the
            // user can select a scoped window, and its history has to answer
            // for the same allowance the chart above it draws.
            modelScope: WindowCardLoader.modelScope(
                payload: agentUsage, cardId: quotaCyclesCardId),
            confirmed: UsageAttribution.confirmed().records)
    }

    /// Whose usage bars are actually on screen, or nil on the all-agent
    /// overview, which renders no window card at all.
    ///
    /// Deliberately NOT `windowCardClients`. That set drives the quota curves,
    /// which are a ~2ms file read each and are wanted for every row. This one
    /// drives the message scan, and scoping the two together was a measured
    /// mistake: `copilot chat.v1` is a 31-day window, so unioning every
    /// displayed client stretched the range to 14.93 days and 109,278 messages
    /// — 67s cold — on a tab that displays none of it. The version before the
    /// two-stage split returned early here whenever no agent tab was open;
    /// this restores that bound without giving up the shared scan.
    var windowUsageClient: String?

    /// Recorded reset cycles of `windowUsageClient`'s selected window, newest
    /// first. Derived from the persisted curve alone, so it lands with stage 1.
    private(set) var quotaCycles: [QuotaCycle] = []
    /// Those cycles joined to what was spent in them. Empty until the scan
    /// lands; the card shows the cycles with a loading row in the meantime,
    /// because the quota half is honest on its own and waiting for the usage
    /// half would hide it for seconds.
    private(set) var quotaHistory: [QuotaHistoryRow] = []
    /// Recorded-cycle summaries for EVERY displayed client's windows, for the
    /// all-agent lens. Curve reads only, so unlike `quotaHistory` this needs no
    /// message scan and is safe on the landing view.
    /// Which window's cycles `quotaCycles` currently holds — client AND card,
    /// not client alone. Two window selections share a client, so a client-only
    /// comparison could keep the previous window's history beneath the newly
    /// selected card. Nil when empty.
    @ObservationIgnored private var quotaCyclesCardId: String?

    /// Whether the persisted curve could not be READ, as distinct from having
    /// no history in it.
    ///
    /// `QuotaHistoryCard` decides on `cycles.isEmpty` and `attempted`, and an
    /// unreadable curve is empty-and-attempted exactly like a window that has
    /// simply not accumulated anything — so it stated "No earlier windows
    /// recorded yet" about a curve it had failed to open, and kept stating it
    /// until some later refresh happened to retry. The same distinction the
    /// usage half already draws with `windowScanFailedClients`; the quota half
    /// had no way to say it.
    private(set) var quotaCurveUnreadable = false

    private(set) var quotaWindowSummaries: [QuotaWindowSummary] = []
    /// One weekday-by-hour grid per window, keyed as `QuotaWindowSummary.id`.
    /// Written in the same guarded block as the summaries, so it cannot be
    /// blanked by a refresh that saw no clients either.
    private(set) var quotaHeatmaps: [String: QuotaHeatmap] = [:]
    /// Every window that has a grid, heaviest first. Separate from the
    /// summaries because a window can have movement in its running cycle and no
    /// completed history at all.
    private(set) var quotaHeatmapWindows: [QuotaHeatmapWindow] = []
    /// Per-window quota equivalence, keyed `"<clientId>|<cardId>"`. Only windows
    /// with enough admitted cycles appear. Empty until the scan lands — the
    /// strips above it are free and must not wait for this.
    private(set) var quotaEquivalences: [String: WindowEquivalence.Row] = [:]
    /// Set when the Quota lens is open on the all-agent tab. Gates a scan that
    /// serves one line about one window, measured at 6.3 days and 8.1s, so it
    /// runs only when that line is actually on screen.
    var quotaLensAllAgents = false
    /// Cycles per qualifying window, kept from stage 1 so the scan can be
    /// scoped to exactly what an estimate needs and no further.
    @ObservationIgnored private var qualifyingCycles: [String: QualifyingWindow] = [:]

    /// One window that has enough admitted history to produce an estimate,
    /// plus the account it belongs to.
    ///
    /// The account is carried rather than parsed back out of the dictionary
    /// key. The key is an `AccountIdentity.windowKey`, whose whole point is
    /// that the primary and an extra account spell it with different segment
    /// counts; re-deriving the account from it here would be a second parser
    /// for a fact the builder already had in hand, and the two would drift.
    struct QualifyingWindow {
        let accountKey: String?
        let cycles: [QuotaCycle]
    }

    /// Identifies the newest window scan any model has started.
    ///
    /// Static, because what it guards is static. `PopoverView` builds a new
    /// `DashboardModel` on every reopen, so an instance-local counter gives the
    /// abandoned model and its replacement the same value of 1 and the guard
    /// passes for both — the token's scope has to match `lastUnionScan`'s, or
    /// it protects nothing across a reopen, which is exactly when a scan is
    /// most likely to be abandoned mid-flight.
    ///
    /// `LiveUsageDataSource.windowUsage` hands the blocking FFI call to a
    /// detached task, so cancelling the SwiftUI task that awaits it does not
    /// stop it — the result still arrives. Without this, switching tabs or
    /// lenses mid-scan lets an older, narrower request land after a newer one
    /// and overwrite `unionScan` with a stale message set stamped `capturedAt`
    /// now, which then serves as fresh for the whole 30s window.
    @ObservationIgnored private static var windowScanToken = 0

    /// Stage 2. One scan for the open agent tab, which every card for that
    /// client then filters from in memory.
    func refreshWindowUsage() async {
        Self.windowScanToken &+= 1
        let scanToken = Self.windowScanToken
        let now = Int64(Date().timeIntervalSince1970 * 1000)
        // Two callers, two scopes. An open agent tab needs its own window and
        // its history; the all-agent Quota lens needs only the windows that can
        // actually produce an estimate, which on live data is one of six.
        let equivalenceStart = quotaLensAllAgents
            ? qualifyingCycles.values.compactMap { $0.cycles.last?.evidenceStartMs }.min() : nil
        guard let client = windowUsageClient else {
            guard let from = equivalenceStart else { return }
            // One scan per account with a qualifying window, not one scan for
            // all of them. Each account's estimate divides its own usage by its
            // own quota movement; a shared scan is the conflation issue #258
            // reports. The accounts partition the roots between them, so this
            // is the same files read once each rather than N passes over
            // everything — measured at 1232ms + 424ms against 2130ms for the
            // single wide scan it replaces.
            let accounts = Set(qualifyingCycles.values.map { Self.scanSlot($0.accountKey) })
            var scanned = false
            for slot in accounts {
                let accountKey = slot.isEmpty ? nil : slot
                if let cached = unionScan(for: accountKey), cached.covers(start: from),
                   Date().timeIntervalSince(cached.capturedAt) < Self.unionScanMaxAge
                { continue }
                // Deliberately does NOT touch `windowScanFailedClients`: this
                // branch serves the all-agent equivalence, which draws no window
                // card. The first version set the shared flag here, so an
                // equivalence scan failing on the lens made every client's card
                // claim its own scan had failed the moment the user opened a tab.
                //
                // One account failing does not abandon the others: their rows
                // are independent, and a rate-limited or unreadable account
                // silently removing every other account's estimate is a worse
                // answer than the estimates it could still give.
                guard let usage = try? await source.windowUsage(
                    accountKey: accountKey, from: from, until: now)
                else { continue }
                guard Self.windowScanToken == scanToken else { return }
                setUnionScan(
                    UnionScan(
                        fromMs: from, untilMs: now, capturedAt: Date(),
                        messages: usage.messages, undatedCount: usage.undatedCount),
                    for: accountKey)
                scanned = true
            }
            // Rebuild even when every account was already cached: the caller
            // reached this branch because something wants the estimate now.
            _ = scanned
            rebuildQuotaEquivalences()
            return
        }
        // The history's oldest cycle CONTAINS the active window, so this widens
        // one scan rather than issuing a second. Measured 2026-08-16 on live
        // data: 5.4 days, 45,844 messages, 4.6s — an order of magnitude below
        // the 14.93-day union this replaced, and paid only on the Quota lens.
        //
        // EITHER bound is a range on its own, which is why this is a `min` over
        // whichever exist rather than a window start the cycles may widen. A
        // window resolves only when the provider gave BOTH a reset and a
        // duration, and a provider can stop giving one: Codex served `resetsAt`
        // with no `limit_window_seconds` for all three of its windows (measured
        // 2026-09-08), so `unionStart` returned nil, this function returned
        // before scanning, and no scan covering that client's cycles was ever
        // cached. `rebuildQuotaHistory` then refused to join — correctly, since
        // another client's narrower scan would under-count — and every row of
        // the history card sat on "Reading local usage…" permanently, for a
        // card that needs the cycles and not the live window at all.
        guard let from = [
            WindowCardLoader.unionStart(
                payload: agentUsage, clients: [client], nowMs: now),
            quotaCycles.last?.evidenceStartMs,
        ].compactMap({ $0 }).min() else { return }
        // Serve the cached scan while it still covers the range and is fresh.
        // Rescanning on every reopen was the whole complaint: the staging made
        // the wait visible, it did not make it rare.
        if let cached = unionScan(for: Self.cardAccountKey), cached.covers(start: from),
           Date().timeIntervalSince(cached.capturedAt) < Self.unionScanMaxAge {
            // A fresh scan that covers this window IS an answer for it, whoever
            // ran it. Returning without clearing left the card reporting a
            // failure while the model held the very data that refutes it — and
            // refusing to rescan for another 30 seconds.
            if windowScanFailedClients.contains(client) {
                windowScanFailedClients = Self.scanFailures(
                    windowScanFailedClients, resolvedBy: client)
                refreshWindowQuotaHalves()
            }
            return
        }
        // A throw here is a SETTLED answer, not a slow one. Returning silently
        // left the card in `.quotaOnly` with "Reading local usage…" under a
        // drawn chart for as long as the failure lasted, which is a spinner
        // that will never stop. The token check keeps an overtaken scan's
        // failure from settling a newer request, exactly as its success is.
        guard let usage = try? await source.windowUsage(
            accountKey: Self.cardAccountKey, from: from, until: now)
        else {
            guard Self.windowScanToken == scanToken else { return }
            windowScanFailedClients.insert(client)
            refreshWindowQuotaHalves()
            return
        }
        guard Self.windowScanToken == scanToken else { return }
        // Only THIS client's failure — not because the scan cannot answer for
        // another (it becomes `unionScan` on the next line, and another
        // client's own visit may then find it covering), but because coverage
        // is not tested here. A bare `nil` cleared whichever client was
        // recorded, which is neither of those things.
        windowScanFailedClients = Self.scanFailures(
            windowScanFailedClients, resolvedBy: client)
        setUnionScan(
            UnionScan(
                fromMs: from, untilMs: now, capturedAt: Date(), messages: usage.messages,
                undatedCount: usage.undatedCount),
            for: Self.cardAccountKey)
        refreshWindowQuotaHalves()
        rebuildQuotaHistory()
        rebuildQuotaEquivalences()
    }

    /// The clients whose stage-two scan settled with an error.
    ///
    /// Distinguishes "still scanning" from "asked and failed" — `.quotaOnly`
    /// alone cannot, and both halves of the lens rendered the second as the
    /// first. A per-client answer, not a shared flag: a single boolean says
    /// "did the last scan fail", which is a different question from "did THIS
    /// card's scan fail", and one transient failure on the all-agent lens made
    /// the next tab the user opened claim its own scan had failed before that
    /// scan had started.
    ///
    /// A SET, not one client, for the mirror of the same reason. The
    /// single-slot version scoped its CLEAR to the client that succeeded but
    /// let its SET overwrite whichever client was recorded, so two failures in
    /// a row lost the first and its card went back to "Reading local usage…" —
    /// the never-ending spinner this state exists to remove. Half a rule is not
    /// a rule.
    ///
    /// Cleared at exactly two sites, both in `refreshWindowUsage`: the cached
    /// branch and the success path. Their conditions are deliberately NOT
    /// restated here. Four successive attempts to paraphrase them each read as
    /// precise and each omitted a conjunct — the scan-token check, the
    /// freshness bound, the fact that the cached branch tests coverage against
    /// `from` rather than against the window. Read the two sites; a summary of
    /// a conjunction is a place for one of its terms to go missing.
    ///
    /// One thing worth stating because it is counter-intuitive: `unionScan` is
    /// cross-client in PROVENANCE, not in effect. A scan run for A can clear B,
    /// but only when B itself visits, and only B.
    ///
    /// Deliberately not cleared when a retry STARTS: the card would then flip
    /// failed → loading → failed on every poll, and the last settled answer is
    /// more useful than a spinner that keeps restarting. During a retry the
    /// card still says the last attempt failed, which is true and is the
    /// intended reading.
    private(set) var windowScanFailedClients: Set<String> = []

    /// Whether the card currently shown for `clientId` should say the scan
    /// failed rather than that it is still running.
    func windowScanFailed(for clientId: String) -> Bool {
        windowScanFailedClients.contains(clientId)
    }

    /// The recorded failures after a scan for `client` succeeds.
    ///
    /// Static and pure so the rule can be asserted on: an earlier version
    /// assigned `nil` unconditionally, so a success on one client erased
    /// another's recorded failure while the doc comment claimed the opposite.
    /// Nothing in a view or an async method could catch that.
    nonisolated static func scanFailures(
        _ current: Set<String>, resolvedBy client: String
    ) -> Set<String> {
        current.subtracting([client])
    }

    private func reconcileQuotaRemaining(with payload: AgentUsagePayload) {
        guard source.allowsQuotaCachePersistence else { return }
        let defaults = UserDefaults.standard
        _ = TrayAnimator.applyQuotaRemaining(
            payload: payload,
            persistedSelection: defaults.string(forKey: TrayAnimator.quotaSourceKey)
                ?? QuotaResolver.auto,
            excluding: ClientRegistry.quotaExcludedClients(),
            cachedRemaining: defaults.object(forKey: TrayAnimator.lastRemainingKey) as? Double,
            defaults: defaults)
    }

    /// Poll the OAuth quota snapshots while the popover is open. The fetch is
    /// network-bound (up to ~30s when a provider hangs), so failures keep the
    /// previous payload; per-provider errors live inside each snapshot.
    func pollAgentUsage() async {
        while !Task.isCancelled {
            // Read BEFORE the fetch. The fetch is network-bound and owns most
            // of the cycle, so a registry change lands during it far more often
            // than during the sleep; carrying the epoch across is what stops
            // that change being dropped.
            let registryEpoch = ClaudeExtraRoots.RegistryChange.epoch
            let payload = try? await source.agentUsage()
            if Task.isCancelled { break }
            // Same guard the tray poll carries, for the same reason and in the
            // same shape: a payload built for the previous account set is not
            // an answer to the question this registry asks. `continue` rather
            // than falling through, so the immediate refetch happens now
            // instead of after the sleep — and `agentUsageAttempted` stays
            // where it was, because nothing about the new registry has settled.
            if ClaudeExtraRoots.RegistryChange.epoch != registryEpoch { continue }
            if let payload {
                let resolved = AgentUsagePublicationCoordinator.resolve(payload)
                agentUsage = resolved
                reconcileQuotaRemaining(with: resolved)
                refreshSnapshotLiveData() // keep the reopen cache's quota cards current
                // Stage 1 is synchronous and lands with the payload; stage 2
                // is kicked off without being awaited, so the poll loop never
                // holds the card behind a scan.
                refreshWindowQuotaHalves()
                Task { await refreshWindowUsage() }
            }
            // Set on failure too: `agentUsage == nil` alone cannot distinguish
            // "the first attempt is still in flight" from "the attempt finished
            // and produced nothing", and UI that waits on the payload would spin
            // forever against a persistent failure.
            let firstSettlement = !agentUsageAttempted
            agentUsageAttempted = true
            // And recompute the cards on that first failure, because
            // `refreshWindowQuotaHalves` is the only writer of `windowCards`
            // and it was called from the success branch alone. Teaching
            // `quotaHalf` to answer `.blocked` once the attempt has settled did
            // nothing on its own: with nothing else writing that state, the
            // card kept the `.loading` it was built with until a tab change
            // rebuilt it. Only on the transition, so a run of failures does not
            // redo the same work every minute.
            if payload == nil, firstSettlement { refreshWindowQuotaHalves() }
            // Interruptible: adding or removing an account must take its card
            // with it in the same turn, not at this loop's convenience.
            await ClaudeExtraRoots.RegistryChange.sleep(upTo: 60, since: registryEpoch)
        }
    }

    /// Poll the live tail (10-minute window) — drives the limits card's
    /// "Live" badge now and the trace card in a later phase. The staticlib
    /// re-parses at most every 10s, so this matches its cadence.
    func pollTrace() async {
        while !Task.isCancelled {
            let buckets = try? await source.usageTrace(windowSecs: 600)
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
    /// `clients` is the active tab's slice (displayClients on Overview,
    /// `[clientId]` on a client tab). It is threaded to the FFI so hourly/agents
    /// totals are accurate for hours/agents shared across clients. Refetches
    /// when the slice changes, not only when the report is nil — keyed on the
    /// slice as a Set so a reorder does not refetch. The year stale-guard
    /// mirrors reload()/pollGraph().
    private func ensureHourlyData(
        year: String?, clients: [String]
    ) async {
        let selection = Set(clients)
        let yearKey = Self.identityYear(year)
        guard hourly == nil || hourlyClients != selection || hourlyYear != yearKey else { return }

        // Restore the exact slice before refreshing so reopen and lens switches
        // do not flash a loading state. Non-popover models neither read nor
        // write this process cache.
        let cacheKey = HourlyCacheKey(year: yearKey, clients: selection)
        if cachesSnapshot, let cached = Self.hourlyCache[cacheKey] {
            publishHourly(cached, year: year, clients: selection)
        } else if hourly != nil {
            hourly = nil
            hourlyClients = nil
            hourlyYear = nil
        }
        let requestToken = beginHourlyRequest()
        let report = try? await source.hourlyReport(
            year: year, clients: clients, priority: .userInitiated)
        guard self.year == year, hourlyRequestToken == requestToken else { return }
        guard !Task.isCancelled, let report else { return }
        publishHourly(report, year: year, clients: selection)
    }

    /// Fetch the model report for a lens that needs it, once the graph has
    /// committed. Idempotent per (year, payload generation): a lens switch or a
    /// re-render does not re-request, while a graph refresh that actually moved
    /// the payload does. A failure keeps the last-good report rather than
    /// blanking the card.
    /// True when the committed payload describes the slice `year` currently
    /// selects — the only state in which a model scan is worth issuing. False
    /// on first paint (no payload yet) and mid-year-switch.
    private var modelSliceIsCommitted: Bool {
        payload != nil && acceptedPayloadYear == Self.identityYear(year)
    }

    /// Whether the last graph fetch threw without committing. Set inside the
    /// gated task, so a waiter reading it after the gate opens sees the same
    /// answer the fetch's owner does; cleared by the next commit.
    ///
    /// `tb_model_report` takes a year and nothing else: it always scans current
    /// logs, and the payload generation is a cache identity rather than a
    /// filter. So when the fetch fails, `try?` leaves the RESTORED payload
    /// standing and a scan issued against it puts a reading of now beside a
    /// chart from the previous popover session — hours apart, tagged as if they
    /// agreed. Before the graph/model split this could not happen: `load()`
    /// awaited both, and a throwing graph discarded the model with it.
    ///
    /// This is not the ordinary skew of two independent scans. `tb_graph`
    /// serves a payload up to `ONESHOT_MAX_AGE_SECS` old while the model report
    /// is uncached, so a 30-second lead is normal and bounded; the failure path
    /// is bounded only by how long the graph keeps failing.
    ///
    /// A restored payload with a fetch still IN FLIGHT is not this state — that
    /// one is what waiting on the gate is for.
    @ObservationIgnored private var graphFetchFailed = false
    /// Issued per graph fetch so an obsolete one cannot report failure over a
    /// newer commit. `load()` for year A can still be in flight when the user
    /// picks year B — the existing phantom-slice guards exist for exactly that
    /// overlap — and if B commits first, A's later failure would otherwise mark
    /// the displayed slice failed and strand its model cards on a spinner until
    /// the next successful poll. Release by ownership, not by value: the same
    /// rule `modelRequestToken` follows.
    @ObservationIgnored private var graphFetchToken = 0

    private func ensureModelReport(priority: TaskPriority) async {
        modelWanted = true
        // Raise the flag BEFORE waiting. A restored snapshot is already
        // `.ready`, so the model cards are on screen for the whole wait below —
        // and with the flag down they render "No model usage in this range",
        // reporting a deferred read as an answered one. Only when nothing is
        // displayable: a last-good report stays visible unflagged.
        if modelReport == nil { modelLoading = true }
        // LP3 restore gate. A restored snapshot (memory or disk) without a
        // model report current for it makes `modelSliceIsCommitted` below
        // true from the FIRST body evaluation — payload present, year
        // matching — while `graphLoadTask` is still nil, because `load()`
        // may not even have called `gatedGraph` yet. Without this wait, a
        // model-task-first ordering would fall straight through the guard
        // below and the `while` loop after it (nothing "in flight" to wait
        // on) and scan against a payload nobody has confirmed live. Awaiting
        // it here is a no-op unless `init` installed it — see
        // `restoreGatePending`'s doc comment — and it resolves as soon as
        // ANY graph fetch settles, whether that is `load()`, `reload()`, or
        // `pollGraph()`.
        await waitForRestoreGate()
        // Settle the slice BEFORE waiting on anything. `setYear` moves `year`
        // synchronously while the payload only catches up when reload commits,
        // and PopoverView's task id contains both — so this can be entered with
        // a phantom slice (new year, previous payload). There is nothing to
        // wait for in that state: the answer for the requested slice cannot
        // exist until the reload commits, and apply() re-fires this task the
        // moment it does. Parking behind the gate instead would hold the caller
        // behind the very fetch that invalidates its question — and, since
        // `setYear` installs that gate synchronously, would deadlock a caller
        // that awaits this directly.
        //
        // The report is absent but the answer is not "none" — leaving the flag
        // down here let the cards render their empty copy for the whole graph
        // reload, telling the user a year has no model usage while it was
        // still being read.
        guard modelSliceIsCommitted else {
            modelLoading = true
            return
        }
        // Wait for a graph fetch already running rather than scanning beside
        // it. A restored snapshot hands this task a real key on the first body
        // evaluation, so on every popover reopen it would otherwise start while
        // `load()` is still scanning — putting both back on the bounded pool,
        // which is the contention this separation exists to remove. Read the
        // payload only after, since the fetch may have committed a new one.
        // Follow the chain, not just the task in hand. A fetch can be
        // superseded while this waits, and a superseded one commits nothing —
        // resuming on it would scan against the payload the newer fetch is
        // about to replace, back on the same bounded pool. Terminates because
        // each turn awaits a strictly newer task, and only a fresh
        // `gatedGraph` call can install one.
        while let inFlight = graphLoadTask {
            _ = try? await inFlight.value
            if graphLoadTask == inFlight { break }
        }
        guard let payload, modelSliceIsCommitted, !graphFetchFailed else {
            modelLoading = true
            return
        }
        let year = self.year
        let generation = payload.meta.generatedAt
        let identity = ModelSliceIdentity(year: Self.identityYear(year), generation: generation)
        if modelReport != nil,
           modelYear == identity.year,
           modelPayloadGeneratedAt == generation
        {
            // A report can land from another task while this one waits on the
            // graph gate above, so the flag raised there has to come back down
            // — nothing is in flight to lower it later.
            modelLoading = false
            return
        }
        // Coalesce re-entry onto the request already running for this exact
        // slice. Every entry bumps the request token, so a second one would
        // strand the first — the earlier and likely sooner-finishing scan —
        // and put two full FFI scans on the same bounded pool. The triggers are
        // ordinary interaction: each Daily/Monthly row expand calls
        // `ensureModelColors`, and PopoverView's model task id includes the
        // lens, so Overview→Models mid-scan re-enters too.
        // Adopt the scan already running for this exact slice rather than
        // returning: the caller is a lens-keyed SwiftUI task, and switching
        // lens mid-scan cancels the one that started it. If publication rode
        // that task, the cancelled owner would discard the result and the
        // re-entrant lens would sit empty with nothing left to publish. Awaiting
        // the shared task instead means whoever is still around gets the value.
        if modelInFlight == identity, let running = modelTask {
            await running.value
            return
        }
        modelInFlight = identity
        let requestToken = beginModelRequest()
        modelLoading = true
        // Unstructured, so the fetch and its publication outlive the view task
        // that triggered them. `Task {}` does not inherit cancellation, which is
        // exactly the property needed here; `invalidateModel` cancels it
        // explicitly when the slice it belongs to stops being displayed.
        let task = Task { [source] in
            try? await source.modelReport(year: year, priority: priority)
        }
        modelTask = task
        // Publish from inside the unstructured task, not after awaiting it: the
        // caller is a lens-keyed view task, and a lens switch cancels it while
        // the scan runs. Gating publication on the caller's liveness is what
        // let a cancelled Overview task discard the report the Models lens was
        // waiting for. Slice identity — year plus request token — decides
        // whether the result is still wanted; the caller's fate does not.
        let report = await task.value
        if modelTask == task { modelTask = nil }
        // Release by OWNERSHIP, not by value. `ModelSliceIdentity` is a value,
        // so two different scans can carry an identical one: a year round-trip
        // A→B→A during a scan returns the same payload from `tb_graph`'s 30s
        // cache, hence the same generation. Comparing identities would let the
        // stale scan clear the live scan's slot, and the next ordinary
        // re-entry would start a third scan concurrent with the second. The
        // request token is unique per scan, so only its owner can release it.
        if modelRequestToken == requestToken { modelInFlight = nil }
        // A year switch or a newer request invalidates this completion: publishing
        // it would strand another slice's models on the current dashboard.
        let isCurrent = modelRequestToken == requestToken
        // Only the current request owns the flag; a superseded one must leave it
        // set for the request that replaced it, and must not strand it either.
        if isCurrent { modelLoading = false }
        guard self.year == year, isCurrent else { return }
        guard let report else { return }
        publishModel(report, year: year, generation: generation)
    }

    /// The retry `pollGraph` performs each tick. Factored out so the condition
    /// exists in ONE place: while the test hook below restated it, a mutation
    /// of the poll's own condition changed nothing the suite could observe.
    ///
    /// Gated on `modelWanted` and nothing else. A `modelReport == nil` test
    /// would restate the staleness rule a second time and get it wrong — a
    /// failure that lands while a LAST-GOOD report is displayed keeps that
    /// report, so the retry never fired and the cards sat on the previous
    /// generation's models beside an advancing chart. `ensureModelReport`
    /// already owns that judgement (year plus payload generation) and returns
    /// immediately when the report is current, so a fresh call costs nothing.
    private func retryModelIfStale(priority: TaskPriority) async {
        guard modelWanted else { return }
        await ensureModelReport(priority: priority)
    }

    /// Drives that retry without waiting 60 seconds for the loop.
    func retryMissingModelForTest() async {
        await retryModelIfStale(priority: .utility)
    }

    /// Model-dependent lenses. Separate from `ensureData` because that task is
    /// keyed on the lens/year/client slice only, so folding the model into it
    /// would start the model scan alongside the graph again — the exact
    /// contention this split removes. PopoverView keys this on the committed
    /// payload generation instead.
    ///
    /// PRECONDITION, not merely convention: `load()` must have been called on
    /// this model at least once before this is. Every production caller
    /// satisfies it (`PopoverView` always runs both `.task { load() }` and the
    /// model-data task), but nothing here enforces it — a future caller that
    /// skips `load()` on a model whose restore installed the gate above will
    /// hang forever awaiting a fetch nobody ever starts. This is a liveness
    /// trap, not a safe API to call on its own.
    func ensureModelData(for view: AppView) async {
        switch view {
        case .overview, .models, .stats:
            await ensureModelReport(priority: .userInitiated)
        default:
            break
        }
    }

    /// Daily/Monthly render no model card, so they never fetch the report on
    /// activation — but expanding a row reveals a per-model drill-down whose
    /// dots are tinted by `ModelColorMap`, and that map is built from the model
    /// report. Before the graph/model split `apply()` populated it on every
    /// load; without this hook the drill-down would flatten every model of a
    /// provider onto the same rank-0 shade. Idempotent, so repeated expands
    /// cost nothing once the report has landed.
    ///
    /// PRECONDITION, not merely convention — same as `ensureModelData`: this
    /// reaches the same gate, so calling it on a restored model that never had
    /// `load()` called first is a future-caller liveness trap, not a safe API.
    func ensureModelColors() async {
        await ensureModelReport(priority: .userInitiated)
    }

    func ensureData(for view: AppView, clients: [String]) async {
        let year = self.year
        switch view {
        case .hourly:
            // Hourly keeps the established nil/empty = all clients contract
            // from `ctb.h`. Daily/Monthly used to opt out of it; they no longer
            // reach this call at all, since their turns ride the graph payload.
            await ensureHourlyData(year: year, clients: clients)
        case .agents:
            let selection = Set(clients)
            guard agents == nil || agentsClients != selection else { return }
            // Nil the stale report on a slice change (see the hourly case).
            if agents != nil, agentsClients != selection { agents = nil; agentsClients = selection }
            let report = try? await source.agentsReport(
                year: year, clients: clients, priority: .userInitiated)
            guard self.year == year, !Task.isCancelled else { return }
            agents = report
            agentsClients = selection
        default:
            break
        }
    }

    /// Shared async live-rate helper for PopoverView and SettingsWindowView.
    /// The source remains the only owner of raw usage calls; hidden-client
    /// filtering follows the same policy as the tray and live-session card.
    func tokensPerMin() async -> Double? {
        try? await LiveRate.current(source: source)
    }
}
