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

// `make_icon --windows <dir>`: the sizes a Windows .ico holds instead (icon_<px>.png, each drawn from scratch), for
// the Windows app, which cannot run this script.
let windows = CommandLine.arguments.count > 2 && CommandLine.arguments[1] == "--windows"
// `make_icon --symbol <dir>.xcassets`: an asset catalog with the monochrome symbol `seedwell` (the same two arrows as
// a custom SF Symbol), which the File Provider extension names in its Info.plist (CFBundleIcons ›
// CFBundlePrimaryIcon › CFBundleSymbolName) so that the Finder sidebar shows it for an on-demand folder's domain
// instead of a plain folder. build.sh compiles it with `actool` into the extension's Assets.car.
let symbol = CommandLine.arguments.count > 2 && CommandLine.arguments[1] == "--symbol"
let out = windows || symbol ? CommandLine.arguments[2] : CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "AppIcon.iconset"
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

/// The symbol's outline in its own units (a 100 pt glyph: baseline at y 0, cap height 70.459, y up): the app icon's
/// two arrows (down on the left, up on the right; shaft, wing and spacing in the icon's proportions) stroked at about
/// SF Symbols' Regular weight and merged into one filled shape, since a custom symbol is filled, not stroked.
/// Returns the outline and the advance width (side bearings included).
func symbolGlyph() -> (CGPath, CGFloat) {
    let stroke: CGFloat = 9, bearing: CGFloat = 3
    let reach: CGFloat = 36                                  // half the shaft: 72 units, centred on the cap height
    let wing = reach * 2 * (0.135 / 0.47)                    // make_icon's wing : shaft
    let spacing = reach * 2 * (0.316 / 0.47)                 // make_icon's arrow spacing : shaft
    let cy: CGFloat = 70.459 / 2
    let left = bearing + stroke / 2 + wing
    var glyph = CGMutablePath() as CGPath
    for (x, up) in [(left, false), (left + spacing, true)] {
        let tip = up ? cy + reach : cy - reach, tail = up ? cy - reach : cy + reach, back = up ? -wing : wing
        let p = CGMutablePath()
        p.move(to: CGPoint(x: x, y: tail))
        p.addLine(to: CGPoint(x: x, y: tip))
        p.move(to: CGPoint(x: x - wing, y: tip + back))
        p.addLine(to: CGPoint(x: x, y: tip))
        p.addLine(to: CGPoint(x: x + wing, y: tip + back))
        glyph = glyph.union(p.copy(strokingWithWidth: stroke, lineCap: .round, lineJoin: .round, miterLimit: 1), using: .winding)
    }
    return (glyph, left + spacing + wing + stroke / 2 + bearing)
}

/// SVG path data for `path`, y flipped (SVG's y runs down; a symbol's glyph sits on its baseline at y 0).
func svgPath(_ path: CGPath) -> String {
    var d: [String] = []
    func f(_ v: CGFloat) -> String { String(format: "%.3f", v) }
    func pt(_ p: CGPoint) -> String { "\(f(p.x)) \(f(-p.y))" }
    path.applyWithBlock { e in
        let el = e.pointee, ps = el.points
        switch el.type {
        case .moveToPoint: d.append("M\(pt(ps[0]))")
        case .addLineToPoint: d.append("L\(pt(ps[0]))")
        case .addQuadCurveToPoint: d.append("Q\(pt(ps[0])) \(pt(ps[1]))")
        case .addCurveToPoint: d.append("C\(pt(ps[0])) \(pt(ps[1])) \(pt(ps[2]))")
        case .closeSubpath: d.append("Z")
        @unknown default: break
        }
    }
    return d.joined(separator: " ")
}

if symbol {
    // Apple's custom-symbol template (SF Symbols' export, version 3.0) with one design, Regular at Medium scale; the
    // system derives the other weights and scales from it.
    let (glyph, advance) = symbolGlyph()
    let x0: CGFloat = 1500
    func line(_ id: String, _ x1: CGFloat, _ y1: CGFloat, _ x2: CGFloat, _ y2: CGFloat) -> String {
        "  <line id=\"\(id)\" style=\"fill:none;stroke:#27AAE1;opacity:1;stroke-width:0.5;\" x1=\"\(x1)\" x2=\"\(x2)\" y1=\"\(y1)\" y2=\"\(y2)\"/>"
    }
    var guides: [String] = []
    for (scale, base) in [("S", 696.0), ("M", 1126.0), ("L", 1556.0)] {
        guides.append(line("Baseline-\(scale)", 263, base, 3036, base))
        guides.append(line("Capline-\(scale)", 263, base - 70.459, 3036, base - 70.459))
    }
    guides.append(line("left-margin-Regular-M", x0, 1030, x0, 1150))
    guides.append(line("right-margin-Regular-M", x0 + advance, 1030, x0 + advance, 1150))
    let svg = """
    <?xml version="1.0" encoding="UTF-8"?>
    <!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN" "http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd">
    <!-- Drawn by app/Resources/make_icon.swift (its symbol mode); not edited by hand. -->
    <svg version="1.1" xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="3300" height="2200">
     <g id="Notes">
      <rect height="2200" id="artboard" style="fill:white;opacity:1" width="3300" x="0" y="0"/>
      <text style="stroke:none;fill:black;font-family:sans-serif;font-size:13;" transform="matrix(1 0 0 1 263 1933)">Seedwell</text>
      <text id="template-version" style="stroke:none;fill:black;font-family:sans-serif;font-size:13;text-anchor:end;" transform="matrix(1 0 0 1 3036 1933)">Template v.3.0</text>
     </g>
     <g id="Guides">
    \(guides.joined(separator: "\n"))
     </g>
     <g id="Symbols">
      <g id="Regular-M" transform="matrix(1 0 0 1 \(x0) 1126)">
       <path d="\(svgPath(glyph))"/>
      </g>
     </g>
    </svg>

    """
    let set = "\(out)/seedwell.symbolset"
    try? FileManager.default.createDirectory(atPath: set, withIntermediateDirectories: true)
    let info = #"{ "info" : { "author" : "xcode", "version" : 1 } }"#
    let contents = #"{ "info" : { "author" : "xcode", "version" : 1 }, "symbols" : [ { "filename" : "seedwell.svg", "idiom" : "universal" } ] }"#
    do {
        try info.write(toFile: "\(out)/Contents.json", atomically: true, encoding: .utf8)
        try contents.write(toFile: "\(set)/Contents.json", atomically: true, encoding: .utf8)
        try svg.write(toFile: "\(set)/seedwell.svg", atomically: true, encoding: .utf8)
    } catch {
        FileHandle.standardError.write("make_icon --symbol: \(error)\n".data(using: .utf8)!)
        exit(1)
    }
    exit(0)
}

if windows {
    for px in [16, 20, 24, 32, 40, 48, 64, 96, 128, 256] {
        try? render(px)?.write(to: URL(fileURLWithPath: "\(out)/icon_\(px).png"))
    }
    exit(0)
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
