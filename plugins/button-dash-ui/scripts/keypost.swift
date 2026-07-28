// keypost — fast CGEvent keystroke poster + window-visibility query.
//   keypost <mods> <key>                 post keystroke (mods: cmd,shift,ctrl,opt)
//   keypost visible <AppName> [WinName]  exit 0 if the app owns an on-screen
//                                        window (optionally matching WinName)
import AppKit
import ApplicationServices
import CoreGraphics
import Foundation

let args = CommandLine.arguments
if args.count == 3, args[1] == "clip" {
    guard let img = NSImage(contentsOfFile: args[2]) else { exit(1) }
    let pb = NSPasteboard.general
    pb.clearContents()
    exit(pb.writeObjects([img]) ? 0 : 1)
}
if args.count >= 3, args[1] == "visible" {
    let app = args[2]
    let win = args.count >= 4 ? args[3] : nil
    let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
    for w in list where (w[kCGWindowOwnerName as String] as? String) == app {
        let name = w[kCGWindowName as String] as? String ?? ""
        if let want = win {
            if name == want { exit(0) }
        } else if let b = w[kCGWindowBounds as String] as? [String: Any],
                  let wd = b["Width"] as? Double, let ht = b["Height"] as? Double,
                  wd > 50, ht > 30 { exit(0) }
    }
    exit(1)
}
let keycodes: [String: CGKeyCode] = ["a": 0, "s": 1, "k": 40, "e": 14, "t": 17]
guard args.count == 3, let code = keycodes[args[2].lowercased()] else {
    FileHandle.standardError.write("usage: keypost <mods> <key> | keypost visible <App> [Window]\n".data(using: .utf8)!)
    exit(2)
}
var flags: CGEventFlags = []
for m in args[1].split(separator: ",") {
    switch m {
    case "cmd": flags.insert(.maskCommand)
    case "shift": flags.insert(.maskShift)
    case "ctrl": flags.insert(.maskControl)
    case "opt": flags.insert(.maskAlternate)
    default: exit(2)
    }
}
if !AXIsProcessTrusted() {
    FileHandle.standardError.write("keypost: NO ACCESSIBILITY permission — keystroke would be dropped\n".data(using: .utf8)!)
    exit(3)
}
let src = CGEventSource(stateID: .hidSystemState)
for down in [true, false] {
    guard let ev = CGEvent(keyboardEventSource: src, virtualKey: code, keyDown: down) else { exit(1) }
    ev.flags = flags
    ev.post(tap: .cghidEventTap)
    usleep(20000)
}
