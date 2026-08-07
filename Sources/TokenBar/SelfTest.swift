import AppKit
import Foundation
import SwiftUI
import TokenBarCore

// Logic checks for the pure TokenBarCore ports, run via `TokenBar --selftest`.
// Plain assertions instead of swift-testing/XCTest because the dev machine has
// Command Line Tools only (no testing modules); CI runs this the same way.

private final class AsyncResultBox<Value: Sendable>: @unchecked Sendable {
    var result: Result<Value, Error>?
}

private actor ControlledTurnUsageDataSource: UsageDataSource {
    private struct PendingHourly {
        let clients: [String]?
        let continuation: CheckedContinuation<HourlyReport, Never>
    }

    nonisolated let allowsQuotaCachePersistence = false

    private let hourlyResponses: [Set<String>: HourlyReport]
    private var blockedGraphYears: Set<String> = []
    private var blockedHourlyYears: Set<String> = []
    /// Model-report call log. LP2B's claim is about *when* the model is
    /// requested relative to the graph, so the count alone is not enough —
    /// `modelCallsWhileGraphPending` records requests that arrived while a
    /// graph was still blocked, which is exactly the contention being removed.
    private var modelCalls = 0
    private var modelCallsWhileGraphPending = 0
    private var blockedModel = false
    private var pendingModel: [CheckedContinuation<ModelReport, Never>] = []
    private var pendingModelYears: [String?] = []
    /// When set, each graph response advances the fixture's "today" so
    /// `meta.generatedAt` actually moves between loads. Without this the demo
    /// payload is byte-identical across a refresh, the model identity gate
    /// always matches, and any test about per-generation refetching would pass
    /// no matter what the production code does.
    private var advancingGraphDays: Int?
    private var pendingGraphs: [String: [CheckedContinuation<UsagePayload, Never>]] = [:]
    private var pendingHourly: [String: [PendingHourly]] = [:]

    init(hourlyResponses: [Set<String>: HourlyReport] = [:]) {
        self.hourlyResponses = hourlyResponses
    }

    private static func key(_ year: String?) -> String { year ?? "" }

    func blockGraph(year: String?) { blockedGraphYears.insert(Self.key(year)) }
    func blockHourly(year: String?) { blockedHourlyYears.insert(Self.key(year)) }

    func hasPendingGraph(year: String?) -> Bool {
        !(pendingGraphs[Self.key(year)] ?? []).isEmpty
    }

    func hasPendingHourly(year: String?) -> Bool {
        !(pendingHourly[Self.key(year)] ?? []).isEmpty
    }

    func releaseGraph(year: String?) {
        let key = Self.key(year)
        blockedGraphYears.remove(key)
        let continuations = pendingGraphs.removeValue(forKey: key) ?? []
        let payload = DemoData.payload(for: year)
        continuations.forEach { $0.resume(returning: payload) }
    }

    func releaseHourly(year: String?) {
        let key = Self.key(year)
        blockedHourlyYears.remove(key)
        let pending = pendingHourly.removeValue(forKey: key) ?? []
        pending.forEach {
            let report = hourlyResponses[Set($0.clients ?? [])]
                ?? DemoData.hourlyReport(for: year, clients: $0.clients)
            $0.continuation.resume(returning: report)
        }
    }

    func graph(year: String?, priority: TaskPriority) async throws -> UsagePayload {
        _ = priority
        let key = Self.key(year)
        if blockedGraphYears.contains(key) {
            return await withCheckedContinuation { pendingGraphs[key, default: []].append($0) }
        }
        if let day = advancingGraphDays {
            advancingGraphDays = day + 1
            return DemoData.payload(for: year, today: Self.fixtureDay(day))
        }
        return DemoData.payload(for: year)
    }

    /// Deterministic fixture dates far from any real "today" so the advancing
    /// sequence cannot collide with the default fixture.
    private static func fixtureDay(_ offset: Int) -> String {
        String(format: "2037-06-%02d", 10 + offset)
    }

    func advanceGraphGenerationPerCall() { advancingGraphDays = 0 }

    func refreshGraph(year: String?, priority: TaskPriority) async throws -> UsagePayload {
        try await graph(year: year, priority: priority)
    }

    func modelReport(year: String?, priority: TaskPriority) async throws -> ModelReport {
        _ = priority
        modelCalls += 1
        if !pendingGraphs.values.allSatisfy(\.isEmpty) || !blockedGraphYears.isEmpty {
            modelCallsWhileGraphPending += 1
        }
        if blockedModel {
            pendingModelYears.append(year)
            return await withCheckedContinuation { pendingModel.append($0) }
        }
        return DemoData.modelReport(for: year)
    }

    func modelCallCount() -> Int { modelCalls }
    func modelCallsRacingGraph() -> Int { modelCallsWhileGraphPending }
    func blockModel() { blockedModel = true }
    func pendingModelCount() -> Int { pendingModel.count }

    /// Retire the oldest parked model request while leaving the rest parked —
    /// needed to reproduce a stale scan completing underneath a live one.
    func releaseOneModel() {
        guard !pendingModel.isEmpty else { return }
        let continuation = pendingModel.removeFirst()
        let year = pendingModelYears.removeFirst()
        continuation.resume(returning: DemoData.modelReport(for: year))
    }

    func releaseModel() {
        blockedModel = false
        let waiting = pendingModel
        let years = pendingModelYears
        pendingModel = []
        pendingModelYears = []
        for (index, continuation) in waiting.enumerated() {
            continuation.resume(returning: DemoData.modelReport(for: years[index]))
        }
    }

    func hourlyReport(
        year: String?, clients: [String]?, priority: TaskPriority
    ) async throws -> HourlyReport {
        _ = priority
        let key = Self.key(year)
        if blockedHourlyYears.contains(key) {
            return await withCheckedContinuation {
                pendingHourly[key, default: []].append(
                    PendingHourly(clients: clients, continuation: $0))
            }
        }
        return hourlyResponses[Set(clients ?? [])]
            ?? DemoData.hourlyReport(for: year, clients: clients)
    }

    func agentsReport(
        year: String?, clients: [String]?, priority: TaskPriority
    ) async throws -> AgentsReport {
        _ = priority
        return DemoData.agentsReport(for: year, clients: clients)
    }

    func agentUsage() async throws -> AgentUsagePayload { DemoData.agentUsage }

    func usageTrace(windowSecs: Int64) async throws -> [TraceBucket] {
        DemoData.trace(windowSecs: windowSecs)
    }

    func tokensPerMin() async throws -> Double { DemoData.tokensPerMin }
}

private func waitUntil(
    _ predicate: @escaping @Sendable () async -> Bool
) async -> Bool {
    for _ in 0..<1_000 {
        if await predicate() { return true }
        await Task.yield()
    }
    return false
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

        func awaitMainActorValue<Value: Sendable>(
            _ operation: @escaping @MainActor @Sendable () async throws -> Value
        ) -> Value? {
            let box = AsyncResultBox<Value>()
            Task { @MainActor in
                do {
                    box.result = .success(try await operation())
                } catch {
                    box.result = .failure(error)
                }
            }
            while box.result == nil {
                RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.001))
            }
            return try? box.result?.get()
        }

        expect(
            !AppLanguage.requiresRelaunch(from: "en", to: "en"),
            "language reselect does not prompt for relaunch")
        expect(
            AppLanguage.requiresRelaunch(from: "en", to: "zh-Hant"),
            "language change prompts for relaunch")
        expect(
            !AppLanguage.requiresRelaunch(from: "en", to: "unsupported"),
            "invalid language does not prompt for relaunch")

        let popoverResizeResult = MainActor.assumeIsolated { () -> (Bool, Bool) in
            let defaults = UserDefaults.standard
            let savedHeight = defaults.object(forKey: PopoverChrome.heightKey)
            defer {
                if let savedHeight {
                    defaults.set(savedHeight, forKey: PopoverChrome.heightKey)
                } else {
                    defaults.removeObject(forKey: PopoverChrome.heightKey)
                }
            }
            defaults.set(620.0, forKey: PopoverChrome.heightKey)
            let chrome = PopoverChrome()
            chrome.resolve(visibleHeight: 1_000)
            var liveResize: (CGFloat, Bool)?
            chrome.onResize = { liveResize = ($0, $1) }
            chrome.setHeight(700, persist: false, live: true)
            let liveBypassedPublication =
                chrome.rawHeight == 620 && liveResize?.0 == 700 && liveResize?.1 == true
            chrome.setHeight(700, persist: false, live: false)
            return (liveBypassedPublication, chrome.rawHeight == 700)
        }
        expect(
            popoverResizeResult.0,
            "live popover resize bypasses environment publication")
        expect(
            popoverResizeResult.1,
            "final popover resize commits published height")

        // Tray animation timing: preserve the shipping integer-millisecond
        // cadence while mapping the runner rate from 2 to 40 fps.
        let idleLoad = TrayAnimator.animationLoad(tokensPerMinute: 0)
        let thresholdLoad = TrayAnimator.animationLoad(tokensPerMinute: 50_000)
        let mediumLoad = TrayAnimator.animationLoad(tokensPerMinute: 100_000)
        let quantizedLoad = TrayAnimator.animationLoad(tokensPerMinute: 333_000)
        let fullLoad = TrayAnimator.animationLoad(tokensPerMinute: 1_000_000)
        let clampedLoad = TrayAnimator.animationLoad(tokensPerMinute: 2_000_000)
        expect(TrayAnimator.effectiveAnimationFPS(load: idleLoad) == 2, "tray idle is 2 fps")
        expect(TrayAnimator.effectiveAnimationFPS(load: thresholdLoad) == 2, "tray 50K threshold is 2 fps")
        expect(TrayAnimator.effectiveAnimationFPS(load: mediumLoad) == 4, "tray 100K is 4 fps")
        expect(
            TrayAnimator.animationIntervalMilliseconds(load: quantizedLoad) == 75,
            "tray cadence preserves integer-ms quantization")
        expect(TrayAnimator.effectiveAnimationFPS(load: fullLoad) == 40, "tray 1M is 40 fps")
        expect(TrayAnimator.effectiveAnimationFPS(load: clampedLoad) == 40, "tray speed clamps at 40 fps")
        expect(TrayAnimator.baseAnimationDuration(frameCount: 5) == 2.5, "tray five-frame base duration")
        expect(TrayAnimator.baseAnimationDuration(frameCount: 10) == 5, "tray ten-frame base duration")

#if DEBUG
        let trayFrameURL = Bundle.tokenBarResources.url(
            forResource: "frame-00",
            withExtension: "png",
            subdirectory: "anim-cat2"
        )
        let trayFrame = trayFrameURL.flatMap(NSImage.init(contentsOf:))
        trayFrame?.size = NSSize(width: 18, height: 18)
        let oneX = trayFrame.flatMap {
            StatusItemAnimationSurface.rasterizedFrameMetricsForTesting(
                $0,
                scale: 1
            )
        }
        let twoX = trayFrame.flatMap {
            StatusItemAnimationSurface.rasterizedFrameMetricsForTesting(
                $0,
                scale: 2
            )
        }
        expect(
            oneX?.pixelSize == CGSize(width: 18, height: 18),
            "tray 1x raster is 18 pixels"
        )
        expect(
            twoX?.pixelSize == CGSize(width: 36, height: 36),
            "tray 2x raster is 36 pixels"
        )
        expect(
            twoX.map { $0.alphaBounds.width } ?? 0
                >= (oneX.map { $0.alphaBounds.width } ?? .infinity) * 1.8
                && (twoX.map { $0.alphaBounds.height } ?? 0)
                    >= (oneX.map { $0.alphaBounds.height } ?? .infinity) * 1.8,
            "tray 2x raster preserves logical alpha coverage"
        )
#endif

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
        let resetFormatter = ISO8601DateFormatter()
        let resetAt = resetFormatter.string(from: now.addingTimeInterval(1_801))
        expect(
            UsagePace.resetText(for: resetAt, now: now) == "Resets in 31m",
            "reset countdown uses structured timestamp and ceil-minute rounding")
        let resetNow = resetFormatter.string(from: now.addingTimeInterval(-1))
        expect(
            UsagePace.resetText(for: resetNow, now: now) == "Resets now",
            "expired reset countdown is localized")

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
        let currentFitReset = resetFormatter.string(from: now.addingTimeInterval(10_800))
        let currentFitJSON = """
        {"cardId":"current-fit.v1","label":"Current fit","usedPercent":36,"remainingPercent":64,
         "resetsAt":"\(currentFitReset)","windowMinutes":300,
         "paceStatus":{"state":"available","windowKey":"current-fit.v1",
         "durationSeconds":18000,"durationSource":"provider","completeCycles":0},
         "historicalPace":{"expectedUsedPercent":30,"etaSeconds":5400,
         "willLastToReset":false}}
        """
        let unavailableJSON = """
        {"cardId":"extra_usage.v1","label":"Extra usage","usedPercent":70,"remainingPercent":30,
         "paceStatus":{"state":"unavailable","windowKey":"extra_usage.v1",
         "completeCycles":0,"reason":"nonRecurring"}}
        """
        let learningDuration = decodeWindow(learningDurationJSON)
        let learningHistory = decodeWindow(learningHistoryJSON)
        let available = decodeWindow(availableJSON)
        let currentFit = decodeWindow(currentFitJSON)
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
            currentFit?.paceStatus.state == UsagePaceState.available &&
                currentFit?.paceStatus.completeCycles == 0 &&
                currentFit?.durationSeconds == 18_000 &&
                currentFit?.historicalPace?.expectedUsedPercent == 30 &&
                currentFit?.historicalPace?.runOutProbability == nil,
            "v3 current fit decodes zero-cycle historical result")
        let currentFitPace = currentFit.flatMap {
            UsagePace.compute(window: $0, mode: .historical, now: now)
        }
        expect(
            currentFitPace?.basis == .historical &&
                currentFitPace?.actualUsedPercent == 36 &&
                currentFitPace?.expectedUsedPercent == 30 &&
                currentFitPace?.etaSeconds == 5_400 &&
                currentFitPace?.willLastToReset == false,
            "zero-cycle current fit uses backend historical projection")
        let currentFitRisk = currentFit.flatMap {
            runOutRiskLabel(window: $0, pace: currentFitPace)
        }
        expect(currentFitRisk == nil, "zero-cycle current fit keeps partial risk absent")
        expect(
            currentFitPace?.expectedUsedPercent != 40,
            "zero-cycle current fit does not fall back to Linear")
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
           "windows":[\(learningDurationJSON),\(learningHistoryJSON),\(availableJSON),\(currentFitJSON),\(unavailableJSON)],
           "credits":{"remaining":12.5,"unlimited":false},"error":null}
        ],"opencodeSubscriptions":["Codex"]}
        """
        let productionPayload = try? JSONDecoder().decode(
            AgentUsagePayload.self, from: Data(productionPayloadJSON.utf8))
        expect(
            productionPayload?.agents.count == 1 &&
                productionPayload?.agents.first?.windows.count == 5 &&
                productionPayload?.agents.first?.windows[2].paceStatus.state == .available &&
                productionPayload?.agents.first?.windows[3].paceStatus.state == .available &&
                productionPayload?.agents.first?.windows[3].paceStatus.completeCycles == 0 &&
                productionPayload?.agents.first?.windows[3].historicalPace?.expectedUsedPercent == 30,
            "complete AgentUsagePayload v3 shape decodes zero-cycle current fit")

        // Rust's publication gate orders generations, while the Swift state
        // guards apply order shared by popover and Settings models.
        let publicationAJSON = """
        {"generatedAt":"A","publicationGeneration":1,"agents":[
          {"clientId":"codex","source":"oauth","updatedAt":"A",
           "windows":[{"cardId":"session.v1","label":"Session","usedPercent":12,"remainingPercent":88}],
           "error":null}]}
        """
        let publicationBJSON = """
        {"generatedAt":"B","publicationGeneration":2,"agents":[
          {"clientId":"codex","source":"oauth","updatedAt":"B",
           "windows":[],"error":"terminal B"}]}
        """
        let legacyPublicationJSON = """
        {"generatedAt":"legacy","agents":[
          {"clientId":"codex","source":"oauth","updatedAt":"legacy",
           "windows":[],"error":"legacy payload"}]}
        """
        let publicationA = try! JSONDecoder().decode(
            AgentUsagePayload.self, from: Data(publicationAJSON.utf8))
        let publicationB = try! JSONDecoder().decode(
            AgentUsagePayload.self, from: Data(publicationBJSON.utf8))
        let legacyPublication = try! JSONDecoder().decode(
            AgentUsagePayload.self, from: Data(legacyPublicationJSON.utf8))

        var stalePublicationState = AgentUsagePublicationState()
        _ = stalePublicationState.resolve(publicationB)
        let staleResult = stalePublicationState.resolve(publicationA)
        expect(
            staleResult.publicationGeneration == 2 &&
                staleResult.agents.first?.error == "terminal B" &&
                staleResult.agents.first?.windows.isEmpty == true,
            "stale generation resolves to newer terminal payload content")

        var orderedPublicationState = AgentUsagePublicationState()
        let firstResult = orderedPublicationState.resolve(publicationA)
        let secondResult = orderedPublicationState.resolve(publicationB)
        expect(
            firstResult.publicationGeneration == 1 &&
                firstResult.agents.first?.windows.first?.usedPercent == 12,
            "generation 1 success applies its own payload")
        expect(
            secondResult.publicationGeneration == 2 &&
                secondResult.agents.first?.error == "terminal B",
            "generation 2 terminal replaces generation 1")

        let legacyResult = stalePublicationState.resolve(legacyPublication)
        expect(
            legacyResult.publicationGeneration == nil &&
                legacyResult.agents.first?.error == "legacy payload",
            "legacy payload passes through without touching generation state")
        let afterLegacy = stalePublicationState.resolve(publicationA)
        expect(
            afterLegacy.publicationGeneration == 2 &&
                afterLegacy.agents.first?.error == "terminal B",
            "legacy payload does not lower generated publication state")

        func settingsQuotaPayload(generation: UInt64?, remaining: Double) -> AgentUsagePayload {
            let generationField = generation.map { #","publicationGeneration":\#($0)"# } ?? ""
            let json = """
            {"generatedAt":"same"\(generationField),"agents":[
              {"clientId":"codex","source":"oauth","updatedAt":"same",
               "windows":[{"cardId":"session.v1","label":"Session","usedPercent":\(100 - remaining),"remainingPercent":\(remaining)}]}]}
            """
            return try! JSONDecoder().decode(AgentUsagePayload.self, from: Data(json.utf8))
        }
        let settingsGeneration41 = settingsQuotaPayload(generation: 41, remaining: 80)
        let settingsGeneration42 = settingsQuotaPayload(generation: 42, remaining: 80)
        let settingsGeneration41ID = SettingsWindowView.quotaReconciliationID(
            payload: settingsGeneration41,
            persistedSelection: "codex|session.v1",
            excluding: [],
            exclusionSignature: "")
        let settingsGeneration42ID = SettingsWindowView.quotaReconciliationID(
            payload: settingsGeneration42,
            persistedSelection: "codex|session.v1",
            excluding: [],
            exclusionSignature: "")
        expect(
            settingsGeneration41ID != settingsGeneration42ID,
            "Settings reconciliation identity tracks publication generation")

        let settingsLegacy80 = settingsQuotaPayload(generation: nil, remaining: 80)
        let settingsLegacy20 = settingsQuotaPayload(generation: nil, remaining: 20)
        let settingsLegacy80ID = SettingsWindowView.quotaReconciliationID(
            payload: settingsLegacy80,
            persistedSelection: "codex|session.v1",
            excluding: [],
            exclusionSignature: "")
        let settingsLegacy20ID = SettingsWindowView.quotaReconciliationID(
            payload: settingsLegacy20,
            persistedSelection: "codex|session.v1",
            excluding: [],
            exclusionSignature: "")
        expect(
            settingsLegacy80ID != settingsLegacy20ID,
            "Settings legacy reconciliation identity fingerprints resolved quota")
        expect(
            settingsGeneration41ID != SettingsWindowView.quotaReconciliationID(
                payload: settingsGeneration41,
                persistedSelection: "codex|weekly.v1",
                excluding: [],
                exclusionSignature: "") &&
                settingsGeneration41ID != SettingsWindowView.quotaReconciliationID(
                    payload: settingsGeneration41,
                    persistedSelection: "codex|session.v1",
                    excluding: ["codex"],
                    exclusionSignature: "codex"),
            "Settings reconciliation identity tracks selection and exclusions")

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
          {"clientId":"antigravity","source":"oauth","updatedAt":"now","windows":[],
           "error":"Antigravity OAuth client was not found."},
          {"clientId":"grok","source":"oauth","updatedAt":"now",
           "windows":[{"cardId":"billing.weekly.v1","label":"Weekly","usedPercent":99,"remainingPercent":1}],
           "error":"Grok request timed out.",
           "transportDiagnostic":{"category":"timeout"}}
        ]}
        """
        let quotaPayload = try! JSONDecoder().decode(
            AgentUsagePayload.self, from: Data(quotaJSON.utf8))

        // Individual client trays: bounded persistence, client-local quota
        // resolution, eligibility, display privacy, and static icon rendering.
        let clientGraphJSON = """
        {"meta":{"generatedAt":"now","version":"1","dateRange":{"start":"2026-07-01","end":"2026-07-01"}},
         "summary":{"totalTokens":0,"totalCost":0,"totalDays":0,"activeDays":0,"averagePerDay":0,
                    "maxCostInSingleDay":0,"clients":["claude","codex","grok"],"models":[]},
         "years":[],"contributions":[]}
        """
        let clientGraph = try! JSONDecoder().decode(
            UsagePayload.self, from: Data(clientGraphJSON.utf8))
        let officialClientIDs = MainActor.assumeIsolated {
            AgentIconView.availableOfficialClientIDs()
        }
        let registeredOfficialClientIDs = MainActor.assumeIsolated {
            AgentIconView.officialClientIDs
        }
        expect(
            officialClientIDs == registeredOfficialClientIDs,
            "every official registry id has a loadable brand asset")
        expect(
            officialClientIDs.contains("antigravity-cli")
                && officialClientIDs.contains("kilo")
                && !officialClientIDs.contains("junie"),
            "icon aliases are official while fallback-only clients are not")
        let renderedBrandImage = MainActor.assumeIsolated {
            AgentIconView.statusItemImage(clientId: "claude")
        }
        let brandReps = renderedBrandImage?.representations.compactMap { $0 as? NSBitmapImageRep } ?? []
        expect(
            renderedBrandImage?.isTemplate == false && brandReps.count == 2
                && brandReps.map(\.pixelsWide) == [18, 36]
                && brandReps.map(\.pixelsHigh) == [18, 36],
            "official status icon has fixed 1x and 2x representations")
        expect(
            MainActor.assumeIsolated {
                AgentIconView.statusItemImage(clientId: "junie") == nil
                    && AgentIconView.statusItemImage(clientId: "unknown") == nil
            },
            "fallback-only and unknown clients cannot create status icons")

        let clientDefaultsName = "TokenBar.SelfTest.ClientTray.\(UUID().uuidString)"
        if let clientDefaults = UserDefaults(suiteName: clientDefaultsName) {
            defer { clientDefaults.removePersistentDomain(forName: clientDefaultsName) }
            expect(ClientTray.enabled(defaults: clientDefaults).isEmpty, "individual trays default off")
            expect(
                ClientTray.canonicalEnabledRaw(["codex", "claude"]) == "claude,codex",
                "enabled clients serialize as deterministic sorted CSV")
            expect(
                ClientTray.enabledRaw(
                    updating: "claude", clientId: "codex", enabled: true)
                    == "claude,codex"
                    && ClientTray.enabledRaw(
                        updating: "claude,codex", clientId: "codex", enabled: false)
                        == "claude",
                "Settings can commit enabled state through its observed AppStorage raw")

            func boundedClientID(_ index: Int, length: Int) -> String {
                let prefix = "c\(index)_"
                return prefix + String(repeating: "a", count: length - prefix.utf8.count)
            }
            var boundaryEnabledIDs = (0 ..< 64).map { boundedClientID($0, length: 127) }
            boundaryEnabledIDs[0] = boundedClientID(0, length: 128)
            let boundaryEnabledRaw = ClientTray.canonicalEnabledRaw(Set(boundaryEnabledIDs))
            expect(
                boundaryEnabledRaw?.utf8.count == ClientTray.maxEnabledRawBytes
                    && boundaryEnabledRaw.map(ClientTray.parseEnabledRaw)?.count == 64,
                "enabled codec accepts an exact raw-byte boundary")
            expect(
                boundaryEnabledRaw.map { ClientTray.parseEnabledRaw($0 + "a").isEmpty } == true,
                "enabled codec rejects one byte over the raw boundary")
            let outgoingOversizedEnabled = Set(
                (0 ..< 65).map { boundedClientID($0, length: 128) })
            expect(
                ClientTray.canonicalEnabledRaw(outgoingOversizedEnabled) == nil,
                "enabled codec refuses to serialize an oversized valid set")

            let selectionRaw = ClientTray.selectionsRaw(
                updating: "{\"codex\":\"weekly.v1\"}",
                clientId: "claude", selection: "model.gpt|preview.v1")
            expect(
                selectionRaw == "{\"claude\":\"model.gpt|preview.v1\",\"codex\":\"weekly.v1\"}",
                "selection map uses deterministic sorted JSON")
            clientDefaults.set(selectionRaw, forKey: ClientTray.selectionsKey)
            expect(
                ClientTray.selections(defaults: clientDefaults)["claude"] == "model.gpt|preview.v1",
                "selection codec preserves card delimiters exactly")
            expect(
                ClientTray.selectionsRaw(
                    updating: "{}", clientId: "codex", selection: "weekly.v1")
                    == "{\"codex\":\"weekly.v1\"}",
                "Settings can commit selection state through its observed AppStorage raw")

            var boundarySelections = Dictionary(uniqueKeysWithValues: (0 ..< 15).map {
                ("c\($0)", String(repeating: "x", count: ClientTray.maxCardIDBytes))
            })
            boundarySelections["final"] = "x"
            let initialBoundaryData = try! JSONSerialization.data(
                withJSONObject: boundarySelections, options: [.sortedKeys])
            let boundaryPadding = ClientTray.maxSelectionRawBytes - initialBoundaryData.count
            boundarySelections["final"] = String(repeating: "x", count: 1 + boundaryPadding)
            let boundarySelectionRaw = ClientTray.canonicalSelectionsRaw(boundarySelections)
            expect(
                boundarySelectionRaw?.utf8.count == ClientTray.maxSelectionRawBytes
                    && boundarySelectionRaw.map(ClientTray.parseSelectionsRaw)?.count == 16,
                "selection codec accepts an exact raw-byte boundary")
            expect(
                boundarySelectionRaw.map {
                    ClientTray.parseSelectionsRaw($0 + " ").isEmpty
                } == true,
                "selection codec rejects one byte over the raw boundary")
            var outgoingOversizedSelections = boundarySelections
            outgoingOversizedSelections["overflow"] = String(
                repeating: "x", count: ClientTray.maxCardIDBytes)
            expect(
                ClientTray.canonicalSelectionsRaw(outgoingOversizedSelections) == nil,
                "selection codec refuses to serialize an oversized valid map")

            let tooManyEnabled = Set((0 ..< ClientTray.maxEntries + 1).map { "c\($0)" })
            expect(
                ClientTray.canonicalEnabledRaw(tooManyEnabled) == nil,
                "over-limit enabled mutation does not serialize")
            let oversizedEnabled = String(repeating: "a,", count: ClientTray.maxEnabledRawBytes)
            clientDefaults.set(oversizedEnabled, forKey: ClientTray.enabledKey)
            expect(
                ClientTray.enabled(defaults: clientDefaults).isEmpty,
                "oversized enabled input fails closed before splitting")
            expect(
                ClientTray.enabledRaw(
                    updating: oversizedEnabled, clientId: "claude", enabled: true) == nil,
                "oversized enabled input is not repaired by write-back")

            let mixedSelections = """
            {"claude":"auto","Codex":"bad","codex":42,"unknown":"bounded"}
            """
            clientDefaults.set(mixedSelections, forKey: ClientTray.selectionsKey)
            expect(
                ClientTray.selections(defaults: clientDefaults) == [
                    "claude": "auto", "unknown": "bounded"],
                "selection codec drops invalid entries but preserves bounded unknown ids")
            clientDefaults.set(42, forKey: ClientTray.selectionsKey)
            expect(
                ClientTray.selections(defaults: clientDefaults).isEmpty,
                "non-string selection defaults fail closed")

            var oversizedRoot: [String: String] = [:]
            for index in 0 ..< ClientTray.maxEntries + 1 {
                oversizedRoot["c\(index)"] = "card\(index)"
            }
            let oversizedRootData = try! JSONSerialization.data(
                withJSONObject: oversizedRoot, options: [.sortedKeys])
            let oversizedRootRaw = String(data: oversizedRootData, encoding: .utf8)!
            clientDefaults.set(oversizedRootRaw, forKey: ClientTray.selectionsKey)
            expect(
                ClientTray.selections(defaults: clientDefaults).isEmpty,
                "selection root over the entry cap fails closed")
            expect(
                ClientTray.selectionsRaw(
                    updating: oversizedRootRaw, clientId: "claude", selection: "auto") == nil,
                "over-limit selection root is not written back")
        } else {
            expect(false, "isolated individual-tray defaults suite is available")
        }

        expect(
            ClientTray.resolveWindow(
                payload: quotaPayload, clientId: "claude", selection: ClientTray.autoSelection
            )?.cardId == "session.v1",
            "client Auto resolves only that client's tightest healthy window")
        expect(
            ClientTray.resolveWindow(
                payload: quotaPayload, clientId: "grok", selection: ClientTray.autoSelection
            ) == nil,
            "client Auto rejects an unhealthy snapshot")
        expect(
            ClientTray.resolveWindow(
                payload: quotaPayload, clientId: "grok", selection: "billing.weekly.v1"
            )?.remainingPercent == 1,
            "explicit client selection accepts an error snapshot fallback")
        expect(
            ClientTray.resolveWindow(
                payload: quotaPayload, clientId: "codex", selection: "missing.v1"
            ) == nil
                && ClientTray.resolveWindow(
                    payload: quotaPayload, clientId: "claude", selection: "weekly.v1")?.cardId
                    != "missing.v1",
            "missing explicit cards never fall back across cards or clients")
        expect(
            ClientTray.percentText(-2) == "0%"
                && ClientTray.percentText(101) == "100%"
                && ClientTray.percentText(.nan) == "—%"
                && ClientTray.percentText(nil) == "—%",
            "client percentage presentation clamps finite values and fails closed")
        expect(
            ClientTray.quotaClientID(for: "antigravity-cli") == "antigravity"
                && ClientTray.processIdentity(for: "antigravity")
                    != ClientTray.processIdentity(for: "antigravity-cli")
                && ClientTray.autosaveName(for: "kilo")
                    != ClientTray.autosaveName(for: "kilocode"),
            "quota lookup aliases never collide in process or placement identity")
        let routeMemory = StatusItemRouteMemory(
            mainClient: ClientTray.overviewTab, mainView: AppView.monthly.rawValue)
        let firstClaudeRoute = routeMemory.activateClient(
            "claude", currentClient: ClientTray.overviewTab,
            currentView: AppView.monthly.rawValue)
        routeMemory.record(clientId: "claude", view: AppView.models.rawValue)
        let firstCodexRoute = routeMemory.activateClient(
            "codex", currentClient: "claude", currentView: AppView.models.rawValue)
        routeMemory.record(clientId: "codex", view: AppView.hourly.rawValue)
        let restoredClaudeRoute = routeMemory.activateClient(
            "claude", currentClient: "codex", currentView: AppView.hourly.rawValue)
        let restoredMainRoute = routeMemory.activateMain(
            currentClient: "claude", currentView: restoredClaudeRoute.view)
        let mainCodexRoute = routeMemory.switchClient(
            from: restoredMainRoute.clientId, currentView: restoredMainRoute.view, to: "codex")
        _ = routeMemory.activateClient(
            "claude", currentClient: mainCodexRoute.clientId, currentView: mainCodexRoute.view)
        let mainAgainRoute = routeMemory.activateMain(
            currentClient: "claude", currentView: AppView.models.rawValue)
        expect(
            ClientTray.activeViewKey == "tokenbar.view"
                && firstClaudeRoute == .init(
                    clientId: "claude", view: AppView.overview.rawValue)
                && firstCodexRoute == .init(
                    clientId: "codex", view: AppView.overview.rawValue)
                && restoredClaudeRoute.view == AppView.models.rawValue
                && restoredMainRoute == .init(
                    clientId: ClientTray.overviewTab, view: AppView.monthly.rawValue)
                // The main item keeps its OWN per-client lens: switching to a tab
                // it has not visited opens Overview, regardless of where the
                // individual Codex item was left (hourly, above).
                && mainCodexRoute == .init(
                    clientId: "codex", view: AppView.overview.rawValue)
                && mainAgainRoute == mainCodexRoute,
            "main and individual items restore independent process-lifetime routes")

        // A session that quit on a client tab persists that tab and lens for the
        // MAIN item. The first click on that client's own item must still open
        // Overview instead of inheriting the previous session's main lens.
        let persistedMainMemory = StatusItemRouteMemory(
            mainClient: "claude", mainView: AppView.models.rawValue)
        let firstItemVisit = persistedMainMemory.activateClient(
            "claude", currentClient: "claude", currentView: AppView.models.rawValue)
        let mainAfterItemVisit = persistedMainMemory.activateMain(
            currentClient: "claude", currentView: firstItemVisit.view)
        expect(
            firstItemVisit == .init(clientId: "claude", view: AppView.overview.rawValue)
                && mainAfterItemVisit == .init(
                    clientId: "claude", view: AppView.models.rawValue),
            "an unvisited client item ignores the persisted main lens and cannot clobber it")

        // The individual-items spinner must terminate. `stats`/`agentUsage` stay
        // nil when a fetch fails, so a payload-presence check would spin forever;
        // the gate is request lifecycle, and a failed phase is terminal.
        //
        // This table treats `.loading` as "a request is still in flight", which
        // holds only because DashboardModel settles phase on EVERY initial path —
        // including the stale-year recovery, where apply() clears the filter and
        // spawns an unfiltered reload before reaching `.ready`, and that reload's
        // failure branch has to move a never-ready model to `.failed`. If a new
        // path can leave phase on `.loading` with no request running, this
        // spinner silently becomes permanent again.
        expect(
            SettingsWindowView.isInitialLoad(phase: .loading, agentUsageAttempted: false)
                && SettingsWindowView.isInitialLoad(
                    phase: .loading, agentUsageAttempted: true)
                && SettingsWindowView.isInitialLoad(
                    phase: .ready, agentUsageAttempted: false)
                && !SettingsWindowView.isInitialLoad(
                    phase: .ready, agentUsageAttempted: true)
                && SettingsWindowView.isInitialLoad(
                    phase: .failed("boom"), agentUsageAttempted: false)
                && !SettingsWindowView.isInitialLoad(
                    phase: .failed("boom"), agentUsageAttempted: true),
            "the individual-items spinner ends once both initial requests settle, including failures")
        let settingsModelUsesAllTime = MainActor.assumeIsolated {
            DashboardModel(initialYear: nil).year == nil
        }
        expect(
            settingsModelUsesAllTime,
            "Settings can pin its client universe to the all-time graph")

        // Browsing a client inside the MAIN popover must not decide what that
        // client's own item opens on.
        let mainBrowsingMemory = StatusItemRouteMemory(
            mainClient: ClientTray.overviewTab, mainView: AppView.overview.rawValue)
        _ = mainBrowsingMemory.switchClient(
            from: ClientTray.overviewTab, currentView: AppView.overview.rawValue,
            to: "claude")
        mainBrowsingMemory.record(clientId: "claude", view: AppView.models.rawValue)
        expect(
            mainBrowsingMemory.activateClient(
                "claude", currentClient: "claude", currentView: AppView.models.rawValue
            ) == .init(clientId: "claude", view: AppView.overview.rawValue),
            "main-popover browsing never seeds an individual item's lens")

        let errorOnlyRows = ClientTray.settingsRows(
            presentClients: ["antigravity-cli"], payload: quotaPayload,
            enabled: [], selections: [:], hidden: [], orderRaw: "",
            officialClients: officialClientIDs)
        expect(
            errorOnlyRows.count == 1
                && errorOnlyRows[0].clientId == "antigravity-cli"
                && errorOnlyRows[0].status == .errorAuto
                && errorOnlyRows[0].valueText == "—%"
                && errorOnlyRows[0].options.map(\.tag) == [ClientTray.autoSelection],
            "error-only quota providers remain configurable while windows are unavailable")

        let normalRows = ClientTray.settingsRows(
            presentClients: ["codex", "claude"], payload: quotaPayload,
            enabled: ["codex"], selections: ["codex": "missing.v1"], hidden: [],
            orderRaw: "claude,codex", officialClients: officialClientIDs)
        expect(
            normalRows.map(\.clientId) == ["claude", "codex"],
            "Settings rows follow the existing client tab order")
        let missingRow = normalRows.first { $0.clientId == "codex" }
        expect(
            missingRow?.status == .missingSelection
                && missingRow?.options.last?.label == "Unavailable selection"
                && missingRow?.options.last?.tag == "missing.v1"
                && missingRow?.options.last?.isEnabled == false
                && missingRow?.valueText == "—%",
            "explicitly missing selection stays selected but unavailable")
        let hiddenRows = ClientTray.settingsRows(
            presentClients: ["codex"], payload: quotaPayload,
            enabled: ["codex"], selections: ["codex": "weekly.v1"], hidden: ["codex"],
            orderRaw: "", officialClients: officialClientIDs)
        expect(
            hiddenRows.first?.status == .suppressed && hiddenRows.first?.isEnabled == true,
            "hidden tabs preserve Settings enablement and selection")
        let errorAutoRow = ClientTray.settingsRows(
            presentClients: ["grok"], payload: quotaPayload,
            enabled: ["grok"], selections: ["grok": ClientTray.autoSelection], hidden: [],
            orderRaw: "", officialClients: officialClientIDs).first
        let errorExplicitRow = ClientTray.settingsRows(
            presentClients: ["grok"], payload: quotaPayload,
            enabled: ["grok"], selections: ["grok": "billing.weekly.v1"], hidden: [],
            orderRaw: "", officialClients: officialClientIDs).first
        expect(
            errorAutoRow?.status == .errorAuto && errorAutoRow?.valueText == "—%"
                && errorExplicitRow?.status == .errorExplicit
                && errorExplicitRow?.valueText == "1%",
            "error snapshots distinguish Auto from explicit fallback")
        expect(
            ClientTray.settingsRows(
                presentClients: ["codex"], payload: nil, enabled: ["codex"], selections: [:],
                hidden: [], orderRaw: "", officialClients: officialClientIDs).first?.status == .unavailable,
            "enabled rows survive a temporarily missing payload")
        let absentExplicitRow = ClientTray.settingsRows(
            presentClients: ["codex"], payload: nil, enabled: ["codex"],
            selections: ["codex": "missing.v1"], hidden: [], orderRaw: "",
            officialClients: officialClientIDs).first
        expect(
            absentExplicitRow?.status == .missingSelection
                && absentExplicitRow?.options.last?.label == "Unavailable selection"
                && absentExplicitRow?.options.last?.tag == "missing.v1",
            "missing payload keeps an explicit selection represented without exposing its id")
        expect(
            ClientTray.settingsRows(
                presentClients: ["claude"], payload: quotaPayload, enabled: ["codex"], selections: [:],
                hidden: [], orderRaw: "", officialClients: officialClientIDs).map(\.clientId) == ["claude"],
            "disabled clients disappear when no longer present while capable rows remain")
        expect(
            ClientTray.settingsRows(
                presentClients: [], payload: nil, enabled: [], selections: [:], hidden: [],
                orderRaw: "", officialClients: officialClientIDs).isEmpty,
            "Settings uses a fixed empty state when no rows are eligible")
        let runtimePresentations = ClientTray.runtimePresentations(
            graph: clientGraph, payload: quotaPayload, enabled: ["claude", "codex"],
            selections: ["codex": "weekly.v1"], hidden: ["claude"],
            officialClients: officialClientIDs)
        expect(
            runtimePresentations.map(\.clientId) == ["codex"]
                && runtimePresentations.first?.valueText == "35%",
            "runtime shells use graph presence, enabled state, official assets, and hidden tabs")
        let lastGoodRuntime = ClientTray.runtimePresentations(
            graph: clientGraph, payload: quotaPayload, enabled: ["grok"],
            selections: ["grok": "billing.weekly.v1"], hidden: [],
            officialClients: officialClientIDs).first
        expect(
            lastGoodRuntime?.status == .errorExplicit
                && lastGoodRuntime?.valueText == "1%"
                && lastGoodRuntime?.toolTip.contains("last known") == true
                && lastGoodRuntime?.accessibilityLabel.contains("last known") == true
                && lastGoodRuntime?.toolTip.contains("timed out") == false,
            "explicit error fallback is labeled as last-known quota, not current data")

        // Rows must not depend on Set iteration order. With no saved tab order
        // and no payload, every row comes from the preserved-enabled path, which
        // must follow the ordered `present` array.
        expect(
            ClientTray.settingsRows(
                presentClients: ["codex", "claude", "grok"], payload: nil,
                enabled: ["grok", "claude", "codex"], selections: [:], hidden: [],
                orderRaw: "", officialClients: officialClientIDs
            ).map(\.clientId) == ["codex", "claude", "grok"],
            "preserved enabled rows follow graph order, not enabled-set hash order")

        // A vanished explicit card is reported as a missing selection on the item
        // itself, not as a generic provider outage — the user has to change the
        // saved window, and waiting will not fix it. The raw card ID stays hidden.
        let missingRuntime = ClientTray.runtimePresentations(
            graph: clientGraph, payload: quotaPayload, enabled: ["codex"],
            selections: ["codex": "missing.v1"], hidden: [],
            officialClients: officialClientIDs).first
        expect(
            missingRuntime?.status == .missingSelection
                && missingRuntime?.valueText == "—%"
                && missingRuntime?.toolTip.contains("missing.v1") == false
                && missingRuntime?.accessibilityLabel.contains("missing.v1") == false
                && missingRuntime?.toolTip != ClientTray.runtimePresentations(
                    graph: clientGraph, payload: nil, enabled: ["codex"],
                    selections: [:], hidden: [],
                    officialClients: officialClientIDs).first?.toolTip,
            "a missing explicit card reads as an unavailable selection, not a provider outage")

        let sensitiveQuotaJSON = """
        {"generatedAt":"now","agents":[
          {"clientId":"codex","source":"SECRET_SOURCE","updatedAt":"now",
           "identity":{"email":"SECRET_IDENTITY","plan":"SECRET_PLAN"},
           "windows":[{"cardId":"SECRET_CARD","label":"SECRET_LABEL","usedPercent":1,"remainingPercent":99}],
           "error":"SECRET_ERROR"}
        ]}
        """
        let sensitiveQuota = try! JSONDecoder().decode(
            AgentUsagePayload.self, from: Data(sensitiveQuotaJSON.utf8))
        let sensitiveRow = ClientTray.settingsRows(
            presentClients: ["codex"], payload: sensitiveQuota, enabled: ["codex"],
            selections: ["codex": "SECRET_CARD"], hidden: [], orderRaw: "",
            officialClients: officialClientIDs).first!
        let sensitiveRuntime = ClientTray.runtimePresentations(
            graph: clientGraph, payload: sensitiveQuota, enabled: ["codex"],
            selections: ["codex": "SECRET_CARD"], hidden: [],
            officialClients: officialClientIDs).first!
        let sensitiveVisibleText = ([
            sensitiveRow.displayName,
            sensitiveRow.valueText,
            sensitiveRow.statusHint ?? "",
            sensitiveRow.accessibilityLabel,
        ] + sensitiveRow.options.filter(\.isEnabled).map(\.label)).joined(separator: "|")
        let sensitiveRuntimeText = [
            sensitiveRuntime.valueText,
            sensitiveRuntime.toolTip,
            sensitiveRuntime.accessibilityLabel,
        ].joined(separator: "|")
        expect(
            !sensitiveVisibleText.contains("SECRET_")
                && !sensitiveRuntimeText.contains("SECRET_"),
            "client Settings and status surfaces redact provider and card input")

        func transportPayload(_ body: String) -> AgentUsagePayload? {
            do {
                return try JSONDecoder().decode(
                    AgentUsagePayload.self,
                    from: Data("{\"generatedAt\":\"now\",\"agents\":[{\(body)}]}".utf8))
            } catch {
                return nil
            }
        }
        func transportEntries(_ body: String) -> [AgentUsageTransportLogEntry]? {
            transportPayload(body).map(agentUsageTransportLogEntries)
        }
        let transportBase = #""clientId":"codex","source":"fixture","updatedAt":"now","windows":[]"#
        expect(
            transportEntries(transportBase + #","error":"SECRET_ERROR""#)?.isEmpty == true,
            "transport diagnostics absent from sensitive error yields no candidates")
        let validTransport = transportEntries(
            transportBase + #","transportDiagnostic":{"category":"serverError","status":504,"osCode":-9806}"#)
        expect(
            validTransport?.count == 1 && validTransport?.first?.clientId == "codex"
                && validTransport?.first?.category == "serverError"
                && validTransport?.first?.status == 504 && validTransport?.first?.osCode == nil,
            "server-error diagnostic keeps status but drops osCode")
        let validRateLimited = transportEntries(
            transportBase + #","transportDiagnostic":{"category":"rateLimited","status":429,"osCode":-1}"#)
        expect(
            validRateLimited?.count == 1 && validRateLimited?.first?.status == 429
                && validRateLimited?.first?.osCode == nil,
            "rate-limit diagnostic accepts only status 429 without osCode")
        let contradictoryHTTP = transportEntries(
            transportBase + #","transportDiagnostic":{"category":"rateLimited","status":500,"osCode":-1}"#)
        let contradictoryServer = transportEntries(
            transportBase + #","transportDiagnostic":{"category":"serverError","status":429,"osCode":-1}"#)
        expect(
            contradictoryHTTP?.count == 1 && contradictoryHTTP?.first?.status == nil
                && contradictoryHTTP?.first?.osCode == nil
                && contradictoryServer?.count == 1 && contradictoryServer?.first?.status == nil
                && contradictoryServer?.first?.osCode == nil,
            "contradictory HTTP diagnostics keep category only")
        let crossFieldTransport = transportEntries(
            transportBase + #","transportDiagnostic":{"category":"timeout","status":504,"osCode":-9806}"#)
        expect(
            crossFieldTransport?.count == 1 && crossFieldTransport?.first?.status == nil
                && crossFieldTransport?.first?.osCode == -9806,
            "transport diagnostic rejects status on timeout but keeps osCode")
        let unknownTransport = transportEntries(
            transportBase.replacingOccurrences(of: "codex", with: "future-client") + #","transportDiagnostic":{"category":"futureCategory","status":200,"osCode":7}"#)
        expect(
            unknownTransport?.first?.clientId == "unknown"
                && unknownTransport?.first?.category == "unknown"
                && unknownTransport?.first?.status == nil
                && unknownTransport?.first?.osCode == nil,
            "unknown transport tuples drop associated numerics")
        let malformedTransportBodies = [
            transportBase + #","transportDiagnostic":"not-an-object""#,
            transportBase + #","transportDiagnostic":{"status":500}"#,
            transportBase + #","transportDiagnostic":{"category":42}"#,
        ]
        expect(
            malformedTransportBodies.allSatisfy { body in
                transportEntries(body)?.isEmpty == true
            },
            "malformed transport diagnostics decode with no candidates")
        let invalidNumeric = transportEntries(
            transportBase + #","transportDiagnostic":{"category":"dns","status":99,"osCode":2147483648}"#)
        expect(
            invalidNumeric?.count == 1 && invalidNumeric?.first?.status == nil
                && invalidNumeric?.first?.osCode == nil,
            "invalid transport numeric fields are retained as nil")
        let malformedOptionalInteger = transportEntries(
            transportBase + #","transportDiagnostic":{"category":"dns","status":"SECRET","osCode":7}"#)
        expect(
            malformedOptionalInteger?.count == 1
                && malformedOptionalInteger?.first?.category == "dns"
                && malformedOptionalInteger?.first?.status == nil
                && malformedOptionalInteger?.first?.osCode == 7,
            "malformed optional status drops only that field")
        let sensitiveTransport = transportEntries(
            #""clientId":"codex","source":"SECRET_SOURCE","updatedAt":"now","identity":{"email":"SECRET_IDENTITY","plan":"SECRET_PLAN"},"windows":[{"cardId":"SECRET_WINDOW","label":"SECRET_LABEL","usedPercent":1,"remainingPercent":99}],"error":"SECRET_ERROR","transportDiagnostic":{"category":"tls","status":502,"osCode":-1}"#)
        let sensitiveDescription = String(describing: sensitiveTransport ?? [])
        expect(
            !["SECRET_SOURCE", "SECRET_IDENTITY", "SECRET_PLAN", "SECRET_WINDOW",
              "SECRET_LABEL", "SECRET_ERROR"].contains { sensitiveDescription.contains($0) },
            "transport candidates exclude sensitive source identity window and error text")

        func failedBoundaryEvents(_ data: Data?) -> [AgentUsageBoundaryLogEvent]? {
            var events: [AgentUsageBoundaryLogEvent] = []
            do {
                _ = try TBCore.decodeAgentUsageBoundary(data) { events.append($0) }
                return nil
            } catch {
                return events
            }
        }
        let bridgeEvents = failedBoundaryEvents(Data(
            #"{"ok":false,"err":"SECRET_TOKEN https://example.invalid/?secret /private/credential"}"#.utf8))
        expect(
            bridgeEvents == [.bridgeFailed]
                && !String(describing: bridgeEvents).contains("SECRET_TOKEN"),
            "agent usage bridge failure logs only a fixed event")
        let decodeEvents = failedBoundaryEvents(Data(
            #"{"ok":true,"data":{"generatedAt":"SECRET_DECODE","agents":"not-an-array"}}"#.utf8))
        expect(
            decodeEvents == [.decodeFailed]
                && !String(describing: decodeEvents).contains("SECRET_DECODE"),
            "agent usage decode failure logs only a fixed event")
        expect(
            failedBoundaryEvents(Data(#"{"ok":true,"data":null}"#.utf8)) == [.decodeFailed],
            "agent usage successful envelope without data is a decode failure")
        expect(
            failedBoundaryEvents(Data(#"{not json"#.utf8)) == [.decodeFailed],
            "agent usage malformed envelope is a decode failure")
        expect(
            failedBoundaryEvents(nil) == [.returnedNull],
            "agent usage null pointer logs only a fixed event")

        var quotaApplyEvents: [String] = []
        MainActor.assumeIsolated {
            TrayAnimator.applyQuotaPayload(
                quotaPayload,
                store: { _ in quotaApplyEvents.append("store") },
                reconcile: { _ in quotaApplyEvents.append("reconcile") },
                persistSelection: { _ in quotaApplyEvents.append("persist") },
                render: { quotaApplyEvents.append("render") },
                notify: { quotaApplyEvents.append("notify") })
        }
        expect(
            quotaApplyEvents == ["store", "reconcile", "persist", "render", "notify"],
            "quota payload applies scalar state before render and notification")

        let suiteName = "TokenBar.SelfTest.PT0.\(UUID().uuidString)"
        if let defaults = UserDefaults(suiteName: suiteName) {
            let sentinelKey = "pt0.sentinel"
            defaults.set("keep", forKey: sentinelKey)
            let beforeFailure = defaults.persistentDomain(forName: suiteName)
            let failed = TrayAnimator.applyQuotaRemaining(
                payload: nil, persistedSelection: "codex|session.v1", excluding: [],
                cachedRemaining: 77, defaults: defaults)
            expect(
                failed == 77 && NSDictionary(dictionary: defaults.persistentDomain(forName: suiteName) ?? [:])
                    .isEqual(to: beforeFailure ?? [:]),
                "outer quota failure returns cached scalar without changing defaults")

            let fresh = TrayAnimator.applyQuotaRemaining(
                payload: quotaPayload, persistedSelection: "codex|session.v1", excluding: [],
                cachedRemaining: 77, defaults: defaults)
            expect(
                fresh == 80 && defaults.double(forKey: TrayAnimator.lastRemainingKey) == 80,
                "successful quota payload replaces cached scalar and defaults")

            let trayRaceRejected = MainActor.assumeIsolated { () -> Bool in
                let raceSuiteName = "TokenBar.SelfTest.PT0.Race.\(UUID().uuidString)"
                guard let raceDefaults = UserDefaults(suiteName: raceSuiteName) else { return false }
                defer { raceDefaults.removePersistentDomain(forName: raceSuiteName) }
                var remaining: Double? = fresh
                var generations: [UInt64?] = []
                for candidate in [publicationB, publicationA] {
                    TrayAnimator.applyQuotaPayload(
                        candidate,
                        store: { generations.append($0.publicationGeneration) },
                        reconcile: {
                            remaining = TrayAnimator.applyQuotaRemaining(
                                payload: $0,
                                persistedSelection: "codex|session.v1",
                                excluding: [],
                                cachedRemaining: remaining,
                                defaults: raceDefaults)
                        },
                        persistSelection: { _ in },
                        render: {},
                        notify: {})
                }
                return generations == [2, 2] && remaining == nil
                    && raceDefaults.object(forKey: TrayAnimator.lastRemainingKey) == nil
            }
            expect(
                trayRaceRejected,
                "late tray generation cannot revive a newer terminal scalar")

            let dashboardGeneration3 = settingsQuotaPayload(generation: 3, remaining: 80)
            let dashboardPublicationReachesTray = MainActor.assumeIsolated { () -> Bool in
                let dashboardSuiteName = "TokenBar.SelfTest.PT0.Dashboard.\(UUID().uuidString)"
                guard let dashboardDefaults = UserDefaults(suiteName: dashboardSuiteName) else {
                    return false
                }
                defer { dashboardDefaults.removePersistentDomain(forName: dashboardSuiteName) }
                let iconSignatureBefore = TrayAnimator.currentIconSignature(
                    defaults: dashboardDefaults)
                let resolved = AgentUsagePublicationCoordinator.resolve(dashboardGeneration3)
                let remaining = TrayAnimator.applyQuotaRemaining(
                    payload: resolved,
                    persistedSelection: "codex|session.v1",
                    excluding: [],
                    cachedRemaining: 20,
                    defaults: dashboardDefaults)
                let trayPayload = TrayAnimator.publishedQuota(publicationA)
                let iconSignatureAfter = TrayAnimator.currentIconSignature(
                    defaults: dashboardDefaults)
                return trayPayload?.publicationGeneration == 3
                    && trayPayload?.agents.first?.windows.first?.remainingPercent == 80
                    && remaining == 80
                    && dashboardDefaults.double(forKey: TrayAnimator.lastRemainingKey) == 80
                    && iconSignatureAfter != iconSignatureBefore
            }
            expect(
                dashboardPublicationReachesTray,
                "dashboard publication updates tray payload scalar and gauge invalidation")

            let unresolved = TrayAnimator.applyQuotaRemaining(
                payload: quotaPayload, persistedSelection: "missing|session.v1", excluding: [],
                cachedRemaining: fresh, defaults: defaults)
            let unresolvedRestarted = UserDefaults(suiteName: suiteName)
            let resumedAfterRestart = TrayAnimator.applyQuotaRemaining(
                payload: nil, persistedSelection: "missing|session.v1", excluding: [],
                cachedRemaining: unresolvedRestarted?.object(
                    forKey: TrayAnimator.lastRemainingKey) as? Double,
                defaults: unresolvedRestarted)
            expect(
                unresolved == nil && resumedAfterRestart == nil
                    && defaults.object(forKey: TrayAnimator.lastRemainingKey) == nil
                    && unresolvedRestarted?.object(forKey: TrayAnimator.lastRemainingKey) == nil,
                "same-generation unresolved selection clears scalar without restart revival")

            let replaced = TrayAnimator.applyQuotaRemaining(
                payload: quotaPayload, persistedSelection: "claude|session.v1", excluding: [],
                cachedRemaining: unresolved, defaults: defaults)
            expect(
                replaced == 12 && defaults.double(forKey: TrayAnimator.lastRemainingKey) == 12,
                "same-generation healthy selection replaces scalar")

            let hiddenAllAfterReplacement = TrayAnimator.applyQuotaRemaining(
                payload: quotaPayload, persistedSelection: QuotaResolver.auto,
                excluding: ["codex", "claude"], cachedRemaining: replaced, defaults: defaults)
            let hiddenAllRestarted = UserDefaults(suiteName: suiteName)
            expect(
                hiddenAllAfterReplacement == nil
                    && hiddenAllRestarted?.object(forKey: TrayAnimator.lastRemainingKey) == nil,
                "same-generation hidden-all clears replaced scalar across restart")

            let terminalPayload = try! JSONDecoder().decode(
                AgentUsagePayload.self,
                from: Data(#"{"generatedAt":"now","agents":[{"clientId":"codex","source":"oauth","updatedAt":"now","windows":[],"error":"Unauthorized"}]}"#.utf8))
            let terminalResult = TrayAnimator.applyQuotaRemaining(
                payload: terminalPayload, persistedSelection: QuotaResolver.auto, excluding: [],
                cachedRemaining: 80, defaults: defaults)
            let restarted = UserDefaults(suiteName: suiteName)
            expect(
                terminalResult == nil && defaults.object(forKey: TrayAnimator.lastRemainingKey) == nil
                    && restarted?.object(forKey: TrayAnimator.lastRemainingKey) == nil,
                "terminal empty provider payload clears scalar across restart")

            defaults.set(80, forKey: TrayAnimator.lastRemainingKey)
            let settingsTerminal = SettingsWindowView.applyQuotaRemaining(
                payload: terminalPayload,
                persistedSelection: QuotaResolver.auto,
                excluding: [],
                defaults: defaults)
            let settingsRestarted = UserDefaults(suiteName: suiteName)
            expect(
                settingsTerminal == nil
                    && defaults.object(forKey: TrayAnimator.lastRemainingKey) == nil
                    && settingsRestarted?.object(forKey: TrayAnimator.lastRemainingKey) == nil,
                "Settings terminal payload clears persisted scalar across restart")

            defaults.set(65, forKey: TrayAnimator.lastRemainingKey)
            let settingsBeforeFailure = defaults.persistentDomain(forName: suiteName)
            let settingsFailure = SettingsWindowView.applyQuotaRemaining(
                payload: nil,
                persistedSelection: QuotaResolver.auto,
                excluding: [],
                defaults: defaults)
            expect(
                settingsFailure == 65
                    && NSDictionary(dictionary: defaults.persistentDomain(forName: suiteName) ?? [:])
                        .isEqual(to: settingsBeforeFailure ?? [:]),
                "Settings outer quota failure preserves persisted scalar")

            let fallback = TrayAnimator.applyQuotaRemaining(
                payload: quotaPayload, persistedSelection: "grok|billing.weekly.v1", excluding: [],
                cachedRemaining: 80, defaults: defaults)
            expect(
                fallback == 1 && defaults.double(forKey: TrayAnimator.lastRemainingKey) == 1,
                "explicit selection keeps same-binding fallback window despite error")

            let errorOnlyPayload = try! JSONDecoder().decode(
                AgentUsagePayload.self,
                from: Data((#"{"generatedAt":"now","agents":[{"clientId":"grok","source":"oauth","updatedAt":"now","windows":[{"cardId":"billing.weekly.v1","label":"Weekly","usedPercent":99,"remainingPercent":1}],"error":"Grok request timed out.","transportDiagnostic":{"category":"timeout"}}]}"#).utf8))
            let autoError = TrayAnimator.applyQuotaRemaining(
                payload: errorOnlyPayload, persistedSelection: QuotaResolver.auto,
                excluding: [], cachedRemaining: 1, defaults: defaults)
            expect(
                autoError == nil && defaults.object(forKey: TrayAnimator.lastRemainingKey) == nil,
                "Auto excludes an error-only fallback payload and clears scalar")

            defaults.set(1, forKey: TrayAnimator.lastRemainingKey)
            let optionalAbsentPayload = try! JSONDecoder().decode(
                AgentUsagePayload.self,
                from: Data(#"{"generatedAt":"now","agents":[{"clientId":"codex","source":"oauth","updatedAt":"now","windows":[{"cardId":"session.v1","label":"Session","usedPercent":20,"remainingPercent":80}]}]}"#.utf8))
            let optionalAbsent = TrayAnimator.applyQuotaRemaining(
                payload: optionalAbsentPayload,
                persistedSelection: "grok|billing.weekly.v1",
                excluding: [], cachedRemaining: 1, defaults: defaults)
            expect(
                optionalAbsent == nil
                    && defaults.object(forKey: TrayAnimator.lastRemainingKey) == nil,
                "optional provider absence clears an explicit cached scalar")

            let standardBefore = UserDefaults.standard.persistentDomain(
                forName: Bundle.main.bundleIdentifier ?? "TokenBar")
            let demoFresh = TrayAnimator.applyQuotaRemaining(
                payload: quotaPayload, persistedSelection: "codex|session.v1", excluding: [],
                cachedRemaining: nil, defaults: nil)
            let standardAfter = UserDefaults.standard.persistentDomain(
                forName: Bundle.main.bundleIdentifier ?? "TokenBar")
            expect(
                demoFresh == 80 && NSDictionary(dictionary: standardBefore ?? [:])
                    .isEqual(to: standardAfter ?? [:]),
                "nil defaults returns fresh quota without touching live defaults")

            let hiddenAll = TrayAnimator.applyQuotaRemaining(
                payload: quotaPayload, persistedSelection: QuotaResolver.auto,
                excluding: ["codex", "claude"], cachedRemaining: 66, defaults: defaults)
            expect(
                hiddenAll == nil && defaults.object(forKey: TrayAnimator.lastRemainingKey) == nil,
                "all-hidden successful payload cannot fall back to cached scalar")
            defaults.removePersistentDomain(forName: suiteName)
        } else {
            expect(false, "isolated quota defaults suite is available")
        }

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

        // A fresh order key still needs the hidden present-client slots before
        // the visible subset is merged. Otherwise re-enabling the hidden tab
        // would append it after every reordered visible tab.
        expect(
            ClientRegistry.mergeReorder(
                full: DashboardTabs.completeOrder(
                    [], present: ["claude", "codex", "gemini"]),
                visible: ["claude", "gemini"], from: "gemini", to: "claude")
                == ["gemini", "codex", "claude"],
            "top-tab reorder preserves an unsaved hidden client slot")

        // Top tab-bar drag reorder: the drop line sits on the edge the
        // direction-aware insert will use (right → after the target, left →
        // before it). Overview is not in `clients`, so it can never be a drop
        // target and no client can be dragged ahead of it.
        let tabIds = ["codex", "claude", "opencode"]
        expect(
            DashboardTabs.dropEdge(
                dragId: "codex", overId: "opencode", tabId: "opencode", in: tabIds) == .trailing,
            "dragging right marks the target's trailing edge")
        expect(
            DashboardTabs.dropEdge(
                dragId: "opencode", overId: "codex", tabId: "codex", in: tabIds) == .leading,
            "dragging left marks the target's leading edge")
        expect(
            DashboardTabs.dropEdge(
                dragId: "codex", overId: "claude", tabId: "opencode", in: tabIds) == nil,
            "only the hovered tab draws a drop line")
        expect(
            DashboardTabs.dropEdge(
                dragId: nil, overId: "claude", tabId: "claude", in: tabIds) == nil,
            "no drop line without an active drag")
        expect(
            DashboardTabs.dropEdge(
                dragId: "codex", overId: "overview", tabId: "overview", in: tabIds) == nil,
            "Overview is never a drop target")

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
        expect(
            mSlices.count == 1 && mSlices[0].key == "m1|p"
                && mSlices[0].tokens == .max && mSlices[0].input == .max
                && mSlices[0].output == 0,
            "drill-down merges the month's model token lanes across days with saturation")
        expect(MonthlyView.modelSlices(
                for: mRows[0], clientIds: ["a", "b"], colors: ModelColorMap(report: nil)).count == 2,
            "drill-down shows client b's model when b is selected")

        // Message-only activity (PR #54 review r3595383789): a contribution
        // with messages but zero tokens and zero cost must still surface —
        // some parsers emit message-count-only rows. Prior guard was
        // `tokens > 0 || cost > 0`, which dropped this month entirely.
        let messageOnlyJSON = """
        {"meta":{"generatedAt":"now","version":"1","dateRange":{"start":"2026-01-01","end":"2026-01-01"}},
         "summary":{"totalTokens":0,"totalCost":0,"totalDays":1,"activeDays":1,"averagePerDay":0,
                    "maxCostInSingleDay":0,"clients":["codex"],"models":[]},
         "years":[],
         "contributions":[
           {"date":"2026-01-01","totals":{"tokens":0,"cost":0,"messages":5},"intensity":0,
            "turnsByClient":{"codex":7},
            "tokenBreakdown":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0},
            "clients":[
              {"client":"codex","modelId":"m1","providerId":"p","cost":0,"messages":5,
               "tokens":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0}}]}
         ]}
        """
        let messageOnlyPayload = try! JSONDecoder().decode(UsagePayload.self, from: Data(messageOnlyJSON.utf8))
        let moRows = MonthlyView.monthRows(payload: messageOnlyPayload, clientIds: ["codex"])
        expect(moRows.count == 1 && moRows[0].messages == 5 && moRows[0].tokens == 0 && moRows[0].cost == 0,
            "a message-only month (zero tokens, zero cost) still surfaces in the Monthly lens")
        let monthlyMessageOnlySlice = moRows.first.flatMap {
            MonthlyView.modelSlices(
                for: $0, clientIds: ["codex"], colors: ModelColorMap(report: nil)
            ).first
        }
        expect(
            monthlyMessageOnlySlice?.key == "m1|p" && monthlyMessageOnlySlice?.tokens == 0
                && monthlyMessageOnlySlice?.cost == 0,
            "a message-only month retains its model drill-down")
        expect(
            UsageStats.hasVisibleActivity(
                contributions: messageOnlyPayload.contributions, hidden: []
            ) && UsageStats.yearsWithVisibleActivity(
                contributions: messageOnlyPayload.contributions, hidden: []
            ) == ["2026"],
            "message-only activity keeps its selected year visible")
        let messageOnlyStats = UsageStats(
            payload: messageOnlyPayload, selectedClients: ["codex"])
        expect(
            messageOnlyStats.activeDays == 1
                && messageOnlyStats.perDayMap["2026-01-01"]?.tokens == 0
                && messageOnlyStats.perDayMap["2026-01-01"]?.hasMessages == true
                && messageOnlyStats.totalTokens == 0 && messageOnlyStats.totalCost == 0,
            "shared usage stats count a selected message-only day as active")
        expect(
            UsageStats(payload: messageOnlyPayload, selectedClients: []).activeDays == 0,
            "shared usage stats do not count an unselected message-only day")

        // Daily/Monthly turn counts reuse the existing local-hour report, but
        // only after strict calendar-key validation and only for Codex/Claude.
        let turnReportJSON = """
        {"entries":[
          {"hour":"2025-12-31 23:00","clients":["codex"],"models":["m"],
           "input":0,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0,
           "total":0,"messageCount":0,"turnCount":2,"cost":0},
          {"hour":"2026-01-01 08:00","clients":["codex"],"models":["m"],
           "input":0,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0,
           "total":0,"messageCount":0,"turnCount":3,"cost":0},
          {"hour":"2026-01-01 09:00","clients":["claude"],"models":["m"],
           "input":0,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0,
           "total":0,"messageCount":0,"turnCount":4,"cost":0},
          {"hour":"2026-02-01 00:00","clients":["claude"],"models":["m"],
           "input":0,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0,
           "total":0,"messageCount":0,"turnCount":0,"cost":0},
          {"hour":"2026-02-30 00:00","clients":["codex"],"models":["m"],
           "input":0,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0,
           "total":0,"messageCount":0,"turnCount":99,"cost":0},
          {"hour":"not-an-hour","clients":["codex"],"models":["m"],
           "input":0,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0,
           "total":0,"messageCount":0,"turnCount":99,"cost":0}
        ],"totalCost":0}
        """
        let turnReport = try! JSONDecoder().decode(
            HourlyReport.self, from: Data(turnReportJSON.utf8))
        // `supportedTurnClients` decides which clients Daily/Monthly sum turns
        // for and which names the "Turns · X only" subtitle reports. Its only
        // coverage was removed with the hourly folds it happened to sit beside;
        // an unsupported client contributing nothing is now harmless, but
        // wrongly EXCLUDING codex or claude would silently undercount with no
        // test turning red.
        expect(
            PopoverView.supportedTurnClients(["gemini", "claude", "opencode", "codex"])
                == ["claude", "codex"],
            "the turn scope keeps only the supported clients, in the order given")
        expect(
            PopoverView.supportedTurnClients(["gemini", "opencode"]).isEmpty
                && PopoverView.supportedTurnClients([]).isEmpty,
            "a slice with no supported client yields an empty turn scope")
        expect(
            TurnCountBuckets.scope(["codex"]) == "Turns · Codex only"
                && TurnCountBuckets.scope([]) == nil,
            "the turn subtitle names the scope it actually summed")

        // LP2A — turns now ride the graph payload. What has to hold is that the
        // per-client map is respected exactly: a client outside the supported
        // turn set contributes nothing, and a hidden one is subtractable —
        // which is precisely what a single per-day total could not express.
        let turnsPayload = DemoData.payload(for: nil)
        let turnsDay = turnsPayload.contributions.first { ($0.turnsByClient ?? [:]).count > 1 }
        expect(
            turnsDay != nil,
            "the demo payload carries a per-client turn map (without this every "
                + "assertion below would pass on absent data)")
        if let turnsDay, let map = turnsDay.turnsByClient {
            let present = map.keys.sorted()
            let first = present[0]
            let both = turnsDay.turns(for: present)
            let single = turnsDay.turns(for: [first])
            let expectedTotal = present.reduce(Int64(0)) { $0 + (map[$1] ?? 0) }
            expect(
                both == expectedTotal && single == map[first],
                "a day's turns are the sum over exactly the requested clients")
            expect(
                turnsDay.turns(for: present.dropFirst().map { $0 })
                    == expectedTotal - (map[first] ?? 0),
                "dropping a client subtracts exactly its own turns — the reason the "
                    + "engine keys the map by client instead of emitting one daily total")
            expect(
                turnsDay.turns(for: ["not-a-client"]) == 0,
                "an unknown client contributes zero rather than falling back to a total")
            expect(
                turnsDay.turns(for: []) == nil,
                "an empty selection is 'nothing selected', not 'zero turns'")
        }
        // Monthly is the same map summed across the month's days.
        let monthlyTurnRows = MonthlyView.monthRows(
            payload: turnsPayload, clientIds: ClientRegistry.allIds,
            turnClientIds: ["codex", "claude"])
        let monthlyTurnsExpected: Int64 = turnsPayload.contributions.reduce(Int64(0)) {
            $0 + ($1.turns(for: ["codex", "claude"]) ?? 0)
        }
        expect(
            monthlyTurnRows.reduce(Int64(0)) { $0 + ($1.turns ?? 0) } == monthlyTurnsExpected,
            "the monthly rows sum each day's turns for the same client slice")

        let dailyMessageOnlyView = DailyView(
            payload: messageOnlyPayload, clientIds: ["codex"],
            turnClientIds: ["codex", "claude"], colors: ModelColorMap(report: nil)
        )
        let dailyMessageOnlyRows = dailyMessageOnlyView.rows
        expect(
            dailyMessageOnlyRows.count == 1 && dailyMessageOnlyRows[0].messages == 5
                && dailyMessageOnlyRows[0].tokens == 0 && dailyMessageOnlyRows[0].cost == 0
                && dailyMessageOnlyRows[0].turns == 7,
            "Daily retains a message-only day and attaches its positive turn count")
        let dailyMessageOnlySlice = dailyMessageOnlyRows.first.flatMap {
            dailyMessageOnlyView.models(for: $0.contribution).first
        }
        expect(
            dailyMessageOnlySlice?.key == "m1|p" && dailyMessageOnlySlice?.tokens == 0
                && dailyMessageOnlySlice?.cost == 0,
            "a message-only Daily row retains its model drill-down")

        let dashboardYearDefaultsKey = "tokenbar.dashboard.year"
        let savedDashboardYear = UserDefaults.standard.object(forKey: dashboardYearDefaultsKey)
        let turnTransitionChecks = awaitMainActorValue { () async -> [String: Bool] in
            let yearA = "2037"
            let yearB = "2038"
            let clients = ["codex", "claude"]

            // A superseded A request must not commit after the model moves to B.
            let staleSource = ControlledTurnUsageDataSource()
            let staleModel = DashboardModel(source: staleSource, initialYear: yearA)
            await staleModel.load()
            await staleSource.blockHourly(year: yearA)
            let staleTask = Task {
                await staleModel.ensureData(for: .hourly, clients: clients)
            }
            let stalePending = await waitUntil {
                await staleSource.hasPendingHourly(year: yearA)
            }
            await staleModel.setYear(yearB)
            await staleSource.releaseHourly(year: yearA)
            await staleTask.value
            let staleSuppressed = await staleModel.hourlyReport(for: clients) == nil
            await staleModel.ensureData(for: .hourly, clients: clients)
            let matchingBAfterStale = await staleModel.hourlyReport(for: clients) != nil

            // The inverse ordering is also fail-closed: B's hourly report may
            // arrive while graph A is still displayed, but remains hidden until
            // graph B commits.
            let inverseSource = ControlledTurnUsageDataSource()
            let inverseModel = DashboardModel(source: inverseSource, initialYear: yearA)
            await inverseModel.load()
            await inverseModel.ensureData(for: .hourly, clients: clients)
            let initialAVisible = await inverseModel.hourlyReport(for: clients) != nil
            await inverseSource.blockGraph(year: yearB)
            let switchTask = Task { await inverseModel.setYear(yearB) }
            let graphBPending = await waitUntil {
                await inverseSource.hasPendingGraph(year: yearB)
            }
            let oldReportSuppressed = await inverseModel.hourlyReport(for: clients) == nil
            await inverseModel.ensureData(for: .hourly, clients: clients)
            await inverseSource.releaseGraph(year: yearB)
            await switchTask.value
            let matchingBVisible = await inverseModel.hourlyReport(for: clients) != nil

            // The phantom-slice guard: `setYear` moves `year` synchronously
            // while the payload catches up only when reload commits, so the
            // model task can fire with a new year against the previous
            // payload. Scanning for that pairing issues a report the year
            // guard must then discard. Blocking the graph holds the model in
            // exactly that window.
            let phantomSource = ControlledTurnUsageDataSource()
            let phantomModel = DashboardModel(source: phantomSource, initialYear: yearA)
            await phantomModel.load()
            await phantomSource.blockGraph(year: yearB)
            let phantomSwitch = Task { await phantomModel.setYear(yearB) }
            _ = await waitUntil { await phantomSource.hasPendingGraph(year: yearB) }
            let phantomCallsBefore = await phantomSource.modelCallCount()
            await phantomModel.ensureModelData(for: .overview)
            let phantomIssuedNoScan =
                await phantomSource.modelCallCount() == phantomCallsBefore
            // Issuing nothing is only half of it. The cards distinguish
            // "loading" from "completed and found nothing" by this flag alone,
            // so leaving it down here made Models and Overview claim the new
            // year had no model usage for the whole graph reload.
            let phantomReadsAsLoading = await phantomModel.modelLoading
            await phantomSource.releaseGraph(year: yearB)
            await phantomSwitch.value

            // A year switch must drop the previous year's model report. Two
            // sites enforce it — `setYear` calls `invalidateModel()`, and
            // `apply()` discards a report whose `modelYear` disagrees with the
            // committed payload — so each masks the other under single-site
            // mutation, and neither was covered. Without both, switching year
            // leaves the old year's models rendered in Overview and Models.
            let yearModelSource = ControlledTurnUsageDataSource()
            let yearModelModel = DashboardModel(source: yearModelSource, initialYear: yearA)
            await yearModelModel.load()
            await yearModelModel.ensureModelData(for: .overview)
            let modelHeldForA = await yearModelModel.modelReport != nil
            await yearModelModel.setYear(yearB)
            let modelDroppedOnYearSwitch = await yearModelModel.modelReport == nil

            // LP2A — Daily/Monthly no longer request any lazy report; their
            // turns ride the graph payload. Blocking hourly and driving both
            // lenses proves the request is gone rather than merely fast.
            let noHourlySource = ControlledTurnUsageDataSource()
            let noHourlyModel = DashboardModel(source: noHourlySource, initialYear: yearA)
            await noHourlyModel.load()
            await noHourlySource.blockHourly(year: yearA)
            // Driven in Tasks and probed, not awaited: if the request came back
            // these would park on the blocked source forever, and a hung suite
            // is a far worse failure signal than a red assertion.
            let dailyDrive = Task { await noHourlyModel.ensureData(for: .daily, clients: clients) }
            let monthlyDrive = Task {
                await noHourlyModel.ensureData(for: .monthly, clients: clients)
            }
            let hourlyReappeared = await waitUntil {
                await noHourlySource.hasPendingHourly(year: yearA)
            }
            let emptyDidNotFetch = !hourlyReappeared
            await noHourlySource.releaseHourly(year: yearA)
            await dailyDrive.value
            await monthlyDrive.value

            // LP2B — the graph must not share its scan window with the model.
            // With the graph blocked, load() is still in flight; a model request
            // arriving now is precisely the contention that made first paint
            // 5x slower warm and 2.5x slower cold on a real corpus.
            let deferSource = ControlledTurnUsageDataSource()
            let deferModel = DashboardModel(source: deferSource, initialYear: yearA)
            await deferSource.blockGraph(year: yearA)
            let deferLoad = Task { await deferModel.load() }
            let deferGraphPending = await waitUntil {
                await deferSource.hasPendingGraph(year: yearA)
            }
            // Drive the model-dependent lens while the graph is still blocked:
            // it must find no committed payload and issue nothing.
            await deferModel.ensureModelData(for: .overview)
            let noModelDuringGraph = await deferSource.modelCallCount() == 0
            await deferSource.releaseGraph(year: yearA)
            await deferLoad.value
            let readyWithoutModel = await deferModel.modelReport == nil
            let phaseReadyBeforeModel = await {
                if case .ready = await deferModel.phase { return true }
                return false
            }()
            // Now the payload has committed, the same call fetches once.
            await deferModel.ensureModelData(for: .overview)
            let modelArrives = await deferModel.modelReport != nil
            let fetchedOnce = await deferSource.modelCallCount() == 1
            // Idempotent per committed generation: a re-render must not refetch.
            await deferModel.ensureModelData(for: .overview)
            let noRefetch = await deferSource.modelCallCount() == 1
            let neverRacedGraph = await deferSource.modelCallsRacingGraph() == 0

            // Daily/Monthly do not depend on the model report at all.
            let dailySource = ControlledTurnUsageDataSource()
            let dailyModel = DashboardModel(source: dailySource, initialYear: yearA)
            await dailyModel.load()
            await dailyModel.ensureModelData(for: .daily)
            await dailyModel.ensureModelData(for: .monthly)
            let lensesSkipModel = await dailySource.modelCallCount() == 0

            // F1 regression — a graph refresh must not fan out into two
            // concurrent model scans. The defect only appears under the real
            // interleaving: the background refresh bumps the request token and
            // THEN suspends, which yields the MainActor and lets PopoverView's
            // generation-keyed task re-fire and issue a second scan. A purely
            // sequential drive cannot reproduce it — the first request would
            // simply publish and the second would find matching identity — so
            // this blocks the model source and re-fires the lens while a
            // request is genuinely in flight.
            let refreshSource = ControlledTurnUsageDataSource()
            await refreshSource.advanceGraphGenerationPerCall()
            let refreshModel = DashboardModel(source: refreshSource, initialYear: nil)
            await refreshModel.load()
            await refreshModel.ensureModelData(for: .overview)
            let beforeRefresh = await refreshSource.modelCallCount()
            await refreshSource.blockModel()
            let refreshTask = Task { await refreshModel.refresh() }
            // Give any background model request a chance to be issued and park.
            let sawInFlight = await waitUntil { await refreshSource.pendingModelCount() > 0 }
            // The lens task re-fires here, exactly as the payload-generation key
            // makes it once apply() commits the refreshed graph.
            let refireTask = Task { await refreshModel.ensureModelData(for: .overview) }
            _ = await waitUntil { await refreshSource.pendingModelCount() > 0 }
            await refreshSource.releaseModel()
            await refreshTask.value
            await refireTask.value
            let afterRefresh = await refreshSource.modelCallCount()
            // EXACTLY one scan per graph commit. `<=` alone was one-sided: it
            // stayed green when the identity gate was mutated to never refetch,
            // which is the opposite failure and the one deleting the background
            // refresh could plausibly have caused.
            let oneScanPerCommit = (afterRefresh - beforeRefresh) == 1
            _ = sawInFlight

            // Re-entry during an in-flight scan must join it, not start a
            // second. Both triggers are ordinary interaction: expanding another
            // Daily/Monthly row, or switching Overview→Models mid-scan.
            let reentrySource = ControlledTurnUsageDataSource()
            let reentryModel = DashboardModel(source: reentrySource, initialYear: yearA)
            await reentryModel.load()
            await reentrySource.blockModel()
            let firstEntry = Task { await reentryModel.ensureModelColors() }
            _ = await waitUntil { await reentrySource.pendingModelCount() > 0 }
            let secondEntry = Task { await reentryModel.ensureModelColors() }
            let thirdEntry = Task { await reentryModel.ensureModelData(for: .models) }
            let coalescedWhileInFlight = await reentrySource.pendingModelCount() == 1
            await reentrySource.releaseModel()
            await firstEntry.value
            await secondEntry.value
            await thirdEntry.value
            let reentryScans = await reentrySource.modelCallCount()
            let reentryCoalesced = coalescedWhileInFlight && reentryScans == 1
            // The coalesced request must still publish — collapsing re-entry is
            // only correct if the surviving scan actually lands.
            let coalescedStillPublished = await reentryModel.modelReport != nil

            // A snapshot whose model lags its payload must refetch on the first
            // model lens; storing only the payload's generation would silently
            // present a stale report as current.
            let lagSource = ControlledTurnUsageDataSource()
            await lagSource.advanceGraphGenerationPerCall()
            let lagSeed = DashboardModel(
                cachesSnapshot: true, source: lagSource, initialYear: nil)
            await lagSeed.load()
            await lagSeed.ensureModelData(for: .overview)
            let lagSeedCalls = await lagSource.modelCallCount()
            // Move the graph on without touching the model, exactly as a poll
            // does while the user sits on a lens that shows no models.
            await lagSeed.refresh()
            let lagRestored = DashboardModel(
                cachesSnapshot: true, source: lagSource, initialYear: nil)
            await lagRestored.ensureModelData(for: .overview)
            let lagRefetched = await lagSource.modelCallCount() > lagSeedCalls

            // A genuinely newer slice must supersede, not be swallowed by the
            // coalescing guard. Without this a guard of the shape
            // `if modelInFlight != nil { return }` would look correct.
            let supersedeSource = ControlledTurnUsageDataSource()
            await supersedeSource.advanceGraphGenerationPerCall()
            // A concrete year, not nil: `lastSnapshot` is process-static and an
            // earlier check in this same function seeds it for the all-time
            // slice, which this model would otherwise restore — masking the
            // first request entirely. The advancing fixture days live in 2037,
            // so a 2037 filter still moves `generatedAt` per graph call.
            let supersedeModel = DashboardModel(source: supersedeSource, initialYear: yearA)
            await supersedeModel.load()
            await supersedeSource.blockModel()
            let staleScan = Task { await supersedeModel.ensureModelData(for: .overview) }
            _ = await waitUntil { await supersedeSource.pendingModelCount() > 0 }
            // Move the graph on, then request again: a different generation, so
            // this must start its own scan rather than join the parked one.
            await supersedeModel.refresh()
            let newerScan = Task { await supersedeModel.ensureModelData(for: .overview) }
            let bothInFlight = await waitUntil { await supersedeSource.pendingModelCount() >= 2 }
            // Retire only the superseded scan. Its completion must not clear
            // the loading flag, which still belongs to the scan replacing it —
            // otherwise the cards fall to the empty copy while work continues.
            await supersedeSource.releaseOneModel()
            await staleScan.value
            let loadingHeldBySuccessor = supersedeModel.modelLoading
            await supersedeSource.releaseModel()
            await newerScan.value
            let newerSliceSupersedes = bothInFlight

            // ABA — the in-flight slot must be released by the scan that owns
            // it, not by any scan carrying an equal identity value. A year
            // round-trip during a scan reproduces the equal-value collision
            // because tb_graph's 30s cache returns the same payload, so the
            // generation is unchanged.
            let abaSource = ControlledTurnUsageDataSource()
            let abaModel = DashboardModel(source: abaSource, initialYear: yearA)
            await abaModel.load()
            await abaSource.blockModel()
            let abaFirst = Task { await abaModel.ensureModelColors() }
            _ = await waitUntil { await abaSource.pendingModelCount() > 0 }
            await abaModel.setYear(yearB)
            await abaModel.setYear(yearA)
            let abaSecond = Task { await abaModel.ensureModelColors() }
            _ = await waitUntil { await abaSource.pendingModelCount() >= 2 }
            // Retire only the stale scan and let it finish unwinding. If it
            // releases the live scan's slot, the next ordinary re-entry starts a
            // third concurrent scan.
            await abaSource.releaseOneModel()
            await abaFirst.value
            let abaThird = Task { await abaModel.ensureModelColors() }
            // Wait for the outcome instead of sampling immediately: the third
            // task had not been scheduled yet when this read the counter, so
            // the assertion passed even with the defect restored.
            let abaThirdStarted = await waitUntil { await abaSource.modelCallCount() >= 3 }
            let abaNoThirdScan = !abaThirdStarted
            await abaSource.releaseModel()
            await abaSecond.value
            await abaThird.value

            // modelLoading is the only thing keeping the model cards from
            // reading "No model usage in this range" mid-scan, and nothing
            // asserted it until now.
            let loadingSource = ControlledTurnUsageDataSource()
            let loadingModel = DashboardModel(source: loadingSource, initialYear: yearA)
            await loadingModel.load()
            await loadingSource.blockModel()
            let loadingTask = Task { await loadingModel.ensureModelData(for: .models) }
            _ = await waitUntil { await loadingSource.pendingModelCount() > 0 }
            let loadingTrueInFlight = await loadingModel.modelLoading
            await loadingSource.releaseModel()
            await loadingTask.value
            let loadingFalseAfter = await loadingModel.modelLoading == false

            // F2 regression — Daily/Monthly render a per-model drill-down whose
            // dots are tinted from the model report. Expanding a row must be
            // able to populate the colour map, or every model of one provider
            // collapses onto the same rank-0 shade.
            let colorSource = ControlledTurnUsageDataSource()
            let colorModel = DashboardModel(source: colorSource, initialYear: yearA)
            await colorModel.load()
            await colorModel.ensureModelData(for: .daily)
            let entriesForColor = DemoData.modelReport(for: yearA).entries
                .sorted { $0.cost > $1.cost }
            let colorProbe: (String, String) -> String = { provider, name in
                colorModel.colors.color(provider, name)
            }
            let flatBeforeExpand: Bool = {
                guard entriesForColor.count >= 2 else { return true }
                let a = colorProbe(entriesForColor[0].provider, entriesForColor[0].model)
                let b = colorProbe(entriesForColor[1].provider, entriesForColor[1].model)
                return a == b
            }()
            await colorModel.ensureModelColors()
            let gradedAfterExpand: Bool = {
                guard entriesForColor.count >= 2 else { return true }
                let sameProvider = entriesForColor.first { entry in
                    entry.provider == entriesForColor[0].provider
                        && entry.model != entriesForColor[0].model
                }
                guard let sameProvider else { return true }
                let a = colorProbe(entriesForColor[0].provider, entriesForColor[0].model)
                let b = colorProbe(sameProvider.provider, sameProvider.model)
                return a != b
            }()

            return [
                "deferGraphPending": deferGraphPending,
                "noModelDuringGraph": noModelDuringGraph,
                "readyWithoutModel": readyWithoutModel,
                "phaseReadyBeforeModel": phaseReadyBeforeModel,
                "modelArrives": modelArrives,
                "fetchedOnce": fetchedOnce,
                "noRefetch": noRefetch,
                "neverRacedGraph": neverRacedGraph,
                "lensesSkipModel": lensesSkipModel,
                "modelHeldForA": modelHeldForA,
                "phantomIssuedNoScan": phantomIssuedNoScan,
                "phantomReadsAsLoading": phantomReadsAsLoading,
                "modelDroppedOnYearSwitch": modelDroppedOnYearSwitch,
                "oneScanPerCommit": oneScanPerCommit,
                "flatBeforeExpand": flatBeforeExpand,
                "gradedAfterExpand": gradedAfterExpand,
                "reentryCoalesced": reentryCoalesced,
                "coalescedStillPublished": coalescedStillPublished,
                "lagRefetched": lagRefetched,
                "newerSliceSupersedes": newerSliceSupersedes,
                "loadingHeldBySuccessor": loadingHeldBySuccessor,
                "abaNoThirdScan": abaNoThirdScan,
                "loadingTrueInFlight": loadingTrueInFlight,
                "loadingFalseAfter": loadingFalseAfter,
                "stalePending": stalePending,
                "staleSuppressed": staleSuppressed,
                "matchingBAfterStale": matchingBAfterStale,
                "initialAVisible": initialAVisible,
                "graphBPending": graphBPending,
                "oldReportSuppressed": oldReportSuppressed,
                "matchingBVisible": matchingBVisible,
                "emptyDidNotFetch": emptyDidNotFetch,
            ]
        }
        if let savedDashboardYear {
            UserDefaults.standard.set(savedDashboardYear, forKey: dashboardYearDefaultsKey)
        } else {
            UserDefaults.standard.removeObject(forKey: dashboardYearDefaultsKey)
        }
        expect(
            turnTransitionChecks?["stalePending"] == true
                && turnTransitionChecks?["staleSuppressed"] == true
                && turnTransitionChecks?["matchingBAfterStale"] == true,
            "a superseded old-year hourly request cannot publish into the new year")
        expect(
            turnTransitionChecks?["initialAVisible"] == true
                && turnTransitionChecks?["graphBPending"] == true
                && turnTransitionChecks?["oldReportSuppressed"] == true
                && turnTransitionChecks?["matchingBVisible"] == true,
            "the Hourly lens shows a report only for the year it was fetched for")
        expect(
            turnTransitionChecks?["deferGraphPending"] == true
                && turnTransitionChecks?["noModelDuringGraph"] == true
                && turnTransitionChecks?["neverRacedGraph"] == true,
            "the model report is never requested while the graph scan is in flight")
        expect(
            turnTransitionChecks?["readyWithoutModel"] == true
                && turnTransitionChecks?["phaseReadyBeforeModel"] == true,
            "the dashboard reaches .ready on the graph alone, without the model")
        expect(
            turnTransitionChecks?["modelArrives"] == true
                && turnTransitionChecks?["fetchedOnce"] == true
                && turnTransitionChecks?["noRefetch"] == true,
            "a committed payload fetches the model exactly once per generation")
        expect(
            turnTransitionChecks?["modelHeldForA"] == true
                && turnTransitionChecks?["modelDroppedOnYearSwitch"] == true,
            "a year switch drops the previous year's model report instead of leaving it "
                + "rendered beside the new year's graph")
        expect(
            turnTransitionChecks?["phantomIssuedNoScan"] == true,
            "no model scan is issued for a phantom slice — a new year paired with the "
                + "payload it has not replaced yet")
        expect(
            turnTransitionChecks?["phantomReadsAsLoading"] == true,
            "during a year switch the model reads as loading, never as an empty result — "
                + "the cards would otherwise claim the new year has no model usage")
        expect(
            turnTransitionChecks?["lensesSkipModel"] == true,
            "Daily/Monthly never request the model report on activation")
        expect(
            turnTransitionChecks?["oneScanPerCommit"] == true,
            "a graph refresh issues exactly one model scan — never two racing ones, "
                + "and never zero (a moved generation must refetch)")
        expect(
            turnTransitionChecks?["reentryCoalesced"] == true
                && turnTransitionChecks?["coalescedStillPublished"] == true,
            "re-entry during an in-flight model scan joins it instead of starting a second")
        expect(
            turnTransitionChecks?["lagRefetched"] == true,
            "a restored snapshot whose model lags its payload refetches instead of "
                + "presenting the stale report as current")
        expect(
            turnTransitionChecks?["newerSliceSupersedes"] == true,
            "coalescing joins only the identical slice — a moved generation still "
                + "starts its own scan instead of being swallowed by the guard")
        expect(
            turnTransitionChecks?["abaNoThirdScan"] == true,
            "a stale scan cannot release a live scan's in-flight slot — the year "
                + "round-trip that gives both the same identity value must not open a third scan")
        expect(
            turnTransitionChecks?["loadingTrueInFlight"] == true
                && turnTransitionChecks?["loadingFalseAfter"] == true
                && turnTransitionChecks?["loadingHeldBySuccessor"] == true,
            "modelLoading is true for the duration of a scan and clears when it lands — "
                + "it is the only thing keeping the cards off the empty copy mid-fetch")

        // The chain that actually gets a deferred model onto the screen is view
        // wiring, and a UI-free test cannot press a SwiftUI Button. Deleting any
        // link left every runtime assertion above green, so the tree this binary
        // was built from is read directly — the same technique the Discord
        // section uses for "declared in exactly one place".
        func lp2bSource(_ name: String) -> String {
            let root = URL(fileURLWithPath: #filePath)
                .deletingLastPathComponent()  // Sources/TokenBar
            for sub in ["", "Views/"] {
                let url = root.appendingPathComponent(sub + name)
                if let text = try? String(contentsOf: url, encoding: .utf8) { return text }
            }
            return ""
        }
        let lp2bDaily = lp2bSource("DailyView.swift")
        let lp2bMonthly = lp2bSource("MonthlyView.swift")
        let lp2bPopover = lp2bSource("PopoverView.swift")
        expect(
            !lp2bDaily.isEmpty && !lp2bMonthly.isEmpty && !lp2bPopover.isEmpty,
            "the LP2B source scan found its three files (without this every wiring "
                + "assertion below would pass on empty strings)")
        expect(
            lp2bDaily.contains("if !isOpen { onExpand?() }")
                && lp2bMonthly.contains("if !isOpen { onExpand?() }"),
            "Daily and Monthly request the model colour map when a row opens")
        expect(
            lp2bPopover.components(separatedBy: "onExpand: { Task { await model.ensureModelColors() } }")
                .count - 1 == 2,
            "PopoverView wires that callback for both lenses, not just one")
        // Pin the model task by its id components AND its body, but tolerate
        // reformatting: an earlier version matched the whole multi-line literal
        // including indentation, so a purely cosmetic rewrap turned it red.
        // Matching a substring alone is the opposite failure — the sibling
        // hidden-client task on the next line carries the same generation
        // interpolation, which let the id lose its generation unnoticed. So
        // strip whitespace entirely, then require the exact id triple next to
        // the call it guards — that survives any rewrap while still pinning
        // every component of the key.
        let lp2bFlat = lp2bPopover.filter { !$0.isWhitespace }
        let lp2bModelTask = #".task(id:"\(activeViewRaw)|\(model.year??"")|"#
            + #"\(model.payload?.meta.generatedAt??"")"){"#
            + "awaitmodel.ensureModelData(for:activeView.wrappedValue)}"
        expect(
            lp2bFlat.contains(lp2bModelTask),
            "the model task is keyed on lens, year AND the committed payload generation — "
                + "the generation is what keeps the model scan off the graph's critical path")
        expect(
            turnTransitionChecks?["flatBeforeExpand"] == true
                && turnTransitionChecks?["gradedAfterExpand"] == true,
            "expanding a Daily/Monthly row populates the model colour map")
        // The old Daily/Monthly contract — "suppress turns until the payload and
        // the hourly report agree on the year" — has no successor assertion on
        // purpose. Turns now come from the very contribution the row is built
        // from, so a row's messages and its turns cannot describe different
        // years; the failure mode is removed by construction rather than
        // guarded at runtime. What remains testable is that the request is gone.
        expect(
            turnTransitionChecks?["emptyDidNotFetch"] == true,
            "Daily and Monthly request no hourly report at all — their turns come "
                + "from the graph payload")

        let hourlyCacheChecks = awaitMainActorValue { () async -> [String: Bool] in
            let year = "2047"
            let turnClients = ["codex", "claude"]
            let hourlyClients = ["codex", "claude", "gemini"]

            func fingerprint(_ report: HourlyReport?) -> String? {
                guard let report else { return nil }
                let entries = report.entries.map {
                    "\($0.hour)|\($0.clients.joined(separator: ","))|\($0.total)|"
                        + "\($0.messageCount)|\($0.turnCount)|\($0.cost)"
                }.joined(separator: ";")
                return "\(entries)|\(report.totalCost)"
            }

            func report(client: String, turns: Int) -> HourlyReport {
                let json: [String: Any] = [
                    "entries": [[
                        "hour": "2047-01-01 00:00", "clients": [client],
                        "models": ["fixture"], "input": 0, "output": 0,
                        "cacheRead": 0, "cacheWrite": 0, "reasoning": 0,
                        "total": turns, "messageCount": turns, "turnCount": turns,
                        "cost": Double(turns),
                    ]],
                    "totalCost": Double(turns),
                ]
                let data = try! JSONSerialization.data(withJSONObject: json)
                return try! JSONDecoder().decode(HourlyReport.self, from: data)
            }

            @MainActor func blockedModel(
                source: ControlledTurnUsageDataSource,
                cachesSnapshot: Bool = true,
                view: AppView,
                clients: [String]
            ) async -> (DashboardModel, Task<Void, Never>, Bool) {
                await source.blockHourly(year: year)
                let model = DashboardModel(
                    cachesSnapshot: cachesSnapshot, source: source, initialYear: year)
                await model.load()
                let task = Task { await model.ensureData(for: view, clients: clients) }
                let pending = await waitUntil { await source.hasPendingHourly(year: year) }
                return (model, task, pending)
            }

            let originalA = report(client: "a", turns: 1)
            let originalB = report(client: "b", turns: 2)
            let refreshedA = report(client: "a-new", turns: 3)
            let nonOwnerB = report(client: "local", turns: 4)
            let fixturesDistinct = Set([
                fingerprint(originalA), fingerprint(originalB), fingerprint(refreshedA),
                fingerprint(nonOwnerB),
            ]).count == 4

            // Seed two distinct popover-owned slices under the same year.
            let seedSource = ControlledTurnUsageDataSource(hourlyResponses: [
                Set(turnClients): originalA,
                Set(hourlyClients): originalB,
            ])
            let seedModel = DashboardModel(
                cachesSnapshot: true, source: seedSource, initialYear: year)
            await seedModel.load()
            await seedModel.ensureData(for: .hourly, clients: turnClients)
            let seededA = fingerprint(seedModel.hourlyReport(for: turnClients))
                == fingerprint(originalA)
            await seedModel.ensureData(for: .hourly, clients: hourlyClients)
            let seededB = fingerprint(seedModel.hourlyReport(for: hourlyClients))
                == fingerprint(originalB)

            // A fresh popover restores A before its deliberately blocked
            // refresh completes, then the accepted newer result replaces A.
            let refreshSource = ControlledTurnUsageDataSource(hourlyResponses: [
                Set(turnClients): refreshedA,
            ])
            let (refreshModel, refreshTask, refreshPending) = await blockedModel(
                source: refreshSource, view: .hourly, clients: turnClients)
            let restoredABeforeRefresh =
                fingerprint(refreshModel.hourlyReport(for: turnClients)) == fingerprint(originalA)
            await refreshSource.releaseHourly(year: year)
            await refreshTask.value
            let acceptedRefreshVisible =
                fingerprint(refreshModel.hourlyReport(for: turnClients)) == fingerprint(refreshedA)

            // Fresh owners prove A's replacement did not overwrite sibling B.
            let verifyASource = ControlledTurnUsageDataSource(hourlyResponses: [
                Set(turnClients): refreshedA,
            ])
            let (verifyAModel, verifyATask, verifyAPending) = await blockedModel(
                source: verifyASource, view: .hourly, clients: turnClients)
            let refreshedARestored =
                fingerprint(verifyAModel.hourlyReport(for: turnClients)) == fingerprint(refreshedA)
            await verifyASource.releaseHourly(year: year)
            await verifyATask.value

            let verifyBSource = ControlledTurnUsageDataSource(hourlyResponses: [
                Set(hourlyClients): originalB,
            ])
            let (verifyBModel, verifyBTask, verifyBPending) = await blockedModel(
                source: verifyBSource, view: .hourly, clients: hourlyClients)
            let siblingBPreserved =
                fingerprint(verifyBModel.hourlyReport(for: hourlyClients)) == fingerprint(originalB)
            await verifyBSource.releaseHourly(year: year)
            await verifyBTask.value

            // An unseen client key cannot borrow either cached slice.
            let unseenClients = ["codex"]
            let unseenSource = ControlledTurnUsageDataSource(hourlyResponses: [
                Set(unseenClients): nonOwnerB,
            ])
            let (unseenModel, unseenTask, unseenPending) = await blockedModel(
                source: unseenSource, view: .hourly, clients: unseenClients)
            let unseenStayedEmpty = unseenModel.hourlyReport(for: unseenClients) == nil
            await unseenSource.releaseHourly(year: year)
            await unseenTask.value

            // Non-owners neither read A nor replace it with their local B.
            let nonOwnerSource = ControlledTurnUsageDataSource(hourlyResponses: [
                Set(turnClients): nonOwnerB,
            ])
            let (nonOwnerModel, nonOwnerTask, nonOwnerPending) = await blockedModel(
                source: nonOwnerSource, cachesSnapshot: false,
                view: .hourly, clients: turnClients)
            let nonOwnerDidNotRead = nonOwnerModel.hourlyReport(for: turnClients) == nil
            await nonOwnerSource.releaseHourly(year: year)
            await nonOwnerTask.value
            let nonOwnerReceivedLocalB =
                fingerprint(nonOwnerModel.hourlyReport(for: turnClients)) == fingerprint(nonOwnerB)

            let ownerAfterSource = ControlledTurnUsageDataSource(hourlyResponses: [
                Set(turnClients): refreshedA,
            ])
            let (ownerAfterModel, ownerAfterTask, ownerAfterPending) = await blockedModel(
                source: ownerAfterSource, view: .hourly, clients: turnClients)
            let nonOwnerDidNotWrite =
                fingerprint(ownerAfterModel.hourlyReport(for: turnClients)) == fingerprint(refreshedA)
            await ownerAfterSource.releaseHourly(year: year)
            await ownerAfterTask.value

            // Six more keys bring the cache to nine total slices; the fixed
            // eight-entry FIFO must evict A, which was inserted first.
            for suffix in 0..<6 {
                let extraYear = "205\(suffix)"
                let extraModel = DashboardModel(
                    cachesSnapshot: true, source: seedSource, initialYear: extraYear)
                await extraModel.load()
                await extraModel.ensureData(for: .hourly, clients: turnClients)
            }
            let evictionSource = ControlledTurnUsageDataSource(hourlyResponses: [
                Set(turnClients): refreshedA,
            ])
            await evictionSource.blockHourly(year: year)
            let evictionModel = DashboardModel(
                cachesSnapshot: true, source: evictionSource, initialYear: year)
            await evictionModel.load()
            let evictionTask = Task {
                await evictionModel.ensureData(for: .hourly, clients: turnClients)
            }
            let evictionPending = await waitUntil {
                await evictionSource.hasPendingHourly(year: year)
            }
            let oldestEvicted = evictionModel.hourlyReport(for: turnClients) == nil
            await evictionSource.releaseHourly(year: year)
            await evictionTask.value

            return [
                "fixturesDistinct": fixturesDistinct,
                "seededA": seededA,
                "seededB": seededB,
                "refreshPending": refreshPending,
                "restoredABeforeRefresh": restoredABeforeRefresh,
                "acceptedRefreshVisible": acceptedRefreshVisible,
                "verifyAPending": verifyAPending,
                "refreshedARestored": refreshedARestored,
                "verifyBPending": verifyBPending,
                "siblingBPreserved": siblingBPreserved,
                "unseenPending": unseenPending,
                "unseenStayedEmpty": unseenStayedEmpty,
                "nonOwnerPending": nonOwnerPending,
                "nonOwnerDidNotRead": nonOwnerDidNotRead,
                "nonOwnerReceivedLocalB": nonOwnerReceivedLocalB,
                "ownerAfterPending": ownerAfterPending,
                "nonOwnerDidNotWrite": nonOwnerDidNotWrite,
                "evictionPending": evictionPending,
                "oldestEvicted": oldestEvicted,
            ]
        }
        expect(
            hourlyCacheChecks?["fixturesDistinct"] == true
                && hourlyCacheChecks?["seededA"] == true
                && hourlyCacheChecks?["seededB"] == true
                && hourlyCacheChecks?["refreshPending"] == true
                && hourlyCacheChecks?["restoredABeforeRefresh"] == true
                && hourlyCacheChecks?["acceptedRefreshVisible"] == true,
            "hourly cache restores immediately and an accepted refresh replaces its exact key")
        expect(
            hourlyCacheChecks?["verifyAPending"] == true
                && hourlyCacheChecks?["refreshedARestored"] == true
                && hourlyCacheChecks?["verifyBPending"] == true
                && hourlyCacheChecks?["siblingBPreserved"] == true,
            "hourly cache keeps refreshed turn and all-client slices independent")
        expect(
            hourlyCacheChecks?["unseenPending"] == true
                && hourlyCacheChecks?["unseenStayedEmpty"] == true,
            "hourly cache never restores a report under an unseen client key")
        expect(
            hourlyCacheChecks?["nonOwnerPending"] == true
                && hourlyCacheChecks?["nonOwnerDidNotRead"] == true
                && hourlyCacheChecks?["nonOwnerReceivedLocalB"] == true
                && hourlyCacheChecks?["ownerAfterPending"] == true
                && hourlyCacheChecks?["nonOwnerDidNotWrite"] == true,
            "non-owning dashboard models neither read nor replace the popover hourly cache")
        expect(
            hourlyCacheChecks?["evictionPending"] == true
                && hourlyCacheChecks?["oldestEvicted"] == true,
            "hourly cache evicts its oldest slice after reaching eight entries")

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
        func messageOnlyDaily(_ client: String, _ date: String) -> String {
            """
            {"date":"\(date)","totals":{"tokens":0,"cost":0,"messages":1},"intensity":0,
             "tokenBreakdown":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0},
             "clients":[{"client":"\(client)","modelId":"m","providerId":"p","cost":0,"messages":1,
              "tokens":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0}}]}
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

        let messageTailPayload = rangeStatsPayload(end: "2026-07-31", days: [
            daily("vis", "2026-07-01", 1), messageOnlyDaily("vis", "2026-07-31"),
        ])
        let messageTailStats = UsageStats(payload: messageTailPayload, selectedClients: ["vis"])
        expect(
            messageTailStats.dateRange.end == "2026-07-31"
                && messageTailStats.streaks.current == 1,
            "a trailing message-only day remains current activity instead of resetting the streak")

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
        // DayBars derives its token/cost anchor from the selected series, so a
        // later non-metric range end cannot shift visible usage out of view.
        let shiftedBars = DayBars.build(
            payload: chartPayload, clientIds: ["vis"], stackBy: .agent,
            colors: chartColors, rangeEnd: "2026-07-05", endFallback: "2026-07-09")
        expect(shiftedBars.last?.date == "2026-07-03" && (shiftedBars.last?.totalTokens ?? 0) > 0,
            "chart derives its range end from selected token/cost activity")
        let messageTailBars = DayBars.build(
            payload: messageTailPayload, clientIds: ["vis"], stackBy: .agent,
            colors: chartColors, rangeEnd: messageTailStats.dateRange.end,
            endFallback: "2026-07-31")
        expect(
            messageTailBars.last?.date == "2026-07-01"
                && (messageTailBars.last?.totalTokens ?? 0) > 0,
            "a later message-only day does not shift the token/cost chart window")

        // Tooltip placement stays inside the visible ScrollView viewport, not
        // merely inside the source card. Region-dodge (container 0.45) prefers
        // above/below by cursor Y; viewport only clamps / rescues.
        let viewport = CGRect(x: 100, y: 200, width: 400, height: 300)
        let chartFrame = CGRect(x: 120, y: 250, width: 360, height: 150)
        let tooltipSize = CGSize(width: 210, height: 120)
        let belowPlacement = PopoverTooltipPlacement.offset(
            anchor: CGPoint(x: 180, y: 30), tooltipSize: tooltipSize,
            containerFrame: chartFrame, viewport: viewport)
        expect(belowPlacement?.height == 42,
            "upper-half anchor places tooltip below the cursor")
        let abovePlacement = PopoverTooltipPlacement.offset(
            anchor: CGPoint(x: 180, y: 140), tooltipSize: tooltipSize,
            containerFrame: chartFrame, viewport: viewport)
        expect(abovePlacement?.height == 8,
            "lower-half anchor places tooltip above the cursor")
        // Lower-half region even when the viewport still has room below —
        // the old "prefer below if fits" path felt like sticky follow.
        let regionDodge = PopoverTooltipPlacement.offset(
            anchor: CGPoint(x: 180, y: 100), tooltipSize: tooltipSize,
            containerFrame: chartFrame, viewport: viewport)
        expect(regionDodge?.height == -32,
            "lower-half still dodges above when the viewport has room below")
        let constrainedPlacement = PopoverTooltipPlacement.offset(
            anchor: CGPoint(x: 180, y: 110), tooltipSize: CGSize(width: 210, height: 260),
            containerFrame: chartFrame, viewport: viewport)
        expect(constrainedPlacement?.height == -46,
            "tooltip clamps to the side with more visible space")
        let edgePlacement = PopoverTooltipPlacement.offset(
            anchor: CGPoint(x: 2, y: 30), tooltipSize: tooltipSize,
            containerFrame: chartFrame, viewport: viewport)
        expect(edgePlacement?.width == 0,
            "tooltip clamps a near-edge anchor horizontally")
        let tallPlacement = PopoverTooltipPlacement.offset(
            anchor: CGPoint(x: 180, y: 30), tooltipSize: CGSize(width: 210, height: 400),
            containerFrame: chartFrame, viewport: viewport)
        expect(tallPlacement?.height == -46,
            "viewport-taller tooltip pins to the visible top inset")
        // Pre-resize viewport that no longer covers the card must not clamp.
        let staleViewport = CGRect(x: 100, y: 0, width: 400, height: 80)
        let stalePlacement = PopoverTooltipPlacement.offset(
            anchor: CGPoint(x: 180, y: 30), tooltipSize: tooltipSize,
            containerFrame: chartFrame, viewport: staleViewport)
        let containerOnly = PopoverTooltipPlacement.offset(
            anchor: CGPoint(x: 180, y: 30), tooltipSize: tooltipSize,
            containerFrame: chartFrame, viewport: nil)
        expect(stalePlacement?.height == containerOnly?.height,
            "stale non-intersecting viewport falls back to container-only")
        // Pre-resize short viewport that still overlaps the card but leaves
        // the hover anchor below it (grow handoff) must also fall back.
        let partialStale = CGRect(x: 100, y: 200, width: 400, height: 80)
        // chartFrame y=250..400; partialStale y=200..280 intersects the card
        // but anchor at local y=100 → global y=350 is outside partialStale.
        let partialPlacement = PopoverTooltipPlacement.offset(
            anchor: CGPoint(x: 180, y: 100), tooltipSize: tooltipSize,
            containerFrame: chartFrame, viewport: partialStale)
        let partialContainerOnly = PopoverTooltipPlacement.offset(
            anchor: CGPoint(x: 180, y: 100), tooltipSize: tooltipSize,
            containerFrame: chartFrame, viewport: nil)
        expect(partialPlacement?.height == partialContainerOnly?.height,
            "partial-overlap stale viewport that misses the anchor falls back")

        let chartWidth: CGFloat = 360
        let barGap = UsageChartGeometry.gap
        let barWidth = (chartWidth - barGap * 2) / 3
        let firstFrame = UsageChartGeometry.barFrame(index: 0, barWidth: barWidth)
        let secondFrame = UsageChartGeometry.barFrame(index: 1, barWidth: barWidth)
        let lastFrame = UsageChartGeometry.barFrame(index: 2, barWidth: barWidth)
        expect(firstFrame.minX == 0 && secondFrame.minX - firstFrame.maxX == barGap,
            "chart bar frames start at zero and leave draw gaps")
        expect(abs(lastFrame.maxX - chartWidth) < 0.0001,
            "last chart bar frame reaches the trailing edge")
        // Production hover uses floor(x / stride); gap pixels attach left.
        let gapX = firstFrame.maxX + barGap / 2
        expect(UsageChartGeometry.barIndex(atX: gapX, barWidth: barWidth, count: 3) == 0,
            "gap pixels floor-attach to the left bar")
        expect(UsageChartGeometry.barIndex(atX: secondFrame.minX, barWidth: barWidth, count: 3) == 1,
            "bar starts hit their own index")
        expect(UsageChartGeometry.barIndex(atX: lastFrame.maxX + barGap + 1, barWidth: barWidth, count: 3) == nil,
            "past the last stride is out of range")

        let modelWidths = ModelBarGeometry.widths(
            values: [1_000_000, 1, 1, 1, 1], totalWidth: 120)
        let renderedModelWidth = modelWidths.reduce(0, +)
            + ModelBarGeometry.gap * CGFloat(modelWidths.count - 1)
        expect(
            abs(renderedModelWidth - 120) < 0.0001
                && modelWidths.dropFirst().allSatisfy { $0 >= 1 },
            "model bar widths preserve tiny segments without trailing overflow")

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
        for (label, passed) in TBCore.filterParityContractChecks() {
            expect(passed, "filter parity: \(label)")
        }

        // MARK: - FLAT-HEATMAP (append-only section; do not reorder/edit above)

        // A1/A2: the heatmap grid must read the exact same, already-filtered
        // `stats.perDayMap` UsageChartCard hands ContributionGraph3D — same
        // pipeline, same values, and NOT the unfiltered payload total.
        let heatJSON = """
        {"meta":{"generatedAt":"now","version":"1","dateRange":{"start":"2026-01-01","end":"2026-01-01"}},
         "summary":{"totalTokens":0,"totalCost":0,"totalDays":1,"activeDays":1,"averagePerDay":0,
                    "maxCostInSingleDay":0,"clients":["a","b"],"models":[]},
         "years":[],
         "contributions":[
           {"date":"2026-01-01","totals":{"tokens":0,"cost":0,"messages":0},"intensity":1,
            "tokenBreakdown":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0},
            "clients":[
              {"client":"a","modelId":"m","providerId":"p","cost":2,"messages":1,
               "tokens":{"input":100,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0}},
              {"client":"b","modelId":"m","providerId":"p","cost":3,"messages":1,
               "tokens":{"input":50,"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0}}]}
         ]}
        """
        let heatPayload = try! JSONDecoder().decode(UsagePayload.self, from: Data(heatJSON.utf8))
        let heatStatsA = UsageStats(payload: heatPayload, selectedClients: ["a"])
        let heatGridA = buildGrid(year: "2026", perDayMap: heatStatsA.perDayMap)
        let heatCellA = heatGridA.cells.first { $0.date == "2026-01-01" }
        expect(
            heatCellA?.tokens == 100 && heatCellA?.cost == 2,
            "heatmap grid cell matches the filtered UsageStats value for the selected client")
        expect(
            (heatCellA?.tokens ?? 0) != 150 && (heatCellA?.cost ?? 0) != 5,
            "heatmap grid cell for one client is not the two-client total")

        // `maxValue` gained a `cutoff` parameter in round 4 (FIX 3); this
        // constant preserves every existing A3/A4 fixture's original
        // semantics (nothing excluded) rather than weakening what they test.
        let noCutoffFilter = "9999-12-31"

        // A3 (invariant 3): a `tokens == 0, cost > 0` day must count as "has
        // data" under the Price metric. This is reachable (UsageStats.swift
        // 105-110) and `cell.active` (Grid.swift:49) is tokens-only — using
        // it here would wrongly blank this day out.
        let costOnlyGrid = buildGrid(
            year: "2026",
            perDayMap: ["2026-05-05": PerDay(date: "2026-05-05", tokens: 0, cost: 5, intensity: 1)])
        let costOnlyCell = costOnlyGrid.cells.first { $0.date == "2026-05-05" }!
        expect(costOnlyCell.active == false, "sanity: a cost-only day is not `active` (tokens-only flag)")
        expect(
            ContributionHeatmap.hasData(costOnlyCell, metric: .cost) == true,
            "Price metric treats cost>0 as data even when active==false")
        expect(
            ContributionHeatmap.hasData(costOnlyCell, metric: .tokens) == false,
            "Tokens metric still has no data on a cost-only day")
        let costOnlyMax = ContributionHeatmap.maxValue(costOnlyGrid, metric: .cost, cutoff: noCutoffFilter)
        expect(
            HeatmapLayout.level(
                value: ContributionHeatmap.value(costOnlyCell, metric: .cost), max: costOnlyMax) >= 1,
            "cost-only day renders at a non-zero heatmap intensity level")

        // A4 (invariant 4): Tokens and Price take their intensity denominator
        // independently — the day with the most tokens need not be the day
        // with the highest cost, and each metric's own top day must still
        // reach the top intensity level under its own max.
        let dualMetricGrid = buildGrid(
            year: "2026",
            perDayMap: [
                "2026-03-01": PerDay(date: "2026-03-01", tokens: 1000, cost: 1, intensity: 1),
                "2026-03-08": PerDay(date: "2026-03-08", tokens: 10, cost: 100, intensity: 1),
            ])
        let dayHighTokens = dualMetricGrid.cells.first { $0.date == "2026-03-01" }!
        let dayHighCost = dualMetricGrid.cells.first { $0.date == "2026-03-08" }!
        let dualMaxTokens = ContributionHeatmap.maxValue(dualMetricGrid, metric: .tokens, cutoff: noCutoffFilter)
        let dualMaxCost = ContributionHeatmap.maxValue(dualMetricGrid, metric: .cost, cutoff: noCutoffFilter)
        expect(
            dualMaxTokens == 1000 && dualMaxCost == 100,
            "tokens and cost maxima are computed independently, from different days")
        expect(
            HeatmapLayout.level(
                value: ContributionHeatmap.value(dayHighTokens, metric: .tokens), max: dualMaxTokens) == 4
                && HeatmapLayout.level(
                    value: ContributionHeatmap.value(dayHighCost, metric: .cost), max: dualMaxCost) == 4,
            "each metric's own top day reaches the highest intensity level")
        expect(
            HeatmapLayout.level(
                value: ContributionHeatmap.value(dayHighCost, metric: .tokens), max: dualMaxTokens) < 4,
            "the cost-max day is not also the tokens-max day (cost wrongly reusing maxTokens would fail this)")

        // Five-level threshold boundaries (invariant 9): >=0.75/0.5/0.25/>0/else.
        expect(
            HeatmapLayout.level(value: 75, max: 100) == 4
                && HeatmapLayout.level(value: 50, max: 100) == 3
                && HeatmapLayout.level(value: 25, max: 100) == 2
                && HeatmapLayout.level(value: 1, max: 100) == 1
                && HeatmapLayout.level(value: 0, max: 100) == 0
                && HeatmapLayout.level(value: 10, max: 0) == 0,
            "five-level intensity thresholds match >=0.75/0.5/0.25/>0/else")

        // A5/A6: calendar boundaries across years, including a leap day.
        // `buildGrid` clamps to `max(53, …)`, so every real year lands on 53
        // or 54 columns; 2028 is the nearest 54-column year to today.
        expect(buildGrid(year: "2026", perDayMap: [:]).cols == 53, "2026 uses the standard 53 columns")
        expect(buildGrid(year: "2028", perDayMap: [:]).cols == 54, "2028 needs a 54th column")
        let leapGrid = buildGrid(
            year: "2028",
            perDayMap: ["2028-02-29": PerDay(date: "2028-02-29", tokens: 1, cost: 0, intensity: 1)])
        let leapCell = leapGrid.cells.first { $0.date == "2028-02-29" }
        expect(
            leapCell?.inYear == true && leapCell?.active == true,
            "2028-02-29 is a valid in-year, active cell (pure ISODay stepping, no Calendar)")

        // A7 (invariant 7): `chartViewRaw` fallback is exhaustive, not an
        // ad hoc `!is3D && !isHeatmap` chain — any unknown value, not just
        // the ones tested here, falls back to Bars.
        expect(ChartView(raw: "2d") == .bars, "legacy '2d' still maps to Bars (no migration needed)")
        expect(ChartView(raw: "3d") == .threeD, "legacy '3d' still maps to 3D")
        expect(ChartView(raw: "heat") == .heatmap, "new 'heat' value maps to Heatmap")
        expect(ChartView(raw: "garbage") == .bars, "an unknown chartViewRaw falls back to Bars, not a crash")

        // MARK: - FLAT-HEATMAP round 2 (append-only; do not reorder/edit above)

        // Item 3(a): future-day cutoff. Current year clips to today; any
        // other (necessarily past) year still runs through Dec 31.
        expect(
            ContributionHeatmap.cutoffDate(year: "2026", today: "2026-07-29") == "2026-07-29",
            "the selected year matching today's year cuts off at today")
        expect(
            ContributionHeatmap.cutoffDate(year: "2025", today: "2026-07-29") == "2025-12-31",
            "a past selected year still runs through Dec 31, not today's date")

        let cutoffCurrent = ContributionHeatmap.cutoffDate(year: "2026", today: "2026-07-29")
        let currentYearGrid = buildGrid(year: "2026", perDayMap: [:])
        let renderableCurrent = currentYearGrid.cells
            .filter { ContributionHeatmap.isRenderable($0, cutoff: cutoffCurrent) }
            .map(\.date)
        expect(
            renderableCurrent.max() == "2026-07-29" && !renderableCurrent.contains("2026-07-30"),
            "the current year renders through today and no further (a `<` vs `<=` slip would fail this)")

        let cutoffPast = ContributionHeatmap.cutoffDate(year: "2025", today: "2026-07-29")
        let pastYearGrid = buildGrid(year: "2025", perDayMap: [:])
        let renderablePast = pastYearGrid.cells
            .filter { ContributionHeatmap.isRenderable($0, cutoff: cutoffPast) }
            .map(\.date)
        expect(
            renderablePast.max() == "2025-12-31",
            "a past year still renders all the way to Dec 31 (forgetting the year check would clip it to today's date)")

        // Item 1: the tooltip's anchor must be derived from the scrolling
        // content's *current* on-screen origin, not pinned to the cell's
        // position within that content alone — that pin is exactly the old
        // clipping bug (tooltip position never accounted for scroll, so it
        // rendered inside the ScrollView's own clipped content layer). This
        // is the pure-logic slice of the fix; the actual on-screen clip
        // behavior needs a human looking at the popover (A9-equivalent).
        expect(
            ContributionHeatmap.tooltipAnchor(cellCenter: CGPoint(x: 50, y: 20), contentOrigin: .zero)
                == CGPoint(x: 50, y: 20),
            "an unscrolled, unmoved content anchors directly on the cell's own center")
        expect(
            ContributionHeatmap.tooltipAnchor(
                cellCenter: CGPoint(x: 50, y: 20), contentOrigin: CGPoint(x: -300, y: 0))
                == CGPoint(x: -250, y: 20),
            "scrolling the content 300pt left shifts the anchor by the same 300pt — proving the tooltip "
                + "tracks the outer container, not a position frozen inside the scrolled/clipped content")

        // MARK: - FLAT-HEATMAP round 3 (append-only; do not reorder/edit above)

        // Layout width (and hit-testing) must derive from the last
        // RENDERABLE column, not `grid.cols` — round 2 correctly stopped
        // drawing/hovering future days but left `grid.cols` driving the
        // layout width, so the blank cutoff-past columns still ate width and
        // `scrollTo(.trailing)` landed on empty space instead of today.
        let r3Today = "2026-07-29"
        let r3CurrentYearGrid = buildGrid(year: "2026", perDayMap: [:])
        let r3TodayCell = r3CurrentYearGrid.cells.first { $0.date == r3Today }!
        // September, not August: July 29 (a Wednesday) and Aug 1 fall in the
        // same Sunday-Saturday week/column, which would make the "later
        // column" assertion below vacuously true regardless of the fix.
        let r3SeptemberCell = r3CurrentYearGrid.cells.first { $0.date == "2026-09-01" }!
        let r3LastColCurrent = ContributionHeatmap.lastRenderableCol(r3CurrentYearGrid, cutoff: r3Today)
        expect(
            r3LastColCurrent == r3TodayCell.col,
            "the current year's last renderable column is today's column, not the last column of the year")
        expect(
            r3LastColCurrent < r3SeptemberCell.col,
            "a column after today contributes no width (using grid.cols here would fail this)")

        // Note: a mutated `cutoffDate` that always returns `today` regardless
        // of year (the round-2 mutation target) does NOT fail this specific
        // assertion — a past year's dates all lexicographically precede a
        // current-year "today" string, so that particular bug still yields
        // full width here by coincidence; it's caught instead by round 2's
        // own "past selected year still runs through Dec 31" test above. This
        // assertion's real mutation target is a wrong past-year end date
        // (e.g. `"\(year)-01-01"` instead of `"\(year)-12-31"`), which does
        // narrow the width and does fail here.
        let r3PastCutoff = ContributionHeatmap.cutoffDate(year: "2025", today: r3Today)
        let r3PastYearGrid = buildGrid(year: "2025", perDayMap: [:])
        let r3LastColPast = ContributionHeatmap.lastRenderableCol(r3PastYearGrid, cutoff: r3PastCutoff)
        expect(
            r3LastColPast == r3PastYearGrid.cols - 1,
            "a past year still spans the full grid width (a wrong past-year cutoff end date would narrow it)")

        // Month labels must stop at the same cutoff as the cells — calling
        // the real `monthLabelCols(grid:cutoff:)`, not a hand-rebuilt copy of
        // its filter, so dropping the cutoff filter inside it would be caught.
        let r3JulyFirstCell = r3CurrentYearGrid.cells.first { $0.date == "2026-07-01" }!
        let r3MonthLabelCols = ContributionHeatmap.monthLabelCols(grid: r3CurrentYearGrid, cutoff: r3Today)
            .map(\.col)
        expect(
            r3MonthLabelCols.contains(r3JulyFirstCell.col),
            "July's label (on or before the cutoff) is still present")
        expect(
            !r3MonthLabelCols.contains(r3SeptemberCell.col),
            "September's label (after the cutoff) is dropped (mutation: skipping the isRenderable filter "
                + "inside monthLabelCols would fail this)")

        // MARK: - FLAT-HEATMAP round 4 (Codex P2 fixes; append-only)

        // FIX 1: the re-scroll-to-trailing trigger is `cutoff`, which changes
        // on both a year-filter change and a day rollover — this is the pure,
        // testable half of the fix. The actual SwiftUI `onChange(of:
        // cutoff)` → `proxy.scrollTo` wiring firing at the right time needs a
        // human watching the popover switch years while on the Heatmap tab;
        // there's no headless SwiftUI view-update harness here to automate
        // that half.
        expect(
            ContributionHeatmap.cutoffDate(year: "2026", today: "2026-07-29")
                != ContributionHeatmap.cutoffDate(year: "2025", today: "2026-07-29"),
            "cutoff changes across a year-filter switch (the re-scroll trigger fires)")
        expect(
            ContributionHeatmap.cutoffDate(year: "2026", today: "2026-07-29")
                != ContributionHeatmap.cutoffDate(year: "2026", today: "2026-07-30"),
            "cutoff also changes across a day rollover while the popover stays open")

        // FIX 2: a horizontal wheel-redirect already parked at an edge must
        // report "not consumed" so the dashboard's vertical ScrollView still
        // sees the wheel tick — the pre-fix code clamped and unconditionally
        // reported the event as handled even when the clamped origin was
        // identical to the one it started with.
        let r4RightEdge = HorizontalWheelScroll.clampedScroll(originX: 500, step: -20, maxX: 500)
        expect(
            r4RightEdge.newOriginX == 500 && r4RightEdge.moved == false,
            "already at the trailing edge: origin doesn't move, so the event is not consumed "
                + "(mutation: always returning moved=true would fail this)")
        let r4LeftEdge = HorizontalWheelScroll.clampedScroll(originX: 0, step: 20, maxX: 500)
        expect(
            r4LeftEdge.newOriginX == 0 && r4LeftEdge.moved == false,
            "already at the leading edge: origin doesn't move, so the event is not consumed")
        let r4MidScroll = HorizontalWheelScroll.clampedScroll(originX: 100, step: 20, maxX: 500)
        expect(
            r4MidScroll.newOriginX == 80 && r4MidScroll.moved == true,
            "a scroll that actually changes the origin IS consumed")

        // FIX 3: a hidden future cell (clock skew, an imported session dated
        // past today) must not sit in either metric's intensity denominator
        // — the same `isRenderable` cutoff that keeps it from being drawn or
        // hoverable must also keep it out of `maxValue`.
        let r4FutureShockGrid = buildGrid(
            year: "2026",
            perDayMap: [
                "2026-07-10": PerDay(date: "2026-07-10", tokens: 100, cost: 5, intensity: 1),
                "2026-08-15": PerDay(date: "2026-08-15", tokens: 999_999, cost: 9999, intensity: 1),
            ])
        let r4Cutoff = "2026-07-29"
        let r4VisibleCell = r4FutureShockGrid.cells.first { $0.date == "2026-07-10" }!
        let r4TokensMax = ContributionHeatmap.maxValue(r4FutureShockGrid, metric: .tokens, cutoff: r4Cutoff)
        let r4CostMax = ContributionHeatmap.maxValue(r4FutureShockGrid, metric: .cost, cutoff: r4Cutoff)
        expect(
            r4TokensMax == 100 && r4CostMax == 5,
            "a hidden future day's huge values don't enter either metric's intensity denominator")
        expect(
            HeatmapLayout.level(
                value: ContributionHeatmap.value(r4VisibleCell, metric: .tokens), max: r4TokensMax) == 4
                && HeatmapLayout.level(
                    value: ContributionHeatmap.value(r4VisibleCell, metric: .cost), max: r4CostMax) == 4,
            "the only visible day still renders at full intensity (mutation: reverting the tokens branch "
                + "to `grid.maxTokens` or the cost branch to an unfiltered reduce would crush this)")

        // MARK: - FLAT-HEATMAP round 5 (Codex P2 fix + audit; append-only)

        // FIX: a FUTURE selected year (reachable if clock skew or an
        // imported session put activity there, so it shows up in the year
        // picker) must render nothing, not the whole year — the old two-way
        // `year == currentYear ? today : "\(year)-12-31"` treated every
        // non-current year as past.
        expect(
            ContributionHeatmap.cutoffDate(year: "2026", today: "2026-07-29") == "2026-07-29",
            "the current year still cuts off at today")
        expect(
            ContributionHeatmap.cutoffDate(year: "2025", today: "2026-07-29") == "2025-12-31",
            "a past year still cuts off at its own Dec 31")
        expect(
            ContributionHeatmap.cutoffDate(year: "2027", today: "2026-07-29") == "2026-07-29",
            "a future year cuts off at today too (mutation: the old `year == currentYear ? today : "
                + "\"\\(year)-12-31\"` two-way branch would return \"2027-12-31\" here and fail this)")

        let r5FutureYearGrid = buildGrid(year: "2027", perDayMap: [:])
        let r5FutureCutoff = ContributionHeatmap.cutoffDate(year: "2027", today: "2026-07-29")
        expect(
            ContributionHeatmap.lastRenderableCol(r5FutureYearGrid, cutoff: r5FutureCutoff) == -1,
            "a future year has zero renderable columns")

        // Zero renderable columns (the future-year case just established, or
        // any grid where nothing passes the cutoff) must not produce a
        // negative canvas width.
        expect(
            ContributionHeatmap.contentWidth(visibleCols: 0, monthLabelCols: []) == 0,
            "zero visible columns is zero width, not a negative width from `0 * step - gap` "
                + "(mutation: dropping the `visibleCols > 0` guard would fail this)")
        expect(
            ContributionHeatmap.contentWidth(visibleCols: 3, monthLabelCols: []) > 0,
            "sanity: a normal, nonzero column count still produces a positive width")

        // MARK: - FLAT-HEATMAP round 6 (Codex round 3 P2 fixes + audit; append-only)

        // FIX 1: `ChartView.next` owns the ⌘G cycle order. The regression
        // this guards was specifically that Heatmap couldn't be distinguished
        // from 3D by the old handler, so it's the heatmap→threeD step (not
        // just "the cycle eventually returns") that matters most here.
        expect(ChartView.bars.next == .heatmap, "cycle: Bars -> Heatmap")
        expect(
            ChartView.heatmap.next == .threeD,
            "cycle: Heatmap -> 3D, not back to Bars — this is exactly the regression: the old handler's "
                + "binary `chartViewRaw == \"2d\" ? \"3d\" : \"2d\"` treated Heatmap the same as \"any "
                + "non-2d value\" and always landed on 3D, then only ever toggled Bars<->3D afterward, so "
                + "a keyboard user starting on Heatmap could never cycle back to it")
        expect(ChartView.threeD.next == .bars, "cycle: 3D -> Bars, closing the loop")
        expect(
            ChartView.bars.next.next.next == .bars,
            "three ⌘G presses from any state return to that same state")

        // FIX 2: contentWidth gains the trailing margin ONLY when the LAST
        // renderable column itself has a month label.
        let r6BaseWidth = ContributionHeatmap.contentWidth(visibleCols: 5, monthLabelCols: [])
        let r6TrailingLabelWidth = ContributionHeatmap.contentWidth(
            visibleCols: 5, monthLabelCols: [(col: 4, label: "Sep")])
        expect(
            r6TrailingLabelWidth == r6BaseWidth + HeatmapLayout.lastColumnLabelMargin,
            "a label landing in the last renderable column adds exactly the named margin")
        let r6MidLabelWidth = ContributionHeatmap.contentWidth(
            visibleCols: 5, monthLabelCols: [(col: 2, label: "Jul")])
        expect(
            r6MidLabelWidth == r6BaseWidth,
            "a label on a column that ISN'T the last one adds no margin (mutation: adding the margin "
                + "whenever monthLabelCols is merely non-empty, instead of checking the last column "
                + "specifically, would fail this)")

        // FIX 3: gap coordinates are dead zones (unlike the bar chart's
        // intentional gap-attaches-to-the-left-bar rule); horizontal and
        // vertical boundaries both tested at the cell's last valid pixel and
        // the gap's first pixel.
        let r6Cell = HeatmapLayout.cell
        let r6Step = HeatmapLayout.step
        expect(
            ContributionHeatmap.withinCell(offset: 0, step: r6Step, cell: r6Cell),
            "the first pixel of a cell is inside it")
        expect(
            ContributionHeatmap.withinCell(offset: r6Cell - 0.1, step: r6Step, cell: r6Cell),
            "the last valid pixel just before the gap is still inside the cell")
        expect(
            !ContributionHeatmap.withinCell(offset: r6Cell, step: r6Step, cell: r6Cell),
            "the first pixel of the gap is rejected (mutation: dropping the `< cell` check, i.e. always "
                + "returning true, would fail this)")
        expect(
            !ContributionHeatmap.withinCell(offset: r6Step - 0.1, step: r6Step, cell: r6Cell),
            "the last pixel of the gap, right before the next cell, is still rejected")
        expect(
            ContributionHeatmap.withinCell(offset: r6Step, step: r6Step, cell: r6Cell),
            "the first pixel of the NEXT cell is inside it again")
        expect(
            ContributionHeatmap.withinCell(offset: r6Step + r6Cell - 0.1, step: r6Step, cell: r6Cell),
            "the second cell's last valid pixel is inside it")
        expect(
            !ContributionHeatmap.withinCell(offset: r6Step + r6Cell, step: r6Step, cell: r6Cell),
            "the second cell's gap is rejected too")

        // MARK: - FLAT-HEATMAP round 7 (Codex round 4 P2 fix + audit; append-only)

        // FIX: `shouldClearHoverOnOriginChange` is the pure half of "clear
        // hover when the content actually scrolled, not on every incidental
        // re-layout". The `onGeometryChange` → `hoverIndex = nil` wiring
        // itself firing at the right moment during a live scroll has no
        // headless SwiftUI harness here and is manual-verification-only.
        expect(
            !ContributionHeatmap.shouldClearHoverOnOriginChange(
                old: CGPoint(x: 10, y: 20), new: CGPoint(x: 10, y: 20)),
            "an unchanged origin never clears the hover (mutation: always returning true here would "
                + "make hover impossible to establish at all, since the geometry modifier's initial call "
                + "would immediately clear it)")
        expect(
            ContributionHeatmap.shouldClearHoverOnOriginChange(
                old: CGPoint(x: 10, y: 20), new: CGPoint(x: 40, y: 20)),
            "a changed origin (e.g. a redirected wheel scroll) clears the hover (mutation: always "
                + "returning false would leave a stale tooltip pinned through a scroll — the original bug)")
        expect(
            ContributionHeatmap.shouldClearHoverOnOriginChange(
                old: CGPoint(x: 10, y: 20), new: CGPoint(x: 10, y: 5)),
            "a vertical-only origin change also clears the hover")

        // MARK: - FLAT-HEATMAP round 8 (perf regression fix; append-only)

        // The scroll-perf fix: measuring `contentOrigin` in a coordinate
        // space anchored to the OUTER container (instead of `.global`)
        // means a shared ancestor translation — the dashboard's own
        // vertical ScrollView scrolling — cancels out, because both the
        // content's and the container's `.global` positions shift by the
        // SAME delta. This models that arithmetic directly: two `.global`
        // snapshots of content/container before an ancestor scroll, and two
        // after a 150pt vertical shift applied to BOTH.
        let r8ContentGlobalBefore = CGPoint(x: 40, y: 320)
        let r8ContainerGlobalBefore = CGPoint(x: 20, y: 300)
        let r8AncestorScrollDelta: CGFloat = 150
        let r8ContentGlobalAfter = CGPoint(
            x: r8ContentGlobalBefore.x, y: r8ContentGlobalBefore.y - r8AncestorScrollDelta)
        let r8ContainerGlobalAfter = CGPoint(
            x: r8ContainerGlobalBefore.x, y: r8ContainerGlobalBefore.y - r8AncestorScrollDelta)
        expect(
            r8ContentGlobalBefore != r8ContentGlobalAfter,
            "sanity: the raw `.global` position genuinely changes during the ancestor scroll — this is "
                + "exactly why tracking `.global` fired `onGeometryChange`'s action, and therefore wrote "
                + "state, on every single frame of a scroll this view had no other stake in")
        func relative(content: CGPoint, container: CGPoint) -> CGPoint {
            CGPoint(x: content.x - container.x, y: content.y - container.y)
        }
        let r8RelativeBefore = relative(content: r8ContentGlobalBefore, container: r8ContainerGlobalBefore)
        let r8RelativeAfter = relative(content: r8ContentGlobalAfter, container: r8ContainerGlobalAfter)
        expect(
            r8RelativeBefore == r8RelativeAfter,
            "content's position relative to its container is invariant under a shared ancestor "
                + "translation — this is exactly the value a coordinate space anchored to the container "
                + "reports directly, instead of two independent `.global` values that must be subtracted")
        expect(
            !ContributionHeatmap.shouldClearHoverOnOriginChange(old: r8RelativeBefore, new: r8RelativeAfter),
            "so a pure vertical ancestor scroll correctly does NOT clear the hover or write state "
                + "(mutation: if the container-relative value were computed wrong — e.g. only the "
                + "content's delta and not the container's — this would go red)")

        // Contrast: a genuine HORIZONTAL scroll of this grid's own content
        // (the container does not move) must still change the relative
        // origin, so hover keeps clearing correctly for the case that
        // actually matters (round 7's fix).
        let r8HScrolledContentGlobal = CGPoint(x: r8ContentGlobalBefore.x - 60, y: r8ContentGlobalBefore.y)
        let r8HScrolledRelative = relative(content: r8HScrolledContentGlobal, container: r8ContainerGlobalBefore)
        expect(
            r8HScrolledRelative != r8RelativeBefore,
            "a genuine horizontal content scroll DOES change the container-relative origin")
        expect(
            ContributionHeatmap.shouldClearHoverOnOriginChange(old: r8RelativeBefore, new: r8HScrolledRelative),
            "...and therefore still clears the hover, same as before this round's fix")

        // MARK: - Tray frame aspect (append-only section)

        // `anim-parrot` art is 48x36. `loadFrames` used to assign 18x18
        // unconditionally, stretching it vertically wherever `NSImage.size`
        // is what renders — `button.image`, i.e. static tray mode and the
        // Settings preview. The animation never showed it, because
        // `rasterizedFrame` fits by the representation's PIXEL size and
        // ignores the logical size, so the two paths disagreed about the
        // same asset. These assert the two now agree.
        func trayArt(_ directory: String) -> NSImage? {
            Bundle.tokenBarResources.url(
                forResource: "frame-00", withExtension: "png", subdirectory: directory
            ).flatMap(NSImage.init(contentsOf:))
        }
        let parrotArt = trayArt("anim-parrot")
        let catArt = trayArt("anim-cat2")
        expect(parrotArt != nil && catArt != nil, "tray frame art loads")

        if let parrot = parrotArt.map({ art -> NSImage in
            art.size = TrayAnimator.barSize(for: art)
            return art
        }) {
            // 48x36 fitted into 18x18 is 18x13.5, not 18x18.
            expect(
                abs(parrot.size.width - 18) < 0.01 && abs(parrot.size.height - 13.5) < 0.01,
                "non-square tray art keeps its aspect ratio (mutation: assigning the 18x18 box "
                    + "unconditionally, as before, makes this 18x18 and fails)")
            // The distortion this guards against: a stretched frame reports a
            // 1:1 logical box for art that is 4:3.
            expect(
                abs(parrot.size.width / parrot.size.height - 48.0 / 36.0) < 0.01,
                "the logical box matches the art's own 4:3 ratio")
        }
        if let cat = catArt {
            // Square art is unchanged by the fix — a no-trigger guard case.
            let size = TrayAnimator.barSize(for: cat)
            expect(
                abs(size.width - 18) < 0.01 && abs(size.height - 18) < 0.01,
                "square tray art still fills the full 18x18 box")
        }
        // The raster path must stay driven by pixels, not by the logical size
        // we just changed — otherwise this fix would silently resize the
        // animation too.
        //
        // `rasterizedFrameMetricsForTesting` is declared `#if DEBUG`, and
        // `scripts/bundle.sh` builds with `swift build -c release`, so an
        // unguarded call here compiles under `make build`/`make selftest`/CI
        // — all debug — and then fails the release build at tag time. The
        // older raster assertions above sit in their own `#if DEBUG` block
        // for exactly this reason.
#if DEBUG
        if let parrot = parrotArt {
            let raster = StatusItemAnimationSurface.rasterizedFrameMetricsForTesting(parrot, scale: 2)
            expect(
                raster?.pixelSize == CGSize(width: 36, height: 36),
                "the animation raster is still a square 18pt box at 2x, unaffected by the logical size")
        }
#endif

        // MARK: - Discord Rich Presence payload (DISCORD-PRESENCE M1)
        //
        // This payload is published to a third party and shown on a public
        // profile, so these are privacy regression guards, not display tests.
        // Every assertion goes through `DiscordPresence.payload(...)` — the
        // published bytes — not through the core fold alone.
        func dpGraph(_ json: String) -> UsagePayload {
            try! JSONDecoder().decode(UsagePayload.self, from: Data(json.utf8))
        }
        func dpDay(_ date: String, _ tokens: Int64, _ cost: Double, _ stripes: String) -> String {
            """
            {"date":"\(date)","totals":{"tokens":\(tokens),"cost":\(cost),"messages":1},
             "intensity":1,
             "tokenBreakdown":{"input":\(tokens),"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0},
             "clients":[\(stripes)]}
            """
        }
        func dpStripe(
            _ client: String, _ tokens: Int64, _ cost: Double,
            model: String = "m", provider: String = "p"
        ) -> String {
            """
            {"client":"\(client)","modelId":"\(model)","providerId":"\(provider)",
             "cost":\(cost),"messages":1,
             "tokens":{"input":\(tokens),"output":0,"cacheRead":0,"cacheWrite":0,"reasoning":0}}
            """
        }
        func dpPayload(_ days: String, summaryTokens: Int64 = 0, summaryCost: Double = 0) -> String {
            """
            {"meta":{"generatedAt":"now","version":"1",
                     "dateRange":{"start":"2026-08-04","end":"2026-08-04"}},
             "summary":{"totalTokens":\(summaryTokens),"totalCost":\(summaryCost),"totalDays":1,
                        "activeDays":1,"averagePerDay":0,"maxCostInSingleDay":0,
                        "clients":[],"models":[]},
             "years":[],"contributions":[\(days)]}
            """
        }
        let dpToday = "2026-08-04"

        // A3a — the hidden set reaches the published payload. The day-level
        // `totals` here (1.2M) deliberately differs from the visible-only sum
        // (200K): a builder that read `Contribution.totals`/`tokenBreakdown`
        // instead of the surviving stripes cannot subtract a client and would
        // publish "1.2M".
        let dpHiddenGraph = dpGraph(dpPayload(dpDay(
            dpToday, 1_200_000, 6.0,
            dpStripe("claude", 1_000_000, 5.0) + "," + dpStripe("codex", 200_000, 1.0))))
        let dpHidden = DiscordPresence.payload(
            graph: dpHiddenGraph, hidden: ["claude"], today: dpToday)
        expect(dpHidden?.details == "200K tokens today",
            "discord payload tokens exclude the hidden client (mutation: reading the day-level "
                + "totals publishes 1.2M)")
        expect(dpHidden.map { !$0.fields.values.joined().contains("1.2M") } == true,
            "discord payload never carries the mixed day-level total")
        expect(dpHidden?.state.contains("Codex CLI") == true
            && dpHidden?.state.contains("Claude Code") == false,
            "discord top client skips the hidden client (mutation: dropping the hidden filter "
                + "from the fold publishes Claude Code)")

        // A4 — outbound label allowlist. The busiest visible client is an
        // UNREGISTERED id whose suffix comes from a user's local config file;
        // a second, hidden client carries a sentinel too. Nothing may escape.
        let dpSecretGraph = dpGraph(dpPayload(dpDay(
            dpToday, 1_400_000, 12.0,
            dpStripe("cc-mirror/SECRET_VARIANT", 500_000, 3.0,
                model: "SECRET_MODEL", provider: "SECRET_PROVIDER")
                + "," + dpStripe("SECRET_HIDDEN", 900_000, 9.0,
                    model: "SECRET_MODEL2", provider: "SECRET_PROVIDER2"))))
        let dpSecret = DiscordPresence.payload(
            graph: dpSecretGraph, hidden: ["SECRET_HIDDEN"], today: dpToday)
        let dpSecretText = (dpSecret?.fields.values ?? [:].values).joined(separator: "|")
        expect(dpSecret != nil && !dpSecretText.contains("SECRET_"),
            "discord payload redacts an unregistered client id (mutation: using "
                + "ClientRegistry.style().displayName without the allowlist gate leaks it)")
        // Literal, not `DiscordPresence.neutralClientLabel`: an assertion whose
        // expected value is read out of the constant it guards passes no matter
        // what that constant becomes.
        expect(dpSecret?.state.contains("an AI tool") == true,
            "an unregistered top client publishes the neutral label")
        // A5 — forbidden fields. modelId/providerId values are covered by the
        // SECRET_ assertion above; these cover shapes rather than values.
        expect(!dpSecretText.contains("/"),
            "discord payload carries no path-like segment (mutation: adding any raw-id or path "
                + "field to Payload.fields fails here)")
        // Pinned by KEY on the published surface itself. `fields` is what the
        // transport serializes, so this covers exactly what leaves the machine:
        // a new outbound field must add a key here and fails, while a property
        // that is not in `fields` publishes nothing and correctly does not.
        // Asserting over anything else — a parallel list of values, or
        // reflection over the stored properties — reintroduces the gap between
        // the published surface and the asserted one that let a computed
        // `startTimestamp` through.
        expect(dpSecret.map { Set($0.fields.keys) } == ["details", "state", "largeImageKey"],
            "the discord payload publishes exactly three named fields "
                + "(mutation: adding a key to Payload.fields, or dropping one, fails here)")

        // A11 — published granularity. Exact figures must not survive: neither
        // the raw token count nor cent-precision cost.
        let dpGrainGraph = dpGraph(dpPayload(dpDay(
            dpToday, 1_234_567, 7.89, dpStripe("claude", 1_234_567, 7.89))))
        let dpGrain = DiscordPresence.payload(graph: dpGrainGraph, hidden: [], today: dpToday)
        let dpGrainText = (dpGrain?.fields.values ?? [:].values).joined(separator: "|")
        expect(dpGrain?.details == "1.2M tokens today",
            "discord tokens publish as a compact string (mutation: String(todayTokens) fails here)")
        expect(dpGrain != nil && !dpGrainText.contains("1234567"),
            "discord payload never carries the raw token count")
        expect(dpGrain != nil && !dpGrainText.contains("7.89"),
            "discord payload never carries cent-precision cost (mutation: Format.usd fails here)")
        expect(dpGrain?.state.hasSuffix("$5-10") == true, "cost publishes as a coarse band")
        // A11 (cont.) — the two inputs where the SHARED tray formatter would
        // publish an exact figure. `Format.compactTokens` returns `String(count)`
        // below 1000 and for every negative value, which is right for the menu
        // bar and wrong on a public profile.
        let dpSmall = DiscordPresence.payload(
            graph: dpGraph(dpPayload(dpDay(dpToday, 850, 0.4, dpStripe("claude", 850, 0.4)))),
            hidden: [], today: dpToday)
        expect(dpSmall?.details == "<1K tokens today",
            "a light day publishes a band, not the exact count (mutation: calling "
                + "Format.compactTokens directly publishes \"850\")")
        // Negative day totals are reachable: the aggregator clamps per lane, so
        // the re-summed slow path can go negative (trayTotals' doc comment).
        // Hiding a non-existent id forces that slow path.
        let dpNegative = DiscordPresence.payload(
            graph: dpGraph(dpPayload(dpDay(
                dpToday, 0, 1.0,
                dpStripe("claude", -1_234_567, 0.0) + "," + dpStripe("codex", 1_000, 1.0)))),
            hidden: ["nobody"], today: dpToday)
        expect(dpNegative.map { !$0.fields.values.joined().contains("1233567") } ?? true,
            "a negative day total never publishes its signed digits")

        // A12 — zero usage publishes nothing (no "machine is on" beacon), and a
        // day absent from the graph publishes nothing either.
        let dpZeroGraph = dpGraph(dpPayload(dpDay(dpToday, 0, 0, dpStripe("claude", 0, 0))))
        expect(DiscordPresence.payload(graph: dpZeroGraph, hidden: [], today: dpToday) == nil,
            "zero usage publishes nothing (mutation: dropping the guard publishes an idle beacon)")
        expect(DiscordPresence.payload(graph: dpGrainGraph, hidden: [], today: "2099-01-01") == nil,
            "a day with no contribution publishes nothing")
        // The isFinite guard, made reachable. JSON cannot express NaN, but the
        // slow path sums Double costs, so two finite stripes overflow to +inf —
        // which costBucket would happily publish as "$100+".
        let dpInfinite = DiscordPresence.payload(
            graph: dpGraph(dpPayload(dpDay(
                dpToday, 10, 1e308,
                dpStripe("claude", 10, 1e308) + "," + dpStripe("codex", 10, 1e308)))),
            hidden: ["nobody"], today: dpToday)
        expect(dpInfinite == nil,
            "an overflowed cost publishes nothing (mutation: dropping the isFinite guard "
                + "publishes $100+)")

        // A14 — the top client is the busiest CLIENT, not the biggest single
        // stripe. `Contribution.clients` holds per client×model×provider
        // stripes, so claude's 30+30 must beat codex's single 50.
        let dpFoldGraph = dpGraph(dpPayload(dpDay(
            dpToday, 110, 1.1,
            dpStripe("claude", 30, 0.3, model: "m1") + ","
                + dpStripe("claude", 30, 0.3, model: "m2") + ","
                + dpStripe("codex", 50, 0.5, model: "m1"))))
        let dpFold = DiscordPresence.payload(graph: dpFoldGraph, hidden: [], today: dpToday)
        expect(dpFold?.state.hasPrefix("Claude Code") == true,
            "top client folds stripes per client first (mutation: max over raw stripes picks "
                + "Codex CLI's single 50 over Claude Code's 30+30)")
        let dpFoldHidden = DiscordPresence.payload(
            graph: dpFoldGraph, hidden: ["claude"], today: dpToday)
        expect(dpFoldHidden?.state.hasPrefix("Codex CLI") == true,
            "hiding the busiest client promotes the next visible one")

        // A15 — deterministic tie-break: tokens, then higher cost, then the
        // lexicographically smallest id. A wobbling label would flip between
        // publishes.
        let dpTieCost = dpGraph(dpPayload(dpDay(
            dpToday, 200, 3.0, dpStripe("amp", 100, 1.0) + "," + dpStripe("zed", 100, 2.0))))
        expect(DiscordPresence.payload(graph: dpTieCost, hidden: [], today: dpToday)?.state
            == "Zed · $1-5",
            "equal tokens break to the higher cost")
        // Six-way tie on purpose, and the fixture lists the ids in REVERSE
        // order so input order cannot supply the answer.
        //
        // Two mutations threaten this, and they are not equally provable:
        //   `>=` in the fold comparison        → picks Zed, fails every run.
        //   `keys.sorted()` → `keys`           → PROBABILISTIC. Swift seeds
        //     Dictionary hashing per process, so an unsorted walk lands on the
        //     right id by luck roughly 1 run in 6. The pair of assertions below
        //     feed the SAME six clients in opposite orders — a different
        //     insertion sequence is a different iteration order — so the
        //     mutation has to get lucky twice to survive. It is still not a
        //     hard proof, and no in-process check can make it one: repeatedly
        //     calling the fold inside one run cannot see the wobble, because
        //     the seed does not change until the process does.
        let dpTieIds = ["zed", "warp", "goose", "droid", "codex", "amp"]
        let dpTieId = dpGraph(dpPayload(dpDay(
            dpToday, 600, 6.0, dpTieIds.map { dpStripe($0, 100, 1.0) }.joined(separator: ","))))
        expect(
            DiscordPresence.payload(graph: dpTieId, hidden: [], today: dpToday)?.state
                == "Amp · $5-10",
            "a full tie breaks to the lexicographically smallest id")
        let dpTieIdReversed = dpGraph(dpPayload(dpDay(
            dpToday, 600, 6.0,
            dpTieIds.reversed().map { dpStripe($0, 100, 1.0) }.joined(separator: ","))))
        expect(
            DiscordPresence.payload(graph: dpTieIdReversed, hidden: [], today: dpToday)?.state
                == "Amp · $5-10",
            "the tie-break does not depend on the order the stripes arrive in")

        // MARK: - Discord Rich Presence transport (DISCORD-PRESENCE M2a)
        //
        // Nothing in the app calls this transport yet. These are the framing,
        // lifecycle and privacy guards for the code that will carry the payload
        // off the machine, and they run against real syscalls: the client's
        // connect factory is handed one end of a `socketpair(AF_UNIX,
        // SOCK_STREAM)`, so the framing, the fd lifetime and the SIGPIPE
        // behaviour are the production ones, not a mock's.

        /// Frames built independently of `DiscordIPC.encode`, so the decoder is
        /// never checked against its own mirror image.
        func dpRaw(_ op: UInt32, _ length: UInt32, _ body: Data) -> Data {
            var out = Data()
            withUnsafeBytes(of: op.littleEndian) { out.append(contentsOf: $0) }
            withUnsafeBytes(of: length.littleEndian) { out.append(contentsOf: $0) }
            out.append(body)
            return out
        }
        func dpFrameBytes(_ op: UInt32, _ text: String) -> Data {
            let body = Data(text.utf8)
            return dpRaw(op, UInt32(body.count), body)
        }
        func dpSocketPair() -> (Int32, Int32) {
            var fds: [Int32] = [-1, -1]
            _ = socketpair(AF_UNIX, SOCK_STREAM, 0, &fds)
            // The peer end is only ever read by the test; the timeout keeps a
            // missing frame a FAIL rather than a hung selftest, and
            // SO_NOSIGPIPE keeps a write after the client closes from killing
            // the harness itself.
            var timeout = timeval(tv_sec: 1, tv_usec: 0)
            setsockopt(fds[1], SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
            var on: Int32 = 1
            setsockopt(fds[1], SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size))
            return (fds[0], fds[1])
        }
        func dpRecv(_ fd: Int32) -> Data {
            var buf = [UInt8](repeating: 0, count: 4096)
            let count = recv(fd, &buf, buf.count, 0)
            return count > 0 ? Data(buf[0..<count]) : Data()
        }
        /// Non-blocking, unlike `dpRecv`. The peer ends carry `SO_RCVTIMEO` of
        /// one second, so a poll loop built on the blocking read spends up to a
        /// second per turn and can outlast the very delay it is trying to
        /// detect — which is exactly how a throttle assertion ends up unable to
        /// fail.
        func dpRecvNow(_ fd: Int32) -> Data {
            var buf = [UInt8](repeating: 0, count: 4096)
            let count = recv(fd, &buf, buf.count, MSG_DONTWAIT)
            return count > 0 ? Data(buf[0..<count]) : Data()
        }
        func dpDrainToEOF(_ fd: Int32) -> Data {
            var out = Data()
            while true {
                let chunk = dpRecv(fd)
                if chunk.isEmpty { return out }
                out.append(chunk)
            }
        }
        func dpWaitUntil(_ ready: () -> Bool) -> Bool {
            for _ in 0..<400 {
                if ready() { return true }
                usleep(5_000)
            }
            return ready()
        }
        /// Counts calls to an injected connect factory. Mutated on the client's
        /// serial queue and read after `drainForTesting()`/`dpWaitUntil`, which
        /// is what orders the two.
        final class DPCounter: @unchecked Sendable {
            var value = 0
        }

        // A6 — framing resilience. `encode` is pinned against an independently
        // built frame first, so a byte-order regression cannot hide behind a
        // symmetric decoder.
        expect(DiscordIPC.encode(.handshake, Data("{}".utf8)) == dpRaw(0, 2, Data("{}".utf8)),
            "the frame encoder emits LE opcode, LE length, then body")

        // A6a — an absurd length is refused before the completeness check, so
        // no allocation is ever sized from the wire. Dropping the bound check
        // does not blow up here; it silently turns this into `needMore`, which
        // is a connection that waits forever for 4 GiB that will never arrive.
        var dpOversize = dpRaw(1, 0xFFFF_FFFF, Data())
        expect(DiscordIPC.decode(from: &dpOversize) == .fatal && dpOversize.count == 8,
            "A6a: an oversized frame length is fatal and consumes nothing "
                + "(mutation: dropping the maxFrameLength check yields needMore)")
        // Literal bounds, not the constant they guard: 64 KiB is the contract.
        var dpAtCap = dpRaw(1, 65_536, Data())
        expect(DiscordIPC.decode(from: &dpAtCap) == .needMore,
            "a length exactly at the 64 KiB cap is allowed")
        var dpOverCap = dpRaw(1, 65_537, Data())
        expect(DiscordIPC.decode(from: &dpOverCap) == .fatal,
            "one byte over the 64 KiB cap is fatal")

        // A6b — opcode allowlist.
        var dpOpFive = dpFrameBytes(5, "{}")
        expect(DiscordIPC.decode(from: &dpOpFive) == .discard && dpOpFive.isEmpty,
            "A6b: opcode 5 is discarded and consumed "
                + "(mutation: dropping the Opcode allowlist surfaces it as a frame)")
        var dpOpMax = dpFrameBytes(0xFFFF_FFFF, "{}")
        expect(DiscordIPC.decode(from: &dpOpMax) == .discard && dpOpMax.isEmpty,
            "A6b: opcode 0xFFFFFFFF is discarded, not read as a negative int")

        // A6c — partial reads never block and never consume.
        var dpHeaderOnly = dpRaw(1, 2, Data())
        expect(DiscordIPC.decode(from: &dpHeaderOnly) == .needMore && dpHeaderOnly.count == 8,
            "A6c: a header with no body needs more and leaves the buffer intact "
                + "(mutation: consuming on an incomplete frame loses the header)")
        var dpHalfHeader = Data([1, 0, 0, 0])
        expect(DiscordIPC.decode(from: &dpHalfHeader) == .needMore && dpHalfHeader.count == 4,
            "A6c: fewer than 8 bytes needs more")

        // A6d — malformed bodies are dropped silently. One validity check
        // covers both cases: JSONSerialization rejects a body that is not valid
        // UTF-8 as well as one that is not valid JSON.
        var dpNonUTF8 = dpRaw(1, 3, Data([0xFF, 0xFE, 0xFD]))
        expect(DiscordIPC.decode(from: &dpNonUTF8) == .discard && dpNonUTF8.isEmpty,
            "A6d: a non-UTF-8 body is discarded "
                + "(mutation: dropping the body validity check surfaces it as a frame)")
        var dpBadJSON = dpFrameBytes(1, "{not json")
        expect(DiscordIPC.decode(from: &dpBadJSON) == .discard && dpBadJSON.isEmpty,
            "A6d: an unparseable JSON body is discarded")

        // A6e — several frames in one read are taken one at a time, in order.
        var dpStream = dpFrameBytes(1, "{\"a\":1}")
            + dpFrameBytes(5, "{}")
            + dpFrameBytes(0, "{\"b\":2}")
        expect(DiscordIPC.decode(from: &dpStream) == .frame(.frame, Data("{\"a\":1}".utf8)),
            "A6e: the first of three concatenated frames comes out intact")
        expect(DiscordIPC.decode(from: &dpStream) == .discard,
            "A6e: the bad middle frame is skipped without disturbing the rest")
        expect(DiscordIPC.decode(from: &dpStream) == .frame(.handshake, Data("{\"b\":2}".utf8)),
            "A6e: the third frame follows "
                + "(mutation: advancing the buffer by the wrong amount fails here)")
        expect(DiscordIPC.decode(from: &dpStream) == .needMore && dpStream.isEmpty,
            "A6e: the buffer is fully drained afterwards")

        // A-wire — the published surface and the serialized bytes are the same
        // set. `leafStrings` walks the real JSON, nesting included, and renders
        // numbers too, so a field smuggled in as a number is just as visible.
        let dpWirePayload = DiscordPresence.Payload(
            details: "12K tokens today", state: "Amp · $1-5", largeImageKey: "tokenbar")
        let dpWire = DiscordIPC.activityJSON(dpWirePayload, pid: 4242, nonce: "NONCE-1")
        // Sorted arrays, not Sets: a Set erases multiplicity, so a smuggled
        // field whose value merely REPEATS an existing leaf — assets
        // .small_image = the same "tokenbar", or a numeric field equal to the
        // pid — left the set unchanged and escaped entirely. Counting leaves
        // is what closes that.
        expect(
            DiscordIPC.leafStrings(dpWire).sorted()
                == (Array(dpWirePayload.fields.values) + ["SET_ACTIVITY", "NONCE-1", "4242"]).sorted(),
            "A-wire: the activity's leaves are exactly Payload.fields plus Discord's structural "
                + "constants, counted (mutation: adding any field — hostname, startTimestamp, or "
                + "one whose value duplicates an existing leaf — fails here)")
        let dpWireObject = (try? JSONSerialization.jsonObject(with: dpWire)) as? [String: Any]
        let dpWireArgs = dpWireObject?["args"] as? [String: Any]
        let dpWireActivity = dpWireArgs?["activity"] as? [String: Any]
        let dpWireAssets = dpWireActivity?["assets"] as? [String: Any]
        expect(dpWireAssets?["large_image"] as? String == "tokenbar",
            "A-wire: largeImageKey is published as assets.large_image, renamed but unaltered")
        expect(dpWireActivity?["details"] as? String == "12K tokens today"
            && dpWireActivity?["state"] as? String == "Amp · $1-5",
            "A-wire: details and state go out verbatim")
        expect(dpWireActivity?.count == 3,
            "A-wire: the activity object holds exactly details, state and assets")
        // The nested level needs its own count: `activity.count` only sees the
        // top level, so a field added under `assets` kept it at 3.
        // Keys are a channel of their own. A leaf scan cannot see them, and an
        // empty container has no leaves at all, so `"x-<hostname>": [:]` was
        // published with all 517 assertions green until this landed. Sorted
        // array, not a Set, so a key repeated at another level shows too.
        expect(
            DiscordIPC.leafKeys(dpWire).sorted() == [
                "activity", "args", "assets", "cmd", "details",
                "large_image", "nonce", "pid", "state",
            ],
            "A-wire: the frame's keys are exactly the protocol's (mutation: adding a key whose "
                + "VALUE is an empty object smuggles the key text out with no new leaf — this is "
                + "the only assertion that sees it)")
        expect(dpWireAssets?.count == 1,
            "A-wire: assets holds exactly large_image (mutation: adding assets.small_image, "
                + "even with a value that duplicates an existing leaf, fails here)")
        expect(dpWireObject?["cmd"] as? String == "SET_ACTIVITY"
            && dpWireArgs?["pid"] as? Int == 4242 && dpWireObject?["nonce"] as? String == "NONCE-1",
            "A-wire: the envelope is SET_ACTIVITY with the given pid and nonce")
        expect(String(decoding: DiscordIPC.activityJSON(nil, pid: 4242, nonce: "N"), as: UTF8.self)
            .contains("\"activity\":null"),
            "clearing the presence sends activity: null")

        // A-wire, the OTHER outbound frames. The activity frame is not the
        // only thing that leaves this machine: the handshake goes out on every
        // connect, and the clear goes out on every stop. Both are serialized
        // by the same helper and neither had a single assertion — the same key
        // and leaf channels were wide open there while the activity frame was
        // being tightened four times over. Pinning what leaves means pinning
        // every frame, not the one that happened to be under review.
        let dpShake = DiscordIPC.handshakeJSON()
        expect(DiscordIPC.leafKeys(dpShake).sorted() == ["client_id", "v"],
            "A-wire: the handshake's keys are exactly v and client_id")
        expect(DiscordIPC.leafStrings(dpShake).sorted()
            == [DiscordIPC.applicationID, "1"].sorted(),
            "A-wire: the handshake carries the application id and the protocol version, and "
                + "nothing else (mutation: adding any field, or a key with an empty value, fails "
                + "one of these two)")
        let dpClear = DiscordIPC.activityJSON(nil, pid: 4242, nonce: "NONCE-2")
        expect(DiscordIPC.leafKeys(dpClear).sorted()
            == ["activity", "args", "cmd", "nonce", "pid"],
            "A-wire: the clear frame's keys are exactly the protocol's")
        expect(DiscordIPC.leafStrings(dpClear).sorted()
            == ["4242", "NONCE-2", "SET_ACTIVITY", "null"].sorted(),
            "A-wire: the clear frame carries no payload data at all")

        // A13 — nothing from an inbound frame may be read, kept or echoed. The
        // READY frame Discord actually sends carries the account's username, id
        // and avatar.
        let dpReadyBody = "{\"cmd\":\"DISPATCH\",\"evt\":\"READY\",\"data\":{\"v\":1,"
            + "\"user\":{\"username\":\"SECRET_USERNAME\",\"id\":\"SECRET_ID\","
            + "\"avatar\":\"SECRET_AVATAR\",\"discriminator\":\"0001\"}}}"
        let dpInboundToken = DiscordIPC.inbound(Data(dpReadyBody.utf8))
        // Literal "ready", not DiscordIPC.readyEvent: an expectation read out of
        // the constant it guards passes whatever that constant becomes.
        expect(dpInboundToken == "ready" && !dpInboundToken.contains("SECRET_"),
            "A13: a READY frame yields a fixed token and no frame content "
                + "(mutation: returning the parsed user object, or the raw body, leaks SECRET_)")
        expect(DiscordIPC.inbound(Data("{\"evt\":\"ERROR\",\"data\":{}}".utf8)) == "other",
            "A13: a non-READY event is not mistaken for READY")

        // A-path — sun_path is 104 bytes and truncation does not fail, it
        // connects somewhere else.
        func dpAddressFits(_ path: String) -> Bool {
            DiscordIPC.unixSocketAddress(path: path).map { _ in true } ?? false
        }
        expect(!dpAddressFits(String(repeating: "a", count: 200)),
            "A-path: an over-long socket path is refused "
                + "(mutation: truncating to fit connects to a different path)")
        expect(!dpAddressFits(String(repeating: "a", count: 104)),
            "A-path: a path filling sun_path exactly leaves no room for the NUL")
        expect(dpAddressFits(String(repeating: "a", count: 103)),
            "A-path: 103 bytes still fits")
        var dpAddress = DiscordIPC.unixSocketAddress(path: "/tmp/discord-ipc-0")!
        let dpAddressPath = withUnsafeBytes(of: &dpAddress.sun_path) {
            String(cString: $0.bindMemory(to: CChar.self).baseAddress!)
        }
        expect(dpAddressPath == "/tmp/discord-ipc-0",
            "A-path: the address carries the path verbatim")

        // A7a/A7c/A13 over a real socket pair.
        let (dpLocal, dpPeer) = dpSocketPair()
        let dpConnects = DPCounter()
        let dpClient = DiscordIPCClient(connect: {
            dpConnects.value += 1
            return dpLocal
        })
        dpClient.start()
        dpClient.start()
        dpClient.drainForTesting()
        expect(dpConnects.value == 1,
            "A7a: start() twice opens one connection "
                + "(mutation: dropping the `!running` guard in start() connects twice)")

        var dpHandshakeBuffer = dpRecv(dpPeer)
        var dpHandshakeOK = false
        if case .frame(.handshake, let body) = DiscordIPC.decode(from: &dpHandshakeBuffer) {
            let text = String(decoding: body, as: UTF8.self)
            dpHandshakeOK = text.contains("\"client_id\":\"1534085299163107348\"")
                && text.contains("\"v\":1")
        }
        expect(dpHandshakeOK, "the client opens with a v1 handshake carrying the application id")

        dpFrameBytes(1, dpReadyBody).withUnsafeBytes { raw in
            _ = send(dpPeer, raw.baseAddress!, raw.count, 0)
        }
        expect(dpWaitUntil { dpClient.inboundTokenForTesting == "ready" },
            "the client recognises READY off the wire")
        expect(!dpClient.inboundTokenForTesting.contains("SECRET_"),
            "A13: nothing from the READY frame survives in the client's state")

        dpClient.publish(dpWirePayload)
        dpClient.publish(DiscordPresence.Payload(
            details: "999K tokens today", state: "Zed · $50-100", largeImageKey: "tokenbar"))
        dpClient.drainForTesting()
        var dpActivityBuffer = dpRecv(dpPeer)
        var dpActivityText = ""
        var dpActivityBody = Data()
        if case .frame(.frame, let body) = DiscordIPC.decode(from: &dpActivityBuffer) {
            dpActivityText = String(decoding: body, as: UTF8.self)
            dpActivityBody = body
        }
        expect(dpActivityText.contains("12K tokens today"),
            "the first publish reaches the socket immediately")

        // The bytes that actually left the socket, pinned the same way the
        // pure-function frames are. Every A-wire assertion above calls
        // `activityJSON` with `pid: 4242, nonce: "NONCE-1"`, so `pid()` and
        // `nonce()` — which supply every real frame — were executed by no
        // assertion at all. A `nonce()` returning
        // `NSUserName() + "@" + hostName` shipped the username and this
        // machine's reverse-DNS name on every publish with all 525 green.
        // Guarding a serializer is not guarding what it is called with.
        expect(
            DiscordIPC.leafKeys(dpActivityBody).sorted() == [
                "activity", "args", "assets", "cmd", "details",
                "large_image", "nonce", "pid", "state",
            ],
            "the frame on the wire carries exactly the protocol's keys")
        let dpLiveLeaves = DiscordIPC.leafStrings(dpActivityBody)
        let dpLivePayloadLeaves = ["12K tokens today", "Amp · $1-5", "tokenbar", "SET_ACTIVITY"]
        expect(
            dpLiveLeaves.count == dpLivePayloadLeaves.count + 2
                && dpLivePayloadLeaves.allSatisfy(dpLiveLeaves.contains),
            "the frame on the wire carries the payload, the envelope constant, and exactly two "
                + "more leaves — the pid and the nonce")
        let dpLiveExtra = dpLiveLeaves.filter { !dpLivePayloadLeaves.contains($0) }
        expect(dpLiveExtra.contains(String(ProcessInfo.processInfo.processIdentifier)),
            "the pid on the wire is this process's own (mutation: deriving it from a hostname "
                + "hash, or anything else, fails here)")
        let dpLiveNonce = dpLiveExtra.first {
            $0 != String(ProcessInfo.processInfo.processIdentifier)
        }
        expect(dpLiveNonce.map { UUID(uuidString: $0) != nil } == true,
            "the nonce on the wire is a bare UUID and carries nothing else (mutation: building it "
                + "from NSUserName() or the host name fails here)")
        expect(!dpActivityText.contains("999K") && dpActivityBuffer.isEmpty,
            "a second publish inside the 15s floor is coalesced rather than sent")

        dpClient.stop()
        dpClient.drainForTesting()
        var dpTail = dpDrainToEOF(dpPeer)
        var dpClearSeen = false
        while case .frame(_, let body) = DiscordIPC.decode(from: &dpTail) {
            if String(decoding: body, as: UTF8.self).contains("\"activity\":null") {
                dpClearSeen = true
            }
        }
        expect(dpClearSeen,
            "A7c: stop() sends the activity clear before closing the socket "
                + "(mutation: closing first loses the frame entirely)")
        expect(!dpClient.isConnectedForTesting, "A7c: stop() closes the socket")
        close(dpPeer)

        // A9 — SIGPIPE. The connection is established first (SO_NOSIGPIPE only
        // applies to a live socket — on a dead one setsockopt returns EINVAL),
        // then the peer is closed and a frame written in the same queue item so
        // the EOF handler cannot get there first. With SO_NOSIGPIPE removed
        // this does not fail, it kills the selftest process: no FAIL line, no
        // "selftest passed", exit status 141.
        let (dpDeadLocal, dpDeadPeer) = dpSocketPair()
        let dpSigClient = DiscordIPCClient(connect: { dpDeadLocal })
        dpSigClient.start()
        dpSigClient.drainForTesting()
        _ = dpRecv(dpDeadPeer)
        dpSigClient.probeWriteForTesting { close(dpDeadPeer) }
        expect(dpSigClient.writeErrnoForTesting == EPIPE,
            "A9: a write to a closed socket returns EPIPE and the process survives "
                + "(mutation: dropping SO_NOSIGPIPE terminates the selftest on signal 13)")
        expect(!dpSigClient.isConnectedForTesting,
            "A9: the failed write tears the connection down")
        dpSigClient.stop()

        // A7b, part 1 — retries are bounded. Every connection here is born
        // broken (peer closed immediately), so the retry path runs to its limit.
        let dpRetries = DPCounter()
        let dpRetryClient = DiscordIPCClient(connect: {
            dpRetries.value += 1
            var fds: [Int32] = [-1, -1]
            _ = socketpair(AF_UNIX, SOCK_STREAM, 0, &fds)
            close(fds[1])
            return fds[0]
        })
        dpRetryClient.reconnectDelay = 0.02
        dpRetryClient.start()
        expect(dpWaitUntil { dpRetries.value >= 2 },
            "a broken connection is retried at all (without this the bound below is vacuous)")
        expect(dpWaitUntil { dpRetries.value == 6 },
            "the retry budget is the initial attempt plus five")
        usleep(300_000)
        expect(dpRetries.value == 6,
            "A7b: reconnection stops at the attempt limit "
                + "(mutation: dropping the maxReconnectAttempts guard retries forever)")
        dpRetryClient.stop()

        // A7b, part 2 — stop() cancels the armed retry.
        let dpCancelCount = DPCounter()
        let dpCancelClient = DiscordIPCClient(connect: {
            dpCancelCount.value += 1
            var fds: [Int32] = [-1, -1]
            _ = socketpair(AF_UNIX, SOCK_STREAM, 0, &fds)
            close(fds[1])
            return fds[0]
        })
        dpCancelClient.reconnectDelay = 0.5
        dpCancelClient.start()
        expect(dpWaitUntil { dpCancelClient.reconnectPendingForTesting },
            "the disconnect arms a retry")
        // A7a, second half: idempotence has to hold while a retry is armed too,
        // where `fd < 0` no longer covers it.
        dpCancelClient.start()
        dpCancelClient.drainForTesting()
        expect(dpCancelCount.value == 1,
            "A7a: start() during an armed retry does not open a second connection "
                + "(mutation: dropping the `!running` guard in start() connects immediately)")
        dpCancelClient.stop()
        dpCancelClient.drainForTesting()
        expect(!dpCancelClient.reconnectPendingForTesting,
            "A7b: stop() cancels the armed retry "
                + "(mutation: dropping reconnectWork?.cancel() leaves it armed)")
        usleep(700_000)
        expect(dpCancelCount.value == 1, "A7b: the cancelled retry never fires")

        // A7b, part 3 — after stop(), no path rebuilds the connection. Driven
        // directly, because once stop() has closed the socket there is no
        // disconnect left to provoke.
        let (dpStopLocal, dpStopPeer) = dpSocketPair()
        let dpStopConnects = DPCounter()
        let dpStopClient = DiscordIPCClient(connect: {
            dpStopConnects.value += 1
            return dpStopLocal
        })
        dpStopClient.reconnectDelay = 0.02
        dpStopClient.start()
        dpStopClient.drainForTesting()
        expect(dpStopConnects.value == 1, "the stopped-client fixture connected once")
        dpStopClient.stop()
        dpStopClient.drainForTesting()
        dpStopClient.scheduleReconnectForTesting()
        usleep(300_000)
        expect(dpStopConnects.value == 1,
            "A7b: no path reconnects after stop() "
                + "(mutation: dropping the running guard in openConnection reconnects here)")
        close(dpStopPeer)

        // A2 — the two behaviours the previous review found unguarded.

        // Superseding a live connection must close the old socket, not leak
        // it. Checked from the OLD PEER, never from the fd number: descriptors
        // get reused, so "is fd N still open" can be true simply because N is
        // now the NEW socket. Only the peer reaching EOF proves every copy of
        // the old local end is gone.
        let dpSupA = dpSocketPair()
        let dpSupB = dpSocketPair()
        let dpSupIdx = DPCounter()
        let dpSupFDs: [Int32] = [dpSupA.0, dpSupB.0]
        let dpSupClient = DiscordIPCClient(connect: {
            let i = min(dpSupIdx.value, dpSupFDs.count - 1)
            dpSupIdx.value += 1
            return dpSupFDs[i]
        })
        dpSupClient.reconnectDelay = 0.02
        dpSupClient.start()
        _ = dpWaitUntil { dpSupClient.isConnectedForTesting }
        // Drain the handshake first, or the peer has bytes waiting and can
        // never report EOF.
        _ = dpRecv(dpSupA.1)
        dpSupClient.scheduleReconnectForTesting()
        _ = dpWaitUntil { dpSupIdx.value >= 2 }
        let dpSupClosed = dpWaitUntil {
            var byte: UInt8 = 0
            return recv(dpSupA.1, &byte, 1, MSG_DONTWAIT) == 0
        }
        expect(dpSupClosed,
            "reconnecting over a live connection closes the superseded socket (mutation: dropping "
                + "openConnection's `if fd >= 0 { teardown() }` leaks the descriptor and the old "
                + "peer never sees EOF)")
        dpSupClient.stop()
        close(dpSupA.1)
        close(dpSupB.1)

        // The retry budget resets once a connection reaches READY, so a long
        // session survives more than `maxReconnectAttempts` Discord restarts.
        // The peer answers READY and only then drops: a fixture that closes
        // immediately never reaches READY and would pass even with the reset
        // removed, which is exactly the trap that made this look untestable.
        let dpReadyFrame = dpFrameBytes(1, "{\"evt\":\"READY\"}")
        let dpReadyCount = DPCounter()
        let dpReadyClient = DiscordIPCClient(connect: {
            var fds: [Int32] = [-1, -1]
            _ = socketpair(AF_UNIX, SOCK_STREAM, 0, &fds)
            var on: Int32 = 1
            setsockopt(fds[1], SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size))
            dpReadyCount.value += 1
            _ = dpReadyFrame.withUnsafeBytes { send(fds[1], $0.baseAddress, $0.count, 0) }
            let peer = fds[1]
            DispatchQueue.global().asyncAfter(deadline: .now() + 0.03) { close(peer) }
            return fds[0]
        })
        dpReadyClient.reconnectDelay = 0.01
        dpReadyClient.start()
        // Initial connect plus the cap is 6; anything beyond it can only come
        // from a budget that was reset.
        let dpReadyBeyondCap = dpWaitUntil {
            dpReadyCount.value > DiscordIPCClient.maxReconnectAttempts + 1
        }
        dpReadyClient.stop()
        expect(dpReadyBeyondCap,
            "reaching READY resets the retry budget (mutation: dropping `attempts = 0` from the "
                + "READY branch caps reconnects at maxReconnectAttempts for the whole start() "
                + "lifetime, so the presence never returns after enough restarts)")

        // The mirror image, and the one that pins the reset's TIMING rather
        // than its existence: a peer that connects, says nothing, and drops.
        // That is Discord running but refusing us, and it must still exhaust
        // the budget. Resetting on the socket opening instead of on READY
        // would reconnect against it forever — the assertion above cannot see
        // that difference, because its connections do reach READY.
        let dpMuteCount = DPCounter()
        let dpMuteClient = DiscordIPCClient(connect: {
            var fds: [Int32] = [-1, -1]
            _ = socketpair(AF_UNIX, SOCK_STREAM, 0, &fds)
            var on: Int32 = 1
            setsockopt(fds[1], SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size))
            dpMuteCount.value += 1
            let peer = fds[1]
            DispatchQueue.global().asyncAfter(deadline: .now() + 0.01) { close(peer) }
            return fds[0]
        })
        dpMuteClient.reconnectDelay = 0.01
        dpMuteClient.start()
        let dpMuteCap = DiscordIPCClient.maxReconnectAttempts + 1
        _ = dpWaitUntil { dpMuteCount.value >= dpMuteCap }
        // Long enough for ~30 more cycles at this delay, short enough not to
        // pad the suite.
        for _ in 0..<60 where dpMuteCount.value <= dpMuteCap { usleep(5_000) }
        dpMuteClient.stop()
        expect(dpMuteCount.value <= dpMuteCap,
            "a peer that never reaches READY still exhausts the retry budget (mutation: resetting "
                + "`attempts` when the socket opens instead of when READY arrives reconnects "
                + "forever against a peer that accepts and immediately drops)")

        // Codex round 1 on the transport PR — four findings that were all the
        // same root: the lifecycle treated "connected, published, dropped" as
        // the end of the story instead of something that has to come back.

        // Reconnecting republishes the last activity. Discord restarting is
        // ordinary; before this the payload was marked sent, `hasPending` went
        // false, and the READY on the replacement connection flushed nothing —
        // the presence stayed missing until the producer happened to publish
        // again, up to the tray's 5-minute poll away.
        let dpRepubReady = dpFrameBytes(1, "{\"evt\":\"READY\"}")
        let dpRepubPairs = [dpSocketPair(), dpSocketPair()]
        let dpRepubIdx = DPCounter()
        let dpRepubClient = DiscordIPCClient(connect: {
            let i = min(dpRepubIdx.value, dpRepubPairs.count - 1)
            dpRepubIdx.value += 1
            return dpRepubPairs[i].0
        })
        dpRepubClient.reconnectDelay = 0.02
        // Deliberately high: a restore re-sends bytes Discord already has, so
        // it must not queue behind the sampling floor. With the throttle
        // applied to it, this frame would arrive 30s late and the wait below
        // would time out.
        dpRepubClient.publishInterval = 30
        dpRepubClient.start()
        _ = dpWaitUntil { dpRepubClient.isConnectedForTesting }
        _ = dpRepubReady.withUnsafeBytes { send(dpRepubPairs[0].1, $0.baseAddress, $0.count, 0) }
        _ = dpWaitUntil { dpRepubClient.inboundTokenForTesting == DiscordIPC.readyEvent }
        dpRepubClient.publish(dpWirePayload)
        dpRepubClient.drainForTesting()
        _ = dpDrainToEOF(dpRepubPairs[0].1)
        // Break the first connection; the client retries onto the second pair.
        close(dpRepubPairs[0].1)
        _ = dpWaitUntil { dpRepubIdx.value >= 2 }
        _ = dpRepubReady.withUnsafeBytes { send(dpRepubPairs[1].1, $0.baseAddress, $0.count, 0) }
        dpRepubClient.drainForTesting()
        // No second `publish()` anywhere: whatever arrives here was resent by
        // the client itself.
        var dpRepubSeen = false
        _ = dpWaitUntil {
            var buf = dpRecv(dpRepubPairs[1].1)
            while case .frame(let op, let body) = DiscordIPC.decode(from: &buf) {
                if op == .frame,
                   String(decoding: body, as: UTF8.self).contains("12K tokens today") {
                    dpRepubSeen = true
                }
            }
            return dpRepubSeen
        }
        expect(dpRepubSeen,
            "a replacement connection republishes the last activity without a new publish() "
                + "(mutation: dropping the READY branch's `hasPending = true` leaves the presence "
                + "missing until the next producer update)")
        dpRepubClient.stop()
        close(dpRepubPairs[1].1)

        // Exhausting the retry budget returns the client to a state a later
        // start() can act on. Leaving `running` true made start() hit the
        // idempotence guard and do nothing, so a client started once at launch
        // could never recover once Discord had been away long enough.
        let dpDeadCount = DPCounter()
        let dpDeadClient = DiscordIPCClient(connect: {
            dpDeadCount.value += 1
            throw DiscordIPC.Failure.unavailable
        })
        dpDeadClient.reconnectDelay = 0.01
        dpDeadClient.start()
        let dpDeadCap = DiscordIPCClient.maxReconnectAttempts + 1
        _ = dpWaitUntil { dpDeadCount.value >= dpDeadCap }
        for _ in 0..<40 where dpDeadCount.value <= dpDeadCap { usleep(5_000) }
        let dpDeadBefore = dpDeadCount.value
        dpDeadClient.start()
        let dpDeadRestarted = dpWaitUntil { dpDeadCount.value > dpDeadBefore }
        dpDeadClient.stop()
        expect(dpDeadBefore == dpDeadCap && dpDeadRestarted,
            "a client whose retry budget ran out can be started again (mutation: returning from "
                + "scheduleReconnect without giveUp() leaves `running` true and the second start() "
                + "silently does nothing)")

        // A peer that accepts and then says nothing must not hold the client
        // forever. SO_RCVTIMEO cannot see this — the read source never fires on
        // an idle socket, so no recv() runs and its timeout is never observed.
        let dpMuteReadyCount = DPCounter()
        let dpMuteReadyClient = DiscordIPCClient(connect: {
            var fds: [Int32] = [-1, -1]
            _ = socketpair(AF_UNIX, SOCK_STREAM, 0, &fds)
            var on: Int32 = 1
            setsockopt(fds[1], SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size))
            dpMuteReadyCount.value += 1
            // The peer end is deliberately never closed and never written to.
            return fds[0]
        })
        dpMuteReadyClient.reconnectDelay = 0.01
        dpMuteReadyClient.readyTimeout = 0.05
        dpMuteReadyClient.start()
        let dpMuteReadyRetried = dpWaitUntil { dpMuteReadyCount.value >= 2 }
        dpMuteReadyClient.stop()
        expect(dpMuteReadyRetried,
            "a silent endpoint trips the READY deadline and is retried (mutation: removing "
                + "armReadyDeadline leaves the client connected-but-never-ready forever, with "
                + "every publish parked behind `ready`)")

        // Codex round 2 — both findings are consequences of round 1's fixes.

        // The kill switch clears the queued payload even when the retry budget
        // already gave up. `giveUp()` keeps `pending` on purpose so a later
        // start() can restore, and `stop()` used to bail on `!running`, which
        // left that payload alive to be republished after the user turned the
        // feature off.
        let dpAbandonReady = dpFrameBytes(1, "{\"evt\":\"READY\"}")
        let dpAbandonPairs = [dpSocketPair(), dpSocketPair()]
        // The counter doubles as the fixture's mode: 0 hands out the first
        // socket, anything below `dpAbandonRevive` fails so the budget is
        // genuinely exhausted, and the test raises it later to let the second
        // start() connect. An earlier version simply indexed the pair array,
        // which handed out the second socket on the first retry and never
        // reached the given-up state the assertion is about.
        let dpAbandonIdx = DPCounter()
        let dpAbandonRevive = DPCounter()
        dpAbandonRevive.value = 1_000_000
        let dpAbandonClient = DiscordIPCClient(connect: {
            let i = dpAbandonIdx.value
            dpAbandonIdx.value += 1
            if i == 0 { return dpAbandonPairs[0].0 }
            guard i >= dpAbandonRevive.value else { throw DiscordIPC.Failure.unavailable }
            return dpAbandonPairs[1].0
        })
        dpAbandonClient.reconnectDelay = 0.01
        dpAbandonClient.publishInterval = 0
        dpAbandonClient.start()
        _ = dpWaitUntil { dpAbandonClient.isConnectedForTesting }
        _ = dpAbandonReady.withUnsafeBytes { send(dpAbandonPairs[0].1, $0.baseAddress, $0.count, 0) }
        _ = dpWaitUntil { dpAbandonClient.inboundTokenForTesting == DiscordIPC.readyEvent }
        dpAbandonClient.publish(dpWirePayload)
        dpAbandonClient.drainForTesting()
        _ = dpDrainToEOF(dpAbandonPairs[0].1)
        // Drop the connection and let every remaining attempt fail, so the
        // client reaches the given-up state with `pending` still set.
        close(dpAbandonPairs[0].1)
        _ = dpWaitUntil { dpAbandonIdx.value > DiscordIPCClient.maxReconnectAttempts + 1 }
        // The user turns the feature off while it is in that state.
        dpAbandonClient.stop()
        dpAbandonClient.drainForTesting()
        // Now start again on the second socket and see whether the abandoned
        // payload comes back.
        dpAbandonRevive.value = dpAbandonIdx.value
        dpAbandonClient.start()
        _ = dpWaitUntil { dpAbandonClient.isConnectedForTesting }
        _ = dpAbandonReady.withUnsafeBytes { send(dpAbandonPairs[1].1, $0.baseAddress, $0.count, 0) }
        dpAbandonClient.drainForTesting()
        var dpAbandonRepublished = false
        for _ in 0..<60 where !dpAbandonRepublished {
            var buf = dpRecvNow(dpAbandonPairs[1].1)
            while case .frame(let op, let body) = DiscordIPC.decode(from: &buf) {
                if op == .frame,
                   String(decoding: body, as: UTF8.self).contains("12K tokens today") {
                    dpAbandonRepublished = true
                }
            }
            usleep(5_000)
        }
        dpAbandonClient.stop()
        close(dpAbandonPairs[1].1)
        expect(!dpAbandonRepublished,
            "stop() clears the payload the retry give-up kept, so a later start() does not "
                + "resurrect it (mutation: restoring stop()'s `guard running else { return }` "
                + "republishes an activity the user already switched off)")

        // A clear is not a new sample, so it must not queue behind the
        // sampling floor: delaying one keeps a stale presence public for up to
        // the whole interval after the user hid the clients that produced it.
        let dpClearReady = dpFrameBytes(1, "{\"evt\":\"READY\"}")
        let dpClearPair = dpSocketPair()
        let dpClearClient = DiscordIPCClient(connect: { dpClearPair.0 })
        dpClearClient.publishInterval = 30
        dpClearClient.start()
        _ = dpWaitUntil { dpClearClient.isConnectedForTesting }
        _ = dpClearReady.withUnsafeBytes { send(dpClearPair.1, $0.baseAddress, $0.count, 0) }
        _ = dpWaitUntil { dpClearClient.inboundTokenForTesting == DiscordIPC.readyEvent }
        dpClearClient.publish(dpWirePayload)
        dpClearClient.drainForTesting()
        _ = dpDrainToEOF(dpClearPair.1)
        dpClearClient.publish(nil)
        dpClearClient.drainForTesting()
        var dpClearSent = false
        for _ in 0..<60 where !dpClearSent {
            var buf = dpRecvNow(dpClearPair.1)
            while case .frame(let op, let body) = DiscordIPC.decode(from: &buf) {
                if op == .frame,
                   String(decoding: body, as: UTF8.self).contains("\"activity\":null") {
                    dpClearSent = true
                }
            }
            usleep(5_000)
        }
        dpClearClient.stop()
        close(dpClearPair.1)
        expect(dpClearSent,
            "a clear goes out immediately rather than waiting for the publish floor (mutation: "
                + "throttling it unconditionally leaves a stale presence public for the whole "
                + "interval after the user hid everything)")

        // Codex round 3 — user intent vs connection state, descriptor
        // inheritance, and the duplicate that round 2's throttle bypass let in.

        // A producer update while the retry budget is spent is still the
        // latest intent, and the client is what a later start() must restore.
        // `giveUp()` used to clear `running`, so `publish()` dropped it and the
        // reconnect resurrected the pre-give-up payload instead.
        let dpIntentReady = dpFrameBytes(1, "{\"evt\":\"READY\"}")
        let dpIntentPairs = [dpSocketPair(), dpSocketPair()]
        let dpIntentIdx = DPCounter()
        let dpIntentRevive = DPCounter()
        dpIntentRevive.value = 1_000_000
        let dpIntentClient = DiscordIPCClient(connect: {
            let i = dpIntentIdx.value
            dpIntentIdx.value += 1
            if i == 0 { return dpIntentPairs[0].0 }
            guard i >= dpIntentRevive.value else { throw DiscordIPC.Failure.unavailable }
            return dpIntentPairs[1].0
        })
        dpIntentClient.reconnectDelay = 0.01
        dpIntentClient.publishInterval = 0
        dpIntentClient.start()
        _ = dpWaitUntil { dpIntentClient.isConnectedForTesting }
        _ = dpIntentReady.withUnsafeBytes { send(dpIntentPairs[0].1, $0.baseAddress, $0.count, 0) }
        _ = dpWaitUntil { dpIntentClient.inboundTokenForTesting == DiscordIPC.readyEvent }
        dpIntentClient.publish(dpWirePayload)
        dpIntentClient.drainForTesting()
        _ = dpDrainToEOF(dpIntentPairs[0].1)
        close(dpIntentPairs[0].1)
        _ = dpWaitUntil { dpIntentIdx.value > DiscordIPCClient.maxReconnectAttempts + 1 }
        // The producer moves on while the client is abandoned.
        let dpIntentNewer = DiscordPresence.Payload(
            details: "77K tokens today", state: "Zed · $1-5", largeImageKey: "tokenbar")
        dpIntentClient.publish(dpIntentNewer)
        dpIntentRevive.value = dpIntentIdx.value
        dpIntentClient.start()
        _ = dpWaitUntil { dpIntentClient.isConnectedForTesting }
        _ = dpIntentReady.withUnsafeBytes { send(dpIntentPairs[1].1, $0.baseAddress, $0.count, 0) }
        dpIntentClient.drainForTesting()
        var dpIntentGotNewer = false
        var dpIntentGotStale = false
        for _ in 0..<60 where !dpIntentGotNewer {
            var buf = dpRecvNow(dpIntentPairs[1].1)
            while case .frame(let op, let body) = DiscordIPC.decode(from: &buf) {
                let text = String(decoding: body, as: UTF8.self)
                if op == .frame, text.contains("77K tokens today") { dpIntentGotNewer = true }
                if op == .frame, text.contains("12K tokens today") { dpIntentGotStale = true }
            }
            usleep(5_000)
        }
        dpIntentClient.stop()
        close(dpIntentPairs[1].1)
        expect(dpIntentGotNewer && !dpIntentGotStale,
            "a publish while the retries are spent is the intent a later start() restores "
                + "(mutation: having giveUp() clear `running` makes publish() drop it and the "
                + "reconnect republishes the pre-give-up payload)")

        // The socket must not survive into a child process. The Rust core
        // spawns helpers, and an inherited descriptor keeps Discord seeing a
        // connection this app has already torn down.
        let dpCloexecPair = dpSocketPair()
        let dpCloexecClient = DiscordIPCClient(connect: { dpCloexecPair.0 })
        dpCloexecClient.start()
        _ = dpWaitUntil { dpCloexecClient.isConnectedForTesting }
        let dpCloexecFlags = fcntl(dpCloexecPair.0, F_GETFD)
        dpCloexecClient.stop()
        close(dpCloexecPair.1)
        expect(dpCloexecFlags >= 0 && (dpCloexecFlags & FD_CLOEXEC) != 0,
            "the adopted socket is close-on-exec (mutation: dropping the fcntl leaks the "
                + "connection into every helper the Rust core spawns)")

        // Publishing the same payload twice on one connection sends it once.
        // Round 2's throttle bypass is for restores; without a per-connection
        // record it also let a duplicate through immediately, which resets the
        // floor's clock and delays the next real payload behind a no-op.
        let dpDupReady = dpFrameBytes(1, "{\"evt\":\"READY\"}")
        let dpDupPair = dpSocketPair()
        let dpDupClient = DiscordIPCClient(connect: { dpDupPair.0 })
        dpDupClient.publishInterval = 0
        dpDupClient.start()
        _ = dpWaitUntil { dpDupClient.isConnectedForTesting }
        _ = dpDupReady.withUnsafeBytes { send(dpDupPair.1, $0.baseAddress, $0.count, 0) }
        _ = dpWaitUntil { dpDupClient.inboundTokenForTesting == DiscordIPC.readyEvent }
        dpDupClient.publish(dpWirePayload)
        dpDupClient.drainForTesting()
        _ = dpDrainToEOF(dpDupPair.1)
        dpDupClient.publish(dpWirePayload)
        dpDupClient.drainForTesting()
        var dpDupResent = false
        for _ in 0..<40 where !dpDupResent {
            var buf = dpRecvNow(dpDupPair.1)
            while case .frame(let op, let body) = DiscordIPC.decode(from: &buf) {
                if op == .frame,
                   String(decoding: body, as: UTF8.self).contains("12K tokens today") {
                    dpDupResent = true
                }
            }
            usleep(5_000)
        }
        dpDupClient.stop()
        close(dpDupPair.1)
        expect(!dpDupResent,
            "an identical payload on the same connection is not sent twice (mutation: dropping "
                + "the deliveredOnThisConnection check resends it immediately, past the floor)")

        // Codex round 4 — the connect path itself, which every earlier round
        // had treated as a detail that either works or throws.

        // Close-on-exec from birth. The previous round set the flag after
        // `connectFD()` returned, which left `socket()` and `connect()` inside
        // the window an exec can inherit through — and `connect()` is exactly
        // the call the next assertion shows can take a while.
        let dpBornFD = DiscordIPC.makeSocket()
        let dpBornFlags = fcntl(dpBornFD, F_GETFD)
        close(dpBornFD)
        expect(dpBornFD >= 0 && (dpBornFlags & FD_CLOEXEC) != 0,
            "the socket is close-on-exec before connect() runs (mutation: moving the fcntl back "
                + "out of makeSocket leaves the whole connect window inheritable)")

        // Codex round 5 — the bypass skipped the floor but still moved its
        // clock, so a restore delayed the next real payload by a full
        // interval measured from the restore instead of from the last sample.
        let dpFloorReady = dpFrameBytes(1, "{\"evt\":\"READY\"}")
        let dpFloorPairs = [dpSocketPair(), dpSocketPair()]
        let dpFloorIdx = DPCounter()
        let dpFloorClient = DiscordIPCClient(connect: {
            let i = min(dpFloorIdx.value, dpFloorPairs.count - 1)
            dpFloorIdx.value += 1
            return dpFloorPairs[i].0
        })
        dpFloorClient.reconnectDelay = 0.02
        dpFloorClient.publishInterval = 1.0
        dpFloorClient.start()
        _ = dpWaitUntil { dpFloorClient.isConnectedForTesting }
        _ = dpFloorReady.withUnsafeBytes { send(dpFloorPairs[0].1, $0.baseAddress, $0.count, 0) }
        _ = dpWaitUntil { dpFloorClient.inboundTokenForTesting == DiscordIPC.readyEvent }
        dpFloorClient.publish(dpWirePayload)
        dpFloorClient.drainForTesting()
        _ = dpDrainToEOF(dpFloorPairs[0].1)
        // Let the interval elapse against the real sample, so the payload that
        // follows the restore is due immediately if the clock was left alone.
        usleep(1_200_000)
        close(dpFloorPairs[0].1)
        _ = dpWaitUntil { dpFloorIdx.value >= 2 }
        _ = dpFloorReady.withUnsafeBytes { send(dpFloorPairs[1].1, $0.baseAddress, $0.count, 0) }
        // Wait for the restore to actually reach the socket before publishing
        // the next payload. `drainForTesting` only syncs the queue, and the
        // READY read event may not be on it yet — publishing early lets the
        // new payload flush before the restore ever runs, at which point both
        // the fixed and the broken build send it immediately and the
        // assertion measures nothing.
        var dpFloorRestored = false
        for _ in 0..<200 where !dpFloorRestored {
            var buf = dpRecvNow(dpFloorPairs[1].1)
            while case .frame(let op, let body) = DiscordIPC.decode(from: &buf) {
                if op == .frame,
                   String(decoding: body, as: UTF8.self).contains("12K tokens today") {
                    dpFloorRestored = true
                }
            }
            usleep(5_000)
        }
        let dpFloorNewer = DiscordPresence.Payload(
            details: "55K tokens today", state: "Amp · $5-10", largeImageKey: "tokenbar")
        dpFloorClient.publish(dpFloorNewer)
        dpFloorClient.drainForTesting()
        // 400ms: comfortably under the 1s the buggy path would defer by, and
        // comfortably over the time a due payload needs to reach the socket.
        var dpFloorPrompt = false
        for _ in 0..<80 where !dpFloorPrompt {
            var buf = dpRecvNow(dpFloorPairs[1].1)
            while case .frame(let op, let body) = DiscordIPC.decode(from: &buf) {
                if op == .frame,
                   String(decoding: body, as: UTF8.self).contains("55K tokens today") {
                    dpFloorPrompt = true
                }
            }
            usleep(5_000)
        }
        dpFloorClient.stop()
        close(dpFloorPairs[1].1)
        expect(dpFloorRestored && dpFloorPrompt,
            "a restore does not consume the publish interval (mutation: advancing lastSent on a "
                + "write that carries no new information throttles the next real payload from the "
                + "restore instead of from the last sample)")

        if failures > 0 {
            print("\(failures) selftest check(s) failed")
            exit(1)
        }
        print("selftest passed")
        exit(0)
    }
}
