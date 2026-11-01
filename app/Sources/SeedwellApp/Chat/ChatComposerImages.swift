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

    /// Checks and adds images (off the main thread: a large photo takes a moment to check); a problem is said once.
    func add(_ inputs: [ChatImageInput], core: Core) {
        let texts = ChatImageWords.texts(core)
        let room = Int(texts.maxImages) - items.count
        if inputs.count > room { alert(texts.tooMany) }
        let taken = Array(inputs.prefix(max(room, 0)))
        guard !taken.isEmpty else { return }
        Task {
            let checked = await Task.detached(priority: .userInitiated) {
                taken.map { input in Result { (input, try core.chatImageCheck(image: input)) } }
            }.value
            var problems: [String] = []
            for r in checked {
                switch r {
                case .success(let (input, draft)):
                    if items.count < Int(texts.maxImages) {
                        items.append(ChatComposerImage(input: input, draft: draft, chip: NSImage(data: draft.previewPng)))
                    }
                case .failure(let e): problems.append(SyncText.message(e))
                }
            }
            if !problems.isEmpty { alert(problems.joined(separator: "\n")) }
        }
    }

    func remove(_ id: UUID) { items.removeAll { $0.id == id } }

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

    /// Cmd-V with images on the pasteboard: they are attached (true); with text, the field pastes it (false).
    func paste(from pb: NSPasteboard = .general, core: Core) -> Bool {
        guard ChatImagePasteboard.hasImage(pb) else { return false }
        let inputs = ChatImagePasteboard.inputs(from: pb, pastedName: ChatImageWords.texts(core).pastedName)
        guard !inputs.isEmpty else { return false }
        add(inputs, core: core)
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
        let body = text.trimmingCharacters(in: .whitespacesAndNewlines)
        items = []
        Task {
            let r = await Task.detached(priority: .userInitiated) {
                Result { try core.chatSendAttachments(place: place, topic: topic, body: body, replyTo: replyTo, inputs: inputs) }
            }.value
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
            HStack(spacing: 8) {
                ForEach(images.items) { item in
                    HStack(spacing: 6) {
                        if let chip = item.chip {
                            Image(nsImage: chip).resizable().aspectRatio(contentMode: .fill).frame(width: 32, height: 32)
                                .clipShape(RoundedRectangle(cornerRadius: 4, style: .continuous))
                        } else if item.draft.file {
                            Image(systemName: ChatFileLayout.symbol(item.draft.icon)).font(.system(size: 18)).foregroundStyle(.secondary)
                                .frame(width: 32, height: 32)
                        }
                        Text(item.draft.caption).font(.system(size: 11)).lineLimit(1).truncationMode(.middle).frame(maxWidth: 220)
                        Button { images.remove(item.id) } label: { Image(systemName: "xmark.circle.fill") }
                            .buttonStyle(.plain).foregroundStyle(.secondary)
                            .accessibilityLabel(ChatImageWords.texts(core).remove).help(ChatImageWords.texts(core).remove)
                    }
                    .padding(4)
                    .background(Color.secondary.opacity(0.1), in: RoundedRectangle(cornerRadius: 6, style: .continuous))
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
        Button { images.choose(core: core) } label: { Image(systemName: "paperclip") }
            .buttonStyle(.borderless)
            .help(ChatImageWords.texts(core).attachHelp)
            .accessibilityLabel(ChatImageWords.texts(core).attach)
            .accessibilityIdentifier("chat-attach")
    }
}

extension View {
    /// Images pasted (Cmd-V while the field is focused) or dropped onto the conversation go into the composer. The key
    /// is the user's own: this only looks at it, and passes it on to the field when the pasteboard holds no image.
    func chatImageInput(_ images: ChatComposerImages, core: Core, enabled: Bool, focused: Bool) -> some View {
        modifier(ChatImageInputModifier(images: images, core: core, enabled: enabled, focused: focused))
    }
}

struct ChatImageInputModifier: ViewModifier {
    let images: ChatComposerImages
    let core: Core
    let enabled: Bool
    let focused: Bool
    @State private var monitor: Any?

    func body(content: Content) -> some View {
        content
            .onDrop(of: [.fileURL, .png, .tiff], isTargeted: Binding(get: { images.dropping }, set: { images.dropping = $0 && enabled })) { providers in
                enabled && images.drop(providers, core: core)
            }
            .overlay {
                if images.dropping {
                    RoundedRectangle(cornerRadius: 10, style: .continuous).strokeBorder(Color.accentColor, lineWidth: 2)
                        .overlay(Text(ChatImageWords.texts(core).dropHint).font(.system(size: 13, weight: .medium)).padding(10)
                            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 8)))
                        .padding(6).allowsHitTesting(false)
                }
            }
            .onChange(of: enabled && focused, initial: true) { _, on in on ? install() : uninstall() }
            .onDisappear { uninstall() }
    }

    private func install() {
        guard monitor == nil else { return }
        let (images, core) = (images, core)
        monitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { event in
            ChatImageInputModifier.isPaste(event) && MainActor.assumeIsolated({ images.paste(core: core) }) ? nil : event
        }
    }

    private func uninstall() {
        if let m = monitor { NSEvent.removeMonitor(m) }
        monitor = nil
    }

    /// Cmd-V, without other modifiers.
    nonisolated static func isPaste(_ event: NSEvent) -> Bool {
        event.modifierFlags.intersection(.deviceIndependentFlagsMask).subtracting([.capsLock, .numericPad, .function]) == .command
            && event.charactersIgnoringModifiers?.lowercased() == "v"
    }
}
