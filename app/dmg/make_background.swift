// Draws the disk image window's background: a quiet light field with an arrow from where Seedwell's icon sits to
// where the Applications folder sits, and one line saying what to do. release-zip.sh makes it at 1x and 2x and joins
// them into one HiDPI TIFF (`tiffutil -cathidpicheck`) for dmgbuild (settings.py holds the matching layout).
//
//     swiftc -O app/dmg/make_background.swift -o /tmp/make_background && /tmp/make_background out@1x.png out@2x.png
//
// Drawn, not a checked-in picture, like the app icon: the layout numbers below and in settings.py are the only source.
import AppKit
import CoreGraphics
import ImageIO

/// The window's content size in points, and the icons' centres (the same numbers as settings.py).
let size = CGSize(width: 640, height: 400)
let appCentre = CGPoint(x: 170, y: 190)
let applicationsCentre = CGPoint(x: 470, y: 190)

let space = CGColorSpace(name: CGColorSpace.sRGB)!

func rgb(_ hex: UInt32, _ a: CGFloat = 1) -> CGColor {
    CGColor(colorSpace: space, components: [CGFloat((hex >> 16) & 255) / 255, CGFloat((hex >> 8) & 255) / 255, CGFloat(hex & 255) / 255, a])!
}

func draw(scale: CGFloat, to path: String) {
    let w = Int(size.width * scale), h = Int(size.height * scale)
    guard let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: 0, space: space,
                              bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { fatalError("no context") }
    ctx.scaleBy(x: scale, y: scale)
    // Finder's coordinates run from the top; CoreGraphics' from the bottom.
    func y(_ top: CGFloat) -> CGFloat { size.height - top }

    // A light field with a faint vertical fall-off (the install page's colours).
    let field = CGGradient(colorsSpace: space, colors: [rgb(0xFBFBFA), rgb(0xF0F0EC)] as CFArray, locations: [0, 1])!
    ctx.drawLinearGradient(field, start: CGPoint(x: 0, y: size.height), end: .zero, options: [])

    // The arrow: a soft stroke between the two icons (they are 128 pt wide), ending in an open head.
    let from = CGPoint(x: appCentre.x + 92, y: y(appCentre.y))
    let to = CGPoint(x: applicationsCentre.x - 92, y: y(applicationsCentre.y))
    ctx.setStrokeColor(rgb(0x2F7D4F, 0.85))
    ctx.setLineWidth(6)
    ctx.setLineCap(.round)
    ctx.setLineJoin(.round)
    ctx.move(to: from)
    ctx.addLine(to: to)
    ctx.strokePath()
    ctx.move(to: CGPoint(x: to.x - 18, y: to.y + 18))
    ctx.addLine(to: to)
    ctx.addLine(to: CGPoint(x: to.x - 18, y: to.y - 18))
    ctx.strokePath()

    // The one instruction, centred under the icons and their names.
    let text = "Drag Seedwell to Applications" as NSString
    let font = NSFont.systemFont(ofSize: 15, weight: .medium)
    let attrs: [NSAttributedString.Key: Any] = [.font: font, .foregroundColor: NSColor(cgColor: rgb(0x6E6E73))!]
    let measured = text.size(withAttributes: attrs)
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(cgContext: ctx, flipped: false)
    text.draw(at: CGPoint(x: (size.width - measured.width) / 2, y: y(320)), withAttributes: attrs)
    NSGraphicsContext.restoreGraphicsState()

    guard let image = ctx.makeImage(),
          let dest = CGImageDestinationCreateWithURL(URL(fileURLWithPath: path) as CFURL, "public.png" as CFString, 1, nil)
    else { fatalError("cannot write \(path)") }
    // 72 dpi at 1x and 144 at 2x, so tiffutil pairs them as one HiDPI image.
    CGImageDestinationAddImage(dest, image, [kCGImagePropertyDPIWidth: 72 * scale, kCGImagePropertyDPIHeight: 72 * scale] as CFDictionary)
    guard CGImageDestinationFinalize(dest) else { fatalError("cannot write \(path)") }
}

let args = CommandLine.arguments
guard args.count == 3 else {
    FileHandle.standardError.write(Data("usage: make_background <out@1x.png> <out@2x.png>\n".utf8))
    exit(2)
}
draw(scale: 1, to: args[1])
draw(scale: 2, to: args[2])
