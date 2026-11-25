// SPDX-FileCopyrightText: 2026 hiunivs
// SPDX-License-Identifier: AGPL-3.0-only
// Alternative commercial licensing and AGPL section 7 terms: see NOTICE.

// Renders the RShell app icon (dark rounded terminal tile with a ">_" prompt)
// to a 1024x1024 PNG. Runs with the stock macOS Swift toolchain, no packages.
import AppKit

guard CommandLine.arguments.count == 2 else {
    fatalError("usage: swift make_icon.swift <output.png>")
}

_ = NSApplication.shared
let canvas: CGFloat = 1024
let image = NSImage(size: NSSize(width: canvas, height: canvas))
image.lockFocus()

let tile = NSBezierPath(
    roundedRect: NSRect(x: 72, y: 72, width: 880, height: 880),
    xRadius: 200, yRadius: 200
)
NSColor(calibratedRed: 0.10, green: 0.11, blue: 0.15, alpha: 1).setFill()
tile.fill()

// Draw ">_" as geometry instead of text: font metrics differ across runner
// images and Retina backing scales, paths are deterministic everywhere.
NSColor(calibratedRed: 0.42, green: 0.86, blue: 0.55, alpha: 1).setStroke()
NSColor(calibratedRed: 0.42, green: 0.86, blue: 0.55, alpha: 1).setFill()

let chevron = NSBezierPath()
chevron.move(to: NSPoint(x: 294, y: 660))
chevron.line(to: NSPoint(x: 504, y: 512))
chevron.line(to: NSPoint(x: 294, y: 364))
chevron.lineWidth = 76
chevron.lineCapStyle = .round
chevron.lineJoinStyle = .round
chevron.stroke()

let cursor = NSBezierPath(
    roundedRect: NSRect(x: 585, y: 330, width: 220, height: 64),
    xRadius: 16, yRadius: 16
)
cursor.fill()
image.unlockFocus()

guard let tiff = image.tiffRepresentation,
      let bitmap = NSBitmapImageRep(data: tiff),
      let png = bitmap.representation(using: .png, properties: [:])
else {
    fatalError("failed to render icon")
}
try png.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))
