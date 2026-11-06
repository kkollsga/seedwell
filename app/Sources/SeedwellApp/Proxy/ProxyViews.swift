import AppKit
import SwiftUI

/// The shield as a control (Settings › Network, the window's header): a click does what the model says (opens the
/// window without a key, else on or off), a right-click shows the shield's menu. Its tooltip and accessible name are the
/// core's.
struct ProxyShieldButton: View {
    let center: ProxyCenter
    var size: CGFloat = 22
    /// Where its menu's "Country: …" opens the country list: Settings' under this shield, the window's under its
    /// country button.
    var place: CountryListPlace = .settings
    @State private var hovering = false

    var body: some View {
        let m = center.model
        let enabled = !(center.busy && !m.clickOpensWindow)
        Button { center.click() } label: {
            ProxyShield(state: m.icon, size: size)
                .opacity(center.busy ? 0.55 : 1)
                // The hover: a glow in the shield's own colour, no button chrome (ProxyShieldDrawing.Glow).
                .shadow(color: Color(nsColor: ProxyShieldDrawing.glowColor(m.icon, level: hovering && enabled ? ProxyShieldDrawing.Glow.hovered : 0)),
                        radius: ProxyShieldDrawing.Glow.radius * size / MenuProxyShield.glyph)
                .animation(.easeInOut(duration: ProxyShieldDrawing.Glow.fade), value: hovering)
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
        .disabled(center.busy && !m.clickOpensWindow)
        .help(m.tooltip)
        // The tooltip: the state and, when it waits for the proxy, why (the status line alone drops the reason).
        .accessibilityLabel(m.tooltip)
        .accessibilityIdentifier("proxy-shield")
        .contextMenu { MenuEntriesView(entries: center.menuEntries(at: place)) }
        .popover(isPresented: Binding(get: { place == .settings && center.countryList?.place == .settings },
                                      set: { if !$0, center.countryList?.place == .settings { center.closeCountryList() } }),
                 arrowEdge: .bottom) {
            if let list = center.countryList?.state { CountryListPopover(state: list) }
        }
    }
}

/// The proxy's On switch (Settings › Network, the window's header), shown only with a key: on while the model says it
/// is on, and a flip is the shield's own `proxy_toggle` (it waits, disabled, for the answer).
struct ProxySwitch: View {
    let center: ProxyCenter

    var body: some View {
        let m = center.model
        Toggle(m.menuToggleLabel, isOn: Binding(get: { m.enabled }, set: { on in if on != m.enabled { center.toggle() } }))
            .toggleStyle(.switch)
            .labelsHidden()
            .disabled(center.busy)
            .help(m.menuToggleLabel)
            .accessibilityIdentifier("proxy-switch")
    }
}

/// Settings › Network's torrent proxy (the core's `SettingsBlock.torrentProxy`): the shield, the status line (with the
/// server and its latency while it is on), the key line (never the key), the problem when there is one, and the button
/// that opens the window.
struct ProxySettingsBox: View {
    let title: String
    let center: ProxyCenter

    var body: some View {
        let m = center.model
        SettingsBox(title: title) {
            HStack(alignment: .top, spacing: Theme.Space.m) {
                ProxyShieldButton(center: center, size: 28)
                    .padding(.top, 1)
                VStack(alignment: .leading, spacing: Theme.Space.hair) {
                    Text(m.statusLine)
                        .font(Theme.headline)
                        .accessibilityLabel([m.statusLine, m.problem].compactMap { $0 }.joined(separator: ". "))
                        .accessibilityIdentifier("proxy-status")
                    // Without a key the status line already says so ("No key installed").
                    if m.hasKey {
                        Text(center.keyLine)
                            .font(Theme.secondary).foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    if let problem = m.problem {
                        Label { Text(problem).fixedSize(horizontal: false, vertical: true) } icon: { Image(systemName: "exclamationmark.triangle.fill") }
                            .font(Theme.secondary).foregroundStyle(.red)
                            .padding(.top, Theme.Space.xs)
                            .accessibilityIdentifier("proxy-problem")
                    }
                    if let notice = center.notice {
                        Text(notice)
                            .font(Theme.secondary).foregroundStyle(.orange)
                            .fixedSize(horizontal: false, vertical: true)
                            .accessibilityIdentifier("proxy-notice")
                    }
                }
                Spacer(minLength: Theme.Space.m)
                if m.hasKey { ProxySwitch(center: center).padding(.trailing, Theme.Space.xs) }
                Button(m.menuOpenLabel) { center.openWindow() }
                    .accessibilityIdentifier("proxy-open")
            }
            .padding(.horizontal, 14)
            .padding(.vertical, Theme.Space.m)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .onAppear { center.watch() }
        .onDisappear { center.unwatch() }
    }
}

/// The "SOCKS5 proxy" window (SPEC §7H): what the proxy does, the key line, NordVPN (with its guide) or another SOCKS5
/// proxy, the country, Find a Faster Server, and the progress and result of the call. Remove Key bottom left (it is
/// destructive), Close bottom right.
struct ProxyWindowView: View {
    @Bindable var state: ProxyWindowState
    let center: ProxyCenter
    @FocusState private var focus: Field?

    enum Field { case username, password, host }

    var body: some View {
        let w = state.model
        VStack(alignment: .leading, spacing: 0) {
            header(w)
            Text(w.explanation)
                .font(Theme.secondary).foregroundStyle(.secondary)
                .lineSpacing(Theme.Line.secondary)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.top, Theme.Space.l)
            VStack(alignment: .leading, spacing: Theme.Space.s) {
                note(w.killSwitch, symbol: "hand.raised.fill", tint: .orange)
                note(w.keyLine, symbol: "key.fill", tint: w.hasKey ? .green : .secondary)
                    .accessibilityIdentifier("proxy-key-line")
            }
            .padding(.top, Theme.Space.m)
            Picker("", selection: $state.pane) {
                Text(w.nordvpnTitle).tag(ProxyKind.nordVpn)
                Text(w.customTitle).tag(ProxyKind.custom)
            }
            .pickerStyle(.segmented).labelsHidden()
            .disabled(state.busy)
            .padding(.top, Theme.Space.l + 2)
            card {
                switch state.pane {
                case .nordVpn: nordvpn(w)
                case .custom: custom(w)
                }
            }
            .padding(.top, Theme.Space.m)
            country(w).padding(.top, Theme.Space.l)
            if let notice = center.notice { status((notice, true, false)).padding(.top, Theme.Space.m) }
            else if let line = state.statusLine { status(line).padding(.top, Theme.Space.m) }
            HStack(spacing: Theme.Space.s) {
                if w.hasKey {
                    Button(w.removeKeyLabel, role: .destructive) { state.removeKey() }
                        .disabled(state.busy)
                        .accessibilityIdentifier("proxy-remove-key")
                }
                if state.running == .remove { ProgressView().controlSize(.small) }
                Spacer(minLength: 0)
                Button(w.closeLabel) { center.closeWindow() }
                    .keyboardShortcut(.cancelAction)
                    .accessibilityIdentifier("proxy-close")
            }
            .padding(.top, Theme.Space.xl - 2)
        }
        .font(Theme.body)
        .padding(Theme.Space.xl - 2)
        .frame(width: 520)
        .background(.background)
        .onAppear { if !w.hasKey, state.pane == .nordVpn { focus = .username } }
    }

    // MARK: Parts

    private func header(_ w: ProxyWindowModel) -> some View {
        HStack(alignment: .center, spacing: Theme.Space.m) {
            ProxyShieldButton(center: center, size: 36, place: .window)
            VStack(alignment: .leading, spacing: Theme.Space.hair) {
                Text(w.title).font(Theme.title)
                Text(center.model.statusLine)
                    .font(Theme.secondary).foregroundStyle(.secondary)
                    .lineLimit(2)
                    .accessibilityIdentifier("proxy-window-status")
                // Why torrents are held (failed), unless the result line below already says it.
                if let problem = center.model.problem, problem != state.statusLine?.text {
                    Text(problem)
                        .font(Theme.secondary).foregroundStyle(.red)
                        .fixedSize(horizontal: false, vertical: true)
                        .accessibilityIdentifier("proxy-window-problem")
                }
            }
            Spacer(minLength: Theme.Space.m)
            if center.model.hasKey { ProxySwitch(center: center) }
        }
    }

    /// A line with a small symbol before it (the kill switch, the key).
    private func note(_ text: String, symbol: String, tint: Color) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: Theme.Space.s) {
            Image(systemName: symbol)
                .font(.system(size: Theme.Icon.small, weight: .semibold))
                .foregroundStyle(tint)
                .frame(width: 14)
            Text(text).fixedSize(horizontal: false, vertical: true)
        }
        .font(Theme.secondary)
    }

    /// The pane's quiet card (the settings boxes' look).
    private func card<Content: View>(@ViewBuilder _ content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: Theme.Space.m) { content() }
            .padding(Theme.Space.l)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(RoundedRectangle(cornerRadius: Theme.Radius.large).fill(.primary.opacity(0.045)))
            .overlay(RoundedRectangle(cornerRadius: Theme.Radius.large).stroke(.primary.opacity(0.06), lineWidth: 0.5))
    }

    @ViewBuilder private func nordvpn(_ w: ProxyWindowModel) -> some View {
        Text(w.nordvpnIntro)
            .font(Theme.secondary).foregroundStyle(.secondary)
            .fixedSize(horizontal: false, vertical: true)
        guide(w)
        if !w.nordvpnInstalled.isEmpty, !state.replacing {
            installed(w.nordvpnInstalled, w)
        } else {
            nordvpnForm(w)
        }
    }

    /// The pane of the installed key: a short line and "Replace Key…" (which shows the form again).
    private func installed(_ line: String, _ w: ProxyWindowModel) -> some View {
        HStack(spacing: Theme.Space.s) {
            Label { Text(line) } icon: { Image(systemName: "checkmark.circle.fill").foregroundStyle(.green) }
                .font(Theme.body)
                .accessibilityIdentifier("proxy-installed")
            Spacer(minLength: Theme.Space.s)
            Button(w.replaceRevealLabel) {
                withAnimation(.snappy(duration: 0.2)) { state.replacing = true }
                focus = state.pane == .nordVpn ? .username : .host
            }
            .disabled(state.busy)
            .accessibilityIdentifier("proxy-replace-reveal")
        }
        .padding(.vertical, Theme.Space.hair)
    }

    /// The pane's own button: Enable NordVPN or Use This Proxy for a first key; with one installed, Cancel and Replace
    /// Key (the Mac's order: the primary at the right).
    @ViewBuilder private func submit(_ w: ProxyWindowModel, first: String, call: ProxyWindowState.Call, enabled: Bool, id: String,
                                     run: @escaping () -> Void) -> some View {
        HStack(spacing: Theme.Space.s) {
            Spacer(minLength: 0)
            if state.running == call { ProgressView().controlSize(.small) }
            if state.replacing {
                Button(w.replaceCancelLabel) { withAnimation(.snappy(duration: 0.2)) { state.cancelReplace() } }
                    .disabled(state.busy)
                    .accessibilityIdentifier("proxy-replace-cancel")
            }
            Button(state.replacing ? w.replaceLabel : first, action: run)
                .buttonStyle(.borderedProminent)
                .disabled(!enabled)
                .accessibilityIdentifier(id)
        }
    }

    @ViewBuilder private func nordvpnForm(_ w: ProxyWindowModel) -> some View {
        VStack(alignment: .leading, spacing: Theme.Space.s) {
            TextField(w.usernamePlaceholder, text: $state.username)
                .textFieldStyle(.roundedBorder)
                .focused($focus, equals: .username)
                .onSubmit { focus = .password }
                .accessibilityIdentifier("proxy-username")
            SecureField(w.passwordPlaceholder, text: $state.password)
                .textFieldStyle(.roundedBorder)
                .focused($focus, equals: .password)
                .onSubmit { if canEnable { state.enable() } }
                .accessibilityIdentifier("proxy-password")
            Label { Text(w.pasteHint).fixedSize(horizontal: false, vertical: true) } icon: { Image(systemName: "doc.on.clipboard") }
                .font(Theme.caption).foregroundStyle(.secondary)
                .accessibilityIdentifier("proxy-paste-hint")
            if state.filledFromCopy {
                Label { Text(w.pastedNote).fixedSize(horizontal: false, vertical: true) } icon: { Image(systemName: "checkmark.circle") }
                    .font(Theme.caption).foregroundStyle(Color.accentColor)
                    .accessibilityIdentifier("proxy-pasted-note")
            }
        }
        .disabled(state.busy)
        submit(w, first: w.enableNordvpnLabel, call: .enable, enabled: canEnable, id: "proxy-enable") { state.enable() }
    }

    private var canEnable: Bool {
        !state.busy && !state.username.trimmingCharacters(in: .whitespaces).isEmpty && !state.password.isEmpty
    }

    /// "How do I get the service credentials?": the steps, shown and hidden inside the window, and the link to Nord
    /// Account.
    @ViewBuilder private func guide(_ w: ProxyWindowModel) -> some View {
        Button {
            withAnimation(.snappy(duration: 0.2)) { state.guideShown.toggle() }
        } label: {
            HStack(spacing: Theme.Space.xs) {
                Image(systemName: "chevron.right")
                    .font(.system(size: Theme.Icon.small - 2, weight: .bold))
                    .rotationEffect(.degrees(state.guideShown ? 90 : 0))
                Text(state.guideShown ? w.guideHideLabel : w.guideShowLabel)
            }
            .font(Theme.secondary.weight(.medium))
            .foregroundStyle(Color.accentColor)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityIdentifier("proxy-guide")
        if state.guideShown {
            VStack(alignment: .leading, spacing: Theme.Space.s) {
                ForEach(Array(w.guideSteps.enumerated()), id: \.offset) { i, step in
                    HStack(alignment: .firstTextBaseline, spacing: Theme.Space.s) {
                        Text("\(i + 1)")
                            .font(Theme.micro).monospacedDigit()
                            .foregroundStyle(Color.accentColor)
                            .frame(width: 18, height: 18)
                            .background(Circle().fill(Color.accentColor.opacity(0.14)))
                            .alignmentGuide(.firstTextBaseline) { $0[.bottom] - 5 }
                        Text(step).font(Theme.secondary).fixedSize(horizontal: false, vertical: true)
                    }
                }
                if let url = URL(string: w.nordAccountUrl) {
                    Link(destination: url) {
                        Label(w.nordAccountLabel, systemImage: "arrow.up.right.square")
                    }
                    .font(Theme.secondary.weight(.medium))
                    .padding(.leading, 26)
                    .accessibilityIdentifier("proxy-nord-account")
                }
            }
            .padding(Theme.Space.m)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(RoundedRectangle(cornerRadius: Theme.Radius.medium).fill(Color(nsColor: .textBackgroundColor).opacity(0.7)))
            .overlay(RoundedRectangle(cornerRadius: Theme.Radius.medium).stroke(.primary.opacity(0.08), lineWidth: 0.5))
            .transition(.opacity.combined(with: .move(edge: .top)))
            .accessibilityIdentifier("proxy-guide-steps")
        }
    }

    @ViewBuilder private func custom(_ w: ProxyWindowModel) -> some View {
        Text(w.customIntro)
            .font(Theme.secondary).foregroundStyle(.secondary)
            .fixedSize(horizontal: false, vertical: true)
        if !w.customInstalled.isEmpty, !state.replacing {
            installed(w.customInstalled, w)
        } else {
            customForm(w)
        }
    }

    @ViewBuilder private func customForm(_ w: ProxyWindowModel) -> some View {
        VStack(alignment: .leading, spacing: Theme.Space.m - 2) {
            HStack(alignment: .bottom, spacing: Theme.Space.s + 2) {
                labelled(w.hostLabel) {
                    TextField("", text: $state.host).textFieldStyle(.roundedBorder)
                        .focused($focus, equals: .host)
                        .accessibilityIdentifier("proxy-host")
                }
                labelled(w.portLabel) {
                    TextField("", text: $state.port).textFieldStyle(.roundedBorder).frame(width: 72)
                        .accessibilityIdentifier("proxy-port")
                }
                .fixedSize()
            }
            HStack(alignment: .bottom, spacing: Theme.Space.s + 2) {
                labelled(w.customUserLabel) {
                    TextField("", text: $state.customUser).textFieldStyle(.roundedBorder)
                        .accessibilityIdentifier("proxy-custom-user")
                }
                labelled(w.customPasswordLabel) {
                    SecureField("", text: $state.customPassword).textFieldStyle(.roundedBorder)
                        .onSubmit { state.useCustom() }
                        .accessibilityIdentifier("proxy-custom-password")
                }
            }
        }
        .disabled(state.busy)
        submit(w, first: w.useCustomLabel, call: .custom, enabled: !state.busy && !state.host.trimmingCharacters(in: .whitespaces).isEmpty,
               id: "proxy-use-custom") { state.useCustom() }
    }

    private func labelled<Content: View>(_ label: String, @ViewBuilder _ field: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: Theme.Space.xs) {
            Text(label).font(Theme.caption).foregroundStyle(.secondary)
            field()
        }
    }

    private func country(_ w: ProxyWindowModel) -> some View {
        VStack(alignment: .leading, spacing: Theme.Space.xs + 2) {
            HStack(spacing: Theme.Space.s + 2) {
                Text(w.countryLabel)
                // A pop-up button: the list it opens is the menu's (6 rows with a scrollbar), not a Picker's menu of
                // every country.
                Button { center.openCountryList(at: .window) } label: {
                    HStack(spacing: Theme.Space.s) {
                        Text(w.countryName)
                        Image(systemName: "chevron.up.chevron.down")
                            .font(.system(size: 9, weight: .bold))
                            .foregroundStyle(.secondary)
                    }
                }
                .fixedSize()
                .disabled(state.busy || center.toggling)
                .accessibilityIdentifier("proxy-country")
                .popover(isPresented: Binding(get: { center.countryList?.place == .window },
                                              set: { if !$0, center.countryList?.place == .window { center.closeCountryList() } }),
                         arrowEdge: .bottom) {
                    if let list = center.countryList?.state { CountryListPopover(state: list) }
                }
                Spacer(minLength: Theme.Space.s)
                if !w.findFasterLabel.isEmpty {
                    if state.running == .findFaster { ProgressView().controlSize(.small) }
                    Button(w.findFasterLabel) { state.findFaster() }
                        .disabled(state.busy)
                        .accessibilityIdentifier("proxy-find-faster")
                }
            }
            Text(w.countryHint)
                .font(Theme.caption).foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private func status(_ line: (text: String, problem: Bool, progress: Bool)) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: Theme.Space.s) {
            if line.progress {
                ProgressView().controlSize(.mini)
            } else {
                Image(systemName: line.problem ? "exclamationmark.triangle.fill" : "checkmark.circle.fill")
                    .foregroundStyle(line.problem ? Color.red : Color.green)
            }
            Text(line.text)
                .foregroundStyle(line.problem ? Color.red : Color.primary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .font(Theme.secondary)
        .accessibilityIdentifier(line.problem ? "proxy-result-problem" : "proxy-result")
    }
}

/// The shield in the menu bar menu, to the left of "Open Seedwell…" in the footer row, with the row's own highlight. A
/// click does what the model says (the window opens after the menu closes; on and off happen in place, so the shield
/// turns green or grey under the pointer); a right-click closes the menu and shows the shield's menu.
@MainActor final class MenuProxyShield: NSView, HoverRow {
    private let center: ProxyCenter
    /// The pointer is over it: the glow fades in (no background; `ProxyShieldDrawing.Glow`).
    var hovered = false { didSet { if hovered != oldValue { fade() } } }
    /// Pressed (between mouse down and up): the glow brightens.
    private var pressed = false { didSet { if pressed != oldValue { fade() } } }
    /// The glow drawn now, fading towards `target`.
    private(set) var glow: CGFloat = 0
    private var fadeTimer: Timer?
    /// QA and the tests: the glow at its end at once (a picture taken right after a hover).
    var animates = true
    /// QA and the tests: the right-click menu as it pops up.
    var onContext: ((NSMenu) -> Void)?

    init(center: ProxyCenter) {
        self.center = center
        super.init(frame: .zero)
        setAccessibilityElement(true)
        setAccessibilityRole(.menuItem)
        sync()
        center.listen(self) { [weak self] in self?.sync() }
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    /// The shield is drawn at 16 pt, the menu's image size.
    static let glyph: CGFloat = 16

    private func sync() {
        toolTip = center.model.tooltip
        // The tooltip: the state and, when it waits for the proxy, why.
        setAccessibilityLabel(center.model.tooltip)
        needsDisplay = true
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        for a in trackingAreas { removeTrackingArea(a) }
        addTrackingArea(NSTrackingArea(rect: .zero, options: [.mouseEnteredAndExited, .mouseMoved, .activeAlways, .inVisibleRect], owner: self))
        hovered = menuPointerInside
    }

    override func mouseEntered(with event: NSEvent) { hovered = menuPointerInside }
    override func mouseMoved(with event: NSEvent) { hovered = menuPointerInside }
    override func mouseExited(with event: NSEvent) { hovered = false }
    override func viewDidMoveToWindow() { hovered = false }
    override func viewWillMove(toWindow newWindow: NSWindow?) {
        if newWindow == nil { hovered = false; center.stopListening(self) }
    }

    override func mouseDown(with event: NSEvent) { pressed = true }

    override func mouseUp(with event: NSEvent) {
        pressed = false
        guard bounds.contains(convert(event.locationInWindow, from: nil)) else { return }
        press()
    }

    private var target: CGFloat { pressed ? ProxyShieldDrawing.Glow.pressed : hovered ? ProxyShieldDrawing.Glow.hovered : 0 }

    /// Fades the glow to the hover's or press's level over `Glow.fade` (in every run loop mode: the menu tracks in
    /// its own).
    private func fade() {
        fadeTimer?.invalidate()
        fadeTimer = nil
        let to = target
        guard animates, window != nil else { glow = to; needsDisplay = true; return }
        let from = glow
        let start = Date()
        let t = Timer(timeInterval: 1.0 / 60, repeats: true) { [weak self] timer in
            let done = MainActor.assumeIsolated { () -> Bool in
                guard let self else { return true }
                let k = min(1, Date().timeIntervalSince(start) / ProxyShieldDrawing.Glow.fade)
                self.glow = from + (to - from) * CGFloat(k)
                self.needsDisplay = true
                if k >= 1 { self.fadeTimer = nil }
                return k >= 1
            }
            if done { timer.invalidate() }
        }
        RunLoop.main.add(t, forMode: .common)
        fadeTimer = t
    }

    override func rightMouseUp(with event: NSEvent) { context() }
    override func accessibilityPerformPress() -> Bool { press(); return true }
    override func accessibilityPerformShowMenu() -> Bool { context(); return true }

    /// What a click does (also QA's `proxy menu-press`).
    func press() {
        guard center.model.clickOpensWindow || !center.busy else { return }
        if center.model.clickOpensWindow {
            hovered = false
            enclosingMenuItem?.menu?.cancelTracking()
            // After the menu has closed, as a plain item's action would run.
            DispatchQueue.main.async { [center] in MainActor.assumeIsolated { center.openWindow() } }
        } else {
            center.toggle()
        }
    }

    /// What a right-click does: the menu bar menu closes and the shield's menu pops up at the pointer.
    func context() {
        hovered = false
        enclosingMenuItem?.menu?.cancelTracking()
        let menu = MenuBuilder.menu(center.menuEntries)
        if let onContext { onContext(menu); return }
        DispatchQueue.main.async { menu.popUp(positioning: nil, at: NSEvent.mouseLocation, in: nil) }
    }

    /// The shield alone, borderless; while hovered or pressed a glow in its own colour around it.
    override func draw(_ dirtyRect: NSRect) {
        let s = Self.glyph
        let icon = center.model.icon
        let image = ProxyShieldDrawing.image(icon, size: s)
        NSGraphicsContext.saveGraphicsState()
        if glow > 0 {
            let shadow = NSShadow()
            shadow.shadowColor = ProxyShieldDrawing.glowColor(icon, level: glow)
            shadow.shadowBlurRadius = ProxyShieldDrawing.Glow.radius
            shadow.shadowOffset = .zero
            shadow.set()
        }
        image.draw(in: NSRect(x: ((bounds.width - s) / 2).rounded(), y: ((bounds.height - s) / 2).rounded(), width: s, height: s),
                   from: .zero, operation: .sourceOver, fraction: center.busy ? 0.55 : 1)
        NSGraphicsContext.restoreGraphicsState()
    }
}
