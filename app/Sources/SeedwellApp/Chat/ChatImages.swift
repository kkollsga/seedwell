import AppKit
import QuickLook
import SwiftUI

/// Images in chat (SPEC 7G.7 Attachments): drawing only. The core decides each image's state and words
/// (`crates/ffi/src/chat_images.rs`) and makes every picture shown here (PNG or JPEG, an SVG drawn in Rust): this never
/// hands another device's bytes or an SVG to the system's decoders.
enum ChatImageLayout {
    /// The largest an image is shown in the conversation, in points; its aspect ratio is kept.
    static let maxWidth: CGFloat = 420
    static let maxHeight: CGFloat = 320
    /// Pixels asked of the core for that (Retina).
    static let side: UInt32 = 840

    /// The size an image of `width` × `height` pixels is shown at: fitted into the column (at most `maxWidth` wide, or
    /// `available` when that is less) and `maxHeight` high, never larger than it is, never smaller than 40 points.
    static func fit(width: UInt32, height: UInt32, available: CGFloat = .infinity) -> CGSize {
        let (w, h) = (CGFloat(max(width, 1)), CGFloat(max(height, 1)))
        let scale = min(min(maxWidth, available) / w, maxHeight / h, 1)
        return CGSize(width: max(40, (w * scale).rounded()), height: max(40, (h * scale).rounded()))
    }
}

/// The core's labels, read once.
@MainActor enum ChatImageWords {
    private static var cached: ChatImageTexts?
    static func texts(_ core: Core) -> ChatImageTexts {
        if let t = cached { return t }
        let t = core.chatImageTexts()
        cached = t
        return t
    }
}

/// The pictures the core made, kept while the app runs (a refresh draws nothing again).
@MainActor final class ChatImageCache {
    static let shared = ChatImageCache()
    private var images: [String: NSImage] = [:]
    private var order: [String] = []

    func image(_ key: String) -> NSImage? { images[key] }

    func put(_ key: String, _ image: NSImage) {
        if images[key] == nil { order.append(key) }
        images[key] = image
        while order.count > 150 { images[order.removeFirst()] = nil }
    }
}

/// A message's images, one under the other.
struct ChatImagesView: View {
    let core: Core
    let channelId: String
    let images: [ChatImage]

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(images, id: \.hash) { ChatImageTile(core: core, channelId: channelId, image: $0) }
        }
        .padding(.top, 4)
    }
}

/// One image: the picture when it is here, else its preview with what the core says about it. A click opens it in
/// Quick Look; its menu saves, copies and opens it.
struct ChatImageTile: View {
    let core: Core
    let channelId: String
    let image: ChatImage
    /// The snapshot renderer's fixture (`chatImageSamples`): its pictures are drawn instead of asking the core.
    let sample: ChatImageSample?
    @State private var picture: NSImage?
    @State private var preview: NSImage?
    @State private var quickLook: URL?

    init(core: Core, channelId: String, image: ChatImage, sample: ChatImageSample? = nil) {
        self.core = core
        self.channelId = channelId
        self.image = image
        self.sample = sample
    }

    private var texts: ChatImageTexts { ChatImageWords.texts(core) }

    var body: some View {
        let size = ChatImageLayout.fit(width: image.width, height: image.height)
        VStack(alignment: .leading, spacing: 3) {
            ZStack {
                if let picture {
                    Image(nsImage: picture).resizable().interpolation(.high).aspectRatio(contentMode: .fit)
                } else if let preview {
                    Image(nsImage: preview).resizable().aspectRatio(contentMode: .fit).blur(radius: 3).opacity(0.7)
                } else {
                    Rectangle().fill(Color.secondary.opacity(0.12))
                }
                if image.state != .available { placeholder }
            }
            .frame(width: size.width, height: size.height)
            .clipShape(RoundedRectangle(cornerRadius: 6, style: .continuous))
            .overlay(RoundedRectangle(cornerRadius: 6, style: .continuous).strokeBorder(Color.secondary.opacity(0.15)))
            .contentShape(Rectangle())
            .onTapGesture { if image.state == .available { openQuickLook() } }
            .contextMenu { menu }
            .help(image.caption)
            .accessibilityLabel(image.caption)
            Text(image.caption).font(.system(size: 11)).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle)
        }
        .task(id: "\(image.hash)-\(String(describing: image.state))") { await load() }
        .quickLookPreview($quickLook)
    }

    /// What shows over the preview while the picture is not here.
    private var placeholder: some View {
        VStack(spacing: 6) {
            if image.state == .fetching { ProgressView().controlSize(.small) }
            Text(image.status).font(.system(size: 11.5)).multilineTextAlignment(.center)
            if image.state == .notDownloaded || image.state == .failed {
                Button(texts.download) { fetch() }.controlSize(.small)
            }
        }
        .padding(10)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
        .padding(8)
    }

    @ViewBuilder private var menu: some View {
        if image.state == .available {
            Button(texts.open) { openInApp() }
            Button(texts.copyImage) { copy() }
            Divider()
            Button(texts.saveAs) { save(asPng: false) }
            if image.kind == .svg { Button(texts.saveAsPng) { save(asPng: true) } }
        } else if image.state == .notDownloaded || image.state == .failed {
            Button(texts.download) { fetch() }
        }
    }

    private func load() async {
        if let sample {
            picture = sample.view.flatMap { NSImage(data: $0.bytes) }
            preview = sample.preview.flatMap { NSImage(data: $0.bytes) }
            return
        }
        let (core, channel, hash) = (core, channelId, image.hash)
        let key = "\(channel)/\(hash)/\(ChatImageLayout.side)"
        if image.state == .available {
            if let cached = ChatImageCache.shared.image(key) { picture = cached; return }
            let view = await core.chatImageView(channelId: channel, hash: hash, maxSide: ChatImageLayout.side)
            if let view, let img = NSImage(data: view.bytes) {
                ChatImageCache.shared.put(key, img)
                picture = img
            }
        } else if image.hasPreview, preview == nil {
            let view = await core.chatImagePreview(channelId: channel, hash: hash)
            if let view { preview = NSImage(data: view.bytes) }
        }
    }

    private func fetch() {
        let (core, channel, hash) = (core, channelId, image.hash)
        Task { if case .failure(let error) = await catching({ try await core.chatImageFetch(channelId: channel, hash: hash) }) { alert(error) } }
    }

    /// A file of the picture the core made (never the bytes as they came), for Quick Look and the default viewer.
    private func pictureFile() async -> URL? {
        let (core, channel, hash) = (core, channelId, image.hash)
        let r = await catching { try await core.chatImageOpenPath(channelId: channel, hash: hash) }
        switch r {
        case .success(let path): return URL(fileURLWithPath: path)
        case .failure(let e): alert(e); return nil
        }
    }

    private func openQuickLook() { Task { quickLook = await pictureFile() } }

    private func openInApp() { Task { if let url = await pictureFile() { NSWorkspace.shared.open(url) } } }

    private func copy() {
        let (core, channel, hash) = (core, channelId, image.hash)
        Task {
            let view = await core.chatImageView(channelId: channel, hash: hash, maxSide: 0)
            guard let view else { return }
            ChatImagePasteboard.write(view, to: .general)
        }
    }

    private func save(asPng: Bool) {
        let panel = NSSavePanel()
        panel.nameFieldStringValue = core.chatImageSaveName(channelId: channelId, hash: image.hash, asPng: asPng)
        panel.canCreateDirectories = true
        guard panel.runModal() == .OK, let url = panel.url else { return }
        let (core, channel, hash) = (core, channelId, image.hash)
        Task {
            if case .failure(let error) = await catching({ try await core.chatImageSave(channelId: channel, hash: hash, path: url.path, asPng: asPng) }) {
                alert(error)
            }
        }
    }

    private func alert(_ error: Error) {
        let a = NSAlert()
        a.messageText = SyncText.message(error)
        a.runModal()
    }
}

/// The pasteboard and images: what a paste or a drop holds, and a picture copied.
enum ChatImagePasteboard {
    /// The images on a pasteboard, as the core takes them: image files (their paths) first, else a picture's bytes (PNG
    /// as it is; TIFF, which screenshots and other apps put there, made a PNG by the system). Empty: none.
    static func inputs(from pb: NSPasteboard, pastedName: String) -> [ChatImageInput] {
        let urls = (pb.readObjects(forClasses: [NSURL.self], options: [.urlReadingFileURLsOnly: true]) as? [URL]) ?? []
        if !urls.isEmpty { return urls.map(input(file:)) }
        if let png = pb.data(forType: .png) { return [ChatImageInput(name: pastedName, path: nil, bytes: png)] }
        if let tiff = pb.data(forType: .tiff), let png = NSBitmapImageRep(data: tiff)?.representation(using: .png, properties: [:]) {
            return [ChatImageInput(name: pastedName, path: nil, bytes: png)]
        }
        return []
    }

    /// The pasteboard holds an image (a picture, or a file) rather than text to paste.
    static func hasImage(_ pb: NSPasteboard) -> Bool {
        if pb.availableType(from: [.png, .tiff]) != nil { return true }
        let urls = (pb.readObjects(forClasses: [NSURL.self], options: [.urlReadingFileURLsOnly: true]) as? [URL]) ?? []
        return !urls.isEmpty
    }

    static func input(file url: URL) -> ChatImageInput {
        ChatImageInput(name: url.lastPathComponent, path: url.path, bytes: nil)
    }

    /// Puts a picture the core made on a pasteboard: its own bytes, and an image for apps that take only TIFF.
    static func write(_ view: ChatImageView, to pb: NSPasteboard) {
        pb.clearContents()
        if let img = NSImage(data: view.bytes) { pb.writeObjects([img]) }
        pb.setData(view.bytes, forType: view.mime == "image/jpeg" ? NSPasteboard.PasteboardType("public.jpeg") : .png)
    }
}
