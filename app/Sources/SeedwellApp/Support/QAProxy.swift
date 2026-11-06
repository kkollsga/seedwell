import AppKit

/// QA's `proxy …` commands (QAControl): the torrent proxy's shield, its right-click menu and the "SOCKS5 proxy" window,
/// through the calls their clicks and fields make (no event is posted). A QA run is isolated, so the core's "NordVPN"
/// servers are the loopback fakes `<state>/qa/proxy-servers.txt` names (`app/qa/fake_socks5.py`), never NordVPN's;
/// the window reads copied text from a pasteboard of QA's own (`proxy copy`), never the user's.
///
///     proxy model                     the shield's model: icon, key, on, status line, problem, what a click does, the menu
///     proxy click                     a click on the shield (Settings, the window's header): the window without a key, else on/off
///     proxy menu-press                a click on the menu bar menu's shield (`menu open` first)
///     proxy context [menubar]         the right-click menu, "open" for `shot … menu` (menubar: the menu bar menu's shield)
///     proxy press <title>             an entry of the right-click menu ("SOCKS5 Proxy…", "Turn Off NordVPN", "Country: Norway…")
///     proxy toggle | find-faster | country <code>   the menu's entries
///     proxy country-list [menu|window|settings]   opens the country list where that place's click opens it (menu: the
///                                     menu bar shield's "Country: …"; window: the window's country button)
///     proxy country-list show         the open list: where, its rows, how many show, the top one, the chosen one
///     proxy country-list up|down|return|escape|type <letter>|choose <name>|close   its keys and a click on a row
///     proxy open | close              the window
///     proxy window                    the window: pane, guide, fields (the password only as set or empty), buttons, progress, result
///     proxy pane nordvpn|custom | guide show|hide | replace [cancel]   (replace: "Replace Key…", cancel: its Cancel)
///     proxy switch                    the On switch (Settings, the window's header): what flipping it calls
///     proxy type username|password|host|port|custom-user|custom-password <text>   typing into a field
///     proxy copy <text>               a copy in another app (QA's own pasteboard): the window may fill a field
///     proxy enable | use | remove     Enable NordVPN, Use This Proxy, Remove Key
@MainActor enum QAProxy {
    /// The pasteboard a QA run's window reads (named, so the user's is never read or written).
    static let pasteboard = NSPasteboard(name: NSPasteboard.Name("seedwell-qa-proxy"))

    static func run(_ arg: String, app: AppDelegate, pop: (NSMenu) -> Void) async -> String {
        let center = app.commands.proxy
        let words = arg.split(separator: " ", maxSplits: 1).map(String.init)
        let rest = words.count > 1 ? words[1] : ""
        switch words.first ?? "model" {
        case "model":
            center.refresh()
            return describe(center)
        case "click":
            center.click()
            await CoreCalls.settled()
            return describe(center)
        case "menu-press":
            guard let shield = app.statusMenuController?.footerRow?.shield else { return "proxy: the menu is not built (menu open first)" }
            shield.press()
            await CoreCalls.settled()
            return describe(center)
        case "context":
            if rest == "menubar" {
                guard let shield = app.statusMenuController?.footerRow?.shield else { return "proxy: the menu is not built (menu open first)" }
                var popped: NSMenu?
                shield.onContext = { popped = $0 }
                shield.context()
                shield.onContext = nil
                guard let menu = popped else { return "proxy: no menu" }
                pop(menu)
                return "proxy: context \(titles(menu))"
            }
            let menu = MenuBuilder.menu(center.menuEntries)
            pop(menu)
            return "proxy: context \(titles(menu))"
        case "press":
            return "proxy: press \(press(rest, in: MenuBuilder.menu(center.menuEntries)))"
        case "toggle", "switch":
            if words.first == "switch", !center.model.hasKey { return "proxy: no switch without a key" }
            center.toggle()
            await CoreCalls.settled()
            return describe(center)
        case "find-faster":
            center.findFaster()
            return describe(center)
        case "country-list":
            return await countryList(rest, center)
        case "country":
            center.setCountry(rest.uppercased() == "NONE" ? "" : rest)
            await CoreCalls.settled()
            return describe(center)
        case "open":
            center.openWindow()
            return window(center)
        case "close":
            center.closeWindow()
            return window(center)
        case "window":
            return window(center)
        default:
            guard let w = center.window else { return "proxy: the window is closed (proxy open first)" }
            switch words.first ?? "" {
            case "pane": w.pane = rest == "custom" ? .custom : .nordVpn
            case "guide": w.guideShown = rest != "hide"
            case "replace": if rest == "cancel" { w.cancelReplace() } else { w.replacing = true }
            case "type":
                let f = rest.split(separator: " ", maxSplits: 1).map(String.init)
                let text = f.count > 1 ? f[1] : ""
                switch f.first ?? "" {
                case "username": w.username = text
                case "password": w.password = text
                case "host": w.host = text
                case "port": w.port = text
                case "custom-user": w.customUser = text
                case "custom-password": w.customPassword = text
                default: return "proxy: type username|password|host|port|custom-user|custom-password <text>"
                }
            case "copy":
                pasteboard.clearContents()
                pasteboard.setString(rest, forType: .string)
                w.checkPasteboard()  // the watch's next look, now
            case "enable": w.enable()
            case "use": w.useCustom()
            case "remove": w.removeKey()
            default: return "proxy: unknown command \(arg)"
            }
            return window(center)
        }
    }

    /// The country list's commands (see the type's comment).
    private static func countryList(_ arg: String, _ center: ProxyCenter) async -> String {
        let f = arg.split(separator: " ", maxSplits: 1).map(String.init)
        let rest = f.count > 1 ? f[1] : ""
        switch f.first ?? "menu" {
        case "menu", "window", "settings":
            let place: CountryListPlace = f.first == "window" ? .window : f.first == "settings" ? .settings : .menuBar
            if place == .window, center.window == nil { return "proxy-country-list: the window is closed (proxy open first)" }
            center.openCountryList(at: place)
            // Until it is laid out and scrolled to the chosen row (SwiftUI presents and scrolls on later passes).
            for _ in 0..<40 {
                try? await Task.sleep(for: .milliseconds(50))
                if scroller(center) != nil { break }
            }
            try? await Task.sleep(for: .milliseconds(150))
            return describeList(center)
        case "show": return describeList(center)
        case "close": center.closeCountryList()
        default:
            guard let list = center.countryList?.state else { return "proxy-country-list: closed" }
            switch f.first ?? "" {
            case "up": list.move(-1)
            case "down": list.move(1)
            case "return": list.chooseHighlighted()
            case "escape": list.cancel()
            case "type": list.jump(to: rest)
            case "choose":
                guard let i = list.countries.firstIndex(where: { $0.name == rest }) else { return "proxy-country-list: no country \(rest)" }
                list.choose(at: i)
            default: return "proxy-country-list: unknown command \(arg)"
            }
            await CoreCalls.settled()
        }
        try? await Task.sleep(for: .milliseconds(100))
        return describeList(center)
    }

    /// The open list's scroll view, as drawn (its window: the menu bar's popover, or a popover of the window or Settings).
    private static func scroller(_ center: ProxyCenter) -> NSScrollView? {
        guard let list = center.countryList else { return nil }
        let windows: [NSWindow]
        switch list.place {
        case .menuBar: windows = center.countryPopoverWindow.map { [$0] } ?? []
        default: windows = NSApp.windows.filter { $0.isVisible && String(describing: type(of: $0)).contains("Popover") }
        }
        for w in windows {
            if let v = w.contentView.flatMap({ find($0) }) { return v }
        }
        return nil
    }

    /// The first scroll view whose content is taller than what it shows.
    private static func find(_ v: NSView) -> NSScrollView? {
        if let s = v as? NSScrollView, (s.documentView?.frame.height ?? 0) > s.contentView.bounds.height + 1 { return s }
        for sub in v.subviews { if let s = find(sub) { return s } }
        return nil
    }

    /// "proxy-country-list: open at menuBar, rows 61, showing 6 (132 pt, 22 pt each), drawn 132 pt, content 1342 pt,
    /// top row 40 (Sweden at 43, shown yes), highlighted none, country SE".
    static func describeList(_ center: ProxyCenter) -> String {
        guard let (place, list) = center.countryList else {
            return "proxy-country-list: closed, country \(center.model.country.isEmpty ? "none" : center.model.country)"
        }
        let rh = CountryListView.rowHeight
        var drawn = "not drawn"
        if let v = scroller(center) {
            let visible = v.contentView.documentVisibleRect
            let doc = v.documentView?.frame.height ?? 0
            // The document's y runs down from the top in a flipped clip view (SwiftUI's), else up from the bottom.
            let fromTop = v.contentView.isFlipped ? visible.minY : doc - visible.maxY
            let top = Int((fromTop / rh).rounded())
            let chosen = list.chosenIndex
            let shown = chosen >= top && chosen < top + list.visibleRows
            let exact = abs(visible.height - CountryListView.height(rows: list.visibleRows)) < 1
            drawn = "drawn \(Int(visible.height.rounded())) pt (the rows exactly: \(exact ? "yes" : "no")), content \(Int(doc.rounded())) pt "
                + "(scrolls \(doc > visible.height + 1 ? "yes" : "no")), top row \(top) "
                + "(\(list.countries[chosen].name) at \(chosen), shown \(shown ? "yes" : "no"))"
        }
        return "proxy-country-list: open at \(place), rows \(list.countries.count), showing \(list.visibleRows) "
            + "(\(Int(CountryListView.height(rows: list.visibleRows))) pt, \(Int(rh)) pt each), \(drawn), "
            + "highlighted \(list.highlighted.map { list.countries[$0].name } ?? "none"), country \(list.chosen.isEmpty ? "none" : list.chosen)"
    }

    /// "proxy: icon on, key yes, on yes, status 'On · NordVPN · 127.0.0.1 · 2 ms', problem none, click toggles,
    /// country NL, polling yes, menu [SOCKS5 Proxy…|Turn Off NordVPN|…]".
    static func describe(_ c: ProxyCenter) -> String {
        let m = c.model
        let menu = QAControl.describe(c.menuEntries)
        return "proxy: icon \(m.icon), key \(m.hasKey ? "yes" : "no"), on \(m.enabled ? "yes" : "no"), switch \(m.hasKey ? "shown" : "hidden"), status '\(m.statusLine)', problem \(m.problem.map { "'\($0)'" } ?? "none"), "
            + "click \(m.clickOpensWindow ? "opens the window" : "toggles"), toggling \(c.toggling ? "yes" : "no"), busy \(c.busy ? "yes" : "no"), "
            + "notice \(c.notice.map { "'\($0)'" } ?? "none"), country \(m.country.isEmpty ? "none" : m.country), "
            + "polling \(c.polling ? "yes" : "no"), tooltip '\(m.tooltip)', menu [\(menu)]"
    }

    /// The window as drawn: "proxy-window: open, pane nordVpn, guide shown, username 'x', password set, busy no, …".
    static func window(_ c: ProxyCenter) -> String {
        guard let w = c.window else { return "proxy-window: closed, polling \(c.polling ? "yes" : "no")" }
        let m = w.model
        let line = w.statusLine.map { "\($0.progress ? "progress" : $0.problem ? "problem" : "result") '\($0.text)'" } ?? "none"
        return "proxy-window: open, pane \(w.pane), guide \(w.guideShown ? "shown (\(m.guideSteps.count) steps)" : "hidden"), "
            + "username \(w.username.isEmpty ? "empty" : "set"), password \(w.password.isEmpty ? "empty" : "set"), "
            + "pasted-note \(w.filledFromCopy ? "shown" : "hidden"), host '\(w.host)', port '\(w.port)', "
            + "key-line '\(m.keyLine)', installed '\(m.nordvpnInstalled)\(m.customInstalled)', replacing \(w.replacing ? "yes" : "no"), "
            + "form \(formShown(w) ? "shown" : "hidden"), remove-key \(m.hasKey ? "shown" : "hidden"), find-faster \(m.findFasterLabel.isEmpty ? "hidden" : "shown"), "
            + "country \(m.country.isEmpty ? "none" : m.country), busy \(w.busy ? "yes" : "no"), watching-pasteboard \(w.watchingPasteboard ? "yes" : "no"), line \(line)"
    }

    /// Whether the shown pane has its fields (no key of its kind installed, or replacing it).
    private static func formShown(_ w: ProxyWindowState) -> Bool {
        let installed = w.pane == .nordVpn ? w.model.nordvpnInstalled : w.model.customInstalled
        return installed.isEmpty || w.replacing
    }

    private static func titles(_ menu: NSMenu) -> String {
        menu.items.map { $0.isSeparatorItem ? "-" : ($0.submenu != nil ? "\($0.title) >" : $0.title + ($0.state == .on ? " ✓" : "")) }.joined(separator: "|")
    }

    /// Presses the entry `path` ("Country > Norway") as a click on it does.
    private static func press(_ path: String, in menu: NSMenu) -> String {
        let parts = path.components(separatedBy: " > ")
        var m = menu
        for (i, part) in parts.enumerated() {
            guard let index = m.items.firstIndex(where: { $0.title == part }) else { return "no entry \(part)" }
            if i == parts.count - 1 {
                m.performActionForItem(at: index)
                return part
            }
            guard let sub = m.items[index].submenu else { return "\(part) has no submenu" }
            m = sub
        }
        return "nothing"
    }
}
