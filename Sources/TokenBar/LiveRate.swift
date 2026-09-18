import Foundation
import TokenBarCore

/// Shared rate policy for every UI surface. Hidden clients are removed from
/// the live trace when necessary; otherwise the injected source's raw rate is
/// used unchanged.
enum LiveRate {
    /// The rate to display, with hidden clients excluded.
    ///
    /// Takes the source's own figure whenever nothing is hidden, which is both
    /// the common case and the cheap one: `tokensPerMin()` is a single value,
    /// while excluding a client means pulling a 10-minute trace and re-summing
    /// it here, because a rate that has already been aggregated cannot have a
    /// client subtracted from it afterwards.
    ///
    /// The hidden set comes from `hiddenTabClients()`, so hiding a grouped tab
    /// removes every client under it rather than only the id that was stored.
    static func current(source: any UsageDataSource) async throws -> Double {
        let hidden = ClientRegistry.hiddenTabClients()
        guard !hidden.isEmpty else { return try await source.tokensPerMin() }
        let rows = try await source.usageTrace(windowSecs: 600)
        return TraceBucket.totalRate(rows, hidden: hidden)
    }
}
