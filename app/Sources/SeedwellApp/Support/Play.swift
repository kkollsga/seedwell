import AppKit
import Observation
import SwiftUI

// Play while downloading (SPEC §7F.3), the app side: the Play commands the rows and menus share (the core decides what
// each comes to: `Core.play`), and the small note that explains why nothing started.

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

extension TransferRow {
    /// Worth asking the engine for the file list (a single video, or a folder that may hold episodes).
    var mayHoldVideo: Bool { rowMayHoldVideo(row: self) }
    /// A finished direct download of a video: opens its file.
    var isFinishedDirectVideo: Bool { rowIsFinishedDirectVideo(row: self) }
    /// The Play button of a row: any torrent that holds a video (a single file or a folder; the engine
    /// worked that out once, `hasVideo`), playing one fetches its start first; or a finished direct video.
    var showsPlayButton: Bool { rowShowsPlay(row: self) }
}

extension Commands {
    /// Shows a note in the main window (opened if need be: the menu bar has nowhere to put it).
    func playNote(_ text: String, symbol: String = "play.fill") {
        showMain(nil, nil)
        playNotice.show(text, symbol: symbol)
    }

    /// A press on a row's Play icon: the torrent's largest video plays at once (a finished file opens,
    /// otherwise it streams). It never opens a menu.
    func playButtonPressed(_ row: TransferRow) { play(row) }

    /// The Play button of a row, and the default entry of the menus: a finished direct video's file, else the largest
    /// selected video.
    func play(_ row: TransferRow) { play(row, fileIndex: nil, playerId: nil) }

    /// Plays one file of a torrent (a file list's Play). `with`: a chosen player (Play With), else the setting decides.
    func play(_ row: TransferRow, file: TorrentFile, with playerId: String? = nil) {
        play(row, fileIndex: file.index, playerId: playerId)
    }

    /// What the core makes of a Play command: a player to start, or the note why none. The engine answers in
    /// milliseconds, but a torrent that is still starting makes the stream wait a moment: keep the main thread free.
    func play(_ row: TransferRow, fileIndex: UInt32?, playerId: String?) {
        let core = self.core
        Task { [weak self] in
            let start = await core.play(row: row, fileIndex: fileIndex, playerId: playerId)
            guard let self else { return }
            switch start {
            case let .note(text):
                self.playNote(text)
            case let .launch(launch, player):
                PlayerLauncher.start(launch, player: player) { [weak self] error in
                    if let error { self?.playNote(error) }
                }
            }
        }
    }

    // MARK: Menu entries

    /// What the Play entries of a row are made of (`rowCommands`, `playCommands`): its playable videos and the players
    /// on this Mac.
    func playOffer(_ row: TransferRow) -> PlayOffer { core.playOffer(row: row) }

    /// "Play" and "Play With ▸" for Get Info; empty when the torrent has no selected video. A pack lists its episodes.
    func playEntries(for row: TransferRow) -> [MenuEntry] {
        let offer = playOffer(row)
        return draw(playCommands(row: row, offer: offer), rows: [row], videos: offer.videos, inMenuBar: false)
    }

    /// The Play entries of these videos of a torrent (a video's menu in the file list).
    func playEntries(row: TransferRow, videos: [TorrentFile]) -> [MenuEntry] {
        let offer = PlayOffer(videos: videos, players: core.playPlayers())
        return draw(playCommands(row: row, offer: offer), rows: [row], videos: videos, inMenuBar: false)
    }
}
