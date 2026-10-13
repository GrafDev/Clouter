// Renders the Clouter app icon (1024 px): a glossy red ribbon "U" inside a glass sphere with rings on a charcoal tile,
// the same visual language as Lexter / Clarence. Usage: swift scripts/make-icon.swift out.png
import AppKit

let side: CGFloat = 1024
func color(_ hex: UInt32, _ alpha: CGFloat = 1) -> NSColor {
    NSColor(srgbRed: CGFloat((hex >> 16) & 255) / 255, green: CGFloat((hex >> 8) & 255) / 255, blue: CGFloat(hex & 255) / 255, alpha: alpha)
}
func gradient(_ stops: [(UInt32, CGFloat, CGFloat)]) -> NSGradient {
    NSGradient(colors: stops.map { color($0.0, $0.1) }, atLocations: stops.map { $0.2 }, colorSpace: .sRGB)!
}

/// The "U": a round-bottomed ribbon letter, slightly tilted like its siblings.
func letterPath() -> NSBezierPath {
    let path = NSBezierPath()
    path.move(to: NSPoint(x: 362, y: 740))
    path.line(to: NSPoint(x: 362, y: 460))
    path.appendArc(withCenter: NSPoint(x: 512, y: 460), radius: 150, startAngle: 180, endAngle: 360)
    path.line(to: NSPoint(x: 662, y: 740))
    path.line(to: NSPoint(x: 562, y: 740))
    path.line(to: NSPoint(x: 562, y: 460))
    path.appendArc(withCenter: NSPoint(x: 512, y: 460), radius: 50, startAngle: 0, endAngle: 180, clockwise: true)
    path.line(to: NSPoint(x: 462, y: 740))
    path.close()
    path.lineJoinStyle = .round
    var t = AffineTransform(translationByX: 512, byY: 512)
    t.rotate(byDegrees: -6)
    t.translate(x: -512, y: -512)
    path.transform(using: t)
    return path
}

let image = NSImage(size: NSSize(width: side, height: side), flipped: false) { _ in
    let ctx = NSGraphicsContext.current!.cgContext
    let tile = NSRect(x: 100, y: 100, width: 824, height: 824)
    let tilePath = NSBezierPath(roundedRect: tile, xRadius: 186, yRadius: 186)

    // drop shadow + tile
    ctx.saveGState()
    ctx.setShadow(offset: CGSize(width: 0, height: -18), blur: 40, color: NSColor.black.withAlphaComponent(0.45).cgColor)
    color(0x1B1D21).setFill(); tilePath.fill()
    ctx.restoreGState()
    gradient([(0x34383E, 1, 0), (0x1A1C20, 1, 0.55), (0x121417, 1, 1)]).draw(in: tilePath, angle: -90)
    // graphite bevel rim
    tilePath.addClip()
    color(0x8A9098, 0.55).setStroke(); tilePath.lineWidth = 6; tilePath.stroke()
    NSBezierPath(roundedRect: tile.insetBy(dx: 9, dy: 9), xRadius: 178, yRadius: 178).stroke()
    // soft emerald glow behind the letter
    gradient([(0xEF4444, 0.28, 0), (0xEF4444, 0, 1)]).draw(fromCenter: NSPoint(x: 512, y: 500), radius: 0, toCenter: NSPoint(x: 512, y: 500), radius: 420, options: [])

    // glass sphere behind the letter, with two crossing rings (Lexter-style)
    let c = NSPoint(x: 512, y: 512), R: CGFloat = 322
    let sphere = NSBezierPath(ovalIn: NSRect(x: c.x - R, y: c.y - R, width: 2 * R, height: 2 * R))
    gradient([(0x991B1B, 0.55, 0), (0x7F1D1D, 0.35, 0.6), (0x2A0A0C, 0.55, 1)]).draw(in: sphere, relativeCenterPosition: NSPoint(x: -0.3, y: 0.4))
    ctx.saveGState()
    sphere.addClip()
    gradient([(0xFFFFFF, 0.24, 0), (0xFFFFFF, 0, 1)]).draw(fromCenter: NSPoint(x: c.x - 120, y: c.y + 170), radius: 0, toCenter: NSPoint(x: c.x - 120, y: c.y + 170), radius: 340, options: [])
    ctx.restoreGState()
    gradient([(0xFCA5A5, 0.9, 0), (0xF43F5E, 0.5, 0.5), (0xFB7185, 0.85, 1)]).draw(in: {
        let o = NSBezierPath(ovalIn: NSRect(x: c.x - R, y: c.y - R, width: 2 * R, height: 2 * R))
        o.append(NSBezierPath(ovalIn: NSRect(x: c.x - R + 14, y: c.y - R + 14, width: 2 * R - 28, height: 2 * R - 28)).reversed)
        return o
    }(), angle: -60)
    for (rot, rx, ry) in [(-28.0, R + 6, 128.0), (32.0, R + 6, 96.0)] as [(CGFloat, CGFloat, CGFloat)] {
        let e = NSBezierPath(ovalIn: NSRect(x: c.x - rx, y: c.y - ry, width: 2 * rx, height: 2 * ry))
        var t = AffineTransform(translationByX: c.x, byY: c.y); t.rotate(byDegrees: rot); t.translate(x: -c.x, y: -c.y)
        e.transform(using: t); e.lineWidth = 7
        color(0xFECACA, 0.42).setStroke(); e.stroke()
    }

    // letter body
    let ribbon = letterPath()
    ctx.saveGState()
    ctx.setShadow(offset: CGSize(width: 0, height: -12), blur: 34, color: color(0xEF4444, 0.85).cgColor)
    color(0xB91C1C).setFill(); ribbon.fill()
    ctx.restoreGState()
    gradient([(0xFEE2E2, 1, 0), (0xF87171, 1, 0.32), (0xEF4444, 1, 0.62), (0x991B1B, 1, 1)]).draw(in: ribbon, angle: -70)

    // gloss: bright sweep on the upper-left of the ribbon, dark fold on the lower right
    ctx.saveGState()
    ribbon.addClip()
    gradient([(0xFFFFFF, 0.70, 0), (0xFFFFFF, 0.0, 0.55)]).draw(in: NSRect(x: 220, y: 480, width: 420, height: 400), angle: -55)
    gradient([(0x2A0A0C, 0.0, 0.45), (0x2A0A0C, 0.55, 1)]).draw(in: NSRect(x: 200, y: 220, width: 640, height: 600), angle: -68)
    ctx.restoreGState()
    color(0xFEE2E2, 0.75).setStroke(); ribbon.lineWidth = 5; ribbon.stroke()

    return true
}

guard CommandLine.arguments.count > 1,
      let tiff = image.tiffRepresentation, let rep = NSBitmapImageRep(data: tiff),
      let png = rep.representation(using: .png, properties: [:]) else { fputs("usage: make-icon.swift out.png\n", stderr); exit(2) }
try png.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))
print("wrote \(CommandLine.arguments[1])")
