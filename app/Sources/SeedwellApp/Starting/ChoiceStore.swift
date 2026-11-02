import Foundation
import Observation

/// The one chooser per torrent that the notifier, the chooser window and the list under a held row share (SPEC
/// 7A.6), as the core keeps them (`Choices`): its ticks, Start, Cancel and a category change are the core's. This
/// class only tells SwiftUI that a chooser changed (`revision`), shows Cancel's quiet note and tells the app that a
/// torrent was resolved.
@MainActor @Observable final class ChoiceStore {
    static let shared = ChoiceStore()

    @ObservationIgnored var choices: Choices?
    /// Told when Start or Cancel resolved a torrent here, so a chooser window open for it can close.
    @ObservationIgnored var didResolve: (String) -> Void = { _ in }
    /// A quiet note (a notification, never an alert).
    @ObservationIgnored var note: (CancelNote) -> Void = { Notifier.post(title: $0.title, body: $0.body) }
    /// Bumped after every change, so whatever draws a chooser reads it again.
    private(set) var revision = 0

    /// The shared chooser of `id`, loaded from the core the first time (`reopen`: a running torrent's Files…). Nil
    /// while the core cannot say (yet).
    func chooser(for id: String, reopen: Bool = false) -> Chooser? {
        let had = choices?.loaded(id: id) != nil
        let c = choices?.chooser(id: id, reopen: reopen)
        if !had, c != nil { changed() }
        return c
    }

    /// The shared chooser of `id`, loaded from the core when there is none yet: the engine is waited for off the main
    /// thread (a chooser window about to open, the list under a held row).
    func load(_ id: String, reopen: Bool = false) async -> Chooser? {
        guard let choices else { return nil }
        let had = choices.loaded(id: id) != nil
        let c = await choices.load(id: id, reopen: reopen)
        if !had, c != nil { changed() }
        return c
    }

    /// Whether Cancel would move a `.torrent` file to the Trash, looked at now off the main thread (`hasSource` answers
    /// at once with what was last seen).
    func hasSourceFile(_ id: String) async -> Bool {
        guard let choices else { return false }
        return await choices.hasSourceFile(id: id)
    }

    /// The chooser of `id` when one is loaded (never loads).
    func loaded(_ id: String) -> Chooser? {
        _ = revision
        return choices?.loaded(id: id)
    }

    /// The chooser of `id` as drawn now (read again after every change).
    func page(_ id: String) -> ChooserPage? {
        guard let c = loaded(id) else { return nil }
        return c.view(hasSourceFile: hasSource(id))
    }

    /// Cancel would move a `.torrent` file to the Trash (so ⇧ has something to keep).
    func hasSource(_ id: String) -> Bool { choices?.hasSource(id: id) ?? false }

    /// A click in a chooser: `body` changes it in the core; everything that shows it draws again.
    func edit(_ body: () -> Void) {
        body()
        changed()
    }

    /// Start: the ticks go to the engine and the torrent starts (no countdown). Returns at once; what shows the chooser
    /// closes once the engine took it, or `failed` says why it could not start.
    func confirm(_ id: String, failed: @escaping @MainActor (Error) -> Void = { _ in }) {
        guard let choices else { return }
        CoreCalls.inOrder({ await catching { try await choices.confirm(id: id) } }) { [weak self] r in
            guard let self else { return }
            changed()
            if case .failure(let error) = r { return failed(error) }
            didResolve(id)
        }
    }

    /// Cancel: the held torrent is removed and its `.torrent` file goes to the Trash unless `keepFile` (⇧-Cancel). A
    /// file that could not be moved never stops the removal; its note is shown quietly.
    func cancel(_ id: String, keepFile: Bool = false) {
        guard let choices else { return }
        CoreCalls.inOrder({ await choices.cancel(id: id, keepFile: keepFile) }) { [weak self] failed in
            guard let self else { return }
            changed()
            didResolve(id)
            if let failed { note(failed) }
        }
    }

    /// The pill's menu and the chooser's Change: the core plans the torrent again and rebuilds the shared chooser.
    func changeCategory(_ id: String, to c: Category) {
        guard let choices else { return }
        CoreCalls.inOrder({ await choices.changeCategory(id: id, category: c) }) { [weak self] in self?.changed() }
    }

    /// Start or Cancel resolved `id` elsewhere (the notifier, through the core's `Starting`): what showed it closes,
    /// and a Cancel's note is shown quietly.
    func resolved(_ id: String, note failed: CancelNote? = nil) {
        changed()
        didResolve(id)
        if let failed { note(failed) }
    }

    func drop(_ id: String) {
        choices?.forget(id: id)
        changed()
    }

    func changed() { revision &+= 1 }
}
