import SwiftUI

/// Selection mode in the main window (SPEC 7A.6, 8.3): a torrent that waits for a choice gets its file list under
/// its row, with checkboxes, sizes and the smart default ticks. It is the popover's own chooser (`ChoiceStore`): a
/// tick here shows there and in the chooser window, and nothing downloads until Start.
struct HeldFilesBlock: View {
    let id: String
    let store: ChoiceStore

    var body: some View {
        if let page = store.page(id) {
            ChoiceChecklist(id: id, page: page, store: store, maxListHeight: FileLayout.maxListHeight, offersShowAll: true)
                .padding(.leading, FileLayout.iconX).padding(.trailing, 10).padding(.top, 2).padding(.bottom, 10)
        }
    }
}

/// The row's actions while its torrent is held (SPEC 8.3): Cancel and Start, the same two as everywhere (the core's
/// `heldActions`, without keys: the row does not own Return and Escape). Start on a torrent that waits for a choice
/// takes the ticks of the list under it; on a countdown it starts it now. Cancel removes the torrent and moves its
/// `.torrent` file to the Trash; ⇧ keeps the file, with the same caption.
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
        let canStart = awaiting ? (store.page(id)?.canConfirm ?? false) : true
        HeldButtons(actions: heldActions(hasSourceFile: hasSource, active: false, canStart: canStart, startHelp: startHelp),
                    cancel: { commands.cancelHeld(id, keepFile: $0) },
                    start: { commands.startHeld(id, awaiting: awaiting) })
            .controlSize(.small)
            .fixedSize()
            .task(id: id) {
                // The shared chooser is loaded here when no popover has done it yet (the engine is waited for off the
                // main thread).
                if awaiting { _ = await store.load(id) }
                hasSource = await store.hasSourceFile(id)
            }
    }
}
