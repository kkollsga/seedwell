// Draws the app icon into an .iconset folder; build.sh turns it into AppIcon.icns with `iconutil`.
//
//     swiftc -O app/Resources/make_icon.swift -o /tmp/make_icon && /tmp/make_icon AppIcon.iconset [preview.png]
//
// The concept is the menu bar icon's: a down arrow (green) and an up arrow (coral) on a deep graphite
// squircle, following Apple's icon grid (an 824 pt squircle centred in a 1024 pt canvas, a soft shadow
// below it, a quiet light from the top). Every size is drawn from scratch, not scaled, and the strokes
// get relatively heavier below 64 px so that the two arrows still read at 16 px.
import AppKit
import CoreGraphics
import ImageIO

let out = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "AppIcon.iconset"
try? FileManager.default.createDirectory(atPath: out, withIntermediateDirectories: true)

let space = CGColorSpace(name: CGColorSpace.sRGB)!

func rgb(_ hex: UInt32, _ a: CGFloat = 1) -> CGColor {
    CGColor(colorSpace: space, components: [CGFloat((hex >> 16) & 255) / 255, CGFloat((hex >> 8) & 255) / 255, CGFloat(hex & 255) / 255, a])!
}

func gradient(_ stops: [(CGFloat, CGColor)]) -> CGGradient {
    CGGradient(colorsSpace: space, colors: stops.map(\.1) as CFArray, locations: stops.map(\.0))!
}

/// Apple's icon shape is a superellipse, not a rounded rectangle with circular corners.
func squircle(_ r: CGRect, n: CGFloat = 4.6) -> CGPath {
    let p = CGMutablePath()
    let steps = 360
    for i in 0...steps {
        let t = CGFloat(i) / CGFloat(steps) * 2 * .pi
        let c = cos(t), s = sin(t)
        let x = r.midX + r.width / 2 * (c < 0 ? -1 : 1) * pow(abs(c), 2 / n)
        let y = r.midY + r.height / 2 * (s < 0 ? -1 : 1) * pow(abs(s), 2 / n)
        i == 0 ? p.move(to: CGPoint(x: x, y: y)) : p.addLine(to: CGPoint(x: x, y: y))
    }
    p.closeSubpath()
    return p
}

func render(_ px: Int) -> Data? {
    guard let ctx = CGContext(data: nil, width: px, height: px, bitsPerComponent: 8, bytesPerRow: 0, space: space,
                              bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return nil }
    let s = CGFloat(px)
    ctx.interpolationQuality = .high
    ctx.setAllowsAntialiasing(true)
    let small = px <= 64
    // 824/1024 body; a little larger at 16 and 32 px, where the margin would waste pixels.
    let inset = s * (px <= 32 ? 0.055 : 0.0977)
    let body = CGRect(x: inset, y: inset, width: s - 2 * inset, height: s - 2 * inset)
    let shape = squircle(body)

    // Shadow under the body (kept off the very small sizes).
    if !small {
        ctx.saveGState()
        ctx.setShadow(offset: CGSize(width: 0, height: -s * 0.012), blur: s * 0.03, color: rgb(0x000000, 0.45))
        ctx.addPath(shape)
        ctx.setFillColor(rgb(0x000000))
        ctx.fillPath()
        ctx.restoreGState()
    }

    ctx.saveGState()
    ctx.addPath(shape)
    ctx.clip()
    // Graphite with a blue-black bottom.
    ctx.drawLinearGradient(gradient([(0, rgb(0x0B0E1A)), (0.55, rgb(0x171B2C)), (1, rgb(0x2A3042))]),
                           start: CGPoint(x: 0, y: body.minY), end: CGPoint(x: 0, y: body.maxY), options: [])
    // Soft light from the top.
    ctx.drawRadialGradient(gradient([(0, rgb(0xFFFFFF, small ? 0.16 : 0.20)), (1, rgb(0xFFFFFF, 0))]),
                           startCenter: CGPoint(x: body.midX, y: body.maxY + s * 0.06), startRadius: 0,
                           endCenter: CGPoint(x: body.midX, y: body.maxY + s * 0.06), endRadius: body.height * 0.7, options: [])

    // The arrows.
    let w = body.width
    let stroke = w * (small ? 0.125 : px <= 128 ? 0.108 : 0.098)
    let reach = w * 0.235          // half the shaft length
    let wing = w * 0.135           // chevron half width
    let dx = w * 0.158
    let cy = body.midY
    func arrow(x: CGFloat, up: Bool, top: CGColor, bottom: CGColor, glow: CGColor) {
        let tip = up ? cy + reach : cy - reach
        let tail = up ? cy - reach : cy + reach
        let back: CGFloat = up ? -wing : wing
        let path = CGMutablePath()
        path.move(to: CGPoint(x: x, y: tail))
        path.addLine(to: CGPoint(x: x, y: tip))
        path.move(to: CGPoint(x: x - wing, y: tip + back))
        path.addLine(to: CGPoint(x: x, y: tip))
        path.addLine(to: CGPoint(x: x + wing, y: tip + back))
        let stroked = path.copy(strokingWithWidth: stroke, lineCap: .round, lineJoin: .round, miterLimit: 1)
        if !small {
            ctx.saveGState()
            ctx.setShadow(offset: .zero, blur: s * 0.035, color: glow)
            ctx.addPath(stroked)
            ctx.setFillColor(bottom)
            ctx.fillPath()
            ctx.restoreGState()
        }
        ctx.saveGState()
        ctx.addPath(stroked)
        ctx.clip()
        ctx.drawLinearGradient(gradient([(0, bottom), (1, top)]),
                               start: CGPoint(x: 0, y: cy - reach - stroke), end: CGPoint(x: 0, y: cy + reach + stroke), options: [])
        ctx.restoreGState()
    }
    // Green down arrow on the left, coral up arrow on the right, like the menu bar icon.
    arrow(x: body.midX - dx, up: false, top: rgb(0x7CF09A), bottom: rgb(0x23C25A), glow: rgb(0x30D158, 0.55))
    arrow(x: body.midX + dx, up: true, top: rgb(0xFF8A78), bottom: rgb(0xF2463E), glow: rgb(0xFF5A4D, 0.55))
    ctx.restoreGState()

    // A hairline of light along the top edge.
    if !small {
        ctx.saveGState()
        ctx.addPath(shape)
        ctx.clip()
        ctx.addPath(shape)
        ctx.setLineWidth(s * 0.004)
        ctx.replacePathWithStrokedPath()
        ctx.clip()
        ctx.drawLinearGradient(gradient([(0, rgb(0xFFFFFF, 0)), (0.6, rgb(0xFFFFFF, 0.04)), (1, rgb(0xFFFFFF, 0.38))]),
                               start: CGPoint(x: 0, y: body.minY), end: CGPoint(x: 0, y: body.maxY), options: [])
        ctx.restoreGState()
    }
    guard let image = ctx.makeImage() else { return nil }
    return NSBitmapImageRep(cgImage: image).representation(using: .png, properties: [:])
}

for (name, px) in [("16", 16), ("16@2x", 32), ("32", 32), ("32@2x", 64), ("128", 128), ("128@2x", 256),
                   ("256", 256), ("256@2x", 512), ("512", 512), ("512@2x", 1024)] {
    let base = name.replacingOccurrences(of: "@2x", with: "")
    let file = "\(out)/icon_\(base)x\(base)\(name.contains("@2x") ? "@2x" : "").png"
    try? render(px)?.write(to: URL(fileURLWithPath: file))
}
// Optional second argument: a 1024 px preview next to the iconset (for the QA shots).
if CommandLine.arguments.count > 2 { try? render(1024)?.write(to: URL(fileURLWithPath: CommandLine.arguments[2])) }
// Optional third argument: a contact sheet of the real sizes (16 to 256 px, @1x pixels) on dark and light, to judge
// the small ones: `make_icon AppIcon.iconset preview.png sizes.png`.
if CommandLine.arguments.count > 3 {
    let sizes = [16, 32, 64, 128, 256]
    let pad = 24, w = sizes.reduce(pad) { $0 + $1 + pad }, h = 256 + 2 * pad
    for (name, bg) in [("dark", rgb(0x1E1E1E)), ("light", rgb(0xECECEC))] {
        guard let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: 0, space: space,
                                  bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { continue }
        ctx.setFillColor(bg); ctx.fill(CGRect(x: 0, y: 0, width: w, height: h))
        var x = pad
        for px in sizes {
            if let data = render(px), let src = CGImageSourceCreateWithData(data as CFData, nil), let img = CGImageSourceCreateImageAtIndex(src, 0, nil) {
                ctx.draw(img, in: CGRect(x: x, y: pad, width: px, height: px))
            }
            x += px + pad
        }
        if let img = ctx.makeImage() {
            let url = CommandLine.arguments[3].replacingOccurrences(of: ".png", with: "-\(name).png")
            try? NSBitmapImageRep(cgImage: img).representation(using: .png, properties: [:])?.write(to: URL(fileURLWithPath: url))
        }
    }
}
