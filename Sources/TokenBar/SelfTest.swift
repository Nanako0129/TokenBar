import Foundation
import TokenBarCore

// Logic checks for the pure TokenBarCore ports, run via `TokenBar --selftest`.
// Plain assertions instead of swift-testing/XCTest because the dev machine has
// Command Line Tools only (no testing modules); CI runs this the same way.

private final class AsyncResultBox<Value: Sendable>: @unchecked Sendable {
    var result: Result<Value, Error>?
}

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

        func awaitValue<Value: Sendable>(
            _ operation: @escaping @Sendable () async throws -> Value
        ) -> Value? {
            let semaphore = DispatchSemaphore(value: 0)
            let box = AsyncResultBox<Value>()
            Task.detached(priority: .userInitiated) {
                defer { semaphore.signal() }
                do {
                    box.result = .success(try await operation())
                } catch {
                    box.result = .failure(error)
                }
            }
            semaphore.wait()
            return try? box.result?.get()
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

        // UsagePace: explicit v3 state fixtures, exact duration timing, and
        // mode/basis policy. No pace assertion uses the legacy constructor.
        let now = Date(timeIntervalSince1970: 1_750_000_000)
        func v3Window(
            used: Double,
            durationSeconds: Int64 = 3_600,
            untilReset: TimeInterval = 1_800,
            state: UsagePaceState = .learningHistory,
            historicalPace: HistoricalPace? = nil,
            windowMinutes: Int64? = nil
        ) -> UsageWindow {
            let duration: Int64? = state == .learningDuration || state == .unavailable
                ? nil : durationSeconds
            let durationSource: UsagePaceDurationSource? = duration == nil
                ? (state == .learningDuration ? .observed : nil) : .contract
            let status = PaceStatus(
                state: state, windowKey: "session.v3", durationSeconds: duration,
                durationSource: durationSource,
                completeCycles: state == .available ? 5 : 0,
                reason: state == .unavailable ? .nonRecurring : nil)
            return UsageWindow(
                label: "Session", usedPercent: used, remainingPercent: 100 - used,
                resetsAt: ISO8601DateFormatter().string(from: now.addingTimeInterval(untilReset)),
                windowMinutes: windowMinutes ?? duration.map { $0 / 60 },
                historicalPace: historicalPace,
                cardId: "session.v3", durationSeconds: duration, paceStatus: status)
        }

        let onPace = UsagePace.compute(window: v3Window(used: 50), now: now)
        expect(onPace?.stage == .onTrack && onPace?.basis == .linear
            && onPace?.label == "On pace", "pace on track at 50%/50%")
        let ahead = UsagePace.compute(window: v3Window(used: 80), now: now)
        expect(ahead?.stage == .farAhead && ahead?.label == "30% in deficit"
            && ahead?.basis == .linear, "pace far ahead label")
        // 80% in 30min → 100% in 37.5min, before the 30min reset → ETA 7.5min.
        expect(ahead?.willLastToReset == false && abs((ahead?.etaSeconds ?? 0) - 450) < 1, "pace eta 450s")
        expect(ahead?.etaText == "Projected empty in 8m", "pace eta text")
        let reserve = UsagePace.compute(window: v3Window(used: 40), now: now)
        expect(reserve?.stage == .behind && reserve?.label == "10% in reserve", "pace reserve label")
        expect(reserve?.willLastToReset == true && reserve?.etaText == "Lasts until reset", "slow burn lasts")
        let learningDurationWindow = v3Window(used: 50, state: .learningDuration)
        expect(UsagePace.compute(window: learningDurationWindow, now: now) == nil,
            "learning duration has no pace")
        expect(UsagePace.compute(
            window: learningDurationWindow, mode: .historical, now: now) == nil,
            "historical learningDuration has no pace")
        expect(UsagePace.compute(
            window: learningDurationWindow, mode: .linear, now: now) == nil,
            "linear learningDuration has no pace")
        expect(UsagePace.compute(window: v3Window(used: 50, untilReset: -10), now: now) == nil,
            "past reset, no pace")
        expect(UsagePace.compute(window: v3Window(used: 50, untilReset: 3_600), now: now) == nil,
            "elapsed-zero positive usage has no pace")

        // A non-minute duration proves timing uses exact v3 seconds rather than
        // the compatibility windowMinutes field.
        let exactDuration = UsagePace.compute(
            window: v3Window(used: 50, durationSeconds: 3_601, untilReset: 1_800), now: now)
        let exactExpected = (Double(3_601 - 1_800) / Double(3_601)) * 100
        expect(exactDuration?.expectedUsedPercent == exactExpected
            && exactDuration?.expectedUsedPercent != 50,
            "pace uses exact duration seconds")

        let historicalLasts = HistoricalPace(
            expectedUsedPercent: 80, etaSeconds: nil,
            willLastToReset: true, runOutProbability: nil)
        let availableWindow = v3Window(
            used: 50, state: .available, historicalPace: historicalLasts)
        let hist = UsagePace.compute(
            window: availableWindow, mode: .historical, now: now)
        expect(hist?.expectedUsedPercent == 80 && hist?.stage == .farBehind
            && hist?.basis == .historical, "historical available uses backend expected")
        expect(hist?.willLastToReset == true && hist?.etaSeconds == nil,
            "historical lasts result is trusted")
        expect(hist?.isHistoricalDeficit == false, "historical reserve is not a deficit")

        let riskyWindow = v3Window(
            used: 90, state: .available,
            historicalPace: HistoricalPace(
                expectedUsedPercent: 50, etaSeconds: 120,
                willLastToReset: false, runOutProbability: 0.8))
        let risky = UsagePace.compute(window: riskyWindow, mode: .historical, now: now)
        expect(risky?.willLastToReset == false && risky?.etaSeconds == 120
            && risky?.basis == .historical && risky?.isHistoricalDeficit == true,
            "historical projected empty trusts backend eta and deficit gate")
        expect(risky?.etaText == "Projected empty in 2m", "historical projected empty text")

        let learningHistoryWindow = v3Window(used: 80, state: .learningHistory)
        let learningEstimate = UsagePace.compute(
            window: learningHistoryWindow, mode: .historical, now: now)
        expect(learningEstimate?.basis == .linear
            && learningEstimate?.stage.isDeficit == true
            && learningEstimate?.isHistoricalDeficit == false,
            "historical learningHistory is identifiable Linear estimate")
        expect(learningEstimate?.expectedUsedPercent == 50,
            "learningHistory historical mode uses Linear estimate")

        // Stage 5D UI presentation: typed state copy and mode gates are pure
        // helper behavior, so these contracts do not depend on SwiftUI layout.
        for mode in [PaceMode.historical, PaceMode.linear] {
            expect(
                AgentLimitsCard.PacePresentation.statusText(
                    state: .learningDuration, reason: nil, mode: mode)
                    == "Learning reset duration",
                "learningDuration copy in \(mode.rawValue) mode")
            expect(
                AgentLimitsCard.PacePresentation.statusText(
                    state: .legacyMissing, reason: nil, mode: mode)
                    == "Pace unavailable · legacy data",
                "legacy pace copy in \(mode.rawValue) mode")
        }
        expect(
            AgentLimitsCard.PacePresentation.statusText(
                state: .learningHistory, reason: nil, mode: .historical)
                == "Learning history · Linear estimate",
            "learningHistory uses exact Linear estimate copy")
        expect(
            AgentLimitsCard.PacePresentation.statusText(
                state: .learningHistory, reason: nil, mode: .linear) == "Linear"
                && AgentLimitsCard.PacePresentation.statusText(
                    state: .available, reason: nil, mode: .linear) == "Linear",
            "linear mode labels duration-ready cards")

        let unavailableCopies: [(UsagePaceUnavailableReason, String)] = [
            (.windowIdentity, "Pace unavailable · unknown quota window"),
            (.missingReset, "Pace unavailable · missing reset"),
            (.invalidEvidence, "Pace unavailable · invalid quota data"),
            (.accountScope, "Pace unavailable · account identity unavailable"),
            (.storeCapacity, "Pace unavailable · history storage full"),
            (.history, "Pace unavailable · history unavailable"),
            (.nonRecurring, "Pace unavailable · non-recurring quota"),
        ]
        for (reason, copy) in unavailableCopies {
            expect(
                AgentLimitsCard.PacePresentation.statusText(
                    state: .unavailable, reason: reason, mode: .historical) == copy,
                "typed unavailable \(reason.rawValue) copy")
        }
        expect(
            AgentLimitsCard.PacePresentation.statusText(
                state: .available, reason: nil, mode: .historical) == nil,
            "available has no learning status copy")
        expect(
            AgentLimitsCard.PacePresentation.statusText(
                state: .learningHistory, reason: nil, mode: .off) == nil
                    && AgentLimitsCard.PacePresentation.statusText(
                        state: .unavailable, reason: .history, mode: .off) == nil
                    && AgentLimitsCard.PacePresentation.statusText(
                        state: .legacyMissing, reason: nil, mode: .off) == nil,
            "off hides pace status")

        expect(UsagePace.compute(window: availableWindow, mode: .off, now: now) == nil,
            "pace mode off")
        let linear = UsagePace.compute(window: availableWindow, mode: .linear, now: now)
        expect(linear?.expectedUsedPercent == 50 && linear?.basis == .linear,
            "linear mode ignores available historical")
        expect(UsagePace.compute(window: availableWindow, now: now)?.basis == .linear,
            "direct pace compute stays linear")
        expect(
            AgentLimitsCard.PacePresentation.isHistoricalDeficit(risky)
                && !AgentLimitsCard.PacePresentation.isHistoricalDeficit(learningEstimate)
                && !AgentLimitsCard.PacePresentation.isHistoricalDeficit(linear),
            "UI warning color requires historical-basis deficit")
        let unavailableWindow = v3Window(used: 50, state: .unavailable)
        expect(UsagePace.compute(
            window: unavailableWindow, mode: .historical, now: now) == nil,
            "historical unavailable has no silent Linear fallback")
        expect(UsagePace.compute(
            window: unavailableWindow, mode: .linear, now: now) == nil,
            "linear unavailable has no pace")

        // Stage 0 old-fail/new-pass baseline: legacy windowMinutes cannot
        // restore pace when the typed paceStatus key is absent.
        let legacyWindow = try? JSONDecoder().decode(
            UsageWindow.self,
            from: Data(#"{"label":"Weekly","usedPercent":80,"remainingPercent":20,"windowMinutes":60}"#.utf8))
        expect(legacyWindow.flatMap {
            UsagePace.compute(window: $0, mode: .historical, now: now)
        } == nil, "stage0 legacy payload has no silent historical Linear fallback")
        expect(legacyWindow.flatMap {
            UsagePace.compute(window: $0, now: now)
        } == nil, "legacy windowMinutes cannot revive direct pace")

        let lastingRiskWindow = v3Window(
            used: 50, state: .available,
            historicalPace: HistoricalPace(
                expectedUsedPercent: 80, etaSeconds: nil,
                willLastToReset: true, runOutProbability: 0.2))
        let lastingRiskPace = UsagePace.compute(
            window: lastingRiskWindow, mode: .historical, now: now)!
        let lastingRiskPresentation = UsagePace.presentation(
            window: lastingRiskWindow, mode: .historical, pace: lastingRiskPace)
        expect(lastingRiskPresentation.etaText == nil
            && lastingRiskPresentation.riskText == "≈ 20% run-out risk",
            "historical available risk suppresses lasts text")
        expect(runOutRiskLabel(window: riskyWindow) == "≈ 80% run-out risk",
            "risk belongs to historical available")
        expect(runOutRiskLabel(window: riskyWindow, pace: linear) == nil,
            "Linear basis cannot display nested risk")
        expect(UsagePace.presentation(
            window: riskyWindow, mode: .linear, pace: linear!).riskText == nil,
            "linear presentation cannot display nested risk")
        expect(UsagePace.presentation(
            window: learningHistoryWindow, mode: .historical, pace: learningEstimate!).riskText == nil,
            "learningHistory Linear estimate cannot display nested risk")
        expect(runOutRiskLabel(window: v3Window(used: 50, state: .learningHistory)) == nil,
            "learningHistory has no historical risk")

        let exhaustedWindow = v3Window(
            used: 100, state: .available,
            historicalPace: HistoricalPace(
                expectedUsedPercent: 80, etaSeconds: 0,
                willLastToReset: false, runOutProbability: 1))
        let exhausted = UsagePace.compute(
            window: exhaustedWindow, mode: .historical, now: now)
        expect(exhausted?.etaSeconds == 0 && exhausted?.willLastToReset == false
            && exhausted?.etaText == "Projected empty now"
            && runOutRiskLabel(window: exhaustedWindow) == "≈ 100% run-out risk",
            "historical exhausted result is coherent")
        expect(UsagePace.durationText(130 * 60) == "2h 10m", "duration text h m")
        expect(UsagePace.durationText(26 * 3600) == "1d 2h", "duration text d h")

        // Stage 5A production decoder: v3 pace states are typed and strict;
        // only an entirely missing paceStatus key takes the internal legacy path.
        func decodeWindow(_ json: String) -> UsageWindow? {
            try? JSONDecoder().decode(UsageWindow.self, from: Data(json.utf8))
        }
        let learningDurationJSON = """
        {"cardId":"session.v1","label":"Session","usedPercent":20,"remainingPercent":80,
         "resetsAt":"2026-07-17T05:00:00Z",
         "paceStatus":{"state":"learningDuration","windowKey":"session.v1",
         "durationSource":"observed","completeCycles":0}}
        """
        let learningHistoryJSON = """
        {"cardId":"weekly.v1","label":"Weekly","usedPercent":35,"remainingPercent":65,
         "resetsAt":"2026-07-24T00:00:00Z","windowMinutes":300,
         "paceStatus":{"state":"learningHistory","windowKey":"weekly.v1",
         "durationSeconds":18000,"durationSource":"contract","completeCycles":2}}
        """
        let availableJSON = """
        {"cardId":"daily.v1","label":"Daily","usedPercent":60,"remainingPercent":40,
         "resetsAt":"2026-07-24T00:00:00Z","windowMinutes":300,
         "paceStatus":{"state":"available","windowKey":"daily.v1",
         "durationSeconds":18000,"durationSource":"contract","completeCycles":4},
         "historicalPace":{"expectedUsedPercent":55,"etaSeconds":900,
         "willLastToReset":false,"runOutProbability":0.25}}
        """
        let unavailableJSON = """
        {"cardId":"extra_usage.v1","label":"Extra usage","usedPercent":70,"remainingPercent":30,
         "paceStatus":{"state":"unavailable","windowKey":"extra_usage.v1",
         "completeCycles":0,"reason":"nonRecurring"}}
        """
        let learningDuration = decodeWindow(learningDurationJSON)
        let learningHistory = decodeWindow(learningHistoryJSON)
        let available = decodeWindow(availableJSON)
        let unavailable = decodeWindow(unavailableJSON)
        expect(
            learningDuration?.paceStatus.state == UsagePaceState.learningDuration &&
                learningDuration?.durationSeconds == nil &&
                learningDuration?.paceStatus.durationSource == .observed,
            "v3 learningDuration decodes with observed source")
        expect(
            learningHistory?.paceStatus.state == UsagePaceState.learningHistory &&
                learningHistory?.durationSeconds == 18_000 &&
                learningHistory?.historicalPace == nil,
            "v3 learningHistory decodes with exact duration")
        expect(
            available?.paceStatus.state == UsagePaceState.available &&
                available?.durationSeconds == 18_000 &&
                available?.historicalPace?.expectedUsedPercent == 55,
            "v3 available decodes with historical result")
        expect(
            unavailable?.paceStatus.state == UsagePaceState.unavailable &&
                unavailable?.paceStatus.reason == .nonRecurring &&
                unavailable?.durationSeconds == nil,
            "v3 unavailable decodes with typed reason")

        let legacyDecoded = decodeWindow(
            "{\"label\":\"Weekly\",\"usedPercent\":50,\"remainingPercent\":50,\"windowMinutes\":60}")
        expect(
            legacyDecoded?.paceStatus.state == UsagePaceState.legacyMissing &&
                legacyDecoded?.cardId == "legacy.missing.v1" &&
                legacyDecoded?.durationSeconds == nil && legacyDecoded?.windowMinutes == 60,
            "missing whole paceStatus uses fixed legacy identity without duration inference")

        let invalidFixtures: [(String, String)] = [
            ("present null paceStatus", """
             {"cardId":"weekly.v1","label":"Weekly","usedPercent":50,"remainingPercent":50,
              "paceStatus":null}
             """),
            ("unknown state", """
             {"cardId":"weekly.v1","label":"Weekly","usedPercent":50,"remainingPercent":50,
              "paceStatus":{"state":"futureState","windowKey":"weekly.v1","completeCycles":0}}
             """),
            ("unknown source", """
             {"cardId":"weekly.v1","label":"Weekly","usedPercent":50,"remainingPercent":50,
              "paceStatus":{"state":"learningHistory","windowKey":"weekly.v1",
              "durationSeconds":18000,"durationSource":"calendar","completeCycles":0}}
             """),
            ("unknown reason", """
             {"cardId":"extra_usage.v1","label":"Extra usage","usedPercent":50,"remainingPercent":50,
              "paceStatus":{"state":"unavailable","windowKey":"extra_usage.v1",
              "completeCycles":0,"reason":"unsupported"}}
             """),
            ("missing cardId", """
             {"label":"Weekly","usedPercent":50,"remainingPercent":50,
              "paceStatus":{"state":"learningDuration","windowKey":"weekly.v1","completeCycles":0}}
             """),
            ("contradictory percentages", """
             {"cardId":"weekly.v1","label":"Weekly","usedPercent":80,"remainingPercent":80,
              "paceStatus":{"state":"learningDuration","windowKey":"weekly.v1","completeCycles":0}}
             """),
            ("available without historicalPace", """
             {"cardId":"weekly.v1","label":"Weekly","usedPercent":50,"remainingPercent":50,
              "windowMinutes":300,"paceStatus":{"state":"available","windowKey":"weekly.v1",
              "durationSeconds":18000,"durationSource":"contract","completeCycles":0}}
             """),
            ("learningHistory with historicalPace", """
             {"cardId":"weekly.v1","label":"Weekly","usedPercent":50,"remainingPercent":50,
              "windowMinutes":300,"paceStatus":{"state":"learningHistory","windowKey":"weekly.v1",
              "durationSeconds":18000,"durationSource":"contract","completeCycles":0},
              "historicalPace":{"expectedUsedPercent":50,"willLastToReset":true}}
             """),
            ("windowKey and reason contradiction", """
             {"cardId":"unknown.v1","label":"Unknown","usedPercent":50,"remainingPercent":50,
              "paceStatus":{"state":"unavailable","windowKey":null,"completeCycles":0,
              "reason":"accountScope"}}
             """),
            ("duration and windowMinutes contradiction", """
             {"cardId":"weekly.v1","label":"Weekly","usedPercent":50,"remainingPercent":50,
              "windowMinutes":301,"paceStatus":{"state":"learningHistory","windowKey":"weekly.v1",
              "durationSeconds":18000,"durationSource":"contract","completeCycles":0}}
             """),
            ("duration without derived windowMinutes", """
             {"cardId":"weekly.v1","label":"Weekly","usedPercent":50,"remainingPercent":50,
              "paceStatus":{"state":"learningHistory","windowKey":"weekly.v1",
              "durationSeconds":18000,"durationSource":"contract","completeCycles":0}}
             """),
        ]
        for (label, json) in invalidFixtures {
            expect(decodeWindow(json) == nil, "v3 rejects \(label)")
        }

        let productionPayloadJSON = """
        {"generatedAt":"2026-07-17T00:00:00Z","agents":[
          {"clientId":"codex","source":"oauth","updatedAt":"2026-07-17T00:00:00Z",
           "identity":{"email":"fixture@example.invalid","plan":"plus"},
           "windows":[\(learningDurationJSON),\(learningHistoryJSON),\(availableJSON),\(unavailableJSON)],
           "credits":{"remaining":12.5,"unlimited":false},"error":null}
        ],"opencodeSubscriptions":["Codex"]}
        """
        let productionPayload = try? JSONDecoder().decode(
            AgentUsagePayload.self, from: Data(productionPayloadJSON.utf8))
        expect(
            productionPayload?.agents.count == 1 &&
                productionPayload?.agents.first?.windows.count == 4 &&
                productionPayload?.agents.first?.windows[2].paceStatus.state == .available,
            "complete AgentUsagePayload v3 shape decodes")

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
        expect(ClientRegistry.style("kimi").displayName == "Kimi", "Kimi registry covers CLI and Code")
        expect(ClientRegistry.style("junie").displayName == "Junie", "Junie registry metadata")
        expect(ClientRegistry.style("opencodereview").displayName == "OpenCodeReview", "OpenCodeReview registry metadata")
        // AgentLimitsCard keeps its own generic "-cli" fold for quota-card
        // attribution: explicit aliases via the registry, then a local strip so
        // antigravity-cli shares the antigravity quota snapshot — this fold must
        // NOT leak into the deny-filter canonicalizer above.
        expect(AgentLimitsCard.normalizeTraceClient("codex-cli") == "codex", "limits wrapper applies explicit alias")
        expect(AgentLimitsCard.normalizeTraceClient("antigravity-cli") == "antigravity", "limits wrapper folds generic -cli for quota attribution")

        // Quota resolver: card IDs are explicit and missing paceStatus remains
        // a valid legacy fixture. Selection tests intentionally read only
        // identity and percentage fields.
        let quotaJSON = """
        {"generatedAt":"now","agents":[
          {"clientId":"codex","source":"oauth","updatedAt":"now",
           "windows":[{"cardId":"session.v1","label":"Session","usedPercent":20,"remainingPercent":80},
                      {"cardId":"weekly.v1","label":"Weekly","usedPercent":65,"remainingPercent":35},
                      {"cardId":"model.gpt|preview.v1","label":"Delimiter","usedPercent":5,"remainingPercent":95}]},
          {"clientId":"claude","source":"oauth","updatedAt":"now",
           "windows":[{"cardId":"session.v1","label":"Session","usedPercent":88,"remainingPercent":12},
                      {"cardId":"weekly.v1","label":"Weekly","usedPercent":10,"remainingPercent":90}]},
          {"clientId":"broken","source":"oauth","updatedAt":"now",
           "windows":[{"cardId":"session.v1","label":"Session","usedPercent":99,"remainingPercent":1}],
           "error":"401"}
        ]}
        """
        let quotaPayload = try! JSONDecoder().decode(
            AgentUsagePayload.self, from: Data(quotaJSON.utf8))
        let tightest = QuotaResolver.resolve(payload: quotaPayload, selection: "auto")
        expect(
            tightest?.clientId == "claude" && tightest?.window.cardId == "session.v1",
            "auto resolves the tightest healthy card")
        expect(
            QuotaResolver.selection(clientId: "codex", cardId: "weekly.v1") == "codex|weekly.v1",
            "canonical selection stores cardId")
        let delimiterSelection = QuotaResolver.selection(
            clientId: "codex", cardId: "model.gpt|preview.v1")
        expect(
            delimiterSelection == "codex|model.gpt|preview.v1"
                && QuotaResolver.canonicalSelection(
                    payload: quotaPayload, selection: delimiterSelection) == delimiterSelection
                && QuotaResolver.resolve(payload: quotaPayload, selection: delimiterSelection)?
                    .window.cardId == "model.gpt|preview.v1",
            "card selection preserves delimiters inside cardId")
        expect(
            QuotaResolver.canonicalSelection(payload: quotaPayload, selection: "codex|Weekly")
                == "codex|weekly.v1"
                && QuotaResolver.resolve(payload: quotaPayload, selection: "codex|Weekly")?
                    .window.cardId == "weekly.v1",
            "unique legacy label migrates to cardId")
        expect(
            QuotaSelectionPolicy.migrationToPersist(
                payload: quotaPayload, persistedSelection: "codex|Weekly") == "codex|weekly.v1"
                && QuotaSelectionPolicy.migrationToPersist(
                    payload: quotaPayload, persistedSelection: "codex|weekly.v1") == nil
                && QuotaSelectionPolicy.migrationToPersist(
                    payload: quotaPayload, persistedSelection: "codex|stale") == nil,
            "selection policy persists only a proven legacy migration")
        expect(
            QuotaResolver.canonicalSelection(payload: quotaPayload, selection: "codex|stale")
                == "codex|stale"
                && QuotaResolver.resolve(payload: quotaPayload, selection: "codex|stale") == nil,
            "temporarily absent explicit card stays selected instead of following Auto")
        expect(
            QuotaResolver.canonicalSelection(payload: quotaPayload, selection: "nope|Session")
                == "nope|Session"
                && QuotaResolver.resolve(payload: quotaPayload, selection: "nope|Session") == nil,
            "temporarily absent explicit client stays selected instead of following Auto")
        expect(
            QuotaResolver.canonicalSelection(payload: quotaPayload, selection: "codex|Weekly|extra")
                == "codex|Weekly|extra"
                && QuotaResolver.resolve(
                    payload: quotaPayload, selection: "codex|Weekly|extra") == nil,
            "unmatched explicit selection preserves delimiter characters")
        expect(
            QuotaResolver.canonicalSelection(payload: nil, selection: "future|legacy-card.v1")
                == "future|legacy-card.v1"
                && QuotaResolver.canonicalSelection(payload: nil, selection: "future|legacy|extra")
                    == "future|legacy|extra",
            "payload nil preserves explicit selection delimiters")
        expect(
            QuotaResolver.canonicalSelection(payload: quotaPayload, selection: "future")
                == QuotaResolver.auto
                && QuotaResolver.canonicalSelection(payload: quotaPayload, selection: "|card")
                    == QuotaResolver.auto
                && QuotaResolver.canonicalSelection(payload: quotaPayload, selection: "future|")
                    == QuotaResolver.auto,
            "structurally malformed selections normalize to Auto")
        expect(QuotaResolver.resolve(payload: nil, selection: "auto") == nil, "no payload, no quota")

        let duplicateJSON = """
        {"generatedAt":"now","agents":[
          {"clientId":"dupe","source":"fixture","updatedAt":"now",
           "windows":[
             {"cardId":"same.v1","label":"Ambiguous","usedPercent":20,"remainingPercent":80},
             {"cardId":"same.v1","label":"Ambiguous","usedPercent":99,"remainingPercent":1},
             {"cardId":"other.v1","label":"Ambiguous","usedPercent":70,"remainingPercent":30},
             {"cardId":"Session","label":"Other","usedPercent":90,"remainingPercent":10},
             {"cardId":"other-session.v1","label":"Session","usedPercent":75,"remainingPercent":25}
           ]}
        ]}
        """
        let duplicatePayload = try! JSONDecoder().decode(
            AgentUsagePayload.self, from: Data(duplicateJSON.utf8))
        let duplicateAgent = duplicatePayload.agents[0]
        expect(
            duplicateAgent.uniqueCardWindows.map(\.cardId)
                == ["same.v1", "other.v1", "Session", "other-session.v1"],
            "unique card view keeps first occurrence order")
        expect(
            duplicateAgent.uniqueCardWindows.allSatisfy { $0.cardId != "same.v1" || $0.remainingPercent == 80 }
                && duplicateAgent.uniqueCardWindows.count == 4,
            "duplicate card later occurrence fails closed")
        expect(
            QuotaResolver.canonicalSelection(payload: duplicatePayload, selection: "dupe|Ambiguous")
                == "dupe|Ambiguous",
            "ambiguous legacy label cannot be migrated")
        expect(
            QuotaResolver.resolve(payload: duplicatePayload, selection: "dupe|Ambiguous") == nil,
            "ambiguous legacy label stays explicit instead of following Auto")
        expect(
            QuotaResolver.resolve(payload: duplicatePayload, selection: "dupe|same.v1")?
                .window.remainingPercent == 80
                && QuotaResolver.resolve(payload: duplicatePayload, selection: "auto")?.window.cardId
                    == "Session",
            "duplicate card is not rendered or considered by Auto")
        expect(
            QuotaResolver.canonicalSelection(payload: duplicatePayload, selection: "dupe|Other")
                == "dupe|Session"
                && QuotaResolver.canonicalSelection(payload: duplicatePayload, selection: "dupe|Session")
                    == "dupe|Session",
            "exact cardId wins over same-named legacy label")

        // Auto pick excludes hidden clients (issue #36): hiding the tightest
        // (claude|Session, 12%) makes auto fall to the next healthy card
        // (codex|Weekly, 35%); an EXPLICIT pick of a hidden client is honored;
        // empty exclusion is byte-identical to the default.
        let autoExClaude = QuotaResolver.resolve(
            payload: quotaPayload, selection: "auto", excluding: ["claude"])
        expect(autoExClaude?.clientId == "codex" && autoExClaude?.window.cardId == "weekly.v1",
            "auto skips a hidden tightest-window client")
        expect(
            QuotaResolver.resolve(
                payload: quotaPayload, selection: "claude|session.v1", excluding: ["claude"])?
                .window.remainingPercent == 12,
            "explicit selection of a hidden client still resolves")
        expect(
            QuotaResolver.resolve(payload: quotaPayload, selection: "auto", excluding: [])?
                .clientId == tightest?.clientId,
            "empty exclusion is byte-identical to the default auto pick")
        // Exclusion vs no-data disambiguation (issue #36 R8): resolve returning
        // nil because EVERY candidate is excluded is distinguishable from nil
        // for no payload / no healthy window, so the tray suppresses a stale
        // hidden cache only in the former.
        expect(
            QuotaResolver.excludedAllCandidates(
                payload: quotaPayload, selection: "auto", excluding: ["codex", "claude"]),
            "excludedAllCandidates true when the only healthy clients are all hidden")
        expect(
            !QuotaResolver.excludedAllCandidates(
                payload: quotaPayload, selection: "auto", excluding: ["claude"]),
            "excludedAllCandidates false while a visible candidate survives")
        expect(
            !QuotaResolver.excludedAllCandidates(
                payload: duplicatePayload, selection: "dupe|Ambiguous", excluding: ["dupe"]),
            "unresolved explicit selection does not acquire Auto exclusion semantics")
        expect(
            !QuotaResolver.excludedAllCandidates(payload: nil, selection: "auto", excluding: ["claude"]),
            "excludedAllCandidates false with no payload (fetch-failure keeps the cache)")
        expect(
            !QuotaResolver.excludedAllCandidates(
                payload: quotaPayload, selection: "claude|session.v1", excluding: ["claude"]),
            "excludedAllCandidates false for an explicit selection")
        expect(
            !QuotaResolver.excludedAllCandidates(payload: quotaPayload, selection: "auto", excluding: []),
            "excludedAllCandidates false for an empty exclusion")

        // Year picker visibility (issue #36): years in which only hidden clients
        // had activity drop from the picker. Fixture: vis active in 2025, hid
        // only in 2026 → hiding hid leaves {2025} visible.
        let yearJSON = """
        {"meta":{"generatedAt":"now","version":"1","dateRange":{"start":"2025-06-01","end":"2026-06-01"}},
         "summary":{"totalTokens":0,"totalCost":0,"totalDays":0,"activeDays":0,"averagePerDay":0,
                    "maxCostInSingleDay":0,"clients":["vis","hid"],"models":[]},
         "years":[],
         "contributions":[
           {"date":"2025-06-01","totals":{"tokens":10,"cost":1,"messages":1},"intensity":1,
            "tokenBreakdown":{"input":10,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0},
            "clients":[{"client":"vis","modelId":"m","providerId":"p","cost":1,"messages":1,
             "tokens":{"input":10,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0}}]},
           {"date":"2026-06-01","totals":{"tokens":10,"cost":1,"messages":1},"intensity":1,
            "tokenBreakdown":{"input":10,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0},
            "clients":[{"client":"hid","modelId":"m","providerId":"p","cost":1,"messages":1,
             "tokens":{"input":10,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0}}]}
         ]}
        """
        let yearPayload = try! JSONDecoder().decode(UsagePayload.self, from: Data(yearJSON.utf8))
        let visYears = UsageStats.yearsWithVisibleActivity(
            contributions: yearPayload.contributions, hidden: ["hid"])
        expect(visYears == ["2025"], "year picker drops a year only hidden clients used")
        expect(
            UsageStats.yearsWithVisibleActivity(contributions: yearPayload.contributions, hidden: [])
                == ["2025", "2026"],
            "no hidden clients keeps every active year")
        // Auto-clear a hidden-only scoped year (issue #36 R8): a year-scoped
        // payload whose only stripe is a hidden client signals needs-clear; any
        // visible stripe keeps it. (2026 in the fixture is hid-only.)
        let scoped2026 = yearPayload.contributions.filter { $0.date.hasPrefix("2026") }
        expect(!UsageStats.hasVisibleActivity(contributions: scoped2026, hidden: ["hid"]),
            "hidden-only scoped year has no visible activity (auto-clear)")
        expect(UsageStats.hasVisibleActivity(contributions: scoped2026, hidden: []),
            "a visible stripe keeps the scoped year")

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
        // UsageStats' day/total accumulators (the filtered Overview/Stats path)
        // must saturate too — a single Int64.max-clamped stripe folded with a
        // normal one renders a pinned total, never a trapping crash.
        let satJSON = """
        {"meta":{"generatedAt":"now","version":"1","dateRange":{"start":"2026-07-01","end":"2026-07-01"}},
         "summary":{"totalTokens":0,"totalCost":0,"totalDays":1,"activeDays":1,"averagePerDay":0,
                    "maxCostInSingleDay":0,"clients":["big","small"],"models":[]},
         "years":[],
         "contributions":[
           {"date":"2026-07-01","totals":{"tokens":0,"cost":2,"messages":2},"intensity":1,
            "tokenBreakdown":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0},
            "clients":[
              {"client":"big","modelId":"m","providerId":"p","cost":1,"messages":1,
               "tokens":{"input":9223372036854775807,"output":9223372036854775807,"cacheRead":0,"cacheWrite":0,"reasoning":0}},
              {"client":"small","modelId":"m","providerId":"p","cost":1,"messages":1,
               "tokens":{"input":100,"output":50,"cacheRead":0,"cacheWrite":0,"reasoning":0}}]}
         ]}
        """
        let satPayload = try! JSONDecoder().decode(UsagePayload.self, from: Data(satJSON.utf8))
        let satAll = UsageStats(payload: satPayload, selectedClients: ["big", "small"])
        expect(satAll.totalTokens == .max && satAll.perDayMap["2026-07-01"]?.tokens == .max
            && satAll.maxTokens == .max,
            "UsageStats saturates an Int64.max stripe instead of trapping")
        let satSmall = UsageStats(payload: satPayload, selectedClients: ["small"])
        expect(satSmall.totalTokens == 150 && satSmall.perDayMap["2026-07-01"]?.tokens == 150,
            "UsageStats is exact for normal stripes")

        // Monthly lens (plan 2026-07-15): month-level date formatter.
        expect(Format.monthYear("2026-07") == "Jul 2026", "monthYear formats YYYY-MM")
        expect(Format.monthYear("2025-12") == "Dec 2025", "monthYear formats December")
        expect(Format.monthYear("garbage") == "garbage", "monthYear passes malformed input through")
        expect(Format.monthYear("2026-13") == "2026-13", "monthYear rejects month 13")

        // Monthly lens bucketing (plan 2026-07-15): group by the FULL
        // "YYYY-MM" prefix (never month-of-year), strict client allowlist,
        // saturating folds, drill-down merges model slices across days.
        let monthlyJSON = """
        {"meta":{"generatedAt":"now","version":"1","dateRange":{"start":"2025-12-31","end":"2026-01-02"}},
         "summary":{"totalTokens":0,"totalCost":0,"totalDays":3,"activeDays":3,"averagePerDay":0,
                    "maxCostInSingleDay":0,"clients":["a","b"],"models":[]},
         "years":[],
         "contributions":[
           {"date":"2025-12-31","totals":{"tokens":0,"cost":1,"messages":1},"intensity":1,
            "tokenBreakdown":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0},
            "clients":[
              {"client":"a","modelId":"m1","providerId":"p","cost":1,"messages":1,
               "tokens":{"input":100,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0}}]},
           {"date":"2026-01-01","totals":{"tokens":0,"cost":3,"messages":2},"intensity":1,
            "tokenBreakdown":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0},
            "clients":[
              {"client":"a","modelId":"m1","providerId":"p","cost":1,"messages":1,
               "tokens":{"input":40,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0}},
              {"client":"b","modelId":"m9","providerId":"p","cost":2,"messages":1,
               "tokens":{"input":7,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0}}]},
           {"date":"2026-01-02","totals":{"tokens":0,"cost":1,"messages":1},"intensity":1,
            "tokenBreakdown":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0},
            "clients":[
              {"client":"a","modelId":"m1","providerId":"p","cost":1,"messages":1,
               "tokens":{"input":9223372036854775807,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0}}]}
         ]}
        """
        let monthlyPayload = try! JSONDecoder().decode(UsagePayload.self, from: Data(monthlyJSON.utf8))
        let mRows = MonthlyView.monthRows(payload: monthlyPayload, clientIds: ["a"])
        expect(mRows.count == 2 && mRows[0].month == "2026-01" && mRows[1].month == "2025-12",
            "monthly buckets split at the year boundary, most recent first")
        expect(mRows[1].tokens == 100 && mRows[1].messages == 1,
            "december totals only december")
        expect(mRows[0].tokens == .max,
            "monthly token fold saturates an Int64.max stripe")
        expect(mRows[0].cost == 2.0 && mRows[0].messages == 2,
            "hidden client b is excluded from january totals")
        expect(MonthlyView.monthRows(payload: monthlyPayload, clientIds: []).isEmpty,
            "empty client selection shows no months")
        let mSlices = MonthlyView.modelSlices(
            for: mRows[0], clientIds: ["a"], colors: ModelColorMap(report: nil))
        expect(mSlices.count == 1 && mSlices[0].key == "m1|p" && mSlices[0].tokens == .max,
            "drill-down merges the month's model slices across days with saturation")
        expect(MonthlyView.modelSlices(
                for: mRows[0], clientIds: ["a", "b"], colors: ModelColorMap(report: nil)).count == 2,
            "drill-down shows client b's model when b is selected")

        // Message-only activity (PR #54 review r3595383789): a contribution
        // with messages but zero tokens and zero cost must still surface —
        // some parsers emit message-count-only rows. Prior guard was
        // `tokens > 0 || cost > 0`, which dropped this month entirely.
        let messageOnlyJSON = """
        {"meta":{"generatedAt":"now","version":"1","dateRange":{"start":"2026-02-01","end":"2026-02-01"}},
         "summary":{"totalTokens":0,"totalCost":0,"totalDays":1,"activeDays":1,"averagePerDay":0,
                    "maxCostInSingleDay":0,"clients":["a"],"models":[]},
         "years":[],
         "contributions":[
           {"date":"2026-02-01","totals":{"tokens":0,"cost":0,"messages":5},"intensity":0,
            "tokenBreakdown":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0},
            "clients":[
              {"client":"a","modelId":"m1","providerId":"p","cost":0,"messages":5,
               "tokens":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0}}]}
         ]}
        """
        let messageOnlyPayload = try! JSONDecoder().decode(UsagePayload.self, from: Data(messageOnlyJSON.utf8))
        let moRows = MonthlyView.monthRows(payload: messageOnlyPayload, clientIds: ["a"])
        expect(moRows.count == 1 && moRows[0].messages == 5 && moRows[0].tokens == 0 && moRows[0].cost == 0,
            "a message-only month (zero tokens, zero cost) still surfaces in the Monthly lens")

        // Tab order (plan 2026-07-16): Monthly leads Daily in the tab row.
        expect(AppView.allCases.map(\.rawValue) ==
            ["overview", "models", "monthly", "daily", "hourly", "stats", "agents"],
            "tab row leads with Monthly, ahead of Daily")

        // View-tabs visibility (plan 2026-07-16, generalized): any of the
        // five toggleable lenses can be hidden independently; Overview and
        // Models are fixed anchors, never in AppView.toggleable.
        expect(AppView.toggleable == [.monthly, .daily, .hourly, .stats, .agents],
            "toggleable lenses are fixed order, excluding Overview and Models")
        expect(AppView.visible(hiddenRaw: "") == AppView.allCases,
            "no hidden lenses shows every lens")
        expect(AppView.visible(hiddenRaw: "monthly,hourly") ==
            AppView.allCases.filter { $0 != .monthly && $0 != .hourly },
            "hiding two lenses removes exactly those two, order otherwise unchanged")
        expect(AppView.effective(.monthly, hiddenRaw: "monthly") == .overview,
            "a hidden lens falls back to overview")
        expect(AppView.effective(.monthly, hiddenRaw: "") == .monthly,
            "a visible lens is unaffected")
        expect(AppView.effective(.daily, hiddenRaw: "monthly") == .daily,
            "hiding one lens doesn't affect another")
        // Hardening (code review, plan 2026-07-16): only `toggleable` lenses
        // are ever actually hideable, even if the persisted raw string is
        // tampered with out-of-band (e.g. a manually edited UserDefaults
        // value) to contain "overview" or "models" — Overview must always
        // remain the guaranteed fallback target.
        expect(AppView.visible(hiddenRaw: "overview,models") == AppView.allCases,
            "overview and models can never be hidden, even via a tampered raw string")
        expect(AppView.effective(.overview, hiddenRaw: "overview") == .overview,
            "overview is never subject to the hidden-lens fallback")

        // Filtered stats derive their range from the SELECTED clients (issue
        // #36 Fix, round 5): a hidden client active AFTER the visible client's
        // last day must not reset/shorten the visible streak. Fixture: "vis"
        // active 07-01..07-03, hidden "hid" active 07-05 → meta.dateRange
        // spans 07-01..07-05. Without the fix, streaks for {vis} walk to 07-05
        // and current resets to 0 on the empty 07-04/07-05 tail; with the fix
        // the range is 07-01..07-03 so current == longest == 3.
        func daily(_ client: String, _ date: String, _ cost: Double) -> String {
            """
            {"date":"\(date)","totals":{"tokens":10,"cost":\(cost),"messages":1},"intensity":1,
             "tokenBreakdown":{"input":10,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0},
             "clients":[{"client":"\(client)","modelId":"m","providerId":"p","cost":\(cost),"messages":1,
              "tokens":{"input":10,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0}}]}
            """
        }
        func rangeStatsPayload(end: String, days: [String]) -> UsagePayload {
            let json = """
            {"meta":{"generatedAt":"now","version":"1","dateRange":{"start":"2026-07-01","end":"\(end)"}},
             "summary":{"totalTokens":0,"totalCost":0,"totalDays":0,"activeDays":0,"averagePerDay":0,
                        "maxCostInSingleDay":0,"clients":["vis","hid"],"models":[]},
             "years":[],
             "contributions":[\(days.joined(separator: ","))]}
            """
            return try! JSONDecoder().decode(UsagePayload.self, from: Data(json.utf8))
        }
        // With the hidden client extending the range to 07-05.
        let withHidden = rangeStatsPayload(end: "2026-07-05", days: [
            daily("vis", "2026-07-01", 1), daily("vis", "2026-07-02", 1),
            daily("vis", "2026-07-03", 1), daily("hid", "2026-07-05", 1),
        ])
        let visFiltered = UsageStats(payload: withHidden, selectedClients: ["vis"])
        expect(visFiltered.streaks.current == 3 && visFiltered.streaks.longest == 3,
            "filtered streak ignores a hidden client's later activity")
        expect(visFiltered.dateRange.end == "2026-07-03",
            "filtered range ends at the selected clients' last active day")
        expect(visFiltered.averagePerDay == 1,
            "filtered averagePerDay divides by selected active days, not the hidden-extended span")
        // Equivalence: same numbers as a payload where the hidden client never
        // existed (range naturally 07-01..07-03, {vis} is all present).
        let noHidden = rangeStatsPayload(end: "2026-07-03", days: [
            daily("vis", "2026-07-01", 1), daily("vis", "2026-07-02", 1),
            daily("vis", "2026-07-03", 1),
        ])
        let visAlone = UsageStats(payload: noHidden, selectedClients: ["vis"])
        expect(visFiltered.streaks.current == visAlone.streaks.current
            && visFiltered.streaks.longest == visAlone.streaks.longest
            && visFiltered.dateRange.end == visAlone.dateRange.end,
            "filtered stats equal a payload without the hidden client")

        // DayBars trailing window anchors to the passed range end, not the
        // unfiltered payload range (issue #36 Fix, round 6): the caller passes
        // the selection-derived stats.dateRange.end, so a hidden client active
        // AFTER the visible client can't shift the window past the visible
        // activity. Fixture: vis active 07-03, hidden active 07-05.
        let chartPayload = rangeStatsPayload(end: "2026-07-05", days: [
            daily("vis", "2026-07-03", 1), daily("hid", "2026-07-05", 1),
        ])
        let chartColors = ModelColorMap(report: nil)
        let visBars = DayBars.build(
            payload: chartPayload, clientIds: ["vis"], stackBy: .agent,
            colors: chartColors, rangeEnd: "2026-07-03", endFallback: "2026-07-09")
        expect(visBars.count == DayBars.window && visBars.last?.date == "2026-07-03",
            "chart window anchors to the filtered range end")
        expect((visBars.last?.totalTokens ?? 0) > 0,
            "visible client's last active day is the last (in-window) bar")
        // The old unfiltered anchor (meta.dateRange.end = the hidden client's
        // later day) shifts the window forward, stranding an empty trailing bar.
        let shiftedBars = DayBars.build(
            payload: chartPayload, clientIds: ["vis"], stackBy: .agent,
            colors: chartColors, rangeEnd: "2026-07-05", endFallback: "2026-07-09")
        expect(shiftedBars.last?.date == "2026-07-05" && (shiftedBars.last?.totalTokens ?? 0) == 0,
            "unfiltered anchor would shift the window past the visible activity")

        // Synthetic --demo source: one fixture must drive every usage lens,
        // quota card, trace row, tray rate, and year selection without a live
        // FFI call. The fixture itself is the only data definition here.
        let demoSource = UsageDataSources.make(arguments: ["TokenBar", "--demo"])
        let liveSource = UsageDataSources.make(arguments: ["TokenBar"])
        expect(demoSource is DemoUsageDataSource, "usage source factory selects demo mode")
        expect(liveSource is LiveUsageDataSource, "usage source factory selects live mode")
        expect(!demoSource.allowsQuotaCachePersistence, "demo source disables quota cache persistence")
        expect(liveSource.allowsQuotaCachePersistence, "live source allows quota cache persistence")

        let demoPayload = DemoData.payload
        let demoDates = demoPayload.contributions.map(\.date)
        let demoDayNumbers = demoDates.compactMap { ISODay($0)?.number }
        let consecutive = zip(demoDayNumbers, demoDayNumbers.dropFirst())
            .allSatisfy { $1 == $0 + 1 }
        expect(
            demoDates.count == 14 && demoDates == demoDates.sorted() && consecutive,
            "demo graph has 14 sorted consecutive days")
        expect(
            demoPayload.contributions.allSatisfy { $0.clients.count == ClientRegistry.allIds.count },
            "demo graph carries every registered client on every day")

        let contributionTokens = demoPayload.contributions.reduce(Int64(0)) {
            $0.saturatingAdding($1.totals.tokens)
        }
        let contributionCost = demoPayload.contributions.reduce(0.0) { $0 + $1.totals.cost }
        expect(
            contributionTokens == demoPayload.summary.totalTokens
                && abs(contributionCost - demoPayload.summary.totalCost) < 0.000_000_001,
            "demo summary totals equal contribution totals")

        let summaryClients = Set(demoPayload.summary.clients)
        let contributionClients = Set(
            demoPayload.contributions.flatMap { $0.clients.map(\.client) })
        let quota = DemoData.agentUsage
        let quotaClients = Set(quota.agents.map(\.clientId))
        let registryClients = Set(ClientRegistry.allIds)
        expect(
            summaryClients == registryClients && contributionClients == registryClients
                && quotaClients == registryClients,
            "demo summary contributions and quota share the client set")
        expect(
            quota.agents.count == ClientRegistry.allIds.count
                && quota.agents.allSatisfy { agent in
                    let windows = agent.uniqueCardWindows
                    return windows.count == 2
                        && windows[0].cardId == "session.v1"
                        && windows[1].cardId == "weekly.v1"
                },
            "demo quota cards use unique canonical window identities")

        let firstDemoWindows = quota.agents.first?.uniqueCardWindows ?? []
        let secondDemoWindows = quota.agents.dropFirst().first?.uniqueCardWindows ?? []
        let demoLearningDuration = firstDemoWindows.first
        let demoLearningHistory = firstDemoWindows.dropFirst().first
        let demoAvailable = secondDemoWindows.first
        let demoUnavailable = secondDemoWindows.dropFirst().first
        expect(
            firstDemoWindows.count == 2
                && demoLearningDuration?.paceStatus.state == .learningDuration
                && demoLearningDuration?.durationSeconds == nil
                && demoLearningDuration?.windowMinutes == nil
                && demoLearningDuration?.paceStatus.durationSource == .observed
                && demoLearningHistory?.paceStatus.state == .learningHistory
                && demoLearningHistory?.durationSeconds == 604_800
                && demoLearningHistory?.windowMinutes == 10_080
                && demoLearningHistory?.historicalPace == nil,
            "demo fixture exposes learning-duration and learning-history rows")
        expect(
            secondDemoWindows.count == 2
                && demoAvailable?.paceStatus.state == .available
                && demoAvailable?.durationSeconds == 18_000
                && demoAvailable?.historicalPace?.expectedUsedPercent == 35
                && demoUnavailable?.paceStatus.state == .unavailable
                && demoUnavailable?.paceStatus.reason == .missingReset
                && demoUnavailable?.resetsAt == nil,
            "demo fixture exposes historical-available and typed-unavailable rows")

        let demoLearningEstimate = demoLearningHistory.flatMap {
            UsagePace.compute(window: $0, mode: .historical)
        }
        let demoHistoricalAhead = demoAvailable.flatMap {
            UsagePace.compute(window: $0, mode: .historical)
        }
        expect(
            demoLearningEstimate?.basis == .linear
                && demoLearningEstimate?.isHistoricalDeficit == false,
            "demo learning-history estimate cannot trigger historical warning color")
        expect(
            demoHistoricalAhead?.basis == .historical
                && demoHistoricalAhead?.stage.isDeficit == true
                && demoHistoricalAhead?.isHistoricalDeficit == true,
            "demo available row is a historical deficit acceptance fixture")
        expect(
            demoLearningDuration.flatMap {
                UsagePace.compute(window: $0, mode: .historical)
            } == nil
                && demoUnavailable.flatMap {
                    UsagePace.compute(window: $0, mode: .historical)
                } == nil,
            "demo learning-duration and unavailable rows suppress projections")
        expect(
            quota.agents.dropFirst(2).allSatisfy { agent in
                agent.uniqueCardWindows.allSatisfy {
                    $0.paceStatus.state == .learningHistory
                        && $0.paceStatus.durationSource == .contract
                        && $0.paceStatus.completeCycles == 0
                        && $0.historicalPace == nil
                }
            },
            "remaining demo quota cards stay on canonical learning-history fixtures")

        let modelReport = DemoData.modelReport
        let hourlyReport = DemoData.hourlyReport
        let agentsReport = DemoData.agentsReport
        let trace = DemoData.trace(windowSecs: 600)
        let modelClients = Set(modelReport.entries.map(\.client))
        let hourlyClients = Set(hourlyReport.entries.flatMap(\.clients))
        let agentClients = Set(agentsReport.entries.flatMap(\.clients))
        let traceClients = Set(trace.map(\.client))
        let hourlyKeys = hourlyReport.entries.map(\.hour)
        expect(
            Set(hourlyKeys).count == hourlyKeys.count && hourlyKeys == hourlyKeys.sorted()
                && hourlyReport.entries.allSatisfy {
                    $0.clients == $0.clients.sorted() && $0.models == $0.models.sorted()
                },
            "demo hourly buckets are unique and sorted")
        expect(
            !modelReport.entries.isEmpty && !hourlyReport.entries.isEmpty
                && !agentsReport.entries.isEmpty && !trace.isEmpty,
            "demo reports and trace are non-empty")
        expect(
            modelClients == registryClients && hourlyClients == registryClients
                && agentClients == registryClients && traceClients == registryClients,
            "demo report and trace ids are registered clients")

        let selectedClient = ClientRegistry.allIds.first ?? ""
        var graphInput: Int64 = 0
        var graphOutput: Int64 = 0
        var graphCacheRead: Int64 = 0
        var graphCacheWrite: Int64 = 0
        var graphReasoning: Int64 = 0
        var graphMessages = 0
        var graphCost = 0.0
        for contribution in demoPayload.contributions {
            for client in contribution.clients where client.client == selectedClient {
                graphInput += client.tokens.input
                graphOutput += client.tokens.output
                graphCacheRead += client.tokens.cacheRead
                graphCacheWrite += client.tokens.cacheWrite
                graphReasoning += client.tokens.reasoning
                graphMessages += client.messages
                graphCost += client.cost
            }
        }
        let selectedHourly = DemoData.hourlyReport(for: nil, clients: [selectedClient])
        let hourlyInput = selectedHourly.entries.reduce(Int64(0)) { $0 + $1.input }
        let hourlyOutput = selectedHourly.entries.reduce(Int64(0)) { $0 + $1.output }
        let hourlyCacheRead = selectedHourly.entries.reduce(Int64(0)) { $0 + $1.cacheRead }
        let hourlyCacheWrite = selectedHourly.entries.reduce(Int64(0)) { $0 + $1.cacheWrite }
        let hourlyReasoning = selectedHourly.entries.reduce(Int64(0)) { $0 + $1.reasoning }
        let hourlyMessages = selectedHourly.entries.reduce(0) { $0 + $1.messageCount }
        let hourlyCost = selectedHourly.entries.reduce(0.0) { $0 + $1.cost }
        expect(
            graphInput == hourlyInput && graphOutput == hourlyOutput
                && graphCacheRead == hourlyCacheRead && graphCacheWrite == hourlyCacheWrite
                && graphReasoning == hourlyReasoning && graphMessages == hourlyMessages
                && abs(graphCost - hourlyCost) < 0.000_000_001,
            "selected demo hourly totals equal graph client rows")

        expect(
            quota.agents.allSatisfy { agent in
                !agent.windows.isEmpty && agent.windows.allSatisfy { window in
                    let durationShapeIsValid = switch window.paceStatus.state {
                    case .learningHistory, .available:
                        (window.windowMinutes ?? 0) > 0
                    case .learningDuration, .unavailable, .legacyMissing:
                        window.windowMinutes == nil
                    }
                    return durationShapeIsValid
                        && window.usedPercent >= 0 && window.remainingPercent > 0
                        && abs(window.usedPercent + window.remainingPercent - 100) < 0.000_001
                }
            },
            "demo quota windows have valid duration and percentage shapes")
        let rawDemoRate = DemoData.tokensPerMin
        let traceRate = trace.reduce(0.0) { $0 + $1.tokensPerMin }
        let selectedTraceRate = trace.first { $0.client == selectedClient }?.tokensPerMin ?? 0
        let hiddenTraceRate = TraceBucket.totalRate(trace, hidden: [selectedClient])
        let allHiddenTraceRate = TraceBucket.totalRate(trace, hidden: registryClients)
        expect(
            rawDemoRate > 0 && trace.allSatisfy { $0.tokensPerMin > 0 }
                && abs(rawDemoRate - traceRate) < 0.000_001
                && abs(hiddenTraceRate - (rawDemoRate - selectedTraceRate)) < 0.000_001
                && allHiddenTraceRate == 0,
            "demo raw rate equals trace and hidden-client reductions")

        let currentYear = String(Format.todayKey().prefix(4))
        let currentPayload = DemoData.payload(for: currentYear)
        let otherYear = String((Int(currentYear) ?? 2000) - 1)
        let otherPayload = DemoData.payload(for: otherYear)
        expect(
            demoPayload.contributions.last?.date == Format.todayKey()
                && currentPayload.contributions.last?.date == Format.todayKey(),
            "demo nil and current-year windows end today")
        expect(
            otherPayload.contributions.count == 14
                && otherPayload.contributions.allSatisfy { $0.date.hasPrefix(otherYear) }
                && otherPayload.years.contains { $0.year == otherYear },
            "demo non-current year stays within the selected year")

        let demoJan1 = DemoData.dates(for: "2024", today: "2024-01-01")
        let demoJan13 = DemoData.dates(for: "2024", today: "2024-01-13")
        let demoJan14 = DemoData.dates(for: "2024", today: "2024-01-14")
        let rollingJan1 = DemoData.dates(for: nil, today: "2024-01-01")
        let leapDay = DemoData.dates(for: "2024", today: "2024-02-29")
        let priorYear = DemoData.dates(for: "2023", today: "2024-02-29")
        let invalidYear = DemoData.dates(for: "not-a-year", today: "2024-02-29")
        expect(
            demoJan1.count == 1 && demoJan1.last == "2024-01-01"
                && demoJan13.count == 13 && demoJan13.last == "2024-01-13"
                && demoJan14.count == 14 && demoJan14.last == "2024-01-14",
            "demo current-year dates clamp at January 1")
        expect(
            rollingJan1.count == 14 && rollingJan1.last == "2024-01-01"
                && rollingJan1.first?.hasPrefix("2023-") == true,
            "demo all-years dates retain the rolling cross-year window")
        expect(
            leapDay.count == 14 && leapDay.contains("2024-02-29")
                && priorYear.count == 14 && priorYear.allSatisfy { $0.hasPrefix("2023-") }
                && invalidYear.count == 14 && invalidYear.last == "2024-02-29",
            "demo date helper handles leap and invalid years")

        let sourcedPayload = awaitValue {
            try await demoSource.graph(year: nil, priority: .userInitiated)
        }
        let sourcedRefresh = awaitValue {
            try await demoSource.refreshGraph(year: nil, priority: .userInitiated)
        }
        let sourcedModels = awaitValue {
            try await demoSource.modelReport(year: nil, priority: .userInitiated)
        }
        let sourcedHourly = awaitValue {
            try await demoSource.hourlyReport(
                year: nil, clients: nil, priority: .userInitiated)
        }
        let sourcedAgents = awaitValue {
            try await demoSource.agentsReport(
                year: nil, clients: nil, priority: .userInitiated)
        }
        let sourcedQuota = awaitValue { try await demoSource.agentUsage() }
        let sourcedTrace = awaitValue { try await demoSource.usageTrace(windowSecs: 600) }
        let sourcedRate = awaitValue { try await demoSource.tokensPerMin() }
        expect(
            sourcedPayload?.summary.totalTokens == demoPayload.summary.totalTokens
                && sourcedRefresh?.summary.totalCost == demoPayload.summary.totalCost,
            "demo source graph and refresh read synthetic data")
        expect(
            sourcedModels?.entries.isEmpty == false && sourcedHourly?.entries.isEmpty == false
                && sourcedAgents?.entries.isEmpty == false && sourcedQuota?.agents.isEmpty == false
                && sourcedTrace?.isEmpty == false && (sourcedRate ?? 0) > 0,
            "demo source serves every usage API")

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
