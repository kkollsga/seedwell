import AppKit
import FinderSync
import SeedwellFinderShared
import SwiftUI

/// Turning on the Finder extension (SPEC 7G.5). macOS lets only the user do it, in System Settings; this window
/// says what it gives, opens the right list, and notices by itself when the switch is on.
@MainActor @Observable final class FinderSetupModel {
    private(set) var enabled: Bool
    @ObservationIgnored private var timer: Timer?
    @ObservationIgnored private let isEnabled: () -> Bool

    init(isEnabled: @escaping () -> Bool = { FinderStatusWriter.extensionEnabled }) {
        self.isEnabled = isEnabled
        enabled = isEnabled()
    }

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
        let now = isEnabled()
        if now != enabled { enabled = now }
    }

    func openSettings() {
        FIFinderSyncController.showExtensionManagementInterface()
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
                Text(model.enabled ? "Finder Shows Sync Status" : "Show Sync Status in Finder").font(.system(size: 17, weight: .semibold))
            }
            Text(model.enabled
                 ? "Your synced folders and their files now carry a badge in Finder. Right-click one for Show in Seedwell."
                 : "Seedwell can mark your synced folders and their files in Finder, so you see at a glance what is up to date.")
                .font(.system(size: 13))
                .fixedSize(horizontal: false, vertical: true)
                .padding(.top, 12)
            legend.padding(.top, 16)
            if !model.enabled {
                VStack(alignment: .leading, spacing: 6) {
                    step(1, "Click Open System Settings.")
                    step(2, "Turn on Seedwell in the list of extensions.")
                    step(3, "Come back here. This window notices by itself.")
                }
                .padding(.top, 18)
                .accessibilityIdentifier("finder-setup-steps")
            }
            if model.enabled {
                HStack {
                    Spacer()
                    Button("Done", action: cancel).buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction)
                }
                .padding(.top, 22)
            } else {
                ModalButtons(primary: "Open System Settings", primaryHelp: "Open the list of extensions in System Settings",
                             cancel: cancel, confirm: model.openSettings)
                    .padding(.top, 22)
            }
        }
        .padding(22)
        .frame(width: 440)
        .background(.background)
        .onAppear { model.watch() }
        .onDisappear { model.stop() }
    }

    private var legend: some View {
        Grid(alignment: .leading, horizontalSpacing: 18, verticalSpacing: 8) {
            GridRow {
                badge(.synced, "Up to date")
                badge(.syncing, "Syncing")
            }
            GridRow {
                badge(.offline, "Paused or no devices online")
                badge(.conflict, "A conflict or a problem")
            }
        }
        .font(.system(size: 12))
        .foregroundStyle(.secondary)
    }

    private func badge(_ b: FinderBadge, _ text: String) -> some View {
        HStack(spacing: 7) {
            Image(nsImage: b.image).resizable().frame(width: 16, height: 16)
            Text(text)
        }
    }

    private func step(_ n: Int, _ text: String) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Text("\(n).").monospacedDigit().foregroundStyle(.secondary).frame(width: 16, alignment: .trailing)
            Text(text)
        }
        .font(.system(size: 13))
    }
}
