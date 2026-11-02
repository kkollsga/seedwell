import AppKit
import SwiftUI

/// Settings › Devices (SPEC §7G.1 "Your devices", §7G.5): this Mac's own devices, pairing with a link that the device
/// which made it confirms, removing one for good, and leaving or starting over. Every word, state and choice is the
/// core's (trs-ffi `devices_text.rs`); this draws them and calls back. Only the user's own clicks reach these calls:
/// agents can neither pair, confirm, remove nor leave.
@MainActor @Observable final class DevicesModel {
    /// What is open over the tab.
    enum Sheet: Equatable {
        /// Pair a Device…: the link, then (once a device asks) the confirmation.
        case pair(PairLinkView)
        /// Pair with a Link….
        case paste
        /// Remove… of a device.
        case remove(RemoveDeviceView)
        /// Leave Your Devices… or Start Over….
        case leave
    }

    @ObservationIgnored let core: Core
    private(set) var view: DevicesView
    var sheet: Sheet?
    /// Pair with a Link…'s field, and why it is not a link this Mac can use.
    private(set) var pasted = ""
    private(set) var pasteProblem: String?
    /// The removal's ticked places (ids).
    var ticked: Set<String> = []
    var error: String?
    var busy = false
    /// Now, for the link's countdown (Unix seconds).
    private(set) var now: UInt64 = UInt64(Date().timeIntervalSince1970)

    init(core: Core) {
        self.core = core
        view = core.devicesView()
    }

    /// While the tab is open: the list, requests and the countdown, every second.
    func watch() async {
        while !Task.isCancelled {
            reload()
            try? await Task.sleep(for: .seconds(1))
        }
    }

    func reload() {
        apply(core.devicesView(), now: UInt64(Date().timeIntervalSince1970))
    }

    /// A new view from the core: a removal whose device is gone closes.
    func apply(_ v: DevicesView, now: UInt64) {
        view = v
        self.now = now
        if case .remove(let r) = sheet, !v.rows.contains(where: { $0.id == r.deviceId }) { sheet = nil }
    }

    /// The device asking to pair while Pair a Device… is open: the sheet shows its confirmation instead of the link.
    var request: PairRequestRow? {
        guard case .pair = sheet else { return nil }
        return view.requests.first
    }

    /// A request that arrived while no sheet is open (the user closed Pair a Device… too early): shown in the tab.
    var waiting: [PairRequestRow] {
        if case .pair = sheet { return [] }
        return view.requests
    }

    func countdown(_ link: PairLinkView) -> String { devicesCountdown(expiresS: link.expiresS, nowS: now) }

    func pair() {
        error = nil
        if let l = core.devicesCurrentLink() { sheet = .pair(l); return }
        let core = self.core
        Task {
            switch await catching({ try await core.devicesPairLink() }) {
            case .success(let l): sheet = .pair(l)
            case .failure(let error): self.error = SyncText.message(error)
            }
        }
    }

    func copy(_ link: PairLinkView) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(link.link, forType: .string)
    }

    func confirm(_ r: PairRequestRow) { act { try await $0.devicesConfirm(deviceId: r.id) } }
    func refuse(_ r: PairRequestRow) { act { try await $0.devicesRefuse(deviceId: r.id) } }

    func showPaste() {
        error = nil
        pasted = ""
        pasteProblem = nil
        sheet = .paste
    }

    func setPasted(_ text: String) {
        guard text != pasted else { return }
        pasted = text
        pasteProblem = core.devicesLinkProblem(text: text)
        error = nil
    }

    var canJoin: Bool { !pasted.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && pasteProblem == nil && !busy }

    func join() {
        guard canJoin else { return }
        let link = pasted.trimmingCharacters(in: .whitespacesAndNewlines)
        act { _ = try await $0.devicesJoin(link: link) }
    }

    func askRemove(_ row: DeviceRow) {
        error = nil
        let core = self.core
        Task {
            switch await catching({ try await core.devicesRemoveView(deviceId: row.id) }) {
            case .success(let v):
                ticked = Set(v.places.filter(\.ticked).map(\.id))
                sheet = .remove(v)
            case .failure(let error): self.error = SyncText.message(error)
            }
        }
    }

    func toggle(_ place: RemovePlaceRow) {
        if ticked.contains(place.id) { ticked.remove(place.id) } else { ticked.insert(place.id) }
    }

    func remove(_ v: RemoveDeviceView) {
        let places = v.places.map(\.id).filter { ticked.contains($0) }
        act { try await $0.devicesRemove(deviceId: v.deviceId, places: places) }
    }

    func askLeave() {
        error = nil
        sheet = .leave
    }

    /// Leave, or (once removed) Start Over.
    func leave() {
        let startOver = view.kind == .removed
        act { core in if startOver { _ = try await core.devicesStartOver() } else { try await core.devicesLeave() } }
    }

    /// Runs a call off the main thread; done, the sheet closes and the view is read again.
    private func act(_ work: @escaping @Sendable (Core) async throws -> Void) {
        guard !busy else { return }
        busy = true
        error = nil
        let core = core
        Task {
            let r: Result<Void, Error> = await catching { try await work(core) }
            busy = false
            switch r {
            case .success: sheet = nil
            case .failure(let e): error = SyncText.message(e)
            }
            reload()
        }
    }
}

struct DevicesTab: View {
    @State private var model: DevicesModel
    /// Brings a link to pair with (`seedwell://devices/…` opened the app).
    var nav: SettingsNavigation

    init(core: Core, nav: SettingsNavigation = SettingsNavigation()) {
        _model = State(initialValue: DevicesModel(core: core))
        self.nav = nav
    }

    /// Pair with a Link…, filled in with the link that opened the app.
    private func takeLink() {
        guard let link = nav.pairLink else { return }
        nav.pairLink = nil
        model.showPaste()
        model.setPasted(link)
    }

    var body: some View {
        SettingsPage {
            Text(model.view.intro).font(.system(size: 13)).foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            if !model.view.rows.isEmpty {
                SettingsBox(title: "Your Devices") {
                    ForEach(model.view.rows, id: \.id) { row in DeviceRowView(row: row) { model.askRemove(row) } }
                }
            }
            ForEach(model.waiting, id: \.id) { r in PairRequestView(request: r, busy: model.busy, pair: { model.confirm(r) }, refuse: { model.refuse(r) }) }
            if let line = model.view.joiningLine {
                VStack(alignment: .leading, spacing: 4) {
                    Label(line, systemImage: "hourglass").font(.system(size: 13))
                    if let p = model.view.joiningProblem { ModalError(text: p) }
                    Text(model.view.myIdLine).font(.system(size: 12, design: .monospaced)).foregroundStyle(.secondary).textSelection(.enabled)
                }
            }
            HStack(spacing: 8) {
                Button(model.view.pairTitle) { model.pair() }.disabled(!model.view.pairEnabled)
                    .help("Make a link that lets another of your devices join this Mac's devices")
                    .accessibilityIdentifier("devices-pair")
                Button(model.view.linkTitle) { model.showPaste() }.disabled(!model.view.linkEnabled)
                    .help("Paste a link made on another of your devices")
                    .accessibilityIdentifier("devices-pair-link")
                Spacer(minLength: 0)
                if let leave = model.view.leaveTitle {
                    Button(leave) { model.askLeave() }.help(model.view.leaveQuestion).accessibilityIdentifier("devices-leave")
                }
            }
            if let e = model.error, model.sheet == nil { ModalError(text: e) }
            Text(model.view.note).font(.system(size: 11.5)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
        }
        .task { await model.watch() }
        .onAppear { takeLink() }
        .onChange(of: nav.pairLink) { takeLink() }
        .sheet(isPresented: Binding(get: { model.sheet != nil }, set: { if !$0 { model.sheet = nil } })) { sheet }
    }

    @ViewBuilder private var sheet: some View {
        switch model.sheet {
        case .pair(let link): PairSheet(model: model, link: link)
        case .paste: PasteSheet(model: model)
        case .remove(let v): RemoveDeviceSheet(model: model, view: v)
        case .leave: LeaveSheet(model: model)
        case nil: EmptyView()
        }
    }
}

private struct DeviceRowView: View {
    let row: DeviceRow
    let remove: () -> Void

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: row.isMe ? "laptopcomputer" : "desktopcomputer").foregroundStyle(.secondary).frame(width: 22)
            VStack(alignment: .leading, spacing: 2) {
                Text(row.title)
                Text(row.detail).font(.system(size: 11.5)).foregroundStyle(.secondary)
                if let w = row.warning {
                    Label(w, systemImage: "exclamationmark.triangle.fill").font(.system(size: 11.5)).foregroundStyle(.orange)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            Spacer(minLength: 12)
            if row.removable {
                Button("Remove…", action: remove).help("Take this device out of your devices for good")
            }
        }
        .font(.system(size: 13))
        .padding(.horizontal, 14).padding(.vertical, 9)
    }
}

/// "“Studio PC” wants to join your devices." with the id to compare, Refuse and Pair.
private struct PairRequestView: View {
    let request: PairRequestRow
    let busy: Bool
    let pair: () -> Void
    let refuse: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(request.question).font(.system(size: 14, weight: .semibold))
            Text(request.idLine).font(.system(size: 15, design: .monospaced)).textSelection(.enabled)
                .accessibilityIdentifier("devices-request-id")
            Text(request.hint).font(.system(size: 12)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            HStack(spacing: 8) {
                if busy { ProgressView().controlSize(.small) }
                Spacer(minLength: 0)
                Button(request.refuseTitle, action: refuse).help("Do not let this device join; the link admits nobody any more")
                Button(request.pairTitle, action: pair).buttonStyle(.borderedProminent).keyboardShortcut(.defaultAction)
                    .disabled(busy).help("Let this device join your devices")
            }
        }
    }
}

private struct PairSheet: View {
    @Bindable var model: DevicesModel
    let link: PairLinkView

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            VStack(alignment: .leading, spacing: 3) {
                ModalCaption(text: "Devices")
                Text(link.title).font(.system(size: 17, weight: .semibold))
            }
            if let r = model.request {
                PairRequestView(request: r, busy: model.busy, pair: { model.confirm(r) }, refuse: { model.refuse(r) })
            } else {
                Text(link.text).font(.system(size: 12.5)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
                HStack(spacing: 8) {
                    Text(link.link).font(.system(size: 11.5, design: .monospaced)).lineLimit(2).truncationMode(.middle)
                        .textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
                    Button(link.copyTitle) { model.copy(link) }.help("Copy the link, to paste it on your other device")
                        .accessibilityIdentifier("devices-copy-link")
                }
                Text(model.countdown(link)).font(.system(size: 12)).foregroundStyle(.secondary).monospacedDigit()
                HStack {
                    Spacer()
                    Button("Done") { model.sheet = nil }.keyboardShortcut(.cancelAction).help("Close; the link keeps working until it expires")
                }
            }
            if let e = model.error { ModalError(text: e) }
        }
        .padding(22)
        .frame(width: 480)
    }
}

private struct PasteSheet: View {
    @Bindable var model: DevicesModel

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            VStack(alignment: .leading, spacing: 3) {
                ModalCaption(text: "Devices")
                Text("Pair with a Link").font(.system(size: 17, weight: .semibold))
            }
            TextField("seedwell://devices/…", text: Binding(get: { model.pasted }, set: { model.setPasted($0) }))
                .textFieldStyle(.roundedBorder).accessibilityIdentifier("devices-link-field")
            if let p = model.pasteProblem {
                Label(p, systemImage: "xmark.circle").font(.system(size: 12)).foregroundStyle(.orange)
            }
            Text(model.view.myIdLine).font(.system(size: 12, design: .monospaced)).foregroundStyle(.secondary).textSelection(.enabled)
            if let e = model.error { ModalError(text: e) }
            ModalButtons(primary: "Pair", enabled: model.canJoin, busy: model.busy, primaryHelp: "Ask the other device to let this Mac join",
                         cancel: { model.sheet = nil }, confirm: { model.join() })
        }
        .padding(22)
        .frame(width: 480)
    }
}

private struct RemoveDeviceSheet: View {
    @Bindable var model: DevicesModel
    let view: RemoveDeviceView

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(view.title).font(.system(size: 15, weight: .semibold)).fixedSize(horizontal: false, vertical: true)
            Text(view.text).font(.system(size: 12.5)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            if !view.places.isEmpty {
                Text(view.placesTitle).font(.system(size: 13))
                ForEach(view.places, id: \.id) { p in
                    Toggle(p.title, isOn: Binding(get: { model.ticked.contains(p.id) }, set: { _ in model.toggle(p) }))
                        .toggleStyle(.checkbox)
                }
            }
            if let a = view.askOwner { Text(a).font(.system(size: 12)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true) }
            if let p = view.paired { Text(p).font(.system(size: 12)).foregroundStyle(.orange).fixedSize(horizontal: false, vertical: true) }
            if let e = model.error { ModalError(text: e) }
            ModalButtons(primary: view.removeTitle, busy: model.busy, primaryHelp: "Remove this device from your devices for good",
                         cancel: { model.sheet = nil }, confirm: { model.remove(view) })
        }
        .padding(22)
        .frame(width: 460)
    }
}

private struct LeaveSheet: View {
    @Bindable var model: DevicesModel

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(model.view.leaveQuestion).font(.system(size: 13)).fixedSize(horizontal: false, vertical: true)
            if let e = model.error { ModalError(text: e) }
            ModalButtons(primary: model.view.leaveAction, busy: model.busy,
                         primaryHelp: model.view.kind == .removed ? "Make new keys for your devices" : "Stop being one of your devices",
                         cancel: { model.sheet = nil }, confirm: { model.leave() })
        }
        .padding(22)
        .frame(width: 420)
    }
}
