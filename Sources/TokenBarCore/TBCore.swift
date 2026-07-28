import CTB
import Foundation
import os

/// FFI-boundary log. Backend failures and contract drift would otherwise vanish
/// — every app-side caller wraps these in `try?` to keep the last good numbers.
/// Agent usage is the sensitive exception: it logs only bounded transport fields
/// plus fixed outer-failure classifications, never backend or decoding text.
let ffiLog = Logger(subsystem: "com.nyanako.tokenbar", category: "ffi")

/// Errors crossing the Rust FFI boundary.
public enum TBCoreError: Error {
    case nullPointer
    case bridge(String)
}

package enum AgentUsageBoundaryLogEvent: Equatable, Sendable {
    case returnedNull
    case bridgeFailed
    case decodeFailed
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
    /// `TBCoreError.bridge`, and a successful envelope without data → decoding
    /// contract failure. Split out from the pointer/free path so the
    /// error contract is unit-testable (`envelopeContractChecks`) without a real
    /// FFI allocation — feeding a synthetic pointer to `decode` would be unsound,
    /// since `tb_free` must only ever release a Rust-allocated pointer.
    static func decodeEnvelope<T: Decodable>(_ data: Data) throws -> T {
        let envelope = try JSONDecoder().decode(TBEnvelope<T>.self, from: data)
        guard envelope.ok else {
            throw TBCoreError.bridge(envelope.err ?? "unknown")
        }
        guard let payload = envelope.data else {
            throw DecodingError.dataCorrupted(.init(
                codingPath: [],
                debugDescription: "successful FFI envelope is missing data"))
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

    /// Pass an optional year filter and optional client filter across the
    /// boundary. nil year = all time; nil OR empty clients = all clients (the
    /// FFI treats a NULL/empty client arg as "every client"). Client ids are
    /// comma-joined. NOTE: an empty selection therefore reaches the core as
    /// "all clients", not "no clients" — the all-hidden case is enforced by the
    /// lens views' strict membership filter, not here.
    private static func withYearAndClients<R>(
        _ year: String?, _ clients: [String]?,
        _ body: (UnsafePointer<CChar>?, UnsafePointer<CChar>?) -> R
    ) -> R {
        let joined = (clients?.isEmpty ?? true) ? nil : clients!.joined(separator: ",")
        return withYear(year) { yearPtr in
            guard let joined else { return body(yearPtr, nil) }
            return joined.withCString { body(yearPtr, $0) }
        }
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

    /// Per-hour report for `year` (nil = all time), restricted to `clients`
    /// (nil/empty = all clients). The core filters at the streaming scan, so a
    /// client slice yields accurate per-client totals for hours shared across
    /// clients (a downstream membership filter cannot — buckets fold all
    /// clients into one mixed total).
    public static func hourlyReport(year: String? = nil, clients: [String]? = nil) throws -> HourlyReport {
        try unwrap(withYearAndClients(year, clients) { tb_hourly_report($0, $1) })
    }

    /// Per-agent report for `year` (nil = all time), restricted to `clients`
    /// (nil/empty = all clients). Scan-level filter, same rationale as
    /// `hourlyReport`.
    public static func agentsReport(year: String? = nil, clients: [String]? = nil) throws -> AgentsReport {
        try unwrap(withYearAndClients(year, clients) { tb_agents_report($0, $1) })
    }

    /// Source-generation-aware parity diagnostic for the hourly and Agents
    /// filter. Rust derives the client list from a fresh graph and brackets all
    /// report calls with one opaque local-source token sequence.
    public static func filterParityProbe() throws -> FilterParityProbe {
        try unwrap(tb_filter_parity_probe())
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

    /// OAuth quota cards for codex/claude/antigravity/copilot/grok. Network-bound;
    /// per-provider failures are reported in each snapshot's `error`.
    public static func agentUsage() throws -> AgentUsagePayload {
        let payload = try decodeAgentUsageBoundary(takeBytes(tb_agent_usage())) {
            logAgentUsageBoundaryEvent($0)
        }
        logAgentUsageTransportDiagnostics(payload)
        return payload
    }

    package static func decodeAgentUsageBoundary(
        _ data: Data?,
        onLog: (AgentUsageBoundaryLogEvent) -> Void
    ) throws -> AgentUsagePayload {
        guard let data else {
            onLog(.returnedNull)
            throw TBCoreError.nullPointer
        }
        do {
            return try decodeEnvelope(data)
        } catch let error as TBCoreError {
            onLog(.bridgeFailed)
            throw error
        } catch {
            onLog(.decodeFailed)
            throw error
        }
    }

    private static func logAgentUsageBoundaryEvent(_ event: AgentUsageBoundaryLogEvent) {
        switch event {
        case .returnedNull:
            ffiLog.error("FFI agent usage returned NULL")
        case .bridgeFailed:
            ffiLog.error("FFI agent usage bridge failed")
        case .decodeFailed:
            ffiLog.error("FFI agent usage decode failed")
        }
    }

    private static func logAgentUsageTransportDiagnostics(_ payload: AgentUsagePayload) {
        for entry in agentUsageTransportLogEntries(payload) {
            switch (entry.status, entry.osCode) {
            case let (status?, osCode?):
                ffiLog.error(
                    "Agent usage transport failure client=\(entry.clientId, privacy: .public) category=\(entry.category, privacy: .public) status=\(status, privacy: .public) osCode=\(osCode, privacy: .public)")
            case let (status?, nil):
                ffiLog.error(
                    "Agent usage transport failure client=\(entry.clientId, privacy: .public) category=\(entry.category, privacy: .public) status=\(status, privacy: .public)")
            case let (nil, osCode?):
                ffiLog.error(
                    "Agent usage transport failure client=\(entry.clientId, privacy: .public) category=\(entry.category, privacy: .public) osCode=\(osCode, privacy: .public)")
            case (nil, nil):
                ffiLog.error(
                    "Agent usage transport failure client=\(entry.clientId, privacy: .public) category=\(entry.category, privacy: .public)")
            }
        }
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

        // ok:true but data absent → decoding contract failure, not bridge failure.
        do {
            let _: TokensPerMin = try decodeEnvelope(Data(#"{"ok":true}"#.utf8))
            check("ok:true without data is decode failure", false)
        } catch is DecodingError {
            check("ok:true without data is decode failure", true)
        } catch {
            check("ok:true without data is decode failure", false)
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

    /// Hermetic decoder and formatting checks for the source-aware filter
    /// parity payload. This keeps the Swift side independent of private local
    /// session data while pinning the lower-camel wire statuses and bounded
    /// smoke labels.
    public static func filterParityContractChecks() -> [(String, Bool)] {
        var out: [(String, Bool)] = []
        func check(_ label: String, _ passed: Bool) { out.append((label, passed)) }

        let body = Data(
            #"{"ok":true,"data":{"hourly":{"status":"match","unfiltered":{"entryCount":1,"input":10,"output":20,"cacheRead":3,"cacheWrite":4,"reasoning":5,"totalTokens":42,"messageCount":2,"totalCost":0.25},"full":{"entryCount":1,"input":10,"output":20,"cacheRead":3,"cacheWrite":4,"reasoning":5,"totalTokens":42,"messageCount":2,"totalCost":0.25},"delta":{"entryCount":0,"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0,"totalTokens":0,"messageCount":0,"totalCost":0}},"agents":{"status":"sourceChanged","unfiltered":null,"full":null,"delta":null},"presentClientCount":2}}"#.utf8
        )
        do {
            let payload: FilterParityProbe = try decodeEnvelope(body)
            check(
                "filter parity lower-camel statuses decode",
                payload.hourly.status == .match && payload.agents.status == .sourceChanged
                    && payload.presentClientCount == 2
            )
            check(
                "filter parity bounded smoke labels",
                payload.smokeSummary
                    == "hourly=MATCH entriesΔ=0 tokensΔ=0 messagesΔ=0 costΔ=0.00; agents=SOURCE_CHANGED / SKIP"
            )
            check(
                "filter parity nullable aggregates decode",
                payload.agents.unfiltered == nil && payload.agents.full == nil
                    && payload.agents.delta == nil
            )
        } catch {
            check("filter parity lower-camel statuses decode", false)
            check("filter parity bounded smoke labels", false)
            check("filter parity nullable aggregates decode", false)
        }

        return out
    }
}
