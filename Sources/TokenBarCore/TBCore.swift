import CTB
import Foundation
import os

/// FFI-boundary log. Backend failures and contract drift would otherwise vanish
/// — every app-side caller wraps these in `try?` to keep the last good numbers,
/// so the only record of a failure is here. Logs the entry-point type and the
/// error string only; never a decoded payload body (agent usage carries emails).
let ffiLog = Logger(subsystem: "com.nyanako.tokenbar", category: "ffi")

/// Errors crossing the Rust FFI boundary.
public enum TBCoreError: Error {
    case nullPointer
    case bridge(String)
}

/// Result of the `tb_probe` smoke entry point.
public struct ProbeResult: Decodable {
    public let ok: Bool
    public let messages: Int?
    public let err: String?
}

/// Standard envelope every non-probe entry point returns:
/// `{"ok":true,"data":<payload>}` or `{"ok":false,"err":"..."}`.
struct TBEnvelope<T: Decodable>: Decodable {
    let ok: Bool
    let data: T?
    let err: String?
}

/// Thin Swift facade over the tb_core_ffi staticlib. All calls are blocking;
/// invoke from a background thread/actor in app code. `agentUsage()` is also
/// network-bound.
public enum TBCore {
    /// Copy the FFI string out of the heap buffer and free it, so decoding never
    /// races the C allocation. Returns nil for a NULL pointer. This is the single
    /// legal consumer of a tb_* return pointer — `tb_free` happens here exactly
    /// once, on every path (the `defer`), which is what keeps the boundary free
    /// of leaks and double-frees.
    private static func takeBytes(_ raw: UnsafeMutablePointer<CChar>?) -> Data? {
        guard let raw else { return nil }
        defer { tb_free(raw) }
        return Data(bytes: raw, count: strlen(raw))
    }

    /// Decode a bare JSON payload returned by a tb_* entry point, then free it.
    /// (Used by the legacy `tb_probe` shape; enveloped entry points use `unwrap`.)
    static func decode<T: Decodable>(_ raw: UnsafeMutablePointer<CChar>?) throws -> T {
        guard let data = takeBytes(raw) else {
            ffiLog.error("FFI returned NULL for \(String(describing: T.self), privacy: .public)")
            throw TBCoreError.nullPointer
        }
        do {
            return try JSONDecoder().decode(T.self, from: data)
        } catch {
            ffiLog.error(
                "FFI decode \(String(describing: T.self), privacy: .public) failed: \(String(describing: error), privacy: .public)")
            throw error
        }
    }

    /// Decode an enveloped payload, surfacing `{"ok":false}` as a thrown error.
    static func unwrap<T: Decodable>(_ raw: UnsafeMutablePointer<CChar>?) throws -> T {
        guard let data = takeBytes(raw) else {
            ffiLog.error("FFI returned NULL for \(String(describing: T.self), privacy: .public)")
            throw TBCoreError.nullPointer
        }
        do {
            return try decodeEnvelope(data)
        } catch {
            ffiLog.error(
                "FFI \(String(describing: T.self), privacy: .public) failed: \(String(describing: error), privacy: .public)")
            throw error
        }
    }

    /// Pure envelope decode: `{"ok":true,"data":..}` → payload, `{"ok":false}` →
    /// thrown `TBCoreError.bridge`. Split out from the pointer/free path so the
    /// error contract is unit-testable (`envelopeContractChecks`) without a real
    /// FFI allocation — feeding a synthetic pointer to `decode` would be unsound,
    /// since `tb_free` must only ever release a Rust-allocated pointer.
    static func decodeEnvelope<T: Decodable>(_ data: Data) throws -> T {
        let envelope = try JSONDecoder().decode(TBEnvelope<T>.self, from: data)
        guard envelope.ok, let payload = envelope.data else {
            throw TBCoreError.bridge(envelope.err ?? "unknown")
        }
        return payload
    }

    /// Pass an optional year filter across the boundary (nil = all time).
    private static func withYear<R>(
        _ year: String?, _ body: (UnsafePointer<CChar>?) -> R
    ) -> R {
        guard let year else { return body(nil) }
        return year.withCString { body($0) }
    }

    public static func probe() throws -> ProbeResult {
        let result: ProbeResult = try decode(tb_probe())
        if !result.ok { throw TBCoreError.bridge(result.err ?? "unknown") }
        return result
    }

    /// Contribution graph for `year` (nil = all time). Served from a <=30s
    /// cache inside the staticlib when warm.
    public static func graph(year: String? = nil) throws -> UsagePayload {
        try unwrap(withYear(year) { tb_graph($0) })
    }

    /// Contribution graph, always recomputed.
    public static func refreshGraph(year: String? = nil) throws -> UsagePayload {
        try unwrap(withYear(year) { tb_refresh_graph($0) })
    }

    public static func modelReport(year: String? = nil) throws -> ModelReport {
        try unwrap(withYear(year) { tb_model_report($0) })
    }

    public static func hourlyReport(year: String? = nil) throws -> HourlyReport {
        try unwrap(withYear(year) { tb_hourly_report($0) })
    }

    public static func agentsReport(year: String? = nil) throws -> AgentsReport {
        try unwrap(withYear(year) { tb_agents_report($0) })
    }

    /// Live trace buckets over the trailing `windowSecs`.
    public static func usageTrace(windowSecs: Int64) throws -> [TraceBucket] {
        try unwrap(tb_usage_trace(windowSecs))
    }

    /// Live tokens/min estimate (10-minute-window average).
    public static func tokensPerMin() throws -> Double {
        let payload: TokensPerMin = try unwrap(tb_tokens_per_min())
        return payload.tokensPerMin
    }

    /// OAuth quota cards for codex/claude/antigravity/copilot. Network-bound;
    /// per-provider failures are reported in each snapshot's `error`.
    public static func agentUsage() throws -> AgentUsagePayload {
        try unwrap(tb_agent_usage())
    }

    /// Hermetic checks for the FFI envelope/error contract, surfaced to the
    /// `--selftest` runner (which lives in the TokenBar module and can't reach
    /// these internal symbols). Exercises the error paths `--smoke` never hits on
    /// live data: an `{"ok":false}` must throw `bridge`, a malformed body must
    /// throw rather than crash. Returns `(label, passed)` pairs.
    public static func envelopeContractChecks() -> [(String, Bool)] {
        var out: [(String, Bool)] = []
        func check(_ label: String, _ passed: Bool) { out.append((label, passed)) }

        // ok:true + data → payload returned verbatim.
        do {
            let ok: TokensPerMin = try decodeEnvelope(
                Data(#"{"ok":true,"data":{"tokensPerMin":42.5}}"#.utf8))
            check("ok:true returns data", ok.tokensPerMin == 42.5)
        } catch {
            check("ok:true returns data", false)
        }

        // ok:false → TBCoreError.bridge carrying the err string.
        do {
            let _: TokensPerMin = try decodeEnvelope(Data(#"{"ok":false,"err":"boom"}"#.utf8))
            check("ok:false throws bridge(boom)", false)
        } catch let TBCoreError.bridge(msg) {
            check("ok:false throws bridge(boom)", msg == "boom")
        } catch {
            check("ok:false throws bridge(boom)", false)
        }

        // ok:true but data absent → bridge (contract violation, not a crash).
        do {
            let _: TokensPerMin = try decodeEnvelope(Data(#"{"ok":true}"#.utf8))
            check("ok:true without data throws", false)
        } catch {
            check("ok:true without data throws", true)
        }

        // Malformed JSON → thrown DecodingError, never a trap.
        do {
            let _: TokensPerMin = try decodeEnvelope(Data(#"{not json"#.utf8))
            check("malformed body throws", false)
        } catch {
            check("malformed body throws", true)
        }

        return out
    }
}
