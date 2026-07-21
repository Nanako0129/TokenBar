import SwiftUI
import TokenBarCore

private struct PopoverScrollViewportKey: EnvironmentKey {
    static let defaultValue: CGRect? = nil
}

extension EnvironmentValues {
    var popoverScrollViewport: CGRect? {
        get { self[PopoverScrollViewportKey.self] }
        set { self[PopoverScrollViewportKey.self] = newValue }
    }
}

enum PopoverTooltipPlacement {
    static let edgeInset: CGFloat = 4
    static let cursorGap: CGFloat = 12
    /// Local Y ratio inside the source container. Above this, prefer placing
    /// the tooltip above the cursor (dodge); below it, prefer below. Matches
    /// the pre-viewport chart dodge (`chartHeight * 0.45`) and Models feel.
    static let preferAboveRatio: CGFloat = 0.45

    /// Position a tooltip in its local overlay while keeping it inside the
    /// visible ScrollView viewport. Prefers the side that dodges the cursor
    /// (region-based), then falls back / clamps so it never sits under the
    /// footer. Container and viewport share global coordinates; the returned
    /// offset is local to the container.
    static func offset(
        anchor: CGPoint,
        tooltipSize: CGSize,
        containerFrame: CGRect,
        viewport: CGRect?
    ) -> CGSize? {
        guard tooltipSize.width > 0, tooltipSize.height > 0 else { return nil }

        let anchorGlobal = CGPoint(
            x: containerFrame.minX + anchor.x,
            y: containerFrame.minY + anchor.y)

        // Freshness: continuous hover only fires over the *visible* scroll
        // area, so a live viewport must contain the anchor (tiny slack for
        // float edges). Mere container intersection is not enough — a
        // pre-resize short viewport can still overlap the card while the
        // newly exposed hover point sits below it, inventing a fake dodge.
        let viewport: CGRect = {
            guard let candidate = viewport, !candidate.isEmpty else { return containerFrame }
            let slack: CGFloat = 2
            if candidate.insetBy(dx: -slack, dy: -slack).contains(anchorGlobal) {
                return candidate
            }
            return containerFrame
        }()
        guard !viewport.isEmpty else { return nil }
        let visible = viewport.insetBy(dx: edgeInset, dy: edgeInset)
        guard visible.width > 0, visible.height > 0 else { return nil }
        let horizontalMin = max(containerFrame.minX, visible.minX)
        let horizontalMax = min(containerFrame.maxX, visible.maxX)
        guard horizontalMax > horizontalMin else { return nil }

        let maxX = horizontalMax - tooltipSize.width
        let originX = maxX >= horizontalMin
            ? min(max(anchorGlobal.x - tooltipSize.width / 2, horizontalMin), maxX)
            : horizontalMin

        let minY = visible.minY
        let maxY = visible.maxY - tooltipSize.height
        let belowY = anchorGlobal.y + cursorGap
        let aboveY = anchorGlobal.y - tooltipSize.height - cursorGap
        // Region dodge uses the source container (chart / rows), not the
        // full scroll viewport — "below if the viewport still has room"
        // always wins on a tall popover and feels like sticky follow.
        let preferBelow = containerFrame.height > 0
            ? anchor.y < containerFrame.height * preferAboveRatio
            : true
        let originY: CGFloat
        if tooltipSize.height >= visible.height {
            originY = minY
        } else if preferBelow {
            if belowY <= maxY {
                originY = max(belowY, minY)
            } else if aboveY >= minY {
                originY = min(aboveY, maxY)
            } else {
                originY = clampedPreferredY(
                    belowY: belowY, aboveY: aboveY,
                    anchorGlobalY: anchorGlobal.y,
                    minY: minY, maxY: maxY, visible: visible)
            }
        } else if aboveY >= minY {
            originY = min(aboveY, maxY)
        } else if belowY <= maxY {
            originY = max(belowY, minY)
        } else {
            originY = clampedPreferredY(
                belowY: belowY, aboveY: aboveY,
                anchorGlobalY: anchorGlobal.y,
                minY: minY, maxY: maxY, visible: visible)
        }

        return CGSize(
            width: originX - containerFrame.minX,
            height: originY - containerFrame.minY)
    }

    private static func clampedPreferredY(
        belowY: CGFloat,
        aboveY: CGFloat,
        anchorGlobalY: CGFloat,
        minY: CGFloat,
        maxY: CGFloat,
        visible: CGRect
    ) -> CGFloat {
        let belowSpace = visible.maxY - anchorGlobalY
        let aboveSpace = anchorGlobalY - visible.minY
        let preferred = belowSpace >= aboveSpace ? belowY : aboveY
        return min(max(preferred, minY), maxY)
    }
}

/// Liquid Glass card surface on macOS 26+, material fallback below — the
/// glass lives on the cards themselves (the Control Center look), not on a
/// backdrop hidden behind opaque fills.
struct GlassCardBackground: ViewModifier {
    var cornerRadius: CGFloat = 10

    @Environment(\.colorScheme) private var colorScheme

    func body(content: Content) -> some View {
        if #available(macOS 26.0, *) {
            // .clear glass lets the wallpaper breathe through the cards
            // themselves (.regular reads as a dense dark slab when cards
            // fill the whole scroll area). The tint follows the appearance:
            // smoked glass in dark mode (a plain white tint washed the dark
            // theme out), a faint white lift in light mode — the Control
            // Center pebble look in both.
            // Smoke layer drawn over the glass (Glass.tint barely moves the
            // needle on .clear): dark mode gets a deterministic dark scrim,
            // light mode a faint white lift.
            content
                .background(
                    colorScheme == .dark
                        ? Color.black.opacity(0.32)
                        : Color.white.opacity(0.10),
                    in: RoundedRectangle(cornerRadius: cornerRadius))
                .glassEffect(.clear, in: .rect(cornerRadius: cornerRadius))
        } else {
            content
                .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: cornerRadius))
        }
    }
}

extension View {
    func glassCard(cornerRadius: CGFloat = 10) -> some View {
        modifier(GlassCardBackground(cornerRadius: cornerRadius))
    }
}

/// Shared dashboard-card chrome: rounded glass panel, matching the Tauri
/// dashboard's card stack.
struct DashCard<Content: View>: View {
    let title: String
    var subtitle: String?
    @ViewBuilder var trailing: () -> AnyView?
    @ViewBuilder var content: () -> Content

    init(
        _ title: String, subtitle: String? = nil,
        @ViewBuilder content: @escaping () -> Content
    ) {
        self.title = title
        self.subtitle = subtitle
        self.trailing = { nil }
        self.content = content
    }

    init<T: View>(
        _ title: String, subtitle: String? = nil,
        @ViewBuilder trailing: @escaping () -> T,
        @ViewBuilder content: @escaping () -> Content
    ) {
        self.title = title
        self.subtitle = subtitle
        self.trailing = { AnyView(trailing()) }
        self.content = content
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .firstTextBaseline) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(title)
                        .font(.system(size: 13, weight: .semibold))
                    if let subtitle {
                        Text(subtitle)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
                Spacer()
                trailing()
            }
            content()
        }
        .padding(12)
        .glassCard()
    }
}

/// The three-cell totals row (Total / Tokens / Best day), ported from
/// TokenUsageCard.tsx in its bare form.
struct TokenUsageRow: View {
    let stats: UsageStats

    var body: some View {
        HStack(spacing: 8) {
            cell(
                Format.usd(stats.totalCost), "Total",
                "\(Format.mmdd(stats.dateRange.start)) → \(Format.mmdd(stats.dateRange.end))")
            cell(
                Format.compactTokens(stats.totalTokens), "Tokens",
                "\(stats.activeDays) active days")
            cell(
                stats.bestDay.map { Format.usd($0.cost) } ?? "$0.00", "Best day",
                stats.bestDay.map { Format.monthDay($0.date) } ?? "—")
        }
    }

    private func cell(_ num: String, _ label: String, _ sub: String) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(num)
                .font(.system(size: 15, weight: .semibold).monospacedDigit())
            Text(label)
                .font(.caption)
                .foregroundStyle(.secondary)
            Text(sub)
                .font(.caption2)
                .foregroundStyle(.tertiary)
                .lineLimit(1)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

/// Streak summary card, ported from StreaksCard.tsx.
struct StreaksCard: View {
    let streaks: Streaks

    var body: some View {
        DashCard("Streaks") {
            HStack(spacing: 8) {
                item(streaks.longest, "Longest")
                item(streaks.current, "Current")
            }
        }
    }

    private func item(_ days: Int, _ label: String) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            (Text("\(days)").font(.system(size: 17, weight: .semibold).monospacedDigit())
                + Text(" days").font(.caption).foregroundStyle(.secondary))
            Text(label)
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}
