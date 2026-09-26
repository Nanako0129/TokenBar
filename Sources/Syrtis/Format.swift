import Foundation
import TokenBarCore

/// Small display formatters shared by the tray title and the popover.
enum Format {
    /// Compact token count: 999 → "999", 12_345 → "12.3K", 1_234_567 → "1.2M".
    static func compactTokens(_ count: Int64) -> String {
        let value = Double(count)
        let scaled: Double
        let suffix: String
        // Tier boundaries sit at the ROUNDING boundary, not at the unit: the
        // `>= 100` arm below prints `%.0f`, so 999_500_000 scales to 999.5 and
        // carries to "1000M" — a mantissa that has left its own tier. Promoting
        // at 999_500 / 999_500_000 renders those bands as "1M" / "1B" instead.
        // Every value outside the two half-unit bands picks the same tier it
        // always did. (The top of the B tier has nowhere to promote to, so
        // 999.5B still prints "1000B".)
        switch value {
        case 999_500_000...:
            (scaled, suffix) = (value / 1_000_000_000, "B")
        case 999_500...:
            (scaled, suffix) = (value / 1_000_000, "M")
        case 1_000...:
            (scaled, suffix) = (value / 1_000, "K")
        default:
            return String(count)
        }
        var text = scaled >= 100 ? String(format: "%.0f", scaled) : String(format: "%.1f", scaled)
        if text.hasSuffix(".0") { text.removeLast(2) }
        return text + suffix
    }

    static func usd(_ amount: Double) -> String {
        String(format: "$%.2f", amount)
    }

    /// A "this many times over" multiple, for the implausible-cost warning.
    ///
    /// Whole numbers only. The value exists to say a cost is wrong by orders
    /// of magnitude, and a decimal place would imply a precision the estimate
    /// behind it does not have: the pricing lookup can land on a near-miss key
    /// or a row missing cache rates, either of which moves the ratio by
    /// single-digit multiples without changing what it is telling you.
    static func compactRatio(_ ratio: Double) -> String {
        String(format: "%.0f", ratio)
    }

    /// `usd`, except that a real amount too small to show is said to be small
    /// rather than rendered as nothing.
    ///
    /// `"$%.2f"` turns anything under half a cent into "$0.00", which reads as
    /// "we measured zero" when the truth is "we measured something below the
    /// resolution of this format". A single list-priced call at $0.003 is
    /// ordinary, and a line reading "2.1M · $0.00" contradicts itself.
    ///
    /// The sub-cent case ONLY. An exact zero still renders "$0.00", which is
    /// right for a total and wrong for a price nobody could compute — where a
    /// token count sits beside the amount, use `money(tokens:cost:)` instead.
    ///
    /// Every amount rendered beside its own token count goes through `money`;
    /// a model the pricing sources do not list yet (Opus 5.5 before LiteLLM
    /// and OpenRouter added it) would otherwise read as free. The remaining
    /// `usd` callers are headline totals with no token count to test against,
    /// plus `CrossCheckHarness`, which exists to check `usd` itself against
    /// the shared `format.json` fixtures and must keep calling it.
    static func usdOrBelowCent(_ amount: Double) -> String {
        // `amount == 0` catches -0.0 too, which `usd` alone renders "$-0.00".
        if amount == 0 { return usd(0) }
        return amount > 0 && amount < 0.005 ? "<$0.01" : usd(amount)
    }

    /// An amount shown beside a token count.
    ///
    /// `"$%.2f"` cannot distinguish "nothing" from "nothing we could price"
    /// from "less than half a cent". All three render "$0.00" and only the
    /// first is true, so a line reading "5.2M tokens · $0.00" states a price
    /// nobody has. Usage with no price at all gets a dash; priced usage below
    /// the format's resolution is said to be small; an exact zero beside no
    /// tokens is a real total and keeps its "$0.00".
    static func money(tokens: Int64, cost: Double) -> String {
        if cost <= 0, tokens > 0 { return "—" }
        return usdOrBelowCent(cost)
    }

    /// A token count shown beside an amount — the mirror of `money`, and the
    /// reason it is a function rather than a ternary at each site.
    ///
    /// A supported cost-only source reports a price with no token counts, so
    /// `compactTokens` renders "0" for usage that certainly happened and simply
    /// was not measured in tokens. The rule was first written inline beside one
    /// column of the quota history and not the other, and the expanded per-model
    /// rows went on printing "0 · $4.10" underneath a total that already said
    /// "—". Stating it once is what keeps the two columns of one row from
    /// disagreeing about whether a number exists.
    ///
    /// An exact zero beside no cost is a real total and keeps its "0".
    static func tokens(tokens: Int64, cost: Double) -> String {
        if tokens <= 0, cost > 0 { return "—" }
        return compactTokens(tokens)
    }

    /// Today's contribution-graph day key. tokscale-core buckets days in the
    /// local timezone as `%Y-%m-%d`, so we must match that exactly.
    /// `timeZone` is injectable so a test can pin one. It defaults to
    /// `.current`, which is what every caller wants and what made the day-key
    /// assertions pass on the author's machine and fail on a UTC runner: an
    /// instant expressed as "07:33 local" is only that in one zone.
    static func todayKey(now: Date = Date(), timeZone: TimeZone = .current) -> String {
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.timeZone = timeZone
        formatter.dateFormat = "yyyy-MM-dd"
        return formatter.string(from: now)
    }

    /// `todayKey` shifted back whole days, in the same `yyyy-MM-dd` shape the
    /// contributions are keyed and ordered by.
    ///
    /// Counted in calendar days through `Calendar`, not by subtracting
    /// `86_400 * n` seconds: a DST transition makes one of those days 23 or 25
    /// hours long, and the arithmetic version silently lands on the wrong date
    /// twice a year.
    static func dayKey(
        daysAgo: Int, now: Date = Date(), timeZone: TimeZone = .current
    ) -> String {
        var calendar = Calendar.current
        calendar.timeZone = timeZone
        let shifted = calendar.date(byAdding: .day, value: -daysAgo, to: now) ?? now
        return todayKey(now: shifted, timeZone: timeZone)
    }

    private static let monthsShort = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun",
        "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]

    /// "2026-06-10" → "Jun 10" ("6月10日" under zh-Hant).
    static func monthDay(_ iso: String) -> String {
        let parts = iso.split(separator: "-").compactMap { Int($0) }
        guard parts.count == 3, (1...12).contains(parts[1]) else { return iso }
        return "format.monthDay".localized(
            default: "%1$@ %2$lld", monthsShort[parts[1] - 1].localized, parts[2])
    }

    /// "2026-07" → "Jul 2026" ("2026年7月" under zh-Hant).
    static func monthYear(_ ym: String) -> String {
        let parts = ym.split(separator: "-").compactMap { Int($0) }
        guard parts.count == 2, (1...12).contains(parts[1]) else { return ym }
        return "format.monthYear".localized(
            default: "%1$@ %2$lld", monthsShort[parts[1] - 1].localized, parts[0])
    }

    /// "2026-06-10" → "06/10".
    static func mmdd(_ iso: String) -> String {
        let parts = iso.split(separator: "-")
        guard parts.count == 3 else { return iso }
        return "\(parts[1])/\(parts[2])"
    }

    /// Exact token count with thousands separators ("1,234,567").
    static func exactTokens(_ count: Int64) -> String {
        count.formatted(.number.grouping(.automatic).locale(Locale(identifier: "en_US")))
    }

    /// Compact "time ago" from a Unix-seconds timestamp: "just now", "5m ago",
    /// "3h ago", "2d ago". Used for the pricing-data freshness hint.
    static func relativeTime(_ epochSecs: UInt64, now: Date = Date()) -> String {
        let diff = max(0, Int(now.timeIntervalSince1970) - Int(epochSecs))
        if diff < 60 { return "just now".localized }
        if diff < 3600 { return "%lldm ago".localized(diff / 60) }
        if diff < 86400 { return "%lldh ago".localized(diff / 3600) }
        return "%lldd ago".localized(diff / 86400)
    }
}

extension Format {
    /// Coarse remaining-time, e.g. "54m", "3h 54m", "5d 14h". Answers "how
    /// long have I got" and nothing finer; a window countdown that ticks
    /// seconds would redraw the card for no information.
    /// Through the localized templates, not interpolated literals. The four
    /// keys already exist and are already translated — this formatter simply
    /// did not use them, so a Traditional Chinese build rendered "3h 5m 後重置"
    /// with the units in one language and the sentence around them in another.
    static func duration(ms: Int64) -> String {
        let (key, args) = durationTemplate(ms: ms)
        return args.count == 2
            ? key.localized(args[0], args[1])
            : key.localized(args[0])
    }

    /// Which template a span uses, and with what numbers.
    ///
    /// Split out so the CHOICE can be asserted on. The rendering cannot: under
    /// English every one of these templates is an identity, so a test that
    /// compares the output string passes whether the code looks the key up or
    /// interpolates it — which is how the interpolated version shipped and
    /// rendered "3h 5m 後重置" in Traditional Chinese, units in one language
    /// inside a sentence in another. Naming the key is the part a test can
    /// still see when the translation is not loaded.
    static func durationTemplate(ms: Int64) -> (key: String, args: [Int64]) {
        let mins = max(ms / 60_000, 0)
        if mins < 60 { return ("%lldm", [mins]) }
        let hours = mins / 60
        if hours < 24 {
            let rest = mins % 60
            return rest > 0 ? ("%lldh %lldm", [hours, rest]) : ("%lldh", [hours])
        }
        let rest = hours % 24
        return rest > 0
            ? ("%lldd %lldh", [hours / 24, rest])
            : ("%lldd", [hours / 24])
    }

    /// Wall-clock span of a hovered interval. POSIX locale so the string is
    /// stable under test, but the user's own time zone and calendar.
    /// One instant, always the same width.
    ///
    /// `clockRange` drops the date when both ends share a day, which reads well
    /// in a tooltip about one interval and badly in a list of them: a five-hour
    /// window crosses midnight often enough that half the rows carried dates
    /// and half did not, and the two-date form ran to 23 characters and
    /// truncated. Every cycle of a window is the same length, so the end is
    /// derivable and the start alone identifies the row.
    static func windowStamp(ms: Int64) -> String {
        let stamp = DateFormatter()
        stamp.locale = Locale(identifier: "en_US_POSIX")
        stamp.dateFormat = "MM-dd HH:mm"
        return stamp.string(from: Date(timeIntervalSince1970: Double(ms) / 1000))
    }

    static func clockRange(fromMs: Int64, toMs: Int64) -> String {
        let from = Date(timeIntervalSince1970: Double(fromMs) / 1000)
        let to = Date(timeIntervalSince1970: Double(toMs) / 1000)
        let time = DateFormatter()
        time.locale = Locale(identifier: "en_US_POSIX")
        time.dateFormat = "HH:mm"
        let sameDay = Calendar.current.isDate(from, inSameDayAs: to)
        if sameDay { return "\(time.string(from: from)) – \(time.string(from: to))" }
        let stamp = DateFormatter()
        stamp.locale = Locale(identifier: "en_US_POSIX")
        stamp.dateFormat = "MM-dd HH:mm"
        return "\(stamp.string(from: from)) – \(stamp.string(from: to))"
    }
}
