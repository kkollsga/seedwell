import AppKit
import Foundation
import SwiftUI
import Testing
@testable import SeedwellApp

// The torrent proxy's shield, its right-click menu and the "SOCKS5 proxy" window (SPEC §7H), driven as their clicks and
// fields drive them, against a mock core in an isolated run: its "NordVPN" servers are the loopback stand-ins its state
// folder names (`qa/proxy-servers.txt`), never NordVPN's. The words and states are the core's (trs-ffi
// `torrent_proxy.rs`, tested there); these check what the host does with them.

/// A mock core of an isolated run whose proxy servers are `servers` on 127.0.0.1 (none: the file is missing).
@MainActor private func isolatedCore(servers: [UInt16] = []) throws -> Core {
    let dir = FileManager.default.temporaryDirectory.appendingPathComponent("seedwell-proxy-\(UUID().uuidString)")
    let qa = dir.appendingPathComponent("state/qa")
    try FileManager.default.createDirectory(at: qa, withIntermediateDirectories: true)
    if !servers.isEmpty {
        try servers.map { "127.0.0.1:\($0)\n" }.joined().write(to: qa.appendingPathComponent("proxy-servers.txt"), atomically: true, encoding: .utf8)
    }
    let options = CoreOptions(configDir: dir.appendingPathComponent("state").path, mock: true, allowLoopbackTrackers: false,
                              forbiddenRoot: dir.appendingPathComponent("home").path)
    return try Core(options: options)
}

/// A loopback SOCKS5 server on 127.0.0.1 that accepts any username and password (`answering`), or one that takes the
/// connection and never answers (a key test then waits for its time limit, 6 s).
private final class FakeSocks: @unchecked Sendable {
    let port: UInt16
    private let fd: Int32

    init(answering: Bool) throws {
        let fd = socket(AF_INET, SOCK_STREAM, 0)
        self.fd = fd
        var addr = sockaddr_in()
        addr.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_addr.s_addr = inet_addr("127.0.0.1")
        var len = socklen_t(MemoryLayout<sockaddr_in>.size)
        let bound = withUnsafePointer(to: &addr) { $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { bind(fd, $0, len) } }
        guard bound == 0, listen(fd, 16) == 0 else { throw POSIXError(.EADDRINUSE) }
        _ = withUnsafeMutablePointer(to: &addr) { $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { getsockname(fd, $0, &len) } }
        port = UInt16(bigEndian: addr.sin_port)
        if answering {
            Thread.detachNewThread {
                while true {
                    let c = accept(fd, nil, nil)
                    if c < 0 { return }
                    Thread.detachNewThread { Self.answer(c); close(c) }
                }
            }
        }
    }

    private static func answer(_ c: Int32) {
        func take(_ n: Int) -> [UInt8]? {
            var buf = [UInt8](repeating: 0, count: n)
            var got = 0
            while got < n {
                let r = buf.withUnsafeMutableBytes { recv(c, $0.baseAddress! + got, n - got, 0) }
                if r <= 0 { return nil }
                got += r
            }
            return buf
        }
        guard let greeting = take(2), take(Int(greeting[1])) != nil else { return }
        _ = send(c, [UInt8(5), 2], 2, 0)
        guard take(1) != nil, let ul = take(1), take(Int(ul[0])) != nil, let pl = take(1), take(Int(pl[0])) != nil else { return }
        _ = send(c, [UInt8(1), 0], 2, 0)
        _ = take(1)  // until the core closes it
    }

    deinit {
        shutdown(fd, SHUT_RDWR)
        close(fd)
    }
}

/// A pasteboard of the test's own: what was copied, and how often it was read.
@MainActor private final class FakePasteboard: ProxyPasteboard {
    private(set) var changeCount = 0
    private var text: String?
    private(set) var reads = 0
    func copy(_ s: String) { text = s; changeCount += 1 }
    func string() -> String? { reads += 1; return text }
}

/// What a click on a menu item does: its action sent to its target (a test runs no menu of its own to track).
@MainActor private func press(_ item: NSMenuItem) {
    _ = (item.target as? NSObject)?.perform(item.action, with: item)
}

@MainActor private func center(_ core: Core, pasteboard: ProxyPasteboard = FakePasteboard()) -> ProxyCenter {
    let c = ProxyCenter(core: core)
    c.drawsWindows = false
    c.pasteboard = pasteboard
    return c
}

@MainActor @Suite(.timeLimit(.minutes(1))) struct ProxyUITests {
    /// Failure: a click on the shield without a key asks the core to switch the proxy on (it refuses) and nothing opens,
    /// so the user has no way to set a key up from the shield.
    @Test func aClickWithoutAKeyOpensTheWindowAndSwitchesNothing() async throws {
        let c = center(try isolatedCore())
        #expect(c.model.icon == .noKey && c.model.clickOpensWindow)
        c.click()
        #expect(c.window != nil, "the window opened")
        #expect(!c.toggling, "no toggle was started")
        await CoreCalls.settled()
        #expect(c.model.icon == .noKey && !c.model.enabled)
        c.closeWindow()
        #expect(c.window == nil && !c.polling, "closed: nothing reads the proxy any more")
    }

    /// Failure: with a key, a click opens the window instead of switching the proxy, or the switch never reaches the
    /// core, or a second click while the first is under way undoes it.
    @Test func aClickWithAKeySwitchesTheProxyOffAndOn() async throws {
        let fake = try FakeSocks(answering: true)
        let core = try isolatedCore(servers: [fake.port])
        _ = try await core.proxySetNordvpnKey(user: "svc-user", pass: "svc-pass")
        let c = center(core)
        #expect(c.model.enabled && !c.model.clickOpensWindow)
        c.click()
        #expect(c.toggling, "the shield waits for the answer")
        c.click()  // ignored while the first runs
        await CoreCalls.settled()
        #expect(!c.toggling && !c.model.enabled && c.model.icon == .off)
        #expect(c.window == nil, "a click with a key opens nothing")
        c.click()
        await CoreCalls.settled()
        #expect(c.model.enabled && c.model.icon == .on)
    }

    /// Failure: the right-click menu lacks the entry that opens the window, or it does not open it; with a key it
    /// lacks the switch; the country entry is a submenu of every country again (taller than the screen), does not
    /// open the list, or the list does not mark the chosen one or does not reach the core.
    @Test func theRightClickMenuOpensTheWindowSwitchesAndChoosesTheCountry() async throws {
        let fake = try FakeSocks(answering: true)
        let core = try isolatedCore(servers: [fake.port])
        let c = center(core)
        var menu = MenuBuilder.menu(c.menuEntries)
        #expect(menu.items.first?.title == c.model.menuOpenLabel, "the model's window entry comes first")
        #expect(!menu.items.contains { $0.title.hasPrefix("Turn ") }, "no switch without a key")
        press(menu.items[0])
        #expect(c.window != nil, "SOCKS5 Proxy… opens the window")
        c.closeWindow()

        _ = try await core.proxySetNordvpnKey(user: "u", pass: "p")
        c.refresh()
        menu = MenuBuilder.menu(c.menuEntries)
        let titles = menu.items.map(\.title)
        #expect(titles.contains(c.model.menuToggleLabel) && !c.model.menuToggleLabel.isEmpty, "\(titles)")
        let countryItem = try #require(menu.items.first { $0.title == c.model.countryMenuLabel })
        #expect(countryItem.submenu == nil, "the country entry opens the list, not a submenu of every country")
        press(countryItem)
        let list = try #require(await opened(c))
        #expect(list.place == .menuBar)
        #expect(list.state.countries[list.state.chosenIndex].code == "", "No preference is chosen")
        let sweden = try #require(list.state.countries.firstIndex { $0.name == "Sweden" })
        list.state.choose(at: sweden)
        #expect(c.countryList == nil, "choosing closes the list")
        await CoreCalls.settled()
        #expect(c.model.country == "SE")
        let label = c.model.countryMenuLabel
        #expect(MenuBuilder.menu(c.menuEntries).items.contains { $0.title == label } && label.contains("Sweden"), "the entry names the chosen country: \(label)")
    }

    /// The list `press` opened (the entry opens it once the menu has closed, on the main queue's next turn).
    private func opened(_ c: ProxyCenter) async -> (place: CountryListPlace, state: CountryListState)? {
        for _ in 0..<100 {
            if let l = c.countryList { return l }
            try? await Task.sleep(for: .milliseconds(10))
        }
        return nil
    }

    /// Failure: the country list shows all its rows (about 60, taller than the screen, the user's complaint) instead
    /// of the model's `country_visible_rows` at the menu's row height; choosing a row, by a click or by the arrows and
    /// Return, does not reach the core's `proxy_set_country`; a typed letter does not jump to a country with it.
    @Test func theCountryListIsAFewRowsTallAndChoosingARowSetsTheCountry() async throws {
        let c = center(try isolatedCore())
        c.openCountryList(at: .window)
        let list = try #require(c.countryList?.state)
        #expect(list.countries.count > list.visibleRows * 4, "a long list (\(list.countries.count)) against a few rows")
        let view = NSHostingView(rootView: CountryListView(state: list))
        let height = view.fittingSize.height
        #expect(abs(height - CGFloat(c.model.countryVisibleRows) * CountryListView.rowHeight) < 0.5,
                "\(height) pt tall, wanted \(c.model.countryVisibleRows) rows of \(CountryListView.rowHeight)")
        #expect(list.chosenIndex == 0, "No preference is chosen")

        // The keys: Down starts at the chosen row, then moves; a letter jumps; Return chooses.
        list.move(1)
        #expect(list.highlighted == 0)
        list.move(1)
        #expect(list.highlighted == 1)
        #expect(list.jump(to: "n"))
        let n = try #require(list.highlighted)
        #expect(list.countries[n].name.hasPrefix("N"), "\(list.countries[n].name)")
        #expect(list.jump(to: "n"))
        #expect(list.highlighted != n && list.countries[list.highlighted!].name.hasPrefix("N"), "the same letter again: the next one")
        let target = list.countries[list.highlighted!].code
        list.chooseHighlighted()
        #expect(c.countryList == nil, "Return closes the list")
        await CoreCalls.settled()
        #expect(c.model.country == target)

        // A click on a row, in a list opened again: scrolled to the new choice.
        c.openCountryList(at: .window)
        let again = try #require(c.countryList?.state)
        #expect(again.countries[again.chosenIndex].code == target, "it opens at the chosen country")
        let sweden = try #require(again.countries.firstIndex { $0.name == "Sweden" })
        again.choose(at: sweden)
        await CoreCalls.settled()
        #expect(c.model.country == "SE")
        // Escape closes it and changes nothing.
        c.openCountryList(at: .settings)
        c.countryList?.state.cancel()
        #expect(c.countryList == nil && c.model.country == "SE")
    }

    /// Failure: a copy fills a field the user already filled, fills the fields while the window is closed or from what
    /// was copied before it opened, or the pasteboard is still read once both fields hold something.
    @Test func copiedCredentialsFillOnlyEmptyFieldsWhileTheWindowIsOpen() throws {
        let pb = FakePasteboard()
        let c = center(try isolatedCore(), pasteboard: pb)
        pb.copy("copied-before")
        c.openWindow()
        let w = try #require(c.window)
        #expect(w.watchingPasteboard)
        w.checkPasteboard()
        #expect(w.username.isEmpty, "what was copied before the window opened is not used")
        pb.copy("svc-user")
        w.checkPasteboard()
        #expect(w.username == "svc-user" && w.password.isEmpty)
        pb.copy("svc-pass")
        w.checkPasteboard()
        #expect(w.username == "svc-user" && w.password == "svc-pass")
        #expect(w.filledFromCopy, "the note says the fields came from a copy")
        let reads = pb.reads
        w.checkPasteboard()
        w.checkPasteboard()
        #expect(pb.reads == reads, "no change, no read: the pasteboard is read only when something new was copied")
        pb.copy("something else")
        w.checkPasteboard()
        #expect(w.username == "svc-user" && w.password == "svc-pass", "filled fields are never changed")
        #expect(pb.reads == reads, "nothing to fill: the pasteboard is not read")

        // A typed username stays, and no copy fills the password then: it would be an unrelated secret.
        w.username = "typed"
        w.password = ""
        pb.copy("pass-2")
        w.checkPasteboard()
        #expect(w.username == "typed" && w.password.isEmpty)
        #expect(!w.filledFromCopy)

        c.closeWindow()
        #expect(!w.watchingPasteboard)
        w.username = ""
        pb.copy("after-close")
        w.checkPasteboard()
        #expect(w.username.isEmpty, "nothing is filled once the window is closed")
    }

    /// Failure: with a key installed the window shows the empty setup form, or a copy fills fields the user cannot see;
    /// Replace Key… does not bring them back, or Cancel leaves what was typed in them.
    @Test func anInstalledKeyHidesTheFormUntilReplaceKey() async throws {
        let fake = try FakeSocks(answering: true)
        let core = try isolatedCore(servers: [fake.port])
        _ = try await core.proxySetNordvpnKey(user: "svc-user", pass: "svc-pass")
        let pb = FakePasteboard()
        let c = center(core, pasteboard: pb)
        c.openWindow()
        let w = try #require(c.window)
        #expect(!w.model.nordvpnInstalled.isEmpty && !w.replacing, "the installed line, not the form")
        pb.copy("someone-else")
        w.checkPasteboard()
        #expect(w.username.isEmpty, "hidden fields take no copy")
        w.replacing = true
        pb.copy("new-user")
        w.checkPasteboard()
        #expect(w.username == "new-user", "Replace Key… shows the fields, and a copy fills them")
        w.cancelReplace()
        #expect(!w.replacing && w.username.isEmpty, "Cancel hides them and forgets what was in them")
        c.closeWindow()
    }

    /// Failure: the menu's shield draws button chrome on hover (a background or bezel behind it, as the first version
    /// did), or no glow at all.
    @Test func theMenuShieldGlowsOnHoverWithoutAButtonBackground() throws {
        let c = center(try isolatedCore())
        let v = MenuProxyShield(center: c)
        v.frame = NSRect(x: 0, y: 0, width: 26, height: 22)
        v.animates = false
        func drawn(hovered: Bool) throws -> NSBitmapImageRep {
            v.hovered = hovered
            let rep = try #require(v.bitmapImageRepForCachingDisplay(in: v.bounds))
            v.cacheDisplay(in: v.bounds, to: rep)
            return rep
        }
        let rest = try drawn(hovered: false)
        let hover = try drawn(hovered: true)
        for (x, y) in [(0, 0), (hover.pixelsWide - 1, 0), (0, hover.pixelsHigh - 1), (hover.pixelsWide - 1, hover.pixelsHigh - 1)] {
            #expect((hover.colorAt(x: x, y: y)?.alphaComponent ?? 0) < 0.05, "a background at the corner (\(x), \(y))")
        }
        var glowing = 0
        for x in 0..<hover.pixelsWide {
            for y in 0..<hover.pixelsHigh where (hover.colorAt(x: x, y: y)?.alphaComponent ?? 0) > (rest.colorAt(x: x, y: y)?.alphaComponent ?? 0) + 0.1 {
                glowing += 1
            }
        }
        #expect(glowing > 20, "the hover draws a glow around the shield (\(glowing) pixels)")
    }

    /// Failure: Enable NordVPN waits for the key test on the main thread (the app hangs for up to 6 s while a server
    /// does not answer), or shows no progress meanwhile, or keeps the password in its field afterwards.
    @Test func theMainThreadNeverWaitsForTheKeyTest() async throws {
        let silent = try FakeSocks(answering: false)
        let c = center(try isolatedCore(servers: [silent.port]))
        c.openWindow()
        let w = try #require(c.window)
        w.username = "svc-user"
        w.password = "svc-pass"
        let clock = ContinuousClock()
        let start = clock.now
        w.enable()
        #expect(w.busy)
        #expect(c.busy, "the shield and the switch wait too")
        // The main thread answers again while the core still tests the key (the test waits up to 6 s for a server
        // that never answers): had it waited for the call, the call would be over by the time it answers. Other
        // suites run on the main thread at the same time, so only "before the call ends" is asked, not how soon.
        try await Task.sleep(for: .milliseconds(100))
        w.refresh()
        #expect(w.busy, "the main thread answered only after the key test ended (\(start.duration(to: clock.now)))")
        // The progress shows while the call runs (once the call has reached the core: under a full test run the
        // cooperative pool it starts from may be busy for a moment).
        var sawProgress = false
        let end = clock.now + .seconds(20)
        while w.busy, clock.now < end {
            w.refresh()
            if w.statusLine?.progress == true { sawProgress = true }
            try await Task.sleep(for: .milliseconds(50))
        }
        #expect(sawProgress, "the progress never showed")
        #expect(!w.busy)
        #expect(w.statusLine?.problem == true, "no server answered: said in red")
        #expect(w.password.isEmpty && w.username == "svc-user", "the password is emptied after the call, the username stays")
        c.closeWindow()
    }
}
