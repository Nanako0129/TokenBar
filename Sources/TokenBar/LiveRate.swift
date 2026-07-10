import Foundation
import TokenBarCore

/// Live tokens/min for menu-bar display and tray-animation speed, with the
/// user's hidden clients excluded (issue #35). A hidden client's live activity
/// must not drive the "X/m" tray mode, the popover rate badge, or the cat/parrot
/// spin speed — consistent with UsageTraceCard's filtered total rate.
///
/// Runs blocking FFI; call off the main actor like every other TBCore call.
enum LiveRate {
    /// The single source of truth for the displayed live rate. With no hidden
    /// clients this is the raw FFI rate unchanged (byte-identical regression
    /// guard). Otherwise it re-derives the rate from the 600s usage-trace rows
    /// (the same 10-minute window `tb_tokens_per_min` uses) filtered to the
    /// non-hidden clients — summing those rows equals `rate_in_window(600)` for
    /// the surviving clients.
    static func current() throws -> Double {
        let hidden = ClientRegistry.hiddenClients()
        guard !hidden.isEmpty else { return try TBCore.tokensPerMin() }
        let rows = try TBCore.usageTrace(windowSecs: 600)
        return TraceBucket.totalRate(rows, hidden: hidden)
    }
}
