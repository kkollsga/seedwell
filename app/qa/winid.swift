// Lists on-screen windows of a process by owner name: `winid SeedwellApp` prints
// "<windowID> <layer> <x>,<y> <w>x<h> <title>" per window. Used by the QA scripts with
// `screencapture -x -o -l <windowID> out.png`.
import CoreGraphics
import Foundation

let owner = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "SeedwellApp"
let opts: CGWindowListOption = [.optionOnScreenOnly, .excludeDesktopElements]
guard let list = CGWindowListCopyWindowInfo(opts, kCGNullWindowID) as? [[String: Any]] else { exit(1) }
// QA_PID limits the list to the QA app's own process: the user's real Seedwell has the same owner name.
let onlyPid = ProcessInfo.processInfo.environment["QA_PID"].flatMap { Int($0) }
for w in list {
    guard (w[kCGWindowOwnerName as String] as? String) == owner else { continue }
    if let onlyPid, (w[kCGWindowOwnerPID as String] as? Int) != onlyPid { continue }
    let id = w[kCGWindowNumber as String] as? Int ?? 0
    let layer = w[kCGWindowLayer as String] as? Int ?? 0
    let b = w[kCGWindowBounds as String] as? [String: Double] ?? [:]
    let title = w[kCGWindowName as String] as? String ?? ""
    print("\(id) \(layer) \(Int(b["X"] ?? 0)),\(Int(b["Y"] ?? 0)) \(Int(b["Width"] ?? 0))x\(Int(b["Height"] ?? 0)) \(title)")
}
