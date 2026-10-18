import AppKit

/// QA only (`--qa-control <file>`): runs scene commands appended to a text file, so a script can
/// put the real windows into each state and capture them from outside with `screencapture -l`.
/// One command per line; after each batch `<file>.ack` holds the number of lines executed.
///
///     window open | window close | window size 1180x820
///     section downloads | section sharing | tab downloads|sharing|tracker <name or id> | tracker <id> | tracker none | search <text> | select <id>
///     info <id> | info none | info-scroll top|bottom|<0...1> | getinfo <name>   (Get Info as the context menu asks for it)
///     policy | policy-real | dock-quit [logout] | quit-menu | where   (Dock icon and Quit paths)
///     settings <tab> | settings close | tracker-edit <id>
///     appearance dark | light | system
///     chooser <n> | chooser-category <name> | chooser-close | scroll top|bottom|<0...1> | settings-scroll top|bottom|<0...1> | activate
///     menu open | menu close
///     starting show | close | state | pause|resume|now|choose|ok|cancel|all|none|toggle|open|fold [name]   the "Starting in …" notifier popover
///     starting cancel [name] keep     ⇧-Cancel (keeps the .torrent file);  starting shift on|off   force the Shift state of the Cancel buttons
///     starting title [name]           a click on the title (opens the torrent in the main window); starting choose = the chevron
///     starting category <label>       the pill's menu: change the first held torrent's category (Movie, TV, Music, Audiobook, Book, App, Game, Other)
///     open-file <path>                a .torrent opened as from Finder (QA roots only)
///     devlog open | close | filter <text> | errors on|off | event <name> | expand all|none|first|last | state
///                                     the tracker log viewer of Settings > Advanced > Developer (state: counts into <file>.bench)
///     play-players                    detection result (installed players, slices, can run) into <file>.bench
///     contextmenu <name> playwith     the row's "Play With" submenu as a menu of its own (add `menubar` for the menu bar's version)
///     settings-popup                  the "Play videos with" choices as a menu over the Settings window
///     files expand|folders|collapse <torrent>   open its file list (folders = every folder too), close it
///     files filter <torrent> <text> | files select <torrent> <file> | files menu <torrent> <file> [priority]   (the file's context menu)
///     files skip|high <torrent> <file>   the checkbox / the Priority menu on one file; files collapse-all
///     play-row <row>                  the row's Play button (largest video); reports it and the blocked launch
///     play <id>                       Play on a row that is not finished (never launches a player: QA blocks launching)
///     favorite <tracker> | unfavorite <tracker>   "Save as Favorite" / "Remove from Favorites" (confirmed) on a tracker by name or id
///     tracker-menu <tracker> | tracker-menu close   the sidebar item's context menu (Info, Rename, Remove from Favorites / Save as Favorite)
///     tracker-info <tracker> | tracker-info close   the tracker's Info page (the sidebar item stays selected) / back to its torrents
///     fav rename <tracker> | fav addurl | fav editurl <i> | fav removeurl <i> | fav type <text> | fav commit | fav cancel | fav remove-ask [tracker]
///                                     the inline edits (the field is focused; `fav type` fills it)
///     click <label> [command|shift|double] [win=main|chooser|popover] | click at=<x>,<y> | ui dump | ui state | ui ping   real mouse events into the window (see QAClicks)
///     net offline | net online        the OS path monitor said so (engine holds everything / probes once); no debounce
///     play-app <App.app> <file>       the one real launch: opens <file> in that app (a finished local test video)
@MainActor final class QAControl {
    private let path: String
    private unowned let app: AppDelegate
    private var executed = 0
    private var lastMod: Date?
    private var watcher: DispatchSourceFileSystemObject?
    private var fd: Int32 = -1
    private var openMenu: NSMenu?

    init(path: String, app: AppDelegate) {
        self.path = path
        self.app = app
        // Captures must show the windows in their active state: a regular app that is frontmost
        // (an accessory app is often refused activation when another app has the focus).
        NSApp.setActivationPolicy(.regular)
        Self.activate()
        FileManager.default.createFile(atPath: path, contents: nil)
        try? "0".write(toFile: path + ".ack", atomically: true, encoding: .utf8)
        // Play never reaches a player here, but the URL it would have handed over lands in `<file>.bench`.
        PlayerLauncher.onBlockedLaunch = { [weak self] url in self?.report("play-url: \(url.absoluteString)") }
        // A file-system watch instead of a timer: the harness must not add wake-ups of its own
        // while idle numbers are measured.
        fd = open(path, O_EVTONLY)
        // The handler runs on a background queue and hops to the main run loop in every mode: a
        // context menu that is open runs the loop in tracking mode, where the main queue is not served.
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

    private func poll() {
        guard let text = try? String(contentsOfFile: path, encoding: .utf8) else { return }
        let lines = text.split(separator: "\n", omittingEmptySubsequences: true).map(String.init)
        guard lines.count > executed else { return }
        // Counted before running: a command may block in a nested menu loop that polls again.
        let batch = Array(lines[executed...])
        executed = lines.count
        for line in batch { run(line) }
        Self.activate()
        try? String(executed).write(toFile: path + ".ack", atomically: true, encoding: .utf8)
    }

    /// Frontmost and key: the main window if open, else any of ours. macOS declines an activation
    /// while the display sleeps or is locked (the windows then draw grey): the harness keeps the
    /// display awake (`caffeinate -d -u`, see env.sh) and sends `activate` before every capture.
    static func activate() {
        NSApp.activate(ignoringOtherApps: true)
        if let front = NSWorkspace.shared.frontmostApplication, front != NSRunningApplication.current {
            // macOS 14+: the frontmost app has to yield; claiming its activation on its behalf is
            // accepted for a QA harness that was started from that app (the terminal).
            _ = NSRunningApplication.current.activate(from: front, options: [.activateAllWindows])
        }
        NSRunningApplication.current.activate(options: [.activateAllWindows])
        if NSApp.keyWindow == nil, let w = NSApp.windows.first(where: { $0.isVisible && $0.canBecomeKey }) {
            w.makeKeyAndOrderFront(nil)
        }
    }

    private func run(_ line: String) {
        let parts = line.split(separator: " ", maxSplits: 1).map(String.init)
        let cmd = parts[0]
        let arg = parts.count > 1 ? parts[1] : ""
        let commands = app.commands!
        switch cmd {
        case "click":
            let (rest, win) = qaTarget(arg)
            QAClicks.command(rest, window: win) { [weak self] in self?.report($0) }
        case "ui":
            let (rest, win) = qaTarget(arg)
            if rest == "dump" { QAClicks.dump(window: win) { report($0) } }
            else if rest == "ping" { QAClicks.ping { [weak self] in self?.report($0) } }
            else { report(qaUIState()) }
        case "net":
            app.networkWatcher?.inject(satisfied: arg == "online")
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
            app.mainWindowController?.libraryModel?.select(arg == "none" ? .sharing : .tracker(arg))
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
                    ?? m.snapshot.trackers.first(where: { $0.name.localizedCaseInsensitiveContains(name) }) { m.select(.tracker(t.id)) }
                else { report("tab: no tracker \(name)") }
            default: report("tab: \(arg)?")
            }
        case "search":
            app.mainWindowController?.libraryModel?.search = arg
        case "select":
            // `select <id>` or `select name1, name2`: rows shown in the window, by id prefix or name.
            guard let m = app.mainWindowController?.libraryModel else { return }
            let wanted = arg.split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) }
            let hits = m.visibleRows.filter { r in wanted.contains { r.id.hasPrefix($0) || r.displayName.localizedCaseInsensitiveContains($0) } }
            m.selection = hits.isEmpty ? [arg] : Set(hits.map(\.id))
        case "rowclick":
            // `rowclick <name> [command|shift]`: a click on that row as the list handles it (anchor and ranges included).
            guard let m = app.mainWindowController?.libraryModel else { return }
            let words = arg.split(separator: " ").map(String.init)
            let flags = Set(words.filter { $0 == "command" || $0 == "shift" })
            let name = words.filter { $0 != "command" && $0 != "shift" }.joined(separator: " ")
            let groups: [RowGroup]
            switch m.section {
            case .tracker: groups = Lists.tracker(m, commands: commands)
            case .sharing: groups = Lists.sharing(m, commands: commands)
            default: groups = Lists.downloads(m, commands: commands)
            }
            let hidden = { (g: RowGroup) in g.parent.map { m.isCollapsed($0) } ?? false }
            let shown = groups.filter { !hidden($0) && ($0.title == nil || !m.isCollapsed($0.id)) }
            let order = shown.flatMap { g in g.rows.map { (group: g.id, id: $0.id) } }
            guard let hit = shown.lazy.flatMap({ g in g.rows.map { (g.id, $0) } }).first(where: { $0.1.displayName.localizedCaseInsensitiveContains(name) || $0.1.id.hasPrefix(name) })
            else { return report("rowclick: no row \(name)") }
            m.click(group: hit.0, id: hit.1.id, command: flags.contains("command"), shift: flags.contains("shift"), order: order)
        case "info":
            app.mainWindowController?.libraryModel?.infoID = arg == "none" ? nil : arg
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
        case "contextmenu":
            // `contextmenu <id> [menubar]`: the row's context menu, open until `contextmenu close`.
            let ids = arg.split(separator: " ").map(String.init)
            if ids.first == "close" { openMenu?.cancelTracking(); return }
            let all = commands.core.snapshot()
            let rows = (all.allDownloading + all.allQueued + all.allRecent + all.sharingBusiest)
                + commands.core.sharingRows(query: RowQuery(group: nil, trackerId: nil, text: "", offset: 0, limit: 400))
            let wanted = ids.first.flatMap { id in id == "first" ? rows.first : rows.first { $0.id.hasPrefix(id) || $0.name.localizedCaseInsensitiveContains(id) || $0.displayName.localizedCaseInsensitiveContains(id) } }
            guard let row = wanted, let win = app.mainWindowController?.nsWindow else { return }
            var entries = commands.entries(for: [row], inMenuBar: ids.contains("menubar"))
            if ids.contains("playwith") {
                // The submenu on its own (a hover-opened submenu is a separate window we cannot summon).
                entries = entries.compactMap { if case let .submenu("Play With", children) = $0 { return children } else { return nil } }.first ?? []
            }
            let menu = MenuBuilder.menu(entries)
            openMenu = menu
            let p = NSPoint(x: win.frame.minX + 520, y: win.frame.maxY - 190)
            DispatchQueue.main.async { menu.popUp(positioning: nil, at: win.contentView?.convert(win.convertFromScreen(NSRect(origin: p, size: .zero)).origin, from: nil) ?? .zero, in: win.contentView) }
        case "devlog":
            devlogCommand(arg)
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
        case "activate":
            // `activate [main|settings|chooser]`: also make that window the key one (a window that
            // is not key draws grey even in the active app).
            let main = app.mainWindowController?.nsWindow, settings = main
            let other = NSApp.windows.first { $0 != main && $0 != settings && $0.isVisible && $0.canBecomeKey && $0.styleMask.contains(.titled) }
            Self.activate()
            switch arg {
            case "main": main?.makeKeyAndOrderFront(nil)
            case "settings": settings?.makeKeyAndOrderFront(nil)
            case "chooser": other?.makeKeyAndOrderFront(nil)
            default: break
            }
            report("active: \(NSApp.isActive), key: \(NSApp.keyWindow != nil), front: \(NSWorkspace.shared.frontmostApplication?.localizedName ?? "-")")
        case "open-file":
            // A .torrent opened from Finder (SPEC 7A.6): the same path as double-clicking one. QA roots only.
            guard ["/private/tmp/", "/tmp/"].contains(where: arg.hasPrefix) else { return report("open-file: only under /private/tmp") }
            app.application(NSApp, open: [URL(fileURLWithPath: arg)])
        case "quit":
            app.quitForReal()
        case "quit-menu":
            // The menu bar menu's Quit row pressed as a click would (accessibility press): ends the process.
            app.statusMenuController?.perform(open: true)
            app.statusMenuController?.qaPressFooter("quit")
        case "policy":
            report(app.qaPolicyLine())
        case "policy-real":
            app.qaPinnedRegular = false
            app.refreshActivationPolicy()
            report(app.qaPolicyLine())
        case "dock-quit":
            // The quit Apple event the Dock menu's Quit sends (no reason), or with a reason: `dock-quit logout`.
            app.qaSendQuitEvent(reason: arg.isEmpty ? nil : arg)
        case "key":
            // `key delete|return|escape [shift]`: a key press posted to the main window (only this app's window).
            let parts = arg.split(separator: " ").map(String.init)
            let codes: [String: (UInt16, String)] = ["delete": (51, "\u{7F}"), "return": (36, "\r"), "escape": (53, "\u{1B}")]
            guard let (code, chars) = parts.first.flatMap({ codes[$0] }), let win = app.mainWindowController?.nsWindow else { return report("key: \(arg)?") }
            let flags: NSEvent.ModifierFlags = parts.contains("shift") ? [.shift] : []
            for type in [NSEvent.EventType.keyDown, .keyUp] {
                if let e = NSEvent.keyEvent(with: type, location: .zero, modifierFlags: flags, timestamp: ProcessInfo.processInfo.systemUptime,
                                            windowNumber: win.windowNumber, context: nil, characters: chars,
                                            charactersIgnoringModifiers: chars, isARepeat: false, keyCode: code) {
                    NSApp.postEvent(e, atStart: false)
                }
            }
        case "fold":
            // `fold <group id>` (active, dormant, error, next, recent…): toggles that group header. On a tracker tab
            // the ids are downloads, sharing (the blocks), downloading, queued, recent, popular, active, dormant, error, dead.
            app.mainWindowController?.libraryModel?.toggleCollapsed(arg)
        case "remove-ask":
            // `remove-ask <name> [delete|menubar]`: the inline Remove… strip under that row; `remove-ask cancel` drops it, `remove-ask confirm` presses Remove.
            if arg == "cancel" { commands.removal.cancel(); return }
            if arg == "confirm" { commands.confirmedRemove(); return }
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
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.8) { [weak self] in
                let m = self?.app.mainWindowController?.libraryModel
                self?.report("getinfo: section \(m?.section == .sharing ? "sharing" : "downloads"), info \(m?.infoID == row.id ? "open" : "closed")")
            }
        case "where":
            let m = app.mainWindowController?.libraryModel
            report("where: section \(m.map { String(describing: $0.section) } ?? "none"), tracker \(m?.trackerID ?? "none"), info \(m?.infoID ?? "none"), selection \(m?.selection.count ?? 0), badge \(m?.downloadsFlag == true ? 1 : 0), window \(m == nil ? "closed" : "open")")
        case "menu":
            app.statusMenuController?.perform(open: arg != "close")
        case "menu-files":
            menuFilesCommand(arg)
        case "menu-titles":
            report("menu: \(app.statusMenuController?.qaTitles ?? "")")
        case "menu-hover":
            // `menu-hover open | quit | none`: the Open / Quit row as a pointer over one side draws it.
            app.statusMenuController?.footerRow?.qaHighlight(arg)
        case "menu-press":
            app.statusMenuController?.footerRow?.qaPress(arg)
        case "starting":
            starting(arg)
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
        if arg == "close" { openMenu?.cancelTracking(); return }
        guard let win = app.mainWindowController?.nsWindow else { return }
        var entries: [MenuEntry] = []
        var at = NSPoint(x: 110, y: 330)
        if let t = qaTracker(arg) {
            entries = TrackerMenus.entries(for: t, commands: commands)
            let sorted = TrackerOrder.sorted(commands.core.snapshot().trackers)
            let n = sorted.firstIndex { $0.id == t.id } ?? 0
            let screen = NSPoint(x: win.frame.minX + 90, y: win.frame.maxY - 195 - CGFloat(n) * 40)
            at = win.contentView?.convert(win.convertFromScreen(NSRect(origin: screen, size: .zero)).origin, from: nil) ?? .zero
        }
        guard !entries.isEmpty else { return report("tracker-menu: none for \(arg)") }
        let menu = MenuBuilder.menu(entries)
        openMenu = menu
        DispatchQueue.main.async { menu.popUp(positioning: nil, at: at, in: win.contentView) }
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
        openMenu = menu
        // Over the Playback row's pop-up (QA captures the General tab at 860 x 700).
        let p = NSPoint(x: 209, y: 570)
        DispatchQueue.main.async { menu.popUp(positioning: auto, at: p, in: content) }
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
        case "folders":
            store.expand(row.id)
            if let st = store.states[row.id], let t = st.tree { for f in t.folderIDs { st.setFolder(f, open: true) } }
        case "filter": store.states[row.id]?.filter = rest
        default:
            guard let st = store.states[row.id], let tree = st.tree else { return report("files: \(row.name) is not open") }
            let fileWords = rest.split(separator: " ", maxSplits: 1).map(String.init)
            guard let pat = fileWords.first, let node = tree.nodes.indices.dropFirst().first(where: { tree.nodes[$0].name.localizedCaseInsensitiveContains(pat) }) else { return report("files: no file \(rest)") }
            switch verb {
            case "select": store.select(torrent: row.id, node: node, command: false, shift: false)
            case "skip": st.toggle([node])
            case "high": st.setPriority([node], .high)
            case "menu":
                store.select(torrent: row.id, node: node, command: false, shift: false)
                var entries = FileRowView.menuEntries(row: row, commands: app.commands, state: st, store: store, tree: tree, node: node)
                if fileWords.count > 1 {
                    // The Priority submenu on its own (a hover-opened submenu is a window we cannot summon).
                    entries = entries.compactMap { if case let .submenu("Priority", c) = $0 { return c } else { return nil } }.first ?? []
                }
                guard let win = app.mainWindowController?.nsWindow else { return }
                let menu = MenuBuilder.menu(entries)
                openMenu = menu
                let p = NSPoint(x: win.frame.minX + 520, y: win.frame.maxY - 330)
                DispatchQueue.main.async { menu.popUp(positioning: nil, at: win.contentView?.convert(win.convertFromScreen(NSRect(origin: p, size: .zero)).origin, from: nil) ?? .zero, in: win.contentView) }
            default: report("files: unknown \(verb)")
            }
        }
    }

    private func scroll(_ arg: String, in window: NSWindow? = nil) {
        guard let root = (window ?? app.mainWindowController?.nsWindow)?.contentView else { return }
        func views(_ v: NSView) -> [NSScrollView] { ((v as? NSScrollView).map { [$0] } ?? []) + v.subviews.flatMap(views) }
        guard let sv = views(root).filter({ ($0.documentView?.frame.height ?? 0) > $0.bounds.height }).max(by: { $0.bounds.width < $1.bounds.width }),
              let doc = sv.documentView else { return }
        let maxY = max(0, doc.frame.height - sv.contentView.bounds.height)
        let f = arg == "bottom" ? 1 : arg == "top" ? 0 : (Double(arg) ?? 0)
        // The top of the list is the negative inset: the toolbar covers the first 50-odd points.
        let top = -sv.contentView.contentInsets.top
        sv.contentView.scroll(to: NSPoint(x: 0, y: top + (maxY - top) * f))
        sv.reflectScrolledClipView(sv.contentView)
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

    /// `devlog open|close|filter|errors|event|expand|state`: drives the tracker log viewer.
    private func devlogCommand(_ arg: String) {
        guard let nav = app.mainWindowController?.nav else { return report("devlog: no window") }
        let core = app.commands.core
        let model = nav.logModel(core: core)
        let parts = arg.split(separator: " ", maxSplits: 1).map(String.init)
        let sub = parts.first ?? ""
        let rest = parts.count > 1 ? parts[1] : ""
        switch sub {
        case "open":
            nav.showTrackerLog = true
            Task { await model.refresh() }
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
            Task {
                await model.refresh()
                report("devlog: \(model.items.count) entries, \(model.items.filter { $0.row.error }.count) errors, dropped \(model.dropped), open \(nav.showTrackerLog), settings dev mode \(app.commands.core.settingsLoad().advanced.developerMode)")
            }
        default:
            report("devlog: unknown command \(arg)")
        }
    }

    /// `win=main|chooser|popover` anywhere in a click / ui argument picks the window the events go to
    /// (default: the main window). Returns the argument without it.
    private func qaTarget(_ arg: String) -> (String, NSWindow?) {
        var words = arg.split(separator: " ").map(String.init)
        var win = app.mainWindowController?.nsWindow
        if let i = words.firstIndex(where: { $0.hasPrefix("win=") }) {
            switch words[i].dropFirst(4) {
            case "chooser": win = app.qaChooserWindow
            case "popover": win = app.startingNotifier?.popoverWindow
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

    /// `ui state`: what the clicks changed, in one line.
    private func qaUIState() -> String {
        let m = app.mainWindowController?.libraryModel
        let f = FilesStore.shared
        let names = (m?.visibleRows ?? []).reduce(into: [String: String]()) { $0[$1.id] = $1.displayName }
        let sel = (m?.selection ?? []).map { String((names[$0] ?? $0).prefix(14)) }.sorted()
        let chev = (m?.visibleRows ?? []).filter { $0.fileCount > 1 && $0.kind == .torrent && !$0.isHeld }.map { String($0.displayName.prefix(10)) }
        let trees = f.states.sorted { $0.key < $1.key }.map { id, st in
            "\(id.prefix(8)):\(st.tree.map { "\($0.wantedCount)/\($0.fileCount)" } ?? "-")\(st.hasPending ? "+pending" : "")"
        }
        return "state: events d\(QAClicks.downs) u\(QAClicks.ups) m\(QAClicks.moves) trees \(trees) chevrons \(chev) rows \(sel) files \(f.selectedTorrent.map { String($0.prefix(8)) } ?? "-") nodes \(f.selectedNodes.sorted()) expanded \(f.expanded.map { String($0.prefix(8)) }.sorted()) \(qaChoiceState)"
    }

    /// The inline file list of the open menu bar menu (`menu open` first):
    ///     menu-files expand <torrent>      a click on the row's chevron (opens, or closes when open; another one closes)
    ///     menu-files press <file|folder|all|none>   a click on a row of the list; `menu-files fold <folder>` its chevron
    ///     menu-files state <torrent>       engine wanted/total, which list is open, rows, footer, menu still tracking
    ///     menu-files context <row>         the titles of the row's right-click menu, its check and Play icon
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

    private func report(_ line: String) {
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
        func scrollViews(_ v: NSView) -> [NSScrollView] {
            (v as? NSScrollView).map { [$0] } ?? [] + v.subviews.flatMap(scrollViews)
        }
        guard let sv = scrollViews(root).filter({ ($0.documentView?.frame.height ?? 0) > $0.bounds.height }).max(by: { $0.bounds.width < $1.bounds.width }) else {
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
        link = window.displayLink(target: self, selector: #selector(frame(_:)))
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
