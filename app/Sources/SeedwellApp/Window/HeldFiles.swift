import SwiftUI

/// Selection mode in the main window (SPEC 7A.6, 8.3): a torrent that waits for a choice gets its file list under
/// its row, with checkboxes, sizes and the smart default ticks. It is the popover's own model (`ChoiceStore`): a
/// tick here shows there and in the chooser window, and nothing downloads until Download.
struct HeldFilesBlock: View {
    let model: ChooserModel
    let id: String

    var body: some View {
        ChoiceChecklist(model: model, active: false, id: id, actions: StartingActions(
            chooseFiles: { _ in }, togglePause: { _, _ in }, startNow: { _ in }),
            showButtons: false, maxListHeight: FileLayout.maxListHeight, offersShowAll: true)
            .padding(.leading, FileLayout.iconX).padding(.trailing, 10).padding(.top, 2).padding(.bottom, 10)
    }
}

/// The row's actions while its torrent is held (SPEC 8.3): Cancel and Start, the same two as everywhere. Start on a
/// torrent that waits for a choice takes the ticks of the list under it; on a countdown it starts it now. Cancel
/// removes the torrent and moves its `.torrent` file to the Trash; ⇧ keeps the file, with the same caption.
struct HeldRowActions: View {
    let id: String
    let commands: Commands
    /// Waiting for a choice (else counting down).
    var awaiting = true
    /// The Start button's tooltip (the core's words).
    let startHelp: String
    @State private var hasSource = false
    private let store = ChoiceStore.shared

    var body: some View {
        HeldButtons(hasSourceFile: hasSource, active: false,
                    canStart: awaiting ? (store.models[id]?.canConfirm ?? false) : true,
                    startHelp: startHelp,
                    cancel: { commands.cancelHeld(id, keepFile: $0) },
                    start: { commands.startHeld(id, awaiting: awaiting) })
            .controlSize(.small)
            .fixedSize()
            .task(id: id) {
                // The shared model is loaded here when no popover has done it yet.
                if awaiting { _ = store.model(for: id) }
                hasSource = store.hasSource(id)
            }
    }
}
