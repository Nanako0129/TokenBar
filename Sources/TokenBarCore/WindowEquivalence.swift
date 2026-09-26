import Foundation

/// "10% of this subscription's quota is worth roughly this much local usage."
///
/// Computed live from the window's own samples every time. There is no
/// token-per-percent coefficient anywhere in this file, and there must never
/// be one: the same subscription measured 21x apart between its session window
/// and its weekly window on the same day.
public enum WindowEquivalence {
    /// The token count a quota ratio may divide, for ONE message.
    ///
    /// A function rather than a convention, because this ratio is computed in
    /// two places — `row` for the live window and `QuotaHistoryFold.spanTotals`
    /// for the pooled history — and they are rendered one above the other in
    /// `QuotaView`. When the basis was a convention rather than a name, the two
    /// disagreed: fixing the pooled path alone left the card on top showing the
    /// old inflated price beside the corrected history directly below it.
    ///
    /// The FULL count, cache reads included, because the cost it is divided
    /// into is `message.cost` — the message's whole priced cost — and that
    /// cannot be narrowed to match a smaller count: `WindowMessage` carries one
    /// cost, not one per token class. Excluding cache reads therefore priced
    /// one set of tokens and counted another, and on a Claude Code workload the
    /// excluded share is most of the volume (issue #237).
    ///
    /// Deliberately NOT the bars' basis. `WindowCardGeometry` sizes bars from
    /// `tokensExCacheRead` because including cache reads decouples them from
    /// the quota line. Different question, different basis — but each stated
    /// where it is used, so a third caller has something to call rather than a
    /// precedent to copy.
    public static func ratioTokens(_ message: WindowMessage) -> Int64 { message.tokens }

    /// The provider reports whole percents, so a measured Δ carries ±0.5.
    static let quantisationHalfStep = 0.5
    /// How much relative error the displayed ratio may carry.
    static let tolerance = 0.10
    /// Derived, not chosen: ±0.5/Δ ≤ tolerance ⇒ Δ ≥ 5.
    ///
    /// Internal on purpose. `deltaQualifies` below is the whole admission rule
    /// and this is only its single-rise half; a caller holding the bare number
    /// can write `delta >= minimumDelta` and silently drop the run scaling,
    /// which is exactly what happened at two sites in the `Syrtis` target and
    /// took a review round to find. Keeping it inside this module makes that
    /// comparison fail to compile out there rather than fail review. The
    /// previous guard was a `git grep` showing one comparison, which is a
    /// statement about the text at the moment it ran and not about the
    /// repository — an edit relocates around it without touching it.
    static var minimumDelta: Double { quantisationHalfStep / tolerance }

    /// The one statement of whether a measured consumption is large enough to
    /// divide by. `minimumDelta` is the single-rise delta at which the ±0.5
    /// quantisation reaches `tolerance`; a delta summed over several rises
    /// carries that uncertainty once per rise, so it has to clear the bar once
    /// per rise to make the same claim.
    ///
    /// Four sites admit cycles — the live row, the pooled aggregate, the
    /// dashboard's qualifying-window filter and the probe — and this rule used
    /// to be written out at each of them. Scaling it at one and not the others
    /// is how `[0, 3, 0, 3]` came to be rejected by the live row as two
    /// sub-threshold rises and admitted by the pooled path as one 6-point
    /// cycle in the same build. It lives here so that cannot recur.
    ///
    /// `delta > 0` is load-bearing, not defensive: `consumed` returns a
    /// positive value only when at least one rise exists, so a positive delta
    /// implies `runs >= 1` and the product below cannot be zero. Without the
    /// guard, `runs == 0` would make the threshold zero and admit a cycle that
    /// never moved.
    public static func deltaQualifies(_ delta: Double, runs: Int) -> Bool {
        delta > 0 && delta >= minimumDelta * Double(runs)
    }

    public enum Row: Equatable, Sendable {
        /// Tokens and cost equivalent to one tenth of the window's quota.
        case ratio(tokensPerTenth: Int64, costPerTenth: Double, errorPercent: Int)
        /// Quota moved, but not far enough to survive the 1% quantisation.
        /// `deltaPercent` is always > 0 here, so `errorPercent` is defined.
        case insufficient(deltaPercent: Double, errorPercent: Int)
        /// Two or more samples, but the reading never moved. Kept separate from
        /// `insufficient` because the error term is 0.5/delta — undefined here,
        /// and a caller that folded the two would divide by zero.
        case notMoved
        /// Quota moved and this machine saw none of it — a zero here would
        /// read as "1% is free", when it means "we cannot see it".
        case unaccounted(deltaPercent: Double)
        /// Fewer than two samples inside the window, so no Δ exists at all.
        case unavailable
        /// Money estimated, tokens not — the mirror of `tokensOnly`.
        ///
        /// Reachable when enough cycles carry a price and too few carry tokens,
        /// which the supported cost-only row shape makes ordinary rather than
        /// exotic. Splitting the two estimates without splitting the threshold
        /// that guards them let a token figure derived from a single cycle sit
        /// beside a properly supported money figure, on one line, with one
        /// error bar that described only the money.
        case costOnly(costPerTenth: Double, errorPercent: Int)
        /// Tokens estimated, money not — the admitted cycles carry usage the
        /// pricing tables could not value.
        ///
        /// Kept apart from `ratio` rather than reported with a zero dollar
        /// figure, which would read as "this quota is free". Unlike `ratio`
        /// this is not gated on the tolerance: `spread` exists to avoid quoting
        /// one money figure the cycles disagree about, and here there is no
        /// money figure at all, so the token estimate with its own error bar is
        /// the entire answer rather than half of one.
        case tokensOnly(tokensPerTenth: Int64, errorPercent: Int)
        /// The cycles are fine; there are not enough of them yet.
        ///
        /// Distinct from `insufficient`, which it was folded into and which
        /// says the readings are too coarse to measure. On live data two cycles
        /// of 35% and 97% rendered as "quota moved only 132% — too little to
        /// estimate (+/-1%)": every clause false, and the one number the reader
        /// could check contradicted the sentence around it. Nothing about that
        /// window is too small. There are two of it.
        case tooFewCycles(count: Int, needed: Int)
        /// Nothing has been declared, so no usage can be charged to this
        /// subscription and the ratio has no numerator.
        ///
        /// Kept apart from `unaccounted`, which it would otherwise be
        /// indistinguishable from: that one says the quota moved and this
        /// machine recorded nothing, which for an undeclared user is false and
        /// alarming. The machine recorded plenty; the app has not been told
        /// whose it is. Most users never open that Settings page, so this is
        /// the common case, not an edge one.
        case undeclared
        /// The cycles disagree by more than the tolerance, so there is no
        /// single figure to give — only the span they cover.
        ///
        /// This is not a softer `insufficient`. That one means the readings are
        /// too coarse to measure; this means they were measured fine and the
        /// underlying rate genuinely moved. A plan change does exactly that,
        /// and the store keys its series on `(provider, account, window)` with
        /// no plan recorded, so a Plus-to-Pro upgrade lands in one series with
        /// the ratio changing partway and nothing marking where.
        case spread(lowPerTenth: Int64, highPerTenth: Int64,
                    lowCostPerTenth: Double, highCostPerTenth: Double)

        /// Whether an estimate was actually produced. Named so assertions can
        /// state the admission rule without re-deriving it from a pattern match.
        public var isRatio: Bool {
            if case .ratio = self { return true }
            return false
        }
    }

    /// The row as displayed. A pure function because the alternative — format
    /// strings inline in a SwiftUI body — has no seam to assert, and a literal
    /// `%` in one of them segfaulted the app: `String(format:)` read "10% of"
    /// as a `% o` octal conversion, ate the first argument, and sent a message
    /// to whatever the next `%@` found past the end of the argument list.
    public static func text(_ row: Row, tokens: (Int64) -> String,
                            money: (Double) -> String) -> String {
        switch row {
        case let .ratio(t, cost, error):
            return "10%% of quota ~ %@ · %@ API-equivalent, ±%@%%".localizedWindowRow(
                tokens(t), money(cost), String(error))
        case let .insufficient(delta, error):
            return "Quota moved only %@%% — too little to estimate (±%@%%)"
                .localizedWindowRow(String(Int(delta.rounded())), String(error))
        case .notMoved:
            return "Quota has not moved yet".localizedWindowRow()
        case let .unaccounted(delta):
            return "Quota moved %@%%, none of it recorded on this machine"
                .localizedWindowRow(String(Int(delta.rounded())))
        case .unavailable:
            return "Not enough quota readings yet".localizedWindowRow()
        case let .costOnly(cost, error):
            return "10%% of quota ~ %@ API-equivalent, tokens unavailable, ±%@%%"
                .localizedWindowRow(money(cost), String(error))
        case let .tokensOnly(t, error):
            return "10%% of quota ~ %@, unpriced models, ±%@%%".localizedWindowRow(
                tokens(t), String(error))
        case let .tooFewCycles(count, needed):
            return "%@ of %@ windows recorded — the estimate needs that many"
                .localizedWindowRow(String(count), String(needed))
        case .undeclared:
            return "Classify your usage in Settings to see what this window is worth"
                .localizedWindowRow()
        case let .spread(low, high, lowCost, highCost):
            return "10%% of quota ~ %@-%@ · %@-%@ API-equivalent".localizedWindowRow(
                tokens(low), tokens(high), money(lowCost), money(highCost))
        }
    }

    /// A `Double` back to `Int64` without trapping.
    ///
    /// `Int64(_:)` traps on a value outside its range and on NaN, and the
    /// values here are ratios: a saturated token count over a small quota delta
    /// scales past Int64 long before it means anything. The clamping integer
    /// initializer only accepts integers, so the bound is done here.
    public static func clamped(_ value: Double) -> Int64 {
        guard value.isFinite else { return 0 }
        if value >= Double(Int64.max) { return .max }
        if value <= Double(Int64.min) { return .min }
        return Int64(value)
    }

    /// `messages` must already be filtered to this subscription's attributed
    /// usage. `samples` must be the ones inside the window, in time order.
    public static func row(
        samples: [QuotaSample], messages: [WindowMessage]
    ) -> Row {
        guard let first = samples.first, let last = samples.last,
              samples.count >= 2
        else { return .unavailable }

        // The distance the readings travelled, not `last - first`. A reset
        // inside the span returns them to zero, and the displacement then
        // collapses while the numerator below keeps every message from both
        // sides of it — which is how a window that consumed 93 points came to
        // divide by 8 and report eleven times the true rate. One statement of
        // the rule, shared with the pooled path.
        let readings = samples.map(\.usedPercent)
        let delta = QuotaHistoryFold.consumed(readings)
        guard delta > 0 else { return .notMoved }
        // Every rise `consumed` summed was quantised on its own, so both the
        // error quoted below and the admission bar scale with how many there
        // were. `minimumDelta` is the single-rise delta at which the error
        // reaches `tolerance`; a group that crossed a reset has to clear it
        // once per rise to make the same claim. Without this, `[0, 3, 0, 3]`
        // read as a 6-point measurement at ±8% — two rises of 3 that each
        // fell short, presented as one that did not.
        let runs = QuotaHistoryFold.risingRuns(readings)

        // Numerator and denominator must cover the same interval or the ratio
        // means nothing — hence the span between samples, not the whole window.
        //
        // Held exactly at the ends and approximately in the middle: a
        // declining interval inside the span contributes to the numerator and
        // not to `consumed`. See `QuotaHistoryFold.consumed` for why that is
        // not fixed here — briefly, dropping those messages is right for a
        // reset and wrong for a correction, and the readings do not say which
        // — and for the measured size of it, which is 0.9% of the span on the
        // group that produced the defect this function was rewritten for.
        let inSpan = messages.filter {
            $0.timestamp > first.atMs && $0.timestamp <= last.atMs
        }
        let tokens = inSpan.reduce(Int64(0)) { $0.saturatingAdding(ratioTokens($1)) }
        let cost = inSpan.reduce(0.0) { $0 + $1.cost }
        let error = Int((quantisationHalfStep * Double(runs) / delta * 100).rounded())

        // "Recorded" means either kind of evidence. A provider row can carry a
        // cost with no token components, and the pooled path already admits
        // one; requiring tokens here made the single-window footer say "none of
        // it recorded on this machine" about usage sitting in the same scan.
        guard tokens > 0 || cost > 0 else { return .unaccounted(deltaPercent: delta) }
        guard deltaQualifies(delta, runs: runs) else {
            return .insufficient(deltaPercent: delta, errorPercent: error)
        }
        // Clamping, not truncating: a saturated token count over a small delta
        // scales past `Int64`, and the plain initializer traps on a Double
        // outside the range.
        let perTenth = clamped((Double(tokens) / delta * 10).rounded())
        // Same split as the pooled path, for the same reason. Admitting either
        // kind of evidence above and then always answering `.ratio` presented
        // the MISSING metric as a measured zero: an unpriced window read as a
        // $0 API equivalent, and a cost-only one as 0 tokens. Both are
        // unavailable, which is a different claim.
        guard cost > 0 else { return .tokensOnly(tokensPerTenth: perTenth, errorPercent: error) }
        guard tokens > 0 else {
            return .costOnly(costPerTenth: cost / delta * 10, errorPercent: error)
        }
        return .ratio(
            tokensPerTenth: perTenth,
            costPerTenth: cost / delta * 10,
            errorPercent: error)
    }

    /// A cycle's contribution to the pooled estimate.
    public struct Cycle: Equatable, Sendable {
        public let deltaPercent: Double
        /// Restricted to the cycle's observed sample span — the only interval
        /// the delta describes.
        public let spanTokens: Int64
        public let spanCost: Double
        public let observedFraction: Double
        /// How many separately measured rises `deltaPercent` sums; the
        /// admission bar scales by it. See `deltaQualifies`.
        public let risingRuns: Int

        public init(
            deltaPercent: Double, spanTokens: Int64, spanCost: Double,
            observedFraction: Double, risingRuns: Int = 1
        ) {
            self.deltaPercent = deltaPercent
            self.spanTokens = spanTokens
            self.spanCost = spanCost
            self.observedFraction = observedFraction
            self.risingRuns = risingRuns
        }
    }

    /// Below this the cycle was barely witnessed, so its delta describes a
    /// stretch the app mostly missed. `QuotaCycle` already carried the fraction
    /// and said so in its own comment; nothing was gating on it.
    public static let minimumObservedFraction = 0.5
    /// Fewer than this and the leave-one-out spread is not computable in any
    /// meaningful way.
    public static let minimumCycles = 3

    // Both gates above also exclude a one-sample cycle, and they do it
    // structurally rather than by where the numbers are set: such a cycle's
    // delta is `max - min` over a single reading and its observed fraction is
    // the span between first and last sample, so BOTH are exactly zero
    // (`QuotaHistory.swift:142,144`). Lowering either constant therefore does
    // not start admitting them — worth knowing before anyone assumes it does.

    /// One estimate pooled over several cycles.
    ///
    /// Sum the deltas, sum the spend, divide ONCE. Not the average of per-cycle
    /// ratios: quantisation is an absolute ±0.5 on each delta, so a cycle's
    /// relative noise runs as 1/delta, and pooling weights each cycle by the
    /// evidence it carries while averaging gives a 5-point cycle the same say
    /// as a 98-point one.
    ///
    /// That distinction is the whole feature. Measured 2026-08-17 on 14 live
    /// cycles: the per-cycle ratios spread 2.7x (38% half-range), while the
    /// pooled estimate agreed to 1% between independent halves and carries a
    /// jackknife standard error of 5%. Reporting the per-cycle spread as the
    /// estimate's error — which this function used to do — overstated it by
    /// most of an order of magnitude and made a usable number look unusable.
    ///
    /// Denominated in money. Tokens are also returned, but cost is the steadier
    /// of the two here (5% against 7%), which is consistent with providers
    /// metering on something closer to cost than to a token count.
    /// `declared` is whether the user has classified anything as belonging to
    /// THIS subscription — `UsageAttribution.declares(subscription:records:)`
    /// is the one place that answers it. It belongs as a parameter rather than
    /// a decision made here because the distinction it makes is this type's
    /// own: with this subscription undeclared every one of its messages
    /// resolves to unassigned, so every cycle arrives carrying zero spend, and
    /// the rules below would report that as "the quota moved and none of it
    /// was recorded on this machine". That sentence describes a data failure.
    /// The actual state is a missing declaration, and it is the state most
    /// users are in.
    ///
    /// It used to be documented — and computed — as "has the user classified
    /// ANY source", which is a question about the table. Declaring one client
    /// then answered for every other, so an undeclared client's card described
    /// usage sitting in the same scan as never recorded (issue #320).
    /// The fold for a subscription, given the attribution table rather than a
    /// conclusion drawn from it.
    ///
    /// This is the overload every shipping call site uses, and it exists
    /// because the other one was too easy to answer wrongly. Three call sites
    /// each derived `declared` themselves and each derived it as
    /// `!records.isEmpty` — a question about the table, not about the
    /// subscription — so declaring one client made every other client's card
    /// claim its usage was never recorded (issue #320). Passing the question
    /// instead of the answer removes the opportunity rather than documenting
    /// it: there is no boolean here for a caller to compute from the wrong
    /// subject.
    ///
    /// `subscription` must be the same one the spans were narrowed to. The
    /// fold describes those spans, and describing them by a different
    /// subscription's declaration state is exactly the defect above.
    public static func aggregate(
        subscription: String, records: [UsageAttribution.Record], cycles: [Cycle]
    ) -> Row {
        aggregate(
            declared: UsageAttribution.declares(
                subscription: subscription, records: records),
            cycles: cycles)
    }

    /// Prefer `aggregate(subscription:records:cycles:)`. This overload takes
    /// the answer; that one takes the question. Every shipping call site uses
    /// the latter, and issue #320 is why: a caller holding a `Bool` is a
    /// caller that had to derive it, and all three derived it from the wrong
    /// subject. Kept public because the fold's own behaviour — that the flag,
    /// not the empty spend, is what selects `.undeclared` — has to be
    /// assertable without building an attribution table.
    public static func aggregate(declared: Bool = true, cycles: [Cycle]) -> Row {
        guard declared else { return .undeclared }
        // Admission is about EVIDENCE, not about money. Gating on `spanCost`
        // alone rejected every cycle whose models carry no price — the tokens
        // were recorded and the quota moved, and the caller then reported that
        // as "none of it recorded on this machine", which is false about the
        // one thing it could see.
        let admitted = cycles.filter {
            deltaQualifies($0.deltaPercent, runs: $0.risingRuns)
                && $0.observedFraction >= minimumObservedFraction
                && ($0.spanCost > 0 || $0.spanTokens > 0)
        }
        guard !admitted.isEmpty else {
            let anyMovement = cycles.reduce(0.0) { $0 + $1.deltaPercent }
            if cycles.isEmpty { return .unavailable }
            if anyMovement <= 0 { return .notMoved }
            // Both kinds of evidence, like the admission gate above. This
            // classifier kept the old cost-only predicate, so cycles carrying
            // real tokens from unpriced models — none of them large enough to
            // be admitted — were still called usage nobody recorded. Reachable:
            // `QuotaHistoryCard` calls `aggregate` with no prefilter, so that
            // card could print "none of it recorded on this machine" above rows
            // listing the tokens it recorded.
            if cycles.allSatisfy({ $0.spanCost <= 0 && $0.spanTokens <= 0 }) {
                return .unaccounted(deltaPercent: anyMovement)
            }
            return .insufficient(
                deltaPercent: anyMovement,
                // One half-step per RISE, summed over every cycle offered —
                // not per cycle. A merged cycle carries several quantised
                // rises and its error is that many half-steps wide. Over
                // `cycles`, not `admitted`: inside this branch `admitted` is
                // empty by construction, and a sum over it would quote ±0%.
                errorPercent: Int((quantisationHalfStep
                    * Double(cycles.reduce(0) { $0 + $1.risingRuns })
                    / anyMovement * 100).rounded()))
        }
        // Count, not size: these cycles each cleared `minimumDelta` on their
        // own, so saying the quota "moved only" their sum is false, and the
        // error term computed from that sum is small precisely because the
        // movement was large. What is missing is cycles to compare.
        guard admitted.count >= minimumCycles else {
            return .tooFewCycles(count: admitted.count, needed: minimumCycles)
        }

        func pooled(_ set: [Cycle], _ pick: (Cycle) -> Double) -> Double {
            let delta = set.reduce(0.0) { $0 + $1.deltaPercent }
            return delta > 0 ? set.reduce(0.0) { $0 + pick($1) } / delta : 0
        }
        // Each estimate pools over the cycles that carry ITS evidence. A cycle
        // contributing quota delta to a denominator while contributing nothing
        // to the numerator drags that estimate down in proportion to how much
        // of the history it represents — true of unpriced cycles for the money
        // figure, and equally true of cost-only cycles for the token figure.
        // Filtering one and not the other was the same defect twice, and only
        // the first half was fixed.
        let priced = admitted.filter { $0.spanCost > 0 }
        let tokenBearing = admitted.filter { $0.spanTokens > 0 }
        let costRatio = pooled(priced) { $0.spanCost }
        let tokenRatio = pooled(tokenBearing) { Double($0.spanTokens) }

        /// Leave-one-out spread of a pooled estimate, relative to the estimate.
        func jackknifeRelative(_ set: [Cycle], _ pick: (Cycle) -> Double) -> Double {
            let estimate = pooled(set, pick)
            guard set.count >= minimumCycles, estimate > 0 else { return .infinity }
            let n = Double(set.count)
            var leaveOneOut: [Double] = []
            for index in set.indices {
                var rest = set
                rest.remove(at: index)
                leaveOneOut.append(pooled(rest, pick))
            }
            let mean = leaveOneOut.reduce(0, +) / n
            let variance = (n - 1) / n
                * leaveOneOut.reduce(0.0) { $0 + ($1 - mean) * ($1 - mean) }
            return variance.squareRoot() / estimate
        }

        // No defensible money figure: report what IS known rather than a
        // dollar amount pooled over cycles that carry no price. And if neither
        // estimate has enough cycles behind it, say that instead of inventing
        // one from the handful that do.
        guard priced.count >= minimumCycles else {
            guard tokenBearing.count >= minimumCycles else {
                return .tooFewCycles(
                    count: max(priced.count, tokenBearing.count), needed: minimumCycles)
            }
            let tokenError = jackknifeRelative(tokenBearing) { Double($0.spanTokens) }
            return .tokensOnly(
                tokensPerTenth: clamped((tokenRatio * 10).rounded()),
                errorPercent: tokenError.isFinite
                    ? max(1, Int((tokenError * 100).rounded())) : 0)
        }

        // Jackknife: recompute the pooled estimate with each cycle left out and
        // take the spread of those. It measures the ESTIMATE's stability rather
        // than the observations' scatter, and it needs nothing beyond what is
        // already stored.
        //
        // It does NOT catch a plan change, and this comment used to claim it
        // did. The user's own history contains a Pro-to-Plus downgrade and the
        // jackknife still reported 5%. That is structural, not bad luck: this
        // statistic reads how much the cycles disagree with each other, while a
        // plan change alters the UNIT each cycle's `deltaPercent` is denominated
        // in — 3% of a Pro allowance and 3% of a Plus one are different absolute
        // quantities. The unit moves, the dispersion does not, and an error bar
        // built on dispersion cannot see it.
        //
        // So do not build anything else on the assumption that a widening error
        // bar will flag a regime change. The fix is to record the plan on each
        // sample and refuse to pool across two of them; that field is landing
        // with the account-scope schema bump, and `Cycle` gains a `planKey` to
        // partition on. Until it does, an estimate spanning a plan change is
        // silently wrong and nothing here can tell.
        // The same threshold on the other estimate. Enough priced cycles says
        // nothing about how many carried tokens, and publishing a token figure
        // from fewer than `minimumCycles` would breach the rule this function
        // enforces two guards above — quietly, because the error bar beside it
        // is the money's.
        guard tokenBearing.count >= minimumCycles else {
            let costError = jackknifeRelative(priced) { $0.spanCost }
            return .costOnly(
                costPerTenth: costRatio * 10,
                errorPercent: costError.isFinite
                    ? max(1, Int((costError * 100).rounded())) : 0)
        }

        // BOTH estimates are jackknifed, and the row is only a `.ratio` if both
        // hold. The cost error alone used to decide it, and the row then
        // published a token figure under the cost's error bar: cycles with
        // stable cost per point but differently priced models have unstable
        // tokens per point, so "1% error" could sit beside a token number the
        // cycles disagreed about wildly. The bar was measured on one quantity
        // and printed beside two.
        let costError = jackknifeRelative(priced) { $0.spanCost }
        let tokenError = jackknifeRelative(tokenBearing) { Double($0.spanTokens) }
        let relativeError = max(costError, tokenError)

        guard relativeError <= tolerance else {
            let ratios = priced.map { $0.spanCost / $0.deltaPercent * 10 }
            let tokens = tokenBearing.map { Double($0.spanTokens) / $0.deltaPercent * 10 }
            return .spread(
                lowPerTenth: clamped((tokens.min() ?? 0).rounded()),
                highPerTenth: clamped((tokens.max() ?? 0).rounded()),
                lowCostPerTenth: ratios.min() ?? 0,
                highCostPerTenth: ratios.max() ?? 0)
        }
        return .ratio(
            tokensPerTenth: clamped((tokenRatio * 10).rounded()),
            costPerTenth: costRatio * 10,
            errorPercent: max(1, Int((relativeError * 100).rounded())))
    }

}


extension String {
    /// Same lookup-then-format as `localized(_:)`, named apart only so the
    /// window-card strings can be asserted from TokenBarCore's own test seam.
    func localizedWindowRow(_ arguments: any CVarArg...) -> String {
        String(format: localized, arguments: arguments)
    }
}
