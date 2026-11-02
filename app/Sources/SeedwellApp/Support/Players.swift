import AppKit

// Play while downloading (SPEC §7F.3): starting a video player. Which players this Mac has, which one plays a file and
// how each is handed a stream are the core's (`Core.play`, trs_platform::players); this runs the launch it decides.

enum PlayerLauncher {
    /// QA runs set this: no Play launches anything (except the one explicit `play-app` test).
    @MainActor static var blocked = false
    /// QA: told the file or URL a blocked launch would have handed to the player, so a script can fetch it.
    @MainActor static var onBlockedLaunch: ((String) -> Void)?

    /// What the launch hands the player, as a URL (a file's `file://`).
    static func target(_ launch: PlayLaunch) -> String {
        switch launch {
        case let .openFile(_, path): URL(fileURLWithPath: path).absoluteString
        case let .openUrl(_, url): url
        case let .run(_, arguments): arguments.joined(separator: " ")
        }
    }

    /// Starts the player. Calls `done` with an error text when LaunchServices refused.
    @MainActor static func start(_ launch: PlayLaunch, player: String, done: @escaping @MainActor (String?) -> Void = { _ in }) {
        if blocked {
            onBlockedLaunch?(target(launch))
            return done("QA: launching players is blocked")
        }
        let config = NSWorkspace.OpenConfiguration()
        config.activates = true
        let finished: @Sendable (NSRunningApplication?, Error?) -> Void = { _, error in
            let text = error.map { playLaunchFailed(player: player, error: $0.localizedDescription) }
            DispatchQueue.main.async { MainActor.assumeIsolated { done(text) } }
        }
        switch launch {
        case let .openFile(app, path):
            NSWorkspace.shared.open([URL(fileURLWithPath: path)], withApplicationAt: URL(fileURLWithPath: app), configuration: config,
                                    completionHandler: finished)
        case let .openUrl(app, url):
            guard let u = URL(string: url) else { return done(playLaunchFailed(player: player, error: url)) }
            NSWorkspace.shared.open([u], withApplicationAt: URL(fileURLWithPath: app), configuration: config, completionHandler: finished)
        case let .run(app, arguments):
            config.arguments = arguments
            NSWorkspace.shared.openApplication(at: URL(fileURLWithPath: app), configuration: config, completionHandler: finished)
        }
    }
}
