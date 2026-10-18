import AppKit
import Observation
import SwiftUI

// Play while downloading (SPEC §7F.3), the app side: which players exist, the Play commands the rows
// and menus share, and the small note that explains why nothing started.

/// The installed players, looked up when needed and then remembered for a few seconds (apps get
/// installed and updated while Seedwell runs; reading seven file headers is cheap but not free).
@MainActor final class PlayerDirectory {
    static let shared = PlayerDirectory()
    private var cache: [InstalledPlayer] = []
    private var at: Date = .distantPast
    /// QA: a fixed list instead of the machine's.
    var override: [InstalledPlayer]?

    func installed(maxAge: TimeInterval = 20) -> [InstalledPlayer] {
        if let override { return override }
        if Date().timeIntervalSince(at) > maxAge {
            cache = PlayerCatalog.detectOnThisMac()
            at = Date()
        }
        return cache
    }
}

/// A calm one-line message at the bottom of the main window; it goes away by itself.
@MainActor @Observable final class PlayNotice {
    private(set) var text: String?
    private(set) var symbol = "play.fill"
    @ObservationIgnored private var task: Task<Void, Never>?

    func show(_ message: String, symbol: String = "play.fill", for seconds: Double = 6) {
        text = message
        self.symbol = symbol
        task?.cancel()
        task = Task { [weak self] in
            try? await Task.sleep(for: .seconds(seconds))
            guard !Task.isCancelled else { return }
            self?.text = nil
        }
    }

    func clear() { task?.cancel(); text = nil }
}

struct PlayNoticeView: View {
    let notice: PlayNotice

    var body: some View {
        if let text = notice.text {
            HStack(spacing: 8) {
                Image(systemName: notice.symbol).font(.system(size: 11)).foregroundStyle(.secondary)
                Text(text).font(.system(size: 12.5))
            }
            .padding(.horizontal, 14).padding(.vertical, 8)
            .background(.regularMaterial, in: Capsule())
            .overlay(Capsule().stroke(.primary.opacity(0.08), lineWidth: 0.5))
            .padding(.bottom, 16)
            .transition(.opacity)
            .accessibilityElement(children: .combine)
            .help("Click to dismiss")
            .onTapGesture { notice.clear() }
        }
    }
}

extension TorrentFile {
    var isFinished: Bool { length > 0 && done >= length }

    /// "S01E02" when the name has it, else the name without its extension.
    var episodeLabel: String {
        let base = (name as NSString).deletingPathExtension
        if let r = base.range(of: #"[Ss]\d{1,2}[Ee]\d{1,3}(-?[Ee]\d{1,3})?"#, options: .regularExpression) {
            return base[r].uppercased()
        }
        return base
    }
}

extension TransferRow {
    /// A torrent or download whose name says it is one video file (cheap: no engine call).
    var isSingleVideo: Bool { VideoFiles.isVideo(ext: fileExt) }
    /// Worth asking the engine for the file list (a single video, or a folder that may hold episodes).
    var mayHoldVideo: Bool {
        guard kind == .torrent, !isHeld else { return false }
        return isSingleVideo || hasVideo || (fileExt.isEmpty && (category == .movie || category == .tv || category == .other))
    }
    /// A finished direct download of a video: opens its file.
    var isFinishedDirectVideo: Bool { kind == .direct && state == .completed && isSingleVideo }
    /// The Play button of a row: any torrent that holds a video (a single file or a folder; the engine
    /// worked that out once, `hasVideo`), playing one fetches its start first; or a finished direct video.
    var showsPlayButton: Bool {
        if isFinishedDirectVideo { return true }
        return (isSingleVideo || hasVideo) && !isHeld && kind == .torrent
    }
}

extension Commands {
    /// Shows a note in the main window (opened if need be: the menu bar has nowhere to put it).
    func playNote(_ text: String, symbol: String = "play.fill") {
        showMain(nil, nil)
        playNotice.show(text, symbol: symbol)
    }

    /// The video files of a torrent that can be played: selected for download, not padding.
    func playableFiles(_ row: TransferRow) -> [TorrentFile] {
        guard row.mayHoldVideo, let files = try? core.files(id: row.id) else { return [] }
        return files.filter { $0.isVideo && $0.wanted }
    }

    /// The largest playable video: what the row's Play button and the default entry play (episodes are
    /// played from the file list, where every video file has its own Play).
    func largestVideo(_ row: TransferRow) -> TorrentFile? {
        playableFiles(row).max { $0.length < $1.length }
    }

    /// A press on a row's Play icon: the torrent's largest video plays at once (a finished file opens,
    /// otherwise it streams). It never opens a menu.
    func playButtonPressed(_ row: TransferRow) { play(row) }

    /// The Play button of a row, and the default entry of the menus: the largest selected video.
    func play(_ row: TransferRow) {
        if row.isFinishedDirectVideo { return playLocal(row, with: nil) }
        guard let file = largestVideo(row) else { return playNote("There is no video to play in this download.") }
        play(row, file: file)
    }

    /// Plays one file: a finished file is opened directly, an unfinished one through the engine's
    /// loopback stream. `with` is a chosen player (Play With), else the setting decides.
    func play(_ row: TransferRow, file: TorrentFile, with chosen: InstalledPlayer? = nil) {
        let container = VideoContainer(isMp4Family: file.isMp4Family)
        let choice = chosen.map(PlayerChoice.play)
            ?? PlayerCatalog.resolve(preference: core.settingsLoad().general.playWith,
                                     installed: PlayerDirectory.shared.installed(), container: container)
        guard case let .play(player) = choice else { return playNote(choice.message ?? "No video player found.") }
        if file.isFinished {
            guard let url = localURL(row, file) else {
                return playNote("“\(file.name)” isn't at its saved location.")
            }
            launch(url, with: player)
            return
        }
        // The engine answers in milliseconds, but a torrent that is still starting makes it wait a moment:
        // keep the main thread free meanwhile.
        let core = self.core
        let (id, index) = (row.id, file.index)
        Task { [weak self] in
            let result = await Task.detached { Result { try core.streamUrl(torrentId: id, fileIndex: index) } }.value
            guard let self else { return }
            switch result {
            case let .success(info) where info.complete:
                // It finished while we were asking: open the file itself.
                if let url = self.localURL(row, file) { self.launch(url, with: player) }
                else { self.playNote("“\(file.name)” isn't at its saved location.") }
            case let .success(info):
                guard let url = URL(string: info.url) else { return self.playNote("Couldn't start playing.") }
                self.launch(url, with: player)
            case let .failure(error):
                self.playNote(Self.playMessage(error))
            }
        }
    }

    /// The sentence the engine gave, else a plain fallback.
    static func playMessage(_ error: Error) -> String {
        if case let CoreError.Failed(message) = error { return message }
        if case let CoreError.InvalidSource(message) = error { return message }
        return "Couldn't start playing."
    }

    /// A finished direct download: the file at the saved path.
    func playLocal(_ row: TransferRow, with chosen: InstalledPlayer?) {
        let container = VideoContainer(fileName: row.url.lastPathComponent)
        let choice = chosen.map(PlayerChoice.play)
            ?? PlayerCatalog.resolve(preference: core.settingsLoad().general.playWith,
                                     installed: PlayerDirectory.shared.installed(), container: container)
        guard case let .play(player) = choice else { return playNote(choice.message ?? "No video player found.") }
        guard FileManager.default.fileExists(atPath: row.savePath) else {
            return playNote("“\(row.url.lastPathComponent)” isn't at its saved location.")
        }
        launch(row.url, with: player)
    }

    private func launch(_ url: URL, with player: InstalledPlayer) {
        PlayerLauncher.play(url: url, with: player) { [weak self] error in
            if let error { self?.playNote(error) }
        }
    }

    /// Where a finished file is: the saved path is the file itself (single-file torrent) or the folder.
    func localURL(_ row: TransferRow, _ file: TorrentFile) -> URL? {
        if !file.diskPath.isEmpty {
            let url = URL(fileURLWithPath: file.diskPath)
            if FileManager.default.fileExists(atPath: url.path) { return url }
        }
        let base = URL(fileURLWithPath: row.savePath)
        var isDir: ObjCBool = false
        guard FileManager.default.fileExists(atPath: base.path, isDirectory: &isDir) else { return nil }
        if !isDir.boolValue { return base }
        let url = base.appendingPathComponent(file.path)
        return FileManager.default.fileExists(atPath: url.path) ? url : nil
    }

    // MARK: Menu entries

    /// "Play" and "Play With ▸" for the context menu and the menu bar submenu; empty when the torrent
    /// has no selected video. A pack lists its episodes.
    func playEntries(for row: TransferRow) -> [MenuEntry] {
        if row.isFinishedDirectVideo {
            let capable = PlayerCatalog.capable(PlayerDirectory.shared.installed(), for: VideoContainer(fileName: row.url.lastPathComponent))
            return [
                .item("Play") { self.playLocal(row, with: nil) },
                .submenu("Play With", capable.isEmpty
                    ? [.item("No Player Installed", enabled: false) {}]
                    : capable.map { p in .item(p.name) { self.playLocal(row, with: p) } }),
            ]
        }
        let videos = playableFiles(row)
        guard !videos.isEmpty else { return [] }
        return playEntries(row: row, videos: videos)
    }

    func playEntries(row: TransferRow, videos: [TorrentFile]) -> [MenuEntry] {
        let installed = PlayerDirectory.shared.installed()
        let shown = Array(videos.prefix(Self.episodesShown))
        let more: [MenuEntry] = videos.count > shown.count ? [.item("+ \(videos.count - shown.count) more in Get Info", enabled: false) {}] : []
        func episodes(_ with: InstalledPlayer?) -> [MenuEntry] {
            shown.map { f in .item(f.episodeLabel) { self.play(row, file: f, with: with) } } + more
        }
        // Play With lists the players that can run here and play the first video's container.
        let capable = PlayerCatalog.capable(installed, for: VideoContainer(isMp4Family: videos[0].isMp4Family))
        let playWith: MenuEntry
        if capable.isEmpty {
            playWith = .submenu("Play With", [.item("No Player Installed", enabled: false) {}])
        } else if videos.count == 1 {
            playWith = .submenu("Play With", capable.map { p in .item(p.name) { self.play(row, file: videos[0], with: p) } })
        } else {
            playWith = .submenu("Play With", capable.map { p in .submenu(p.name, episodes(p)) })
        }
        let play: MenuEntry = videos.count == 1
            ? .item("Play") { self.play(row, file: videos[0]) }
            : .submenu("Play", episodes(nil))
        return [play, playWith]
    }

    static let episodesShown = 40
}
