import AppKit

// Launch arguments: see `LaunchOptions`. `--home-root` redirects `~` before anything starts.
let options = LaunchOptions(CommandLine.arguments)
options.applyHomeRoot()

let app = NSApplication.shared
let delegate = AppDelegate(options: options)
app.delegate = delegate
app.setActivationPolicy(.accessory)
app.run()
