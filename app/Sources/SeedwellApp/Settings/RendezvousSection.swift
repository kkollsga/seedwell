import AppKit
import SwiftUI

/// What the rendezvous section asks the core to do.
enum RendezvousAction {
    case setUp(token: String)
    case use(address: String)
    case test
    case turnOff
}

/// Settings › Folder Sync › "Find your devices over the Internet (Cloudflare)" (SPEC 7G.5): the rendezvous on the
/// user's own Cloudflare account. Every text and the state come from the core (`Core.syncRendezvous`); this draws
/// them and hands the user's input back. The API token goes straight to the core and is cleared from the field.
struct RendezvousSection: View {
    let view: RendezvousView
    /// Runs an action; returns why it was refused at once (it then runs in the background).
    let act: (RendezvousAction) -> String?
    @State private var token = ""
    @State private var address = ""
    @State private var updating = false
    @State private var refused: String?

    var body: some View {
        SettingsBox(title: view.title) {
            VStack(alignment: .leading, spacing: 12) {
                Text(view.explanation).font(.system(size: 12)).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                switch view.state {
                case .off:
                    guide
                    existing
                case .working:
                    HStack(spacing: 8) {
                        ProgressView().controlSize(.small)
                        Text(view.progress).font(.system(size: 12.5))
                    }
                    .accessibilityIdentifier("rendezvous-progress")
                case .on:
                    inUse
                    if updating { guide }
                }
                if let line = refused ?? (view.result.isEmpty ? nil : view.result) {
                    let problem = refused != nil || view.resultIsProblem
                    Label(line, systemImage: problem ? "exclamationmark.triangle.fill" : "checkmark.circle.fill")
                        .font(.system(size: 12)).foregroundStyle(problem ? .orange : .secondary)
                        .fixedSize(horizontal: false, vertical: true)
                        .accessibilityIdentifier("rendezvous-result")
                }
            }
            .padding(14)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .onChange(of: view.state) { if view.state != .on { updating = false } }
    }

    /// The numbered steps to the token, the token field and Set Up.
    private var guide: some View {
        VStack(alignment: .leading, spacing: 8) {
            ForEach(Array(view.steps.enumerated()), id: \.offset) { i, step in
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    Text("\(i + 1).").monospacedDigit().foregroundStyle(.secondary).frame(width: 16, alignment: .trailing)
                    Text(step).fixedSize(horizontal: false, vertical: true)
                }
                .font(.system(size: 12.5))
            }
            HStack(spacing: 10) {
                Button(view.openLabel) {
                    if let url = URL(string: view.tokenPageUrl) { NSWorkspace.shared.open(url) }
                }
                .accessibilityIdentifier("rendezvous-open-token-page")
                SecureField(view.tokenPlaceholder, text: $token)
                    .textFieldStyle(.roundedBorder).frame(minWidth: 200)
                    .onSubmit(setUp)
                    .accessibilityIdentifier("rendezvous-token")
                Button(view.setUpLabel, action: setUp)
                    .keyboardShortcut(.defaultAction)
                    .disabled(token.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                    .accessibilityIdentifier("rendezvous-set-up")
            }
        }
    }

    /// Another device's address (or one deployed with wrangler).
    private var existing: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(view.existingHint).font(.system(size: 12)).foregroundStyle(.secondary)
            HStack(spacing: 10) {
                TextField(view.urlPlaceholder, text: $address)
                    .textFieldStyle(.roundedBorder).frame(minWidth: 280)
                    .onSubmit(useAddress)
                    .accessibilityIdentifier("rendezvous-address")
                Button(view.useLabel, action: useAddress)
                    .disabled(address.trimmingCharacters(in: .whitespaces).isEmpty)
                    .accessibilityIdentifier("rendezvous-use")
            }
        }
    }

    /// The address with Copy, and Test, Update and Turn Off.
    private var inUse: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 10) {
                CodePill(text: view.url).textSelection(.enabled)
                Button(view.copyLabel) {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(view.url, forType: .string)
                }
                .accessibilityIdentifier("rendezvous-copy")
            }
            if !view.status.isEmpty {
                Text(view.status).font(.system(size: 12)).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    .accessibilityIdentifier("rendezvous-status")
            }
            HStack(spacing: 10) {
                Button(view.updateLabel) { updating.toggle() }.accessibilityIdentifier("rendezvous-update")
                Spacer()
                Button(view.turnOffLabel) { run(.turnOff) }.accessibilityIdentifier("rendezvous-turn-off")
                Button(view.testLabel) { run(.test) }.accessibilityIdentifier("rendezvous-test")
            }
        }
    }

    private func setUp() {
        let t = token.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !t.isEmpty else { return }
        token = ""
        run(.setUp(token: t))
    }

    private func useAddress() {
        let a = address.trimmingCharacters(in: .whitespaces)
        guard !a.isEmpty else { return }
        if run(.use(address: a)) { address = "" }
    }

    @discardableResult private func run(_ a: RendezvousAction) -> Bool {
        refused = act(a)
        return refused == nil
    }
}
