import AppKit
import FinderSync
import SeedwellFinderShared
import SwiftUI

/// Seedwell in Finder (SPEC 7G.5): the sync status badges (the Finder extension) and Seedwell in the system Share
/// menu (the Share extension). macOS lets only the user switch extensions on, in System Settings; this window says
/// what each gives, opens the right list, and notices by itself when a switch is on.
@MainActor @Observable final class FinderSetupModel {
    private(set) var badges: Bool
    private(set) var shareMenu: Bool
    @ObservationIgnored private var timer: Timer?
    @ObservationIgnored private let check: () -> (badges: Bool, shareMenu: Bool)

    init(check: @escaping () -> (badges: Bool, shareMenu: Bool) = { (FinderStatusWriter.extensionEnabled, ShareMenu.enabled) }) {
        self.check = check
        (badges, shareMenu) = check()
    }

    var allOn: Bool { badges && shareMenu }

    /// Checks once a second while the window is open (System Settings has no callback).
    func watch() {
        timer?.invalidate()
        timer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.refresh() }
        }
    }

    func stop() {
        timer?.invalidate()
        timer = nil
    }

    func refresh() {
        let now = check()
        if now.badges != badges { badges = now.badges }
        if now.shareMenu != shareMenu { shareMenu = now.shareMenu }
    }
}

/// Seedwell's entry in the system Share menu (the Share extension).
@MainActor enum ShareMenu {
    /// Seedwell is in the Share menu: the system offers its service for a folder. A switched-off extension is left
    /// out of that list, which is the only way an app can read the setting (hence the deprecated call).
    static var enabled: Bool {
        let folder = URL(fileURLWithPath: NSTemporaryDirectory(), isDirectory: true)
        return NSSharingService.sharingServices(forItems: [folder]).contains { $0.title == AppInfo.name }
    }

    /// System Settings at the Share menu's list of extensions.
    static func openSettings() {
        let pane = "x-apple.systempreferences:com.apple.ExtensionsPreferences?extensionPointIdentifier=com.apple.share-services"
        if let url = URL(string: pane) { NSWorkspace.shared.open(url) }
    }
}

/// Where the rest of the app opens the setup window from (Settings › Folder Sync). Wired by the app delegate.
@MainActor enum FinderSetup {
    static var show: () -> Void = {}
}

struct FinderSetupView: View {
    let model: FinderSetupModel
    var cancel: () -> Void = {}

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 3) {
                ModalCaption(text: "Folder Sync")
                Text(model.allOn ? "Seedwell Is in Finder" : "Add Seedwell to Finder").font(.system(size: 17, weight: .semibold))
            }
            Text(model.allOn
                 ? "Synced folders and their files carry a badge, and Share ▸ Seedwell shares any folder."
                 : "Two switches in System Settings, each optional. Click Open Settings, then turn on Seedwell in the list. This window notices by itself.")
                .font(.system(size: 13))
                .fixedSize(horizontal: false, vertical: true)
                .padding(.top, 12)
            VStack(alignment: .leading, spacing: 14) {
                feature(on: model.badges, icon: Image(nsImage: FinderBadge.synced.image), title: "Sync status badges",
                        note: "Ticks and arrows on synced folders and their files", open: FIFinderSyncController.showExtensionManagementInterface)
                    .accessibilityIdentifier("finder-setup-badges")
                if !model.badges { legend.padding(.leading, 34) }
                feature(on: model.shareMenu, icon: Image(systemName: "square.and.arrow.up"), title: "Seedwell in the Share menu",
                        note: "Right-click any folder, Share ▸ Seedwell", open: ShareMenu.openSettings)
                    .accessibilityIdentifier("finder-setup-share")
            }
            .padding(.top, 18)
            HStack {
                Spacer()
                Button("Done", action: cancel).buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction)
                    .help(model.allOn ? "Close" : "Close; Settings › Folder Sync has these switches too")
            }
            .padding(.top, 22)
        }
        .padding(22)
        .frame(width: 460)
        .background(.background)
        .onAppear { model.watch() }
        .onDisappear { model.stop() }
    }

    /// A feature: what it is, and On or the way to switch it on.
    private func feature(on: Bool, icon: Image, title: String, note: String, open: @escaping () -> Void) -> some View {
        HStack(spacing: 12) {
            icon.resizable().scaledToFit().frame(width: 22, height: 22).foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 1) {
                Text(title).font(.system(size: 13))
                Text(note).font(.system(size: 12)).foregroundStyle(.secondary)
            }
            Spacer()
            if on {
                Label("On", systemImage: "checkmark.circle.fill").font(.system(size: 12.5)).foregroundStyle(.secondary)
            } else {
                Button("Open Settings", action: open).help("Open the list in System Settings where Seedwell is switched on")
            }
        }
    }

    private var legend: some View {
        Grid(alignment: .leading, horizontalSpacing: 18, verticalSpacing: 6) {
            GridRow {
                badge(.synced, "Up to date")
                badge(.syncing, "Syncing")
            }
            GridRow {
                badge(.offline, "Paused or no devices online")
                badge(.conflict, "A conflict or a problem")
            }
        }
        .font(.system(size: 11.5))
        .foregroundStyle(.secondary)
    }

    private func badge(_ b: FinderBadge, _ text: String) -> some View {
        HStack(spacing: 6) {
            Image(nsImage: b.image).resizable().frame(width: 14, height: 14)
            Text(text)
        }
    }
}
