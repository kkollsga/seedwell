import Foundation
import IOKit.ps

/// Tells the core the power source (SPEC §8.4): at start and at every change, with a run-loop source, so
/// nothing polls. The core's "On battery" rule decides what that means (keep going, Limit Speed, pause
/// downloads, and what to undo on AC power).
@MainActor final class PowerWatcher {
    private let core: Core
    private var source: CFRunLoopSource?
    private var onBattery: Bool

    init(core: Core) {
        self.core = core
        onBattery = Self.readOnBattery()
        core.powerChanged(onBattery: onBattery)
        let ctx = Unmanaged.passUnretained(self).toOpaque()
        source = IOPSNotificationCreateRunLoopSource({ ctx in
            guard let ctx else { return }
            let me = Unmanaged<PowerWatcher>.fromOpaque(ctx).takeUnretainedValue()
            DispatchQueue.main.async { MainActor.assumeIsolated { me.report(PowerWatcher.readOnBattery()) } }
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

    /// A power source as macOS says it (QA: `power battery|ac` says it instead).
    func report(_ now: Bool) {
        guard now != onBattery else { return }
        onBattery = now
        core.powerChanged(onBattery: now)
    }
}
