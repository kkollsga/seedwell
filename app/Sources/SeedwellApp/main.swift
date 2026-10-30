import AppKit

// Launch arguments: see `LaunchOptions`. `--home-root` redirects `~` before anything starts.
let options = LaunchOptions(CommandLine.arguments)
options.applyHomeRoot()

// A QA-control run never comes forward and shows no window on a screen (QuietRun), from its first window on.
if options.qaControl != nil { MainActor.assumeIsolated { QuietRun.start() } }

let app = NSApplication.shared
let delegate = AppDelegate(options: options)
app.delegate = delegate
app.setActivationPolicy(.accessory)
app.run()
