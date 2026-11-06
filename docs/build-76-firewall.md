# Build 76: "Other devices can't reach this PC", and allowing Seedwell through Windows Firewall

Measured on the user's PC "Makro" (0.1.19): Seedwell's sync port answers on the PC itself over IPv4 and IPv6, but
from the Mac `nc -vz 192.168.68.73 51710` times out. Windows Firewall blocks inbound connections: there is no rule,
allow or block, for the installed `%LOCALAPPDATA%\Programs\Seedwell\Seedwell.exe` (only for development paths, which
do not count: rules match by program path), and the network is Public, whose profile has `NotifyOnListen` off, so
Windows never asked. Outbound works (the PC dials the Mac), so nothing looked wrong on the PC, but no device can dial
it: it cannot host for other devices, and peers that rely on dialing it wait.

The user's decision: when Seedwell sees that it cannot be reached, it says so plainly, and a one-time **Allow Seedwell
through Windows Firewall…** asks for administrator rights once (UAC) and adds an inbound allow rule for the installed
Seedwell.exe only. The rule is SPEC §7G.3, *Can other devices reach this one*.

## What the core does

- **Detection** (`crates/sync-net/src/inbound.rs`): a device asks a device it dialed directly over TCP to dial it back
  (ReachAsk / ReachAnswer, types 33 and 34, feature bit 6; PROTOCOL.md). Blocked only with evidence: a device failed
  to connect back and none reached this one within the hour. A session this device accepted proves it reachable.
  `SyncHandle::inbound()`, `SyncHandle::check_inbound_again()`.
- **The rule** (`crates/platform/src/firewall.rs`): `firewall::state(exe, limit)` reads, with PowerShell's
  NetSecurity module in a hidden window (`-EncodedCommand`; the exe's path in the environment variable
  `SEEDWELL_EXE`, never in the script's text, where a quote in a folder name, `'` or U+2018 to U+201B, would break
  it), whether an enabled inbound
  allow rule for this exe covers the current networks' profiles and no enabled block rule applies; also the profiles
  and their `NotifyOnListen`. Allowed, missing, blocked or unknown. `firewall::allow_command(exe, rule)` is the
  command.
- **The notice** (`crates/ffi/src/sync_reach.rs`): `SyncReach`, one per app run. `model()` is instant (a lock): it
  never waits for PowerShell, which runs on a thread of its own while the device is blocked (at most every 5
  minutes). `recheck()` after the action reads the rule again and has the devices dial back again (asks still
  waiting are dropped); the model says nothing until both are in (150 s at most). A rule that reads as before the
  action means an administrator's policy decides: the notice says so, without the action.

## What the Windows app draws and runs

1. **Make one** `SyncReach` at start (`new SyncReach(core)`), beside the `Notices`.
2. **Poll** `Model()` every `RefreshMs` (60 s; 2 s while something is being read or after a recheck), off the UI
   thread or on it (it is a lock; in a QA run it also reads two small files). Redraw when it changes.
3. **Draw** `Notice` (when not null) as a banner at the top of the Folder Sync page, above the folder list and the
   page, in every state of the page (no folders too): `Title` in the body's semibold, `Body` under it wrapped, in the
   caution colour as `OnDemandOff` is (`SystemFillColorCautionBrush`), and the `Action`'s button at the right
   (`Action.Title`, "Allow Seedwell through Windows Firewall…") when there is one.
4. **The button** runs the command elevated, off the UI thread:
   ```csharp
   var psi = new ProcessStartInfo(action.Program, action.Arguments)
   {
       UseShellExecute = true,          // ShellExecuteEx
       Verb = "runas",                  // the UAC prompt
       WindowStyle = ProcessWindowStyle.Hidden,
   };
   await Task.Run(() => { using var p = Process.Start(psi); p?.WaitForExit(30_000); });
   ```
   `Program` is `netsh.exe`; `Arguments` is the core's string as it is (it quotes the exe path itself; never build
   or change it in C#). The user saying No to UAC throws `Win32Exception` with `NativeErrorCode` 1223
   (`ERROR_CANCELLED`): catch it and do nothing else (the notice stays; no recheck, which would read the same rule
   and take it for a policy). Any other ending, whatever netsh's exit code: call `reach.Recheck()` and redraw from
   the next `Model()` (no notice while the check runs; then none, the policy notice without a button, or another). Never read the firewall in C#: the core's check
   is the only answer.
5. Nothing else: no rule is added at install or start, never without the user's click.

## What the Mac draws

The same banner from `SyncReach.model()` at the top of the Folder Sync page, with no button (`action` is always nil on
the Mac: its application firewall asks by itself; the body says where to let Seedwell in). Not drawn yet: a host
change in `app/Sources/SeedwellApp/Window/SyncPage.swift` (poll `model()` every `refreshMs` off the main actor).

## QA on the PC

UAC cannot be clicked in QA, so QA tests the command, the detection and the drawing, never the elevation:

- **Rust, on Windows** (`app-windows\qa\rust-tests.ps1`): `firewall::tests::the_rule_of_this_program_is_read_on_windows`
  runs the PowerShell check for real and fails when it cannot answer (missing, or allowed where the firewall is off,
  but never unknown); `the_command_quotes_a_path_with_spaces_whole` checks the command; `sync_reach::tests::*` the
  notice and that `model()` never waits.
- **A QA check `firewall`** in `run.ps1` (new): an isolated run (state folder in a temporary folder) with the QA files
  the core reads at every `model()`:
  - `<state>\qa\inbound.txt`: `blocked` (or `blocked-internet`, `reachable`, `unknown`) stands for folder sync's
    verdict;
  - `<state>\qa\firewall.txt`: a line as the check prints it, `missing|Public|False`, `blocked|Public|True`,
    `allowed|Public|False`, anything else not known; it stands for the rule (and the real check is not run).

  Steps: start with `inbound.txt` = `blocked` and `firewall.txt` = `missing|Public|False`; a QA command (new, in the
  app's QA control, `reach`) writes the app's current `SyncReach` model to app.log; check the title, that the body
  names Windows Firewall and "Public", and the action's `Program` and `Arguments` (`program="<the QA app's exe>"`
  quoted whole). Shoot the page (`shot reach`). Then write `allowed|Public|False`, run the QA command `reach-recheck`
  (calls `Recheck()`, as the button does after netsh), and see the notice gone in the next model; then `blocked|...`
  shows "Windows Firewall blocks Seedwell" with the `set rule` command. Policy: with `missing|Domain|False`, run
  `reach-recheck` without changing the file: the notice says an administrator's policy controls it, with no
  button. Never run the elevated command in QA.
- **By hand on a PC, once, not in QA** (the user has since added the rules on Makro, so it is no test of the button
  there): on a PC without the rule, from a device on the same network, `nc -vz <pc> <sync port>` times out before and
  connects after the button.
- `seedwell` CLI: `sync settings` already shows the port; the verdict can be read in the core's log (`could not connect
  to this device at …`).

## Not in this build

- A device whose every peer is older than 0.1.20 is never asked to be dialed back: *not known*, no notice.
- From the Internet, only with a router mapping (NAT-PMP/UPnP) is a dial back asked for; behind a carrier-grade NAT,
  nothing.
- A rule Group Policy decides cannot be changed by netsh: after one click the notice says so and drops the button.
