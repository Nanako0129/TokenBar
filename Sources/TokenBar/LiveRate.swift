import Foundation
import TokenBarCore

/// Shared rate policy for every UI surface. Hidden clients are removed from
/// the live trace when necessary; otherwise the injected source's raw rate is
/// used unchanged.
enum LiveRate {
    static func current(source: any UsageDataSource) async throws -> Double {
        let hidden = ClientRegistry.hiddenClients()
        guard !hidden.isEmpty else { return try await source.tokensPerMin() }
        let rows = try await source.usageTrace(windowSecs: 600)
        return TraceBucket.totalRate(rows, hidden: hidden)
    }
}
