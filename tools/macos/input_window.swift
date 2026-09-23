// macOS hardware check: a window that shows and logs every mouse button, scroll and key event it
// receives, so the gun's HID mouse/keyboard output can be checked without Input Monitoring
// permission (a window sees its own events). Park the gun's cursor over it.
//
// Args: [log path]. Each line: unix time, event, detail. Quit with Cmd-Q or the log's "q" key.

import AppKit

let logPath = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "input.log"
FileManager.default.createFile(atPath: logPath, contents: nil)
let log = FileHandle(forWritingAtPath: logPath)!

final class EventView: NSView {
    var lines: [String] = []
    override var acceptsFirstResponder: Bool { true }
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    func record(_ what: String, _ detail: String = "") {
        let t = String(format: "%.3f", Date().timeIntervalSince1970)
        log.write("\(t) \(what) \(detail)\n".data(using: .utf8)!)
        let clock = DateFormatter.localizedString(from: Date(), dateStyle: .none, timeStyle: .medium)
        lines.insert("\(clock)  \(what) \(detail)", at: 0)
        lines = Array(lines.prefix(14))
        needsDisplay = true
    }

    override func mouseDown(with e: NSEvent) { record("mouse_left down") }
    override func mouseUp(with e: NSEvent) { record("mouse_left up") }
    override func rightMouseDown(with e: NSEvent) { record("mouse_right down") }
    override func rightMouseUp(with e: NSEvent) { record("mouse_right up") }
    override func otherMouseDown(with e: NSEvent) { record("mouse_other down", "button \(e.buttonNumber)") }
    override func otherMouseUp(with e: NSEvent) { record("mouse_other up", "button \(e.buttonNumber)") }
    override func scrollWheel(with e: NSEvent) { record("scroll", "\(e.scrollingDeltaX),\(e.scrollingDeltaY)") }
    override func keyDown(with e: NSEvent) {
        if e.isARepeat { return }
        record("key down", keyName(e))
    }
    override func keyUp(with e: NSEvent) { record("key up", keyName(e)) }
    override func flagsChanged(with e: NSEvent) { record("modifiers", "\(e.modifierFlags.rawValue)") }

    func keyName(_ e: NSEvent) -> String {
        switch e.keyCode {
        case 123: return "left"
        case 124: return "right"
        case 125: return "down"
        case 126: return "up"
        default: return "\(e.charactersIgnoringModifiers ?? "?") (code \(e.keyCode))"
        }
    }

    override func draw(_ r: NSRect) {
        NSColor.windowBackgroundColor.setFill()
        r.fill()
        let head: [NSAttributedString.Key: Any] = [.font: NSFont.boldSystemFont(ofSize: 22), .foregroundColor: NSColor.labelColor]
        let body: [NSAttributedString.Key: Any] = [.font: NSFont.monospacedSystemFont(ofSize: 20, weight: .regular), .foregroundColor: NSColor.labelColor]
        // Crosshair at the screen centre, where the probe aims the gun.
        NSColor.systemRed.setStroke()
        let c = NSPoint(x: bounds.midX, y: bounds.midY)
        let cross = NSBezierPath()
        cross.move(to: NSPoint(x: c.x - 30, y: c.y)); cross.line(to: NSPoint(x: c.x + 30, y: c.y))
        cross.move(to: NSPoint(x: c.x, y: c.y - 30)); cross.line(to: NSPoint(x: c.x, y: c.y + 30))
        cross.lineWidth = 3
        cross.stroke()
        let top = bounds.height - 120
        "Sinden input test: press each control (Cmd-Q quits)".draw(at: NSPoint(x: 80, y: top), withAttributes: head)
        for (i, l) in lines.enumerated() {
            l.draw(at: NSPoint(x: 80, y: top - 46 - CGFloat(i) * 30), withAttributes: body)
        }
    }
}

let app = NSApplication.shared
app.setActivationPolicy(.regular)
let menu = NSMenu()
let appItem = NSMenuItem()
menu.addItem(appItem)
appItem.submenu = NSMenu()
appItem.submenu!.addItem(withTitle: "Quit", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
app.mainMenu = menu

// Borderless and covering the whole main screen, so a click lands in it wherever the gun aims.
final class FullWindow: NSWindow {
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { true }
}
let screen = NSScreen.main!
let win = FullWindow(contentRect: screen.frame, styleMask: [.borderless], backing: .buffered, defer: false)
win.setFrame(screen.frame, display: true)
let view = EventView(frame: NSRect(origin: .zero, size: screen.frame.size))
win.contentView = view
win.level = .floating
win.makeKeyAndOrderFront(nil)
win.makeFirstResponder(view)
app.activate(ignoringOtherApps: true)
view.record("ready", "window \(win.frame) screens \(NSScreen.screens.map { $0.frame })")

// Every event the app receives, whatever its type, before any view sees it; catches clicks that
// arrive as tablet or other event types instead of mouseDown.
NSEvent.addLocalMonitorForEvents(matching: .any) { e in
    switch e.type {
    case .mouseMoved, .scrollWheel, .keyDown, .keyUp, .flagsChanged, .appKitDefined, .systemDefined:
        break
    default:
        view.record("any", "\(e)")
    }
    return e
}

// System-wide state, which needs no permission: which mouse buttons are down anywhere, and
// where the cursor is. Tells "clicks landed elsewhere" apart from "no clicks at all".
var lastButtons = -1
var lastSpot = NSPoint(x: -1, y: -1)
Timer.scheduledTimer(withTimeInterval: 0.01, repeats: true) { _ in
    let b = NSEvent.pressedMouseButtons
    let p = NSEvent.mouseLocation
    if b != lastButtons || abs(p.x - lastSpot.x) + abs(p.y - lastSpot.y) > 20 {
        view.record("system", "buttons=\(b) cursor=(\(Int(p.x)),\(Int(p.y))) inWindow=\(win.frame.contains(p))")
        lastButtons = b
        lastSpot = p
    }
}
app.run()
