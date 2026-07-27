import Foundation

// Usage pace — port of the Tauri app's src/lib/usagePace.ts (itself ported
// from codexbar's UsagePace).
//
// Linear mode derives expected usage and run-out time from elapsed duration.
// Historical mode consumes one coherent Rust projection instead. Both compare
// expected with actual usage to classify the gap: positive delta = ahead of
// pace ("in deficit", burning fast); negative = behind ("in reserve").

/// How the pace marker is derived (`PaceMode` in settings.ts).
public enum PaceMode: String, CaseIterable, Sendable {
    case historical, linear, off
}

public enum PaceStage: Sendable, Equatable {
    case onTrack
    case slightlyAhead, ahead, farAhead
    case slightlyBehind, behind, farBehind

    public var isDeficit: Bool {
        switch self {
        case .slightlyAhead, .ahead, .farAhead: return true
        default: return false
        }
    }
}

/// The source of the expected-usage projection.
public enum UsagePaceBasis: Sendable, Equatable {
    case linear
    case historical
}

public struct UsagePace: Sendable {
    public let stage: PaceStage
    public let basis: UsagePaceBasis
    /// actual − expected, in percentage points (>0 = ahead/deficit).
    public let deltaPercent: Double
    public let expectedUsedPercent: Double
    public let actualUsedPercent: Double
    /// Seconds until the window empties, if before reset. Historical mode uses
    /// the backend evaluator's value; linear mode derives it locally.
    public let etaSeconds: Double?
    /// True if the current rate lasts past the reset (won't run out).
    public let willLastToReset: Bool

    /// Yellow historical deficit is valid only for a backend historical result.
    public var isHistoricalDeficit: Bool {
        basis == .historical && stage.isDeficit
    }

    /// Short left-hand label: "On pace" / "12% in deficit" / "8% in reserve".
    public var label: String {
        if stage == .onTrack { return "On pace".localized }
        let d = Int(abs(deltaPercent).rounded())
        return stage.isDeficit
            ? "%lld%% in deficit".localized(d)
            : "%lld%% in reserve".localized(d)
    }

    /// Right-hand projection: "Lasts until reset" / "Projected empty in 2h 10m".
    public var etaText: String? {
        if willLastToReset { return "Lasts until reset".localized }
        guard let etaSeconds else { return nil }
        let t = Self.durationText(etaSeconds)
        // `durationText` is already localized, so compare against the same
        // localized token rather than the literal "now".
        return t == Self.nowText
            ? "Projected empty now".localized
            : "Projected empty in %@".localized(t)
    }

    /// Semantic key: bare "now" is too generic to safely use as a lookup key.
    static var nowText: String { "duration.now".localized(default: "now") }

    public static func durationText(_ seconds: Double) -> String {
        let m = Int((seconds / 60).rounded())
        if m < 1 { return nowText }
        if m < 60 { return "%lldm".localized(m) }
        let h = m / 60
        let rem = m % 60
        if h < 24 {
            return rem > 0 ? "%lldh %lldm".localized(h, rem) : "%lldh".localized(h)
        }
        let days = h / 24
        let hr = h % 24
        return hr > 0 ? "%lldd %lldh".localized(days, hr) : "%lldd".localized(days)
    }

    /// Localized countdown matching the Rust `resetText` rounding contract.
    /// The wire text is intentionally retained for compatibility, while the
    /// structured reset timestamp is the source for user-facing copy.
    public static func resetText(for resetsAt: String, now: Date = Date()) -> String? {
        guard let reset = parseRFC3339(resetsAt) else { return nil }
        let seconds = floor(reset.timeIntervalSince(now))
        guard seconds > 0 else { return "Resets now".localized }
        let minutes = Int((seconds + 59) / 60)
        return "Resets in %@".localized(durationText(Double(minutes * 60)))
    }
}

/// UI-free projection text assembled from one pace result and its optional
/// historical risk. A non-zero visible risk takes precedence over the generic
/// "Lasts until reset" phrase when the backend says the window will last; this
/// keeps the two historical signals from rendering as contradictory claims.
public struct UsagePacePresentation: Sendable, Equatable {
    public let etaText: String?
    public let riskText: String?
}

private func clamp(_ v: Double, _ lo: Double, _ hi: Double) -> Double {
    min(hi, max(lo, v))
}

private func stageFor(_ delta: Double) -> PaceStage {
    let a = abs(delta)
    if a <= 2 { return .onTrack }
    if a <= 6 { return delta >= 0 ? .slightlyAhead : .slightlyBehind }
    if a <= 12 { return delta >= 0 ? .ahead : .behind }
    return delta >= 0 ? .farAhead : .farBehind
}

/// RFC3339 parser tolerating fractional seconds (the backend emits both).
/// ISO8601DateFormatter is not Sendable, so build per call — pace runs a
/// handful of times per refresh, never hot.
func parseRFC3339(_ s: String) -> Date? {
    let fractional = ISO8601DateFormatter()
    fractional.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
    return fractional.date(from: s) ?? ISO8601DateFormatter().date(from: s)
}

extension UsagePace {
    /// Compute *linear* pace for a duration-ready v3 window.
    public static func compute(window: UsageWindow, now: Date = Date()) -> UsagePace? {
        guard isDurationReady(window.paceStatus.state) else { return nil }
        return computeCore(window: window, now: now)
    }

    /// Compute pace under the user's chosen mode:
    /// - `off`             → nil (no pace marker).
    /// - `historical`      → backend projection for `available`, exact-duration
    ///                       Linear only while `learningHistory`.
    /// - `linear`          → exact-duration Linear for duration-ready states.
    public static func compute(
        window: UsageWindow, mode: PaceMode, now: Date = Date()
    ) -> UsagePace? {
        switch mode {
        case .off:
            return nil
        case .historical:
            switch window.paceStatus.state {
            case .available:
                guard let historical = window.historicalPace else { return nil }
                return computeHistorical(window: window, historical: historical, now: now)
            case .learningHistory:
                return computeCore(window: window, now: now)
            case .learningDuration, .unavailable, .legacyMissing:
                return nil
            }
        case .linear:
            guard isDurationReady(window.paceStatus.state) else { return nil }
            return computeCore(window: window, now: now)
        }
    }

    private static func isDurationReady(_ state: UsagePaceState) -> Bool {
        state == .learningHistory || state == .available
    }

    /// Assemble display-only projection strings. Historical ETA and
    /// lasts-to-reset values are already carried by `pace`; this helper only
    /// decides whether a visible risk should suppress the generic lasts text.
    public static func presentation(
        window: UsageWindow, mode: PaceMode, pace: UsagePace
    ) -> UsagePacePresentation {
        let risk = mode == .historical
            ? runOutRiskLabel(window: window, pace: pace)
            : nil
        let eta = pace.willLastToReset && risk != nil ? nil : pace.etaText
        return UsagePacePresentation(etaText: eta, riskText: risk)
    }

    private static func computeHistorical(
        window: UsageWindow, historical: HistoricalPace, now: Date
    ) -> UsagePace? {
        // Keep the same window validity gates as linear pace. Historical data
        // supplies the projection values, but a quota card still needs a
        // current reset boundary before showing a pace marker.
        guard let timing = timing(for: window, now: now) else { return nil }
        let actual = clamp(window.usedPercent, 0, 100)
        if timing.elapsed == 0 && actual > 0 { return nil }
        let expected = clamp(historical.expectedUsedPercent, 0, 100)
        let delta = actual - expected
        return UsagePace(
            stage: stageFor(delta), basis: .historical, deltaPercent: delta,
            expectedUsedPercent: expected, actualUsedPercent: actual,
            etaSeconds: historical.etaSeconds,
            willLastToReset: historical.willLastToReset)
    }

    private static func computeCore(window: UsageWindow, now: Date) -> UsagePace? {
        guard let timing = timing(for: window, now: now) else { return nil }
        let elapsed = timing.elapsed
        let expected = clamp(elapsed / timing.duration * 100, 0, 100)
        let actual = clamp(window.usedPercent, 0, 100)
        if elapsed == 0 && actual > 0 { return nil }

        let delta = actual - expected

        var etaSeconds: Double?
        var willLastToReset = false
        if elapsed > 0 && actual > 0 {
            let rate = actual / elapsed // %% per second
            if rate > 0 {
                let remaining = max(0, 100 - actual)
                let candidate = remaining / rate
                if candidate >= timing.timeUntilReset {
                    willLastToReset = true
                } else {
                    etaSeconds = candidate
                }
            }
        } else if elapsed > 0 && actual == 0 {
            willLastToReset = true
        }

        return UsagePace(
            stage: stageFor(delta), basis: .linear, deltaPercent: delta,
            expectedUsedPercent: expected, actualUsedPercent: actual,
            etaSeconds: etaSeconds, willLastToReset: willLastToReset)
    }

    private struct WindowTiming {
        let duration: Double
        let timeUntilReset: Double
        let elapsed: Double
    }

    private static func timing(for window: UsageWindow, now: Date) -> WindowTiming? {
        guard let resetsAtRaw = window.resetsAt,
              let durationSeconds = window.paceStatus.durationSeconds,
              durationSeconds > 0,
              let resetsAt = parseRFC3339(resetsAtRaw)
        else { return nil }

        let duration = Double(durationSeconds)
        let timeUntilReset = resetsAt.timeIntervalSince(now)
        if timeUntilReset <= 0 || timeUntilReset > duration { return nil }
        return WindowTiming(
            duration: duration,
            timeUntilReset: timeUntilReset,
            elapsed: clamp(duration - timeUntilReset, 0, duration))
    }
}

/// codexbar-style historical run-out risk, e.g. "≈ 30% run-out risk", or nil.
/// A supplied pace lets presentation suppress backend risk for a Linear result.
public func runOutRiskLabel(window: UsageWindow, pace: UsagePace? = nil) -> String? {
    guard window.paceStatus.state == .available,
          pace?.basis != .linear,
          let probability = window.historicalPace?.runOutProbability
    else { return nil }
    let pct = Int((clamp(probability, 0, 1) * 100).rounded())
    if pct <= 0 { return nil }
    return "≈ %lld%% run-out risk".localized(pct)
}
