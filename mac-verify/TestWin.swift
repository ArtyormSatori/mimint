// TestWin.swift — macOS conformance test target for Mimint bodies.
//
// ⚠️ UNCOMPILED: this file has never been built. It was written from
// the AppKit API as documented, mirroring the behavior of the X11
// `testwin` binary (crates/mimint-body/src/bin/testwin.rs). The
// first Mac run must compile it, fix any API drift, and record the
// fixes before trusting any result from it. The Phase 3 fix pass
// edited this file (framebuffer-pixel geometry, pattern variant,
// Unicode tokens) WITHOUT compiling it — the same warning applies
// doubly to those edits.
//
// Behavior contract (identical to the X11 testwin):
// - Window 400x300 points at screen position (100, 100), top-left
//   origin. ALL logged coordinates are FRAMEBUFFER PIXELS
//   (points × backingScaleFactor), origin top-left — the same
//   space the suite clicks and samples in. The `GEOM` line reports
//   the content view's REAL screen position (a window may be
//   moved; GEOM is re-logged on move/resize).
// - Logs one line per event to the log file given as argv[1]:
//     GEOM x=.. y=.. w=.. h=..   (framebuffer pixels)
//     READY
//     BUTTON root_x=.. root_y=.. win_x=.. win_y=.. detail=1
//     KEY sym=0x.. tok=..
//     COLOR <name>
//   The `sym` field is informational; the oracle parses only `tok`.
// - Polls the control file given as argv[2] every 100ms; its
//   content (0/1/2/3) selects the content: 0/1/2 solid red/green/
//   blue, 3 = the pixel-pattern oracle quadrants (red/green/blue/
//   yellow). F5 toggles red <-> green (variants 0/1).
// - A space is logged as the visible token `<Space>`; Return/Tab/
//   Escape/Delete/F5 log as `<Enter>`/`<Tab>`/<Esc>`/`<BS>`/`<F5>`;
//   other keys log their character (from `event.characters`, WITH
//   modifiers, so Shift letters log uppercase); multi-character
//   input logs the first scalar as `<U+XXXX>`.
//
// Build (on the Mac):
//   swiftc -O -o TestWin TestWin.swift -framework AppKit
//
// Run (on the Mac, from this directory):
//   ./TestWin /tmp/mimint-testwin.log /tmp/mimint-testwin.ctl

import AppKit
import Foundation

let WIN_X: CGFloat = 100
let WIN_Y: CGFloat = 100
let WIN_W: CGFloat = 400
let WIN_H: CGFloat = 300

// Variant colors: far apart so screenshots differ unambiguously.
let VARIANTS: [(NSColor, String)] = [
    (.red, "red"),
    (.green, "green"),
    (.blue, "blue"),
]

final class EventLog {
    private let handle: FileHandle

    init?(path: String) {
        FileManager.default.createFile(atPath: path, contents: nil)
        guard let handle = FileHandle(forWritingAtPath: path) else { return nil }
        self.handle = handle
    }

    func log(_ line: String) {
        if let data = (line + "\n").data(using: .utf8) {
            handle.write(data)
            try? handle.synchronize()
        }
    }
}

final class TestView: NSView {
    var variant: Int = 0 {
        didSet { needsDisplay = true }
    }
    var log: EventLog?

    override var acceptsFirstResponder: Bool { true }

    override func draw(_ dirtyRect: NSRect) {
        if variant == 3 {
            // Pixel-pattern oracle: quadrants over the full bounds.
            let w = bounds.width / 2
            let h = bounds.height / 2
            NSColor.red.setFill()
            NSRect(x: 0, y: h, width: w, height: h).fill()
            NSColor.green.setFill()
            NSRect(x: w, y: h, width: w, height: h).fill()
            NSColor.blue.setFill()
            NSRect(x: 0, y: 0, width: w, height: h).fill()
            NSColor.yellow.setFill()
            NSRect(x: w, y: 0, width: w, height: h).fill()
            return
        }
        VARIANTS[variant].0.setFill()
        bounds.fill()
    }

    override func mouseDown(with event: NSEvent) {
        // Screen coordinates in FRAMEBUFFER PIXELS, origin top-left:
        // NSEvent points, flipped vertically, scaled by the backing
        // factor (1.0 non-Retina, 2.0 Retina).
        let scale = window?.backingScaleFactor ?? 1
        let screenPoint = NSEvent.mouseLocation
        let screenHeight = NSScreen.main?.frame.height ?? 0
        let rootX = Int((screenPoint.x * scale).rounded())
        let rootY = Int(((screenHeight - screenPoint.y) * scale).rounded())
        let winPoint = convert(event.locationInWindow, from: nil)
        let winX = Int((winPoint.x * scale).rounded())
        let winY = Int(((bounds.height - winPoint.y) * scale).rounded())
        log?.log("BUTTON root_x=\(rootX) root_y=\(rootY) win_x=\(winX) win_y=\(winY) detail=1")
    }

    override func keyDown(with event: NSEvent) {
        let keyCode = event.keyCode
        let token: String
        switch keyCode {
        case 36: token = "<Enter>"
        case 48: token = "<Tab>"
        case 53: token = "<Esc>"
        case 51: token = "<BS>"
        case 96: token = "<F5>"
        default:
            // `characters` (WITH modifiers) so Shift letters log
            // uppercase, matching what the suite sent.
            let chars = event.characters ?? ""
            if chars == " " {
                token = "<Space>"
            } else if chars.count == 1, let first = chars.first {
                token = String(first)
            } else if let scalar = chars.unicodeScalars.first {
                token = "<U+\(String(format: "%04X", scalar.value))>"
            } else {
                token = "<U+\(String(format: "%04X", keyCode))>"
            }
        }
        // sym is informational: the character scalar when there is
        // one, else the key code.
        let sym: UInt32
        if let scalar = event.characters?.unicodeScalars.first {
            sym = scalar.value
        } else {
            sym = UInt32(keyCode)
        }
        log?.log("KEY sym=0x\(String(format: "%x", sym)) tok=\(token)")
        if keyCode == 96 {
            // F5 toggles red <-> green (variants 0/1).
            variant = (variant == 0) ? 1 : 0
            log?.log("COLOR \(VARIANTS[variant].1)")
        }
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    var window: NSWindow?
    var view: TestView?
    var log: EventLog?
    let controlPath: String
    var lastControlContent: String = ""

    init(logPath: String, controlPath: String) {
        self.controlPath = controlPath
        super.init()
        self.log = EventLog(path: logPath)
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        guard let log = log else {
            fputs("TestWin: cannot open log file\n", stderr)
            NSApp.terminate(nil)
            return
        }
        // NSWindow coordinates are bottom-left origin: convert the
        // top-left (100, 100) target.
        let screenHeight = NSScreen.main?.frame.height ?? 0
        let frame = NSRect(
            x: WIN_X,
            y: screenHeight - WIN_Y - WIN_H,
            width: WIN_W,
            height: WIN_H
        )
        let window = NSWindow(
            contentRect: frame,
            styleMask: [.titled, .closable],
            backing: .buffered,
            defer: false
        )
        window.title = "Mimint TestWin"
        let view = TestView(frame: NSRect(x: 0, y: 0, width: WIN_W, height: WIN_H))
        view.log = log
        window.contentView = view
        window.makeKeyAndOrderFront(nil)
        window.makeFirstResponder(view)
        self.window = window
        self.view = view
        logGeom()
        log.log("READY")
        // Re-log geometry when the window moves or resizes: the
        // suite always aims from the latest GEOM line.
        NotificationCenter.default.addObserver(
            forName: NSWindow.didMoveNotification, object: window, queue: .main
        ) { [weak self] _ in self?.logGeom() }
        NotificationCenter.default.addObserver(
            forName: NSWindow.didResizeNotification, object: window, queue: .main
        ) { [weak self] _ in self?.logGeom() }
        // Poll the control file for content changes.
        Timer.scheduledTimer(withTimeInterval: 0.1, repeats: true) { [weak self] _ in
            self?.pollControlFile()
        }
    }

    /// Log the content view's REAL screen geometry in framebuffer
    /// pixels (points × backingScaleFactor, origin top-left).
    func logGeom() {
        guard let window = window, let view = view, let log = log else { return }
        let scale = window.backingScaleFactor
        let screenHeight = NSScreen.main?.frame.height ?? 0
        let windowRect = view.convert(view.bounds, to: nil)
        let screenRect = window.convertToScreen(windowRect)
        let x = Int((screenRect.origin.x * scale).rounded())
        let y = Int(((screenHeight - screenRect.origin.y - screenRect.height) * scale).rounded())
        let w = Int((screenRect.width * scale).rounded())
        let h = Int((screenRect.height * scale).rounded())
        log.log("GEOM x=\(x) y=\(y) w=\(w) h=\(h)")
    }

    func pollControlFile() {
        guard let content = try? String(contentsOfFile: controlPath, encoding: .utf8) else {
            return
        }
        let trimmed = content.trimmingCharacters(in: .whitespacesAndNewlines)
        guard trimmed != lastControlContent else { return }
        lastControlContent = trimmed
        if let value = Int(trimmed), (0...3).contains(value) {
            view?.variant = value
            let name = value == 3 ? "pattern" : VARIANTS[value].1
            log?.log("COLOR \(name)")
        }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }
}

let args = CommandLine.arguments
guard args.count == 3 else {
    fputs("usage: TestWin <log-path> <control-path>\n", stderr)
    exit(2)
}
let delegate = AppDelegate(logPath: args[1], controlPath: args[2])
let app = NSApplication.shared
app.delegate = delegate
app.run()
