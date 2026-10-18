import Foundation
import Testing
@testable import SeedwellApp

// The path monitor's chatter before the engine hears of it (SPEC 7C.4c): a 3 s settle, one delivery
// per real change, nothing for a launch that starts online.

@MainActor
private final class Clock {
    var now: TimeInterval = 0
    private var due: [(at: TimeInterval, id: Int, work: @MainActor () -> Void)] = []
    private var next = 0
    var delivered: [Bool] = []

    func schedule(_ delay: TimeInterval, _ work: @escaping @MainActor () -> Void) -> () -> Void {
        next += 1
        let id = next
        due.append((now + delay, id, work))
        return { [weak self] in self?.due.removeAll { $0.id == id } }
    }

    func advance(_ s: TimeInterval) {
        now += s
        for item in due.filter({ $0.at <= now }).sorted(by: { $0.at < $1.at }) {
            due.removeAll { $0.id == item.id }
            item.work()
        }
    }

    func make() -> PathDebounce {
        PathDebounce(delay: 3, schedule: { [unowned self] d, w in self.schedule(d, w) }, deliver: { [unowned self] in self.delivered.append($0) })
    }
}

private func path(_ ok: Bool, _ ifaces: String = "en0") -> PathDebounce.Path { .init(satisfied: ok, interfaces: ifaces) }

@Suite("Path monitor debounce")
@MainActor
struct PathDebounceTests {
    @Test func aLaunchThatStartsOnlineTellsTheEngineNothing() {
        let c = Clock(); let d = c.make()
        d.report(path(true))
        c.advance(10)
        #expect(c.delivered.isEmpty)
    }

    @Test func aLossIsDeliveredOnlyAfterItStoodStillForThreeSeconds() {
        let c = Clock(); let d = c.make()
        d.report(path(true))
        d.report(path(false))
        c.advance(2.9)
        #expect(c.delivered.isEmpty)
        c.advance(0.2)
        #expect(c.delivered == [false])
    }

    @Test func aFlappingInterfaceDeliversNothingWhenItEndsWhereItStarted() {
        let c = Clock(); let d = c.make()
        d.report(path(true))
        d.report(path(false)); c.advance(1)
        d.report(path(true)); c.advance(1)
        d.report(path(false)); c.advance(1)
        d.report(path(true)); c.advance(10)
        #expect(c.delivered.isEmpty, "back to what the engine already knew")
    }

    @Test func aLossAndARecoveryAreTwoDeliveries() {
        let c = Clock(); let d = c.make()
        d.report(path(true))
        d.report(path(false)); c.advance(3.1)
        d.report(path(true)); c.advance(3.1)
        #expect(c.delivered == [false, true])
    }

    @Test func aNewInterfaceWhileOnlineIsOneDeliveryAndTheSameOneAgainIsNone() {
        let c = Clock(); let d = c.make()
        d.report(path(true, "en0"))
        d.report(path(true, "en1")); c.advance(3.1)
        #expect(c.delivered == [true])
        d.report(path(true, "en1")); c.advance(10)
        #expect(c.delivered == [true])
    }

    @Test func launchingOfflineIsDelivered() {
        let c = Clock(); let d = c.make()
        d.report(path(false))
        c.advance(3.1)
        #expect(c.delivered == [false])
    }
}
