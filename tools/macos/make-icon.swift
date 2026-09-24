// Draw Sindenrs.app's icon and write tools/macos/AppIcon.icns: a dark rounded square (the
// macOS app-icon grid: an 824-point shape on a 1024 canvas) with the white "scope" crosshair
// the menu bar item uses and a red centre dot.
//
//   swift tools/macos/make-icon.swift        (then commit tools/macos/AppIcon.icns)

import AppKit

let here = URL(fileURLWithPath: CommandLine.arguments[0]).deletingLastPathComponent()
let iconset = FileManager.default.temporaryDirectory.appendingPathComponent("AppIcon.iconset")
try? FileManager.default.removeItem(at: iconset)
try FileManager.default.createDirectory(at: iconset, withIntermediateDirectories: true)

func render(_ px: Int) -> Data {
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: px, pixelsHigh: px,
                               bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                               colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    let s = CGFloat(px) / 1024
    // Background shape.
    let shape = NSRect(x: 100 * s, y: 100 * s, width: 824 * s, height: 824 * s)
    let path = NSBezierPath(roundedRect: shape, xRadius: 185 * s, yRadius: 185 * s)
    NSGradient(starting: NSColor(calibratedRed: 0.20, green: 0.22, blue: 0.27, alpha: 1),
               ending: NSColor(calibratedRed: 0.06, green: 0.07, blue: 0.09, alpha: 1))!
        .draw(in: path, angle: -90)
    // A thin white frame inside, a nod to the border the gun tracks.
    NSColor(white: 1, alpha: 0.18).setStroke()
    let frame = NSBezierPath(roundedRect: shape.insetBy(dx: 58 * s, dy: 58 * s), xRadius: 120 * s, yRadius: 120 * s)
    frame.lineWidth = max(1, 14 * s)
    frame.stroke()
    // The crosshair.
    let config = NSImage.SymbolConfiguration(pointSize: 520 * s, weight: .medium)
        .applying(NSImage.SymbolConfiguration(paletteColors: [.white]))
    if let scope = NSImage(systemSymbolName: "scope", accessibilityDescription: nil)?
        .withSymbolConfiguration(config) {
        let size = scope.size
        scope.draw(in: NSRect(x: 512 * s - size.width / 2, y: 512 * s - size.height / 2,
                              width: size.width, height: size.height))
    }
    // Red centre dot.
    NSColor(calibratedRed: 0.93, green: 0.18, blue: 0.16, alpha: 1).setFill()
    let r = 34 * s
    NSBezierPath(ovalIn: NSRect(x: 512 * s - r, y: 512 * s - r, width: 2 * r, height: 2 * r)).fill()
    NSGraphicsContext.restoreGraphicsState()
    return rep.representation(using: .png, properties: [:])!
}

for (points, scales) in [(16, [1, 2]), (32, [1, 2]), (128, [1, 2]), (256, [1, 2]), (512, [1, 2])] {
    for scale in scales {
        let name = scale == 1 ? "icon_\(points)x\(points).png" : "icon_\(points)x\(points)@2x.png"
        try render(points * scale).write(to: iconset.appendingPathComponent(name))
    }
}
let out = here.appendingPathComponent("AppIcon.icns")
let p = Process()
p.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
p.arguments = ["-c", "icns", iconset.path, "-o", out.path]
try p.run()
p.waitUntilExit()
print(p.terminationStatus == 0 ? "wrote \(out.path)" : "iconutil failed")
