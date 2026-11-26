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

// macOS icon grid: the rounded tile occupies 824x824 on the 1024 canvas.
// Using the standard grid matters because setApplicationIconImage draws the
// image as-is at runtime, without the system's icon scaling.
let tile = NSBezierPath(
    roundedRect: NSRect(x: 100, y: 100, width: 824, height: 824),
    xRadius: 185, yRadius: 185
)
NSColor(calibratedRed: 0.10, green: 0.11, blue: 0.15, alpha: 1).setFill()
tile.fill()

// Draw ">_" as geometry instead of text: font metrics differ across runner
// images and Retina backing scales, paths are deterministic everywhere.
NSColor(calibratedRed: 0.42, green: 0.86, blue: 0.55, alpha: 1).setStroke()
NSColor(calibratedRed: 0.42, green: 0.86, blue: 0.55, alpha: 1).setFill()

let chevron = NSBezierPath()
chevron.move(to: NSPoint(x: 308, y: 651))
chevron.line(to: NSPoint(x: 405, y: 512))
chevron.line(to: NSPoint(x: 308, y: 373))
chevron.lineWidth = 71
chevron.lineCapStyle = .round
chevron.lineJoinStyle = .round
chevron.stroke()

let cursor = NSBezierPath(
    roundedRect: NSRect(x: 580, y: 342, width: 206, height: 60),
    xRadius: 15, yRadius: 15
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
