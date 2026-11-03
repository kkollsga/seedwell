import AppKit
import SwiftUI

/// The composer's text: an AppKit text view, because a drop onto it must know the character it lands on (SwiftUI's
/// `TextField` and its drop give no such place). Images and files pasted or dropped onto the text are the composer's
/// (`ChatComposerImages`), with a reference where they went; text is the field's. Return sends, Shift- or
/// Option-Return starts a line (`chatComposerKey`); Escape cancels a reply. Drawing and OS services only.
struct ChatComposerField: NSViewRepresentable {
    @Binding var text: String
    /// The selection, in UTF-16 (as `NSRange`): a paste replaces it.
    @Binding var selection: NSRange
    /// The field has the keyboard (the composer's outline); set it to put the cursor there.
    @Binding var focused: Bool
    let placeholder: String
    /// Images and files can be added: a paste or a drop of them goes to `images`.
    let attach: Bool
    let images: ChatComposerImages
    let core: Core
    let send: () -> Void
    /// Escape: true when it was used (a reply cancelled).
    let escape: () -> Bool

    /// One line high at least, eight at most; then it scrolls.
    static let lines: ClosedRange<CGFloat> = 1...8
    /// Space above and below the text, inside the view: a drop there still lands in the text.
    static let inset = NSSize(width: 0, height: 7)

    func makeCoordinator() -> Coordinator { Coordinator(self) }

    func makeNSView(context: Context) -> NSScrollView {
        let view = ChatComposerTextView.make()
        view.delegate = context.coordinator
        view.focusChanged = { [weak coordinator = context.coordinator] on in coordinator?.focusChanged(on) }
        let scroll = NSScrollView()
        scroll.drawsBackground = false
        scroll.borderType = .noBorder
        scroll.hasVerticalScroller = true
        scroll.hasHorizontalScroller = false
        scroll.autohidesScrollers = true
        scroll.documentView = view
        view.frame = NSRect(origin: .zero, size: scroll.contentSize)
        update(view, context: context)
        return scroll
    }

    func updateNSView(_ scroll: NSScrollView, context: Context) {
        guard let view = scroll.documentView as? ChatComposerTextView else { return }
        context.coordinator.parent = self
        let changed = view.string != text
        update(view, context: context)
        if changed { scroll.invalidateIntrinsicContentSize() }
    }

    private func update(_ view: ChatComposerTextView, context: Context) {
        let (images, core, attach) = (images, core, attach)
        view.addImages = attach ? { pb, at in images.paste(from: pb, core: core, at: at) } : nil
        view.images = images
        view.dropTargeted = attach ? { images.dropping = $0 } : nil
        if view.placeholder != placeholder {
            view.placeholder = placeholder
            view.setAccessibilityPlaceholderValue(placeholder)
            view.needsDisplay = true
        }
        if view.string != text {
            // The core changed the text (a reference put in or taken out, the field emptied on Send): the caret goes
            // where it says.
            context.coordinator.updating = true
            view.replace(with: text)
            let length = (text as NSString).length
            view.setSelectedRange(NSRange(location: min(selection.location, length), length: 0))
            context.coordinator.updating = false
            view.needsDisplay = true
        }
        // The keyboard is taken when `focused` turns true (a reply opened), never again on every update: a click
        // elsewhere gives it away before the binding hears of it.
        view.wantsFocus = focused
        if focused != context.coordinator.focused {
            context.coordinator.focused = focused
            if focused, let window = view.window, window.firstResponder !== view { window.makeFirstResponder(view) }
        }
    }

    func sizeThatFits(_ proposal: ProposedViewSize, nsView scroll: NSScrollView, context: Context) -> CGSize? {
        guard let view = scroll.documentView as? ChatComposerTextView, let width = proposal.width, width.isFinite, width > 0 else { return nil }
        return CGSize(width: width, height: view.height(forWidth: width, lines: Self.lines))
    }

    @MainActor final class Coordinator: NSObject, NSTextViewDelegate {
        var parent: ChatComposerField
        /// The text is being set from the binding: the view's changes are not the user's.
        var updating = false
        /// Whether the field has the keyboard, as last known here (the binding follows a moment later).
        var focused = false

        init(_ parent: ChatComposerField) { self.parent = parent }

        func textDidChange(_ notification: Notification) {
            guard !updating, let view = notification.object as? NSTextView else { return }
            parent.text = view.string
            parent.selection = view.selectedRange()
            view.enclosingScrollView?.invalidateIntrinsicContentSize()
        }

        func textViewDidChangeSelection(_ notification: Notification) {
            guard !updating, let view = notification.object as? NSTextView, parent.selection != view.selectedRange() else { return }
            parent.selection = view.selectedRange()
        }

        func textView(_ textView: NSTextView, doCommandBy selector: Selector) -> Bool {
            let flags = (textView as? ChatComposerTextView)?.commandFlags ?? NSApp.currentEvent?.modifierFlags ?? []
            switch ChatComposerTextView.command(selector, flags: flags, composing: textView.hasMarkedText()) {
            case .send?:
                parent.send()
                return true
            case .commit?:
                // Return while an input method composes (Japanese, Chinese, a dead key): the composition becomes text;
                // nothing is sent, and no line starts in place of what was being written.
                textView.unmarkText()
                textView.inputContext?.discardMarkedText()
                return true
            case .escape?:
                return parent.escape()
            case .newLine?, nil:
                return false
            }
        }

        /// The keyboard came or went: told after the view update that may have caused it.
        func focusChanged(_ on: Bool) {
            focused = on
            Task { @MainActor in if parent.focused != on { parent.focused = on } }
        }
    }
}

/// The composer's text view: plain text in the theme's body font, a placeholder while empty, its height the text's
/// (`height(forWidth:lines:)`), and images or files pasted or dropped on it handed to the composer with where they went.
final class ChatComposerTextView: NSTextView {
    /// What a key command does in the composer.
    enum Command: Equatable {
        /// Return while an input method composes: the composition is committed (never sent, no new line).
        case send, newLine, escape, commit
    }

    /// Images or files on a pasteboard (a paste, a drop) and where they go (the selection a paste replaces, the point a
    /// drop lands on); true when the composer took them. `nil`: they cannot be added here (then they are refused, never
    /// pasted as their paths).
    var addImages: ((NSPasteboard, NSRange) -> Bool)?
    /// Images are being dragged over the text (the conversation's drop outline).
    var dropTargeted: ((Bool) -> Void)?
    var placeholder = ""
    /// The field should have the keyboard: taken as soon as it is in a window.
    var wantsFocus = false
    var focusChanged: ((Bool) -> Void)?
    /// The pasteboard Paste reads: the system's (a test's or QA's own).
    var pasteboard = NSPasteboard.general
    /// The images and files waiting in the composer (QA's `chat state` lists them, `chat drop … chips` drops there).
    weak var images: ChatComposerImages?
    /// The modifier keys of the key being handled (`keyDown`, or QA's `chat key`), which a Return's command reads.
    private var keyFlags: NSEvent.ModifierFlags?

    /// The text view on its own TextKit 1 stack (a drop's character comes from its layout).
    static func make() -> ChatComposerTextView {
        let storage = NSTextStorage()
        let layout = NSLayoutManager()
        storage.addLayoutManager(layout)
        let container = NSTextContainer(size: NSSize(width: 0, height: CGFloat.greatestFiniteMagnitude))
        container.widthTracksTextView = true
        container.lineFragmentPadding = 0
        layout.addTextContainer(container)
        let view = ChatComposerTextView(frame: .zero, textContainer: container)
        view.isRichText = false
        view.importsGraphics = false
        view.allowsUndo = true
        view.drawsBackground = false
        view.isVerticallyResizable = true
        view.isHorizontallyResizable = false
        view.autoresizingMask = .width
        view.minSize = .zero
        view.maxSize = NSSize(width: CGFloat.greatestFiniteMagnitude, height: CGFloat.greatestFiniteMagnitude)
        view.textContainerInset = ChatComposerField.inset
        view.isContinuousSpellCheckingEnabled = true
        let paragraph = NSMutableParagraphStyle()
        paragraph.lineSpacing = Theme.Line.secondary
        view.defaultParagraphStyle = paragraph
        view.font = Theme.NS.body()
        view.textColor = .labelColor
        view.typingAttributes = view.textAttributes
        view.setAccessibilityIdentifier("chat-field")
        return view
    }

    /// The text's own attributes (typing, the placeholder's base).
    var textAttributes: [NSAttributedString.Key: Any] {
        [.font: font ?? Theme.NS.body(), .foregroundColor: NSColor.labelColor, .paragraphStyle: defaultParagraphStyle ?? .default]
    }

    /// What a text command does: Return and its variants send or start a line as the core says (`chatComposerKey`),
    /// Escape cancels a reply; anything else is the text view's. While an input method composes (`composing`: marked
    /// text), Return commits the composition and Escape is the input method's.
    static func command(_ selector: Selector, flags: NSEvent.ModifierFlags, composing: Bool = false) -> Command? {
        switch selector {
        case #selector(NSResponder.insertNewline(_:)), #selector(NSResponder.insertNewlineIgnoringFieldEditor(_:)),
             #selector(NSResponder.insertLineBreak(_:)):
            composing ? .commit
                : chatComposerKey(shift: flags.contains(.shift), alt: flags.contains(.option)) == .send ? .send : .newLine
        case #selector(NSResponder.cancelOperation(_:)):
            composing ? nil : .escape
        default:
            nil
        }
    }

    /// The modifier keys a command was given with: the key event's being handled, else the app's current event's.
    var commandFlags: NSEvent.ModifierFlags { keyFlags ?? NSApp?.currentEvent?.modifierFlags ?? [] }

    override func keyDown(with event: NSEvent) {
        keyFlags = event.modifierFlags
        defer { keyFlags = nil }
        super.keyDown(with: event)
    }

    /// A key's command as the key makes it, with `flags` held (QA's `chat key`; no key event is posted): the text view
    /// and its delegate do with it what they do with the key's.
    func perform(_ selector: Selector, flags: NSEvent.ModifierFlags) {
        keyFlags = flags
        defer { keyFlags = nil }
        doCommand(by: selector)
    }

    /// The placeholder is drawn: the field is empty (a composition in progress is text).
    var showsPlaceholder: Bool { string.isEmpty && !placeholder.isEmpty }

    /// The field's height at `width`: its text's, from one line to `lines.upperBound` (then it scrolls), and the insets.
    /// Measured on a copy of the text, so the view's own layout keeps its width.
    func height(forWidth width: CGFloat, lines: ClosedRange<CGFloat>) -> CGFloat {
        let storage = NSTextStorage(attributedString: textStorage ?? NSAttributedString())
        let layout = NSLayoutManager()
        storage.addLayoutManager(layout)
        let container = NSTextContainer(size: NSSize(width: max(width - textContainerInset.width * 2, 1), height: CGFloat.greatestFiniteMagnitude))
        container.lineFragmentPadding = textContainer?.lineFragmentPadding ?? 0
        layout.addTextContainer(container)
        layout.ensureLayout(for: container)
        let (line, gap) = (layout.defaultLineHeight(for: font ?? Theme.NS.body()), Theme.Line.secondary)
        let used = layout.usedRect(for: container).height
        // The fewest lines with the room of their spacing (the one-line field's height); the most as they are laid out,
        // with the spacing between them only, so the line after the last one scrolls instead of adding the spacing.
        let least = (line + gap) * lines.lowerBound
        let most = line * lines.upperBound + gap * (lines.upperBound - 1)
        return ceil(min(max(used, least), most) + textContainerInset.height * 2)
    }

    /// The whole text replaced by `text` (the core changed it) as an edit, so Undo takes it back.
    func replace(with text: String) {
        let all = NSRange(location: 0, length: (string as NSString).length)
        guard shouldChangeText(in: all, replacementString: text) else { return }
        textStorage?.replaceCharacters(in: all, with: NSAttributedString(string: text, attributes: textAttributes))
        didChangeText()
    }

    override func draw(_ dirtyRect: NSRect) {
        super.draw(dirtyRect)
        guard showsPlaceholder else { return }
        var attributes = textAttributes
        attributes[.foregroundColor] = NSColor.placeholderTextColor
        let origin = textContainerOrigin
        let width = bounds.width - origin.x * 2
        NSAttributedString(string: placeholder, attributes: attributes)
            .draw(with: NSRect(x: origin.x, y: origin.y, width: width, height: bounds.height - origin.y * 2),
                  options: [.usesLineFragmentOrigin, .truncatesLastVisibleLine])
    }

    override func didChangeText() {
        super.didChangeText()
        markReferences()
        needsDisplay = true
    }

    /// The image references in the text (`chatComposerImageRefs`) in the accent colour on a faint tint of it, as the
    /// chips they become in the message; the text itself stays as typed.
    func markReferences() {
        guard let layout = layoutManager else { return }
        let all = NSRange(location: 0, length: (string as NSString).length)
        layout.removeTemporaryAttribute(.foregroundColor, forCharacterRange: all)
        layout.removeTemporaryAttribute(.backgroundColor, forCharacterRange: all)
        for r in chatComposerImageRefs(draft: string) {
            layout.addTemporaryAttributes([.foregroundColor: NSColor.controlAccentColor,
                                           .backgroundColor: NSColor.controlAccentColor.withAlphaComponent(0.14)],
                                          forCharacterRange: NSRange(location: Int(r.start), length: Int(r.length)))
        }
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        if wantsFocus, let window, window.firstResponder !== self { window.makeFirstResponder(self) }
    }

    override func becomeFirstResponder() -> Bool {
        let took = super.becomeFirstResponder()
        if took { focusChanged?(true) }
        return took
    }

    override func resignFirstResponder() -> Bool {
        let gave = super.resignFirstResponder()
        if gave { focusChanged?(false) }
        return gave
    }

    // MARK: Paste

    /// Cmd-V (and Edit > Paste): images or files are the composer's, a reference each at the selection; text is pasted
    /// from `pasteboard` (what `super.paste` does with the system's). Paste and Match Style stays the text view's.
    override func paste(_ sender: Any?) {
        if let addImages, addImages(pasteboard, selectedRange()) { return }
        if !readSelection(from: pasteboard) { NSSound.beep() }
    }

    // MARK: Drop

    /// Files and pictures may be dragged onto the text, besides text.
    override var acceptableDragTypes: [NSPasteboard.PasteboardType] {
        super.acceptableDragTypes + [.fileURL, .png, .tiff]
    }

    /// The drag carries images or files (else it is text, the text view's).
    private func carriesImages(_ info: NSDraggingInfo) -> Bool { ChatImagePasteboard.hasImage(info.draggingPasteboard) }

    /// Where a drag at `info`'s location would go: the character it is over.
    func dropIndex(_ info: NSDraggingInfo) -> Int {
        characterIndexForInsertion(at: convert(info.draggingLocation, from: nil))
    }

    override func draggingEntered(_ sender: NSDraggingInfo) -> NSDragOperation {
        guard carriesImages(sender) else { return super.draggingEntered(sender) }
        guard addImages != nil else { return [] }
        dropTargeted?(true)
        _ = super.draggingEntered(sender)   // the text view's drop caret
        return .copy
    }

    override func draggingUpdated(_ sender: NSDraggingInfo) -> NSDragOperation {
        guard carriesImages(sender) else { return super.draggingUpdated(sender) }
        guard addImages != nil else { return [] }
        _ = super.draggingUpdated(sender)
        return .copy
    }

    override func draggingExited(_ sender: NSDraggingInfo?) {
        if let sender, carriesImages(sender) { dropTargeted?(false) }
        super.draggingExited(sender)
    }

    override func prepareForDragOperation(_ sender: NSDraggingInfo) -> Bool {
        guard carriesImages(sender) else { return super.prepareForDragOperation(sender) }
        return addImages != nil
    }

    /// Images or files dropped onto the text: attached, with a reference each at the character the drop is over (none
    /// in an empty field). Never their paths as text.
    override func performDragOperation(_ sender: NSDraggingInfo) -> Bool {
        guard carriesImages(sender) else { return super.performDragOperation(sender) }
        let at = NSRange(location: dropIndex(sender), length: 0)
        cleanUpAfterDragOperation()
        dropTargeted?(false)
        return addImages?(sender.draggingPasteboard, at) ?? false
    }

    override func concludeDragOperation(_ sender: NSDraggingInfo?) {
        if let sender, carriesImages(sender) { return }
        super.concludeDragOperation(sender)
    }
}
