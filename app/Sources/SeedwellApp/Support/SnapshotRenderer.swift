import AppKit
import SwiftUI

/// Debug-only visual QA: renders the real windows (real AppKit controls, toolbar and sidebar)
/// to PNG at 2x. Launch with `--render-snapshots <dir>`. Not used in normal operation.
@MainActor struct SnapshotRenderer {
    let core: Core
    let commands: Commands
    let settings: SettingsModel
    let dir: String

    func renderAll() async {
        NSApp.setActivationPolicy(.regular)  // so the offscreen windows are key and drawn as active
        try? FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        let mainSize = NSSize(width: 1180, height: 820)

        await render("downloads-dark", size: mainSize) { MainWindowView(model: model(.downloads, select: nil), commands: commands, settings: settings, nav: SettingsNavigation()) }
        await render("downloads-info-dark", size: mainSize) {
            let m = model(.downloads, select: "sprite")
            return MainWindowView(model: m, commands: commands, settings: settings, nav: SettingsNavigation()).overlay(alignment: .topTrailing) { infoOverlay(m) }
        }
        await render("downloads-light", size: mainSize, dark: false) { MainWindowView(model: model(.downloads, select: nil), commands: commands, settings: settings, nav: SettingsNavigation()) }
        await render("sharing-dark", size: NSSize(width: 1180, height: 1100)) { MainWindowView(model: model(.sharing, select: nil), commands: commands, settings: settings, nav: SettingsNavigation()) }
        await render("sharing-light", size: NSSize(width: 1180, height: 1100), dark: false) { MainWindowView(model: model(.sharing, select: nil), commands: commands, settings: settings, nav: SettingsNavigation()) }
        await render("tracker-filter-dark", size: NSSize(width: 1180, height: 1100)) { MainWindowView(model: model(.tracker("tracker-z.cc"), select: nil), commands: commands, settings: settings, nav: SettingsNavigation()) }
        await render("downloads-filter-dark", size: mainSize) { MainWindowView(model: model(.tracker("tracker-y.net"), select: nil), commands: commands, settings: settings, nav: SettingsNavigation()) }

        for tab in SettingsTab.allCases {
            let h: CGFloat = tab == .folders ? 1180 : tab == .transfers ? 860 : tab == .rules ? 860 : 700
            await render("settings-\(tab.rawValue)-dark", size: NSSize(width: 860, height: h), settings: true) {
                SettingsView(model: settings, nav: nav(tab))
            }
        }
        await render("settings-transfers-light", size: NSSize(width: 860, height: 860), dark: false, settings: true) {
            SettingsView(model: settings, nav: nav(.transfers))
        }
        await render("chooser-dark", size: NSSize(width: 640, height: 640), settings: true) { ChooserView(model: ChooserModel.sample()) }
        await render("chooser-light", size: NSSize(width: 640, height: 640), dark: false, settings: true) { ChooserView(model: ChooserModel.sample()) }
    }

    private func nav(_ tab: SettingsTab) -> SettingsNavigation {
        let n = SettingsNavigation()
        n.tab = tab
        return n
    }

    private func model(_ s: LibraryModel.Section, select: String?) -> LibraryModel {
        let m = LibraryModel(core: core, section: s)
        if select == "sprite" { m.selection = m.snapshot.downloading.dropFirst().first.map { [$0.id] } ?? [] }
        return m
    }

    /// Stands in for the real popover (a separate window that cannot be captured offscreen).
    @ViewBuilder private func infoOverlay(_ m: LibraryModel) -> some View {
        if let row = m.snapshot.downloading.dropFirst().first {
            InfoCard(core: core, row: row, commands: commands)
                .background(RoundedRectangle(cornerRadius: 14).fill(.regularMaterial))
                .overlay(RoundedRectangle(cornerRadius: 14).stroke(.primary.opacity(0.12), lineWidth: 0.5))
                .shadow(color: .black.opacity(0.35), radius: 24, y: 8)
                .padding(.top, 205).padding(.trailing, 40)
        }
    }

    private func render<V: View>(_ name: String, size: NSSize, dark: Bool = true, settings: Bool = false,
                                 @ViewBuilder _ content: () -> V) async {
        let w = FreeWindow(contentViewController: NSHostingController(rootView: content()))
        w.styleMask = settings ? [.titled, .closable, .fullSizeContentView] : [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView]
        w.appearance = NSAppearance(named: dark ? .darkAqua : .aqua)
        w.titlebarAppearsTransparent = true
        if settings { w.titleVisibility = .hidden }
        if !settings { w.titleVisibility = .hidden; w.toolbarStyle = .unified }
        w.setContentSize(size)
        w.setFrameOrigin(NSPoint(x: 0, y: 0))
        w.alphaValue = 0
        w.makeKeyAndOrderFront(nil)
        NSApp.activate()
        try? await Task.sleep(for: .seconds(1.0))
        w.makeFirstResponder(nil)
        try? await Task.sleep(for: .seconds(0.3))
        defer { w.contentViewController = nil; w.close() }
        guard let frame = w.contentView?.superview else { return }
        // 2x regardless of the display: a retina-sized rep whose point size is the view's.
        let b = frame.bounds
        guard let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(b.width * 2), pixelsHigh: Int(b.height * 2),
                                         bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                                         colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0) else { return }
        rep.size = b.size
        frame.cacheDisplay(in: b, to: rep)
        guard let png = rep.representation(using: .png, properties: [:]) else { return }
        try? png.write(to: URL(fileURLWithPath: dir).appendingPathComponent("\(name).png"))
        print("wrote \(name).png (\(rep.pixelsWide)x\(rep.pixelsHigh))")
    }
}

/// A window that may be larger than the screen, so snapshots are not clipped on small displays.
private final class FreeWindow: NSWindow {
    override func constrainFrameRect(_ frameRect: NSRect, to screen: NSScreen?) -> NSRect { frameRect }
}
