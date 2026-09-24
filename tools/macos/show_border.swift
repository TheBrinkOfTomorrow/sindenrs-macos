// Show an image (the coded border from `sindenrs border export`) full screen above everything,
// transparent where the image is and click-through, for tracking tests before the real overlay.
//
//   swiftc -O show_border.swift -o show_border && ./show_border border.png [seconds]

import AppKit

let args = CommandLine.arguments
guard args.count > 1, let image = NSImage(contentsOfFile: args[1]) else {
    FileHandle.standardError.write("usage: show_border <image.png> [seconds]\n".data(using: .utf8)!)
    exit(2)
}
let seconds = args.count > 2 ? Double(args[2]) ?? 0 : 0

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
let screen = NSScreen.main!
let win = NSWindow(contentRect: screen.frame, styleMask: [.borderless], backing: .buffered, defer: false)
win.setFrame(screen.frame, display: false)
win.isOpaque = false
win.backgroundColor = .clear
win.hasShadow = false
win.ignoresMouseEvents = true
win.level = .screenSaver
win.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .stationary]
let view = NSImageView(frame: NSRect(origin: .zero, size: screen.frame.size))
view.image = image
view.imageScaling = .scaleAxesIndependently
win.contentView = view
win.orderFrontRegardless()
if seconds > 0 {
    DispatchQueue.main.asyncAfter(deadline: .now() + seconds) { app.terminate(nil) }
}
signal(SIGTERM) { _ in exit(0) }
app.run()
