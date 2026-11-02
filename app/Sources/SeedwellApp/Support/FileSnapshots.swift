import AppKit
import SwiftUI

// Debug-only visual QA (`--render-snapshots <dir>`, see SnapshotRenderer): the file list under a row of the window, the
// menu's inline checklist, the chooser, the held torrent's list and the starting notifier, from the mock library and
// the core's sample chooser (`chooserSample`: a season pack, two seasons in the library, the rest to choose, Extras).
//
// Files: files-window-*, files-menu-*, chooser-sample-*, held-list-*, starting-*, starting-open-* (dark and light).
extension SnapshotRenderer {
    func renderFileLists() async {
        for dark in [true, false] {
            let suffix = dark ? "dark" : "light"
            await renderWindowList(suffix: suffix, dark: dark)
            await renderMenuList(suffix: suffix, dark: dark)
            await renderChoices(suffix: suffix, dark: dark)
        }
    }

    /// The Downloads list with a torrent's files open under it, its second file row selected.
    private func renderWindowList(suffix: String, dark: Bool) async {
        let m = model(.downloads, select: nil)
        guard let row = (m.snapshot.allDownloading + m.snapshot.allQueued).first(where: { $0.fileCount > 1 }) else { return }
        let files = FilesStore.shared
        files.expand(row.id)
        await Self.until { files.views[row.id]?.rows.isEmpty == false }
        if let node = files.views[row.id]?.rows.dropFirst().first?.file.id {
            files.select(torrent: row.id, node: node, command: false, shift: false)
        }
        await render("files-window-\(suffix)", size: NSSize(width: 1180, height: 820), dark: dark) {
            MainWindowView(model: m, commands: commands, settings: settings, nav: SettingsNavigation())
        }
        files.collapseAll()
    }

    /// The menu bar menu with the inline checklist of a downloading torrent open.
    private func renderMenuList(suffix: String, dark: Bool) async {
        let controller = StatusMenuController(core: core, commands: commands, settings: settings, inMenuBar: false)
        _ = controller.qaBuildItems()
        guard let row = (core.snapshot().allDownloading + core.snapshot().allQueued).first(where: { $0.fileCount > 1 }),
              controller.menuFiles.qaToggle(row.displayName, items: controller.qaItems) else { return }
        let panel = MenuPanel(items: controller.qaItems, minWidth: 220)
        await capture("files-menu-\(suffix)", dark: dark, panels: [(panel, .zero)],
                      caption: "Menu bar menu with \(row.displayName)'s file list open. Real item views: torrent rows, the file list, Open/Quit. Stand-ins: headers, separators, plain rows, the panel.") {}
        controller.menuFiles.collapse()
    }

    /// The sample chooser (held), its list in the window, and the notifier with a stopped countdown, the sample
    /// waiting for a choice and a magnet link fetching its files; then the sample alone, its checklist open.
    private func renderChoices(suffix: String, dark: Bool) async {
        let choices = Choices.sample()
        let store = ChoiceStore()
        store.choices = choices
        guard let c = await store.load("sample") else { return }
        // The notifier's Cancel caption needs the `.torrent` file looked at (the core's polls keep what it found).
        _ = await store.hasSourceFile("sample")
        await render("chooser-sample-\(suffix)", size: NSSize(width: 640, height: 720), dark: dark, settings: true) {
            ChooserView(chooser: c, hasSourceFile: true)
        }
        await render("held-list-\(suffix)", size: NSSize(width: 760, height: 360), dark: dark, settings: true) {
            HeldFilesBlock(id: "sample", store: store).frame(width: 760, height: 360, alignment: .topLeading).background(.background)
        }
        let rows = core.snapshot().allDownloading + core.snapshot().allQueued
        guard rows.count >= 3 else { return }
        var countdown = rows[0]
        countdown.pending = PendingStart(startsAtMs: 0, remainingMs: 7_000, paused: true)
        countdown.awaitingChoice = false
        var held = rows[1]
        held.id = "sample"
        held.awaitingChoice = true
        held.pending = nil
        var magnet = rows[2]
        magnet.pending = nil
        magnet.awaitingChoice = false
        magnet.sizeBytes = 0
        let all = StartingModel()
        let starting = Starting(core: core, choices: choices)
        starting.request(id: magnet.id)
        all.set(starting.refresh(rows: [countdown, held, magnet], nowMs: Date().unixMs, launch: false))
        await render("starting-\(suffix)", size: NSSize(width: StartingView.width, height: 460), dark: dark, settings: true) {
            StartingView(model: all, store: store, actions: StartingActions())
        }
        let one = StartingModel()
        let alone = Starting(core: core, choices: choices)
        _ = alone.refresh(rows: [held], nowMs: Date().unixMs, launch: false)
        alone.expand(id: "sample")
        one.set(alone.pageNow(nowMs: Date().unixMs))
        await render("starting-open-\(suffix)", size: NSSize(width: StartingView.width, height: 560), dark: dark, settings: true) {
            StartingView(model: one, store: store, actions: StartingActions())
        }
    }

    /// Waits (up to two seconds) until `done` holds: a list loads off the main thread.
    static func until(_ done: () -> Bool) async {
        for _ in 0..<40 where !done() { try? await Task.sleep(for: .milliseconds(50)) }
    }
}

