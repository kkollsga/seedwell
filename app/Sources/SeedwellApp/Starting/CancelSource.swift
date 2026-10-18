import AppKit
import SwiftUI

/// The words of Cancel on a torrent that waits for a decision (SPEC 7A.6): Cancel removes the torrent and moves
/// the `.torrent` file it came from to the Trash; with Shift held it keeps the file, and a small caption slides
/// out under the button says so. The button always reads "Cancel". Without a file to trash (a magnet link, a
/// web address, a file that is gone, one the engine does not know) there is no Shift variant and no caption.
enum CancelText {
    static let title = "Cancel"
    static let keepCaption = "Keep .torrent file"

    /// The caption under the button: only while Shift is held and there is a file to keep.
    static func caption(shift: Bool, hasSourceFile: Bool) -> String? {
        keepsFile(shift: shift, hasSourceFile: hasSourceFile) ? keepCaption : nil
    }

    static func help(hasSourceFile: Bool) -> String {
        hasSourceFile
            ? "Remove this torrent and move its .torrent file to the Trash. Hold ⇧ to keep the file."
            : "Remove this torrent. Nothing has been downloaded"
    }

    /// ⇧ keeps the file, but only where there is a file to keep.
    static func keepsFile(shift: Bool, hasSourceFile: Bool) -> Bool { shift && hasSourceFile }
}

/// Whether Shift is down. QA can force it (`starting shift on|off`), since no key is held then.
enum ShiftKey {
    @MainActor static var qaOverride: Bool?
    @MainActor static var down: Bool { qaOverride ?? NSEvent.modifierFlags.contains(.shift) }
}

/// Moves the `.torrent` file of a cancelled torrent to the Trash, and nothing else. The core already checked that
/// the file is the torrent's own (same info-hash); here it must also still be a regular file, not a link, named
/// `*.torrent`. The move itself is a closure so that tests never touch the real Trash.
struct SourceTrash: Sendable {
    enum Result: Equatable { case trashed, skipped, failed(name: String) }

    var move: @Sendable (URL) throws -> Void

    static let live = SourceTrash { url in try FileManager.default.trashItem(at: url, resultingItemURL: nil) }

    /// The real Trash, or, when `SEEDWELL_QA_TRASH_DIR` names a folder under a temp root, that folder (QA only: the
    /// real Trash must not receive QA files).
    static func standard(environment: [String: String] = ProcessInfo.processInfo.environment) -> SourceTrash {
        guard let dir = environment["SEEDWELL_QA_TRASH_DIR"], !dir.isEmpty,
              ["/private/tmp/", "/tmp/", "/var/folders/", "/private/var/folders/"].contains(where: dir.hasPrefix)
        else { return .live }
        return SourceTrash { url in
            let fm = FileManager.default
            let folder = URL(fileURLWithPath: dir, isDirectory: true)
            try fm.createDirectory(at: folder, withIntermediateDirectories: true)
            var target = folder.appendingPathComponent(url.lastPathComponent)
            var n = 2
            while fm.fileExists(atPath: target.path) {
                target = folder.appendingPathComponent("\(url.deletingPathExtension().lastPathComponent) (\(n)).\(url.pathExtension)")
                n += 1
            }
            try fm.moveItem(at: url, to: target)
        }
    }

    func trash(_ path: String) -> Result {
        let url = URL(fileURLWithPath: path)
        // `attributesOfItem` does not follow links: a link reports `.typeSymbolicLink`.
        guard url.pathExtension.lowercased() == "torrent",
              (try? FileManager.default.attributesOfItem(atPath: path))?[.type] as? FileAttributeType == .typeRegular
        else { return .skipped }
        do {
            try move(url)
            return .trashed
        } catch {
            return .failed(name: url.lastPathComponent)
        }
    }
}

/// A normal Cancel button. While Shift is held a caption slides out just below it ("Keep .torrent file") and the
/// click keeps the file. The caption hangs in the popover's own padding, so nothing moves when it appears.
/// Gives its action whether to keep the `.torrent` file.
struct CancelControl: View {
    let hasSourceFile: Bool
    /// Escape cancels (⇧Escape keeps the file) while this is true.
    var escape = false
    /// Where the caption sits under the button.
    var captionAlignment: HorizontalAlignment = .trailing
    let action: (_ keepFile: Bool) -> Void
    @State private var shift = false

    var body: some View {
        let help = CancelText.help(hasSourceFile: hasSourceFile)
        // The state of Shift is read again at the click: a click that arrives before the next poll still counts.
        let go = { action(CancelText.keepsFile(shift: ShiftKey.down, hasSourceFile: hasSourceFile)) }
        Group {
            if escape {
                Button(CancelText.title, role: .cancel, action: go).keyboardShortcut(.cancelAction).help(help)
            } else {
                Button(CancelText.title, role: .cancel, action: go).help(help)
            }
        }
        .overlay(alignment: captionAlignment == .trailing ? .bottomTrailing : .bottomLeading) {
            if let caption = CancelText.caption(shift: shift, hasSourceFile: hasSourceFile) {
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
            if escape && hasSourceFile {
                Button("", action: { action(true) }).keyboardShortcut(.escape, modifiers: .shift)
                    .opacity(0).frame(width: 0, height: 0).accessibilityHidden(true)
            }
        }
        .task(id: hasSourceFile) {
            guard hasSourceFile else { shift = false; return }
            while !Task.isCancelled {
                shift = ShiftKey.down
                try? await Task.sleep(for: .milliseconds(100))
            }
        }
    }
}

/// The two actions of everything that is held or new (SPEC 7A.6): **Cancel** and **Start**, right-aligned on the
/// bottom line, Cancel first and Start (the default) last. Return is Start, Escape is Cancel and ⇧Escape is Cancel
/// keeping the `.torrent` file, for the block that `active` says owns the keys. The ⇧ caption hangs under Cancel.
struct HeldButtons: View {
    let hasSourceFile: Bool
    /// This block owns Return and Escape.
    var active = true
    var canStart = true
    var startHelp = "Start downloading now"
    let cancel: (_ keepFile: Bool) -> Void
    let start: () -> Void

    var body: some View {
        HStack(spacing: 8) {
            Spacer(minLength: 0)
            CancelControl(hasSourceFile: hasSourceFile, escape: active, action: cancel)
            if active {
                Button("Start", action: start).buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction)
                    .disabled(!canStart).help(startHelp)
            } else {
                Button("Start", action: start).buttonStyle(.borderedProminent).disabled(!canStart).help(startHelp)
            }
        }
        .padding(.bottom, hasSourceFile ? 8 : 0)
    }
}
