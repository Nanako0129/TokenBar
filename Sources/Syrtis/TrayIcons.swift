import AppKit

/// Shared quota thresholds for automatic gauges and custom menu-bar text.
enum QuotaColorLevel: String, CaseIterable, Identifiable {
    case normal, warning, critical

    var id: String { rawValue }

    init(remaining: Double) {
        if remaining <= 10 { self = .critical }
        else if remaining <= 25 { self = .warning }
        else { self = .normal }
    }
}

/// Quota-gauge icon styles for the status item (the cat/parrot frame sets
/// live in TrayAnimator's resources; these are drawn programmatically).
enum QuotaIconStyle: String, CaseIterable {
    case bars, ring, popsicle

    var label: String {
        switch self {
        case .bars: return "Signal bars"
        case .ring: return "Ring gauge"
        case .popsicle: return "Melting popsicle"
        }
    }
}

/// When the quota icons pick up the gauge color. Mirrors the battery icon's
/// behavior in `warningOnly` (color only when it matters).
enum IconColoring: String, CaseIterable {
    case warningOnly = "warning"
    case always
    case never

    static let storageKey = "tokenbar.icon.coloring"

    var label: String {
        switch self {
        case .warningOnly: return "Color on warning only"
        case .always: return "Always colored"
        case .never: return "Never colored"
        }
    }
}

/// Programmatic menu-bar gauge icons. All drawing is on a 16x16 design grid
/// via resolution-independent drawing handlers (crisp on retina).
enum TrayIcons {
    /// limits-card gauge palette: green / amber under 25% left / red under 10%.
    static func gaugeColor(remaining: Double) -> NSColor {
        switch QuotaColorLevel(remaining: remaining) {
        case .critical: NSColor(srgbRed: 0.94, green: 0.27, blue: 0.27, alpha: 1)
        case .warning: NSColor(srgbRed: 0.96, green: 0.62, blue: 0.04, alpha: 1)
        case .normal: NSColor(srgbRed: 0.13, green: 0.77, blue: 0.37, alpha: 1)
        }
    }

    /// The fill ink for a gauge icon under the chosen coloring policy.
    static func ink(remaining: Double?, dark: Bool, coloring: IconColoring) -> NSColor {
        let mono: NSColor = dark ? .white : .black
        guard let remaining else { return mono }
        switch coloring {
        case .never: return mono
        case .always: return gaugeColor(remaining: remaining)
        case .warningOnly: return QuotaColorLevel(remaining: remaining) != .normal ? gaugeColor(remaining: remaining) : mono
        }
    }

    static func image(
        style: QuotaIconStyle, remaining: Double?, dark: Bool, coloring: IconColoring
    ) -> NSImage {
        let mono: NSColor = dark ? .white : .black
        let fill = ink(remaining: remaining, dark: dark, coloring: coloring)
        let level = remaining ?? 100
        let image = NSImage(size: NSSize(width: 16, height: 16), flipped: false) { _ in
            switch style {
            case .bars: drawBars(remaining: level, mono: mono, fill: fill)
            case .ring: drawRing(remaining: level, mono: mono, fill: fill)
            case .popsicle: drawPopsicle(remaining: level, mono: mono, fill: fill)
            }
            return true
        }
        image.isTemplate = false
        return image
    }

    // MARK: - Shared 16-grid helpers

    private static func rect(_ x: Double, _ y: Double, _ w: Double, _ h: Double) -> NSRect {
        NSRect(x: x, y: y, width: w, height: h)
    }

    private static func point(_ x: Double, _ y: Double) -> NSPoint { NSPoint(x: x, y: y) }

    // MARK: - Staircase bars with a continuous waterline

    /// Four ascending bars as the outline; the remaining percent sweeps a
    /// continuous fill from the left (volume-HUD metaphor).
    private static func drawBars(remaining: Double, mono: NSColor, fill: NSColor) {
        let heights: [Double] = [5, 8, 11, 14]
        let fillX = 16.0 * min(100, max(0, remaining)) / 100
        for (i, h) in heights.enumerated() {
            let x = Double(i) * 4.0
            let bar = rect(x, 1, 3, h)
            let path = NSBezierPath(roundedRect: bar, xRadius: 1.2, yRadius: 1.2)
            mono.withAlphaComponent(0.3).setFill()
            path.fill()
            let visible = max(0, min(3, fillX - x))
            guard visible > 0 else { continue }
            NSGraphicsContext.current?.saveGraphicsState()
            NSBezierPath(rect: rect(x, 1, visible, h)).addClip()
            fill.setFill()
            path.fill()
            NSGraphicsContext.current?.restoreGraphicsState()
        }
    }

    // MARK: - Ring gauge

    private static func drawRing(remaining: Double, mono: NSColor, fill: NSColor) {
        let center = point(8, 8)
        let radius = 5.6
        let track = NSBezierPath()
        track.appendArc(withCenter: center, radius: radius, startAngle: 0, endAngle: 360)
        track.lineWidth = 2.6
        mono.withAlphaComponent(0.28).setStroke()
        track.stroke()
        let sweep = 360.0 * min(100, max(0, remaining)) / 100
        guard sweep > 1 else { return }
        let arc = NSBezierPath()
        arc.appendArc(
            withCenter: center, radius: radius,
            startAngle: 90, endAngle: 90 - sweep, clockwise: true)
        arc.lineWidth = 2.6
        arc.lineCapStyle = .round
        fill.setStroke()
        arc.stroke()
    }

    // MARK: - Melting popsicle (v7: square shoulders, grooves, hugging dome)

    /// The remaining mass shrinks concentrically toward the stick collar; the
    /// stick tip pokes out as it melts, drips fall and a puddle grows. Two
    /// classic face grooves melt away first. Depth via opacity layers only.
    private static func drawPopsicle(remaining: Double, mono: NSColor, fill ice: NSColor) {
        let r = min(100, max(0, remaining)) / 100
        let melt = 1 - r

        // stick (same ink, lighter layer)
        mono.withAlphaComponent(0.55).setFill()
        NSBezierPath(roundedRect: rect(7.25, 1.5, 1.5, 10.5), xRadius: 0.75, yRadius: 0.75).fill()

        // runoff puddle
        if melt > 0.3 {
            let pw = 3 + 9 * (melt - 0.3) / 0.7
            ice.withAlphaComponent(0.35).setFill()
            NSBezierPath(ovalIn: rect(8 - pw / 2, 0.5, pw, 1.4)).fill()
        }

        guard remaining > 3 else { return }
        let topY = 5.0 + 10.0 * r
        let w = 2.6 + 5.4 * r
        let halfW = w / 2
        let capR = min(1.8, (topY - 5.0) * 0.32)
        let dip = 0.5 + 0.9 * melt
        let shoulder = 0.12 * melt

        let body = NSBezierPath()
        body.move(to: point(8 - halfW, 5.6))
        body.curve(
            to: point(8 + halfW, 5.6),
            controlPoint1: point(8 - halfW * 0.5, 5.6 - dip),
            controlPoint2: point(8 + halfW * 0.5, 5.6 - dip))
        body.curve(
            to: point(8 + halfW - shoulder - capR * 0.3, topY - capR * 0.2),
            controlPoint1: point(8 + halfW + 0.15, 5.6 + (topY - 5.6) * 0.5),
            controlPoint2: point(8 + halfW - shoulder + 0.1, topY - capR))
        body.curve(
            to: point(8 - halfW + shoulder + capR * 0.3, topY - capR * 0.2),
            controlPoint1: point(8 + halfW * 0.45, topY + capR * 0.3 - 0.25 * melt),
            controlPoint2: point(8 - halfW * 0.45, topY + capR * 0.3 - 0.25 * melt))
        body.curve(
            to: point(8 - halfW, 5.6),
            controlPoint1: point(8 - halfW + shoulder - 0.1, topY - capR),
            controlPoint2: point(8 - halfW - 0.15, 5.6 + (topY - 5.6) * 0.5))
        body.close()
        ice.setFill()
        body.fill()

        // two classic face grooves, punched out; gone below ~45%
        if remaining > 45 {
            let presence = min(1, (r - 0.45) / 0.4)
            let inset = w / 3.2
            let gBottom = 6.6
            let gTop = topY - capR - 0.9
            if gTop > gBottom + 0.8 {
                NSGraphicsContext.current?.compositingOperation = .destinationOut
                for gx in [8 - inset, 8 + inset] {
                    NSColor.black.withAlphaComponent(0.85 * presence).setFill()
                    NSBezierPath(
                        roundedRect: rect(gx - 0.33, gBottom, 0.66, gTop - gBottom),
                        xRadius: 0.33, yRadius: 0.33
                    ).fill()
                }
                NSGraphicsContext.current?.compositingOperation = .sourceOver
            }
        }

        // hanging drip + falling droplet
        if melt > 0.15 {
            ice.withAlphaComponent(0.7).setFill()
            NSBezierPath(ovalIn: rect(8.5, 5.6 - dip - 1.5, 1.0, 1.5)).fill()
        }
        if melt > 0.5 {
            ice.withAlphaComponent(0.45).setFill()
            NSBezierPath(ovalIn: rect(6.7, 2.8, 0.85, 1.1)).fill()
        }
    }
}
