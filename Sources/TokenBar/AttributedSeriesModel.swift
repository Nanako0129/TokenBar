import Foundation
import Observation
import TokenBarCore

@MainActor @Observable final class AttributedSeriesModel {
    private(set) var points: [AttributedDailySeries.Point]?

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
    }

    /// Retained so a failed fetch can re-derive with current declarations
    /// instead of publishing the classification the user just changed away from.
    @ObservationIgnored private var contributions: [Contribution]?

    func load(
        source: any UsageDataSource,
        confirmed: [UsageAttribution.Record],
        timeZone: String = TimeZone.current.identifier
    ) async {
        Self.installTimeZoneObserver()
        // A timezone change invalidates every day key, and the graph cache will
        // not notice: past its 30s window an unchanged source token makes it
        // re-stamp and serve the same entry indefinitely (tb_core_ffi/src/lib.rs
        // :285-313). Only refreshGraph recomputes. This is the sole condition
        // that forces one.
        let generation = Self.timeZoneGeneration
        let shouldRefresh = Self.cacheTrustLost || Self.acquiredTimeZone != timeZone
        let payload: UsagePayload
        do {
            payload = try await (shouldRefresh
                ? source.refreshGraph(year: nil, priority: .userInitiated)
                : source.graph(year: nil, priority: .userInitiated))
        } catch {
            // Never keep publishing a series built from inputs that have moved
            // on. Classification can always be brought current from rows we
            // already hold; day keys cannot, so a failed recompute after a
            // timezone change drops the series rather than showing buckets
            // already known to be misdated.
            points = shouldRefresh ? nil : contributions.map {
                AttributedDailySeries.points(contributions: $0, confirmed: confirmed)
            }
            return
        }

        // A transition landed while this acquisition was suspended, so nothing
        // it returned is known to have been built under the current zone.
        // Publish nothing rather than day buckets of unverified provenance, keep
        // no rows to re-derive from, and leave provenance invalidated so the next
        // load recomputes. Stamping here would be worse than stale output: it
        // would overwrite the invalidation with a zone nobody verified, and no
        // later load could correct that.
        guard Self.timeZoneGeneration == generation else {
            points = nil
            return
        }

        contributions = payload.contributions
        points = AttributedDailySeries.points(
            contributions: payload.contributions,
            confirmed: confirmed)
        Self.acquiredTimeZone = timeZone
    }
}
