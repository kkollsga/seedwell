import AppKit

/// A drag as AppKit hands it to a drop destination: a pasteboard at a point of a window (window coordinates, as
/// `NSDraggingInfo.draggingLocation` has it). QA's `chat drop` and the tests call a view's drag-destination methods with
/// it: no drag session, no mouse events (AGENTS.md).
final class QADrag: NSObject, NSDraggingInfo {
    let draggingPasteboard: NSPasteboard
    let draggingLocation: NSPoint
    let draggingDestinationWindow: NSWindow?

    @MainActor init(_ pb: NSPasteboard, at point: NSPoint, in window: NSWindow?) {
        draggingPasteboard = pb
        draggingLocation = point
        draggingDestinationWindow = window
    }

    var draggingSourceOperationMask: NSDragOperation { .copy }
    var draggedImageLocation: NSPoint { draggingLocation }
    var draggedImage: NSImage? { nil }
    var draggingSource: Any? { nil }
    var draggingSequenceNumber: Int { 1 }
    func slideDraggedImage(to screenPoint: NSPoint) {}
    override func namesOfPromisedFilesDropped(atDestination dropDestination: URL) -> [String]? { nil }
    var draggingFormation: NSDraggingFormation = .default
    var animatesToDestination = false
    var numberOfValidItemsForDrop = 1
    func enumerateDraggingItems(options enumOpts: NSDraggingItemEnumerationOptions = [], for view: NSView?, classes classArray: [AnyClass],
                                searchOptions: [NSPasteboard.ReadingOptionKey: Any] = [:],
                                using block: (NSDraggingItem, Int, UnsafeMutablePointer<ObjCBool>) -> Void) {}
    var springLoadingHighlight: NSSpringLoadingHighlight { .none }
    func resetSpringLoading() {}

    /// A pasteboard of its own (never the system's drag or general pasteboard): release it when done.
    static func pasteboard() -> NSPasteboard {
        let pb = NSPasteboard(name: NSPasteboard.Name("seedwell-qa-\(UUID().uuidString)"))
        pb.clearContents()
        return pb
    }
}

extension ChatComposerTextView {
    /// The window point over the left part of the character at `index` (UTF-16): a drop there lands just before it; at
    /// the end of the text, just after its last character. From the layout, converted to the window as AppKit gives a
    /// drag's location (the scroll view's offset included). `nil`: no such character.
    func dropPoint(before index: Int) -> NSPoint? {
        guard let layout = layoutManager, let container = textContainer else { return nil }
        let length = (string as NSString).length
        guard index >= 0, index <= length else { return nil }
        layout.ensureLayout(for: container)
        let o = textContainerOrigin
        guard length > 0 else { return convert(NSPoint(x: o.x + 4, y: o.y + 8), to: nil) }
        let last = index == length
        let glyph = layout.glyphIndexForCharacter(at: last ? length - 1 : index)
        let r = layout.boundingRect(forGlyphRange: NSRange(location: glyph, length: 1), in: container)
        return convert(NSPoint(x: (last ? r.maxX + 2 : r.minX + 1) + o.x, y: r.midY + o.y), to: nil)
    }

    /// Drops what `pb` holds at the window point `point` as AppKit does (entered, updated, then prepare, perform and
    /// conclude, or exited when no operation was offered): the operation offered and whether the drop was taken.
    func drop(_ pb: NSPasteboard, at point: NSPoint) -> (NSDragOperation, Bool) {
        let drag = QADrag(pb, at: point, in: window)
        _ = draggingEntered(drag)
        let op = draggingUpdated(drag)
        guard !op.isEmpty else {
            draggingExited(drag)
            return (op, false)
        }
        let ok = prepareForDragOperation(drag) && performDragOperation(drag)
        if ok { concludeDragOperation(drag) }
        return (op, ok)
    }
}
