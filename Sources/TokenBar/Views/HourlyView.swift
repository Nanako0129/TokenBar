import SwiftUI
import TokenBarCore

/// "Hourly" lens, port of HourlyView.tsx. Two modes behind a toggle, mirroring
/// tokscale: a chronological per-day-per-hour "Timeline" (default, newest
/// first) and a 24-hour-of-day "Profile" rhythm. Timeline windows its rows
/// (200 + "Show more") — LazyVStack alone still made WKWebView jank in the
/// Tauri app; native is faster but the windowing keeps parity and bounds the
/// view tree.
struct HourlyView: View {
    let report: HourlyReport?
    /// The active client slice. The report arrives already filtered to this
    /// slice at the FFI (per-client-accurate totals even for hours shared
    /// across clients), so this membership check is now a harmless pass-through
    /// for a loaded slice — kept only as the strict all-hidden guard (an empty
    /// slice shows nothing, consistent with the other lenses/DayBars).
    var clientIds: [String] = []

    private enum Mode: String {
        case timeline, profile
    }

    private static let timelineInitial = 200
    private static let timelineStep = 200

    @AppStorage("tokenbar.hourly.mode") private var modeRaw = Mode.timeline.rawValue
    @State private var visible = HourlyView.timelineInitial

    private var mode: Mode { Mode(rawValue: modeRaw) ?? .timeline }

    private struct HourBucket {
        let hour: Int
        var tokens: Int64 = 0
        var cost = 0.0
    }

    private func allowed(_ e: HourlyReportEntry, _ allow: Set<String>) -> Bool {
        e.clients.contains { allow.contains($0) }
    }

    /// Profile: fold every slot into a 24-hour-of-day rhythm.
    private var buckets: [HourBucket] {
        let allow = Set(clientIds)
        var out = (0..<24).map { HourBucket(hour: $0) }
        for e in report?.entries ?? [] where allowed(e, allow) {
            // "YYYY-MM-DD HH:00" → HH
            guard e.hour.count >= 13, let hh = Int(e.hour.dropFirst(11).prefix(2)),
                  (0...23).contains(hh)
            else { continue }
            out[hh].tokens = out[hh].tokens.saturatingAdding(e.total)
            out[hh].cost += e.cost
        }
        return out
    }

    /// Timeline: each slot on its own, newest first.
    private var timeline: [HourlyReportEntry] {
        let allow = Set(clientIds)
        return (report?.entries ?? [])
            .filter { allowed($0, allow) && ($0.total > 0 || $0.cost > 0) }
            .sorted { $0.hour > $1.hour }
    }

    /// Local "YYYY-MM-DD HH:00" for the current hour, to highlight the live slot.
    private var nowKey: String {
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.dateFormat = "yyyy-MM-dd HH:00"
        return formatter.string(from: Date())
    }

    var body: some View {
        let buckets = self.buckets
        let timeline = self.timeline
        let hasData = mode == .profile
            ? buckets.contains { $0.tokens > 0 || $0.cost > 0 }
            : !timeline.isEmpty

        DashCard(
            mode == .profile ? "Hourly rhythm" : "Hourly usage",
            subtitle: subtitle(buckets: buckets, timeline: timeline, hasData: hasData),
            trailing: { modeToggle }
        ) {
            if report == nil {
                Text("Loading…")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else if !hasData {
                Text("No usage in this range")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else if mode == .profile {
                let maxTokens = max(buckets.map(\.tokens).max() ?? 1, 1)
                VStack(spacing: 3) {
                    ForEach(buckets, id: \.hour) { b in
                        hourRow(
                            label: String(format: "%02d:00", b.hour),
                            tokens: b.tokens, cost: b.cost, maxTokens: maxTokens,
                            isCurrent: false)
                    }
                }
            } else {
                let shown = Array(timeline.prefix(visible))
                let maxTokens = max(timeline.map(\.total).max() ?? 1, 1)
                let now = nowKey
                LazyVStack(spacing: 3) {
                    ForEach(shown, id: \.hour) { e in
                        hourRow(
                            // "YYYY-MM-DD HH:00" → "MM-DD HH:00"
                            label: String(e.hour.dropFirst(5)),
                            tokens: e.total, cost: e.cost, maxTokens: maxTokens,
                            isCurrent: e.hour == now)
                    }
                }
                if timeline.count > visible {
                    Button(
                        "Show \(min(Self.timelineStep, timeline.count - visible)) more · \(visible) of \(timeline.count)"
                    ) {
                        visible = min(visible + Self.timelineStep, timeline.count)
                    }
                    .buttonStyle(.plain)
                    .font(.caption2.weight(.medium))
                    .foregroundStyle(.secondary)
                }
            }
        }
        .onChange(of: modeRaw) {
            // Reset the window when switching back to timeline so we don't
            // keep a huge list mounted.
            visible = Self.timelineInitial
        }
    }

    private func subtitle(buckets: [HourBucket], timeline: [HourlyReportEntry], hasData: Bool)
        -> String
    {
        guard hasData else { return "—" }
        switch mode {
        case .profile:
            let peak = buckets.max { $0.tokens < $1.tokens } ?? buckets[0]
            let cost = buckets.reduce(0) { $0 + $1.cost }
            return String(format: "peak %02d:00 · %@", peak.hour, Format.usd(cost))
        case .timeline:
            let cost = timeline.reduce(0) { $0 + $1.cost }
            return "\(timeline.count) hrs · \(Format.usd(cost))"
        }
    }

    private var modeToggle: some View {
        HStack(spacing: 2) {
            ForEach([Mode.timeline, Mode.profile], id: \.rawValue) { m in
                Button(m.rawValue.prefix(1).uppercased() + m.rawValue.dropFirst()) {
                    modeRaw = m.rawValue
                }
                .buttonStyle(.plain)
                .font(.caption2.weight(mode == m ? .semibold : .regular))
                .foregroundStyle(mode == m ? .primary : .secondary)
                .padding(.horizontal, 6)
                .padding(.vertical, 2)
                .background(
                    mode == m ? AnyShapeStyle(Color.primary.opacity(0.16)) : AnyShapeStyle(.clear),
                    in: RoundedRectangle(cornerRadius: 4))
            }
        }
        .padding(1)
        .background(Color.primary.opacity(0.07), in: RoundedRectangle(cornerRadius: 6))
    }

    private func hourRow(
        label: String, tokens: Int64, cost: Double, maxTokens: Int64, isCurrent: Bool
    ) -> some View {
        HStack(spacing: 8) {
            Text(label)
                .font(.caption2.monospacedDigit())
                .foregroundStyle(isCurrent ? .primary : .secondary)
                .frame(width: mode == .profile ? 38 : 74, alignment: .leading)
            GeometryReader { geo in
                ZStack(alignment: .leading) {
                    RoundedRectangle(cornerRadius: 2)
                        .fill(.quaternary.opacity(0.5))
                    RoundedRectangle(cornerRadius: 2)
                        .fill(isCurrent ? Color.green.opacity(0.8) : Color.accentColor.opacity(0.7))
                        .frame(width: geo.size.width * CGFloat(tokens) / CGFloat(maxTokens))
                }
            }
            .frame(height: 8)
            Text(tokens > 0 ? Format.compactTokens(tokens) : "")
                .font(.caption2.monospacedDigit())
                .foregroundStyle(.secondary)
                .frame(width: 44, alignment: .trailing)
            Text(cost > 0 ? Format.usd(cost) : "")
                .font(.caption2.monospacedDigit())
                .frame(width: 52, alignment: .trailing)
        }
    }
}
