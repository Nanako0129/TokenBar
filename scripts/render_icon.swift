import AppKit

// Final icon "B": ear-peak cat bar + tail baseline. Dark & light themes,
// parametrically re-drawn at every iconset size (no downscaling blur).

let iris = NSColor(srgbRed: 0x7c/255.0, green: 0x6c/255.0, blue: 1.0, alpha: 1)
let irisBright = NSColor(srgbRed: 0x9b/255.0, green: 0x8c/255.0, blue: 1.0, alpha: 1)
let irisDeep = NSColor(srgbRed: 0x5b/255.0, green: 0x46/255.0, blue: 0xd6/255.0, alpha: 1)

func lerpColor(_ a: NSColor, _ b: NSColor, _ t: CGFloat) -> NSColor {
    NSColor(
        srgbRed: a.redComponent + (b.redComponent - a.redComponent) * t,
        green: a.greenComponent + (b.greenComponent - a.greenComponent) * t,
        blue: a.blueComponent + (b.blueComponent - a.blueComponent) * t, alpha: 1)
}

func drawIcon(dark: Bool, size: CGFloat) {
    let margin = size * 0.09
    let shape = NSRect(x: margin, y: margin, width: size - 2 * margin, height: size - 2 * margin)
    let bg = NSBezierPath(roundedRect: shape, xRadius: shape.width * 0.225, yRadius: shape.width * 0.225)
    if dark {
        NSGradient(
            starting: NSColor(srgbRed: 0.085, green: 0.08, blue: 0.115, alpha: 1),
            ending: NSColor(srgbRed: 0.13, green: 0.12, blue: 0.18, alpha: 1))!.draw(in: bg, angle: 90)
    } else {
        NSGradient(
            starting: NSColor(srgbRed: 0.93, green: 0.92, blue: 0.97, alpha: 1),
            ending: NSColor(srgbRed: 1.0, green: 1.0, blue: 1.0, alpha: 1))!.draw(in: bg, angle: 90)
    }
    NSGraphicsContext.current?.saveGraphicsState()
    bg.addClip()

    let w = shape.width
    let count = 4
    let barW = w * 0.135
    let gap = w * 0.06
    let totalW = barW * CGFloat(count) + gap * CGFloat(count - 1)
    let x0 = shape.minX + w * 0.135
    let baseY = shape.minY + shape.height * 0.30
    let heights: [CGFloat] = [0.16, 0.26, 0.36, 0.50].map { $0 * shape.height }
    let ear = shape.height * 0.07

    for i in 0..<count {
        let bar = NSRect(x: x0 + CGFloat(i) * (barW + gap), y: baseY, width: barW, height: heights[i])
        lerpColor(irisDeep, dark ? irisBright : iris, CGFloat(i) / 3).setFill()
        if i == count - 1 {
            let p = NSBezierPath()
            p.move(to: NSPoint(x: bar.minX, y: bar.minY))
            p.line(to: NSPoint(x: bar.maxX, y: bar.minY))
            p.line(to: NSPoint(x: bar.maxX, y: bar.maxY - ear * 0.1))
            p.line(to: NSPoint(x: bar.maxX - bar.width * 0.08, y: bar.maxY + ear))
            p.line(to: NSPoint(x: bar.midX + bar.width * 0.14, y: bar.maxY))
            p.line(to: NSPoint(x: bar.midX - bar.width * 0.14, y: bar.maxY))
            p.line(to: NSPoint(x: bar.minX + bar.width * 0.08, y: bar.maxY + ear))
            p.line(to: NSPoint(x: bar.minX, y: bar.maxY - ear * 0.1))
            p.close()
            p.fill()
        } else {
            NSBezierPath(roundedRect: bar, xRadius: barW / 2.8, yRadius: barW / 2.8).fill()
        }
    }

    let lineW = shape.height * 0.045
    let loopR = shape.width * 0.085
    let lineY = baseY - shape.height * 0.07
    let endX = x0 + totalW + shape.width * 0.01
    let tail = NSBezierPath()
    tail.move(to: NSPoint(x: x0 - shape.width * 0.01, y: lineY))
    tail.line(to: NSPoint(x: endX, y: lineY))
    tail.appendArc(
        withCenter: NSPoint(x: endX + loopR * 0.05, y: lineY + loopR),
        radius: loopR, startAngle: 270, endAngle: 160, clockwise: false)
    tail.lineWidth = lineW
    tail.lineCapStyle = .round
    tail.lineJoinStyle = .round
    (dark ? iris : irisDeep).setStroke()
    tail.stroke()
    NSGraphicsContext.current?.restoreGraphicsState()
}

func renderPNG(dark: Bool, pixels: Int, to path: String) {
    let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: pixels, pixelsHigh: pixels,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    let ctx = NSGraphicsContext(bitmapImageRep: rep)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = ctx
    drawIcon(dark: dark, size: CGFloat(pixels))
    NSGraphicsContext.restoreGraphicsState()
    try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: path))
}

let sizes: [(Int, String)] = [
    (16, "16x16"), (32, "16x16@2x"), (32, "32x32"), (64, "32x32@2x"),
    (128, "128x128"), (256, "128x128@2x"), (256, "256x256"), (512, "256x256@2x"),
    (512, "512x512"), (1024, "512x512@2x"),
]
for theme in ["dark", "light"] {
    let dir = "/tmp/Syrtis-\(theme).iconset"
    try? FileManager.default.removeItem(atPath: dir)
    try! FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
    for (px, name) in sizes {
        renderPNG(dark: theme == "dark", pixels: px, to: "\(dir)/icon_\(name).png")
    }
}
// preview sheet
let canvas = NSImage(size: NSSize(width: 560, height: 300))
canvas.lockFocus()
NSColor(srgbRed: 0.16, green: 0.16, blue: 0.19, alpha: 1).setFill()
NSRect(x: 0, y: 0, width: 560, height: 300).fill()
NSColor(srgbRed: 0.88, green: 0.88, blue: 0.9, alpha: 1).setFill()
NSRect(x: 280, y: 0, width: 280, height: 300).fill()
for (i, dark) in [true, false].enumerated() {
    let ox = CGFloat(30 + i * 280)
    for (size, dx, dy): (CGFloat, CGFloat, CGFloat) in [(200, 0, 70), (64, 30, 8), (32, 120, 24)] {
        NSGraphicsContext.current?.saveGraphicsState()
        let t = NSAffineTransform(); t.translateX(by: ox + dx, yBy: dy); t.concat()
        drawIcon(dark: dark, size: size)
        NSGraphicsContext.current?.restoreGraphicsState()
    }
}
canvas.unlockFocus()
let png = NSBitmapImageRep(data: canvas.tiffRepresentation!)!.representation(using: .png, properties: [:])!
try! png.write(to: URL(fileURLWithPath: "/tmp/appicon-final-preview.png"))
print("iconsets + preview written")
