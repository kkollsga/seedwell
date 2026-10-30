import AppKit

/// QA only (`--qa-control <file>`): runs scene commands appended to a text file, one per line. A run with this flag is
/// quiet (QuietRun): the app never becomes active or frontmost, never takes the focus or a key, and none of its windows
/// is on a screen; `shot` draws them in-process, so no screen capture, awake display or Screen Recording permission is
/// needed. `<file>.ack` holds the number of lines done, written once their effects are applied and laid out (a `shot`
/// or `where` right after sees them); reports go to `<file>.bench`. Every click or key is the QA command that makes the
/// call it makes: no event is ever posted (AGENTS.md).
///
///     shot <name> [main|settings|info|chooser|popover|menu|alert|window <id>]   <name>.png (a bare name: in the folder of
///                                     <file>; a path ending in .png: there) drawn at 2x, see QAShots
///     wait <command> ~ <text> [timeout <s>] | wait <command> !~ <text> [timeout <s>]   answered once the command's report
///                                     holds (or no longer holds) the text, or after the timeout (default 15 s): its last
///                                     report line, or "wait: timeout after <s> s: <last line>"
///     windows                         the open windows: main (sheet, info), chooser, popover, menu, alert, chat-new, sync-…
///     config                          "config: reloads <n>.": how often the engine applied an edited settings file
///     window open | window close | window size 1180x820
///     section downloads | section sharing | tab downloads|sharing|tracker <name or id> | tracker <id> | tracker none | search <text> | select <id>
///     info <id> | info none | info-scroll top|bottom|<0...1> | getinfo <name>   (Get Info as the context menu asks for it)
///     getinfo-window                  the open Get Info popover: whose row it shows, a child of the main window, has the keys
///     getinfo-card                    the open Get Info card's lines, trackers and buttons, as the core drew them up
///     policy | dock-quit [logout] | quit-menu | where   (Dock icon and Quit paths; policy: the Dock icon the app would have)
///     settings <tab> | settings close | tracker-edit <id>
///     appearance dark | light | system
///     chooser <n> | chooser-category <name> | chooser-close | scroll top|bottom|<0...1> | settings-scroll top|bottom|<0...1>
///     menu open | menu close          the menu bar menu (a quiet run opens and refreshes it without showing it)
///     menu-sync | menu-sync edit|remove <name>   the Folder Sync submenu as drawn (`menu open` first); a row's Edit / Remove button
///     menu-hover open|quit|none | menu-press open|quit | menu-titles | menu-subtitles | menu-files …   the open menu's
///                                     footer, titles, the rows' subtitles, file lists
///     contextmenu <name> [menubar] [playwith] | contextmenu close   a row's context menu, "open" for `shot … menu`
///     starting show | close | state | pause|resume|now|choose|ok|cancel|all|none|toggle|open|fold [name]   the "Starting in …" notifier popover
///     starting cancel [name] keep     ⇧-Cancel (keeps the .torrent file);  starting shift on|off   force the Shift state of the Cancel buttons
///     starting title [name]           a click on the title (opens the torrent in the main window); starting choose = the chevron
///     starting category <label>       the pill's menu: change the first held torrent's category (Movie, TV, Music, Audiobook, Book, App, Game, Other)
///     starting pill [name] | starting pill close   the pill's menu as a native menu (shown only; `starting category` picks)
///     choice tick <title> | fold <title> | all | pick <All|Latest season|What I don't have> | state
///                                     a checklist click on the shared chooser model (chooser window, popover, main window): a box
///                                     or a row's title, a group's chevron, the "Select all / none" box, a quick pick
///     open-file <path>                a .torrent opened as from Finder (QA roots only)
///     devlog open | close | filter <text> | errors on|off | event <name> | expand all|none|first|last | state
///                                     the tracker log viewer of Settings > Advanced > Developer (state: counts into <file>.bench)
///     play-players                    detection result (installed players, slices, can run) into <file>.bench
///     rowcommands <name> [menubar]    the row's context menu as drawn (submenus in brackets) into <file>.bench
///     settings-popup                  the "Play videos with" choices as a menu (`shot … menu`)
///     files expand|folders|collapse <torrent>   open its file list (folders = every folder too), close it
///     files filter <torrent> <text> | files select <torrent> <file> | files menu <torrent> <file> [priority]   (the file's context menu)
///     files skip|high <torrent> <file>   the checkbox / the Priority menu on one file; files collapse-all
///     files toggle <torrent>          the row's disclosure triangle; files select <torrent> <file> [command] [shift] is a click on a file row
///     rowclick <name> [command|shift] a click on a row of the main window's list (anchor and ranges included)
///     key <key> [shift] [cmd] [list]  what that key does in the main window (the core's answer; no key event is posted)
///     add-sheet open | close | text <text>   File › Add Magnet Link or URL…'s sheet, and typing in its field
///     play-row <row>                  the row's Play button (largest video); reports it and the blocked launch
///     play <id>                       Play on a row that is not finished (never launches a player: QA blocks launching)
///     favorite <tracker> | unfavorite <tracker>   "Save as Favorite" / "Remove from Favorites" (confirmed) on a tracker by name or id
///     tracker-menu <tracker> | tracker-menu close   the sidebar item's context menu (Info, Rename, Remove from Favorites / Save as Favorite)
///     tracker-info <tracker> | tracker-info close   the tracker's Info page (the sidebar item stays selected) / back to its torrents
///     fav rename <tracker> | fav addurl | fav editurl <i> | fav removeurl <i> | fav type <text> | fav commit | fav cancel | fav remove-ask [tracker]
///                                     the inline edits (the field is focused; `fav type` fills it)
///     ui dump | ui state | ui ping [win=main|chooser|popover|info]   read the window (info: Get Info's popover): its accessibility tree, the selection and
///                                     chooser state, how fast the main run loop answers (see QAInspect)
///     alert <n>                       presses the n-th button of the open alert
///     net offline | net online        the OS path monitor said so (engine holds everything / probes once); no debounce
///     power battery | power ac        the power source as PowerWatcher tells the core (the "On battery" setting acts);
///                                     a real change of the power source still reports as well
///     play-app <App.app> <file>       the one real launch: opens <file> in that app (a finished local test video)
@MainActor final class QAControl {
    private let path: String
    private unowned let app: AppDelegate
    /// Lines read from the file (empty ones included), and lines done.
    private var read = 0
    private var done = 0
    private var pending: [String] = []
    private var draining = false
    private var watcher: DispatchSourceFileSystemObject?
    private var fd: Int32 = -1
    /// The context menu QA has open: drawn by `shot … menu` (a quiet run pops none up on a screen).
    private var openMenu: NSMenu?
    /// The menu bar menu's footer side drawn hovered (`menu-hover`), applied again before each drawing of the menu.
    private var menuHover: String?
    /// Set while `wait` runs its command: the reports go here instead of `<file>.bench`.
    private var captured: [String]?

    init(path: String, app: AppDelegate) {
        self.path = path
        self.app = app
        FileManager.default.createFile(atPath: path, contents: nil)
        try? "0".write(toFile: path + ".ack", atomically: true, encoding: .utf8)
        // Play never reaches a player here, but the URL it would have handed over lands in `<file>.bench`.
        PlayerLauncher.onBlockedLaunch = { [weak self] url in self?.report("play-url: \(url.absoluteString)") }
        // A file-system watch instead of a timer: the harness must not add wake-ups of its own
        // while idle numbers are measured.
        fd = open(path, O_EVTONLY)
        // The handler runs on a background queue and hops to the main run loop in every mode (an alert's modal loop too).
        watcher = Self.watch(fd: fd) { [weak self] in
            RunLoop.main.perform(inModes: [.common, .default, .eventTracking, .modalPanel]) {
                MainActor.assumeIsolated { self?.poll() }
            }
        }
    }

    /// Built outside the actor: the handler runs on a background queue.
    private nonisolated static func watch(fd: Int32, _ onChange: @escaping @Sendable () -> Void) -> DispatchSourceFileSystemObject {
        let src = DispatchSource.makeFileSystemObjectSource(fileDescriptor: fd, eventMask: [.write, .extend], queue: DispatchQueue(label: "qa.control"))
        src.setEventHandler(handler: onChange)
        src.resume()
        return src
    }

    /// New complete lines join the queue; one drain at a time runs them in order.
    private func poll() {
        guard let text = try? String(contentsOfFile: path, encoding: .utf8) else { return }
        let lines = Self.completeLines(text)
        guard lines.count > read else { return }
        let new = Array(lines[read...])  // lines is an array: indices from 0
        read = lines.count
        if draining, NSApp.modalWindow != nil {
            // A command is waiting in an alert's modal loop (backlog offer, Move?): these run inside it (`alert 1`).
            Task { for line in new { await run(line); done += 1 } }
            return
        }
        pending += new
        if !draining { drain() }
    }

    /// Runs the queue, then lays every window out and writes the ack.
    private func drain() {
        draining = true
        Task {
            while !pending.isEmpty {
                let line = pending.removeFirst()
                if !line.trimmingCharacters(in: .whitespaces).isEmpty { await run(line) }
                done += 1
                if pending.isEmpty { await settle() }
            }
            try? String(done).write(toFile: path + ".ack", atomically: true, encoding: .utf8)
            draining = false
        }
    }

    /// The commands' effects applied and laid out: SwiftUI applies a change on the next turns of the run loop.
    private func settle() async {
        for _ in 0..<2 {
            try? await Task.sleep(for: .milliseconds(20))
            for w in NSApp.windows where w.isVisible {
                w.contentView?.superview?.layoutSubtreeIfNeeded()
                w.displayIfNeeded()
            }
        }
    }

    /// A context menu QA asked for: drawn by `shot … menu`; shown at its place only outside a quiet run.
    private func pop(_ menu: NSMenu, show: @escaping @MainActor () -> Void) {
        openMenu = menu
        guard !QuietRun.on else { return }
        DispatchQueue.main.async { MainActor.assumeIsolated { show() } }
    }

    private func closeMenu() {
        openMenu?.cancelTracking()
        openMenu = nil
    }

    private var scene: QAShots.Scene { QAShots.Scene(app: app, menu: openMenu, menuHover: menuHover) }

    /// `shot <name> [what]`: the picture as `<name>.png`.
    private func shot(_ arg: String) async {
        let words = arg.split(separator: " ", maxSplits: 1).map(String.init)
        guard let name = words.first else { return report("shot: name?") }
        let file = name.hasSuffix(".png") ? name : name + ".png"
        let url = file.hasPrefix("/") ? URL(fileURLWithPath: file)
            : URL(fileURLWithPath: path).deletingLastPathComponent().appendingPathComponent(file)
        let what = words.count > 1 ? words[1] : "main"
        switch await QAShots.image(what, in: scene) {
        case let .success(rep):
            guard let png = rep.representation(using: .png, properties: [:]) else { return report("shot: \(name) FAILED: no PNG") }
            try? FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
            do { try png.write(to: url) } catch { return report("shot: \(name) FAILED: \(error.localizedDescription)") }
            report("shot: \(url.path) \(rep.pixelsWide)x\(rep.pixelsHigh) \(what)")
        case let .failure(why):
            report("shot: \(name) FAILED: \(why)")
        }
    }

    /// The complete lines of the control file, empty ones included (they count in the ack as in `wc -l`); the text after
    /// the last newline may still be being written.
    nonisolated static func completeLines(_ text: String) -> [String] {
        Array(text.components(separatedBy: "\n").dropLast())
    }

    /// `wait`'s argument: the command, the text, whether it must be absent (`!~`), the limit in seconds (default 15).
    struct WaitSpec: Equatable {
        let command: String
        let text: String
        let absent: Bool
        let limit: Double

        init(command: String, text: String, absent: Bool, limit: Double) {
            self.command = command; self.text = text; self.absent = absent; self.limit = limit
        }

        /// `<command> ~ <text> [timeout <s>]` or `<command> !~ <text> [timeout <s>]`; nil when it is neither (or waits
        /// for a wait).
        init?(_ arg: String) {
            var rest = arg
            var limit = 15.0
            if let r = rest.range(of: " timeout ", options: .backwards),
               let t = Double(rest[r.upperBound...].trimmingCharacters(in: .whitespaces)) {
                limit = t
                rest = String(rest[..<r.lowerBound])
            }
            let absent = rest.contains(" !~ ")
            let parts = rest.components(separatedBy: absent ? " !~ " : " ~ ")
            guard parts.count == 2, !parts[0].isEmpty, !parts[1].isEmpty, parts[0].split(separator: " ").first != "wait" else { return nil }
            self.init(command: parts[0], text: parts[1], absent: absent, limit: limit)
        }
    }

    /// `wait <command> ~|!~ <text> [timeout <s>]`: runs the command every 100 ms until its report holds (`~`) or no
    /// longer holds (`!~`) the text.
    private func wait(_ arg: String) async {
        guard let spec = WaitSpec(arg) else { return report("wait: <command> ~ <text> [timeout <s>]") }
        let (command, text, negate, limit) = (spec.command, spec.text, spec.absent, spec.limit)
        let clock = ContinuousClock()
        let end = clock.now + .milliseconds(Int(limit * 1000))
        var last = ""
        while true {
            captured = []
            await run(command)
            let lines = captured ?? []
            captured = nil
            last = lines.last ?? ""
            if lines.contains(where: { $0.contains(text) }) != negate { return report(last) }
            if clock.now >= end { return report("wait: timeout after \(Int(limit)) s: \(last)") }
            try? await Task.sleep(for: .milliseconds(100))
        }
    }

    private func run(_ line: String) async {
        let parts = line.split(separator: " ", maxSplits: 1).map(String.init)
        let cmd = parts[0]
        let arg = parts.count > 1 ? parts[1] : ""
        let commands = app.commands!
        switch cmd {
        case "shot":
            await shot(arg)
        case "wait":
            await wait(arg)
        case "windows":
            report(QAShots.listing(scene))
        case "ui":
            let (rest, win) = qaTarget(arg)
            if rest == "dump" { QAInspect.dump(window: win) { report($0) } }
            else if rest == "views" { QAInspect.views(window: win) { report($0) } }
            else if rest == "ping" { report(await withCheckedContinuation { done in QAInspect.ping { done.resume(returning: $0) } }) }
            else { report(qaUIState()) }
        case "net":
            app.networkWatcher?.inject(satisfied: arg == "online")
        case "power":
            guard arg == "battery" || arg == "ac" else { return report("power: battery or ac") }
            app.power?.report(arg == "battery")
        case "favorite", "unfavorite":
            guard let t = qaTracker(arg) else { return report("\(cmd): no tracker \(arg)") }
            if cmd == "favorite" { commands.favorites.saveAsFavorite(t.id) }
            else { commands.favorites.askRemove(t.id); commands.favorites.confirmRemoval() }
        case "tracker-menu":
            qaTrackerMenu(arg)
        case "tracker-info":
            if arg == "close" { app.mainWindowController?.libraryModel?.trackerInfo = false }
            else if let t = qaTracker(arg) { commands.showTrackerInfo(t.id) }
            else { report("tracker-info: no tracker \(arg)") }
        case "fav":
            qaFav(arg)
        case "window":
            if arg == "close" { app.mainWindowController?.closeForQuit() }
            else if arg.hasPrefix("size") {
                let wh = arg.dropFirst(5).split(separator: "x").compactMap { Double($0) }
                commands.showMain(nil, nil)
                if wh.count == 2, let w = app.mainWindowController?.nsWindow {
                    w.setContentSize(NSSize(width: wh[0], height: wh[1]))
                    w.center()
                }
            } else { commands.showMain(nil, nil) }
        case "section":
            commands.showMain(arg == "sharing" ? .sharing : .downloads, nil)
        case "tracker":
            commands.showMain(nil, nil)
            app.mainWindowController?.libraryModel?.select(arg == "none" ? .sharing : .tracker(trackerId: arg))
        case "tab":
            // `tab downloads | sharing | tracker <name or id>`: the sidebar item, selected like a click.
            commands.showMain(nil, nil)
            guard let m = app.mainWindowController?.libraryModel else { return }
            let words = arg.split(separator: " ", maxSplits: 1).map(String.init)
            switch words.first {
            case "downloads": m.select(.downloads)
            case "sharing": m.select(.sharing)
            case "tracker":
                let name = words.count > 1 ? words[1] : ""
                if let t = m.snapshot.trackers.first(where: { $0.id == name || $0.name.localizedCaseInsensitiveCompare(name) == .orderedSame })
                    ?? m.snapshot.trackers.first(where: { $0.name.localizedCaseInsensitiveContains(name) }) { m.select(.tracker(trackerId: t.id)) }
                else { report("tab: no tracker \(name)") }
            default: report("tab: \(arg)?")
            }
        case "search":
            app.mainWindowController?.libraryModel?.setSearch(arg)
        case "add-sheet":
            // `add-sheet open | close | text <text>`: File › Add Magnet Link or URL…'s sheet; `text` is what its field's
            // binding calls as the user types.
            let m = app.mainWindowController?.libraryModel
            if arg == "close" { m?.closeAddSheet() }
            else if arg.hasPrefix("text ") { m?.setAddSheetText(String(arg.dropFirst(5))) }
            else { m?.showAddSheet() }
        case "select":
            // `select <id>` or `select name1, name2`: rows shown in the window, by id prefix or name.
            guard let m = app.mainWindowController?.libraryModel else { return }
            let wanted = arg.split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) }
            let hits = m.visibleRows.filter { r in wanted.contains { r.id.hasPrefix($0) || r.displayName.localizedCaseInsensitiveContains($0) } }
            m.setSelection(hits.isEmpty ? [arg] : hits.map(\.id))
        case "rowclick":
            // `rowclick <name> [command|shift]`: a click on that row as the list handles it (anchor and ranges included).
            guard let m = app.mainWindowController?.libraryModel else { return }
            let words = arg.split(separator: " ").map(String.init)
            let flags = Set(words.filter { $0 == "command" || $0 == "shift" })
            let name = words.filter { $0 != "command" && $0 != "shift" }.joined(separator: " ")
            // The rows on screen, in list order (folded groups and the groups of a folded block have none).
            let shown = m.view.list?.groups ?? []
            guard let hit = shown.lazy.flatMap({ g in g.rows.map { (g.key, $0.transfer) } }).first(where: { $0.1.displayName.localizedCaseInsensitiveContains(name) || $0.1.id.hasPrefix(name) })
            else { return report("rowclick: no row \(name)") }
            m.click(group: hit.0, id: hit.1.id, command: flags.contains("command"), shift: flags.contains("shift"))
        case "info":
            app.mainWindowController?.libraryModel?.openInfo(arg == "none" ? nil : arg)
        case "settings":
            if arg == "close" { app.mainWindowController?.libraryModel?.select(.downloads) }
            else { commands.showSettings(SettingsTab(rawValue: arg)) }
        case "tracker-edit":
            commands.editTracker(arg)
        case "appearance":
            NSApp.appearance = arg == "dark" ? NSAppearance(named: .darkAqua) : arg == "light" ? NSAppearance(named: .aqua) : nil
        case "chooser":
            // `chooser first` opens the first torrent that waits for a choice; `chooser <n>` the n-th.
            let held = commands.core.snapshot().allDownloading.filter(\.awaitingChoice)
            let i = Int(arg) ?? 0
            if held.indices.contains(i) { commands.showChooser(held[i].id) }
        case "chooser-category":
            // `chooser-category movie`: Change ▸ Movie in the open chooser.
            if let c = Category.all.first(where: { $0.label.lowercased() == arg.lowercased() }) { app.qaChooserChange(to: c) }
        case "scroll":
            scroll(arg)
        case "playglyph":
            QAFlags.playGlyph = arg.isEmpty ? "play.fill" : arg
        case "info-scroll":
            // `info-scroll top|bottom|<0...1>`: scrolls the open Get Info popover.
            if let w = NSApp.windows.first(where: { String(describing: type(of: $0)).contains("Popover") }) { scroll(arg, in: w) }
        case "settings-scroll":
            scroll(arg)
        case "chooser-close":
            app.closeChoosers()
        case "rowcommands":
            // `rowcommands <id> [menubar]`: the row's context menu as drawn, submenus in brackets, into `<file>.bench`.
            let ids = arg.split(separator: " ").map(String.init)
            guard let row = ids.first.flatMap(contextRow) else { report("rowcommands: no row"); return }
            report("rowcommands: \(Self.describe(commands.entries(for: [row], inMenuBar: ids.contains("menubar"))))")
        case "contextmenu":
            // `contextmenu <id> [menubar]`: the row's context menu, open until `contextmenu close`.
            let ids = arg.split(separator: " ").map(String.init)
            if ids.first == "close" { return closeMenu() }
            guard let row = ids.first.flatMap(contextRow), let win = app.mainWindowController?.nsWindow else { return report("contextmenu: no row \(arg)") }
            var entries = commands.entries(for: [row], inMenuBar: ids.contains("menubar"))
            if ids.contains("playwith") {
                // The submenu on its own (a hover-opened submenu is a separate window we cannot summon).
                entries = entries.compactMap { if case let .submenu("Play With", children) = $0 { return children } else { return nil } }.first ?? []
            }
            let menu = MenuBuilder.menu(entries)
            let p = NSPoint(x: win.frame.minX + 520, y: win.frame.maxY - 190)
            pop(menu) { menu.popUp(positioning: nil, at: win.contentView?.convert(win.convertFromScreen(NSRect(origin: p, size: .zero)).origin, from: nil) ?? .zero, in: win.contentView) }
        case "devlog":
            await devlogCommand(arg)
        case "files":
            filesCommand(arg)
        case "play-players":
            playPlayers()
        case "settings-popup":
            settingsPopup()
        case "play":
            // Only unfinished rows: they open a stream; the blocked launch reports its URL.
            let snap = commands.core.snapshot()
            guard let row = (snap.allDownloading + snap.allQueued + snap.allRecent).first(where: { $0.id.hasPrefix(arg) || $0.displayName.localizedCaseInsensitiveContains(arg) }) else { return report("play: no row \(arg)") }
            guard row.doneBytes < row.sizeBytes else { return report("play: \(row.displayName) is finished, QA does not launch players") }
            commands.play(row)
        case "play-row":
            // `play-row <row>`: the Play button of any row (finished or not); the blocked launch reports the file or URL.
            let snap = commands.core.snapshot()
            let all = snap.allDownloading + snap.allQueued + snap.allRecent + snap.sharingBusiest
                + commands.core.sharingRows(query: RowQuery(group: nil, trackerId: nil, text: "", offset: 0, limit: 400))
            guard let row = all.first(where: { $0.id.hasPrefix(arg) || $0.displayName.localizedCaseInsensitiveContains(arg) }) else { return report("play-row: no row \(arg)") }
            let largest = commands.largestVideo(row)
            report("play-row: \(row.displayName) largest video \(largest?.name ?? "none") (\(largest?.length ?? 0) bytes)")
            commands.playButtonPressed(row)
        case "play-live":
            // `play-live <row>`: the real Play of an unfinished row, launching the chosen player once
            // (the stream URL is resolved off the main thread, so launching stays open for 20 s).
            let snap = commands.core.snapshot()
            guard let row = (snap.allDownloading + snap.allQueued).first(where: { $0.id.hasPrefix(arg) || $0.displayName.localizedCaseInsensitiveContains(arg) }) else { return report("play-live: no row \(arg)") }
            PlayerLauncher.blocked = false
            commands.play(row)
            DispatchQueue.main.asyncAfter(deadline: .now() + 20) { PlayerLauncher.blocked = true }
            report("play-live: \(row.displayName)")
        case "play-app":
            // `play-app /path/App.app /path/video.mkv`: a real launch, once, of a local file.
            let parts = arg.components(separatedBy: " /").enumerated().map { $0.offset == 0 ? $0.element : "/" + $0.element }
            guard parts.count == 2, let spec = Bundle(path: parts[0])?.bundleIdentifier.flatMap(KnownPlayers.spec(forID:)) else { return report("play-app: need '<App.app> <file>' of a known player") }
            let archs = MachO.executable(ofApp: URL(fileURLWithPath: parts[0])).flatMap(MachO.architectures(ofFileAt:))
            let player = InstalledPlayer(spec: spec, appURL: URL(fileURLWithPath: parts[0]), archs: archs, runs: HostCPU.current.canRun(archs))
            guard player.runs else { return report("play-app: \(player.name) cannot run on this Mac") }
            PlayerLauncher.blocked = false
            PlayerLauncher.play(url: URL(fileURLWithPath: parts[1]), with: player) { [weak self] err in
                self?.report("play-app: \(player.name) \(err ?? "opened")")
            }
            PlayerLauncher.blocked = true
        case "diag":
            let d = commands.core.diagnostics()
            report("diagnostics: wakeups \(d.wakeups), connections \(d.connections), rechecks \(d.rechecks), promotions \(d.promotions), demotions \(d.demotions)")
        case "bench-snapshot":
            benchSnapshot()
        case "bench-scroll":
            benchScroll()
        case "alert":
            // `alert 1` presses the first button of the alert that is open (1000 = first button).
            NSApp.stopModal(withCode: NSApplication.ModalResponse(rawValue: 999 + (Int(arg) ?? 1)))
        case "open-file":
            // A .torrent opened from Finder (SPEC 7A.6): the same path as double-clicking one. QA roots only.
            guard ["/private/tmp/", "/tmp/"].contains(where: arg.hasPrefix) else { return report("open-file: only under /private/tmp") }
            app.application(NSApp, open: [URL(fileURLWithPath: arg)])
        case "quit":
            app.quitForReal()
        case "quit-menu":
            // The menu bar menu's Quit row pressed: the call its click makes; ends the process.
            app.statusMenuController?.perform(open: true)
            app.statusMenuController?.qaPressFooter("quit")
        case "policy":
            report(app.qaPolicyLine())
        case "config":
            // How often the engine re-read and applied its settings files (a hand edit or `seedwell settings set` of
            // config.toml shows here once it is in force).
            report("config: reloads \(app.qaConfigReloads).")
        case "dock-quit":
            // The quit Apple event the Dock menu's Quit sends (no reason), or with a reason: `dock-quit logout`.
            app.qaSendQuitEvent(reason: arg.isEmpty ? nil : arg)
        case "key":
            // `key delete|return|escape|space|left|right|up|down|a|i [shift] [cmd] [list]`: what that key does in the main
            // window, through the same core call a key press reaches (no key event is posted). The list's own keys
            // (space, arrows) and `list` use the list's scope, the others the window's.
            let parts = arg.split(separator: " ").map(String.init)
            let keys: [String: LibraryKey] = [
                "delete": .delete, "return": .return, "escape": .escape, "space": .space, "left": .left, "right": .right,
                "up": .up, "down": .down, "a": .a, "i": .i,
            ]
            guard let name = parts.first, let k = keys[name], let m = app.mainWindowController?.libraryModel else { return report("key: \(arg)?") }
            let listKey = ["space", "left", "right", "up", "down"].contains(name) || parts.contains("list")
            let mods = KeyModifiers(shift: parts.contains("shift"), command: parts.contains("cmd"), option: false, control: false)
            report("key: \(arg) \(m.key(k, mods, scope: listKey ? .list : .window) ? "handled" : "ignored")")
        case "fold":
            // `fold <group id>` (active, dormant, error, next, recent…): toggles that group header. On a tracker tab
            // the ids are downloads, sharing (the blocks), downloading, queued, recent, popular, active, dormant, error, dead.
            app.mainWindowController?.libraryModel?.toggleCollapsed(arg)
        case "remove-ask":
            // `remove-ask <name> [delete|menubar]`: the inline Remove… strip under that row; `remove-ask cancel` drops it, `remove-ask confirm` presses Remove.
            if arg == "cancel" { app.mainWindowController?.libraryModel?.cancelRemoval(); return }
            if arg == "confirm" { app.mainWindowController?.libraryModel?.confirmRemoval(); return }
            let parts = arg.split(separator: " ").map(String.init)
            let snap = commands.core.snapshot()
            let rows = snap.allDownloading + snap.allQueued + snap.allRecent
                + commands.core.sharingRows(query: RowQuery(group: nil, trackerId: nil, text: "", offset: 0, limit: 400))
            guard let name = parts.first, let row = rows.first(where: { $0.id.hasPrefix(name) || $0.displayName.localizedCaseInsensitiveContains(name) || $0.name.localizedCaseInsensitiveContains(name) })
            else { return report("remove-ask: no row \(arg)") }
            commands.confirmRemove([row], deleteFiles: parts.contains("delete"), openWindow: parts.contains("menubar"))
        case "getinfo":
            // Get Info the way the context menu asks for it: it must stay in the section shown.
            let snap = commands.core.snapshot()
            let rows = snap.allDownloading + snap.allQueued + snap.allRecent + snap.sharingBusiest
                + commands.core.sharingRows(query: RowQuery(group: nil, trackerId: nil, text: "", offset: 0, limit: 400))
            guard let row = rows.first(where: { $0.id.hasPrefix(arg) || $0.displayName.localizedCaseInsensitiveContains(arg) || $0.name.localizedCaseInsensitiveContains(arg) })
            else { return report("getinfo: no row \(arg)") }
            commands.showMain(nil, row.id)
            // The popover opens once the row is laid out (MainWindowController.show): answered when it is up.
            for _ in 0..<60 where app.mainWindowController?.infoWindow == nil || app.mainWindowController?.libraryModel?.infoRow != row.id {
                try? await Task.sleep(for: .milliseconds(50))
            }
            let m = app.mainWindowController?.libraryModel
            report("getinfo: section \(m?.section == .sharing ? "sharing" : "downloads"), info \(m?.infoRow == row.id ? "open" : "closed")")
        case "getinfo-card":
            // The open Get Info card as the core drew it up and the popover draws it: its lines, trackers and buttons.
            guard let m = InfoCardHolder.shown?.model else { return report("getinfo-card: none") }
            report("getinfo-card: \(m.title) | \(m.subtitle)")
            for s in m.sections {
                report("  section \(s.title)")
                for r in s.rows {
                    let value = r.value.map(\.text).joined()
                    report("    \(r.label): \(r.menu.map { "menu \($0.title)" } ?? value)\(r.button.map { " [\($0.title)]" } ?? "")\(r.playControls.isEmpty ? "" : " play")")
                }
                if let a = s.askAgain { report("    [\(a.title)] \(s.askAgainNote ?? "")") }
                for t in s.trackers {
                    report("    tracker \(t.url)")
                    report("      \(t.live)\(t.allowed.map { " · \($0)" } ?? "")")
                    report("      \(t.detail)")
                }
            }
            report("  buttons \(m.buttons.map(\.title).joined(separator: ", ")) · next read in \(m.reloadDueMs) ms")
        case "getinfo-window":
            // Get Info's popover as drawn: open on whose row (by the name it shows), a child of the main window (its
            // ↑/↓ reach the list), and whether it has the keys.
            guard let m = app.mainWindowController?.libraryModel, let id = m.infoRow, let w = app.mainWindowController?.infoWindow else {
                return report("getinfo-window: none")
            }
            let name = m.visibleRows.first { $0.id == id }?.displayName ?? id
            let shows = QAInspect.items(in: w).contains { $0.label.contains(name) }
            report("getinfo-window: open, row '\(name)' \(shows ? "shown" : "not shown"), child \(w.parent === app.mainWindowController?.nsWindow), key \(w.isKeyWindow)")
        case "where":
            let m = app.mainWindowController?.libraryModel
            report("where: section \(m.map { String(describing: $0.section) } ?? "none"), tracker \(m?.trackerID ?? "none"), info \(m?.infoRow ?? "none"), selection \(m?.selection.count ?? 0), badge \(m?.downloadsFlag == true ? 1 : 0), window \(m == nil ? "closed" : "open"), sheet \(m?.view.addSheet.map { "open '\($0.text)'" } ?? "closed"), rows \(m?.visibleRows.count ?? 0), arrived \(commands.arrivals.arrivals.arrived().count)")
        case "menu":
            app.statusMenuController?.perform(open: arg != "close")
        case "menu-files":
            menuFilesCommand(arg)
        case "menu-titles":
            report("menu: \(app.statusMenuController?.qaTitles ?? "")")
        case "menu-subtitles":
            report("menu-subtitles: \(app.statusMenuController?.qaSubtitles ?? "")")
        case "menu-sync":
            menuSyncCommand(arg)
        case "menu-hover":
            // `menu-hover open | quit | none`: the Open / Quit row as a pointer over one side draws it.
            menuHover = arg == "none" ? nil : arg
            app.statusMenuController?.footerRow?.qaHighlight(menuHover)
        case "menu-press":
            app.statusMenuController?.footerRow?.qaPress(arg)
        case "starting":
            starting(arg)
        case "choice":
            choiceCommand(arg)
        default:
            break
        }
    }

    /// What player detection finds on this Mac, and what Play would use for an MKV and an MP4.
    private func qaTracker(_ name: String) -> TrackerSummary? {
        let all = commands.core.snapshot().trackers
        return all.first { $0.id == name || $0.name.localizedCaseInsensitiveCompare(name) == .orderedSame }
            ?? all.first { $0.name.localizedCaseInsensitiveContains(name) }
    }

    private var commands: Commands { app.commands }

    private func qaFav(_ arg: String) {
        let words = arg.split(separator: " ", maxSplits: 1).map(String.init)
        let rest = words.count > 1 ? words[1] : ""
        let f = commands.favorites
        let current = app.mainWindowController?.libraryModel?.trackerID
        switch words.first {
        case "rename": if let t = qaTracker(rest) { f.beginRename(t.id) }
        case "addurl": if let id = qaTracker(rest)?.id ?? current { f.beginAddURL(id) }
        case "removeurl": if let id = current, let i = Int(rest) { f.removeURL(id, index: i) }
        case "editurl": if let id = current, let i = Int(rest) { f.beginEditURL(id, index: i) }
        case "type": f.text = rest
        case "commit": f.commit()
        case "cancel": f.cancel(); f.cancelRemoval()
        case "remove-ask": if let id = qaTracker(rest)?.id ?? current { f.askRemove(id) }
        default: report("fav: \(arg)?")
        }
    }

    /// The context menus of the trackers, open until `tracker-menu close`.
    private func qaTrackerMenu(_ arg: String) {
        if arg == "close" { return closeMenu() }
        guard let win = app.mainWindowController?.nsWindow else { return }
        var entries: [MenuEntry] = []
        var at = NSPoint(x: 110, y: 330)
        if let t = qaTracker(arg), let m = app.mainWindowController?.libraryModel {
            let place = LibraryPlace.tracker(trackerId: t.id)
            entries = SidebarMenus.entries(for: place, model: m, commands: commands)
            let n = m.view.sidebar.trackers?.items.firstIndex { $0.place == place } ?? 0
            let screen = NSPoint(x: win.frame.minX + 90, y: win.frame.maxY - 195 - CGFloat(n) * 40)
            at = win.contentView?.convert(win.convertFromScreen(NSRect(origin: screen, size: .zero)).origin, from: nil) ?? .zero
        }
        guard !entries.isEmpty else { return report("tracker-menu: none for \(arg)") }
        let menu = MenuBuilder.menu(entries)
        pop(menu) { menu.popUp(positioning: nil, at: at, in: win.contentView) }
    }

    private func playPlayers() {
        let host = HostCPU.current
        report("host: \(host.appleSilicon ? "Apple silicon" : "Intel"), Rosetta \(host.rosettaInstalled ? "installed" : "not installed")")
        let installed = PlayerDirectory.shared.installed(maxAge: 0)
        for p in installed {
            let archs = (p.archs ?? []).map { "\($0)" }.sorted().joined(separator: "+")
            report("player: \(p.menuTitle) at \(p.appURL.path) [\(archs.isEmpty ? "unknown" : archs)] \(p.runs ? "runs" : "CANNOT RUN")")
        }
        if installed.isEmpty { report("player: none installed") }
        let pref = app.commands.core.settingsLoad().general.playWith
        for (label, c) in [("mkv", VideoContainer.other), ("mp4", .mp4Family)] {
            let choice = PlayerCatalog.resolve(preference: pref, installed: installed, container: c)
            let text: String
            switch choice {
            case let .play(p): text = "plays with \(p.name)"
            case let .needsUpdate(p): text = "needs update: \(p.name)"
            case .nothingInstalled: text = "no player"
            }
            let withList = PlayerCatalog.capable(installed, for: c).map(\.name).joined(separator: ", ")
            report("setting \(pref): \(label) \(text); Play With: \(withList.isEmpty ? "-" : withList)")
        }
    }

    /// The pop-up of Settings ▸ General ▸ Play videos with, as a menu over the Settings window.
    private func settingsPopup() {
        guard let win = app.mainWindowController?.nsWindow, let content = win.contentView else { return }
        let pref = app.commands.core.settingsLoad().general.playWith
        let menu = NSMenu()
        menu.autoenablesItems = false
        let auto = NSMenuItem(title: "Automatic", action: nil, keyEquivalent: "")
        auto.state = pref == "auto" ? .on : .off
        menu.addItem(auto)
        for p in PlayerDirectory.shared.installed(maxAge: 0) {
            let item = NSMenuItem(title: p.menuTitle, action: nil, keyEquivalent: "")
            item.isEnabled = p.runs
            item.state = pref == p.id ? .on : .off
            menu.addItem(item)
        }
        // Over the Playback row's pop-up (QA captures the General tab at 860 x 700).
        let p = NSPoint(x: 209, y: 570)
        pop(menu) { menu.popUp(positioning: auto, at: p, in: content) }
    }

    /// `scroll top | bottom | <0...1>`: the main window's list, to show the groups further down.
    /// `files <verb> <torrent-substring> [args]`; the torrent is found by a part of its name.
    private func filesCommand(_ arg: String) {
        let store = FilesStore.shared
        let words = arg.split(separator: " ", maxSplits: 2).map(String.init)
        guard let verb = words.first else { return }
        if verb == "collapse-all" { for id in store.expanded { store.collapse(id) }; return }
        guard words.count > 1 else { return }
        let core = app.commands.core
        let snap = core.snapshot()
        let rows = snap.allDownloading + snap.allQueued + snap.allRecent + core.sharingRows(query: RowQuery(group: nil, trackerId: nil, text: "", offset: 0, limit: 400))
        guard let row = rows.first(where: { $0.id.hasPrefix(words[1]) || $0.name.localizedCaseInsensitiveContains(words[1]) }) else { return report("files: no torrent \(words[1])") }
        let rest = words.count > 2 ? words[2] : ""
        switch verb {
        case "expand": store.expand(row.id)
        case "collapse": store.collapse(row.id)
        // The row's disclosure triangle.
        case "toggle": store.toggle(row.id)
        case "folders":
            store.expand(row.id)
            if let st = store.states[row.id], let t = st.tree { for f in t.folderIds() { st.setFolder(f, open: true) } }
        case "filter": store.states[row.id]?.filter = rest
        default:
            guard let st = store.states[row.id], let tree = st.tree else { return report("files: \(row.name) is not open") }
            let fileWords = rest.split(separator: " ", maxSplits: 1).map(String.init)
            guard let pat = fileWords.first,
                  let file = tree.rows(expanded: tree.folderIds(), filter: "").first(where: { $0.name.localizedCaseInsensitiveContains(pat) }) else { return report("files: no file \(rest)") }
            let node = file.id
            switch verb {
            case "select":
                // A click on the row, with ⌘ or ⇧ held when named: the torrent rows lose the selection, as for a click.
                let held = Set(fileWords.dropFirst().flatMap { $0.split(separator: " ").map(String.init) })
                FileRowView.click(torrent: row.id, node: node, store: store, command: held.contains("command"), shift: held.contains("shift")) { [weak self] in
                    self?.app.mainWindowController?.libraryModel?.setSelection([])
                }
            case "skip": st.toggle([node])
            case "high": st.setPriority([node], .high)
            case "menu":
                store.select(torrent: row.id, node: node, command: false, shift: false)
                var entries = FileRowView.menuEntries(row: row, commands: app.commands, state: st, store: store, file: file)
                if fileWords.count > 1 {
                    // The Priority submenu on its own (a hover-opened submenu is a window we cannot summon).
                    entries = entries.compactMap { if case let .submenu("Priority", c) = $0 { return c } else { return nil } }.first ?? []
                }
                guard let win = app.mainWindowController?.nsWindow else { return }
                let menu = MenuBuilder.menu(entries)
                let p = NSPoint(x: win.frame.minX + 520, y: win.frame.maxY - 330)
                pop(menu) { menu.popUp(positioning: nil, at: win.contentView?.convert(win.convertFromScreen(NSRect(origin: p, size: .zero)).origin, from: nil) ?? .zero, in: win.contentView) }
            default: report("files: unknown \(verb)")
            }
        }
    }

    private func scroll(_ arg: String, in window: NSWindow? = nil) {
        guard let root = (window ?? app.mainWindowController?.nsWindow)?.contentView,
              let sv = Self.scrollTarget(in: root), let doc = sv.documentView else { return }
        let maxY = max(0, doc.frame.height - sv.contentView.bounds.height)
        let f = arg == "bottom" ? 1 : arg == "top" ? 0 : (Double(arg) ?? 0)
        // The top of the list is the negative inset: the toolbar covers the first 50-odd points.
        let top = -sv.contentView.contentInsets.top
        sv.contentView.scroll(to: NSPoint(x: 0, y: top + (maxY - top) * f))
        sv.reflectScrolledClipView(sv.contentView)
    }

    /// The scroll view that `scroll` and `bench-scroll` drive: the widest one whose content is taller than it, nested
    /// ones included (an open file list scrolls inside the window's list once it is taller than about 13 rows).
    static func scrollTarget(in root: NSView) -> NSScrollView? {
        func views(_ v: NSView) -> [NSScrollView] { ((v as? NSScrollView).map { [$0] } ?? []) + v.subviews.flatMap(views) }
        return views(root).filter { ($0.documentView?.frame.height ?? 0) > $0.bounds.height }.max { $0.bounds.width < $1.bounds.width }
    }

    // MARK: Benchmarks (QA only)

    /// `starting show | close | state | shift on|off | cancel [name] [keep] | pause|resume|now|choose [name]`: the "Starting in …" notifier (SPEC 8.6).
    /// `show` keeps the popover up until `close`, `state` reports what it holds and its window.
    private func starting(_ arg: String) {
        guard let notifier = app.startingNotifier else { return report("starting: no notifier") }
        let words = arg.split(separator: " ", maxSplits: 1).map(String.init)
        let verb = words.first ?? "state"
        var name = words.count > 1 ? words[1] : ""
        // `cancel [name] keep` is ⇧-Cancel: the .torrent file stays.
        var keep = false
        if verb == "cancel" {
            var parts = name.split(separator: " ").map(String.init)
            keep = parts.contains("keep")
            parts.removeAll { $0 == "keep" }
            name = parts.joined(separator: " ")
        }
        let commands = app.commands!
        let rows = commands.core.snapshot().allDownloading.filter { $0.pending != nil || $0.awaitingChoice || $0.isUnresolvedMagnet }
        let row = rows.first { name.isEmpty || $0.displayName.localizedCaseInsensitiveContains(name) || $0.id.hasPrefix(name) }
        switch verb {
        case "show": notifier.qaShow()
        case "close": notifier.qaClose()
        case "shift": ShiftKey.qaOverride = name == "on" ? true : name == "off" ? false : nil
        case "pause": row.map { commands.pendingCountdown($0.id, pause: true) }
        case "resume": row.map { commands.pendingCountdown($0.id, pause: false) }
        case "now": row.map { commands.pendingStartNow($0.id) }
        case "choose": row.map { notifier.chooseFromPopover($0.id) }
        case "pill":
            // The pill's menu, popped up under the pill; Escape or `starting pill close` puts it away.
            if name == "close" { return closeMenu() }
            guard let r = row, let menu = notifier.qaPillMenu(r.id), let w = notifier.popoverWindow, let content = w.contentView else { return report("starting: no pill") }
            let at = QAInspect.items(in: w).first { $0.role == "AXPopUpButton" || $0.role == "AXMenuButton" }?.frame
                ?? NSRect(x: content.bounds.width - 70, y: 6, width: 52, height: 18)
            // `at` is in window points from the top, as `ui dump` prints them; the menu opens under the pill like a pull-down.
            let p = content.convert(NSPoint(x: at.minX, y: content.bounds.height - at.maxY - 4), from: nil)
            report("starting: pill menu \(menu.items.map { ($0.state == .on ? "*" : "") + $0.title }) at \(Int(at.minX)),\(Int(at.minY))")
            pop(menu) { menu.popUp(positioning: nil, at: p, in: content) }
            return
        case "ok", "cancel", "all", "none", "toggle", "open", "fold", "title", "category":
            // `ok|cancel|all|none|open|fold [name]` (fold opens or closes the block), `toggle <title of a file row>` (first held torrent)
            let named = name.isEmpty ? nil : rows.first { $0.displayName.localizedCaseInsensitiveContains(name) }
            let held = named ?? rows.first { $0.awaitingChoice } ?? row
            held.map { notifier.qaChoice(verb, id: $0.id, arg: keep ? "keep" : name) }
        default: break
        }
        if verb != "show" && verb != "close" { notifier.refresh() }
        let w = notifier.popoverWindow
        report(notifier.qaSummary + (w.map { " window \($0.windowNumber) \(Int($0.frame.width))x\(Int($0.frame.height)) at \(Int($0.frame.minX)),\(Int($0.frame.minY))" } ?? ""))
    }

    /// `choice tick <title> | fold <title> | all | pick <name> | state`: a click in a checklist, on the shared chooser model
    /// (the chooser window, the popover's list and the main window's list of a held torrent show one model): a box or a
    /// row's title (`tick`, both call `toggle`), a group's chevron (`fold`), the "Select all / none" box (`all`), a quick
    /// pick (`pick`). The model of the torrent that waits for a choice, else the first. Reports the ticks and the folds.
    private func choiceCommand(_ arg: String) {
        let words = arg.split(separator: " ", maxSplits: 1).map(String.init)
        let verb = words.first ?? "state"
        let rest = words.count > 1 ? words[1] : ""
        let models = ChoiceStore.shared.models.sorted { $0.key < $1.key }.map(\.value)
        guard let m = models.first(where: \.awaiting) ?? models.first else { return report("choice: no chooser") }
        let all = m.items + m.items.flatMap(\.children)
        let item = all.first { $0.title.localizedCaseInsensitiveCompare(rest) == .orderedSame }
            ?? all.first { $0.title.localizedCaseInsensitiveContains(rest) }
        switch verb {
        case "tick", "fold":
            guard !rest.isEmpty, let item else { return report("choice: no row \(rest)") }
            if verb == "tick" { m.toggle(item) } else { m.toggleExpanded(item) }
        case "all": m.toggleAll()
        case "pick":
            guard let p = ChooserModel.QuickPick.allCases.first(where: { $0.rawValue.localizedCaseInsensitiveCompare(rest) == .orderedSame })
            else { return report("choice: no quick pick \(rest)") }
            m.apply(p)
        case "state": break
        default: return report("choice: \(arg)?")
        }
        let open = m.items.filter { m.expanded.contains($0.id) }.map(\.title)
        report("choice: \(arg) -> checked \(m.checked.count) all \(m.allState) expanded \(open) rows \(m.visibleRows.map(\.item.title))")
    }

    /// `devlog open|close|filter|errors|event|expand|state`: drives the tracker log viewer.
    private func devlogCommand(_ arg: String) async {
        guard let nav = app.mainWindowController?.nav else { return report("devlog: no window") }
        let core = app.commands.core
        let model = nav.logModel(core: core)
        let parts = arg.split(separator: " ", maxSplits: 1).map(String.init)
        let sub = parts.first ?? ""
        let rest = parts.count > 1 ? parts[1] : ""
        switch sub {
        case "open":
            nav.showTrackerLog = true
            await model.refresh()
        case "close":
            nav.showTrackerLog = false
        case "filter":
            model.text = rest
        case "errors":
            model.errorsOnly = rest == "on"
        case "event":
            model.event = rest == "all" ? "" : rest
        case "expand":
            switch rest {
            case "all": model.expanded = Set(model.items.map(\.id))
            case "first": model.expanded = Set(model.items.first(where: { $0.row.kind == "announce" }).map(\.id).map { [$0] } ?? [])
            case "last": model.expanded = Set(model.items.last(where: { $0.row.kind == "announce" }).map(\.id).map { [$0] } ?? [])
            default: model.expanded = []
            }
        case "state":
            await model.refresh()
            report("devlog: \(model.items.count) entries, \(model.items.filter { $0.row.error }.count) errors, dropped \(model.dropped), open \(nav.showTrackerLog), settings dev mode \(app.commands.core.settingsLoad().advanced.developerMode)")
        default:
            report("devlog: unknown command \(arg)")
        }
    }

    /// `win=main|chooser|popover|info` anywhere in a `ui` argument picks the window it reads (default: the main window;
    /// `info`: Get Info's popover).
    /// Returns the argument without it.
    private func qaTarget(_ arg: String) -> (String, NSWindow?) {
        var words = arg.split(separator: " ").map(String.init)
        var win = app.mainWindowController?.nsWindow
        if let i = words.firstIndex(where: { $0.hasPrefix("win=") }) {
            switch words[i].dropFirst(4) {
            case "chooser": win = app.qaChooserWindow
            case "popover": win = app.startingNotifier?.popoverWindow
            case "info": win = app.mainWindowController?.infoWindow
            default: break
            }
            words.remove(at: i)
        }
        return (words.joined(separator: " "), win)
    }

    /// The shared choice models in one clause: `choice <id8> checked <n> confirm <bool> primary "<title>" all <state> rows <n>`.
    private var qaChoiceState: String {
        ChoiceStore.shared.models.sorted { $0.key < $1.key }.map { id, m in
            "choice \(id.prefix(8)) checked \(m.checked.count) bytes \(m.selectedBytes) confirm \(m.canConfirm) primary \"\(m.primaryTitle)\" all \(m.allState) rows \(m.visibleRows.count) expanded [\(m.expanded.sorted().joined(separator: ","))]"
        }.joined(separator: " ")
    }

    /// `ui state`: what the commands changed, in one line.
    private func qaUIState() -> String {
        let m = app.mainWindowController?.libraryModel
        let f = FilesStore.shared
        let names = (m?.visibleRows ?? []).reduce(into: [String: String]()) { $0[$1.id] = $1.displayName }
        let sel = (m?.selection ?? []).map { String((names[$0] ?? $0).prefix(14)) }.sorted()
        let chev = (m?.visibleRows ?? []).filter { $0.fileCount > 1 && $0.kind == .torrent && !$0.isHeld }.map { String($0.displayName.prefix(10)) }
        let trees = f.states.sorted { $0.key < $1.key }.map { id, st in
            "\(id.prefix(8)):\(st.totals.map { "\($0.wantedCount)/\($0.fileCount)" } ?? "-")\(st.hasPending ? "+pending" : "")"
        }
        return "state: trees \(trees) chevrons \(chev) rows \(sel) files \(f.selectedTorrent.map { String($0.prefix(8)) } ?? "-") nodes \(f.selectedNodes.sorted()) expanded \(f.expanded.map { String($0.prefix(8)) }.sorted()) \(qaChoiceState)"
    }

    /// The inline file list of the open menu bar menu (`menu open` first):
    ///     menu-files expand <torrent>      a click on the row's chevron (opens, or closes when open; another one closes)
    ///     menu-files press <file|folder|all|none>   a click on a row of the list; `menu-files fold <folder>` its chevron
    ///     menu-files state <torrent>       engine wanted/total, which list is open, rows, footer, menu still tracking
    ///     menu-files context <row>         the titles of the row's right-click menu, its check and Play icon
    /// `menu-sync`: the Folder Sync submenu as drawn (rows, the open confirmation, the items) and its state;
    /// `menu-sync edit|remove <name>`: the row's Edit or Remove button pressed (Remove opens the confirmation under it,
    /// pressed again it closes; nothing is removed).
    private func menuSyncCommand(_ arg: String) {
        guard let sync = app.statusMenuController?.menuSync else { return report("menu-sync: none") }
        let words = arg.split(separator: " ", maxSplits: 1).map(String.init)
        switch words.first {
        case "edit", "remove": report("menu-sync: \(words[0]) \(sync.qaPress(words.count > 1 ? words[1] : "", edit: words[0] == "edit"))")
        default: report("menu-sync: \(sync.qaState) || \(sync.qaItems)")
        }
    }

    private func menuFilesCommand(_ arg: String) {
        guard let menu = app.statusMenuController else { return }
        let files = menu.menuFiles
        let words = arg.split(separator: " ", maxSplits: 1).map(String.init)
        let rest = words.count > 1 ? words[1] : ""
        func row(_ key: String) -> TransferRow? {
            let snap = app.commands.core.snapshot()
            return (snap.allDownloading + snap.allQueued + snap.allRecent).first { $0.displayName.localizedCaseInsensitiveContains(key) || $0.id.hasPrefix(key) }
        }
        switch words.first {
        case "expand": report("menu-files: expand \(files.qaToggle(rest, items: menu.qaItems))")
        case "press": report("menu-files: press \(files.qaPress(rest, fold: false))")
        case "fold": report("menu-files: fold \(files.qaPress(rest, fold: true))")
        case "frames": report("menu-files: frames \(files.qaFrames)")
        case "context": report("menu-files: context \(menu.qaRowContext(rest))")
        case "state":
            guard let r = row(rest) else { report("menu-files: no torrent"); return }
            report("menu-files: engine \(files.qaEngineWanted(r.id, core: app.commands.core)) expanded \(files.expanded.map { String($0.prefix(8)) } ?? "none") this \(files.expanded == r.id) rows \(files.qaRowCount) open \(menu.qaMenuOpen) footer \(files.qaFooter)")
        default: report("menu-files: unknown command")
        }
    }

    /// A row of any list by id prefix or name (`first`: the first one).
    private func contextRow(_ key: String) -> TransferRow? {
        let all = app.commands.core.snapshot()
        let rows = (all.allDownloading + all.allQueued + all.allRecent + all.sharingBusiest)
            + app.commands.core.sharingRows(query: RowQuery(group: nil, trackerId: nil, text: "", offset: 0, limit: 400))
        if key == "first" { return rows.first }
        return rows.first { $0.id.hasPrefix(key) || $0.name.localizedCaseInsensitiveContains(key) || $0.displayName.localizedCaseInsensitiveContains(key) }
    }

    /// "Pause|Move to Top|-|Priority [★ High|Normal ✓|Low]|Remove and Delete Files… (red)".
    static func describe(_ entries: [MenuEntry]) -> String {
        entries.map { e in
            switch e {
            case .separator: "-"
            case let .submenu(title, children): "\(title) [\(describe(children))]"
            case let .item(title, checked, enabled, role, key, _):
                title + (checked ? " ✓" : "") + (enabled ? "" : " (off)") + (role == .destructive ? " (red)" : "") + (key.map { " " + $0 } ?? "")
            }
        }.joined(separator: "|")
    }

    private func report(_ line: String) {
        if captured != nil { captured?.append(line); return }
        let url = URL(fileURLWithPath: path + ".bench")
        let old = (try? String(contentsOf: url, encoding: .utf8)) ?? ""
        try? (old + line + "\n").write(to: url, atomically: true, encoding: .utf8)
    }

    /// Cost of what the window polls once a second: `snapshot()` (cold: the core caches for 400 ms)
    /// and the sharing queries, measured on the main thread as the UI would.
    private func benchSnapshot() {
        let core = app.commands.core
        var snap: [Double] = [], rows: [Double] = []
        for _ in 0..<15 {
            Thread.sleep(forTimeInterval: 0.45)
            let t0 = DispatchTime.now().uptimeNanoseconds
            let s = core.snapshot()
            snap.append(Double(DispatchTime.now().uptimeNanoseconds - t0) / 1e6)
            let t1 = DispatchTime.now().uptimeNanoseconds
            _ = core.sharingCounts(trackerId: nil, category: nil, text: "")
            for g in SeedGroup.allCases {
                _ = core.sharingRows(query: RowQuery(group: g, trackerId: nil, text: "", offset: 0, limit: 150))
            }
            rows.append(Double(DispatchTime.now().uptimeNanoseconds - t1) / 1e6)
            if snap.count == 1 { report("rows in snapshot: sharing \(s.sharingTotal), downloading \(s.downloading.count), queued \(s.queued.count)") }
        }
        func stat(_ v: [Double]) -> String {
            let sorted = v.sorted()
            return String(format: "median %.2f ms, max %.2f ms", sorted[sorted.count / 2], sorted.last ?? 0)
        }
        report("core.snapshot() cold: " + stat(snap))
        report("sharing counts + 6 group pages (150 rows each): " + stat(rows))
    }

    /// Scrolls the main window's list down and up for a few seconds and records frame times.
    private func benchScroll() {
        guard let window = app.mainWindowController?.nsWindow, let root = window.contentView else { return }
        guard let sv = Self.scrollTarget(in: root) else {
            report("scroll: nothing to scroll"); return
        }
        let doc = sv.documentView!
        let maxY = max(0, doc.frame.height - sv.contentView.bounds.height)
        let perFrame = 18.0
        let runner = ScrollBench(window: window, scroll: sv, maxY: maxY, perFrame: perFrame) { [weak self] text in self?.report(text) }
        ScrollBench.current = runner
        runner.start()
    }
}

/// Drives a scroll view with the display link and measures how long frames take.
@MainActor final class ScrollBench: NSObject {
    static var current: ScrollBench?
    private let window: NSWindow
    private let scroll: NSScrollView
    private let maxY: Double
    private let perFrame: Double
    private let done: (String) -> Void
    private var link: CADisplayLink?
    private var last: CFTimeInterval = 0
    private var intervals: [Double] = []
    private var direction = 1.0
    private var passes = 0
    private var y = 0.0

    init(window: NSWindow, scroll: NSScrollView, maxY: Double, perFrame: Double, done: @escaping (String) -> Void) {
        self.window = window; self.scroll = scroll; self.maxY = maxY; self.perFrame = perFrame; self.done = done
    }

    func start() {
        // A quiet run's window is on no screen, so it has no display of its own to follow: the main display's.
        link = QuietRun.on ? (NSScreen.main ?? NSScreen.screens.first)?.displayLink(target: self, selector: #selector(frame(_:)))
            : window.displayLink(target: self, selector: #selector(frame(_:)))
        link?.add(to: .main, forMode: .common)
    }

    @objc private func frame(_ l: CADisplayLink) {
        if last != 0 { intervals.append((l.timestamp - last) * 1000) }
        last = l.timestamp
        y += direction * perFrame
        if y >= maxY { y = maxY; direction = -1; passes += 1 }
        if y <= 0 { y = 0; direction = 1; passes += 1 }
        scroll.contentView.scroll(to: NSPoint(x: 0, y: y))
        scroll.reflectScrolledClipView(scroll.contentView)
        if passes >= 2 || intervals.count > 900 {
            link?.invalidate()
            let v = intervals.sorted()
            let budget = 1000.0 / Double(window.screen?.maximumFramesPerSecond ?? 60)
            let late = v.filter { $0 > budget * 1.5 }.count
            done(String(format: "scroll: %d frames over %.0f pt of content, interval median %.2f ms, p95 %.2f ms, max %.2f ms, late (>1.5 frames) %d (%.1f%%)",
                        v.count, maxY, v[v.count / 2], v[Int(Double(v.count) * 0.95)], v.last ?? 0, late, 100 * Double(late) / Double(max(1, v.count))))
            ScrollBench.current = nil
        }
    }
}
