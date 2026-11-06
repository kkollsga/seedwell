import AppKit
import SwiftUI

/// Debug-only visual QA: renders the real windows (real AppKit controls, toolbar and sidebar)
/// to PNG at 2x. Launch with `--render-snapshots <dir>`. Not used in normal operation. The menus: MenuSnapshots.
@MainActor struct SnapshotRenderer {
    let core: Core
    let commands: Commands
    let settings: SettingsModel
    let dir: String
    /// `SEEDWELL_RENDER_ONLY=prompt-,settings-banner-`: only the snapshots whose names start so (and no menus).
    private let only = ProcessInfo.processInfo.environment["SEEDWELL_RENDER_ONLY"].flatMap { $0.isEmpty ? nil : $0.split(separator: ",").map(String.init) }

    private func wanted(_ name: String) -> Bool { only.map { $0.contains { name.hasPrefix($0) } } ?? true }

    func renderAll() async {
        try? FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        // SEEDWELL_SNAPSHOTS=files: only the file lists, the chooser and the notifier (a quick before/after of them).
        if ProcessInfo.processInfo.environment["SEEDWELL_SNAPSHOTS"] == "files" { return await renderFileLists() }
        // The menu bar icon, plain and with the attention dot (IconSnapshots); `SEEDWELL_RENDER_ONLY=menubar-icon` alone.
        if wanted("menubar-icon") { renderMenuBarIcons() }
        // The proxy icon (IconSnapshots): its contact sheet, and in place; `SEEDWELL_RENDER_ONLY=proxy-shield` alone.
        if wanted("proxy-shield") { renderProxyShields() }
        await renderProxyShieldRows()
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
        await render("chooser-dark", size: NSSize(width: 640, height: 640), settings: true) { ChooserView(chooser: Chooser(info: chooserSample(), origin: "Opened from Finder")) }
        await renderTrackerInfo()
        await renderSync()
        if only == nil { await renderMenus() }
        await render("chooser-light", size: NSSize(width: 640, height: 640), dark: false, settings: true) { ChooserView(chooser: Chooser(info: chooserSample(), origin: "Opened from Finder")) }
        await renderPrompts()
        await renderFileLists()
    }

    /// The prompts (an add request of a watch folder in "Ask first" mode, the location move, the backlog offer) and the
    /// settings banner, from fixed sample inputs (never the user's data).
    private func renderPrompts() async {
        let request = AddRequestInfo(path: "/tmp/sample.torrent", name: "Night of the Living Dead (1968)", totalBytes: 1_468_006_400,
                                     category: .movie, tracker: "tracker-y.net")
        let item = { (i: Int, cross: Bool) in
            MoveItemInfo(id: "m\(i)", name: "Sample \(i)", bytes: 4_000_000_000, from: "/Volumes/Old/Movies", to: "/Volumes/New/Movies", crossVolume: cross)
        }
        let banner = SettingsModel(core: core)
        banner.noteRejected("config.toml: network.listen_port: 80 is not a valid value (below 1024); the running values stay")
        banner.noteReloaded(warnings: [], restart: ["uTP", "listening interface"])
        for dark in [true, false] {
            let suffix = dark ? "dark" : "light"
            await renderAlert("prompt-add-request-\(suffix)", dark: dark, ChooserPresenter.addRequestAlert(addRequestPrompt(request: request)))
            if let p = locationMovePrompt(proposal: MoveProposalInfo(items: [item(1, true), item(2, false), item(3, false)],
                                                                     totalBytes: 12_000_000_000, crossVolume: 1)) {
                await renderAlert("prompt-location-move-\(suffix)", dark: dark, AppDelegate.locationMoveAlert(p))
            }
            if let p = locationMovePrompt(proposal: MoveProposalInfo(items: [item(1, false)], totalBytes: 4_000_000_000, crossVolume: 0)) {
                await renderAlert("prompt-location-move-one-\(suffix)", dark: dark, AppDelegate.locationMoveAlert(p))
            }
            await renderAlert("prompt-backlog-\(suffix)", dark: dark,
                              AppDelegate.backlogAlert(backlogOffer(count: 5, folderPath: "/Users/me/Downloads", afterAdd: .trash)))
            await render("settings-banner-\(suffix)", size: NSSize(width: 860, height: 700), dark: dark, settings: true) {
                SettingsView(model: banner, nav: nav(.general))
            }
        }
    }

    /// A tracker's Info page (SPEC 7E): an automatic tracker, then a favorite (the mock lists none, so one is saved in
    /// the mock's state and drawn on its own) with a speed limit, after an address with a passkey was added, with a bad
    /// address being edited, and with its Remove from Favorites question.
    private func renderTrackerInfo() async {
        let size = NSSize(width: 760, height: 1180)
        if let auto = core.snapshot().trackers.first(where: { $0.id == "tracker-y.net" }) {
            await render("tracker-info-auto-dark", size: size, settings: true) {
                TrackerInfoPage(model: model(.tracker(trackerId: auto.id), select: nil), tracker: auto, commands: commands).background(.background)
            }
        }
        guard let fav = try? core.favoriteAdd(groupId: "tracker-z.cc") else { return }
        let t = TrackerSummary(id: fav.id, name: fav.name, count: 0, favorite: true, hidden: false, hideOnlySeeding: false, private: true)
        let e = commands.favorites
        e.reload()
        e.setLimits(fav.id, down: 5_000_000, up: 0)
        await CoreCalls.settled()
        func page(_ name: String, dark: Bool = true) async {
            await render(name, size: size, dark: dark, settings: true) {
                TrackerInfoPage(model: model(.tracker(trackerId: "tracker-z.cc"), select: nil), tracker: t, commands: commands).background(.background)
            }
        }
        await page("tracker-info-favorite-dark")
        e.run(fav.id, .addAddress)
        await CoreCalls.settled()
        e.text = "https://tracker.example.org/a/0123456789abcdef0123456789abcdef/announce?passkey=XYZ"
        e.commit()
        await CoreCalls.settled()
        await page("tracker-info-passkey-dark")
        e.run(fav.id, .editAddress(index: 0))
        await CoreCalls.settled()
        e.text = "bad address!"
        e.commit()
        await CoreCalls.settled()
        await page("tracker-info-editing-dark")
        e.cancel()
        e.askRemove(fav.id)
        await page("tracker-info-removal-light", dark: false)
        e.cancelRemoval()
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
            let link = (try? await core.syncCreateInvite(id: folders[0].id, role: .readOnly, maxUses: 1, validDays: 7, channelId: core.syncSnapshot().folders[0].channelId))?.link ?? ""
            await render("sync-add-shared-\(suffix)", size: NSSize(width: 480, height: 290), dark: dark, settings: true) {
                SyncAddView(model: SyncAddModel(core: core, prefill: link))
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
            _ = try? await core.syncCreateInvite(id: folders[0].id, role: .readWrite, maxUses: 1, validDays: 7, channelId: nil)
            await render("sync-share-\(suffix)", size: NSSize(width: 460, height: 440), dark: dark, settings: true) {
                SyncShareView(model: SyncShareModel(core: core, id: folders[0].id, copied: true), actions: sync)
            }
            // A sharing period running: the countdown with Extend.
            _ = try? await core.syncSetShareUntil(id: folders[0].id, untilS: UInt64(Date().timeIntervalSince1970) + 7 * 86_400 - 3_600)
            await render("sync-share-period-\(suffix)", size: NSSize(width: 460, height: 360), dark: dark, settings: true) {
                SyncShareView(model: SyncShareModel(core: core, id: folders[0].id), actions: sync)
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
            await render("chats-agents-\(suffix)", size: NSSize(width: 1180, height: 1380), dark: dark) {
                // Agents writing Markdown, each with its name and computer.
                let c = core.chatChannels().first { $0.name == "Release notes" }?.id ?? ""
                return MainWindowView(model: model(.chat(channelId: c, topicId: nil), select: nil), commands: commands, settings: settings, nav: SettingsNavigation())
            }
            await render("chats-direct-\(suffix)", size: NSSize(width: 1180, height: 760), dark: dark) {
                let m = model(.direct(deviceId: core.chatDirects().first?.deviceId ?? ""), select: nil)
                return MainWindowView(model: m, commands: commands, settings: settings, nav: SettingsNavigation())
            }
            await render("chat-markdown-\(suffix)", size: NSSize(width: 820, height: 1700), dark: dark, settings: true) {
                // Complex messages (the core's `chatSampleMessages`) as the conversation shows them: in a scroll view, as there.
                ScrollView { VStack(alignment: .leading, spacing: 0) {
                    ForEach(Self.sampleCards(), id: \.id) { ChatMessageRow(card: $0) }
                }
                .padding(.horizontal, Theme.Space.xl).padding(.vertical, Theme.Space.l) }
                .frame(width: 820, height: 1700)
                .background(.background)
            }
            await render("chat-new-\(suffix)", size: NSSize(width: 440, height: 240), dark: dark, settings: true) {
                ChatNewView(model: ChatNewModel(core: core, prefill: nil))
            }
            let notes = core.chatChannels().first { $0.name == "Release notes" }?.id ?? ""
            await render("chat-reply-\(suffix)", size: NSSize(width: 1180, height: 760), dark: dark) {
                // Reply on the other person's message: the reply bar on top of the composer.
                let m = model(.chat(channelId: notes, topicId: nil), select: nil)
                m.loadChatMessages()
                if let card = m.chatPage?.messages.last(where: { $0.who == .theirs }) { m.reply(to: card) }
                return MainWindowView(model: m, commands: commands, settings: settings, nav: SettingsNavigation())
            }
            await render("chat-code-copy-\(suffix)", size: NSSize(width: 820, height: 520), dark: dark, settings: true) {
                // The pointer on a code block (an agent's Markdown): its copy button in the top-right corner.
                let m = model(.chat(channelId: notes, topicId: nil), select: nil)
                m.loadChatMessages()
                let cards = (m.chatPage?.messages ?? []).filter { $0.who == .ourAgent }
                return ScrollView { VStack(alignment: .leading, spacing: 0) {
                    ForEach(cards, id: \.id) { ChatMessageRow(card: $0, core: core) }
                }
                .padding(.horizontal, Theme.Space.xl).padding(.vertical, Theme.Space.l) }
                .environment(\.chatCodeHover, true)
                .frame(width: 820, height: 520)
                .background(.background)
            }
            // The pointer on a message: its time, Tag and Reply under it, at the right end of its line (a message inside a
            // run: the user's first; the last of a run: the other person's), and in a private conversation.
            let direct = core.chatDirects().first?.deviceId ?? ""
            for (name, place, pick) in [("inrun", LibraryModel.Section.chat(channelId: notes, topicId: nil), { (c: [ChatMessageCard]) in c.first { $0.who == .mine && $0.meta == nil } }),
                                        ("last", .chat(channelId: notes, topicId: nil), { c in c.last { $0.who == .theirs } }),
                                        ("direct", .direct(deviceId: direct), { c in c.first { $0.meta == nil && $0.who == .theirs } }),
                                        ("direct-last", .direct(deviceId: direct), { c in c.first { $0.who == .mine } })] {
                await render("chat-hover-\(name)-\(suffix)", size: NSSize(width: 820, height: 640), dark: dark, settings: true) {
                    let m = model(place, select: nil)
                    m.loadChatMessages()
                    let page = m.chatPage
                    let cards = page?.messages ?? []
                    let hovered = pick(cards)?.id
                    return ScrollView { VStack(alignment: .leading, spacing: 0) {
                        ForEach(cards, id: \.id) { ChatMessageRow(card: $0, core: core, bubbles: page?.bubbles ?? false, hovered: $0.id == hovered) }
                    }
                    .padding(.horizontal, Theme.Space.xl).padding(.vertical, Theme.Space.l) }
                    .frame(width: 820, height: 640)
                    .background(.background)
                }
            }
        }
        await renderFiles()
        await renderImages()
        await renderImageLayouts()
        await render("settings-folderSync-light", size: NSSize(width: 860, height: 900), dark: false, settings: true) {
            SettingsView(model: settings, nav: nav(.folderSync))
        }
    }

    /// The core's sample conversation as cards, in a channel this Mac may only read (no Reply or tags under the pointer).
    nonisolated static func sampleCards() -> [ChatMessageCard] {
        let c = SyncChannel(id: "C", name: "Samples", role: .readOnly, shareUntilS: nil, members: 2, online: 1, unread: 0, lastMessageS: nil,
                            muted: false, agents: true, wakeCommand: nil, progress: nil, problem: nil, waiting: nil, gone: [])
        let read = ChatPageRead(messages: chatSampleMessages(), pending: [], follow: nil, directAgents: .off)
        let input = ChatPageInput(place: .chat(channelId: "C", topicId: nil), title: c.name, channel: c, topic: nil, direct: nil, read: read,
                                  members: [], nowUnixS: UInt64(Date().timeIntervalSince1970))
        return chatPageModel(input: input, clock: MacClock(), host: hostWindowsAll())?.messages ?? []
    }

    /// Files in chat (SPEC 7G.7 Files): a card in each state, the pending sends, the question before opening a script,
    /// and a private message with a file. Fixtures made by the core (`chatFileSamples`, the mock's conversation), never
    /// chat state.
    private func renderFiles() async {
        let samples = chatFileSamples()
        for dark in [true, false] {
            let suffix = dark ? "dark" : "light"
            for s in samples.cards {
                await render("chat-file-\(s.label)-\(suffix)", size: NSSize(width: 420, height: 130), dark: dark, settings: true) {
                    ChatFileCardView(core: core, place: "", card: s.card).padding(16)
                        .frame(width: 420, height: 130, alignment: .topLeading).background(.background)
                }
            }
            await render("chat-file-pending-\(suffix)", size: NSSize(width: 620, height: 140), dark: dark, settings: true) {
                ChatPendingSendsView(core: core, place: "", pending: samples.pending) {}.padding(16)
                    .frame(width: 620, height: 140, alignment: .topLeading).background(.background)
            }
            await renderAlert("chat-file-open-script-\(suffix)", dark: dark, ChatFileCardView.alert(samples.confirm))
            await render("chat-file-direct-\(suffix)", size: NSSize(width: 1180, height: 760), dark: dark) {
                let m = model(.direct(deviceId: core.chatDirects().first?.deviceId ?? ""), select: nil)
                return MainWindowView(model: m, commands: commands, settings: settings, nav: SettingsNavigation())
            }
        }
    }

    /// Images in a conversation, every shape (the core's `chatImageLayoutSamples`): a wide photo, a tall screenshot, a
    /// small picture and an icon at their own sizes, and a five-image gallery (its second image shown) with a file
    /// under it, in a channel and in a private conversation. Never chat state.
    private func renderImageLayouts() async {
        // Made off the main thread: drawing nine pictures at Retina size takes a moment.
        let samples = await Task.detached { chatImageLayoutSamples(maxSide: ChatImageLayout.side) }.value
        let byLabel = Dictionary(uniqueKeysWithValues: samples.map { ($0.label, $0.image) })
        let byHash = Dictionary(uniqueKeysWithValues: samples.map { ($0.image.hash, $0) })
        let file = chatFileSamples().cards.first { $0.label == "available" }?.card
        let start = UInt64(Date().timeIntervalSince1970) - 3_600
        let me = "D0"
        func message(_ n: Int, _ device: String, _ name: String, _ user: String, agent: String? = nil, _ body: String, _ labels: [String], files: [ChatFileCard] = []) -> SyncMessage {
            SyncMessage(id: "img\(n)-c0ffee", channelId: "C", authorDevice: device, authorName: name, authorUser: user, isMe: device == me, agent: agent,
                        atS: start + UInt64(n) * 400, replyTo: nil, topic: nil, kind: .text, body: body,
                        images: labels.compactMap { byLabel[$0] }, files: files, place: "C")
        }
        let messages = [
            message(1, "D1", "Kari's iPhone", "Kari", "Utsikten fra hytta i kveld", ["wide"]),
            message(2, me, "Kristian's MacBook Pro", "Kristian", "", ["tall"]),
            message(3, "D1", "Kari's iPhone", "Kari", "Ny logo-skisse", ["small"]),
            message(4, "D2", "Studio iMac", "", agent: "Website", "The favicon at its own size:", ["icon"]),
            message(5, me, "Kristian's MacBook Pro", "Kristian", "Bilder fra turen: utsikten fra toppen ![Image 2](att:2), og hytta i snøen ![Image 4](att:4).", (1...5).map { "gallery-\($0)" }, files: file.map { [$0] } ?? []),
        ]
        let channel = SyncChannel(id: "C", name: "Hytta", role: .owner, shareUntilS: nil, members: 3, online: 2, unread: 0, lastMessageS: nil,
                                  muted: false, agents: true, wakeCommand: nil, progress: nil, problem: nil, waiting: nil, gone: [])
        let read = ChatPageRead(messages: messages, pending: [], follow: nil, directAgents: .off, ownDevices: [me])
        let inChannel = ChatPageInput(place: .chat(channelId: "C", topicId: nil), title: "Hytta", channel: channel, topic: nil, direct: nil, read: read,
                                      members: [], nowUnixS: UInt64(Date().timeIntervalSince1970))
        let direct = SyncDirect(deviceId: "D1", name: "Kari's iPhone", user: "Kari", online: true, unread: 0, lastMessageS: nil)
        let privately = ChatPageRead(messages: messages.filter { $0.authorDevice != "D2" }, pending: [], follow: nil, directAgents: .off, ownDevices: [me])
        let inDirect = ChatPageInput(place: .direct(deviceId: "D1"), title: "Kari (Kari's iPhone)", channel: nil, topic: nil, direct: direct, read: privately,
                                     members: [], nowUnixS: UInt64(Date().timeIntervalSince1970))
        for (name, input) in [("channel", inChannel), ("direct", inDirect)] {
            guard let page = chatPageModel(input: input, clock: MacClock(), host: hostWindowsAll()) else { continue }
            for dark in [true, false] {
                await render("chat-images-\(name)-\(dark ? "dark" : "light")", size: NSSize(width: 900, height: 2300), dark: dark, settings: true) {
                    ScrollView { VStack(alignment: .leading, spacing: 0) {
                        ForEach(page.messages, id: \.id) { ChatMessageRow(card: $0, core: core, bubbles: page.bubbles) }
                    }
                    .frame(maxWidth: ChatLook.column)
                    .padding(.horizontal, Theme.Space.xl).padding(.vertical, Theme.Space.l) }
                    .environment(\.chatImageSamples, byHash)
                    .environment(\.chatGalleryStart, 1)
                    .frame(width: 900, height: 2300)
                    .background(.background)
                }
            }
        }
    }

    /// Images in chat (SPEC 7G.7 Attachments): an image in each state with its preview or picture, and the composer's
    /// chips. Fixtures made by the core (`chatImageSamples`), never chat state.
    private func renderImages() async {
        let samples = await Task.detached { chatImageSamples(maxSide: ChatImageLayout.side) }.value
        for dark in [true, false] {
            let suffix = dark ? "dark" : "light"
            for s in samples.images {
                let size = ChatImageLayout.fit(width: s.image.width, height: s.image.height)
                let canvas = NSSize(width: size.width + 32, height: size.height + 52)
                await render("chat-image-\(s.label)-\(suffix)", size: canvas, dark: dark, settings: true) {
                    ChatImageTile(core: core, channelId: "", image: s.image, sample: s).padding(16)
                        .frame(width: canvas.width, height: canvas.height, alignment: .topLeading).background(.background)
                }
            }
            await render("chat-image-chips-\(suffix)", size: NSSize(width: 620, height: 120), dark: dark, settings: true) {
                let images = ChatComposerImages()
                images.items = samples.drafts.map {
                    ChatComposerImage(input: ChatImageInput(name: $0.name, path: nil, bytes: nil), draft: $0, chip: NSImage(data: $0.previewPng))
                }
                return ChatComposerChips(images: images, core: core).padding(16)
                    .frame(width: 620, height: 120, alignment: .topLeading).background(.background)
            }
            // The composer after two images were dropped into its text: each reference where it landed (the core's
            // `chatComposerWithImages`, as a drop calls it), set apart in the accent colour, and the two attachments.
            let channel = core.chatChannels().first?.id ?? ""
            let m = model(.chat(channelId: channel, topicId: nil), select: nil)
            m.loadChatMessages()
            var text = "Before and after the fix: which one reads better?"
            for (n, word) in ["Before", "after"].enumerated() {
                let r = (text as NSString).range(of: word)
                let at = ChatTextRange(start: UInt32(r.location + r.length), length: 0)
                text = chatComposerWithImages(draft: text, first: UInt32(n), count: 1, at: at)?.text ?? text
            }
            if let page = m.chatPage, page.composer.canPost {
                await render("chat-composer-drop-\(suffix)", size: NSSize(width: 720, height: 150), dark: dark, settings: true) {
                    let images = ChatComposerImages()
                    // The samples' one picture, dropped twice.
                    let picture = samples.drafts.filter { !$0.file }.prefix(1)
                    images.items = (picture + picture).map {
                        ChatComposerImage(input: ChatImageInput(name: $0.name, path: nil, bytes: nil), draft: $0, chip: NSImage(data: $0.previewPng))
                    }
                    return ChatComposerView(model: m, page: page, draft: .constant(text), selection: .constant(NSRange(location: 0, length: 0)),
                                            typing: .constant(false), images: images, send: {})
                        .padding(Theme.Space.l).frame(width: 720, height: 150, alignment: .bottom).background(.background)
                }
            }
        }
    }

    /// An alert's window, drawn as it is laid out (never run: nothing waits for an answer).
    private func renderAlert(_ name: String, dark: Bool, _ alert: NSAlert) async {
        guard wanted(name) else { return }
        UIAction.mark("render \(name)")
        alert.window.animationBehavior = .none
        alert.window.appearance = NSAppearance(named: dark ? .darkAqua : .aqua)
        alert.layout()
        alert.window.setFrameOrigin(SnapshotWindow.offscreen)
        alert.window.orderFront(nil)
        defer { alert.window.close() }
        guard let frame = alert.window.contentView?.superview, let rep = await SnapshotWindow.settledImage(of: frame),
              let png = rep.representation(using: .png, properties: [:]) else { return }
        try? png.write(to: URL(fileURLWithPath: dir).appendingPathComponent("\(name).png"))
        print("wrote \(name).png (\(rep.pixelsWide)x\(rep.pixelsHigh))")
    }

    private func nav(_ tab: SettingsTab) -> SettingsNavigation {
        let n = SettingsNavigation()
        n.tab = tab
        return n
    }

    func model(_ s: LibraryModel.Section, select: String?) -> LibraryModel {
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

    func render<V: View>(_ name: String, size: NSSize, dark: Bool = true, settings: Bool = false,
                         @ViewBuilder _ content: () -> V) async {
        guard wanted(name) else { return }
        UIAction.mark("render \(name)")
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

    /// No ordering animation: it would never end off every screen. Each one kept a system worker thread waiting
    /// (`-[NSAnimation _runBlocking]`) for good; a render left sixty of them, the pool ran dry, and the shutdown of the
    /// quit after it never got a thread.
    override init(contentRect: NSRect, styleMask style: NSWindow.StyleMask, backing backingStoreType: NSWindow.BackingStoreType, defer flag: Bool) {
        super.init(contentRect: contentRect, styleMask: style, backing: backingStoreType, defer: flag)
        animationBehavior = .none
    }

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
        if let c = confirm, snapshot.folders.indices.contains(c) { removal.toggle(id: snapshot.folders[c].id, onlyHere: 0) }
        let m = syncMenuModel(snapshot: snapshot, removal: removal, host: hostWindowsAll())
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
        if let add = m.add {
            let sep = NSBox(frame: NSRect(x: 14, y: 0, width: width - 28, height: 11))
            sep.boxType = .separator
            let sepWrap = NSView(frame: NSRect(x: 0, y: 0, width: width, height: 11))
            sep.frame = NSRect(x: 14, y: 5, width: width - 28, height: 1)
            sepWrap.addSubview(sep)
            views.append(sepWrap)
            views.append(MenuCheckRow(content: .init(title: add.title), width: width) {})
        }
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
