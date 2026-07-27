import Foundation
import TokenBarCore

// Swift side of the Swift↔C# fixture cross-check. Feeds identical fixture JSON
// to the shipping TokenBarCore/Format code and writes <basename>.actual.json so
// diff.py can compare it field-by-field against the C# port. See the contract
// in the Windows repo: crosscheck/README.md.
//
// Format.swift is symlinked in from Sources/TokenBar so the harness compiles the
// exact same source the app ships (the README says TokenBarCore/Format.swift;
// it actually lives in the app target). No reimplementation — fidelity is the point.

// --- fail fast on timezone (todayKey/today* bucket in the local zone) ---
guard ProcessInfo.processInfo.environment["TZ"] == "Asia/Taipei" else {
    FileHandle.standardError.write(Data("error: TZ must be Asia/Taipei (got \(ProcessInfo.processInfo.environment["TZ"] ?? "unset"))\n".utf8))
    exit(1)
}

// `Format` is locale-dependent since i18n, so the cross-check contract pins the
// language through the NSUserDefaults argument domain (`-AppleLanguages "(en)"`).
// UserDefaults reads that pair straight from argv and leaves it in
// CommandLine.arguments, where it would shift the positional arguments below.
//
// Strip only that one known option, and reject anything else dash-prefixed.
// Consuming an unrecognized flag's "value" blindly would silently eat a fixture
// path and run the comparison against the wrong directory.
let args: [String] = {
    let raw = CommandLine.arguments
    var kept: [String] = Array(raw.prefix(1))
    var appleLanguages: String?
    var i = 1
    while i < raw.count {
        let token = raw[i]
        guard token.hasPrefix("-"), token.count > 1 else {
            kept.append(token)
            i += 1
            continue
        }
        guard token == "-AppleLanguages", i + 1 < raw.count else {
            FileHandle.standardError.write(Data(
                "error: unsupported option \(token) — only `-AppleLanguages <value>` is recognized, and it requires a value\n".utf8))
            exit(2)
        }
        appleLanguages = raw[i + 1]
        i += 2
    }
    guard appleLanguages == "(en)" else {
        FileHandle.standardError.write(Data(
            "error: language must be pinned with -AppleLanguages \"(en)\"\n".utf8))
        exit(1)
    }
    return kept
}()

// --- fail closed on the language pin (Format's dates/relative times localize) ---
// An unpinned run on a non-English host silently emits values the byte-exact C#
// comparison can never match, which reads as a port defect rather than a harness
// misinvocation. The argument check above is authoritative even when a sibling
// `TokenBar_TokenBar.bundle` is present: TokenBarCore's shared lookup searches
// that bundle, while Bundle.main belongs to this harness executable.
let language = Bundle.main.preferredLocalizations.first ?? "en"
guard language == "en" else {
    FileHandle.standardError.write(Data(
        "error: language must be pinned to en (got \(language)) — pass -AppleLanguages \"(en)\"\n".utf8))
    exit(1)
}

guard (3...4).contains(args.count) else {
    FileHandle.standardError.write(Data("usage: crosscheck-harness <fixtures-dir> <out-dir> [format|usage-pace|provider-quota-pace-v3]\n".utf8))
    exit(2)
}
let fixturesDir = URL(fileURLWithPath: args[1], isDirectory: true)
let outDir = URL(fileURLWithPath: args[2], isDirectory: true)
let selector = args.count == 4 ? args[3] : "all"
try FileManager.default.createDirectory(at: outDir, withIntermediateDirectories: true)

// Harness-side strict RFC3339 parser for `now` (NOT the module under test — the
// module parses window.resetsAt itself, which some fixtures exercise).
func parseNow(_ s: String) -> Date? {
    let plain = ISO8601DateFormatter()
    plain.formatOptions = [.withInternetDateTime]
    if let d = plain.date(from: s) { return d }
    let frac = ISO8601DateFormatter()
    frac.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
    return frac.date(from: s)
}

// Polymorphic scalar `arg`. Int64-first so int64-max survives exactly (a Double
// path would mangle exact-int64-max / compact-int64-max).
enum Arg: Decodable {
    case int(Int64), double(Double), string(String)
    init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if let i = try? c.decode(Int64.self) { self = .int(i) }
        else if let d = try? c.decode(Double.self) { self = .double(d) }
        else { self = .string(try c.decode(String.self)) }
    }
    var asInt64: Int64 { switch self { case .int(let i): return i; case .double(let d): return Int64(d); case .string: return 0 } }
    var asDouble: Double { switch self { case .int(let i): return Double(i); case .double(let d): return d; case .string: return 0 } }
    var asString: String { switch self { case .string(let s): return s; default: return "" } }
}

func stageName(_ s: PaceStage) -> String {
    switch s {
    case .onTrack: return "onTrack"
    case .slightlyAhead: return "slightlyAhead"
    case .ahead: return "ahead"
    case .farAhead: return "farAhead"
    case .slightlyBehind: return "slightlyBehind"
    case .behind: return "behind"
    case .farBehind: return "farBehind"
    }
}

let decoder = JSONDecoder() // same config production uses (plain default)

func write(_ obj: [String: Any], to name: String) throws {
    let data = try JSONSerialization.data(withJSONObject: obj, options: [.prettyPrinted, .sortedKeys])
    try data.write(to: outDir.appendingPathComponent(name))
}

// ---------------- usage-pace.json ----------------
func runUsagePace() throws {
    let data = try Data(contentsOf: fixturesDir.appendingPathComponent("usage-pace.json"))
    guard let root = try JSONSerialization.jsonObject(with: data) as? [String: Any],
          let cases = root["cases"] as? [[String: Any]]
    else {
        throw NSError(domain: "CrossCheckHarness", code: 1, userInfo: [
            NSLocalizedDescriptionKey: "usage-pace.json must contain a cases array"
        ])
    }

    var out: [String: Any] = [:]
    for (index, c) in cases.enumerated() {
        guard let name = c["name"] as? String,
              let kind = c["kind"] as? String,
              let windowObject = c["window"],
              JSONSerialization.isValidJSONObject(windowObject)
        else {
            out["invalid-case-\(index)"] = ["error": "invalid case metadata"]
            continue
        }
        let windowData = try JSONSerialization.data(withJSONObject: windowObject)
        guard let window = try? decoder.decode(UsageWindow.self, from: windowData) else {
            // Preserve the complete legacy baseline run while using the current
            // production decoder: old payloads that are now invalid are explicit
            // intended mismatches until the Windows v3 wire port lands.
            out[name] = ["rejected": true]
            continue
        }

        switch kind {
        case "compute":
            let modeRaw = c["mode"] as? String
            guard let modeRaw, let mode = PaceMode(rawValue: modeRaw) else {
                out[name] = ["error": "unknown pace mode \(modeRaw ?? "nil")"]; continue
            }
            let nowRaw = c["now"] as? String
            guard let nowRaw, let now = parseNow(nowRaw) else {
                out[name] = ["error": "unparseable now \(nowRaw ?? "nil")"]; continue
            }
            guard let pace = UsagePace.compute(window: window, mode: mode, now: now) else {
                out[name] = NSNull(); continue
            }
            out[name] = [
                "stage": stageName(pace.stage),
                "deltaPercent": pace.deltaPercent,
                "expectedUsedPercent": pace.expectedUsedPercent,
                "actualUsedPercent": pace.actualUsedPercent,
                "etaSeconds": pace.etaSeconds.map { $0 as Any } ?? NSNull(),
                "willLastToReset": pace.willLastToReset,
                "label": pace.label,
                "etaText": pace.etaText.map { $0 as Any } ?? NSNull(),
            ]
        case "runOutRisk":
            out[name] = runOutRiskLabel(window: window).map { $0 as Any } ?? NSNull()
        default:
            out[name] = ["error": "unknown kind \(kind)"]
        }
    }
    try write(out, to: "usage-pace.actual.json")
    print("usage-pace.actual.json: \(out.count) cases")
}

// ---------------- format.json ----------------
struct FormatCase: Decodable {
    let name: String
    let fn: String
    let now: String?
    let arg: Arg?
}
struct FormatFile: Decodable {
    let graph: UsagePayload // production DTO, production decoder
    let cases: [FormatCase]
}

func runFormat() throws {
    let data = try Data(contentsOf: fixturesDir.appendingPathComponent("format.json"))
    let file = try decoder.decode(FormatFile.self, from: data)
    var out: [String: Any] = [:]
    for c in file.cases {
        func nowDate() -> Date? { c.now.flatMap(parseNow) }
        switch c.fn {
        case "compactTokens":
            out[c.name] = Format.compactTokens(c.arg?.asInt64 ?? 0)
        case "exactTokens":
            out[c.name] = Format.exactTokens(c.arg?.asInt64 ?? 0)
        case "usd":
            out[c.name] = Format.usd(c.arg?.asDouble ?? 0)
        case "monthDay":
            out[c.name] = Format.monthDay(c.arg?.asString ?? "")
        case "mmdd":
            out[c.name] = Format.mmdd(c.arg?.asString ?? "")
        case "relativeTime":
            guard let now = nowDate() else { out[c.name] = ["error": "relativeTime needs now"]; continue }
            out[c.name] = Format.relativeTime(UInt64(c.arg?.asInt64 ?? 0), now: now)
        case "todayKey":
            guard let now = nowDate() else { out[c.name] = ["error": "todayKey needs now"]; continue }
            out[c.name] = Format.todayKey(now: now)
        case "todayTokens":
            guard let now = nowDate() else { out[c.name] = ["error": "todayTokens needs now"]; continue }
            out[c.name] = file.graph.trayTotals(hidden: [], today: Format.todayKey(now: now)).todayTokens
        case "todayCost":
            guard let now = nowDate() else { out[c.name] = ["error": "todayCost needs now"]; continue }
            out[c.name] = file.graph.trayTotals(hidden: [], today: Format.todayKey(now: now)).todayCost
        case "paceDurationText":
            out[c.name] = UsagePace.durationText(c.arg?.asDouble ?? 0)
        default:
            out[c.name] = ["error": "unknown fn \(c.fn)"]
        }
    }
    try write(out, to: "format.actual.json")
    print("format.actual.json: \(out.count) cases")
}

// ---------------- provider-quota-pace-v3.json ----------------
struct ProviderQuotaPaceCase: Decodable {
    let name: String
    let kind: String
    let clientId: String?
    let cardId: String?
    let mode: String?
    let now: String?
    let selection: String?
    let rawWindow: String?
}

struct ProviderQuotaPaceFile: Decodable {
    let schemaVersion: Int
    let payload: AgentUsagePayload // production DTO, production decoder
    let cases: [ProviderQuotaPaceCase]
}

func paceStateName(_ state: UsagePaceState) -> String {
    state.rawValue
}

func durationSourceName(_ source: UsagePaceDurationSource?) -> Any {
    source?.rawValue ?? NSNull()
}

func basisName(_ basis: UsagePaceBasis) -> String {
    switch basis {
    case .linear: return "linear"
    case .historical: return "historical"
    }
}

func quotaWindow(
    payload: AgentUsagePayload, clientId: String, cardId: String
) -> UsageWindow? {
    payload.agents.first(where: { $0.clientId == clientId })?
        .uniqueCardWindows.first(where: { $0.cardId == cardId })
}

func lifecycleRows(_ payload: AgentUsagePayload) -> [[String: Any]] {
    payload.agents.flatMap { agent in
        agent.windows.map { window in
            [
                "clientId": agent.clientId,
                "cardId": window.cardId,
                "label": window.label,
                "state": paceStateName(window.paceStatus.state),
                "reason": window.paceStatus.reason.map { $0.rawValue as Any } ?? NSNull(),
                "durationSeconds": window.paceStatus.durationSeconds.map { NSNumber(value: $0) } ?? NSNull(),
                "durationSource": durationSourceName(window.paceStatus.durationSource),
                "completeCycles": window.paceStatus.completeCycles,
                "hasHistorical": window.historicalPace != nil,
            ]
        }
    }
}

func paceOutput(
    window: UsageWindow, mode: PaceMode, pace: UsagePace
) -> [String: Any] {
    let presentation = UsagePace.presentation(window: window, mode: mode, pace: pace)
    return [
        "basis": basisName(pace.basis),
        "stage": stageName(pace.stage),
        "deltaPercent": pace.deltaPercent,
        "expectedUsedPercent": pace.expectedUsedPercent,
        "actualUsedPercent": pace.actualUsedPercent,
        "etaSeconds": pace.etaSeconds.map { NSNumber(value: $0) } ?? NSNull(),
        "willLastToReset": pace.willLastToReset,
        "label": pace.label,
        "etaText": presentation.etaText.map { $0 as Any } ?? NSNull(),
        "riskText": presentation.riskText.map { $0 as Any } ?? NSNull(),
        "isHistoricalDeficit": pace.isHistoricalDeficit,
    ]
}

func decodeRawWindow(_ raw: String) throws -> UsageWindow {
    try decoder.decode(UsageWindow.self, from: Data(raw.utf8))
}

func runProviderQuotaPaceV3() throws {
    let data = try Data(contentsOf: fixturesDir.appendingPathComponent("provider-quota-pace-v3.json"))
    let file = try decoder.decode(ProviderQuotaPaceFile.self, from: data)
    guard file.schemaVersion == 3 else {
        throw NSError(domain: "CrossCheckHarness", code: 1, userInfo: [
            NSLocalizedDescriptionKey: "provider-quota-pace-v3 schemaVersion must be 3"
        ])
    }

    var casesOut: [String: Any] = [:]
    for c in file.cases {
        switch c.kind {
        case "pace":
            guard let clientId = c.clientId,
                  let cardId = c.cardId,
                  let modeRaw = c.mode,
                  let mode = PaceMode(rawValue: modeRaw),
                  let nowRaw = c.now,
                  let now = parseNow(nowRaw),
                  let window = quotaWindow(payload: file.payload, clientId: clientId, cardId: cardId)
            else {
                casesOut[c.name] = ["error": "invalid pace case metadata"]
                continue
            }
            guard let pace = UsagePace.compute(window: window, mode: mode, now: now) else {
                casesOut[c.name] = NSNull()
                continue
            }
            casesOut[c.name] = paceOutput(window: window, mode: mode, pace: pace)

        case "selection":
            guard let selection = c.selection else {
                casesOut[c.name] = ["error": "selection case needs selection"]
                continue
            }
            let canonical = QuotaResolver.canonicalSelection(
                payload: file.payload, selection: selection)
            let resolved = QuotaResolver.resolve(payload: file.payload, selection: selection)
            casesOut[c.name] = [
                "selection": selection,
                "canonicalSelection": canonical,
                "resolvedClientId": resolved.map { $0.clientId as Any } ?? NSNull(),
                "resolvedCardId": resolved.map { $0.window.cardId as Any } ?? NSNull(),
            ]

        case "legacy":
            guard let raw = c.rawWindow, let nowRaw = c.now, let now = parseNow(nowRaw) else {
                casesOut[c.name] = ["error": "legacy case needs rawWindow and now"]
                continue
            }
            do {
                let window = try decodeRawWindow(raw)
                casesOut[c.name] = [
                    "rejected": false,
                    "state": paceStateName(window.paceStatus.state),
                    "reason": window.paceStatus.reason.map { $0.rawValue as Any } ?? NSNull(),
                    "durationSeconds": window.paceStatus.durationSeconds.map { NSNumber(value: $0) } ?? NSNull(),
                    "durationSource": durationSourceName(window.paceStatus.durationSource),
                    "completeCycles": window.paceStatus.completeCycles,
                    "windowMinutes": window.windowMinutes.map { NSNumber(value: $0) } ?? NSNull(),
                    "historicalPace": UsagePace.compute(window: window, mode: .historical, now: now) != nil,
                    "linearPace": UsagePace.compute(window: window, mode: .linear, now: now) != nil,
                ]
            } catch {
                casesOut[c.name] = ["rejected": true]
            }

        case "malformed":
            guard let raw = c.rawWindow else {
                casesOut[c.name] = ["error": "malformed case needs rawWindow"]
                continue
            }
            casesOut[c.name] = ["rejected": (try? decodeRawWindow(raw)) == nil]

        default:
            casesOut[c.name] = ["error": "unknown kind \(c.kind)"]
        }
    }

    try write([
        "schemaVersion": file.schemaVersion,
        "lifecycle": lifecycleRows(file.payload),
        "cases": casesOut,
    ], to: "provider-quota-pace-v3.actual.json")
    print("provider-quota-pace-v3.actual.json: \(casesOut.count) cases")
}

switch selector {
case "all":
    try runUsagePace()
    try runFormat()
case "usage-pace":
    try runUsagePace()
case "format":
    try runFormat()
case "provider-quota-pace-v3":
    try runProviderQuotaPaceV3()
default:
    FileHandle.standardError.write(Data("error: unknown selector \(selector)\n".utf8))
    exit(2)
}
