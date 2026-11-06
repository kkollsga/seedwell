# Build 77: the torrent proxy, SOCKS5 with a NordVPN preset (for the Windows app)

Build 77. The user's request: torrents (never folder sync, chat or device sessions) can go through NordVPN, the way
NordVPN documents it: its SOCKS5 servers on port 1080 with the user's NordVPN *service credentials* from Nord
Account. A generic SOCKS5 proxy can be set up in the same window. SPEC §7H says what the core does: what goes
through the proxy, what is off while it is on, the kill switch (torrents are held, never connected directly, while
it does not work), how the server is chosen, the health thresholds, where the key is kept.

**Rust decides, the host draws.** Every word, every state and the polling interval come from the core. The key never
comes back from the core: after it is saved the host only learns `HasKey`.

## What the Windows host must do

1. **Build with this core** (`app-windows\build.ps1` regenerates the C# bindings; from bash:
   `powershell.exe -NoProfile -ExecutionPolicy Bypass -File app-windows/build.ps1`). The bindings gain:
   - enums `ProxyIcon` (`NoKey`, `Off`, `On`, `Slow`, `Failed`) and `ProxyKind` (`NordVpn`, `Custom`);
   - record `ProxyModel`: `Icon`, `Tooltip`, `StatusLine`, `HasKey`, `Enabled`, `Kind`, `Server` (the server in use,
     as shown; empty when none), `LatencyMs` (`uint?`), `Problem` (`string?`), `ClickOpensWindow`, `MenuOpenLabel`,
     `MenuToggleLabel` (empty without a key), `RefreshMs`, `CountryMenuLabel` ("Country: Sweden…", the chosen one's name), `Countries`
     (`List<ProxyCountry>`, the first "No preference" with code ""), `Country` (the chosen code, "" none),
     `CountryVisibleRows` (`uint`, 6: how many rows of the country list show),
     `FindFasterLabel` (empty unless the proxy is on);
   - record `ProxyCountry`: `Code`, `Name`;
   - record `ProxyWindowModel` (the "SOCKS5 proxy" window): `Title`, `Explanation`, `KillSwitch`, `KeyLine`, `HasKey`,
     `Kind`, `NordvpnTitle`, `NordvpnIntro`, `GuideShowLabel`, `GuideHideLabel`, `GuideSteps` (`List<string>`),
     `NordAccountUrl`, `UsernamePlaceholder`, `PasswordPlaceholder`, `EnableNordvpnLabel`, `CustomTitle`,
     `CustomIntro`, `HostLabel`, `PortLabel`, `CustomHost`, `CustomPort`, `CustomUserLabel`, `CustomPasswordLabel`,
     `UseCustomLabel`, `NordvpnInstalled`, `CustomInstalled`, `ReplaceRevealLabel`, `ReplaceLabel`,
     `ReplaceCancelLabel`, `PasteHint`, `PastedNote`, `NordAccountLabel`, `RemoveKeyLabel`, `CloseLabel`, `CountryLabel`, `CountryHint`, `Countries`, `Country`,
     `CountryName` (the chosen one's name, "No preference" with none), `CountryVisibleRows`,
     `FindFasterLabel`, `Progress`, `Result`, `ResultIsProblem`;
   - calls on `Core` (and one free function, after the table):

     | call | kind | what it does |
     |---|---|---|
     | `ProxyModel()` | sync, cheap | the icon and the status line; call every `RefreshMs` (500 ms while a key is tested or the servers are checked, else 3 s) |
     | `ProxyOpenModalModel()` | sync, cheap | the window's words and its progress or last result |
     | `ProxySetNordvpnKey(string user, string pass)` | `Task<string>` | tests the credentials with NordVPN's servers (a few seconds at most), keeps them, turns the proxy on; the sentence to show. Throws `CoreException.Failed` with the words to show when they are wrong (it says these look like the Nord Account login, not the service credentials) or no server answered; nothing is kept then |
     | `ProxySetCustom(string host, ushort port, string? user, string? pass)` | `Task<string>` | the same for a SOCKS5 proxy of the user's own (username and password both or neither) |
     | `ProxyToggle()` | `Task<ProxyModel>` | on or off; throws without a key (the host opens the window instead), and while a key test or Find a Faster Server runs (`CoreException.Failed` with a short sentence to show: switching then would be undone when the test ends) |
     | `ProxyRemoveKey()` | `Task` | forgets the key and turns the proxy off; refused the same way while a test runs |
     | `ProxyFindFaster()` | `Task<string>` | "Find a Faster Server": ranks every server now and switches to the best; the sentence to show ("Now using …" / "Already on the best server: …"). While it runs `ProxyModel().StatusLine` is "Testing servers…" and `RefreshMs` 500 |
     | `ProxySetCountry(string code)` | `Task<ProxyModel>` | the user's country (a `Code` from `Countries`, "" for none), chosen by hand only |

   The free function `ProxyPasteFill(string username, string password, string copied, ulong? usernameCopiedMsAgo)`
   (generated as a static function of the bindings' module class, not on `Core`): which field a copied text fills,
   or null (step 4).

   Problems and refusals are plain sentences ("No server answered. The proxy refused the connection."): the core never
   passes on the system's text or "os error N"; show them as they are.

   No C# code constructs these records, so the app builds unchanged before the steps below.

2. **The shield icon** (in the flyout, where the Mac shows it in its menu, and in Settings › Network beside the status
   line). One glyph, a shield with a proxy logo, drawn in five looks from `ProxyModel.Icon`:
   - `NoKey`: all grey;
   - `Off`: green outline, dark grey logo;
   - `On`: all green, white logo;
   - `Slow`: orange;
   - `Failed`: red.
   Tooltip: `Tooltip`. Accessible name: `Tooltip` too (it says the state and, while torrents wait for the proxy, why;
   `StatusLine` alone drops the reason).
   - **No button chrome, a glow on hover** (the user's choice): the icon stays plain and borderless in every state (no
     background, border or bezel, also on hover and press); on hover a soft glow in the icon's own colour (the shield
     colour of its state: green off and on, orange slow, red failed, grey without a key), faded in and out over
     0.15 s, a little brighter while pressed; the hit target stays the same. On WinUI: a Composition `DropShadow` in
     that colour on the icon's visual (`docs/proxy-shield.md`, *Hover*, has the numbers). QA checks the glow by its
     target opacity (rest, hover, pressed), which a QA command sets as a hover or press would; it never moves the
     pointer and never compares pixels.
   - **Left click:** `if (m.ClickOpensWindow) OpenProxyWindow(); else m = await Core.ProxyToggle();`. Run the toggle
     with `await` from the click handler (never `.Result` or `.Wait()` on the UI thread); disable the icon until it
     returns, and while the window's call runs (a key test, Find a Faster Server): the core would refuse it then.
   - **Right click:** a `MenuFlyout` with `MenuOpenLabel` (opens the window); when `MenuToggleLabel` is not
     empty, `MenuToggleLabel` (calls `ProxyToggle`); when `FindFasterLabel` is not empty, `FindFasterLabel` (calls
     `ProxyFindFaster`, then shows its sentence as a notification or in the flyout); and a `MenuFlyoutItem`
     `CountryMenuLabel` that opens **the country list** (below) in a `Flyout` from the tray flyout's shield (the Mac
     opens it in a popover from its menu bar item, its menu having closed). Not a `MenuFlyoutSubItem`: a submenu of
     every country (about 60) is taller than the screen (the user's request: 6 rows with a scrollbar).
   - **The country list** (the same in the right-click menu and the window): a `ListView` exactly
     `CountryVisibleRows` rows tall (the rest scroll, with a scrollbar), each row at the menu's row height and font,
     `Countries` in their order ("No preference" first, by `Name`), a checkmark on the one whose `Code == Country`,
     scrolled so that one shows when it opens (`ScrollIntoView`). Up and Down move, Enter chooses, a letter jumps to
     the first country with it, Escape closes; a click or Enter closes the list and calls
     `await Core.ProxySetCountry(Code)` (never on the UI thread's wait).
   - Poll `ProxyModel()` on a `DispatcherQueueTimer` with `Interval = TimeSpan.FromMilliseconds(m.RefreshMs)`, reset
     after each read, only while the flyout or the settings page is open (the call is cheap: no Credential Manager
     or DPAPI read after the first, no network).

3. **Settings › Network: the proxy area.** Where the page's blocks have `SettingsBlock.TorrentProxy` (between
   Connection and Peers; until it is drawn the page's `_ =>` branch leaves it empty), a box titled with its `Title` and a
   row with the icon, `StatusLine`, the window model's `KeyLine` under it (only with `HasKey`: without one the status
   line says so), and at the right, with `HasKey` only, an On switch (`ToggleSwitch`, on while `Enabled`; flipping it
   calls `ProxyToggle()`, disabled until it answers and while the window's call runs; tooltip and accessible name `MenuToggleLabel`) left of a button
   "SOCKS5 Proxy…" (`MenuOpenLabel`) that opens the window. When `Problem` is set, show it under the line in the error style (with the
   proxy waiting for a server that works, `StatusLine` is only "Waiting for proxy · NordVPN": the reason is `Problem`).
   The row's accessible name is `StatusLine` and `Problem` together. A toggle refused while a test runs: show its
   message under the line for a few seconds. The key itself is never shown anywhere.

4. **The "SOCKS5 proxy" window** (a `ContentDialog` or a small window; Close bottom right as Windows orders it). From
   `ProxyOpenModalModel()`:
   - `Title` with the icon and `ProxyModel().StatusLine` (and `Problem` in red) beside it, and at the right of the title,
     with `HasKey` only, the same On switch as in Settings; `Explanation`; `KillSwitch`; `KeyLine`.
   - **A key installed:** in the pane of its kind, `NordvpnInstalled` (NordVPN's pane) or `CustomInstalled` (the custom
     pane) is not empty: show that short line and a button `ReplaceRevealLabel` ("Replace Key…") instead of the fields.
     The button shows the pane's fields again, with `ReplaceCancelLabel` (hides them, empties them) and `ReplaceLabel`
     ("Replace Key", the same call as Enable NordVPN or Use This Proxy) in place of the pane's own button. Windows
     puts `ReplaceLabel` before `ReplaceCancelLabel` (its platform order, primary first; the Mac puts Cancel first). The guide's
     link stays. The other pane shows its form as usual (setting it up replaces the installed key). Clipboard text
     fills only fields that show.
   - **NordVPN section:** `NordvpnTitle`, `NordvpnIntro`; a `HyperlinkButton` with `GuideShowLabel` that shows the
     guide inside the window and turns into `GuideHideLabel` (the guide: `GuideSteps` as a numbered list; its first
     step names Nord Account, `NordAccountUrl` may be a link); a `TextBox` (`UsernamePlaceholder`) and a
     `PasswordBox` (`PasswordPlaceholder`); the button `EnableNordvpnLabel` → `await Core.ProxySetNordvpnKey(user,
     pass)`. Clear the `PasswordBox` after the call, whatever it answered.
   - **Custom section:** `CustomTitle`, `CustomIntro`; `HostLabel` (filled with `CustomHost`), `PortLabel` (filled
     with `CustomPort`), `CustomUserLabel`, `CustomPasswordLabel` (a `PasswordBox`); the button `UseCustomLabel` →
     `await Core.ProxySetCustom(host, port, user, pass)` (empty fields as `null`; a port that is not a number: show
     the core's answer to port 0).
   - With `HasKey`: the button `RemoveKeyLabel` → `await Core.ProxyRemoveKey()`.
   - **Copied credentials fill the fields** (the Mac app does it; `PasteHint` under the two fields says so): while the
     window is open, poll `GetClipboardSequenceNumber()` on a 500 ms `DispatcherQueueTimer` (only while it is open)
     and read the clipboard's text only when the number changed since the last look, the fields show and one of them
     is empty. Not `Clipboard.ContentChanged`: it may not fire while another app (the browser with Nord Account) is in
     front, which is exactly when the user copies. Hand each new text with what the two fields hold, and how many ms ago the username
     was filled from a copy (null when it was typed or changed since), to `ProxyPasteFill`: it answers which field the
     text fills (`ProxyPaste.Username` / `ProxyPaste.Password`) or null. The password is filled only within two minutes
     of a copied username, so an unrelated secret copied later never reaches NordVPN. While a field holds what a copy
     put there, show `PastedNote` under the fields. Never read the clipboard while the window is closed, while the
     fields are hidden (a key installed and not being replaced), or when both fields hold something. The guide's link to Nord Account is `NordAccountLabel` → `NordAccountUrl`.
   - **Country:** `CountryLabel`, then a pop-up-style button (a `DropDownButton`) showing `CountryName` that opens
     the country list (step 2) in a `Flyout` under it; `CountryHint` under them. Disabled, like the On switch, while a
     call runs. Not a `ComboBox`: its drop-down shows far more than `CountryVisibleRows` rows.
   - Problems are plain sentences ("No server answered. The proxy refused the connection."): the core never passes
     on the system's "os error N".
   - When `FindFasterLabel` is not empty: a button with it → `await Core.ProxyFindFaster()`; show the sentence in
     `Result`'s place.
   - While a call runs: `Progress` with a `ProgressRing`, the buttons disabled; refresh the model every 500 ms
     (`ProxyOpenModalModel()` is cheap). After it: `Result`, in the error style when `ResultIsProblem`. A failed call
     throws `CoreException.Failed`; show its `Message` (the same words as `Result`).
   - `CloseLabel` closes it.

5. **QA commands** (`app-windows\qa\run.ps1`): add `proxy-model` (prints `ProxyModel()`), `proxy-window` (prints
   `ProxyOpenModalModel()`), `proxy-toggle`, `proxy-click` (does what a left click does: the window's open state
   or the toggle), `proxy-find-faster`, `proxy-country <code>`,
   `proxy-country-list window|menu` (opens the country list where a click would, for a render: 6 rows, the checked one
   shown) and one that puts the shield at rest, hovered or
   pressed and prints the glow's target opacity, so a check never clicks or moves the pointer. Tests and QA never use a real NordVPN key or server: the core's own tests
   use a loopback SOCKS5 fake, and an isolated run (`forbidden_root` set, as QA's) never reaches NordVPN at all: its
   "NordVPN" servers are the ones `<state>\qa\proxy-servers.txt` names (one `host:port [country] [city]` per line, none
   without the file); the Mac's `app/qa/fake_socks5.py` is a loopback fake for them.

## What the core already does on Windows

- The key lives in the state folder's DPAPI-sealed vault (`sync\vault`, the same store as the folder and channel
  keys), never in `config.toml` or `core.log`.
- With the proxy on at start, the engine starts with it in place: the first connection is already a proxied one.
- Folder sync, chat and device sessions never touch the proxy, also while it holds every torrent (the user's
  decision: NordVPN never sees them).
- `.torrent` links and other direct downloads go through the proxy while it is on.
