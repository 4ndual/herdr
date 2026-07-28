// draw-overlay — self-owned screen-annotation overlay + screenshot.
//
// Replaces the third-party DrawPen dependency the draw-screenshot feature used
// to drive. DrawPen's overlay windows were plain, non-sticky NSWindows bound to
// their birth Space, so invoking them from any other Space (a native-fullscreen
// hr window, or just a second desktop) force-switched the display to that Space
// and drew on the wrong screen. This helper owns the window, so it sets the
// exact collectionBehavior macOS needs to float the overlay over ANY Space,
// including a fullscreen auxiliary Space:
//     collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
//     level              = .screenSaver   (above normal/fullscreen content)
// Verify with spacediag: the window reports sticky=true across all Space ids.
//
// One-shot process: each launch is a fresh, BLANK canvas (no residency, no
// stale drawings) and it exits after capture/cancel — so the "always start
// blank, no delay" guarantee is structural, not config-dependent.
//
// Usage:
//   draw-overlay full     draw, then ENTER captures the whole screen
//   draw-overlay region   draw, then ENTER captures an interactive rectangle
// Live controls:
//   drag           freehand draw
//   1..6           red / orange / yellow / green / blue / white
//   [ ]            thinner / thicker stroke
//   u / ⌘Z         undo last stroke
//   c              clear canvas
//   RETURN         capture (clipboard + ~/Desktop/drawshot-<ts>.png) then quit
//   ESC            cancel + quit (no capture)
//   SIGUSR1        capture (lets the button/Karabiner second-press trigger it)
import AppKit

let mode = CommandLine.arguments.count >= 2 ? CommandLine.arguments[1] : "full"

// ── stroke model ─────────────────────────────────────────────────────────────
struct Stroke { var color: NSColor; var width: CGFloat; var points: [NSPoint] }

final class Canvas: NSView {
    var strokes: [Stroke] = []
    var current: Stroke?
    var color: NSColor = .systemRed
    var width: CGFloat = 4
    var hideChrome = false   // hint text is removed just before a capture

    override var isFlipped: Bool { false }
    override var acceptsFirstResponder: Bool { true }
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    override func mouseDown(with e: NSEvent) {
        current = Stroke(color: color, width: width, points: [convert(e.locationInWindow, from: nil)])
        needsDisplay = true
    }
    override func mouseDragged(with e: NSEvent) {
        current?.points.append(convert(e.locationInWindow, from: nil))
        needsDisplay = true
    }
    override func mouseUp(with e: NSEvent) {
        if var s = current { s.points.append(convert(e.locationInWindow, from: nil)); strokes.append(s) }
        current = nil
        needsDisplay = true
    }

    func draw(_ s: Stroke) {
        guard s.points.count > 1 else {
            // a single tap → a dot
            if let p = s.points.first {
                s.color.setFill()
                NSBezierPath(ovalIn: NSRect(x: p.x - s.width/2, y: p.y - s.width/2, width: s.width, height: s.width)).fill()
            }
            return
        }
        let path = NSBezierPath()
        path.lineWidth = s.width
        path.lineCapStyle = .round
        path.lineJoinStyle = .round
        path.move(to: s.points[0])
        for p in s.points.dropFirst() { path.line(to: p) }
        s.color.setStroke()
        path.stroke()
    }

    override func draw(_ dirty: NSRect) {
        for s in strokes { draw(s) }
        if let c = current { draw(c) }
        guard !hideChrome else { return }
        // small, non-intrusive hint bar (top-center) — removed before capture
        let hint = mode == "region"
            ? "draw  ·  RETURN = crop region  ·  ESC = cancel  ·  1-6 color  ·  [ ] size  ·  u undo  ·  c clear"
            : "draw  ·  RETURN = screenshot  ·  ESC = cancel  ·  1-6 color  ·  [ ] size  ·  u undo  ·  c clear"
        let attrs: [NSAttributedString.Key: Any] = [
            .font: NSFont.systemFont(ofSize: 13, weight: .medium),
            .foregroundColor: NSColor.white,
        ]
        let size = (hint as NSString).size(withAttributes: attrs)
        let pad: CGFloat = 10
        let box = NSRect(x: (bounds.width - size.width)/2 - pad, y: bounds.height - size.height - 24,
                         width: size.width + pad*2, height: size.height + pad)
        NSColor(white: 0, alpha: 0.62).setFill()
        NSBezierPath(roundedRect: box, xRadius: 8, yRadius: 8).fill()
        // current-color swatch
        color.setFill()
        NSBezierPath(ovalIn: NSRect(x: box.minX + 6, y: box.midY - 5, width: 10, height: 10)).fill()
        (hint as NSString).draw(at: NSPoint(x: box.minX + pad, y: box.minY + pad/2), withAttributes: attrs)
    }
}

final class OverlayWindow: NSWindow {
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { true }
}

final class Controller: NSObject {
    let window: OverlayWindow
    let canvas: Canvas
    let palette: [NSColor] = [.systemRed, .systemOrange, .systemYellow, .systemGreen, .systemBlue, .white]

    override init() {
        // union frame across all screens so the overlay covers every display
        var frame = NSScreen.screens.first?.frame ?? .zero
        for s in NSScreen.screens { frame = frame.union(s.frame) }
        window = OverlayWindow(contentRect: frame, styleMask: [.borderless], backing: .buffered, defer: false)
        canvas = Canvas(frame: NSRect(origin: .zero, size: frame.size))
        super.init()
        window.isOpaque = false
        window.backgroundColor = .clear
        window.hasShadow = false
        window.level = .screenSaver
        window.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .stationary, .ignoresCycle]
        window.ignoresMouseEvents = false
        window.acceptsMouseMovedEvents = true
        window.contentView = canvas
        window.setFrame(frame, display: true)
    }

    func run() {
        focus()
        // Re-assert key focus a few times: when launched detached from a shell
        // (nohup, the button/Karabiner path) the first activate() can lose the
        // race with the GUI session, which would drop an immediate RETURN/ESC
        // before the user ever clicks. Retrying guarantees the overlay is key.
        for delay in [0.05, 0.15, 0.35] {
            DispatchQueue.main.asyncAfter(deadline: .now() + delay) { [weak self] in self?.focus() }
        }
        signal(SIGUSR1, SIG_IGN)
        let src = DispatchSource.makeSignalSource(signal: SIGUSR1, queue: .main)
        src.setEventHandler { [weak self] in self?.capture() }
        src.resume()
        // keep the source alive
        objc_setAssociatedObject(self, "sig", src, .OBJC_ASSOCIATION_RETAIN)
    }

    func key(_ e: NSEvent) {
        // number keys 1..6 → palette
        if let ch = e.charactersIgnoringModifiers, let n = Int(ch), (1...palette.count).contains(n) {
            canvas.color = palette[n-1]; canvas.needsDisplay = true; return
        }
        switch e.keyCode {
        case 36, 76: capture()                                   // Return / keypad Enter
        case 53: NSApp.terminate(nil)                            // Esc
        case 33: canvas.width = max(1, canvas.width - 1); canvas.needsDisplay = true  // [
        case 30: canvas.width = min(40, canvas.width + 1); canvas.needsDisplay = true // ]
        case 8:  canvas.strokes.removeAll(); canvas.needsDisplay = true               // c
        case 6:  if e.modifierFlags.contains(.command) { undo() }                      // ⌘Z
        case 32: undo()                                                                // u
        default: break
        }
    }
    func undo() { if !canvas.strokes.isEmpty { canvas.strokes.removeLast(); canvas.needsDisplay = true } }

    func focus() {
        NSApp.activate(ignoringOtherApps: true)
        window.makeKeyAndOrderFront(nil)
        window.makeFirstResponder(canvas)
    }

    func capture() {
        // strip the hint bar so the shot contains only the underlying screen +
        // the user's drawings, then flush the redraw synchronously before the
        // framebuffer grab.
        canvas.hideChrome = true
        canvas.display()
        window.displayIfNeeded()
        CATransaction.flush()

        let ts = fmtNow()
        let out = ("\(NSHomeDirectory())/Desktop/drawshot-\(ts).png") as String
        if mode == "region" {
            runTool("/usr/sbin/screencapture", ["-i", "-t", "png", out])   // interactive crop
            if Fileapp.exists(out) { copyToPasteboard(out) }
        } else {
            runTool("/usr/sbin/screencapture", ["-x", "-t", "png", out])   // full screen
            if Fileapp.exists(out) { copyToPasteboard(out) }
        }
        NSApp.terminate(nil)
    }

    func copyToPasteboard(_ path: String) {
        guard let img = NSImage(contentsOfFile: path) else { return }
        let pb = NSPasteboard.general
        pb.clearContents()
        pb.writeObjects([img])
    }
}

// tiny fs + exec helpers
enum Fileapp { static func exists(_ p: String) -> Bool { let a = try? FileManager.default.attributesOfItem(atPath: p); return (a?[.size] as? Int ?? 0) > 0 } }
func runTool(_ tool: String, _ args: [String]) {
    let p = Process(); p.executableURL = URL(fileURLWithPath: tool); p.arguments = args
    try? p.run(); p.waitUntilExit()
}
func fmtNow() -> String {
    let f = DateFormatter(); f.dateFormat = "yyyyMMdd-HHmmss"; return f.string(from: Date())
}

// ── app wiring ───────────────────────────────────────────────────────────────
final class AppDelegate: NSObject, NSApplicationDelegate {
    var controller: Controller?
    func applicationDidFinishLaunching(_ n: Notification) {
        let c = Controller(); controller = c; c.run()
    }
}

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
let delegate = AppDelegate()
app.delegate = delegate
// route key events (borderless overlay is key window)
NSEvent.addLocalMonitorForEvents(matching: .keyDown) { e in
    delegate.controller?.key(e); return nil
}
app.run()
