import Foundation
import Sparkle

/// Sparkle 2 behind two settings (Settings > General). The updater only starts when the build carries an
/// EdDSA public key (`SUPublicEDKey`, set by build.sh from SEEDWELL_SPARKLE_PUBLIC_KEY); without one the
/// section says so and nothing is started, so a default build never touches the network. Automatic checks
/// are off in Info.plist (`SUEnableAutomaticChecks`) until the user switches them on.
@MainActor @Observable final class UpdateSettings {
    static let shared = UpdateSettings()

    private var controller: SPUStandardUpdaterController?
    private let delegate = Relaunch()
    /// Called just before Sparkle relaunches the app: quitting is allowed then (Quit only closes windows otherwise).
    var willRelaunch: () -> Void {
        get { delegate.run }
        set { delegate.run = newValue }
    }
    /// False in builds without a Sparkle key (ad-hoc and development builds).
    let configured: Bool

    var automaticChecks: Bool {
        didSet { controller?.updater.automaticallyChecksForUpdates = automaticChecks }
    }

    private init() {
        let key = (Bundle.main.object(forInfoDictionaryKey: "SUPublicEDKey") as? String)?
            .trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        configured = !key.isEmpty && Bundle.main.object(forInfoDictionaryKey: "SUFeedURL") != nil
        automaticChecks = false
    }

    /// Called once at launch. Starts Sparkle only when configured; its first scheduled check happens
    /// only if the user turned automatic checks on.
    func start() {
        guard configured, controller == nil else { return }
        let c = SPUStandardUpdaterController(startingUpdater: true, updaterDelegate: delegate, userDriverDelegate: nil)
        controller = c
        automaticChecks = c.updater.automaticallyChecksForUpdates
    }

    var canCheckNow: Bool { controller?.updater.canCheckForUpdates ?? false }
    func checkNow() { controller?.checkForUpdates(nil) }
}

/// Sparkle asks to quit the app to install an update; that quit has to go through.
private final class Relaunch: NSObject, SPUUpdaterDelegate, @unchecked Sendable {
    nonisolated(unsafe) var run: () -> Void = {}

    func updaterWillRelaunchApplication(_ updater: SPUUpdater) {
        MainActor.assumeIsolated { run() }
    }

    func updater(_ updater: SPUUpdater, willInstallUpdate item: SUAppcastItem) {
        MainActor.assumeIsolated { run() }
    }
}
