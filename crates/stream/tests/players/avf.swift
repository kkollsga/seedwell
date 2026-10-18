// AVFoundation probe (QuickTime Player's engine): loads the duration, plays, seeks to an
// unfetched point and reports wall-clock milliseconds (unix epoch) for each milestone as JSON lines.
//
//   avf <url> <seek-seconds> <timeout-seconds>
import AVFoundation
import Foundation

func now() -> Double { Date().timeIntervalSince1970 * 1000.0 }
func emit(_ k: String, _ v: Double) { print("{\"\(k)\": \(Int64(v))}"); fflush(stdout) }
func fail(_ m: String) -> Never { print("{\"error\": \"\(m)\"}"); fflush(stdout); exit(2) }

let args = CommandLine.arguments
guard args.count >= 4, let url = URL(string: args[1]), let seekTo = Double(args[2]), let timeout = Double(args[3]) else {
    fail("usage: avf <url> <seek-seconds> <timeout-seconds>")
}

Task {
    let deadline = now() + timeout * 1000
    emit("start_ms", now())
    let asset = AVURLAsset(url: url)
    do {
        let d = try await asset.load(.duration)
        emit("duration_loaded_ms", now())
        print("{\"duration_s\": \(d.seconds)}"); fflush(stdout)
        let playable = try await asset.load(.isPlayable)
        if !playable { fail("not playable") }
    } catch { fail("load: \(error)") }

    let item = AVPlayerItem(asset: asset)
    let player = AVPlayer(playerItem: item)
    player.isMuted = true
    player.play()

    func waitPlaying(after t: Double) async -> Bool {
        // "Playing" = rate > 0, status ready, and the clock moved at least 0.3 s past `t`.
        while now() < deadline {
            if item.status == .failed { fail("item failed: \(String(describing: item.error))") }
            if item.status == .readyToPlay, player.timeControlStatus == .playing, player.currentTime().seconds >= t + 0.3 {
                return true
            }
            try? await Task.sleep(nanoseconds: 20_000_000)
        }
        return false
    }
    if await waitPlaying(after: 0) { emit("first_play_ms", now()) } else { fail("never started playing") }

    emit("seek_issued_ms", now())
    let ok = await player.seek(to: CMTime(seconds: seekTo, preferredTimescale: 600), toleranceBefore: .zero, toleranceAfter: .zero)
    emit("seek_returned_ms", now())
    if !ok { fail("seek cancelled") }
    if await waitPlaying(after: seekTo) { emit("seek_playing_ms", now()) } else { fail("never resumed after seek") }
    player.pause()
    emit("done_ms", now())
    exit(0)
}
dispatchMain()
