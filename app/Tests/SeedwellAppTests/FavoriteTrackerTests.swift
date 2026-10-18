import Foundation
import Testing
@testable import SeedwellApp

// Favorite trackers (SPEC 7E.1, 8.3): the sidebar order and the inline editing of a favorite.

private func t(_ id: String, _ name: String, _ count: UInt32, fav: Bool = false) -> TrackerSummary {
    TrackerSummary(id: id, name: name, count: count, favorite: fav)
}

/// What the core does with a line: drops a query and secret-looking path segments.
private func check(_ raw: String) -> UrlCheck {
    if raw.contains("!") { return UrlCheck(pattern: nil, passkeyRemoved: false, error: "Not a tracker address") }
    if raw.contains("?") {
        let base = String(raw.split(separator: "?")[0])
        return UrlCheck(pattern: base, passkeyRemoved: true, error: nil)
    }
    return UrlCheck(pattern: raw.lowercased(), passkeyRemoved: false, error: nil)
}

@Suite("Tracker order")
struct TrackerOrderTests {
    @Test func favoritesThenBusiestThenPublic() {
        let list = [
            t("public", "Public", 40),
            t("debian.org", "Debian", 3),
            t("zeta.org", "Zeta", 30, fav: true),
            t("other-private", "Other private", 1),
            t("127.0.0.1", "127.0.0.1", 9),
            t("alpha.org", "alpha", 1, fav: true),
            t("b.test", "Bee", 3),
        ]
        #expect(TrackerOrder.sorted(list).map(\.id) == ["alpha.org", "zeta.org", "127.0.0.1", "b.test", "debian.org", "other-private", "public"])
    }

    @Test func equalCountsFallBackToTheName() {
        let list = [t("b.org", "Bravo", 2), t("a.org", "alpha", 2), t("c.org", "Charlie", 2)]
        #expect(TrackerOrder.sorted(list).map(\.name) == ["alpha", "Bravo", "Charlie"])
    }

    @Test func publicIsLastEvenWhenItIsTheBusiest() {
        #expect(TrackerOrder.sorted([t("public", "Public", 99), t("a.org", "A", 0)]).last?.id == "public")
    }
}

@Suite("Favorite addresses")
struct FavoriteURLsTests {
    @Test func addingAppendsTheStoredForm() {
        let r = FavoriteURLs.commit(["tracker.a.org"], index: nil, text: "  Tracker.B.org ", check: check)
        #expect(r == .saved(["tracker.a.org", "tracker.b.org"], passkeyRemoved: false))
    }

    @Test func aPasskeyIsDroppedAndReported() {
        let r = FavoriteURLs.commit([], index: nil, text: "https://t.org/announce?passkey=SECRET", check: check)
        #expect(r == .saved(["https://t.org/announce"], passkeyRemoved: true))
        if case .saved(let list, _) = r { #expect(!list.joined().contains("SECRET")) }
    }

    @Test func editingReplacesTheLine() {
        let r = FavoriteURLs.commit(["a.org", "b.org"], index: 1, text: "c.org", check: check)
        #expect(r == .saved(["a.org", "c.org"], passkeyRemoved: false))
    }

    @Test func theSameTextIsNothing() {
        #expect(FavoriteURLs.commit(["a.org"], index: 0, text: "a.org", check: check) == .unchanged)
        #expect(FavoriteURLs.commit(["a.org"], index: nil, text: "   ", check: check) == .unchanged)
    }

    @Test func anAddressAlreadyThereIsNotAddedTwice() {
        #expect(FavoriteURLs.commit(["a.org", "b.org"], index: nil, text: "A.org", check: check) == .duplicate)
        #expect(FavoriteURLs.commit(["a.org", "b.org"], index: 1, text: "a.org", check: check) == .duplicate)
    }

    @Test func invalidLinesStayOpenWithAHint() {
        #expect(FavoriteURLs.commit(["a.org"], index: nil, text: "bad!", check: check) == .invalid("Not a tracker address"))
        #expect(FavoriteURLs.commit(["a.org"], index: 0, text: "", check: check) == .invalid("Enter a tracker address"))
    }

    @Test func removingTheLastAddressAsks() {
        #expect(FavoriteURLs.removing(["a.org", "b.org"], at: 0) == .removed(["b.org"]))
        #expect(FavoriteURLs.removing(["a.org"], at: 0) == .lastOne)
        #expect(FavoriteURLs.removing(["a.org"], at: 5) == .removed(["a.org"]))
    }
}

/// A core stand-in holding the profiles in memory.
private final class FakeStore: FavoriteStore {
    var profiles: [ProfileRecord]
    var rules: UInt32 = 0
    init(_ p: [ProfileRecord]) { profiles = p }
    func profilesLoad() -> [ProfileRecord] { profiles }
    func profilesSave(profiles p: [ProfileRecord]) throws -> [ProfileRecord] { profiles = p; return p }
    func urlCheck(raw: String) -> UrlCheck { check(raw) }
    func favoriteAdd(groupId: String) throws -> ProfileRecord {
        let r = ProfileRecord(id: "fav", name: "Fav", urls: [groupId], rule: nil, color: nil, icon: nil, downLimit: nil, upLimit: nil)
        profiles.append(r)
        return r
    }
    func favoriteRuleCount(id: String) -> UInt32 { rules }
}

private func rec(_ id: String, _ name: String, _ urls: [String]) -> ProfileRecord {
    ProfileRecord(id: id, name: name, urls: urls, rule: nil, color: nil, icon: nil, downLimit: nil, upLimit: nil)
}

@MainActor @Suite("Favorite editing")
struct FavoriteEditingTests {
    @Test func renameCommitsOnReturnAndCancelKeepsTheName() {
        let s = FakeStore([rec("tl", "Example", ["t.example.org"])])
        let e = FavoriteEditing(store: s)
        e.beginRename("tl")
        #expect(e.target == .sidebarName("tl") && e.text == "Example")
        e.text = "TL"
        e.cancel()
        #expect(s.profiles[0].name == "Example" && e.target == nil)
        e.beginRename("tl")
        e.text = "  My TL "
        e.commit()
        #expect(s.profiles[0].name == "My TL" && e.target == nil)
    }

    @Test func speedCapsAreStoredOnTheFavoriteAndZeroClearsThem() {
        let s = FakeStore([rec("tl", "TL", ["t.example.org"])])
        let e = FavoriteEditing(store: s)
        e.setLimits("tl", down: 5_000_000, up: 250_000)
        #expect(s.profiles[0].downLimit == 5_000_000 && s.profiles[0].upLimit == 250_000)
        e.setLimits("tl", down: 0, up: 250_000)
        #expect(s.profiles[0].downLimit == nil && s.profiles[0].upLimit == 250_000)
        e.setLimits("nope", down: 1, up: 1)
        #expect(s.profiles.count == 1)
    }

    @Test func anEmptyNameIsACancel() {
        let s = FakeStore([rec("tl", "TL", ["t.example.org"])])
        let e = FavoriteEditing(store: s)
        e.beginRename("tl")
        e.text = "   "
        e.commit()
        #expect(s.profiles[0].name == "TL" && e.target == nil)
    }

    @Test func addingAnAddressSavesItAndNotesAPasskey() {
        let s = FakeStore([rec("tl", "TL", ["t.example.org"])])
        let e = FavoriteEditing(store: s)
        e.beginAddURL("tl")
        #expect(e.target == .url("tl", nil))
        e.text = "https://u.example.org/announce?passkey=SECRET"
        e.commit()
        #expect(s.profiles[0].urls == ["t.example.org", "https://u.example.org/announce"])
        #expect(e.note == FavoriteEditing.passkeyNote && e.target == nil)
        #expect(!s.profiles[0].urls.joined().contains("SECRET"))
    }

    @Test func aBadLineKeepsTheFieldAndShowsTheHint() {
        let s = FakeStore([rec("tl", "TL", ["t.example.org"])])
        let e = FavoriteEditing(store: s)
        e.beginAddURL("tl")
        e.text = "no!"
        e.commit()
        #expect(e.target == .url("tl", nil) && e.hint == "Not a tracker address")
        e.text = "t.example.org"
        e.commit()
        #expect(e.hint == "Already in the list" && s.profiles[0].urls == ["t.example.org"])
        e.cancel()
        #expect(e.hint == nil)
    }

    @Test func editingAndRemovingAnAddress() {
        let s = FakeStore([rec("tl", "TL", ["a.org", "b.org"])])
        let e = FavoriteEditing(store: s)
        e.beginEditURL("tl", index: 0)
        #expect(e.text == "a.org")
        e.text = "c.org"
        e.commit()
        #expect(s.profiles[0].urls == ["c.org", "b.org"])
        e.removeURL("tl", index: 1)
        #expect(s.profiles[0].urls == ["c.org"] && e.removal == nil)
        e.removeURL("tl", index: 0)
        #expect(s.profiles[0].urls == ["c.org"], "the last address asks first")
        #expect(e.removal?.lastAddress == true)
    }

    @Test func removingAFavoriteWarnsAboutItsRules() {
        let s = FakeStore([rec("tl", "TL", ["a.org"]), rec("x", "X", ["x.org"])])
        s.rules = 2
        let e = FavoriteEditing(store: s)
        var gone: [String] = []
        e.gone = { gone.append($0) }
        e.askRemove("tl")
        #expect(e.removal?.message == "Remove “TL” from Favorites? 2 rules use it.")
        e.cancelRemoval()
        #expect(s.profiles.count == 2)
        s.rules = 1
        e.askRemove("tl")
        #expect(e.removal?.message.hasSuffix("1 rule uses it.") == true)
        e.confirmRemoval()
        #expect(s.profiles.map(\.id) == ["x"] && gone == ["tl"] && e.removal == nil)
    }

    @Test func saveAsFavoriteOpensItsTab() {
        let s = FakeStore([])
        let e = FavoriteEditing(store: s)
        var opened: [String] = []
        e.openTab = { opened.append($0) }
        e.saveAsFavorite("debian.org")
        #expect(opened == ["fav"] && s.profiles.count == 1)
    }
}

@Suite("Tracker facts")
struct TrackerFactsTests {
    private func line(_ url: String, _ status: String, last: UInt64? = nil, next: UInt64? = nil) -> TrackerLine {
        TrackerLine(url: url, tier: 0, status: status, lastAnnounce: last, nextAnnounce: next, seeders: nil, leechers: nil)
    }

    @Test func hostsOfMaskedUrls() {
        #expect(TrackerFacts.host(of: "https://Tracker.Example.org:443/a/••••••/announce") == "tracker.example.org")
        #expect(TrackerFacts.host(of: "udp://[::1]:6969/announce") == "::1")
        #expect(TrackerFacts.host(of: "http://127.0.0.1:7101/announce?x=1") == "127.0.0.1")
    }

    @Test func belongingToAnAutomaticTrackerOrAFavorite() {
        #expect(TrackerFacts.belongs("http://bt.debian.org/announce", hosts: [], domain: "debian.org"))
        #expect(!TrackerFacts.belongs("http://notdebian.org/announce", hosts: [], domain: "debian.org"))
        #expect(TrackerFacts.belongs("http://t.example.org/a/••••••/announce", hosts: ["t.example.org", "u.example.org/announce"], domain: nil))
        #expect(TrackerFacts.belongs("http://x.example.org/announce", hosts: ["*.example.org"], domain: nil))
        #expect(!TrackerFacts.belongs("http://other.org/announce", hosts: ["t.example.org"], domain: nil))
    }

    @Test func summaryTakesTheNewestAnnounceTheMeanIntervalAndTheFirstProblem() {
        let f = TrackerFacts.summarize([
            line("a", "announced", last: 1000, next: 2800),
            line("b", "announced", last: 1500, next: 2300),
            line("c", "failed: unreachable"),
        ])
        #expect(f.lastAnnounce == 1500 && f.intervalSecs == 1300 && f.problem == "failed: unreachable" && f.sampled == 3)
        #expect(TrackerFacts.summarize([]) == TrackerFacts())
    }

    @Test func spans() {
        #expect(TrackerInfoPage.span(45) == "45 s" && TrackerInfoPage.span(1800) == "30 min")
        #expect(TrackerInfoPage.span(7200) == "2 h" && TrackerInfoPage.span(259_200) == "3 days")
    }
}

@Suite("Sidebar shows only trackers with torrents")
struct SidebarVisibleTrackersTests {
    @Test func emptyTrackersAreHiddenUnlessOpen() {
        let list = [t("public", "Public", 0), t("debian.org", "Debian", 2), t("fav", "Fav", 0, fav: true)]
        #expect(TrackerOrder.visible(list, selected: nil).map(\.id) == ["debian.org"])
        #expect(TrackerOrder.visible(list, selected: "public").map(\.id) == ["debian.org", "public"])
    }
}
