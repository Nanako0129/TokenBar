import Foundation
import Observation
import TokenBarCore

@MainActor @Observable final class AttributedSeriesModel {
    private(set) var points: [AttributedDailySeries.Point]?

    /// Folds the previous open's rows at construction, so the card draws on the
    /// FIRST frame of a reopen instead of after one.
    ///
    /// `load()` has always republished `lastRows` before awaiting anything, and
    /// measurement on a real install showed it doing exactly that — 147 rows,
    /// `republish=true`, on every reopen. The card still flashed its empty
    /// state, because `load()` runs from a `.task` and a `.task` does not start
    /// until the view has been laid out once. `points` was therefore nil for
    /// that first frame every time, and nil is what `PopoverView` maps into the
    /// card's absent trend. The data was never missing; it arrived one frame
    /// after it was drawn.
    ///
    /// This does NOT cache the finished points, which the note on `lastRows`
    /// rejects and still rejects: rows are folded here against the declarations
    /// as they are RIGHT NOW, exactly as `load()` folds them, so a
    /// classification the user changed while the popover was closed is applied
    /// rather than replayed. The only thing that moved is when the fold runs.
    ///
    /// Gated on the same provenance `load()` gates its republish on. A zone
    /// that does not match what the cache was acquired under makes every day
    /// key wrong, and `cacheTrustLost` means a transition is pending; in either
    /// case there is nothing honest to draw yet and the card waits, as before.
    init(timeZone: String = TimeZone.current.identifier) {
        guard !Self.cacheTrustLost, Self.acquiredTimeZone == timeZone,
              let rows = Self.lastRows
        else { return }
        contributions = rows
        points = AttributedDailySeries.points(
            contributions: rows,
            // The same single reader `load`'s caller uses, not a second parse
            // of the same defaults key. One statement of what "declared" means.
            confirmed: UsageAttribution.confirmed().records)
    }

    /// The timezone whatever is in the graph cache was produced under, scoped
    /// to the process rather than to one model. Everything it tracks is
    /// process-scoped: the staticlib's graph cache is a process-lifetime static,
    /// and AppDelegate's title-refresh loop warms the all-time entry long before
    /// any popover opens. This model, meanwhile, is mounted the way
    /// DashboardModel is — `@State` in a PopoverView that StatusItemController
    /// tears down and rebuilds on every open/close — so an instance-scoped
    /// baseline would be nil again on each open and the rule below would never
    /// fire.
    ///
    /// nil means nobody recorded it, so the cache's provenance is unknown and
    /// the next acquisition recomputes rather than trusting it.
    @ObservationIgnored private static var acquiredTimeZone: String?

    /// Bumped on every timezone transition, so an acquisition that suspended
    /// across one can tell the provenance it started from is gone.
    @ObservationIgnored private static var timeZoneGeneration = 0
    @ObservationIgnored private static var timeZoneObserver: NSObjectProtocol?
    @ObservationIgnored private static var didCaptureLaunch = false

    /// Seed provenance at launch. The cache is empty with the process, so the
    /// launch zone is what any entry in it can have been produced under.
    ///
    /// This buys only the cheap path: without it provenance starts unknown and
    /// the first load recomputes — correct, at the price of one full
    /// re-aggregation. Once-only, because a later call would re-assert a
    /// provenance that may since have been invalidated.
    static func captureLaunchTimeZone(_ id: String = TimeZone.current.identifier) {
        guard !didCaptureLaunch else { return }
        didCaptureLaunch = true
        acquiredTimeZone = id
        installTimeZoneObserver()
    }

    /// Forget provenance on every timezone transition.
    ///
    /// Comparing identifiers alone is not enough, because the marker only ever
    /// records QD-1's own loads while AppDelegate's refresh loop and
    /// DashboardModel write the same shared cache. Travel from A to B and back
    /// to A and the marker still reads A while the cache holds B's day buckets,
    /// so a load would accept them and re-stamp them as A. Forgetting means QD-1
    /// never claims a provenance it did not observe, and an unknown one
    /// recomputes.
    ///
    /// Installed from `load` as well as from launch, so correctness never
    /// depends on the launch hook being wired up.
    private static func installTimeZoneObserver() {
        guard timeZoneObserver == nil else { return }
        timeZoneObserver = NotificationCenter.default.addObserver(
            forName: .NSSystemTimeZoneDidChange, object: nil, queue: .main
        ) { _ in
            MainActor.assumeIsolated { invalidateTimeZoneProvenance() }
        }
    }

    /// The transition path itself. Internal so a self-test can drive it without
    /// depending on when NotificationCenter chooses to deliver.
    static func invalidateTimeZoneProvenance() {
        acquiredTimeZone = nil
        lastRows = nil
        timeZoneGeneration &+= 1
        cacheTrustLost = true
    }

    /// Set by the first transition and never cleared for the process lifetime.
    ///
    /// `graph_compute` (`tb_core_ffi/src/lib.rs:319-333`) does its work outside
    /// the cache lock and then inserts unconditionally, so a computation begun
    /// before a transition can land after one begun after it and leave the
    /// entry holding the old zone's day buckets. Recomputing once cannot fix
    /// that: the straggler may still be in flight. Once any transition has been
    /// seen, stop reading the shared entry at all and consume what
    /// `refreshGraph` itself computed. The entry can still be poisoned for other
    /// consumers — repairing that is the cache's own problem, not this one.
    @ObservationIgnored private static var cacheTrustLost = false

    static var timeZoneGenerationForTesting: Int { timeZoneGeneration }

    /// Independent starting state for a self-test case, including tearing the
    /// observer down — otherwise one case's observer keeps working in the next
    /// and a case cannot tell which code path installed it.
    static func resetForTesting() {
        if let timeZoneObserver { NotificationCenter.default.removeObserver(timeZoneObserver) }
        timeZoneObserver = nil
        acquiredTimeZone = nil
        timeZoneGeneration = 0
        didCaptureLaunch = false
        cacheTrustLost = false
        lastRows = nil
    }

    /// Retained so a failed fetch can re-derive with current declarations
    /// instead of publishing the classification the user just changed away from.
    @ObservationIgnored private var contributions: [Contribution]?

    /// The last rows any instance published under verified provenance.
    ///
    /// Process-scoped for the reason `acquiredTimeZone` is: this model is
    /// rebuilt on every popover open, so an instance-scoped copy is nil again
    /// each time and the card sits on its spinner for the length of a full
    /// acquisition — every open, with nothing on screen, while the graph cache
    /// upstairs already holds the answer.
    ///
    /// Rows, not points: the split depends on declarations the user can change
    /// while the popover is closed, and re-folding them is one pass. Caching
    /// the finished points would republish a classification they moved away
    /// from.
    @ObservationIgnored private static var lastRows: [Contribution]?

    /// Drop the reopen cache. Called with the other scan-derived caches when
    /// the extra-scan-root registry is replaced — these rows come from the
    /// graph, the graph comes from the roots, and `load` republishes them
    /// immediately on the next open before awaiting the fresh one. Without
    /// this, the subscription trend showed a removed root's usage, or omitted
    /// an added one, for as long as the replacement scan took.
    @MainActor
    static func invalidateRowCache() {
        lastRows = nil
        // Supersede any load already in flight. Clearing the cache alone loses
        // to a suspended one: it snapshotted its inputs before the roots
        // changed, and the FFI hands back its pre-change result even when the
        // engine's own generation check refuses to cache it — so the old task
        // resumes and republishes exactly what was just cleared. Advancing the
        // token makes its `guard Self.loadToken == token` drop it, which is the
        // same shape `ROOT_GENERATION` uses on the Rust side.
        loadToken &+= 1
    }

    /// Identifies the newest load this model has started.
    ///
    /// `load` is `@MainActor` but suspends, so a second one — a declaration
    /// change, or SwiftUI restarting the task — runs its own body in the gaps.
    /// Whichever acquisition returns first publishes, and the other then resumes
    /// and overwrites it with an older payload and the `confirmed` set it
    /// captured. Cancellation does not prevent this: `LiveUsageDataSource` hands
    /// the blocking FFI call to a detached task, so its result arrives regardless.
    /// Static for the same reason `lastRows` is: two popover instances can
    /// overlap, and an instance-local counter gives each of them a token of 1,
    /// so both pass their own check and the older one's result can overwrite
    /// the newer rows in the shared cache. The next reopen then publishes those
    /// stale rows immediately.
    @ObservationIgnored private static var loadToken = 0

    func load(
        source: any UsageDataSource,
        confirmed: [UsageAttribution.Record],
        timeZone: String = TimeZone.current.identifier
    ) async {
        Self.installTimeZoneObserver()
        Self.loadToken &+= 1
        let token = Self.loadToken
        // A timezone change invalidates every day key, and the graph cache will
        // not notice: past its 30s window an unchanged source token makes it
        // re-stamp and serve the same entry indefinitely (tb_core_ffi/src/lib.rs
        // :285-313). Only refreshGraph recomputes. This is the sole condition
        // that forces one.
        let generation = Self.timeZoneGeneration
        let shouldRefresh = Self.cacheTrustLost || Self.acquiredTimeZone != timeZone

        // Publish the previous open's rows first, folded against the CURRENT
        // declarations, so the card draws this frame instead of after the scan.
        // Gated on provenance: same zone, no transition pending, or the day
        // keys are not known to be datable and nothing may be shown.
        //
        // Deliberately NOT gated on `points == nil`. That guard read as "only
        // when there is nothing on screen", but the second thing that restarts
        // this load is a declaration change — `PopoverView`'s task is keyed on
        // the attribution string — and then `points` holds the split the user
        // just moved away from while the rows needed to correct it are already
        // in hand. The guard therefore skipped the one case where the stale
        // frame is the user's own edit, and left it up for the length of a full
        // acquisition. Re-folding is one pass over rows already held.
        //
        // Still `lastRows` rather than this instance's `contributions`:
        // `invalidateRowCache` clears the former and not the latter, so reading
        // the instance copy would republish a removed scan root's usage — the
        // exact thing that function exists to prevent.
        if !shouldRefresh, let rows = Self.lastRows {
            contributions = rows
            points = AttributedDailySeries.points(
                contributions: rows, confirmed: confirmed)
        }
        let payload: UsagePayload
        do {
            payload = try await (shouldRefresh
                ? source.refreshGraph(year: nil, priority: .userInitiated)
                : source.graph(year: nil, priority: .userInitiated))
        } catch {
            guard Self.loadToken == token else { return }
            // Never keep publishing a series built from inputs that have moved
            // on. Classification can always be brought current from rows we
            // already hold; day keys cannot, so a failed recompute after a
            // timezone change drops the series rather than showing buckets
            // already known to be misdated.
            //
            // `shouldRefresh` was decided before the acquisition and cannot see
            // a transition that landed during it, so the generation is what
            // actually answers whether the retained rows are still datable.
            guard !shouldRefresh, Self.timeZoneGeneration == generation else {
                points = nil
                contributions = nil
                Self.lastRows = nil
                return
            }
            points = contributions.map {
                AttributedDailySeries.points(contributions: $0, confirmed: confirmed)
            }
            return
        }

        guard Self.loadToken == token else { return }

        // A transition landed while this acquisition was suspended, so nothing
        // it returned is known to have been built under the current zone.
        // Publish nothing rather than day buckets of unverified provenance, keep
        // no rows to re-derive from, and leave provenance invalidated so the next
        // load recomputes. Stamping here would be worse than stale output: it
        // would overwrite the invalidation with a zone nobody verified, and no
        // later load could correct that.
        guard Self.timeZoneGeneration == generation else {
            points = nil
            contributions = nil
            Self.lastRows = nil
            return
        }

        contributions = payload.contributions
        points = AttributedDailySeries.points(
            contributions: payload.contributions,
            confirmed: confirmed)
        Self.lastRows = payload.contributions
        Self.acquiredTimeZone = timeZone
    }
}
