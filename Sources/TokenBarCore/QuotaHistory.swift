import Foundation

/// Past quota windows, each joined to what was actually spent inside it.
///
/// The window card answers "this window"; this answers "the windows before
/// it", and the join is the point. Measured 2026-08-16 on live data, a
/// `claude/session.v1` window moved **1%** while 308M tokens and $535 of
/// API-equivalent value were recorded in the same five hours — because almost
/// all of it was `gpt-5.6-sol`, which is charged to a different subscription.
/// A history that showed either half alone would be actively misleading.

/// One recorded reset cycle, derived from the persisted quota curve alone.
public struct QuotaCycle: Equatable, Sendable {
    /// Reset instant in ms. Doubles as the cycle's identity — the engine groups
    /// its samples by exactly this value.
    public let resetAtMs: Int64
    public let startMs: Int64
    /// How much of the allowance this cycle consumed: the distance the readings
    /// travelled, summed over their rises, NOT the highest reading alone and
    /// NOT the range between the lowest and the highest.
    ///
    /// Counting only rises is not cosmetic. Sampling starts whenever the app
    /// happens to be running, so a cycle first observed at 40% used would report
    /// 40% as its consumption when the app only witnessed the last few points of
    /// it. What travelled between readings is what this app can honestly claim
    /// to have seen.
    ///
    /// The range was the earlier answer and agrees with this one on every cycle
    /// whose readings only rise. It stops agreeing when a reset falls inside the
    /// group: the readings return to zero and the range then saturates at the
    /// highest reading while the consumption keeps going. See
    /// `QuotaHistoryFold.consumed`, which states the rule once for both this and
    /// the live single-window path.
    ///
    /// Note the floor this leaves: `QuotaCurvePoint` rejects `usedPercent == 0`
    /// at decode, so a cycle's first reading is always above zero and the
    /// distance necessarily omits whatever was consumed before it. That
    /// understates, and understating is the right direction — the alternative
    /// assumes the app witnessed a window it may have joined late.
    ///
    /// May exceed 100 when a group holds more than one window's worth of
    /// consumption, which is the deliberate outcome of keeping a window keyed by
    /// its reset time rather than splitting it at a reset the provider never
    /// reported.
    public let usedPercent: Double
    /// How many separately measured rises `usedPercent` sums. One on a cycle
    /// whose readings only rose; more when a reset fell inside the group. The
    /// admission bar and the quoted error scale by it, because each rise was
    /// quantised on its own. See `QuotaHistoryFold.risingRuns`.
    public let risingRuns: Int
    /// The highest absolute reading seen in this cycle.
    ///
    /// Separate from `usedPercent`, which is the observed SPAN. The two answer
    /// different questions and only coincide when the app watched the cycle
    /// from zero: a cycle first seen at 40% and last seen at 100% consumed 60
    /// points as far as this machine can tell, and reached the ceiling. Deriving
    /// "never ran out" from the span called that cycle a quiet one.
    public let peakUsedPercent: Double
    public let sampleCount: Int
    /// The instants of the first and last reading in this cycle.
    ///
    /// Carried because a ratio is only meaningful when its numerator and
    /// denominator cover the same interval, and the denominator is a span
    /// between two readings — not the whole window. Usage from a stretch the
    /// app was not running for would otherwise be counted against quota
    /// movement nobody observed. `WindowEquivalence.row` already had this
    /// right; the aggregate over cycles did not, and it cost 8 points of
    /// spread on live data (46% to 38%).
    public let firstSampleMs: Int64
    public let lastSampleMs: Int64

    /// How far back this cycle's evidence reaches — the earlier of where the
    /// window is computed to have started and where sampling actually began.
    ///
    /// Normally `startMs`, since the first reading lands after the window
    /// opens. They invert when the provider SHORTENS its reported duration
    /// mid-cycle: `startMs` is derived from the newest point's duration, so a
    /// window that went from seven days to five moves its own start forward,
    /// past readings already taken. Bounding a message scan at `startMs` would
    /// then drop usage from before it while `usedPercent` — the span between
    /// the first and last reading — still counts the movement those readings
    /// showed. Numerator short, denominator whole.
    public var evidenceStartMs: Int64 { min(startMs, firstSampleMs) }
    /// Fraction of the window the samples actually cover, 0...1. A cycle
    /// observed for eight minutes of five hours is not evidence about that
    /// cycle, and the UI has to be able to say so.
    public let observedFraction: Double

    public var durationMs: Int64 { resetAtMs - startMs }

    public init(
        resetAtMs: Int64, startMs: Int64, usedPercent: Double,
        sampleCount: Int, observedFraction: Double,
        firstSampleMs: Int64 = 0, lastSampleMs: Int64 = 0,
        peakUsedPercent: Double? = nil,
        risingRuns: Int = 1
    ) {
        self.peakUsedPercent = peakUsedPercent ?? usedPercent
        self.risingRuns = risingRuns
        self.resetAtMs = resetAtMs
        self.startMs = startMs
        self.usedPercent = usedPercent
        self.sampleCount = sampleCount
        self.observedFraction = observedFraction
        self.firstSampleMs = firstSampleMs
        self.lastSampleMs = lastSampleMs
    }
}

/// A cycle plus what was spent inside it, split by whether it counted against
/// the subscription this history belongs to.
public struct QuotaHistoryRow: Equatable, Sendable, Identifiable {
    public let cycle: QuotaCycle
    /// Attributed to THIS subscription — the number the quota bar is about.
    public let mineTokens: Int64
    /// The same total on the BARS' basis — cache reads removed.
    ///
    /// Not the equivalence's basis. `spanTokens` below is the full count, for
    /// the reason given there; this one matches `WindowCardGeometry`, which
    /// sizes the bars from everything except cache reads because including
    /// them decouples the bars from the quota line (measured: 4.5x vs 1.2x
    /// discrimination). A figure printed beside the bars should be countable
    /// in them.
    public let mineTokensExCacheRead: Int64
    /// `mineTokens` split by token class, for the row's hover breakdown.
    ///
    /// Five classes, because `WindowMessage.tokens` sums exactly five and
    /// `mineTokens` is its running total: a four-way split would not add up to
    /// the number printed beside it, and a breakdown that does not reconcile
    /// with the figure it explains is worse than none. `TokenBreakdown.total`
    /// exists so that identity can be asserted rather than assumed.
    public let mineBreakdown: TokenBreakdown
    public let mineCost: Double
    /// The same two quantities restricted to the cycle's OBSERVED span, which
    /// is the only interval the quota delta describes. The whole-window figures
    /// above are what the row displays — "this window cost me X" is a question
    /// about the window — while these are the only ones a ratio may divide.
    ///
    /// The FULL token count, cache reads included, and that is the load-bearing
    /// part. These two feed `WindowEquivalence`, which prints them on one line
    /// as "10% of quota ~ X tokens · $Y API-equivalent" — two descriptions of
    /// the same work, which a reader divides. `spanCost` is `message.cost`, the
    /// message's whole priced cost, and a message's cost cannot be decomposed
    /// here: `WindowMessage` carries one `cost`, not one per token class. So a
    /// count excluding cache reads beside a cost including them is the one
    /// pairing that cannot be made consistent, and it inflated the implied
    /// per-token price by the cache-read share — most of a Claude Code
    /// workload, which this repo's own bar comment puts at 200x the volume at
    /// a tenth the price.
    ///
    /// Full-and-full is therefore not a preference between two workable
    /// options; it is the only achievable pairing until per-class cost crosses
    /// the FFI. See issue #237.
    public let spanTokens: Int64
    public let spanCost: Double
    /// Everything else recorded in the same interval, summed. Not noise: it is
    /// the answer to "the window barely moved, so where did the work go".
    ///
    /// THREE attribution states, not one. A message lands here when the user
    /// declared it against another subscription, when they declared it excluded,
    /// and when they have not classified it at all — and the card called the
    /// whole bucket "other subscriptions", which on an undeclared setup is a
    /// claim about every message this machine recorded. `otherAssigned` carries
    /// the part that claim is true of, so the copy can name what it knows.
    public let otherTokens: Int64
    public let otherCost: Double
    /// Which of those states are PRESENT in the bucket, as facts rather than
    /// as totals.
    ///
    /// Deliberately not a token count. The first version carried
    /// `otherAssignedTokens` and the label compared it against `otherTokens` —
    /// which is a comparison in one dimension over data that has two, so an
    /// unclassified row carrying cost and no tokens made the totals equal and
    /// the whole bucket read as "other subscriptions" while some of the spend
    /// was unclassified. A presence flag cannot be fooled by which dimension a
    /// contribution happens to arrive in.
    public let otherHasAssigned: Bool
    /// Declared EXCLUDED by the user, kept apart from never-classified.
    ///
    /// Both are "not this subscription's", so the fold sent them to one flag
    /// and the card labelled the result "Unclassified usage" — telling the user
    /// their own explicit exclusion was work they had not got round to
    /// classifying. Excluding a source IS classifying it; the difference
    /// between "I dealt with this" and "there is something here for you to
    /// deal with" is the only thing this line is read for.
    public let otherHasExcluded: Bool
    public let otherHasUnattributed: Bool
    /// This subscription's models, largest first.
    public let models: [QuotaHistoryModel]

    public var id: Int64 { cycle.resetAtMs }
}

public struct QuotaHistoryModel: Equatable, Sendable, Identifiable {
    /// Carried alongside the model because `ModelColorMap.color` keys on the
    /// pair. Without it the segments here would be coloured by a different rule
    /// than the same models in the model breakdown and the usage chart, and one
    /// model would be two colours depending on which card you were looking at.
    public let providerId: String
    public let modelId: String
    public let tokens: Int64
    /// `tokens` split by class, on the same five lanes as the row's. A model
    /// line and the row above it are the same question at two scopes, so a
    /// reader who can break one down and not the other is being told the
    /// detail exists only sometimes.
    public let breakdown: TokenBreakdown
    public let cost: Double

    public var id: String { "\(providerId)|\(modelId)" }
}

public enum QuotaHistoryFold {
    /// Grouping key for the model breakdown. The pair, not the model alone:
    /// the same model id reached through two providers is two rows everywhere
    /// else in this app, and collapsing them here would make this card the one
    /// place that disagrees.
    public struct ModelKey: Hashable {
        public let providerId: String
        public let modelId: String
        public init(providerId: String, modelId: String) {
            self.providerId = providerId
            self.modelId = modelId
        }
    }

    /// Groups curve points into cycles, newest first.
    ///
    /// `durationSeconds` is per point rather than per cycle, so the cycle's
    /// length is taken from its own newest point: a window whose provider changed
    /// its reported duration mid-cycle should be placed by what it reports now,
    /// not by what it reported when sampling began.
    /// Completed cycles only.
    ///
    /// The running cycle is excluded, and each point says for itself whether it
    /// is in it. Folding it in put the running cycle in a strip captioned "past
    /// windows", let a partially observed span stand beside completed ones, and
    /// let it count toward the three-cycle threshold the equivalence needs — an
    /// estimate that would then change under the reader as the cycle filled.
    ///
    /// The first version took the curve's `activeResetAt` and compared it to
    /// each point's `resetAt`. Those are not comparable values: the engine
    /// publishes the RAW provider reset there while every stored `resetAt` has
    /// been through `normalize_sample_reset`, so the exclusion silently did
    /// nothing whenever the provider's reset was off the quantum — which is
    /// most of the time. `isActiveGroup` is the producer's own answer, so there
    /// is no longer a parameter a caller can omit and no rule stated twice in
    /// two languages.
    ///
    /// Returns EVERY recorded cycle. The cap that bounds the message scan is
    /// `considered(_:)`, applied by the consumers that pay for a scan — not
    /// here. Putting it here looked like the tidier place ("bound it once, at
    /// the source") and was wrong: `QuotaOverviewFold.summaries` derives
    /// LIFETIME facts from this array — `peakPercent`, `neverExhausted`,
    /// `cycleCount` — and costs no scan at all. A capped fold made a window
    /// that ran out thirty-three cycles ago report that it never had.
    public static func cycles(points: [QuotaCurvePoint]) -> [QuotaCycle] {
        var grouped: [Int64: [QuotaCurvePoint]] = [:]
        for point in points where !point.isActiveGroup {
            grouped[point.resetAt, default: []].append(point)
        }

        return grouped.compactMap { resetAt, raw -> QuotaCycle? in
            let sorted = raw.sorted { $0.sampledAt < $1.sampledAt }
            guard let last = sorted.last, let first = sorted.first,
                  last.durationSeconds > 0
            else { return nil }
            let used = sorted.map(\.usedPercent)
            let start = resetAt - last.durationSeconds
            return QuotaCycle(
                resetAtMs: resetAt * 1000,
                startMs: start * 1000,
                usedPercent: consumed(used),
                sampleCount: sorted.count,
                observedFraction: min(1, max(0, Double(last.sampledAt - first.sampledAt)
                    / Double(last.durationSeconds))),
                firstSampleMs: first.sampledAt * 1000,
                lastSampleMs: last.sampledAt * 1000,
                peakUsedPercent: used.max() ?? 0,
                risingRuns: risingRuns(used))
        }
        .sorted { $0.resetAtMs > $1.resetAtMs }
    }

    /// The newest cycles a scan-paying surface may look at.
    ///
    /// Applied by the consumers whose cost grows with the answer: the history
    /// card's cycle list, which bounds the union scan through its oldest
    /// entry's `evidenceStartMs`, and the admitted set behind the equivalence.
    /// Lifetime summaries deliberately do not call this.
    public static func considered(_ cycles: [QuotaCycle]) -> [QuotaCycle] {
        Array(cycles.prefix(consideredCycles))
    }

    /// How far back any cycle-derived surface reaches, in cycles.
    ///
    /// The engine retains 128 cycles per series and this fold used to return
    /// all of them, but nothing downstream wants that many: the history card
    /// draws 12 rows and the overview strip 16. The cost of the extra ones is
    /// not the list, it is that the OLDEST cycle sets where the message scan
    /// starts — `min(windowStart, cycles.last.startMs)` — so a 5-hour session
    /// window at 128 cycles asked for a 26-day scan to render twelve rows, and
    /// a weekly window walked that start backwards for ever. Capping the fold
    /// bounds the scan as a consequence, which is why the cap lives here and
    /// not at each consumer.
    ///
    /// 32, not the 16 the issue proposed. `--window-probe` swept the cap over
    /// this machine's real history on 2026-08-21: at 20 and above the session
    /// window reports ~650-690k tokens per 10% with an 8-10% error bar, and at
    /// 12 and below it reports no figure at all, only a 242k-955k spread.
    ///
    /// 16 is the cliff edge, and the sweep caught it moving. Two runs an hour
    /// apart, differing by one newly completed cycle, put 16 on opposite sides:
    /// `spread` at 27 recorded cycles, `ratio` at 28. A cap whose output flips
    /// between "here is the number" and "we cannot say" as one ordinary cycle
    /// closes is not a bound, it is a coin toss the user watches. 16 is enough
    /// for the jackknife in principle and is not on the data, because admitted
    /// cycles are a subset of recorded ones and the pool empties faster than
    /// the count suggests.
    ///
    /// 32 does not bite on any window here today (the widest has 27), which is
    /// the point: it bounds the growth without moving a number anyone reads.
    ///
    /// The surface that has to fit inside it is the history card's row list,
    /// which draws `QuotaHistoryCard.visibleRows`; a cap below that would
    /// silently draw fewer rows than the card intends, so a selftest asserts
    /// the fit. NOT `QuotaOverviewFold.stripLength`, which an earlier version
    /// of this comment named: the overview strip reads the uncapped `collected`
    /// list, so the cap never reaches it.
    public static let consideredCycles = 32

    /// Joins each cycle to the messages inside it.
    ///
    /// `subscription` is the attribution target this history belongs to — the
    /// subscription whose quota the cycles measure. A message counts as "mine"
    /// only when the user's own declaration assigns it there; excluded and
    /// unassigned usage lands in the other column rather than being dropped,
    /// because an unclassified source still consumed real time in that window.
    /// `modelScope` narrows the fold to the model a window's allowance counts,
    /// and has NO DEFAULT on purpose — see the note on `spans`.
    /// for a provider-scoped window like Claude's "Fable only" weekly limit.
    /// Nil means the window is not scoped and every model counts.
    ///
    /// Applied HERE rather than by the caller, and that is the point of the
    /// parameter existing. The current-window chart, these history rows and the
    /// equivalence spans are three surfaces of one quota; narrowing only the
    /// chart made them disagree about the same window — corrected bars above,
    /// uncorrected "past windows" underneath. A rule the fold owns cannot be
    /// applied at two of three call sites.
    public static func rows(
        cycles: [QuotaCycle], messages: [WindowMessage], subscription: String,
        modelScope: String?,
        confirmed: [UsageAttribution.Record]
    ) -> [QuotaHistoryRow] {
        // Sorted once, then each cycle takes a contiguous slice: the naive
        // filter-per-cycle is O(cycles x messages), and on live data that is
        // 15 x 45,844 walks of the whole array on the main actor.
        let sorted = inScope(messages, modelScope).sorted { $0.timestamp < $1.timestamp }
        let stamps = sorted.map(\.timestamp)
        let spans = spanTotals(
            cycles: cycles, sorted: sorted, stamps: stamps,
            subscription: subscription, confirmed: confirmed)

        return zip(cycles, spans).map { cycle, span in
            // `[evidenceStart, reset)` — inclusive at the start, exclusive at
            // the reset. The start is `evidenceStartMs` rather than `startMs`
            // so this column cannot come out SMALLER than the span inside it
            // when a provider shortens its reported duration; a lengthened
            // duration still moves `startMs` backwards, which `min` leaves
            // alone and which no cycle boundary here has ever guarded. The reset instant is when the allowance refills, so work
            // stamped exactly there was charged to the cycle that instant
            // OPENS, not the one it closes. Adjacent cycles share that
            // boundary, so getting it wrong double counts rather than merely
            // misfiling.
            let lo = lowerBound(stamps, cycle.evidenceStartMs)
            let hi = lowerBound(stamps, cycle.resetAtMs)
            var mine = (tokens: Int64(0), exCacheRead: Int64(0), cost: 0.0)
            // Per class, alongside the totals rather than derived from them:
            // `message.tokens` is already a sum, and a split recovered by
            // subtraction is the mistake `tokensExCacheRead` documents.
            var split = (input: Int64(0), output: Int64(0), cacheRead: Int64(0),
                         cacheWrite: Int64(0), reasoning: Int64(0))
            var other = (tokens: Int64(0), cost: 0.0, hasAssigned: false,
                         hasExcluded: false, hasUnattributed: false)
            var byModel: [ModelKey: (tokens: Int64, cost: Double, split: TokenBreakdown)] = [:]

            for message in sorted[lo..<max(lo, hi)] {
                let state = UsageAttribution.resolve(
                    client: message.client, provider: message.providerId,
                    model: message.modelId, records: confirmed)
                if case let .assigned(target) = state, target == subscription {
                    // `saturatingAdding`, like every other fold over these
                    // counters. Saturating per message is not enough: two rows
                    // that each saturate still trap when added together, and
                    // the accumulator is where a corrupt transcript would land.
                    let exCacheRead = message.tokensExCacheRead
                    mine.tokens = mine.tokens.saturatingAdding(message.tokens)
                    mine.exCacheRead = mine.exCacheRead.saturatingAdding(exCacheRead)
                    split.input = split.input.saturatingAdding(message.input)
                    split.output = split.output.saturatingAdding(message.output)
                    split.cacheRead = split.cacheRead.saturatingAdding(message.cacheRead)
                    split.cacheWrite = split.cacheWrite.saturatingAdding(message.cacheWrite)
                    split.reasoning = split.reasoning.saturatingAdding(message.reasoning)
                    mine.cost += message.cost
                    let key = ModelKey(
                        providerId: message.providerId, modelId: message.modelId)
                    let current = byModel[key] ?? (0, 0, TokenBreakdown())
                    byModel[key] = (
                        current.tokens.saturatingAdding(message.tokens),
                        current.cost + message.cost,
                        // Per class here too, accumulated rather than derived,
                        // for the reason the row's own split states.
                        TokenBreakdown(
                            input: current.split.input.saturatingAdding(message.input),
                            output: current.split.output.saturatingAdding(message.output),
                            cacheRead: current.split.cacheRead.saturatingAdding(message.cacheRead),
                            cacheWrite: current.split.cacheWrite
                                .saturatingAdding(message.cacheWrite),
                            reasoning: current.split.reasoning
                                .saturatingAdding(message.reasoning)))
                } else {
                    // Three states reach here, and they mean three different
                    // things to the person reading the line: someone else's
                    // subscription, a source they excluded, and one they have
                    // not classified. Only the last is an open question.
                    switch state {
                    case .assigned: other.hasAssigned = true
                    case .excluded: other.hasExcluded = true
                    case .unassigned: other.hasUnattributed = true
                    }
                    other.tokens = other.tokens.saturatingAdding(message.tokens)
                    other.cost += message.cost
                }
            }

            return QuotaHistoryRow(
                cycle: cycle,
                mineTokens: mine.tokens, mineTokensExCacheRead: mine.exCacheRead,
                mineBreakdown: TokenBreakdown(
                    input: split.input, output: split.output,
                    cacheRead: split.cacheRead, cacheWrite: split.cacheWrite,
                    reasoning: split.reasoning),
                mineCost: mine.cost,
                spanTokens: span.tokens, spanCost: span.cost,
                otherTokens: other.tokens, otherCost: other.cost,
                otherHasAssigned: other.hasAssigned,
                otherHasExcluded: other.hasExcluded,
                otherHasUnattributed: other.hasUnattributed,
                models: byModel
                    .map {
                        QuotaHistoryModel(
                            providerId: $0.key.providerId, modelId: $0.key.modelId,
                            tokens: $0.value.tokens, breakdown: $0.value.split,
                            cost: $0.value.cost)
                    }
                    // Tokens, then cost, then the model key. Ordering on
                    // tokens alone leaves every cost-only model tied at zero,
                    // so their order came from dictionary iteration while the
                    // card renders `prefix(4)` — the four shown could reshuffle
                    // between refreshes and omit the largest recorded spend.
                    // The key breaks the remaining ties so the order is stable
                    // rather than merely deterministic-per-run.
                    .sorted {
                        if $0.tokens != $1.tokens { return $0.tokens > $1.tokens }
                        if $0.cost != $1.cost { return $0.cost > $1.cost }
                        return ($0.providerId, $0.modelId) < ($1.providerId, $1.modelId)
                    })
        }
    }

    /// First index whose value is >= `value`.
    /// What this subscription spent inside each cycle's OBSERVED span —
    /// `(firstSampleMs, lastSampleMs]`, the interval between the two readings
    /// the cycle's `usedPercent` is the difference of.
    ///
    /// One statement of that rule, for the two surfaces that need it: the
    /// history card's per-cycle numbers and the equivalence estimate's
    /// numerators. It was written twice, and the second copy re-filtered the
    /// whole message array per cycle on the main actor — the same
    /// O(cycles x messages) shape the comment in `rows` documents having
    /// removed, reintroduced by a second implementation of the same fold.
    ///
    /// Returned parallel to `cycles`, one entry each, so a caller that already
    /// has the sorted array pays no second sort.
    /// Same `modelScope` contract as `rows`: the equivalence estimate divides a
    /// quota delta by the usage that produced it, so counting a model the
    /// allowance does not charge inflates the denominator and understates the
    /// price of the quota.
    ///
    /// No default value, and the omission is the safeguard. When this argument
    /// defaulted to nil, `--window-probe` kept compiling unchanged and silently
    /// began measuring something the shipping path no longer computes — and
    /// that probe exists to tune `consideredCycles` against the shipping
    /// calculation, so a divergence there invalidates the measurement rather
    /// than merely differing from it. A required argument turns "a caller
    /// forgot" into a build error, which is the only version of this rule that
    /// cannot be forgotten again.
    public static func spans(
        cycles: [QuotaCycle], messages: [WindowMessage], subscription: String,
        modelScope: String?,
        confirmed: [UsageAttribution.Record]
    ) -> [(tokens: Int64, cost: Double)] {
        let sorted = inScope(messages, modelScope).sorted { $0.timestamp < $1.timestamp }
        return spanTotals(
            cycles: cycles, sorted: sorted, stamps: sorted.map(\.timestamp),
            subscription: subscription, confirmed: confirmed)
    }

    /// How much allowance a run of readings records consuming: the distance
    /// they travelled, summed over the rises between consecutive readings.
    ///
    /// Neither the range nor the displacement, though on a cycle whose readings
    /// only rise all three are the same number. They stop agreeing the moment a
    /// reset falls inside the group: the readings return to zero, so the
    /// displacement collapses toward nothing while the range saturates at
    /// whatever the highest reading happened to be. Measured on a real merged
    /// group — 8 by displacement, 85 by range, 93 actually travelled. The
    /// numerator keeps every message from both sides of that reset whichever
    /// denominator is used, so one that does not accumulate makes the ratio
    /// report a multiple of the truth: 11.6x on that group, which is where a
    /// card reading `$2974.92` came from.
    ///
    /// Only rises count, which keeps the property the range was chosen for.
    /// Sampling starts whenever the app happens to be running, so a cycle first
    /// seen at 40% must not claim the 40% nobody watched.
    ///
    /// Order-dependent where the range was not. Every caller passes readings
    /// already sorted by sample time; one that did not used to get the right
    /// answer by accident and now gets a wrong one.
    ///
    /// Every rise counts, including one that only undoes a provider's downward
    /// correction: `[0, 40, 35, 40]` returns 45 where 40 was consumed. That is
    /// a known overstatement and it is not fixable from the readings — a
    /// decline does not say whether the quota was refilled or the earlier
    /// number was wrong, and neither its size nor where it lands separates the
    /// two. Measured across every series in a real store: persistent drops run
    /// 2 to 100 points and bounce-backs 2 to 22, fully overlapping; real resets
    /// land anywhere from 0% to 21%, with bounce-backs landing at 0% as well.
    ///
    /// The numerator has the twin of this, and it is left in place for the
    /// same reason. A declining interval contributes nothing here while every
    /// message inside it still counts above the line, so whatever was consumed
    /// between the last reading before a reset and the reset itself is divided
    /// by the rises around it. Excluding those messages is right when the
    /// decline was a reset and wrong twice over when it was a correction —
    /// numerator down, denominator already up — and the paragraph above is the
    /// measurement saying the two cannot be told apart. Bound, from the same
    /// store: 2 of 128 groups contain any decline at all, declining intervals
    /// hold 0.17% of all observed time, 4.08% in the worst single group and
    /// 0.92% in the merged group this whole change was written for. Time, not
    /// messages — the message share is not measured, and a gap denser than
    /// average carries proportionally more.
    ///
    /// It is kept because the bound is small and the alternative costs more.
    /// Over 121 groups in that store, this and the range agreed on 119 — every
    /// cycle whose readings only rise — and differed by at most 3.5% on the
    /// two that had been refilled, where the range prices the window at its
    /// widest excursion rather than at what it consumed. The residual also
    /// leans the safe way: overstating the denominator understates the ratio,
    /// so a quota reads as worth less rather than more.
    public static func consumed(_ readings: [Double]) -> Double {
        zip(readings, readings.dropFirst()).reduce(0.0) { $0 + max(0, $1.1 - $1.0) }
    }

    /// How many separately measured rises `consumed` summed: the number of
    /// times the readings started going up after not going up. Zero on
    /// readings that never rose.
    ///
    /// The provider reports whole percents, so each rise carries the same
    /// ±0.5 quantisation whether it spans 3 points or 90. Summing two rises
    /// sums their uncertainty as well, and a ratio built on `[0, 3, 0, 3]`
    /// is not a 6-point measurement at ±0.5 — it is two 3-point measurements
    /// at ±0.5 each, neither of which clears the bar on its own. Callers that
    /// quote an error or gate on a minimum delta scale both by this count, so
    /// a group that crossed a reset does not come out looking more precise
    /// than a group that did not.
    public static func risingRuns(_ readings: [Double]) -> Int {
        // A positive delta starts a run iff no earlier delta was positive, or
        // the most recent non-zero delta before it was negative. A zero delta
        // is transparent: a plateau inside a rise is still one measurement
        // span under the ±0.5-per-reading model, so it neither starts nor ends
        // a run. Counting declines instead — the first cut here — charged a
        // trailing decline as a run that contributed no rise.
        var runs = 0
        var lastNonZeroWasNegative = true
        for (before, after) in zip(readings, readings.dropFirst()) {
            if after > before {
                if lastNonZeroWasNegative { runs += 1 }
                lastNonZeroWasNegative = false
            } else if after < before {
                lastNonZeroWasNegative = true
            }
        }
        return runs
    }

    /// The messages a scoped window may count. One statement of the rule, so
    /// the three surfaces of a window cannot apply it differently.
    public static func inScope(_ messages: [WindowMessage], _ scope: String?) -> [WindowMessage] {
        guard let scope else { return messages }
        return messages.filter { ModelScope.covers(scope, modelId: $0.modelId) }
    }

    private static func spanTotals(
        cycles: [QuotaCycle], sorted: [WindowMessage], stamps: [Int64],
        subscription: String, confirmed: [UsageAttribution.Record]
    ) -> [(tokens: Int64, cost: Double)] {
        cycles.map { cycle in
            // Bounded by the SPAN, not by `[start, reset)`. The span is what
            // the denominator measures, and it is not always inside the cycle:
            // see `evidenceStartMs`. The slice is a superset — the `where`
            // below states the actual rule — so the bounds only have to be
            // safe, and `saturatingAdding` keeps the exclusive upper edge from
            // overflowing on a corrupt timestamp.
            let lo = lowerBound(stamps, cycle.firstSampleMs)
            let hi = lowerBound(stamps, cycle.lastSampleMs.saturatingAdding(1))
            var span = (tokens: Int64(0), cost: 0.0)
            for message in sorted[lo..<max(lo, hi)]
            where message.timestamp > cycle.firstSampleMs
                && message.timestamp <= cycle.lastSampleMs
            {
                guard case let .assigned(target) = UsageAttribution.resolve(
                    client: message.client, provider: message.providerId,
                    model: message.modelId, records: confirmed), target == subscription
                else { continue }
                // Through `WindowEquivalence.ratioTokens`, which is where the
                // basis is stated. The live-window path in `row` computes the
                // same ratio and reads the same function, so the two surfaces
                // cannot drift apart the way they did when each summed for
                // itself.
                span.tokens = span.tokens.saturatingAdding(
                    WindowEquivalence.ratioTokens(message))
                span.cost += message.cost
            }
            return span
        }
    }

    public static func lowerBound(_ values: [Int64], _ value: Int64) -> Int {
        var low = 0, high = values.count
        while low < high {
            let mid = (low + high) / 2
            if values[mid] < value { low = mid + 1 } else { high = mid }
        }
        return low
    }
}
