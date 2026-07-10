import Foundation
import TokenBarCore

// Logic checks for the pure TokenBarCore ports, run via `TokenBar --selftest`.
// Plain assertions instead of swift-testing/XCTest because the dev machine has
// Command Line Tools only (no testing modules); CI runs this the same way.

enum SelfTest {
    static func run() -> Never {
        var failures = 0
        func expect(_ condition: @autoclosure () -> Bool, _ label: String) {
            if condition() {
                print("ok   \(label)")
            } else {
                failures += 1
                print("FAIL \(label)")
            }
        }

        // ModelColors: provider inference + shade math.
        expect(ModelColors.providerFromModel("claude-sonnet-4-6") == "anthropic", "provider claude")
        expect(ModelColors.providerFromModel("gpt-5.5") == "openai", "provider gpt")
        expect(ModelColors.providerFromModel("o3-mini") == "openai", "provider o3")
        expect(ModelColors.providerFromModel("gemini-3-pro") == "google", "provider gemini")
        expect(ModelColors.providerFromModel("auto") == "cursor", "provider cursor auto")
        expect(ModelColors.providerFromModel("mystery") == "unknown", "provider unknown")
        expect(ModelColors.providerColorKey("litellm, openai", "gpt-5.5") == "openai", "merged provider id")
        expect(ModelColors.providerColorKey("Anthropic", "whatever") == "anthropic", "provider id alias")
        expect(ModelColors.shadeFromBase("#da7756", rank: 0) == "#da7756", "shade rank 0 is base")
        // rank 1 factor 0.11: 59→81 (0x51), 130→144 (0x90), 246→247 (0xf7)
        expect(ModelColors.shadeFromBase("#3b82f6", rank: 1) == "#5190f7", "shade rank 1 lerp")

        // ModelColorMap: cost ranking drives shades; unseen models fall back.
        let map = ModelColorMap(entries: [
            ("anthropic", "claude-opus-4-8", 100.0),
            ("anthropic", "claude-haiku-4-5", 1.0),
        ])
        expect(map.color("anthropic", "claude-opus-4-8") == "#da7756", "priciest model gets base shade")
        expect(map.color("anthropic", "claude-haiku-4-5") != "#da7756", "cheaper model is tinted")
        expect(map.color(nil, "gemini-3-pro") == "#06b6d4", "unseen model falls back to provider base")

        // ISODay: civil-date round trip.
        expect(ISODay("1970-01-01")?.number == 0, "epoch day number")
        expect(ISODay("2026-06-10")?.iso == "2026-06-10", "iso round trip")
        expect(ISODay("garbage") == nil, "invalid iso rejected")

        // Streaks: longest run vs current run touching the range end.
        func perDay(_ dates: [String]) -> [String: PerDay] {
            Dictionary(uniqueKeysWithValues: dates.map {
                ($0, PerDay(date: $0, tokens: 10, cost: 1, intensity: 1))
            })
        }
        let s1 = Streaks.compute(
            perDayMap: perDay(["2026-06-01", "2026-06-02", "2026-06-03", "2026-06-05", "2026-06-06"]),
            rangeStart: "2026-06-01", rangeEnd: "2026-06-06")
        expect(s1.longest == 3 && s1.current == 2, "streaks longest 3 current 2")
        let s2 = Streaks.compute(
            perDayMap: perDay(["2026-06-01"]),
            rangeStart: "2026-06-01", rangeEnd: "2026-06-03")
        expect(s2.longest == 1 && s2.current == 0, "broken current streak is zero")
        let s3 = Streaks.compute(perDayMap: [:], rangeStart: "2026-06-10", rangeEnd: "2026-06-01")
        expect(s3.longest == 0 && s3.current == 0, "inverted range is empty")

        // UsagePace: expected-vs-actual classification, ETA projection, modes.
        // Fixture: 60-minute window, 30 minutes elapsed (linear expected 50%).
        let now = Date(timeIntervalSince1970: 1_750_000_000)
        func window(
            used: Double, minutes: Int64 = 60, untilReset: TimeInterval = 1800,
            historical: Double? = nil, runOut: Double? = nil
        ) -> UsageWindow {
            UsageWindow(
                label: "Session", usedPercent: used, remainingPercent: 100 - used,
                resetsAt: ISO8601DateFormatter().string(from: now.addingTimeInterval(untilReset)),
                windowMinutes: minutes, historicalExpectedPercent: historical,
                runOutProbability: runOut)
        }
        let onPace = UsagePace.compute(window: window(used: 50), now: now)
        expect(onPace?.stage == .onTrack && onPace?.label == "On pace", "pace on track at 50%/50%")
        let ahead = UsagePace.compute(window: window(used: 80), now: now)
        expect(ahead?.stage == .farAhead && ahead?.label == "30% in deficit", "pace far ahead label")
        // 80% in 30min → 100% in 37.5min, before the 30min reset → ETA 7.5min.
        expect(ahead?.willLastToReset == false && abs((ahead?.etaSeconds ?? 0) - 450) < 1, "pace eta 450s")
        expect(ahead?.etaText == "Projected empty in 8m", "pace eta text")
        let reserve = UsagePace.compute(window: window(used: 40), now: now)
        expect(reserve?.stage == .behind && reserve?.label == "10% in reserve", "pace reserve label")
        expect(reserve?.willLastToReset == true && reserve?.etaText == "Lasts until reset", "slow burn lasts")
        expect(UsagePace.compute(window: window(used: 50, minutes: 0), now: now) == nil, "no window length, no pace")
        expect(UsagePace.compute(window: window(used: 50, untilReset: -10), now: now) == nil, "past reset, no pace")
        // Modes: off → nil; historical override replaces expected; run-out
        // probability drives the lasts/empty projection in historical mode.
        expect(UsagePace.compute(window: window(used: 50), mode: .off, now: now) == nil, "pace mode off")
        let hist = UsagePace.compute(
            window: window(used: 50, historical: 80, runOut: 0.2), mode: .historical, now: now)
        expect(hist?.expectedUsedPercent == 80 && hist?.stage == .farBehind, "historical expected override")
        expect(hist?.willLastToReset == true, "low run-out risk lasts to reset")
        let risky = UsagePace.compute(
            window: window(used: 90, historical: 50, runOut: 0.8), mode: .historical, now: now)
        expect(risky?.willLastToReset == false && risky?.etaSeconds != nil, "high run-out risk projects empty")
        let linear = UsagePace.compute(
            window: window(used: 50, historical: 80), mode: .linear, now: now)
        expect(linear?.expectedUsedPercent == 50, "linear mode ignores historical")
        expect(runOutRiskLabel(window: window(used: 50, runOut: 0.3)) == "≈ 30% run-out risk", "run-out risk label")
        expect(runOutRiskLabel(window: window(used: 50)) == nil, "no probability, no risk label")
        expect(UsagePace.durationText(130 * 60) == "2h 10m", "duration text h m")
        expect(UsagePace.durationText(26 * 3600) == "1d 2h", "duration text d h")

        // Contribution grid: GitHub layout, col 0 row 0 = Sunday on/before
        // Jan 1; out-of-year cells are never active; max tracks active only.
        expect(ISODay("1970-01-01")?.weekday == 4, "epoch day is Thursday")
        expect(ISODay("2026-06-07")?.weekday == 0, "2026-06-07 is Sunday")
        let grid = buildGrid(
            year: "2026",
            perDayMap: [
                "2026-01-01": PerDay(date: "2026-01-01", tokens: 500, cost: 1, intensity: 1),
                "2025-12-29": PerDay(date: "2025-12-29", tokens: 900, cost: 1, intensity: 1),
            ])
        expect(grid.rows == 7 && grid.cols >= 53 && grid.cells.count == grid.cols * 7, "grid shape")
        expect(grid.cells.first?.date == "2025-12-28" && grid.cells.first?.inYear == false, "grid starts on the prior Sunday")
        let jan1 = grid.cells.first { $0.date == "2026-01-01" }
        expect(jan1?.col == 0 && jan1?.row == 4 && jan1?.active == true, "jan 1 lands on Thursday row")
        expect(grid.maxTokens == 500, "out-of-year tokens don't drive max")
        expect(grid.cells.first { $0.date == "2025-12-29" }?.active == false, "out-of-year cell inactive")

        // Trace collapse: one row per client, agents/models joined sorted,
        // "unknown" dropped when named models exist, rows sorted by tokens.
        func bucket(_ client: String, _ agent: String, _ model: String, _ tokens: Int64) -> TraceBucket {
            TraceBucket(
                client: client, agent: agent, model: model, tokens: tokens,
                messages: 1, tokensPerMin: Double(tokens))
        }
        let collapsed = TraceBucket.collapseByClient([
            bucket("claude-code", "Main", "claude-opus-4-8", 100),
            bucket("claude-code", "Subagent", "unknown", 50),
            bucket("codex-cli", "Main", "gpt-5.5", 400),
        ])
        expect(collapsed.count == 2 && collapsed[0].client == "codex-cli", "collapse groups and sorts by tokens")
        expect(collapsed[1].tokens == 150 && collapsed[1].tokensPerMin == 150, "collapse sums tokens and rate")
        expect(collapsed[1].agent == "Main, Subagent", "collapse joins agents sorted")
        expect(collapsed[1].model == "claude-opus-4-8", "collapse drops unknown among named models")
        expect(
            TraceBucket.collapseByClient([bucket("amp", "Main", "unknown", 5)]).first?.model == "unknown",
            "collapse keeps a lone unknown model")

        // Live-rate total with hidden clients excluded (issue #35). Bucket
        // tokens_per_min == tokens here (see `bucket`), so sums are exact. The
        // rows carry RAW tail ids (claude-code/codex-cli); the hidden set holds
        // CANONICAL short ids (claude/codex) — totalRate normalizes each row
        // before the membership test, so hiding "claude" must drop claude-code.
        let rateRows = [
            bucket("claude-code", "Main", "claude-opus-4-8", 100),
            bucket("claude-code", "Subagent", "unknown", 50),
            bucket("codex-cli", "Main", "gpt-5.5", 400),
        ]
        expect(TraceBucket.totalRate(rateRows, hidden: []) == 550, "rate empty-hidden is the plain sum")
        expect(TraceBucket.totalRate(rateRows, hidden: ["codex"]) == 150, "rate hiding canonical codex drops codex-cli rows")
        expect(TraceBucket.totalRate(rateRows, hidden: ["claude"]) == 400, "rate hiding canonical claude drops claude-code rows")
        expect(TraceBucket.totalRate(rateRows, hidden: ["claude", "codex"]) == 0, "rate all-hidden is zero")

        // Trace id canonicalization (issue #36): raw tail ids fold to the
        // registry's short ids via EXPLICIT aliases only — a mixed set drops
        // only the hidden client, and already-canonical ids pass through. There
        // is deliberately NO generic "-cli" strip: `antigravity-cli` is a
        // registered client distinct from the `antigravity` IDE, so stripping
        // would conflate them.
        let mixedRows = [
            bucket("claude-code", "Main", "m", 100),
            bucket("codex-cli", "Main", "m", 50),
            bucket("cursor", "Main", "m", 30),
        ]
        expect(TraceBucket.totalRate(mixedRows, hidden: ["claude"]) == 80, "canonical hide drops only claude-code rows")
        expect(ClientRegistry.canonicalClient("gemini-cli") == "gemini", "canonical explicit gemini-cli")
        expect(ClientRegistry.canonicalClient("antigravity-cli") == "antigravity-cli", "canonical preserves registered antigravity-cli")
        expect(ClientRegistry.canonicalClient("droid-cli") == "droid-cli", "canonical does NOT strip a generic -cli")
        expect(ClientRegistry.canonicalClient("claude") == "claude", "canonical short id passes through")
        // AgentLimitsCard keeps its own generic "-cli" fold for quota-card
        // attribution: explicit aliases via the registry, then a local strip so
        // antigravity-cli shares the antigravity quota snapshot — this fold must
        // NOT leak into the deny-filter canonicalizer above.
        expect(AgentLimitsCard.normalizeTraceClient("codex-cli") == "codex", "limits wrapper applies explicit alias")
        expect(AgentLimitsCard.normalizeTraceClient("antigravity-cli") == "antigravity", "limits wrapper folds generic -cli for quota attribution")

        // Quota resolver: auto picks the tightest window across agents,
        // erroring agents are skipped, explicit selections parse. The payload
        // builds via JSON (the snapshot types have no memberwise inits).
        let quotaJSON = """
        {"generatedAt":"now","agents":[
          {"clientId":"codex","source":"oauth","updatedAt":"now",
           "windows":[{"label":"Session","usedPercent":20,"remainingPercent":80},
                      {"label":"Weekly","usedPercent":65,"remainingPercent":35}]},
          {"clientId":"claude","source":"oauth","updatedAt":"now",
           "windows":[{"label":"Session","usedPercent":88,"remainingPercent":12},
                      {"label":"Weekly","usedPercent":10,"remainingPercent":90}]},
          {"clientId":"broken","source":"oauth","updatedAt":"now",
           "windows":[{"label":"Session","usedPercent":99,"remainingPercent":1}],
           "error":"401"}
        ]}
        """
        let quotaPayload = try! JSONDecoder().decode(
            AgentUsagePayload.self, from: Data(quotaJSON.utf8))
        let tightest = QuotaResolver.resolve(payload: quotaPayload, selection: "auto")
        expect(
            tightest?.clientId == "claude" && tightest?.window.label == "Session",
            "auto resolves the tightest healthy window")
        expect(
            QuotaResolver.resolve(payload: quotaPayload, selection: "codex|Weekly")?
                .window.remainingPercent == 35,
            "explicit quota selection resolves")
        expect(
            QuotaResolver.resolve(payload: quotaPayload, selection: "nope|Session") == nil,
            "unknown quota selection is nil")
        expect(QuotaResolver.resolve(payload: nil, selection: "auto") == nil, "no payload, no quota")

        // Limits-card drag reorder: direction-aware insert (down → after the
        // target, up → before it) so single-step moves both work.
        let order = ["a", "b", "c", "d"]
        expect(AgentLimitsCard.reorder(order, from: "a", to: "b") == ["b", "a", "c", "d"], "reorder one step down")
        expect(AgentLimitsCard.reorder(order, from: "d", to: "c") == ["a", "b", "d", "c"], "reorder one step up")
        expect(AgentLimitsCard.reorder(order, from: "a", to: "d") == ["b", "c", "d", "a"], "reorder to the end")
        expect(AgentLimitsCard.reorder(order, from: "d", to: "a") == ["d", "a", "b", "c"], "reorder to the front")
        expect(AgentLimitsCard.reorder(order, from: "a", to: "a") == order, "reorder onto itself is a no-op")
        expect(AgentLimitsCard.reorder(order, from: "x", to: "b") == order, "reorder unknown id is a no-op")

        // mergeReorder: dragging within a visible SUBSET must not drop the
        // off-screen ids from the shared tab-order key. Non-visible ids keep
        // their exact slots; the visible slots refill in the new order.
        expect(
            ClientRegistry.mergeReorder(
                full: ["g", "a", "c", "x"], visible: ["c", "x"], from: "x", to: "c")
                == ["g", "a", "x", "c"],
            "mergeReorder keeps non-visible ids in place")
        // A visible id not yet in the saved order appends at the end.
        expect(
            ClientRegistry.mergeReorder(
                full: ["a"], visible: ["a", "z"], from: "a", to: "a")
                == ["a", "z"],
            "mergeReorder appends visible ids absent from full")
        // A no-op drag leaves the full order untouched.
        expect(
            ClientRegistry.mergeReorder(
                full: ["a", "b", "c"], visible: ["a", "b", "c"], from: "a", to: "a")
                == ["a", "b", "c"],
            "mergeReorder no-op leaves full order unchanged")
        // Empty saved order → just the reordered visible sequence.
        expect(
            ClientRegistry.mergeReorder(
                full: [], visible: ["a", "b"], from: "a", to: "b")
                == ["b", "a"],
            "mergeReorder with empty full writes the visible sequence")

        // knownLimitsClients (the hoisted universe): present clients with a
        // known limit, unioned with quota-snapshot holders (dedup, ordered).
        expect(
            ClientRegistry.knownLimitsClients(
                present: ["cursor", "claude"], quotaIds: ["antigravity"],
                placeholders: ["codex", "claude", "gemini"])
                == ["claude", "antigravity"],
            "knownLimitsClients drops no-limit present ids, keeps quota-only ids")

        // CSV id-set parse helper: empty string → empty set; commas split.
        expect(ClientRegistry.parseIdSet("").isEmpty, "parseIdSet empty string is empty")
        expect(
            ClientRegistry.parseIdSet("a,b,a") == Set(["a", "b"]),
            "parseIdSet splits and dedups")

        // Tray totals with hidden clients excluded (issue #35). Fixture: two
        // days, two clients (claude/codex), "today" = 2026-07-01. Client stripe
        // tokens = input+output+cacheRead+cacheWrite+reasoning.
        //   today  claude 150 tok $1.5 · codex 200 tok $2.0  (day totals 350/$3.5)
        //   06-01  claude 300 tok $3.0 · codex 400 tok $4.0  (day totals 700/$7.0)
        //   summary 1050 tok / $10.5
        let trayJSON = """
        {"meta":{"generatedAt":"now","version":"1","dateRange":{"start":"2026-06-01","end":"2026-07-01"}},
         "summary":{"totalTokens":1050,"totalCost":10.5,"totalDays":2,"activeDays":2,
                    "averagePerDay":5.25,"maxCostInSingleDay":7.0,"clients":["claude","codex"],"models":[]},
         "years":[],
         "contributions":[
           {"date":"2026-06-01","totals":{"tokens":700,"cost":7.0,"messages":2},"intensity":2,
            "tokenBreakdown":{"input":700,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0},
            "clients":[
              {"client":"claude","modelId":"m","providerId":"p","cost":3.0,"messages":1,
               "tokens":{"input":300,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0}},
              {"client":"codex","modelId":"m","providerId":"p","cost":4.0,"messages":1,
               "tokens":{"input":400,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0}}]},
           {"date":"2026-07-01","totals":{"tokens":350,"cost":3.5,"messages":2},"intensity":1,
            "tokenBreakdown":{"input":300,"output":50,"cacheRead":0,"cacheWrite":0,"reasoning":0},
            "clients":[
              {"client":"claude","modelId":"m","providerId":"p","cost":1.5,"messages":1,
               "tokens":{"input":100,"output":50,"cacheRead":0,"cacheWrite":0,"reasoning":0}},
              {"client":"codex","modelId":"m","providerId":"p","cost":2.0,"messages":1,
               "tokens":{"input":200,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0}}]}
         ]}
        """
        let trayPayload = try! JSONDecoder().decode(UsagePayload.self, from: Data(trayJSON.utf8))
        let today = "2026-07-01"
        // (a) Empty hidden set == unfiltered totals (byte-identical fast path).
        let unfiltered = trayPayload.trayTotals(hidden: [], today: today)
        expect(unfiltered.totalTokens == trayPayload.summary.totalTokens
            && unfiltered.totalCost == trayPayload.summary.totalCost,
            "tray empty-hidden totals equal summary")
        expect(unfiltered.todayTokens == 350 && unfiltered.todayCost == 3.5,
            "tray empty-hidden today equals contribution totals")
        // (b) Hiding one client subtracts exactly that client's stripes.
        let noCodex = trayPayload.trayTotals(hidden: ["codex"], today: today)
        expect(noCodex.totalTokens == unfiltered.totalTokens - 600
            && noCodex.totalCost == unfiltered.totalCost - 6.0,
            "tray hiding a client drops its total stripes")
        expect(noCodex.todayTokens == unfiltered.todayTokens - 200
            && noCodex.todayCost == unfiltered.todayCost - 2.0,
            "tray hiding a client drops its today stripes")
        // (c) All clients hidden -> zeros.
        let allHidden = trayPayload.trayTotals(hidden: ["claude", "codex"], today: today)
        expect(allHidden.totalTokens == 0 && allHidden.totalCost == 0
            && allHidden.todayTokens == 0 && allHidden.todayCost == 0,
            "tray all-hidden totals are zero")
        // Empty selection zeros the stats aggregate too (issue #36 Fix 2): the
        // lens views now filter strictly, so an all-hidden slice (clientIds=[])
        // shows nothing everywhere instead of leaking through an empty-allowlist
        // "show all" — consistent with DayBars/UsageStats' strict membership.
        let emptyStats = UsageStats(payload: trayPayload, selectedClients: [])
        expect(emptyStats.totalTokens == 0 && emptyStats.totalCost == 0 && emptyStats.activeDays == 0,
            "empty selection zeros the stats aggregate")

        // Saturating token folds (issue #36 Fix 4): corrupt Antigravity lanes
        // can be Int64.max-clamped by the Rust side; the Swift re-sums must
        // saturate, not trap, and stay byte-identical for normal values.
        expect(Int64.max.saturatingAdding(Int64.max) == .max, "saturating add clamps at Int64.max")
        expect(Int64.max.saturatingAdding(1) == .max, "saturating add caps a small overflow")
        expect(Int64.min.saturatingAdding(-1) == .min, "saturating add clamps at Int64.min")
        expect((100 as Int64).saturatingAdding(50) == 150, "saturating add is exact without overflow")
        let maxLanes = try! JSONDecoder().decode(
            TokenBreakdown.self,
            from: Data(#"{"input":9223372036854775807,"output":9223372036854775807,"cacheRead":0,"cacheWrite":0,"reasoning":0}"#.utf8))
        expect(maxLanes.total == .max, "TokenBreakdown.total saturates two Int64.max lanes")
        let normalLanes = try! JSONDecoder().decode(
            TokenBreakdown.self,
            from: Data(#"{"input":100,"output":50,"cacheRead":10,"cacheWrite":5,"reasoning":2}"#.utf8))
        expect(normalLanes.total == 167, "TokenBreakdown.total is exact for normal lanes")

        // FFI envelope/error contract (hermetic; no FFI allocation or live data).
        for (label, passed) in TBCore.envelopeContractChecks() {
            expect(passed, "envelope: \(label)")
        }

        if failures > 0 {
            print("\(failures) selftest check(s) failed")
            exit(1)
        }
        print("selftest passed")
        exit(0)
    }
}
