import Foundation
import IOKit.ps

/// "On battery" setting (SPEC §8.4): keep going, apply the Limit Speed limits, or pause downloads.
/// Follows the power source with a run-loop source, so nothing polls.
@MainActor final class PowerWatcher {
    private let core: Core
    var mode: OnBatteryMode = .limit { didSet { if mode != oldValue { apply() } } }
    private var source: CFRunLoopSource?
    private(set) var onBattery = false
    /// We switched Limit Speed on ourselves, so we switch it off again (and only then).
    private var limitedByUs = false
    private var pausedByUs: [String] = []

    init(core: Core) {
        self.core = core
        onBattery = Self.readOnBattery()
        let ctx = Unmanaged.passUnretained(self).toOpaque()
        source = IOPSNotificationCreateRunLoopSource({ ctx in
            guard let ctx else { return }
            let me = Unmanaged<PowerWatcher>.fromOpaque(ctx).takeUnretainedValue()
            DispatchQueue.main.async { MainActor.assumeIsolated { me.changed() } }
        }, ctx).takeRetainedValue()
        CFRunLoopAddSource(CFRunLoopGetMain(), source, .commonModes)
    }

    isolated deinit {
        if let source { CFRunLoopRemoveSource(CFRunLoopGetMain(), source, .commonModes) }
    }

    private static func readOnBattery() -> Bool {
        guard let kind = IOPSGetProvidingPowerSourceType(nil)?.takeRetainedValue() as String? else { return false }
        return kind == kIOPSBatteryPowerValue
    }

    private func changed() {
        let now = Self.readOnBattery()
        guard now != onBattery else { return }
        onBattery = now
        apply()
    }

    func apply() {
        if onBattery {
            switch mode {
            case .keepGoing: undo()
            case .limit:
                if !core.snapshot().altSpeed { core.setAltSpeed(on: true); limitedByUs = true }
            case .pause:
                let ids = core.snapshot().downloading.filter { $0.state == .downloading && !$0.awaitingChoice }.map(\.id)
                if !ids.isEmpty { core.pause(ids: ids); pausedByUs += ids }
            }
        } else {
            undo()
        }
    }

    private func undo() {
        if limitedByUs { core.setAltSpeed(on: false); limitedByUs = false }
        if !pausedByUs.isEmpty { core.resume(ids: pausedByUs); pausedByUs = [] }
    }
}
