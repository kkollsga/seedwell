import Foundation
import Observation

// Favorite trackers (SPEC 7E.1, 8.3): the core's `FavoriteEditor` decides (the edit in progress, validation, the
// texts, the removal question); this mirrors its view for SwiftUI and does what each change asks of the window.

/// The one inline edit in progress (a name or an address) and the question of removing a favorite.
@MainActor @Observable final class FavoriteEditing {
    let editor: FavoriteEditor
    private(set) var view: FavoriteEditorView
    /// Opens the favorite's tab (wired by Commands).
    @ObservationIgnored var openTab: (String) -> Void = { _ in }
    /// Opens the tracker's Info page (wired by Commands).
    @ObservationIgnored var openInfo: (String) -> Void = { _ in }
    @ObservationIgnored private var refreshTask: Task<Void, Never>?

    init(core: Core) {
        editor = FavoriteEditor(core: core)
        view = editor.view()
    }

    var target: FavoriteTarget? { view.target }
    var hint: String? { view.hint }
    var note: String? { view.note }
    var removal: FavoriteRemoval? { view.removal }

    /// The field's text; each change goes to the core.
    var text: String {
        get { view.text }
        set { editor.setText(text: newValue); refresh() }
    }

    func reload() { editor.reload() }
    func record(_ id: String) -> ProfileRecord? { editor.record(id: id) }

    func beginRename(_ id: String) { editor.beginRename(id: id); refresh() }
    func cancel() { editor.cancel(); refresh() }
    func commit() { apply(editor.commit()) }
    func askRemove(_ id: String) { apply(editor.askRemove(id: id)) }
    func cancelRemoval() { editor.cancelRemoval(); refresh() }
    func confirmRemoval() { apply(editor.confirmRemoval()) }
    /// "Save as Favorite" on an automatic tracker.
    func saveAsFavorite(_ groupID: String) { apply(editor.saveAsFavorite(groupId: groupID)) }
    func setLimits(_ id: String, down: UInt64, up: UInt64) { apply(editor.setLimits(id: id, down: down, up: up)) }
    func setFolderDraft(_ id: String, _ record: ProfileRecord) { editor.setFolderDraft(id: id, record: record); refresh() }
    func run(_ id: String, _ command: TrackerInfoCommand) { apply(editor.trackerInfoRun(trackerId: id, command: command)) }

    /// Does what a change asks, then reads the view again.
    func apply(_ effect: FavoriteEffect) {
        refresh()
        if let id = effect.openTab { openTab(id) }
    }

    /// Reads the view again, and again when its note has gone.
    func refresh() {
        view = editor.view()
        refreshTask?.cancel()
        guard let ms = view.refreshInMs else { return }
        refreshTask = Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(Int(ms)))
            if !Task.isCancelled { self?.refresh() }
        }
    }
}
