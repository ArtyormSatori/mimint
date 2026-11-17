// TestWin.swift — macOS conformance test target for Mimint bodies.
//
// ⚠️ UNCOMPILED: this file has never been built. It was written from
// the AppKit API as documented, mirroring the behavior of the X11
// `testwin` binary (crates/mimint-body/src/bin/testwin.rs). The
// first Mac run must compile it, fix any API drift, and record the
// fixes before trusting any result from it.
//
// Behavior contract (identical to the X11 testwin):
// - Window 400x300 at screen position (100, 100), top-left origin.
// - Logs one line per event to the log file given as argv[1]:
//     GEOM x=100 y=100 w=400 h=300
//     READY
//     BUTTON root_x=.. root_y=.. win_x=.. win_y=.. detail=1
//     KEY sym=0x.. tok=..
//     COLOR <name>
//   Coordinates are Quartz screen pixels, origin TOP-left (the
//   NSEvent screen origin is bottom-left; the conversion is in
//   TestView.mouseDown). The `sym` field is informational; the
//   oracle parses only `tok`.
// - Polls the control file given as argv[2] every 100ms; its
//   content (0/1/2) selects the fill color (red/green/blue).
// - F5 toggles red <-> green (variants 0/1).
// - A space is logged as the visible token `<Space>`; Return/Tab/
//   Escape/Delete/F5 log as `<Enter>`/`<Tab>`/<Esc>`/`<BS>`/`<F5>`;
//   other keys log their character.
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
        VARIANTS[variant].0.setFill()
        dirtyRect.fill()
    }

    override func mouseDown(with event: NSEvent) {
        // Screen coordinates, origin top-left (Quartz space).
        let screenPoint = NSEvent.mouseLocation
        let screenHeight = NSScreen.main?.frame.height ?? 0
        let rootX = Int(screenPoint.x.rounded())
        let rootY = Int((screenHeight - screenPoint.y).rounded())
        let winPoint = convert(event.locationInWindow, from: nil)
        let winX = Int(winPoint.x.rounded())
        let winY = Int((bounds.height - winPoint.y).rounded())
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
            let chars = event.charactersIgnoringModifiers ?? ""
            if chars == " " {
                token = "<Space>"
            } else if let first = chars.first, chars.count == 1 {
                token = String(first)
            } else {
                token = "<U+\(String(format: "%04X", keyCode))>"
            }
        }
        // sym is informational: the character scalar when there is
        // one, else the key code.
        let sym: UInt32
        if let scalar = event.charactersIgnoringModifiers?.unicodeScalars.first {
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
        log.log("GEOM x=100 y=100 w=400 h=300")
        log.log("READY")
        // Poll the control file for content changes.
        Timer.scheduledTimer(withTimeInterval: 0.1, repeats: true) { [weak self] _ in
            self?.pollControlFile()
        }
    }

    func pollControlFile() {
        guard let content = try? String(contentsOfFile: controlPath, encoding: .utf8) else {
            return
        }
        let trimmed = content.trimmingCharacters(in: .whitespacesAndNewlines)
        guard trimmed != lastControlContent else { return }
        lastControlContent = trimmed
        if let value = Int(trimmed), (0..<VARIANTS.count).contains(value) {
            view?.variant = value
            log?.log("COLOR \(VARIANTS[value].1)")
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
