import AppKit
import UniformTypeIdentifiers

/// Seedwell in the system Share menu (SPEC 7G.5): sharing a folder or file hands its path to the app
/// (`seedwell://share?path=…`), which opens the folder's Share dialog, or Add Folder for a folder not synced yet. The
/// extension shows no UI of its own and never reads Seedwell's data: the app decides.
@objc(SeedwellShareViewController)
final class ShareViewController: NSViewController {
    override func loadView() {
        view = NSView(frame: NSRect(x: 0, y: 0, width: 1, height: 1))
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        let providers = (extensionContext?.inputItems as? [NSExtensionItem] ?? []).flatMap { $0.attachments ?? [] }
        guard let p = providers.first(where: { $0.hasItemConformingToTypeIdentifier(UTType.fileURL.identifier) }) else { return finish() }
        _ = p.loadObject(ofClass: URL.self) { [weak self] url, _ in
            Task { @MainActor in
                if let url, url.isFileURL { Self.open(url) }
                self?.finish()
            }
        }
    }

    private static func open(_ file: URL) {
        var c = URLComponents()
        c.scheme = "seedwell"
        c.host = "share"
        c.queryItems = [URLQueryItem(name: "path", value: file.path)]
        if let url = c.url { NSWorkspace.shared.open(url) }
    }

    private func finish() {
        extensionContext?.completeRequest(returningItems: nil, completionHandler: nil)
    }
}
