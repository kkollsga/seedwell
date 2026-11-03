import AppKit
import QuickLook
import SwiftUI

/// Images in chat (SPEC 7G.7 Attachments): drawing only. The core decides each image's state and words
/// (`crates/ffi/src/chat_images.rs`) and makes every picture shown here (PNG or JPEG, an SVG drawn in Rust): this never
/// hands another device's bytes or an SVG to the system's decoders.
enum ChatImageLayout {
    /// The tallest an image is shown, in points (a tall screenshot is fitted into it, never cropped).
    static let maxHeight: CGFloat = 420
    /// The width assumed until the message's content area has been measured.
    static let fallbackWidth: CGFloat = 560
    /// Pixels asked of the core: the widest content area on a Retina display.
    static let side: UInt32 = 1600
    /// A gallery's filmstrip: its thumbnails' boxes and the picture's room inside one.
    static let thumb: CGFloat = 46
    static let thumbInset: CGFloat = 5

    /// Pixels per point: an image is drawn at most at its own pixels (2 on a Retina display), never enlarged, so a
    /// small one stays sharp.
    @MainActor static var pixelsPerPoint: CGFloat { NSScreen.main?.backingScaleFactor ?? 2 }

    /// The size an image of `width` × `height` pixels is shown at: at most its own size, fitted into `available` points
    /// of width and `maxHeight` of height, its aspect ratio kept; never smaller than 16 points. `scale`: pixels per point
    /// (the display's when not given).
    @MainActor static func fit(width: UInt32, height: UInt32, available: CGFloat = fallbackWidth, scale ppp: CGFloat? = nil) -> CGSize {
        let ppp = ppp ?? pixelsPerPoint
        let (w, h) = (CGFloat(max(width, 1)) / ppp, CGFloat(max(height, 1)) / ppp)
        let scale = min(available / w, maxHeight / h, 1)
        return CGSize(width: max(16, (w * scale).rounded()), height: max(16, (h * scale).rounded()))
    }
}

/// The snapshot renderer's fixtures (`chatImageSamples`, `chatImageLayoutSamples`) by hash: their pictures are drawn
/// instead of asking the core. Empty in the app.
extension EnvironmentValues {
    @Entry var chatImageSamples: [String: ChatImageSample] = [:]
    /// The image a gallery shows first (the renderer's; the app starts at the first).
    @Entry var chatGalleryStart: Int = 0
    /// Scrolls the conversation to a message's top (an image reference in its text shows the gallery above).
    @Entry var chatReveal: (String) -> Void = { _ in }
    /// Scrolls to a message and lights it for a moment (a reply's quote).
    @Entry var chatShowMessage: (String) -> Void = { _ in }
    /// The message lit for a moment.
    @Entry var chatHighlighted: String? = nil
    /// The code blocks show their copy button as under the pointer (the snapshots).
    @Entry var chatCodeHover: Bool = false
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

    /// An image's picture (when it is here) and its preview (when it is not and its message carries one): a fixture's,
    /// the cache's, or asked of the core off the main thread.
    func load(core: Core, channelId: String, image: ChatImage, sample: ChatImageSample?) async -> (picture: NSImage?, preview: NSImage?) {
        if let sample {
            return (sample.view.flatMap { NSImage(data: $0.bytes) }, sample.preview.flatMap { NSImage(data: $0.bytes) })
        }
        let key = "\(channelId)/\(image.hash)/\(ChatImageLayout.side)"
        if image.state == .available {
            if let cached = self.image(key) { return (cached, nil) }
            let view = await core.chatImageView(channelId: channelId, hash: image.hash, maxSide: ChatImageLayout.side)
            guard let view, let img = NSImage(data: view.bytes) else { return (nil, nil) }
            put(key, img)
            return (img, nil)
        }
        guard image.hasPreview else { return (nil, nil) }
        let view = await core.chatImagePreview(channelId: channelId, hash: image.hash)
        return (nil, view.flatMap { NSImage(data: $0.bytes) })
    }
}

/// A message's images (the core's order): one alone, at most as wide as the content area and `maxHeight` high, never
/// enlarged; several as a gallery. Centred in a channel's column, on its side under a bubble.
struct ChatImagesView: View {
    let core: Core
    let channelId: String
    let images: [ChatImage]
    /// More than one: a gallery (the core's `ChatMessageCard.gallery`).
    var gallery = false
    /// Where a picture narrower than the area sits.
    var alignment: Alignment = .center
    /// The widest the area is (a bubble's), else the content area's width.
    var maxWidth: CGFloat = .infinity
    /// The gallery's image shown, when one was chosen (a thumbnail, an arrow, a reference in the text).
    var selection: Binding<Int?> = .constant(nil)
    @State private var width: CGFloat = 0
    @Environment(\.chatGalleryStart) private var start

    var body: some View {
        let available = min(width > 0 ? width : ChatImageLayout.fallbackWidth, maxWidth)
        Group {
            if gallery {
                ChatGalleryView(core: core, channelId: channelId, images: images, available: available, selection: selection, start: start)
            } else if let image = images.first {
                ChatImageTile(core: core, channelId: channelId, image: image, available: available)
                    .frame(width: available, alignment: alignment)
            }
        }
        .frame(maxWidth: .infinity, alignment: alignment)
        .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { width = $0 }
        .padding(.top, Theme.Space.hair)
    }
}

/// Several images as Finder's Gallery view has them: one large on a stage as tall as the tallest of them (capped), so
/// switching never changes the message's height; a filmstrip of evenly sized thumbnails under it, centred, scrolling
/// sideways when there are many, the active one in the accent's rounded highlight and the others dimmed until the
/// pointer is on them; arrows over the stage while the pointer is on it, the arrow keys, and a quiet "2 / 5".
struct ChatGalleryView: View {
    let core: Core
    let channelId: String
    let images: [ChatImage]
    let available: CGFloat
    /// The image shown when one was chosen; else `start`.
    @Binding var selection: Int?
    var start = 0
    @State private var overStage = false
    @FocusState private var focused: Bool

    private var active: Int { Self.active(selection: selection, start: start, count: images.count) }

    /// The image shown: the one chosen, else `start`, within the gallery.
    static func active(selection: Int?, start: Int = 0, count: Int) -> Int { min(max(selection ?? start, 0), max(count - 1, 0)) }

    /// The image after (`step` 1: the right arrow, key or button) or before (-1: the left) `active`; nil at either end.
    static func step(_ active: Int, by step: Int, count: Int) -> Int? {
        let next = active + step
        return (0..<count).contains(next) ? next : nil
    }

    var body: some View {
        let stage = images.map { ChatImageLayout.fit(width: $0.width, height: $0.height, available: available).height }.max() ?? 0
        let shown = images[min(active, images.count - 1)]
        VStack(spacing: Theme.Space.s) {
            ChatImageTile(core: core, channelId: channelId, image: shown, available: available)
                .id(shown.hash)
                .frame(width: available, height: stage)
                .overlay(alignment: .leading) { if overStage && active > 0 { arrow("chevron.left") { go(-1) } } }
                .overlay(alignment: .trailing) { if overStage && active < images.count - 1 { arrow("chevron.right") { go(1) } } }
                .overlay(alignment: .topTrailing) {
                    Text(chatGalleryCounter(active: UInt32(active + 1), count: UInt32(images.count)))
                        .font(Theme.micro).monospacedDigit().foregroundStyle(.secondary)
                        .padding(.horizontal, Theme.Space.s - 2).padding(.vertical, Theme.Space.hair)
                        .background(.regularMaterial, in: Capsule())
                        .padding(Theme.Space.s)
                }
                .onHover { overStage = $0 }
            filmstrip
        }
        .frame(width: available)
        .focusable()
        .focused($focused)
        .focusEffectDisabled()
        .onKeyPress(.leftArrow) { go(-1) ? .handled : .ignored }
        .onKeyPress(.rightArrow) { go(1) ? .handled : .ignored }
    }

    private var filmstrip: some View {
        ScrollViewReader { scroll in
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: Theme.Space.xs) {
                    // By place, not by picture: the same picture twice in one message is two thumbnails.
                    ForEach(Array(images.enumerated()), id: \.offset) { n, image in
                        ChatGalleryThumb(core: core, channelId: channelId, image: image, active: n == active)
                            .id(n)
                            .onTapGesture { selection = n; focused = true }
                    }
                }
                .padding(.horizontal, Theme.Space.hair)
                .frame(minWidth: available)
            }
            .frame(width: available)
            .onChange(of: active) { _, n in withAnimation(.easeOut(duration: 0.15)) { scroll.scrollTo(n, anchor: .center) } }
            .onAppear { scroll.scrollTo(active, anchor: .center) }
        }
    }

    private func arrow(_ symbol: String, _ action: @escaping () -> Void) -> some View {
        Button { action(); focused = true } label: {
            Image(systemName: symbol).font(Theme.body.weight(.semibold))
                .frame(width: 30, height: 30)
                .background(.regularMaterial, in: Circle())
                .overlay(Circle().strokeBorder(Color.primary.opacity(0.1)))
                .contentShape(Circle())
        }
        .buttonStyle(.plain)
        .padding(.horizontal, Theme.Space.s)
    }

    /// The next (1) or the one before (-1): false at either end.
    @discardableResult private func go(_ step: Int) -> Bool {
        guard let next = Self.step(active, by: step, count: images.count) else { return false }
        selection = next
        return true
    }
}

/// A thumbnail of a gallery's filmstrip: the picture fitted into a square box (never cropped); the active one in the
/// accent's rounded highlight, the others dimmed until the pointer is on them.
struct ChatGalleryThumb: View {
    let core: Core
    let channelId: String
    let image: ChatImage
    let active: Bool
    @State private var picture: NSImage?
    @State private var hovered = false
    @Environment(\.chatImageSamples) private var samples

    var body: some View {
        let box = ChatImageLayout.thumb
        ZStack {
            if let picture {
                Image(nsImage: picture).resizable().interpolation(.high).aspectRatio(contentMode: .fit)
                    .clipShape(RoundedRectangle(cornerRadius: 3, style: .continuous))
            } else {
                RoundedRectangle(cornerRadius: 3, style: .continuous).fill(Color.secondary.opacity(0.15))
            }
        }
        .padding(ChatImageLayout.thumbInset)
        .frame(width: box, height: box)
        .background(RoundedRectangle(cornerRadius: Theme.Radius.small + 1, style: .continuous).fill(active ? Color.accentColor.opacity(0.22) : .clear))
        .overlay(RoundedRectangle(cornerRadius: Theme.Radius.small + 1, style: .continuous).strokeBorder(active ? Color.accentColor : .clear, lineWidth: 2))
        .opacity(active || hovered ? 1 : 0.6)
        .contentShape(Rectangle())
        .onHover { hovered = $0 }
        .help(image.caption)
        .accessibilityLabel(image.caption)
        .accessibilityAddTraits(active ? [.isButton, .isSelected] : .isButton)
        .task(id: image.hash) { picture = await ChatImageCache.shared.load(core: core, channelId: channelId, image: image, sample: samples[image.hash]).picture }
    }
}

/// One image: the picture when it is here, else its preview with what the core says about it, at its size for
/// `available` points (`ChatImageLayout.fit`). A click opens it in Quick Look; its menu saves, copies and opens it.
struct ChatImageTile: View {
    let core: Core
    let channelId: String
    let image: ChatImage
    /// The snapshot renderer's fixture (`chatImageSamples`): its pictures are drawn instead of asking the core.
    let sample: ChatImageSample?
    let available: CGFloat
    @State private var picture: NSImage?
    @State private var preview: NSImage?
    @State private var quickLook: URL?
    @Environment(\.chatImageSamples) private var samples

    init(core: Core, channelId: String, image: ChatImage, sample: ChatImageSample? = nil, available: CGFloat = ChatImageLayout.fallbackWidth) {
        self.core = core
        self.channelId = channelId
        self.image = image
        self.sample = sample
        self.available = available
    }

    private var texts: ChatImageTexts { ChatImageWords.texts(core) }

    var body: some View {
        let size = ChatImageLayout.fit(width: image.width, height: image.height, available: available)
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
        .clipShape(RoundedRectangle(cornerRadius: Theme.Radius.large - 2, style: .continuous))
        .contentShape(Rectangle())
        .onTapGesture { if image.state == .available { openQuickLook() } }
        .contextMenu { menu }
        // Its name, size and dimensions are in the tooltip: the picture stands alone.
        .help(image.caption)
        .accessibilityLabel(image.caption)
        .task(id: "\(image.hash)-\(String(describing: image.state))") { await load() }
        .quickLookPreview($quickLook)
    }

    /// What shows over the preview while the picture is not here.
    private var placeholder: some View {
        VStack(spacing: Theme.Space.s - 2) {
            if image.state == .fetching { ProgressView().controlSize(.small) }
            Text(image.status).font(Theme.caption).multilineTextAlignment(.center)
            if image.state == .notDownloaded || image.state == .failed {
                Button(texts.download) { fetch() }.controlSize(.small)
            }
        }
        .padding(Theme.Space.s + 2)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: Theme.Radius.medium, style: .continuous))
        .padding(Theme.Space.s)
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
        let got = await ChatImageCache.shared.load(core: core, channelId: channelId, image: image, sample: sample ?? samples[image.hash])
        if let p = got.picture { picture = p }
        if let p = got.preview { preview = p }
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
