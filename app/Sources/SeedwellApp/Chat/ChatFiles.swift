import AppKit
import SwiftUI

/// Files in chat (SPEC 7G.7 Files): drawing only. Each card's state, words and buttons are the core's
/// (`crates/ffi/src/chat_files.rs`); this draws them and calls back. Nothing downloads or opens by itself.
enum ChatFileLayout {
    /// The SF Symbol for a file's icon.
    static func symbol(_ icon: ChatFileIcon) -> String {
        switch icon {
        case .document: return "doc.richtext"
        case .text: return "doc.text"
        case .pdf: return "doc.text.image"
        case .image: return "photo"
        case .audio: return "waveform"
        case .video: return "film"
        case .archive: return "doc.zipper"
        case .code: return "chevron.left.forwardslash.chevron.right"
        case .app: return "app.badge"
        case .diskImage: return "externaldrive"
        case .spreadsheet: return "tablecells"
        case .presentation: return "rectangle.on.rectangle"
        case .other: return "doc"
        }
    }

    /// The widest a card is drawn, in points.
    static let width: CGFloat = 360
}

/// A message's files, one card under the other.
struct ChatFilesView: View {
    let core: Core
    let place: String
    let files: [ChatFileCard]

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.s - 2) {
            // By place: the same file twice in one message is two cards (one identity each).
            ForEach(Array(files.enumerated()), id: \.offset) { ChatFileCardView(core: core, place: place, card: $1) }
        }
        .padding(.top, Theme.Space.hair)
    }
}

/// One file: its icon, name, size and state, a progress bar while it downloads, and its buttons.
struct ChatFileCardView: View {
    let core: Core
    let place: String
    let card: ChatFileCard

    var body: some View {
        HStack(alignment: .top, spacing: Theme.Space.m - 2) {
            Image(systemName: ChatFileLayout.symbol(card.icon)).font(.system(size: Theme.Icon.large)).foregroundStyle(.secondary)
                .frame(width: 34, height: 34)
                .background(RoundedRectangle(cornerRadius: Theme.Radius.small + 1, style: .continuous).fill(Color.primary.opacity(0.06)))
            VStack(alignment: .leading, spacing: Theme.Space.hair) {
                Text(card.name).font(Theme.body.weight(.medium)).lineLimit(1).truncationMode(.middle)
                Text(card.status).font(Theme.caption).foregroundStyle(card.state == .failed || card.state == .gone ? .red : .secondary)
                    .lineLimit(2).fixedSize(horizontal: false, vertical: true)
                if let p = card.progress { ProgressView(value: p).controlSize(.small).padding(.top, Theme.Space.hair) }
                if !card.actions.isEmpty {
                    HStack(spacing: Theme.Space.s - 2) {
                        ForEach(Array(card.actions.enumerated()), id: \.offset) { n, a in
                            Button(a.label) { run(a.kind) }.controlSize(.small)
                                .buttonStyle(.bordered).tint(n == 0 ? .accentColor : nil)
                        }
                    }
                    .padding(.top, Theme.Space.xs)
                }
            }
            Spacer(minLength: 0)
        }
        .padding(Theme.Space.s + 2)
        .frame(maxWidth: ChatFileLayout.width, alignment: .leading)
        .background(Color.primary.opacity(0.035), in: RoundedRectangle(cornerRadius: Theme.Radius.medium + 2, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: Theme.Radius.medium + 2, style: .continuous).strokeBorder(Color.primary.opacity(0.08)))
        .help(card.status)
        .accessibilityElement(children: .contain)
        .accessibilityLabel("\(card.name), \(card.status)")
    }

    private func run(_ kind: ChatFileActionKind) {
        let (core, place, hash) = (core, place, card.hash)
        // The core's calls run off the main thread; a failure is told in an alert.
        func failed(_ error: Error) {
            let a = NSAlert()
            a.messageText = SyncText.message(error)
            a.runModal()
        }
        switch kind {
        case .saveAs:
            let panel = NSSavePanel()
            panel.nameFieldStringValue = core.chatFileSaveName(place: place, hash: hash)
            panel.canCreateDirectories = true
            guard panel.runModal() == .OK, let url = panel.url else { return }
            Task { if case .failure(let e) = await catching({ try await core.chatFileDownload(place: place, hash: hash, to: url.path) }) { failed(e) } }
        case .showInFolder:
            if let path = card.path { NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)]) }
        case .open:
            Task {
                switch await catching({ try await core.chatFileOpen(place: place, hash: hash) }) {
                case .failure(let e): failed(e)
                case .success(let open):
                    if let ask = open.confirm, !ChatFileCardView.confirm(ask) { return }
                    NSWorkspace.shared.open(URL(fileURLWithPath: open.path))
                }
            }
        case .download, .retry, .cancel, .stopSharing:
            Task { if case .failure(let e) = await catching({ try await ChatFileCardView.press(kind, core: core, place: place, hash: hash) }) { failed(e) } }
        }
    }

    /// The buttons that call the core and nothing else (also the QA command `chatfile`): the card as it stands after.
    nonisolated static func press(_ kind: ChatFileActionKind, core: Core, place: String, hash: String) async throws -> ChatFileCard? {
        switch kind {
        case .download, .retry: return try await core.chatFileDownload(place: place, hash: hash, to: nil)
        case .cancel: return try await core.chatFileCancel(place: place, hash: hash)
        case .stopSharing: return try await core.chatFileStopSharing(place: place, hash: hash)
        case .saveAs, .showInFolder, .open: return nil
        }
    }

    /// Asks the core's question before opening an app or a script: true to open it.
    static func confirm(_ ask: ChatFileConfirm) -> Bool {
        alert(ask).runModal() == .alertSecondButtonReturn
    }

    /// The question as an alert (Cancel first, the default; Open second).
    static func alert(_ ask: ChatFileConfirm) -> NSAlert {
        let a = NSAlert()
        a.alertStyle = .warning
        a.messageText = ask.title
        a.informativeText = ask.message
        a.addButton(withTitle: ask.cancel)
        a.addButton(withTitle: ask.open)
        return a
    }
}

/// Sends whose files are being read, or that failed: a line each above the composer.
struct ChatPendingSendsView: View {
    let core: Core
    let place: String
    let pending: [ChatPendingSend]
    let changed: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: Theme.Space.xs) {
            ForEach(pending, id: \.id) { p in
                HStack(spacing: Theme.Space.s) {
                    if p.failed {
                        Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.orange)
                        VStack(alignment: .leading, spacing: 1) {
                            Text(p.title).font(Theme.caption.weight(.medium))
                            Text(p.problem).font(Theme.caption).foregroundStyle(.secondary).lineLimit(4).fixedSize(horizontal: false, vertical: true)
                        }
                        Spacer()
                        Button(p.dismiss) { core.chatDismissPending(place: place, id: p.id); changed() }.controlSize(.small)
                    } else {
                        ProgressView(value: p.progress).controlSize(.small).frame(width: 80)
                        Text(p.title).font(Theme.caption).foregroundStyle(.secondary).lineLimit(1)
                        Spacer()
                    }
                }
            }
        }
    }
}
