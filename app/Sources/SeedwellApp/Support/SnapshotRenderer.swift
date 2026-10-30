import AppKit
import SwiftUI

/// Debug-only visual QA: renders the real windows (real AppKit controls, toolbar and sidebar)
/// to PNG at 2x. Launch with `--render-snapshots <dir>`. Not used in normal operation. The menus: MenuSnapshots.
@MainActor struct SnapshotRenderer {
    let core: Core
    let commands: Commands
    let settings: SettingsModel
    let dir: String

    func renderAll() async {
        try? FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        let mainSize = NSSize(width: 1180, height: 820)

        await render("downloads-dark", size: mainSize) { MainWindowView(model: model(.downloads, select: nil), commands: commands, settings: settings, nav: SettingsNavigation()) }
        await render("downloads-info-dark", size: mainSize) {
            let m = model(.downloads, select: "sprite")
            return MainWindowView(model: m, commands: commands, settings: settings, nav: SettingsNavigation()).overlay(alignment: .topTrailing) { infoOverlay(m) }
        }
        await render("downloads-light", size: mainSize, dark: false) { MainWindowView(model: model(.downloads, select: nil), commands: commands, settings: settings, nav: SettingsNavigation()) }
        await render("sharing-dark", size: NSSize(width: 1180, height: 1100)) { MainWindowView(model: model(.sharing, select: nil), commands: commands, settings: settings, nav: SettingsNavigation()) }
        await render("sharing-light", size: NSSize(width: 1180, height: 1100), dark: false) { MainWindowView(model: model(.sharing, select: nil), commands: commands, settings: settings, nav: SettingsNavigation()) }
        await render("tracker-filter-dark", size: NSSize(width: 1180, height: 1100)) { MainWindowView(model: model(.tracker(trackerId: "tracker-z.cc"), select: nil), commands: commands, settings: settings, nav: SettingsNavigation()) }
        await render("downloads-filter-dark", size: mainSize) { MainWindowView(model: model(.tracker(trackerId: "tracker-y.net"), select: nil), commands: commands, settings: settings, nav: SettingsNavigation()) }

        for tab in SettingsTab.allCases {
            let h: CGFloat = tab == .folders ? 1180 : tab == .transfers ? 860 : tab == .rules ? 860 : tab == .folderSync ? 760 : 700
            await render("settings-\(tab.rawValue)-dark", size: NSSize(width: 860, height: h), settings: true) {
                SettingsView(model: settings, nav: nav(tab))
            }
        }
        await render("settings-transfers-light", size: NSSize(width: 860, height: 860), dark: false, settings: true) {
            SettingsView(model: settings, nav: nav(.transfers))
        }
        await render("chooser-dark", size: NSSize(width: 640, height: 640), settings: true) { ChooserView(model: ChooserModel.sample()) }
        await renderSync()
        await renderMenus()
        await render("chooser-light", size: NSSize(width: 640, height: 640), dark: false, settings: true) { ChooserView(model: ChooserModel.sample()) }
    }

    /// Folder sync (SPEC 7G.5): the window page, the menu's submenu (a stand-in: a real menu cannot be drawn
    /// offscreen), the add and edit modals, and turning on Finder badges.
    private func renderSync() async {
        let sync = commands.sync
        let folders = core.syncSnapshot().folders
        let projects = folders.first { $0.conflicts > 0 }?.id
        for dark in [true, false] {
            let suffix = dark ? "dark" : "light"
            await render("sync-window-\(suffix)", size: NSSize(width: 1180, height: 760), dark: dark) {
                let m = model(.folderSync, select: nil)
                m.syncSelection = projects
                return MainWindowView(model: m, commands: commands, settings: settings, nav: SettingsNavigation())
            }
            await render("sync-menu-\(suffix)", size: NSSize(width: 330, height: 420), dark: dark, settings: true) {
                SyncMenuPreview(snapshot: core.syncSnapshot(), hovered: 1, confirm: 2)
            }
            await render("sync-add-local-\(suffix)", size: NSSize(width: 480, height: 250), dark: dark, settings: true) {
                let m = SyncAddModel(core: core)
                m.form.folder = NSHomeDirectory() + "/Documents/Novel"
                return SyncAddView(model: m)
            }
            await render("sync-add-shared-\(suffix)", size: NSSize(width: 480, height: 290), dark: dark, settings: true) {
                let link = (try? core.syncCreateInvite(id: folders[0].id, role: .readOnly, maxUses: 1, validDays: 7, channelId: core.syncSnapshot().folders[0].channelId))?.link ?? ""
                return SyncAddView(model: SyncAddModel(core: core, prefill: link))
            }
            await render("sync-add-bad-\(suffix)", size: NSSize(width: 480, height: 340), dark: dark, settings: true) {
                let m = SyncAddModel(core: core, prefill: "SWW-NOTAKEY")
                m.error = "Folders in iCloud Drive or other cloud storage cannot be synced."
                return SyncAddView(model: m)
            }
            await render("sync-edit-\(suffix)", size: NSSize(width: 500, height: 560), dark: dark, settings: true) {
                let m = SyncEditModel(core: core, id: folders[0].id)
                m.optionsOpen = true
                return SyncEditView(model: m, actions: sync)
            }
            await render("sync-share-\(suffix)", size: NSSize(width: 460, height: 440), dark: dark, settings: true) {
                _ = try? core.syncCreateInvite(id: folders[0].id, role: .readWrite, maxUses: 1, validDays: 7, channelId: nil)
                return SyncShareView(model: SyncShareModel(core: core, id: folders[0].id, copied: true), actions: sync)
            }
            await render("sync-share-period-\(suffix)", size: NSSize(width: 460, height: 360), dark: dark, settings: true) {
                // A sharing period running: the countdown with Extend.
                _ = try? core.syncSetShareUntil(id: folders[0].id, untilS: UInt64(Date().timeIntervalSince1970) + 7 * 86_400 - 3_600)
                return SyncShareView(model: SyncShareModel(core: core, id: folders[0].id), actions: sync)
            }
            await render("sync-share-new-\(suffix)", size: NSSize(width: 460, height: 330), dark: dark, settings: true) {
                // Share ▸ Seedwell on a folder not synced yet: add and share in one.
                SyncShareView(model: SyncShareModel(core: core, newFolder: NSHomeDirectory() + "/Documents/Novel"), actions: sync)
            }
            for (name, state) in [("off", (false, false)), ("badges-on", (true, false)), ("on", (true, true))] {
                await render("finder-setup-\(name)-\(suffix)", size: NSSize(width: 460, height: state.0 ? 260 : 330), dark: dark, settings: true) {
                    FinderSetupView(model: FinderSetupModel(check: { (badges: state.0, shareMenu: state.1) }))
                }
            }
        }
        // Channels (SPEC 7G.7): the mock's first channel's page, and the New Channel window.
        for dark in [true, false] {
            let suffix = dark ? "dark" : "light"
            await render("chats-window-\(suffix)", size: NSSize(width: 1180, height: 760), dark: dark) {
                let m = model(.chat(channelId: core.chatChannels().first?.id ?? "", topicId: nil), select: nil)
                return MainWindowView(model: m, commands: commands, settings: settings, nav: SettingsNavigation())
            }
            await render("chats-topic-\(suffix)", size: NSSize(width: 1180, height: 760), dark: dark) {
                let c = core.chatChannels().first?.id ?? ""
                let m = model(.chat(channelId: c, topicId: core.chatTopics(channelId: c).first?.id), select: nil)
                return MainWindowView(model: m, commands: commands, settings: settings, nav: SettingsNavigation())
            }
            await render("chats-agents-\(suffix)", size: NSSize(width: 1180, height: 760), dark: dark) {
                // Agents writing Markdown, each with its name and computer.
                let c = core.chatChannels().first { $0.name == "Release notes" }?.id ?? ""
                return MainWindowView(model: model(.chat(channelId: c, topicId: nil), select: nil), commands: commands, settings: settings, nav: SettingsNavigation())
            }
            await render("chats-direct-\(suffix)", size: NSSize(width: 1180, height: 760), dark: dark) {
                let m = model(.direct(deviceId: core.chatDirects().first?.deviceId ?? ""), select: nil)
                return MainWindowView(model: m, commands: commands, settings: settings, nav: SettingsNavigation())
            }
            await render("chat-markdown-\(suffix)", size: NSSize(width: 820, height: 1700), dark: dark, settings: true) {
                // Complex messages (ChatSamples) as the conversation shows them: in a scroll view, as there.
                ScrollView { VStack(alignment: .leading, spacing: 0) {
                    ForEach(Array(ChatSamples.messages.enumerated()), id: \.offset) { n, s in
                        ChatMessageRow(message: SyncMessage(id: "S\(n)", channelId: "C", authorDevice: "D\(n)", authorName: s.computer, authorUser: "", isMe: false,
                                                            agent: s.agent, atS: 1_792_953_675 + UInt64(n) * 60, replyTo: nil, topic: nil,
                                                            kind: .text, body: s.body),
                                       header: true, reply: nil,
                                       tags: s.body == ChatSamples.bugMail
                                           ? [ChatTagOwner(owner: "kglite", isYou: false, tags: ["seen", "backlog"]), ChatTagOwner(owner: "You", isYou: true, tags: ["needs-triage"])]
                                           : [],
                                       onReply: nil)
                    }
                }
                .padding(.horizontal, 22).padding(.vertical, 12) }
                .frame(width: 820, height: 1700)
                .background(.background)
            }
            await render("chat-new-\(suffix)", size: NSSize(width: 440, height: 240), dark: dark, settings: true) {
                ChatNewView(model: ChatNewModel(core: core, prefill: nil))
            }
        }
        await render("settings-folderSync-light", size: NSSize(width: 860, height: 900), dark: false, settings: true) {
            SettingsView(model: settings, nav: nav(.folderSync))
        }
    }

    private func nav(_ tab: SettingsTab) -> SettingsNavigation {
        let n = SettingsNavigation()
        n.tab = tab
        return n
    }

    private func model(_ s: LibraryModel.Section, select: String?) -> LibraryModel {
        let m = LibraryModel(core: core, section: s)
        if select == "sprite" { m.setSelection(m.snapshot.downloading.dropFirst().first.map { [$0.id] } ?? []) }
        return m
    }

    /// Stands in for the real popover (a separate window that cannot be captured offscreen).
    @ViewBuilder private func infoOverlay(_ m: LibraryModel) -> some View {
        if let row = m.snapshot.downloading.dropFirst().first {
            InfoCard(core: core, row: row, commands: commands)
                .background(RoundedRectangle(cornerRadius: 14).fill(.regularMaterial))
                .overlay(RoundedRectangle(cornerRadius: 14).stroke(.primary.opacity(0.12), lineWidth: 0.5))
                .shadow(color: .black.opacity(0.35), radius: 24, y: 8)
                .padding(.top, 205).padding(.trailing, 40)
        }
    }

    private func render<V: View>(_ name: String, size: NSSize, dark: Bool = true, settings: Bool = false,
                                 @ViewBuilder _ content: () -> V) async {
        let w = SnapshotWindow(contentViewController: NSHostingController(rootView: content()))
        w.styleMask = settings ? [.titled, .closable, .fullSizeContentView] : [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView]
        w.appearance = NSAppearance(named: dark ? .darkAqua : .aqua)
        w.titlebarAppearsTransparent = true
        if settings { w.titleVisibility = .hidden }
        if !settings { w.titleVisibility = .hidden; w.toolbarStyle = .unified }
        w.setContentSize(size)
        w.setFrameOrigin(SnapshotWindow.offscreen)
        w.orderFront(nil)
        w.makeFirstResponder(nil)
        defer { w.contentViewController = nil; w.close() }
        guard let frame = w.contentView?.superview, let rep = await SnapshotWindow.settledImage(of: frame),
              let png = rep.representation(using: .png, properties: [:]) else { return }
        try? png.write(to: URL(fileURLWithPath: dir).appendingPathComponent("\(name).png"))
        print("wrote \(name).png (\(rep.pixelsWide)x\(rep.pixelsHigh))")
    }
}

/// A snapshot's window: off every screen and never really key, so the renderer never becomes the active app and never
/// takes the user's keys or focus (a Space typed for another app once replaced the selected link of the add modal's
/// focused field). It draws as the key and main window all the same (accent colours, not the grey inactive look),
/// may be larger than the screen (snapshots are not clipped on small displays) and takes no input.
class SnapshotWindow: NSWindow {
    /// Far from every screen: nothing shows, and the pointer never lights a row.
    static let offscreen = NSPoint(x: -30_000, y: -30_000)

    override var isKeyWindow: Bool { true }
    override var isMainWindow: Bool { true }
    override var canBecomeKey: Bool { false }
    override var canBecomeMain: Bool { false }

    override func constrainFrameRect(_ frameRect: NSRect, to screen: NSScreen?) -> NSRect { frameRect }

    /// `view` drawn at 2x (whatever the display) once it has settled: SwiftUI lays out and loads over several run loop
    /// turns, so it is drawn every 50 ms until two drawings in a row are the same, or for `limit` at most (something
    /// that keeps moving). `before` runs before each drawing (a state the window may undo, such as a row's hover).
    static func settledImage(of view: NSView, limit: Duration = .seconds(1.3), before: () -> Void = {}) async -> NSBitmapImageRep? {
        await settled(limit: limit) {
            before()
            view.layoutSubtreeIfNeeded()
            return image(of: view)
        }
    }

    /// What `draw` draws, drawn every 50 ms until two drawings in a row are the same, or for `limit` at most.
    static func settled(limit: Duration = .seconds(1.3), _ draw: () -> NSBitmapImageRep?) async -> NSBitmapImageRep? {
        let clock = ContinuousClock()
        let end = clock.now + limit
        var last: Data?
        while true {
            try? await Task.sleep(for: .milliseconds(50))
            guard let rep = draw() else { return nil }
            let data = rep.bitmapData.map { Data(bytes: $0, count: rep.bytesPerRow * rep.pixelsHigh) }
            if data == last || clock.now >= end { return rep }
            last = data
        }
    }

    /// `view` drawn now at 2x: a retina-sized rep whose point size is the view's.
    static func image(of view: NSView) -> NSBitmapImageRep? {
        let b = view.bounds
        guard let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(b.width * 2), pixelsHigh: Int(b.height * 2),
                                         bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                                         colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0) else { return nil }
        rep.size = b.size
        view.cacheDisplay(in: b, to: rep)
        return rep
    }

    override func sendEvent(_ event: NSEvent) {
        if !Self.isInput(event.type) { super.sendEvent(event) }
    }

    /// Key equivalents (Return, Escape, ⌘-keys) are handled here and go no further, not to the menu either.
    override func performKeyEquivalent(with event: NSEvent) -> Bool { true }

    /// Typing, clicks, scrolling and the pointer (hover would light rows): what the user does, not the renderer.
    static func isInput(_ type: NSEvent.EventType) -> Bool {
        switch type {
        case .keyDown, .keyUp, .flagsChanged,
             .leftMouseDown, .leftMouseUp, .leftMouseDragged, .rightMouseDown, .rightMouseUp, .rightMouseDragged,
             .otherMouseDown, .otherMouseUp, .otherMouseDragged, .mouseMoved, .mouseEntered, .mouseExited, .cursorUpdate,
             .scrollWheel, .magnify, .rotate, .swipe, .smartMagnify, .gesture, .beginGesture, .endGesture, .pressure,
             .tabletPoint, .tabletProximity:
            true
        default:
            false
        }
    }
}

/// The Folder Sync submenu drawn from its real row views on the menu's material (QA: a real NSMenu cannot be
/// captured offscreen). Row `hovered` shows the hover buttons; a confirmation is open under row `confirm`.
private struct SyncMenuPreview: NSViewRepresentable {
    let snapshot: SyncSnapshot
    var hovered: Int?
    var confirm: Int?

    func makeNSView(context: Context) -> NSView {
        let width: CGFloat = 330
        let removal = SyncRemoval()
        if let c = confirm, snapshot.folders.indices.contains(c) { removal.toggle(id: snapshot.folders[c].id) }
        let m = syncMenuModel(snapshot: snapshot, removal: removal)
        var views: [NSView] = []
        let top = NSView(frame: NSRect(x: 0, y: 0, width: width, height: 5))
        views.append(top)
        for (i, r) in m.rows.enumerated() {
            let row = MenuSyncRow(row: r)
            // A row drops its hover when it joins a window: light it once it is there.
            if i == hovered { DispatchQueue.main.async { row.hovered = true } }
            views.append(row)
            if let c = m.confirm, c.folderId == r.id { views.append(SyncConfirmPanel(confirm: c, width: width)) }
        }
        let sep = NSBox(frame: NSRect(x: 14, y: 0, width: width - 28, height: 11))
        sep.boxType = .separator
        let sepWrap = NSView(frame: NSRect(x: 0, y: 0, width: width, height: 11))
        sep.frame = NSRect(x: 14, y: 5, width: width - 28, height: 1)
        sepWrap.addSubview(sep)
        views.append(sepWrap)
        views.append(MenuCheckRow(content: .init(title: m.add.title), width: width) {})
        let total = views.reduce(0) { $0 + $1.frame.height } + 5
        let root = NSVisualEffectView(frame: NSRect(x: 0, y: 0, width: width, height: total))
        root.material = .menu
        root.state = .active
        var y = total
        for v in views {
            y -= v.frame.height
            v.frame = NSRect(x: 0, y: y, width: width, height: v.frame.height)
            root.addSubview(v)
        }
        return root
    }

    func updateNSView(_ nsView: NSView, context: Context) {}
}
