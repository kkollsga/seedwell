import AppKit
import SwiftUI

/// Whether Shift is down. QA can force it (`starting shift on|off`), since no key is held then.
enum ShiftKey {
    @MainActor static var qaOverride: Bool?
    @MainActor static var down: Bool { qaOverride ?? NSEvent.modifierFlags.contains(.shift) }
}

/// Cancel on a torrent that waits for a decision (SPEC 7A.6), as the core words it (`HeldActions`): it removes the
/// torrent and moves the `.torrent` file it came from to the Trash; where there is a file to keep (`keep`), Shift held
/// keeps it, and the caption slides out just below the button while Shift is down. It hangs in the popover's own
/// padding, so nothing moves when it appears. Gives its action whether to keep the `.torrent` file.
struct CancelControl: View {
    let cancel: HeldAction
    let keep: HeldKeepFile?
    /// Where the caption sits under the button.
    var captionAlignment: HorizontalAlignment = .trailing
    let action: (_ keepFile: Bool) -> Void
    @State private var shift = false

    var body: some View {
        // The state of Shift is read again at the click: a click that arrives before the next poll still counts.
        let go = { action(keep != nil && ShiftKey.down) }
        Group {
            if cancel.keys.contains(.escape) {
                Button(cancel.title, role: .cancel, action: go).keyboardShortcut(.cancelAction).help(cancel.help)
            } else {
                Button(cancel.title, role: .cancel, action: go).help(cancel.help)
            }
        }
        .overlay(alignment: captionAlignment == .trailing ? .bottomTrailing : .bottomLeading) {
            if shift, let caption = keep?.caption {
                Text(caption)
                    .font(.system(size: 11))
                    .foregroundStyle(.secondary)
                    .lineLimit(1).fixedSize()
                    .padding(.horizontal, 2)
                    .offset(y: 17)
                    .transition(.move(edge: .top).combined(with: .opacity))
                    .allowsHitTesting(false)
            }
        }
        .animation(.easeOut(duration: 0.18), value: shift)
        .background {
            if keep?.keys.contains(.shiftEscape) == true {
                Button("", action: { action(true) }).keyboardShortcut(.escape, modifiers: .shift)
                    .opacity(0).frame(width: 0, height: 0).accessibilityHidden(true)
            }
        }
        .task(id: keep != nil) {
            guard keep != nil else { shift = false; return }
            while !Task.isCancelled {
                shift = ShiftKey.down
                try? await Task.sleep(for: .milliseconds(100))
            }
        }
    }
}

/// The two actions of everything that is held or new (SPEC 7A.6), as the core gives them: **Cancel** and **Start**,
/// right-aligned on the bottom line, Cancel first and Start (the default) last on the Mac. The keys (Return: Start,
/// Escape: Cancel, ⇧Escape: Cancel keeping the `.torrent` file) are the core's, for the block that owns them. The ⇧
/// caption hangs under Cancel.
struct HeldButtons: View {
    let actions: HeldActions
    let cancel: (_ keepFile: Bool) -> Void
    let start: () -> Void

    var body: some View {
        HStack(spacing: 8) {
            Spacer(minLength: 0)
            CancelControl(cancel: actions.cancel, keep: actions.keepFile, action: cancel)
            if let s = actions.start {
                if s.keys.contains(.return) {
                    Button(s.title, action: start).buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction)
                        .disabled(!s.enabled).help(s.help)
                } else {
                    Button(s.title, action: start).buttonStyle(.borderedProminent).disabled(!s.enabled).help(s.help)
                }
            }
        }
        .padding(.bottom, actions.keepFile != nil ? 8 : 0)
    }
}
