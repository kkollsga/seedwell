import Foundation
import Testing
@testable import SeedwellApp

// A row's commands (SPEC §8.5) are the core's list (`rowCommands`, tested in trs-ffi's row_commands); the app draws
// it as menu entries and runs each command. These check the drawing and the running against a mock core.

@MainActor @Suite("Row commands")
struct RowCommandsTests {
    /// One mock core for the suite (it holds its state directory open).
    static let core: Core? = {
        let dir = NSTemporaryDirectory() + "seedwell-row-commands-\(getpid())"
        try? FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        return try? Core(options: CoreOptions(configDir: dir, mock: true, allowLoopbackTrackers: false, forbiddenRoot: nil))
    }()

    static func titles(_ entries: [MenuEntry]) -> [String] {
        entries.map { e in
            switch e {
            case .separator: "-"
            case let .item(title, checked, enabled, role, key, _):
                title + (checked ? " ✓" : "") + (enabled ? "" : " (off)") + (role == .destructive ? " (red)" : "") + (key.map { " " + $0 } ?? "")
            case let .submenu(title, _): "\(title) >"
            }
        }
    }

    @Test func aDownloadsMenuIsTheCoresListWithFinderTheShortcutAndTheRedRemove() throws {
        let core = try #require(Self.core)
        PlayerDirectory.shared.override = []
        defer { PlayerDirectory.shared.override = nil }
        let commands = Commands(core: core)
        let row = try #require(core.snapshot().allDownloading.first { !$0.isHeld && $0.state == .downloading })
        let window = Self.titles(commands.entries(for: [row]))
        #expect(window.contains("Show in Finder"))
        #expect(window.contains("Get Info ⌘I"))
        #expect(window.contains("Move to…"))
        #expect(window.contains("Move to Top"))
        #expect(window.last == "Remove and Delete Files… (red)")
        let menuBar = Self.titles(commands.entries(for: [row], inMenuBar: true))
        #expect(menuBar.contains("Move to Top of Queue") && !menuBar.contains("Move to…"))
        #expect(commands.entries(for: []).isEmpty)
    }

    @Test func aNoteIsADisabledItemAndSubmenusAreDrawnWhole() throws {
        let core = try #require(Self.core)
        let commands = Commands(core: core)
        let list: [CommandEntry] = [
            .submenu(title: "Category: Movie", entries: [
                .item(command: .setCategory(category: .tv), title: "TV", checked: true, enabled: true, destructive: false, shortcut: nil),
                .separator,
                .note(title: "Detected: Movie"),
            ]),
        ]
        let drawn = commands.draw(list, rows: [], videos: [], inMenuBar: false)
        guard case let .submenu(title, children) = drawn.first else { Issue.record("no submenu"); return }
        #expect(title == "Category: Movie")
        #expect(Self.titles(children) == ["TV ✓", "-", "Detected: Movie (off)"])
    }

    @Test func pauseFromTheMenuPausesTheRowInTheCore() throws {
        let core = try #require(Self.core)
        PlayerDirectory.shared.override = []
        defer { PlayerDirectory.shared.override = nil }
        let commands = Commands(core: core)
        let row = try #require(core.snapshot().allDownloading.first { !$0.isHeld && $0.state == .downloading })
        let pause = commands.entries(for: [row]).compactMap { e -> (@MainActor () -> Void)? in
            if case let .item("Pause", _, _, _, _, run) = e { return run } else { return nil }
        }.first
        try #require(pause != nil)
        pause?()
        #expect(try core.info(id: row.id).state == .paused)
        core.resume(ids: [row.id])
    }
}
