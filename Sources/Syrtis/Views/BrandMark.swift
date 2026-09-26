import SwiftUI

/// The Syrtis mark (ascending bars, cat ears on the tallest, tail-loop
/// baseline) as a flat monochrome glyph — the app-icon geometry from
/// scripts/render_icon.swift without the squircle, for in-app use like the
/// popover header. Tinted by the current foreground style.
struct BrandMark: View {
    var body: some View {
        Canvas { ctx, size in
            let s = min(size.width, size.height)
            // Icon-space coordinates (y up); flip once here.
            ctx.translateBy(x: (size.width - s) / 2, y: (size.height + s) / 2)
            ctx.scaleBy(x: s, y: -s)

            let shading = GraphicsContext.Shading.style(.secondary)
            let barW = 0.16
            let gap = 0.065
            let x0 = 0.03
            let baseY = 0.22
            let heights: [CGFloat] = [0.20, 0.34, 0.48, 0.66]
            let ear = 0.10

            for i in 0..<4 {
                let x = x0 + Double(i) * (barW + gap)
                let bar = CGRect(x: x, y: baseY, width: barW, height: heights[i])
                if i == 3 {
                    var p = Path()
                    p.move(to: CGPoint(x: bar.minX, y: bar.minY))
                    p.addLine(to: CGPoint(x: bar.maxX, y: bar.minY))
                    p.addLine(to: CGPoint(x: bar.maxX, y: bar.maxY - ear * 0.1))
                    p.addLine(to: CGPoint(x: bar.maxX - bar.width * 0.08, y: bar.maxY + ear))
                    p.addLine(to: CGPoint(x: bar.midX + bar.width * 0.14, y: bar.maxY))
                    p.addLine(to: CGPoint(x: bar.midX - bar.width * 0.14, y: bar.maxY))
                    p.addLine(to: CGPoint(x: bar.minX + bar.width * 0.08, y: bar.maxY + ear))
                    p.addLine(to: CGPoint(x: bar.minX, y: bar.maxY - ear * 0.1))
                    p.closeSubpath()
                    ctx.fill(p, with: shading)
                } else {
                    ctx.fill(
                        Path(roundedRect: bar, cornerRadius: barW / 2.8),
                        with: shading)
                }
            }

            // tail baseline with the loop at the right end
            let lineW = 0.06
            let loopR = 0.10
            let lineY = baseY - 0.10
            let endX = x0 + barW * 4 + gap * 3 + 0.01
            var tail = Path()
            tail.move(to: CGPoint(x: x0 - 0.01, y: lineY))
            tail.addLine(to: CGPoint(x: endX, y: lineY))
            tail.addArc(
                center: CGPoint(x: endX + loopR * 0.05, y: lineY + loopR),
                radius: loopR,
                startAngle: .degrees(270), endAngle: .degrees(160),
                clockwise: false)
            ctx.stroke(
                tail, with: shading,
                style: StrokeStyle(lineWidth: lineW, lineCap: .round, lineJoin: .round))
        }
    }
}
