import AppKit
import SwiftUI
import UniformTypeIdentifiers

/// An image or file waiting in the composer: what the core takes when it is sent, and its chip (checked by the core).
struct ChatComposerImage: Identifiable {
    let id = UUID()
    let input: ChatImageInput
    let draft: ChatImageDraft
    let chip: NSImage?
}

/// The images and files the composer holds until the message is sent: added by the attach button, a paste (Cmd-V) or
/// a drop, each checked by the core first (image or file, caps, sizes and words come from Rust). Drawing and OS
/// services only.
@MainActor @Observable final class ChatComposerImages {
    var items: [ChatComposerImage] = []
    /// Images are being dragged over the conversation.
    var dropping = false
    /// Images pasted or dropped into the text (`first` and how many, counted in `items`, and where: the selection a
    /// paste replaces, the point a drop lands on, UTF-16): the composer puts a reference to each there
    /// (`chatComposerWithImages`).
    @ObservationIgnored var placed: ((_ first: Int, _ count: Int, _ at: NSRange?) -> Void)?
    /// An attachment taken out (its index): the composer drops its references (`chatComposerWithoutImage`).
    @ObservationIgnored var removed: ((Int) -> Void)?
    /// Added and still being checked: they take their places after `items`, in the order they came.
    @ObservationIgnored private var checking = 0
    /// The checks, one batch after another, so the attachments keep the order (and the numbers) they came in.
    @ObservationIgnored private var queue: Task<Void, Never>?

    /// Checks and adds images (off the main thread: a large photo takes a moment to check); a problem is said once. One
    /// that the core refuses gives up its place: its references go and the later ones are renumbered (`removed`).
    /// Returns how many were taken (the rest did not fit).
    @discardableResult func add(_ inputs: [ChatImageInput], core: Core) -> Int {
        let texts = ChatImageWords.texts(core)
        let room = Int(texts.maxImages) - items.count - checking
        if inputs.count > room { alert(texts.tooMany) }
        let taken = Array(inputs.prefix(max(room, 0)))
        guard !taken.isEmpty else { return 0 }
        checking += taken.count
        let before = queue
        queue = Task {
            await before?.value
            var checked: [Result<(ChatImageInput, ChatImageDraft), Error>] = []
            for input in taken {
                checked.append(await catching { (input, try await core.chatImageCheck(image: input)) })
            }
            checking -= taken.count
            var problems: [String] = []
            for r in checked {
                switch r {
                case .success(let (input, draft)) where items.count < Int(texts.maxImages):
                    items.append(ChatComposerImage(input: input, draft: draft, chip: NSImage(data: draft.previewPng)))
                case .success: removed?(items.count)
                case .failure(let e):
                    problems.append(SyncText.message(e))
                    removed?(items.count)
                }
            }
            if !problems.isEmpty { alert(problems.joined(separator: "\n")) }
        }
        return taken.count
    }

    /// The checks under way are done (tests).
    func settled() async { await queue?.value }

    func remove(_ id: UUID) {
        guard let n = items.firstIndex(where: { $0.id == id }) else { return }
        items.remove(at: n)
        removed?(n)
    }

    /// The attach button: an open panel for the files the core takes (any, when it names no types).
    func choose(core: Core) {
        let panel = NSOpenPanel()
        panel.allowsMultipleSelection = true
        panel.canChooseDirectories = false
        let types = ChatImageWords.texts(core).extensions.compactMap { UTType(filenameExtension: $0) }
        if !types.isEmpty { panel.allowedContentTypes = types }
        guard panel.runModal() == .OK else { return }
        add(panel.urls.map(ChatImagePasteboard.input(file:)), core: core)
    }

    /// A paste (Cmd-V, Edit > Paste) or a drop onto the text with images or files on the pasteboard: they are attached
    /// (true), with a reference each `at` the selection or drop point when the field holds text; with text, the field
    /// pastes it (false).
    func paste(from pb: NSPasteboard = .general, core: Core, at: NSRange? = nil) -> Bool {
        guard ChatImagePasteboard.hasImage(pb) else { return false }
        let inputs = ChatImagePasteboard.inputs(from: pb, pastedName: ChatImageWords.texts(core).pastedName)
        guard !inputs.isEmpty else { return false }
        let first = items.count + checking
        let taken = add(inputs, core: core)
        placed?(first, taken, at)
        return true
    }

    /// A drop: image files, or pictures (PNG, TIFF) from other apps.
    func drop(_ providers: [NSItemProvider], core: Core) -> Bool {
        let pasted = ChatImageWords.texts(core).pastedName
        var any = false
        for p in providers {
            if p.hasItemConformingToTypeIdentifier(UTType.fileURL.identifier) {
                any = true
                _ = p.loadObject(ofClass: URL.self) { url, _ in
                    guard let url else { return }
                    Task { @MainActor in self.add([ChatImagePasteboard.input(file: url)], core: core) }
                }
            } else if let type = [UTType.png, UTType.tiff].first(where: { p.hasItemConformingToTypeIdentifier($0.identifier) }) {
                any = true
                p.loadDataRepresentation(forTypeIdentifier: type.identifier) { data, _ in
                    guard let data else { return }
                    let png = type == .png ? data : NSBitmapImageRep(data: data)?.representation(using: .png, properties: [:])
                    guard let png else { return }
                    Task { @MainActor in self.add([ChatImageInput(name: pasted, path: nil, bytes: png)], core: core) }
                }
            }
        }
        return any
    }

    /// Posts `text` with the attachments to a channel's General or topic, or a private conversation (`dm:<device>`), as
    /// the person at this Mac. The core reads the files after this returns; the conversation shows them meanwhile.
    func send(core: Core, place: String, topic: String?, text: String, replyTo: String?, done: @escaping (Bool) -> Void) {
        let inputs = items.map(\.input)
        let body = chatComposerBody(draft: text) ?? ""
        items = []
        Task {
            let r = await catching { try await core.chatSendAttachments(place: place, topic: topic, body: body, replyTo: replyTo, inputs: inputs) }
            if case .failure(let e) = r {
                alert(SyncText.message(e))
                if items.isEmpty { add(inputs, core: core) }
            }
            done((try? r.get()) != nil)
        }
    }

    private func alert(_ text: String) {
        let a = NSAlert()
        a.messageText = text
        a.runModal()
    }
}

/// The composer's images: a row of chips, each with its preview, words and a remove button.
struct ChatComposerChips: View {
    let images: ChatComposerImages
    let core: Core

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: Theme.Space.s) {
                ForEach(images.items) { item in
                    HStack(spacing: Theme.Space.s - 2) {
                        if let chip = item.chip {
                            Image(nsImage: chip).resizable().aspectRatio(contentMode: .fill).frame(width: 32, height: 32)
                                .clipShape(RoundedRectangle(cornerRadius: 4, style: .continuous))
                        } else if item.draft.file {
                            Image(systemName: ChatFileLayout.symbol(item.draft.icon)).font(.system(size: Theme.Icon.large)).foregroundStyle(.secondary)
                                .frame(width: 32, height: 32)
                        }
                        Text(item.draft.caption).font(Theme.caption).lineLimit(1).truncationMode(.middle).frame(maxWidth: 220)
                        Button { images.remove(item.id) } label: { Image(systemName: "xmark.circle.fill") }
                            .buttonStyle(.plain).foregroundStyle(.secondary)
                            .accessibilityLabel(ChatImageWords.texts(core).remove).help(ChatImageWords.texts(core).remove)
                    }
                    .padding(Theme.Space.xs)
                    .background(Color.primary.opacity(0.06), in: RoundedRectangle(cornerRadius: Theme.Radius.medium, style: .continuous))
                }
            }
        }
    }
}

/// The attach button (a paperclip).
struct ChatAttachButton: View {
    let images: ChatComposerImages
    let core: Core

    var body: some View {
        Button { images.choose(core: core) } label: {
            Image(systemName: "paperclip").font(.system(size: Theme.Icon.regular + 1)).foregroundStyle(.secondary)
                .frame(width: 28, height: 30).contentShape(Rectangle())
        }
            .buttonStyle(.plain)
            .help(ChatImageWords.texts(core).attachHelp)
            .accessibilityLabel(ChatImageWords.texts(core).attach)
            .accessibilityIdentifier("chat-attach")
    }
}

extension View {
    /// Images and files dropped onto the conversation go into the composer, without a reference (a drop onto the text
    /// is the field's: `ChatComposerTextView`).
    func chatImageInput(_ images: ChatComposerImages, core: Core, enabled: Bool) -> some View {
        modifier(ChatImageInputModifier(images: images, core: core, enabled: enabled))
    }
}

struct ChatImageInputModifier: ViewModifier {
    let images: ChatComposerImages
    let core: Core
    let enabled: Bool

    func body(content: Content) -> some View {
        content
            .onDrop(of: [.fileURL, .png, .tiff], isTargeted: Binding(get: { images.dropping }, set: { images.dropping = $0 && enabled })) { providers in
                enabled && images.drop(providers, core: core)
            }
            .overlay {
                if images.dropping {
                    RoundedRectangle(cornerRadius: Theme.Radius.large, style: .continuous).strokeBorder(Color.accentColor, lineWidth: 2)
                        .overlay(Text(ChatImageWords.texts(core).dropHint).font(Theme.body.weight(.medium)).padding(Theme.Space.s + 2)
                            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: Theme.Radius.medium)))
                        .padding(Theme.Space.s - 2).allowsHitTesting(false)
                }
            }
    }
}
