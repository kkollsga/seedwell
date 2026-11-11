<#
.SYNOPSIS
  All Windows app QA in one command: the visual QA renders and the behaviour checks.

.DESCRIPTION
  app-windows\qa\run.ps1 [name ...|all] [-Build] [-Out <dir>]

  Each check runs on its own temporary state folder (deleted after), one at a time, and prints PASS or FAIL with its
  time and its pictures; then a summary. Exit code 1 when any check failed. Every run is quiet: no window is activated
  or brought forward, nothing needs the screen awake or unlocked, no tracker is contacted (new torrents wait ten
  minutes before they start, the most the setting allows, and the checks end long before), and nothing else reaches
  the network either (no DHT, PEX, LSD, UPnP or NAT-PMP).

  Checks: render, handoff, separate, gate, selection, keys, removal, info, alert, trash, choice, move, add, chat,
  dialogs, newchannel, links, tray, scheme, rendezvous, follow, firewall, proxy, proxysecrets, navpane, listen, readonly, adminremove, folders, chatpage, chatslowdisk, platform, slowstart, damaged, notices, settingspage, requests, files, chooser, play, favorites, rules, cloudfiles, ondemand, ondemandfetch, ondemandpin, ondemandchanges, ondemandwrites, ondemandstale, ondemandfirst, ondemandbusy, startturns, shotturns, addbigfolder, timeleft, hangwatch, shareopening, shellstarts, addlink, dialogfocus, focuswalk, leaks, notray, settings, quit (all of them: all, the default).
  By name only, not in all: interactive (the first seconds of a large add, measured against the user's limits; see the check).
  -Build builds the app first (build.ps1; -CoreProfile Debug builds its core from the debug test build, as CI does).
  -Out keeps the pictures in <dir> (default: a temp folder, printed).
  Flyouts (menus, the members panel and its role menus) are drawn by the render run (SnapshotRenderer.cs, check
  render), never shot from the running app by a QA command: a QA launch is a second Seedwell.exe that hands its
  command over and takes the focus, and a flyout closes when its window loses the focus.
  Needs the seedwell CLI: target\debug\seedwell.exe (built with cargo when missing).
#>
param(
  [Parameter(Position = 0, ValueFromRemainingArguments = $true)][string[]]$Names = @('all'),
  [switch]$Build,
  [ValidateSet('Debug', 'Release')][string]$CoreProfile = 'Release',
  [string]$Out
)

# The CLI writes to stderr while the app starts; PowerShell 5.1 would make that a terminating error. Checks throw their own.
$ErrorActionPreference = 'Continue'
$App = Split-Path $PSScriptRoot -Parent
$Repo = Split-Path $App -Parent
$Exe = Join-Path $App 'bin\x64\Release\net10.0-windows10.0.19041.0\win-x64\Seedwell.exe'
$Cli = Join-Path $Repo 'target\debug\seedwell.exe'
$Fixtures = Join-Path $Repo 'crates\metainfo\tests\fixtures'
$All = @('render', 'handoff', 'separate', 'gate', 'selection', 'keys', 'removal', 'info', 'alert', 'trash', 'choice', 'move', 'add', 'chat', 'dialogs', 'groups', 'syncinplace', 'clisettings', 'restart', 'newchannel', 'links', 'tray', 'scheme', 'rendezvous', 'follow', 'firewall', 'proxy', 'proxysecrets', 'navpane', 'listen', 'readonly', 'adminremove', 'folders', 'chatpage', 'chatslowdisk', 'platform', 'slowstart', 'damaged', 'notices', 'settingspage', 'requests', 'files', 'chooser', 'play', 'favorites', 'rules', 'cloudfiles', 'ondemand', 'ondemandfetch', 'ondemandpin', 'ondemandchanges', 'ondemandwrites', 'ondemandstale', 'ondemandfirst', 'ondemandbusy', 'startturns', 'shotturns', 'addbigfolder', 'timeleft', 'hangwatch', 'shareopening', 'shellstarts', 'addlink', 'dialogfocus', 'focuswalk', 'leaks', 'notray', 'settings', 'quit')

if ($Build) {
  & powershell -NoProfile -File (Join-Path $App 'build.ps1') -CoreProfile $CoreProfile | Out-Host
  if ($LASTEXITCODE -ne 0) { Write-Host 'The build failed (is the app running from bin\? Quit it with Seedwell.exe --quit).'; exit 1 }
}
if (-not (Test-Path $Exe)) { throw "No app at ${Exe}: run with -Build" }
# With -Build the CLI too: one older than the core fails checks for options it has not got.
if ($Build -or -not (Test-Path $Cli)) {
  Push-Location $Repo; cargo build -q --bin seedwell; $built = $LASTEXITCODE; Pop-Location
  if ($built -ne 0) { Write-Host 'The CLI build failed.'; exit 1 }
}
# What earlier runs left in the temp folder: their default picture folders, and states of runs that were stopped or crashed.
Get-ChildItem ([IO.Path]::GetTempPath()) -Directory -Filter 'seedwell-*' -ErrorAction SilentlyContinue |
  Where-Object { $_.Name -match '^seedwell-(qa|render)-' -and $_.LastWriteTime -lt (Get-Date).AddHours(-1) } |
  ForEach-Object { Remove-Item -LiteralPath $_.FullName -Recurse -Force -ErrorAction SilentlyContinue }
if (-not $Out) { $Out = Join-Path ([IO.Path]::GetTempPath()) "seedwell-qa-$([DateTime]::Now.ToString('yyyyMMdd-HHmmss'))" }
New-Item -ItemType Directory -Force $Out | Out-Null
$names = if ($Names -contains 'all') { $All } else { $Names }
foreach ($n in $names) { if ($All + 'interactive' -notcontains $n) { throw "Unknown check '$n'. Checks: $($All -join ', '), interactive" } }

# ---------------------------------------------------------------------------------------------- helpers

Add-Type @'
using System; using System.Runtime.InteropServices;
public static class QaForeground {
  [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  public static uint Pid() { uint p; GetWindowThreadProcessId(GetForegroundWindow(), out p); return p; }
}
// Which processes hold a file open (Restart Manager): a refused write names who refused it.
public static class QaHolder {
  [StructLayout(LayoutKind.Sequential)] struct Unique { public uint Pid; public System.Runtime.InteropServices.ComTypes.FILETIME Start; }
  [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)] struct Info {
    public Unique Process;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 256)] public string App;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 64)] public string Service;
    public uint Type, Status, Session;
    [MarshalAs(UnmanagedType.Bool)] public bool Restartable;
  }
  [DllImport("rstrtmgr.dll", CharSet = CharSet.Unicode)] static extern int RmStartSession(out uint session, int flags, System.Text.StringBuilder key);
  [DllImport("rstrtmgr.dll")] static extern int RmEndSession(uint session);
  [DllImport("rstrtmgr.dll", CharSet = CharSet.Unicode)] static extern int RmRegisterResources(uint session, uint files, string[] names, uint apps, IntPtr unique, uint services, string[] serviceNames);
  [DllImport("rstrtmgr.dll")] static extern int RmGetList(uint session, out uint needed, ref uint count, [In, Out] Info[] infos, out uint reasons);
  public static string Of(string path) {
    uint session;
    if (RmStartSession(out session, 0, new System.Text.StringBuilder(64)) != 0) return "unknown";
    try {
      RmRegisterResources(session, 1, new[] { path }, 0, IntPtr.Zero, 0, null);
      uint needed, count = 16, reasons;
      var infos = new Info[16];
      if (RmGetList(session, out needed, ref count, infos, out reasons) != 0) return "unknown";
      var names = new System.Collections.Generic.List<string>();
      for (var i = 0; i < count; i++) names.Add(infos[i].Process.Pid + " " + infos[i].App);
      return names.Count == 0 ? "nobody by then" : string.Join(", ", names);
    } finally { RmEndSession(session); }
  }
}
// A process's handles of the two types a timer holds, from the system's list (the process itself is not opened).
public static class QaHandles {
  [DllImport("ntdll.dll")] static extern int NtQuerySystemInformation(int c, IntPtr b, int l, out int r);
  [DllImport("ntdll.dll")] static extern int NtQueryObject(IntPtr h, int c, IntPtr b, int l, out int r);
  static System.Collections.Generic.HashSet<int> TimerTypes() {
    int len = 1 << 20, r; var b = Marshal.AllocHGlobal(len);
    try {
      int s = NtQueryObject(IntPtr.Zero, 3, b, len, out r);
      if (s != 0) throw new Exception("the type list: " + s.ToString("x"));
      var found = new System.Collections.Generic.HashSet<int>();
      int n = Marshal.ReadInt32(b); long at = b.ToInt64() + 8;
      for (int i = 0; i < n; i++) {
        var entry = new IntPtr(at);
        var name = Marshal.PtrToStringUni(Marshal.ReadIntPtr(entry, 8));
        if (name == "Timer" || name == "WaitCompletionPacket") found.Add(i + 2);
        at += 0x68 + ((Marshal.ReadInt16(entry, 2) + 7) & ~7);
      }
      if (found.Count != 2) throw new Exception("the type list has " + found.Count + " of Timer and WaitCompletionPacket");
      return found;
    } finally { Marshal.FreeHGlobal(b); }
  }
  public static int Timers(int pid) {
    var types = TimerTypes();
    int len = 64 << 20, r, s; IntPtr b;
    while (true) {
      b = Marshal.AllocHGlobal(len);
      s = NtQuerySystemInformation(64, b, len, out r);
      if (s == 0) break;
      Marshal.FreeHGlobal(b);
      len *= 2;
      if (len > (1 << 30)) throw new Exception("the handle list: " + s.ToString("x"));
    }
    try {
      long n = Marshal.ReadInt64(b); int count = 0;
      for (long i = 0; i < n; i++) {
        var e = new IntPtr(b.ToInt64() + 16 + i * 40);
        if (Marshal.ReadInt64(e, 8) == pid && types.Contains(Marshal.ReadInt16(e, 30))) count++;
      }
      return count;
    } finally { Marshal.FreeHGlobal(b); }
  }
}
// Another process freeing a placeholder's space, as Storage Sense does (the app's NOTIFY_DEHYDRATE decides).
public static class QaCloud {
  [DllImport("kernel32.dll", SetLastError = true)]
  static extern bool DeviceIoControl(Microsoft.Win32.SafeHandles.SafeFileHandle h, uint code, IntPtr inBuf, uint inSize, IntPtr outBuf, uint outSize, out uint returned, IntPtr overlapped);
  // A file of this size that takes no room on the disk (FSCTL_SET_SPARSE), as big as a film.
  public static void Sparse(string path, long length) {
    using (var f = new System.IO.FileStream(path, System.IO.FileMode.Create, System.IO.FileAccess.Write, System.IO.FileShare.None)) {
      uint returned;
      if (!DeviceIoControl(f.SafeFileHandle, 0x000900C4, IntPtr.Zero, 0, IntPtr.Zero, 0, out returned, IntPtr.Zero)) throw new System.ComponentModel.Win32Exception();
      f.SetLength(length);
    }
  }
  [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
  static extern Microsoft.Win32.SafeHandles.SafeFileHandle CreateFileW(string name, uint access, uint share, IntPtr security, uint disposition, uint flags, IntPtr template);
  [DllImport("cldapi.dll")]
  static extern int CfDehydratePlaceholder(Microsoft.Win32.SafeHandles.SafeFileHandle file, long offset, long length, uint flags, IntPtr overlapped);
  public static int Dehydrate(string path) {
    using (var h = CreateFileW(path, 0x180, 7, IntPtr.Zero, 3, 0x02000000, IntPtr.Zero)) {
      if (h.IsInvalid) return Marshal.GetHRForLastWin32Error();
      return CfDehydratePlaceholder(h, 0, -1, 0, IntPtr.Zero);
    }
  }
}
'@

# A placeholder freed by another process (Storage Sense's way): 'freed', or the HRESULT the system answered.
function CfDehydrate([string]$path) {
  $hr = [QaCloud]::Dehydrate($path)
  if ($hr -eq 0) { 'freed' } else { 'refused 0x{0:X8}' -f $hr }
}

function Fail([string]$why) { throw $why }

function WaitFor([scriptblock]$test, [int]$ms, [string]$what) {
  $clock = [Diagnostics.Stopwatch]::StartNew()
  while ($clock.ElapsedMilliseconds -lt $ms) {
    $r = & $test
    if ($r) { return $r }
    Start-Sleep -Milliseconds 100
  }
  Fail "timed out after $ms ms waiting for $what"
}

# A file's text for a check that polls it, or nothing while it is not there or busy: a read never ends a check, and
# never stands in the core's way (it shares read, write and delete). Get-Content opens without sharing delete, so it can
# meet the core replacing the file ("being used by another process", which ends the check in Windows PowerShell).
function ReadText([string]$path) {
  try {
    $s = [IO.FileStream]::new($path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]"ReadWrite, Delete")
    try { [IO.StreamReader]::new($s).ReadToEnd() } finally { $s.Dispose() }
  } catch { $null }
}

# A temporary state: its own folders, .torrent files left where they are, new torrents held ten minutes (no tracker is
# asked), no network beyond its own port (no DHT, PEX, LSD, UPnP or NAT-PMP: nothing reaches the router or the
# Internet), agents read-only (the checks read the core through the CLI).
function New-State([string]$permission = 'read_only') {
  $dir = Join-Path ([IO.Path]::GetTempPath()) "seedwell-qa-$([Guid]::NewGuid().ToString('N').Substring(0, 8))"
  New-Item -ItemType Directory -Force "$dir\state", "$dir\dl" | Out-Null
  @"
[transfers]
start_delay_secs = 600

[network]
upnp = false
natpmp = false
dht = false
pex = false
lsd = false
listen_port = 0

[folders]
download_dir = '$dir\dl'
incomplete_dir = '$dir\dl\.incomplete'
torrent_after_add = "leave"
# No watch folder: without one the app watches Downloads, the user's real one (and an isolated run is refused).
watch = []

[agents]
permission = "$permission"
"@ | Set-Content -Encoding utf8 "$dir\state\config.toml"
  $dir
}

function Log([string]$dir) {
  $f = "$dir\state\logs\app.log"
  if (Test-Path $f) { @(Get-Content $f -Encoding utf8) } else { @() }
}

# The core's standard error (its eprintln! lines and panics), which an app with no console sends to core.log.
function CoreLog([string]$dir) {
  $f = "$dir\state\logs\core.log"
  if (Test-Path $f) { @(Get-Content $f -Encoding utf8) } else { @() }
}

function List([string]$dir) { @(& $Cli --socket "$dir\state\control.sock" list 2>&1 | ForEach-Object { "$_" }) }

# Starts an instance on the state (quiet with -QaControl) and waits until its core answers.
function Start-App([string]$dir, [switch]$QaControl, [string[]]$extra = @()) {
  $args = @('--state-dir', "$dir\state") + $(if ($QaControl) { @('--qa-control') } else { @() }) + $extra
  $p = Start-Process $Exe -ArgumentList $args -PassThru
  $script:cliSaid = ''
  try {
    WaitFor { $script:cliSaid = (& $Cli --socket "$dir\state\control.sock" status 2>&1 | Out-String).Trim(); $LASTEXITCODE -eq 0 } 20000 'the core to answer' | Out-Null
  } catch {
    # Say why, so a failure on a machine nobody can look at (CI) can be read from its log: whether the app is still
    # running or ended (and how), what the CLI last said, and the end of the app's own log.
    $state = if ($p.HasExited) { "it ended with exit code $($p.ExitCode)" } else { 'it is still running' }
    $tail = (Log $dir | Select-Object -Last 8) -join ' | '
    # Never leave an instance behind: one that does not answer is ended here.
    if (-not $p.HasExited) { $p.Kill(); $p.WaitForExit(5000) | Out-Null }
    Fail "the core did not answer within 20 s: $state; the CLI said: $script:cliSaid; app.log: $(if ($tail) { $tail } else { '(none)' })"
  }
  $p
}

# A second launch with these words; returns how long it took to hand over and end.
function Launch([string]$dir, [string[]]$words) {
  $clock = [Diagnostics.Stopwatch]::StartNew()
  Start-Process $Exe -ArgumentList (@('--state-dir', "$dir\state") + $words) -Wait
  $clock.ElapsedMilliseconds
}

# A --qa command, then its line in app.log (the acknowledgement: the command has run).
function Qa([string]$dir, [string[]]$words, [int]$ms = 10000) {
  $before = (Log $dir).Count
  Launch $dir (@('--qa') + $words) | Out-Null
  # (The app's QaSaid: what a credential field is given never reaches the log.)
  $said = if ($words[0] -eq 'proxy-window' -and $words[1] -in 'fill', 'custom', 'copied', 'last') { "proxy-window $($words[1]) [*][*][*]" } else { $words -join ' ' }
  $pattern = "qa ${said}:"
  # (Not "waits for the core": the app is still making what needs the core, and the command's own line comes after.)
  WaitFor { Log $dir | Select-Object -Skip $before | Where-Object { ($_ -like "*$pattern*" -and $_ -notlike '*: waits for the core*') -or $_ -like '*qa refused*' } | Select-Object -Last 1 } $ms "'$pattern' in app.log"
}

# Folder sync starts in the background, a moment after the core answers.
function WaitForSync([string]$dir) {
  WaitFor { & $Cli --socket "$dir\state\control.sock" sync settings 2>&1 | Out-Null; $LASTEXITCODE -eq 0 } 20000 'folder sync to start' | Out-Null
}

function Stop-App([string]$dir, $p) {
  if ($p -and -not $p.HasExited) {
    Launch $dir @('--quit') | Out-Null
    if (-not $p.WaitForExit(30000)) { $p.Kill(); Fail 'the app did not quit within 30 s' }
  }
  # The app never hangs: a stall of the core's fail_ms in the run fails it (HangWatch's line in app.log).
  $hang = Log $dir | Where-Object { $_ -like '*ui stall:*this run fails*' -or $_ -like '*ui stutter:*this run fails*' } | Select-Object -First 1
  if ($hang) {
    # A debug core (CI's) says which of folder sync's locks anything waited for.
    $waits = @(CoreLog $dir | Where-Object { $_ -like '*waited * ms for a lock at*' } | Select-Object -Last 8) -join ' | '
    Fail "the UI thread stalled: $hang$(if ($waits) { " || the core: $waits" })"
  }
  # The temp config must be taken whole: a value the core replaced would undo the offline settings above.
  $problem = Log $dir | Where-Object { $_ -like '*settings problem:*' } | Select-Object -First 1
  if ($problem) { Fail "the temp config has a problem: $problem" }
}

$fixture = @{
  debian = Join-Path $Fixtures 'debian-13.7.0-amd64-netinst.iso.torrent'
  ubuntu = Join-Path $Fixtures 'ubuntu-26.04.1-desktop-amd64.iso.torrent'
  fedora = Join-Path $Fixtures 'Fedora-Workstation-Live-x86_64-44.torrent'
}

# ---------------------------------------------------------------------------------------------- checks
# Each returns its pictures (paths) and fails by throwing.

$checks = @{}

$checks.render = {
  $dir = Join-Path $Out 'render'
  $pictures = @()
  foreach ($theme in 'dark', 'light') {
    $p = Start-Process $Exe -ArgumentList '--render-snapshots', $dir, '--theme', $theme -PassThru
    $front = 0
    while (-not $p.HasExited) { if ([QaForeground]::Pid() -eq $p.Id) { $front++ }; Start-Sleep -Milliseconds 50 }
    # With every stall of the run (its log, beside the pictures), so a failure on CI names the step.
    if ($p.ExitCode -ne 0) { Fail "the $theme render ended with $($p.ExitCode): $(Get-Content "$dir\error-$theme.txt" -TotalCount 1) || $((Get-Content "$dir\logs\app.log" -ErrorAction SilentlyContinue | Where-Object { $_ -like '*ui stall*' -or $_ -like '*ui stutter*' }) -join ' || ')" }
    if ($front -gt 0) { Fail "the $theme render held the foreground ($front samples)" }
  }
  $pictures = @(Get-ChildItem $dir -Filter *.png | ForEach-Object FullName)
  foreach ($must in 'library-files-dark', 'library-files-light', 'menu-files-dark', 'library-chat-notes-dark', 'library-chat-notes-light', 'library-chat-hover-light', 'library-chat-reply-dark', 'chat-images-light', 'chat-gallery-dark', 'menu-dark', 'menu-sync-hover-dark', 'menu-sync-hover-light', 'menu-sync-confirm-light', 'row-menu-dark', 'remove-light', 'library-downloads-dark', 'library-info-light', 'library-removal-dark', 'library-foldersync-light', 'library-settings-dark', 'channel-menu-light', 'tray-menu-dark', 'new-channel-light', 'new-channel-join-dark', 'dialog-new-topic-dark', 'dialog-leave-light', 'dialog-agents-dark', 'library-foldersync-dark', 'foldersync-remove-light', 'add-folder-dark', 'add-folder-shared-light', 'edit-folder-dark', 'share-light', 'library-chat-dark', 'library-direct-light', 'chat-menu-dark', 'chat-files-light', 'prompt-add-request-dark', 'prompt-location-move-light', 'prompt-backlog-dark', 'settings-general-dark', 'settings-transfers-light', 'settings-folders-dark', 'settings-network-light', 'settings-agents-dark', 'settings-advanced-light', 'file-icons-dark', 'tray-both-32') {
    if (-not (Test-Path "$dir\$must.png")) { Fail "missing $must.png" }
  }
  $pictures
}

$checks.handoff = {
  $dir = New-State
  $p = Start-App $dir
  try {
    $ms = Launch $dir @($fixture.debian)
    if ($ms -gt 5000) { Fail "the second launch took $ms ms to hand over" }
    WaitFor { List $dir | Where-Object { $_ -match 'pending' -and $_ -match 'Debian' } } 10000 'the added torrent, held' | Out-Null
    WaitFor { Log $dir | Where-Object { $_ -like '*second launch: 1 to open*' } } 5000 'the hand-off in app.log' | Out-Null
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @()
}

# Instances on different state folders never talk: with a stand-in for the user's app running on its own state, a test
# instance's --qa, .torrent hand-off and --quit reach only the test instance (the stand-in keeps running and its log
# gets nothing). The same folder written with a trailing \ is the same instance.
$checks.separate = {
  $real = New-State
  $test = New-State
  $pr = Start-App $real -QaControl
  $pt = $null
  try {
    $pt = Start-App $test -QaControl
    $before = (Log $real).Count
    Qa $test @('window') | Out-Null
    Launch $test @($fixture.debian) | Out-Null
    WaitFor { List $test | Where-Object { $_ -match 'Debian' } } 10000 'the test instance to take the torrent' | Out-Null
    Launch $test @('--quit') | Out-Null
    if (-not $pt.WaitForExit(30000)) { Fail 'the test instance did not quit' }
    if ($pr.HasExited) { Fail 'the stand-in for the user''s app quit' }
    # The test's commands in the stand-in's log, not its own lines (it goes on starting: "on-demand folders: 0 ()").
    $reached = @(Log $real | Select-Object -Skip $before | Where-Object { $_ -match 'qa window|quit:' })
    if ($reached.Count) { Fail "the stand-in heard the test: $($reached -join ' | ')" }
    if (List $real | Where-Object { $_ -match 'Debian' }) { Fail 'the torrent went to the stand-in' }
    # The stand-in's folder with a trailing \: a hand-off to it, not a second instance on the same state.
    $launch = Start-Process $Exe -ArgumentList '--state-dir', "$real\state\", $fixture.ubuntu -PassThru
    if (-not $launch.WaitForExit(10000)) { $launch.Kill(); Fail 'a launch with a trailing \ did not hand over (a second instance)' }
    WaitFor { List $real | Where-Object { $_ -match 'Ubuntu' } } 10000 'the stand-in to take the torrent' | Out-Null
  } finally {
    if ($pt -and -not $pt.HasExited) { Stop-App $test $pt }
    Stop-App $real $pr
    Remove-Item -Recurse -Force $real, $test
  }
  @()
}

$checks.gate = {
  $dir = New-State
  $p = Start-App $dir -extra @($fixture.debian)
  try {
    WaitFor { List $dir | Where-Object { $_ -match 'Debian' } } 10000 'the torrent' | Out-Null
    $line = Qa $dir @('remove', 'debian')
    if ($line -notlike '*qa refused*') { Fail "a --qa command ran without --qa-control: $line" }
    Qa $dir @('confirm') | Out-Null
    if (-not (List $dir | Where-Object { $_ -match 'Debian' })) { Fail 'the torrent was removed through a refused --qa' }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @()
}

$checks.selection = {
  $dir = New-State
  $p = Start-App $dir -QaControl -extra @($fixture.debian, $fixture.ubuntu)
  try {
    WaitFor { (List $dir | Where-Object { $_ -match 'pending' }).Count -ge 2 } 10000 'both torrents' | Out-Null
    Qa $dir @('window') | Out-Null
    $one = Qa $dir @('rowclick', 'debian')
    if ($one -notmatch 'selection \[[0-9a-f]{40}\]') { Fail "a click did not select one row: $one" }
    $two = Qa $dir @('rowclick', 'ubuntu', 'command')
    if ($two -notmatch 'selection \[[0-9a-f]{40}, [0-9a-f]{40}\]') { Fail "a Ctrl-click did not add a row: $two" }
    $back = Qa $dir @('rowclick', 'ubuntu')
    if ($back -notmatch 'selection \[[0-9a-f]{40}\]') { Fail "a plain click did not narrow to one row: $back" }
    Qa $dir @('shot', 'selection') | Out-Null
    Copy-Item "$dir\state\qa\selection.png" "$Out\selection.png"
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @("$Out\selection.png")
}

# Keys through the Library (the window's handler calls the same), in the list as when it has the focus: Down moves the
# selection to the next row, Up back, Ctrl+A selects every row.
$checks.keys = {
  $dir = New-State
  $p = Start-App $dir -QaControl -extra @($fixture.debian, $fixture.ubuntu)
  try {
    WaitFor { (List $dir | Where-Object { $_ -match 'pending' }).Count -ge 2 } 10000 'both torrents' | Out-Null
    Qa $dir @('window') | Out-Null
    $one = Qa $dir @('rowclick', 'debian')
    if ($one -notmatch 'selection \[([0-9a-f]{40})\]') { Fail "a click did not select one row: $one" }
    $first = $Matches[1]
    $down = Qa $dir @('key', 'down', 'list')
    if ($down -notmatch 'handled True.*selection \[([0-9a-f]{40})\]' -or $Matches[1] -eq $first) { Fail "Down did not move the selection: $down" }
    $up = Qa $dir @('key', 'up', 'list')
    if ($up -notmatch "selection \[$first\]") { Fail "Up did not move it back: $up" }
    $all = Qa $dir @('key', 'a', 'command', 'list')
    if ($all -notmatch 'selection \[[0-9a-f]{40}, [0-9a-f]{40}\]') { Fail "Ctrl+A did not select both rows: $all" }
    Qa $dir @('shot', 'keys') | Out-Null
    Copy-Item "$dir\state\qa\keys.png" "$Out\keys.png"
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @("$Out\keys.png")
}

$checks.removal = {
  $dir = New-State
  $p = Start-App $dir -QaControl -extra @($fixture.debian)
  try {
    WaitFor { List $dir | Where-Object { $_ -match 'Debian' } } 10000 'the torrent' | Out-Null
    Qa $dir @('window') | Out-Null
    $asked = Qa $dir @('remove', 'debian')
    if ($asked -notmatch 'removal [0-9a-f]{40}: ') { Fail "no question under the row: $asked" }
    Qa $dir @('shot', 'removal') | Out-Null
    Copy-Item "$dir\state\qa\removal.png" "$Out\removal.png"
    $cancel = Qa $dir @('cancel')
    if ($cancel -notmatch 'removal none') { Fail "Cancel left the question: $cancel" }
    if (-not (List $dir | Where-Object { $_ -match 'Debian' })) { Fail 'Cancel removed the torrent' }
    Qa $dir @('remove', 'debian') | Out-Null
    Qa $dir @('confirm') | Out-Null
    WaitFor { -not (List $dir | Where-Object { $_ -match 'Debian' }) } 10000 'the torrent to be removed' | Out-Null
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @("$Out\removal.png")
}

# The failure: something made over and over is never let go, and the app grows for as long as it runs (0.1.20: a timer for
# each text field of each Settings draw and for Cancel's Shift caption in each of the notifier's draws, each keeping itself
# alive with the draw's tree; 15.8 GB and 232,000 handles after 22 hours). What such a timer holds is counted by type (Timer and
# WaitCompletionPacket, one of each for each): the handle total also holds what the thread pool keeps for a while. After a
# collection, as what can still be collected is no leak. Each surface opened and closed again and again, Settings drawn again
# and again, and each surface left alone. Every miss is printed, not only the first.
$checks.leaks = {
  $dir = New-State 'control'
  New-Item -ItemType Directory -Force "$dir\Shared" | Out-Null
  $p = Start-App $dir -QaControl -extra @($fixture.debian)
  $misses = @()
  try {
    WaitForSync $dir
    WaitFor { List $dir | Where-Object { $_ -match 'Debian' } } 10000 'the torrent' | Out-Null
    & $Cli --socket "$dir\state\control.sock" sync add "$dir\Shared" | Out-Null
    if ($LASTEXITCODE -ne 0) { Fail "the CLI could not add a folder (exit $LASTEXITCODE)" }
    Qa $dir @('window') | Out-Null
    Qa $dir @('favorite', 'save', 'debian') | Out-Null
    function Timers { [QaHandles]::Timers($p.Id) }
    function Held { $s = Qa $dir @('memory'); $m = [regex]::Match($s, 'handles (\d+) private (\d+) managed (\d+)'); if (-not $m.Success) { Fail "memory said: $s" }; [pscustomobject]@{ Timers = (Timers); Handles = [int]$m.Groups[1].Value; Private = [int]$m.Groups[2].Value; Managed = [int]$m.Groups[3].Value } }
    $tabs = 'General', 'Transfers', 'Folders', 'Rules', 'Network', 'FolderSync', 'Devices', 'Agents', 'Advanced'
    $rounds = [ordered]@{
      'the pages' = @(@('place', 'downloads'), @('place', 'sharing'), @('place', 'foldersync'), @('syncpage'), @('place', 'settings'))
      'the Settings tabs' = @($tabs | ForEach-Object { , @('settings', 'tab', $_) })
      'the sync flyout' = @(@('sync-flyout', 'open'), @('sync-flyout', 'close'))
      'the Get Info card' = @(@('place', 'downloads'), @('info', 'debian'))
      'the tracker page' = @(@('place', 'downloads'), @('tracker-info', 'debian'))
      'the Add window' = @(@('add-folder', 'open'), @('add-folder', 'cancel'))
      'the Edit window' = @(@('edit-folder', 'open', 'Shared'), @('edit-folder', 'cancel'))
      'the Share window' = @(@('share', 'open', 'Shared'), @('share', 'cancel'))
      'the proxy window' = @(@('proxy-window', 'open'), @('proxy-window', 'close'))
    }
    # First visits make what stays for the app's life (a page, a window's resources): these are not what is measured.
    foreach ($steps in $rounds.Values) { foreach ($s in $steps) { Qa $dir $s | Out-Null } }

    # Each surface alone for a while: what leaks by itself, as nobody clicks for 22 hours. 0.1.20 made 2.9 of these a second.
    # First, while the torrent is still in its start countdown: the notifier's window, drawn at every tick, goes when that ends.
    $idle = @(
      @{ name = 'Downloads'; go = @(, @('place', 'downloads')) }
      @{ name = 'Sharing'; go = @(, @('place', 'sharing')) }
      @{ name = 'Folder Sync'; go = @(, @('place', 'foldersync')) }
    ) + @($tabs | ForEach-Object { @{ name = "Settings, $_"; go = @(@('place', 'settings'), @('settings', 'tab', $_)) } }) + @(
      @{ name = 'the sync flyout'; go = @(@('place', 'downloads'), @('sync-flyout', 'open')) }
      @{ name = 'the Get Info card'; go = @(@('sync-flyout', 'close'), @('info', 'debian')) }
    )
    $seconds = 10
    $first = Held
    foreach ($s in $idle) {
      foreach ($w in $s.go) { Qa $dir $w | Out-Null }
      Start-Sleep 2
      $from = Timers
      Start-Sleep $seconds
      $to = Timers
      Write-Host "  leaks: left on $($s.name) for $seconds s: timers $from to $to"
      if ($to - $from -ge 6) { $misses += "left on $($s.name) for $seconds s, timer handles climbed from $from to $to" }
    }
    $last = Held
    Write-Host "  leaks: over all the idling: timers $($first.Timers) to $($last.Timers), handles $($first.Handles) to $($last.Handles), private $($first.Private) to $($last.Private) MB, managed $($first.Managed) to $($last.Managed) MB"
    if ($last.Private - $first.Private -ge 40) { $misses += "private bytes climbed from $($first.Private) to $($last.Private) MB while nobody did anything" }

    # Under one a round: what each round leaves behind is at least one, and a self-holding timer two (and the stream of a
    # second or so a round, whichever surface is up).
    $times = 8
    foreach ($name in $rounds.Keys) {
      $before = Held
      foreach ($i in 1..$times) { foreach ($s in $rounds[$name]) { Qa $dir $s | Out-Null } }
      $after = Held
      $grew = $after.Timers - $before.Timers
      Write-Host "  leaks: $name, $times rounds: timers $($before.Timers) to $($after.Timers), handles $($before.Handles) to $($after.Handles), managed $($before.Managed) to $($after.Managed) MB"
      # The count wanders by some ten either way with nothing leaking (a first visit's, the system's own). A leak does it again.
      if ($grew -ge $times) {
        $longer = $times * 3
        $before = Held
        foreach ($i in 1..$longer) { foreach ($s in $rounds[$name]) { Qa $dir $s | Out-Null } }
        $after = Held
        $again = $after.Timers - $before.Timers
        Write-Host "  leaks: $name again, $longer rounds: timers $($before.Timers) to $($after.Timers)"
        if ($again -ge $longer) { $misses += "$name left $grew timer handles behind in $times rounds and $again in $longer more" }
      }
    }

    # Settings drawn anew: what a change in the model does, on the tab with the most fields. (These draws are of one model, so
    # the idle part below does not see them on code that keeps what it drew.)
    Qa $dir @('place', 'settings') | Out-Null
    Qa $dir @('settings', 'tab', 'Transfers') | Out-Null
    Qa $dir @('settings', 'redraw', '3') | Out-Null
    $before = Held
    Qa $dir @('settings', 'redraw', '40') 60000 | Out-Null
    $after = Held
    $grew = $after.Timers - $before.Timers
    Write-Host "  leaks: 40 Settings draws: timers $($before.Timers) to $($after.Timers), handles $($before.Handles) to $($after.Handles), managed $($before.Managed) to $($after.Managed) MB"
    if ($grew -ge 40) { $misses += "40 draws of Settings left $grew timer handles behind" }

  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  if ($misses) { Fail ($misses -join ' | ') }
}

$checks.info = {
  $dir = New-State
  $p = Start-App $dir -QaControl -extra @($fixture.debian)
  try {
    WaitFor { List $dir | Where-Object { $_ -match 'Debian' } } 10000 'the torrent' | Out-Null
    Qa $dir @('window') | Out-Null
    Qa $dir @('info', 'debian') | Out-Null
    WaitFor { Log $dir | Where-Object { $_ -like '*info: shown for*' } } 5000 'the Get Info card' | Out-Null
    Qa $dir @('shot', 'info') | Out-Null
    Copy-Item "$dir\state\qa\info.png" "$Out\info.png"
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @("$Out\info.png")
}

$checks.alert = {
  $dir = New-State
  $p = Start-App $dir -QaControl
  try {
    Launch $dir @("$dir\missing.torrent") | Out-Null
    $line = WaitFor { Log $dir | Where-Object { $_ -like '*alert: *' } | Select-Object -Last 1 } 10000 'the alert'
    if ($line -notmatch 'File not found') { Fail "the alert does not say the file is missing: $line" }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @()
}

# An isolated run (--state-dir) trashes into its own state: Cancel on a held torrent moves its .torrent file to
# <state>\Trash, and the Recycle Bin is the same before and after. Then a config with a folder in the user's profile
# (outside the temp folder) is refused: the app logs it and ends.
$checks.trash = {
  $dir = New-State
  Copy-Item $fixture.debian "$dir\held.torrent"
  $bin = (New-Object -ComObject Shell.Application).Namespace(10)
  $before = @($bin.Items() | ForEach-Object { $_.Path }) -join "`n"
  $p = Start-App $dir -QaControl -extra @("$dir\held.torrent")
  try {
    WaitFor { List $dir | Where-Object { $_ -match 'Debian' } } 10000 'the torrent' | Out-Null
    Qa $dir @('window') | Out-Null
    Qa $dir @('shot', 'held') | Out-Null
    Copy-Item "$dir\state\qa\held.png" "$Out\held.png"
    $done = Qa $dir @('control', 'debian', 'CancelHeld')
    if ($done -match 'no such') { Fail "no Cancel on the held row: $done" }
    WaitFor { -not (List $dir | Where-Object { $_ -match 'Debian' }) } 10000 'the torrent to be cancelled' | Out-Null
    if (Test-Path "$dir\held.torrent") { Fail 'the .torrent file was not moved' }
    if (-not (Get-ChildItem "$dir\state\Trash" -Recurse -Filter '*.torrent' -ErrorAction SilentlyContinue)) { Fail "the .torrent file is not in <state>\Trash" }
    $after = @($bin.Items() | ForEach-Object { $_.Path }) -join "`n"
    if ($after -ne $before) { Fail 'the Recycle Bin changed' }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  $dir = New-State
  try {
    $cfg = "$dir\state\config.toml"
    (Get-Content $cfg -Raw) -replace "download_dir = '[^']*'", "download_dir = '$env:USERPROFILE\Downloads'" | Set-Content -NoNewline -Encoding utf8 $cfg
    $q = Start-Process $Exe -ArgumentList '--state-dir', "$dir\state" -PassThru
    if (-not $q.WaitForExit(20000)) { $q.Kill(); Fail 'a run with a folder in the profile was not refused' }
    if (-not (Log $dir | Where-Object { $_ -like '*launch refused:*' })) { Fail 'no "launch refused" in app.log' }
  } finally { Remove-Item -Recurse -Force $dir }
  @("$Out\held.png")
}

# A torrent that waits for a choice of files: Start (the row's own control) starts it
# with the core's default selection, and nothing waits for a choice any more.
$checks.choice = {
  $dir = New-State
  # "Ask before adding: always": the fixture (an ISO and its checksum) holds for a choice (the default asks only when
  # there is a real one: several episodes or films).
  $cfg = "$dir\state\config.toml"
  (Get-Content $cfg -Raw) -replace 'start_delay_secs = 600', "start_delay_secs = 600`nask_before_adding = `"always`"" | Set-Content -NoNewline -Encoding utf8 $cfg
  # Start makes it announce: a copy whose tracker is a closed port on loopback (the info hash is the same), never
  # Fedora's.
  $latin = [Text.Encoding]::GetEncoding(28591)
  $text = $latin.GetString([IO.File]::ReadAllBytes($fixture.fedora))
  $tracker = 'http://127.0.0.1:9/announce'
  $text = $text -replace '^d8:announce\d+:https?://[^/]+/announce', "d8:announce$($tracker.Length):$tracker"
  if ($text -notlike "*$tracker*") { Fail 'the tracker of the copy was not replaced' }
  [IO.File]::WriteAllBytes("$dir\fedora.torrent", $latin.GetBytes($text))
  $p = Start-App $dir -QaControl -extra @("$dir\fedora.torrent")
  try {
    WaitFor { List $dir | Where-Object { $_ -match 'Fedora' } } 10000 'the torrent' | Out-Null
    $held = (List $dir) -join ' | '
    if ($held -notmatch 'awaiting-choice') { Fail "it does not wait for a choice: $held" }
    Qa $dir @('window') | Out-Null
    Qa $dir @('shot', 'choice') | Out-Null
    Copy-Item "$dir\state\qa\choice.png" "$Out\choice.png"
    $done = Qa $dir @('control', 'fedora', 'ConfirmChoice')
    if ($done -match 'no such') { Fail "no Start on the held row: $done" }
    $started = WaitFor { $now = (List $dir) -join ' | '; if ($now -ne $held) { $now } } 10000 'the torrent to start'
    $trackers = (& $Cli --socket "$dir\state\control.sock" info fedora | Select-String -Pattern 'https?://' | ForEach-Object { "$_".Trim() }) -join '; '
    if ($trackers -notmatch '127\.0\.0\.1:9' -or $trackers -match 'fedoraproject') { Fail "its trackers: $trackers" }
    if (Log $dir | Where-Object { $_ -like '*placeholder:*' }) { Fail "a placeholder opened: $(Log $dir | Where-Object { $_ -like '*placeholder:*' } | Select-Object -Last 1)" }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  # The core's own guard: an isolated run never reaches a public tracker. The unmodified fixture, started, is refused.
  $dir = New-State
  $cfg = "$dir\state\config.toml"
  (Get-Content $cfg -Raw) -replace 'start_delay_secs = 600', 'start_delay_secs = 0' | Set-Content -NoNewline -Encoding utf8 $cfg
  $p = Start-App $dir -QaControl -extra @($fixture.fedora)
  try {
    # Refused by the destination policy (nothing was sent), with the core's reason, and never answered.
    $refused = WaitFor { $i = (& $Cli --socket "$dir\state\control.sock" debug trackers) -join ' | '; if ($i -match [regex]::Escape('tracker address not allowed: public address (an isolated test run reaches only local trackers)')) { $i } } 15000 'the public tracker to be refused'
    if ($refused -notmatch 'answered never') { Fail "the tracker answered: $refused" }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @("$Out\choice.png")
}

# Move to…: the folder the dialog would answer goes to move_to. The torrent is held (it waits ten minutes), so the core
# refuses it with its reason, which is logged (NEED: shown, once Rust has the alert).
$checks.move = {
  $dir = New-State
  New-Item -ItemType Directory -Force "$dir\moved" | Out-Null
  $p = Start-App $dir -QaControl -extra @($fixture.debian)
  try {
    WaitFor { List $dir | Where-Object { $_ -match 'Debian' } } 10000 'the torrent' | Out-Null
    Qa $dir @('window') | Out-Null
    Qa $dir @('move', 'debian', "$dir\moved") | Out-Null
    $line = WaitFor { Log $dir | Where-Object { $_ -like '*move to *' -or $_ -like '*move failed:*' } | Select-Object -Last 1 } 5000 'the move in app.log'
    if ($line -notmatch 'move to .*moved: (1 queued, 0 refused|0 queued, 1 refused)') { Fail "the move did not reach the core: $line" }
    if ($line -match '1 refused' -and -not (Log $dir | Where-Object { $_ -like '*move refused: ?*' })) { Fail 'a refusal without its reason' }
    if (Log $dir | Where-Object { $_ -like '*placeholder:*' }) { Fail 'a placeholder opened' }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @()
}

# Add Torrent File… (Ctrl+O) opens the Open dialog (logged in a QA run); the flyout's "+N more" and Sharing lines open
# their places in the window.
$checks.add = {
  $dir = New-State
  $p = Start-App $dir -QaControl
  try {
    Qa $dir @('add-file') | Out-Null
    WaitFor { Log $dir | Where-Object { $_ -like '*open dialog: .torrent files*' } } 5000 'the Open dialog' | Out-Null
    $sharing = Qa $dir @('menu-line', 'sharing')
    if ($sharing -notmatch 'place Sharing') { Fail "the Sharing line did not open Sharing: $sharing" }
    $more = Qa $dir @('menu-line', 'more')
    if ($more -notmatch 'place Downloads') { Fail "+N more did not open Downloads: $more" }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @()
}

# A channel's sidebar menu: Mute and Unmute, and Agents No and Yes (this PC made it: its owner). The channel is made
# through the CLI on the temp state (agents may control there) and never leaves it (no network).
$checks.chat = {
  $dir = New-State 'control'
  $p = Start-App $dir -QaControl
  try {
    WaitForSync $dir
    & $Cli --socket "$dir\state\control.sock" chat new 'QA-Channel' | Out-Null
    if ($LASTEXITCODE -ne 0) { Fail "the CLI could not make a channel (exit $LASTEXITCODE)" }
    Qa $dir @('window') | Out-Null
    $muted = Qa $dir @('sidebar', 'qa-channel', 'Mute')
    if ($muted -notmatch 'channel muted True') { Fail "Mute did not mute: $muted" }
    # The sidebar follows a change the core makes in place (the same items, a muted icon).
    if ($muted -notmatch 'drawn muted True') { Fail "the sidebar still draws the channel unmuted: $muted" }
    Qa $dir @('shot', 'chat-muted') | Out-Null
    Copy-Item "$dir\state\qa\chat-muted.png" "$Out\chat-muted.png"
    $unmuted = Qa $dir @('sidebar', 'qa-channel', 'Unmute')
    if ($unmuted -notmatch 'channel muted False, .*drawn muted False') { Fail "Unmute did not unmute: $unmuted" }
    # Agents: Yes… asks first (the core's chat_agents_dialog), then switches it the other way.
    $asked = Qa $dir @('sidebar', 'qa-channel', 'ChannelAgents')
    if ($asked -notmatch 'agents True') { Fail "the question changed the channel before it was answered: $asked" }
    $confirmed = Qa $dir @('dialog', 'confirm')
    if ($confirmed -notmatch 'dialog none') { Fail "the Agents question did not close: $confirmed" }
    $closed = Qa $dir @('sidebar', 'qa-channel', 'Mute')
    if ($closed -notmatch 'agents False') { Fail "Agents: No did not close it: $closed" }
    Qa $dir @('sidebar', 'qa-channel', 'Unmute') | Out-Null
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @("$Out\chat-muted.png")
}

# The chat dialogs from a channel's sidebar menu, each drawn from the core's ChatDialog and checked by
# chat_dialog_check: New Topic (empty: Start disabled; a title: the topic is made and opens), Rename (the core's name
# check, then renamed), Wake Command (saved), Leave (the channel is gone). The channel is made through the CLI on the
# temp state and never leaves it.
$checks.dialogs = {
  $dir = New-State 'control'
  $p = Start-App $dir -QaControl
  try {
    WaitForSync $dir
    & $Cli --socket "$dir\state\control.sock" chat new 'QA-Channel' | Out-Null
    if ($LASTEXITCODE -ne 0) { Fail "the CLI could not make a channel (exit $LASTEXITCODE)" }
    Qa $dir @('window') | Out-Null
    function Shot([string]$name) { Qa $dir @('shot', $name, 'dialog') | Out-Null; Copy-Item "$dir\state\qa\$name.png" "$Out\$name.png" }
    $topic = Qa $dir @('sidebar', 'qa-channel', 'NewTopic')
    if ($topic -match 'no such entry') { Fail "no New Topic… in the channel menu: $topic" }
    $empty = Qa $dir @('dialog', 'confirm')
    if ($empty -notmatch 'not confirmed') { Fail "an empty topic title could start: $empty" }
    Qa $dir @('dialog', 'type', 'Plans') | Out-Null
    Shot 'dialog-new-topic'
    Qa $dir @('dialog', 'confirm') | Out-Null
    # The topic is made off the UI thread (build 42): it opens once the core has made it.
    WaitFor { $s = Qa $dir @('window'); if ($s -match 'place Chat \{ ChannelId = [^,]+, TopicId = \S') { $s } } 10000 'the new topic to open' | Out-Null
    $topics = (& $Cli --socket "$dir\state\control.sock" chat topics 'QA-Channel' 2>&1) -join ' '
    if ($topics -notmatch 'Plans') { Fail "the topic was not made: $topics" }
    Qa $dir @('sidebar', 'qa-channel', 'RenameChannel') | Out-Null
    Qa $dir @('dialog', 'type', 'Renamed') | Out-Null
    Shot 'dialog-rename'
    Qa $dir @('dialog', 'confirm') | Out-Null
    $names = (& $Cli --socket "$dir\state\control.sock" chat list 2>&1) -join ' '
    if ($names -notmatch 'Renamed') { Fail "the channel was not renamed: $names" }
    Qa $dir @('wake-command', 'renamed') | Out-Null
    Qa $dir @('dialog', 'type', 'cmd /c exit 0') | Out-Null
    Shot 'dialog-wake-command'
    Qa $dir @('dialog', 'confirm') | Out-Null
    if (-not (Log $dir | Where-Object { $_ -like '*chat dialog WakeCommand: done*' })) { Fail 'the wake command was not saved' }
    Qa $dir @('sidebar', 'renamed', 'LeaveChannel') | Out-Null
    Shot 'dialog-leave'
    $left = Qa $dir @('dialog', 'confirm')
    if ($left -notmatch 'dialog none') { Fail "Leave did not close: $left" }
    WaitFor { -not ((& $Cli --socket "$dir\state\control.sock" chat list 2>&1) -join ' ' -match 'Renamed') } 5000 'the channel to be left' | Out-Null
    if (Log $dir | Where-Object { $_ -like '*placeholder*' }) { Fail 'a placeholder opened' }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @('dialog-new-topic', 'dialog-rename', 'dialog-wake-command', 'dialog-leave') | ForEach-Object { "$Out\$_.png" }
}

# The sidebar's groups of channels (SPEC 7G.7 Groups), made through the CLI as an agent would. The failures caught: a
# channel drawn both under its group and in the plain list, or left behind in its old place when it moves between groups
# (the Mac had a lingering row); a closed group that still draws its channels, or doesn't mark the selection it holds;
# the menus without Move to Group, its check, or the group's own commands; a group command or dialog that never reaches
# the core; a deleted group taking its channels with it. Shots: open, closed, holding the selection.
$checks.groups = {
  $dir = New-State 'control'
  $p = Start-App $dir -QaControl
  $pictures = New-Object Collections.Generic.List[string]
  try {
    WaitForSync $dir
    function GroupCli([string[]]$words) { $o = (& $Cli --socket "$dir\state\control.sock" @words 2>&1) -join ' '; if ($LASTEXITCODE -ne 0) { Fail "seedwell $($words -join ' ') failed ($LASTEXITCODE): $o" }; $o }
    function Rows { (Qa $dir @('sidebar-rows')) -replace '^.*?rows ', '' -replace '; place .*$', '' }
    # Each name drawn exactly once, at the indent given (0: the plain list, 1: under a group).
    function Once([string]$rows, [string]$name, [int]$indent) {
      $all = @($rows -split ' \| ' | Where-Object { $_.TrimStart('.') -eq $name })
      if ($all.Count -ne 1) { Fail "$name is drawn $($all.Count) times: $rows" }
      if ($all[0] -ne ('.' * $indent + $name)) { Fail "$name is drawn at the wrong place ($($all[0]), wanted indent $indent): $rows" }
    }
    function Absent([string]$rows, [string]$name) { if (@($rows -split ' \| ' | Where-Object { $_.TrimStart('.') -eq $name }).Count -ne 0) { Fail "$name is drawn: $rows" } }
    function Shot([string]$name) { Qa $dir @('shot', $name) | Out-Null; Copy-Item "$dir\state\qa\$name.png" "$Out\$name.png"; $pictures.Add("$Out\$name.png") }
    GroupCli @('chat', 'new', 'QA-Channel') | Out-Null
    GroupCli @('chat', 'new', 'QA-Other') | Out-Null
    Qa $dir @('window') | Out-Null
    $plain = WaitFor { $r = Rows; if ($r -match '(^| \| )QA-Other( \||$)' -and $r -match '(^| \| )QA-Channel( \||$)') { $r } } 5000 'the channels in the sidebar'
    if ($plain -match '\[group') { Fail "a group drawn before there is one: $plain" }
    Once $plain 'QA-Channel' 0
    # The CLI's group: Agents gets sparkles from its name.
    GroupCli @('chat', 'group', 'new', 'Agents') | Out-Null
    $listed = GroupCli @('chat', 'group', 'list')
    if ($listed -notmatch 'sparkles') { Fail "the CLI's Agents group has no sparkles: $listed" }
    GroupCli @('chat', 'group', 'move', 'QA-Channel', 'Agents') | Out-Null
    $open = WaitFor { $r = Rows; if ($r -match '\[group Agents open Sparkles\]' -and $r -match '\.QA-Channel') { $r } } 5000 'the channel under its group'
    Once $open 'QA-Channel' 1
    Once $open 'QA-Other' 0
    $groupAt = [array]::IndexOf(@($open -split ' \| '), '[group Agents open Sparkles]')
    $otherAt = [array]::IndexOf(@($open -split ' \| '), 'QA-Other')
    if ($groupAt -lt 0 -or $otherAt -lt $groupAt) { Fail "the groups are not drawn before the plain channels: $open" }
    Shot 'groups-open'
    # The menus: the channel's Move to Group with its group checked, and the group's own.
    $channelMenu = Qa $dir @('menu-of', 'channel', 'QA-Channel')
    if ($channelMenu -notmatch 'Move to Group > \[[^\]]*Agents \[x\]' -or $channelMenu -notmatch 'New Group' -or $channelMenu -notmatch 'Remove from Group') { Fail "the channel's Move to Group is not right: $channelMenu" }
    $otherMenu = Qa $dir @('menu-of', 'channel', 'QA-Other')
    if ($otherMenu -notmatch 'Move to Group > \[' -or $otherMenu -match 'Agents \[x\]' -or $otherMenu -match 'Remove from Group') { Fail "a channel in no group: its Move to Group is not right: $otherMenu" }
    $groupMenu = Qa $dir @('menu-of', 'group', 'Agents')
    if ($groupMenu -notmatch 'Rename Group' -or $groupMenu -notmatch 'Icon > \[[^\]]*Sparkles \[x\]' -or $groupMenu -notmatch 'Delete Group !') { Fail "the group's menu is not right: $groupMenu" }
    # Closed while it holds the channel shown: no channel rows, the selection marked; then closed holding nothing.
    Qa $dir @('place', 'chat', 'QA-Channel') | Out-Null
    $holding = (Qa $dir @('group-open', 'Agents', 'false')) -replace '^.*?rows ', ''
    if ($holding -notmatch '\[group Agents closed holds Sparkles\]') { Fail "the closed group does not hold the selection: $holding" }
    Absent $holding 'QA-Channel'
    Shot 'groups-holding'
    Qa $dir @('place', 'chat', 'QA-Other') | Out-Null
    $closed = Rows
    if ($closed -notmatch '\[group Agents closed Sparkles\]') { Fail "the closed group still holds a selection it hasn't: $closed" }
    Shot 'groups-closed'
    Qa $dir @('group-open', 'Agents', 'true') | Out-Null
    # The app's own commands: New Group… from a channel's menu (its dialog), Move to Group, the icon, the order, Rename.
    Qa $dir @('sidebar', 'qa-other', 'NewGroup') | Out-Null
    Qa $dir @('dialog', 'type', 'Work') | Out-Null
    Qa $dir @('dialog', 'confirm') | Out-Null
    $work = WaitFor { $r = Rows; if ($r -match '\[group Work open Briefcase\]' -and $r -match '\.QA-Other') { $r } } 5000 'New Group… to make Work with the channel'
    Once $work 'QA-Other' 1
    # A channel moved between groups: drawn once, under the new one (no row left in the old).
    $moved = Qa $dir @('sidebar', 'qa-channel', 'MoveToGroup', 'Work')
    $moved = WaitFor { $r = Rows; if (($r -split ' \| ' | Where-Object { $_ -eq '.QA-Channel' }).Count -eq 1 -and $r -match '\[group Agents open Sparkles\] \| \[group Work') { $r } } 5000 "the channel under Work only ($moved)"
    Once $moved 'QA-Channel' 1
    $icon = Qa $dir @('group', 'Work', 'SetGroupIcon', 'Star')
    if ($icon -notmatch '\[group Work open Star\]') { Fail "the icon did not change: $icon" }
    $up = Qa $dir @('group', 'Work', 'MoveGroupUp')
    if ($up -notmatch '\[group Work open Star\] \|.*\[group Agents') { Fail "Move Up did not move Work first: $up" }
    Qa $dir @('group', 'Work', 'RenameGroup') | Out-Null
    Qa $dir @('dialog', 'type', 'Projects') | Out-Null
    Qa $dir @('dialog', 'confirm') | Out-Null
    WaitFor { $r = Rows; if ($r -match '\[group Projects open Star\]') { $r } } 5000 'the group renamed' | Out-Null
    Qa $dir @('sidebar', 'qa-other', 'RemoveFromGroup') | Out-Null
    $removed = WaitFor { $r = Rows; if ($r -match '(^| \| )QA-Other( \||$)') { $r } } 5000 'QA-Other back in the plain list'
    Once $removed 'QA-Other' 0
    # Deleted: its channel back in the plain list, nothing lost.
    $deleted = Qa $dir @('group', 'Projects', 'DeleteGroup')
    $deleted = WaitFor { $r = Rows; if ($r -notmatch '\[group Projects') { $r } } 5000 "the group deleted ($deleted)"
    Once $deleted 'QA-Channel' 0
    $groups = GroupCli @('chat', 'group', 'list')
    if ($groups -match 'Projects') { Fail "the CLI still lists the deleted group: $groups" }
    if (Log $dir | Where-Object { $_ -like '*nothing to run*' -or $_ -like '*placeholder*' }) { Fail "a command ran nothing: $((Log $dir | Where-Object { $_ -like '*nothing to run*' }) -join ' | ')" }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  $pictures
}

# The Folder Sync page updated in place (the model's structure apart from its values, win-101). The failures caught: a
# value drawn stale (the status the page shows is not the model's), and the page built anew for every value that changes
# while a folder is added (each build made enough WinRT objects for a blocking full collection every few draws).
$checks.syncinplace = {
  $dir = New-State 'full'
  New-Item -ItemType Directory -Force "$dir\Movies" | Out-Null
  1..400 | ForEach-Object { [IO.File]::WriteAllBytes("$dir\Movies\film$_.bin", [byte[]]::new(65536)) }
  $p = Start-App $dir -QaControl
  try {
    WaitForSync $dir
    Qa $dir @('window') | Out-Null
    Qa $dir @('place', 'foldersync') | Out-Null
    $said = (& $Cli --socket "$dir\state\control.sock" sync add "$dir\Movies" 2>&1) -join ' '
    if ($said -notlike 'Sharing Movies*') { Fail "sync add: $said" }
    function Page {
      $d = Qa $dir @('syncpage')
      if ($d -notmatch 'status "([^"]*)".*drawn (\d+), in place (\d+), status drawn "([^"]*)"') { Fail "no page: $d" }
      $r = [pscustomobject]@{ status = $Matches[1]; draws = [int]$Matches[2]; inPlace = [int]$Matches[3]; drawn = $Matches[4] }
      if ($r.status -ne $r.drawn) { Fail "the page shows a stale status: model '$($r.status)', drawn '$($r.drawn)'" }
      $r
    }
    # Settled: the folder listed and up to date.
    $settled = WaitFor { $r = Page; if ($r.status -match 'Up to date') { $r } } 60000 'the folder up to date'
    # Values only: more files in the folder change its counts and status, not the page's elements.
    401..1200 | ForEach-Object { [IO.File]::WriteAllBytes("$dir\Movies\film$_.bin", [byte[]]::new(65536)) }
    $seen = @(); $clock = [Diagnostics.Stopwatch]::StartNew()
    while ($clock.Elapsed.TotalSeconds -lt 15) { $seen += Page }
    $last = $seen[-1]
    $statuses = @($seen | ForEach-Object { $_.status } | Select-Object -Unique)
    Write-Host ("  syncinplace: settled at {0} draws; then {1} statuses, {2} more draws, {3} more in place" -f $settled.draws, $statuses.Count, ($last.draws - $settled.draws), ($last.inPlace - $settled.inPlace))
    Write-Host ("  syncinplace: statuses {0}" -f ($statuses -join ' | '))
    if ($last.inPlace -le $settled.inPlace) { Fail "the new files changed nothing in place (statuses: $($statuses -join ' | '))" }
    if ($last.draws -ne $settled.draws) { Fail "the page was built again $($last.draws - $settled.draws) times for values only (statuses: $($statuses -join ' | '))" }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @()
}

# Every setting from the CLI (934aaf5f), as the Windows app takes it. The failures caught: a change the CLI makes that
# the app never applies (launch at login, the sleep holds, the updater's switches) or that an open Settings page never
# shows; the proxy's shield left as it was after `seedwell proxy`; a key on standard input refused or misread from
# PowerShell 5.1 (CRLF, its own encoding) or from Git Bash; a JSON value that PowerShell 5.1's argument quoting turns into
# something else stored without a word; `log trackers --clear` failing on Windows. The run's own state folder: launch at
# login is never written to the registry here (only the installed app's is).
$checks.clisettings = {
  $dir = New-State 'full'
  $qa = "$dir\state\qa"
  New-Item -ItemType Directory -Force $qa | Out-Null
  $free = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0); $free.Start(); $port = $free.LocalEndpoint.Port; $free.Stop()
  $socks = Resolve-Path "$PSScriptRoot\..\..\app\qa\fake_socks5.py"
  $fake = Start-Process python -ArgumentList @('-I', "`"$socks`"", "`"$qa`"", $port, 'fake-user', 'fake-pass') -PassThru -WindowStyle Hidden
  $p = Start-App $dir -QaControl
  $sock = "$dir\state\control.sock"
  $bash = Join-Path $env:ProgramFiles 'Git\bin\bash.exe'
  function SetCli([string[]]$pairs) {
    $o = (& $Cli --socket $sock settings set @pairs 2>&1) -join ' '
    if ($LASTEXITCODE -ne 0) { Fail "settings set $($pairs -join ' ') failed ($LASTEXITCODE): $o" }
    $o
  }
  function Applied([string]$what, [string]$like) { WaitFor { Log $dir | Where-Object { $_ -like $like } | Select-Object -Last 1 } 10000 $what }
  function BashCli([string]$line) {
    $cli = $Cli -replace '\\', '/'; $s = $sock -replace '\\', '/'
    $o = (& $bash -c ($line -replace '\{cli\}', "'$cli' --socket '$s'") 2>&1) -join ' '
    @{ Exit = $LASTEXITCODE; Out = $o }
  }
  try {
    WaitForSync $dir
    if ($fake.HasExited) { Fail 'the fake SOCKS5 server did not start (python?)' }
    if (-not (Test-Path $bash)) { Fail "no Git Bash at $bash" }
    Qa $dir @('window') | Out-Null

    # 1, 2. Launch at login and the sleep holds reach the app (and the Run entry stays the installed app's).
    $runBefore = (Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -ErrorAction SilentlyContinue).Seedwell
    SetCli @('general.launch_at_login=true', 'general.prevent_sleep_while_downloading=false', 'general.keep_awake_for_agents=false') | Out-Null
    Applied 'launch at login and the sleep holds' '*settings applied: launch at login True (not written*, awake while downloading False, awake for agents False*' | Out-Null
    SetCli @('general.launch_at_login=false', 'general.prevent_sleep_while_downloading=true', 'general.keep_awake_for_agents=true') | Out-Null
    Applied 'the settings back' '*settings applied: launch at login False*, awake while downloading True, awake for agents True*' | Out-Null
    $runAfter = (Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -ErrorAction SilentlyContinue).Seedwell
    if ($runAfter -ne $runBefore) { Fail "a QA run changed the user's Run entry: '$runBefore' became '$runAfter'" }

    # 3. updates.* reach the app, and the updater plans again with them (the core reads the switches itself: a local
    # build on a temporary state never plans an automatic check, so its line stays "no automatic check").
    foreach ($w in 'false', 'true') {
      $v = @($w, ($w.Substring(0, 1).ToUpper() + $w.Substring(1)))
      $n = @(Log $dir).Count
      SetCli @("updates.check=$($v[0])", "updates.auto_install=$($v[0])") | Out-Null
      WaitFor { $l = @(Log $dir | Select-Object -Skip $n); $i = [array]::FindIndex($l, [Predicate[object]]{ param($x) $x -like "*settings applied:*update checks $($v[1]), automatic install $($v[1])*" }); $i -ge 0 -and @($l | Select-Object -Skip $i | Where-Object { $_ -like '*update: *automatic check*' }).Count -gt 0 } 10000 "updates.check=$($v[0]) to reach the app and the updater to plan again" | Out-Null
    }

    # 4. An open Settings page reads the change again.
    Qa $dir @('settings', 'tab', 'Transfers') | Out-Null
    SetCli @('transfers.max_active_downloads=7') | Out-Null
    WaitFor { $r = Qa $dir @('settings', 'row', 'queue.active'); if ($r -match 'Active downloads: 7\b') { $r } } 10000 'Settings to show 7 active downloads' | Out-Null

    # 5, 6. The proxy from the CLI, its key on standard input from Git Bash, then from PowerShell 5.1; the shield follows.
    $none = Qa $dir @('proxy-model')
    if ($none -notmatch 'icon NoKey') { Fail "not the no-key state: $none" }
    $b = BashCli "printf '%s\n%s\n' fake-user fake-pass | {cli} proxy custom 127.0.0.1 $port --key"
    if ($b.Exit -ne 0) { Fail "the key from Git Bash was refused ($($b.Exit)): $($b.Out)" }
    $on = WaitFor { $m = Qa $dir @('proxy-model'); if ($m -notmatch 'icon NoKey' -and $m -match 'has key True') { $m } } 10000 'the shield to show the key from Git Bash'
    & $Cli --socket $sock proxy off 2>&1 | Out-Null
    $off = WaitFor { $m = Qa $dir @('proxy-model'); if ($m -ne $on -and $m -match 'has key True') { $m } } 10000 "the shield to change on proxy off (was: $on)"
    & $Cli --socket $sock proxy remove-key 2>&1 | Out-Null
    WaitFor { (Qa $dir @('proxy-model')) -match 'icon NoKey' } 10000 'the shield back to no key' | Out-Null
    $ps = ("fake-user`nfake-pass" | & $Cli --socket $sock proxy custom 127.0.0.1 $port --key 2>&1) -join ' '
    if ($LASTEXITCODE -ne 0) { Fail "the key from PowerShell 5.1 was refused ($LASTEXITCODE): $ps" }
    WaitFor { (Qa $dir @('proxy-model')) -match 'has key True' } 10000 'the shield to show the key from PowerShell' | Out-Null
    & $Cli --socket $sock proxy remove-key 2>&1 | Out-Null
    $logs = @(Get-ChildItem "$dir\state\logs" -File | ForEach-Object { Get-Content -Raw -LiteralPath $_.FullName -ErrorAction SilentlyContinue }) -join "`n"
    if ($logs -match 'fake-pass') { Fail 'the key reached a log' }

    # 7. JSON quoting: Git Bash keeps the quotes; PowerShell 5.1 drops them on the way to a native program, and the CLI
    # must then refuse with its sentence rather than store something else.
    $j = BashCli "{cli} settings set transfers.alt='{\`"schedule\`":{\`"enabled\`":true}}'"
    if ($j.Exit -ne 0) { Fail "a JSON table from Git Bash was refused ($($j.Exit)): $($j.Out)" }
    $got = (& $Cli --socket $sock --json settings get transfers.alt 2>&1) -join ''
    if ($got -notmatch '"enabled":\s*true') { Fail "the JSON table from Git Bash did not arrive: $got" }
    SetCli @('transfers.alt.schedule.enabled=false') | Out-Null
    $psJson = (& $Cli --socket $sock settings set 'transfers.alt={"schedule":{"enabled":true}}' 2>&1) -join ' '
    $psExit = $LASTEXITCODE
    $after = (& $Cli --socket $sock --json settings get transfers.alt 2>&1) -join ''
    if ($psExit -eq 0 -and $after -notmatch '"enabled":\s*true') { Fail "PowerShell's JSON was taken as something else without a word: $psJson; now $after" }
    # (win-101: the CLI refuses it itself, exit 2, naming the key and how to quote.)
    if ($psExit -ne 0 -and ($psExit -ne 2 -or $psJson -notmatch 'transfers\.alt' -or $psJson -notmatch 'PowerShell' -or $psJson -notmatch '--%')) { Fail "PowerShell's JSON was refused, but not naming the key and how to quote (exit $psExit): $psJson" }
    if ($after -match '"schedule":\s*"') { Fail "PowerShell's JSON was stored as a string: $after" }
    Write-Host "  clisettings: PowerShell 5.1 'transfers.alt={...}': exit $psExit, $psJson"

    # 8. log trackers --clear.
    & $Cli --socket $sock dev on 2>&1 | Out-Null
    $cleared = (& $Cli --socket $sock --json log trackers --clear 2>&1) -join ''
    if ($LASTEXITCODE -ne 0 -or $cleared -notmatch '"cleared"') { Fail "log trackers --clear: $cleared" }
    & $Cli --socket $sock dev off 2>&1 | Out-Null
  } finally {
    & $Cli --socket "$dir\state\control.sock" proxy remove-key 2>&1 | Out-Null
    Stop-App $dir $p
    if (-not $fake.HasExited) { Stop-Process -Id $fake.Id -Force -ErrorAction SilentlyContinue }
    Remove-Item -Recurse -Force $dir
  }
  @()
}

# seedwell restart (e713c893) on an installed copy (a temporary folder with the app, its install.ps1 and the CLI, on a
# temporary state), never the user's app. The failures caught: the app not back, or back on another state folder; the
# CLI that waits ended by the quit (it runs from the install folder); another Seedwell from the same folder quit too;
# update.log without the restart's lines; --no-wait waiting; a run below permission Full let through, or refused
# without naming `seedwell agents request full`; the activity log losing "restart Seedwell" across the restart; a copy
# without its installer (a build run from its build folder) restarting rather than saying why not.
$checks.restart = {
  $root = Join-Path ([IO.Path]::GetTempPath()) "seedwell-qa-restart-$([Guid]::NewGuid().ToString('N').Substring(0, 8))"
  $inst = "$root\Programs\Seedwell"
  New-Item -ItemType Directory -Force "$inst\cli" | Out-Null
  Copy-Item -Recurse "$(Split-Path $Exe)\*" $inst
  Copy-Item "$Repo\install.ps1" $inst
  Copy-Item $Cli "$inst\cli\seedwell.exe"
  $icli = "$inst\cli\seedwell.exe"
  $a = New-State 'full'; $b = New-State 'full'; $low = New-State 'control'
  $procs = @()
  # -AtOnce: back as soon as the socket answers, the app maybe still starting (its updater not there yet).
  function StartInstalled([string]$dir, [switch]$AtOnce) {
    $p = Start-Process "$inst\Seedwell.exe" -ArgumentList @('--state-dir', "$dir\state", '--qa-control') -WorkingDirectory $inst -PassThru
    WaitFor { & $icli --socket "$dir\state\control.sock" status 2>&1 | Out-Null; $LASTEXITCODE -eq 0 } 20000 'the installed copy to answer' | Out-Null
    if (-not $AtOnce) { Started $dir }
    $p
  }
  # The control socket answers once the core runs; restart needs the app's updater, made in a later startup turn. Until
  # win-102 the core answered then as a daemon would; now it waits for the updater. The first restart of each copy asks
  # at once, the others once the app has started.
  function Early([string]$dir) { if (Log $dir | Where-Object { $_ -like '*launch: on the UI thread*' }) { 'after the app had started' } else { 'while the app was still starting' } }
  function Started([string]$dir) { WaitFor { Log $dir | Where-Object { $_ -like '*launch: on the UI thread*' } } 20000 'the app to finish starting' | Out-Null }
  function AppPid([string]$dir) { ((& $icli --socket "$dir\state\control.sock" --json status 2>&1) -join '' | ConvertFrom-Json).pid }
  function OnState([string]$dir) { @(Get-CimInstance Win32_Process -Filter "Name='Seedwell.exe'" | Where-Object { $_.CommandLine -like "*$dir\state*" -and $_.CommandLine -notmatch '--qa |--quit' }) }
  try {
    $pb = StartInstalled $b; $pa = StartInstalled $a -AtOnce; $procs += $pa, $pb
    $oldA = $pa.Id
    # a) From the installed location, waiting, asked as soon as the socket answers; c) the waiting CLI is the install
    # folder's own.
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $when = Early $a
    $said = (& $icli --socket "$a\state\control.sock" restart 2>&1) -join ' '
    $exit = $LASTEXITCODE
    Write-Host "  restart: the installed copy asked at once ($when)"
    if ($said -match 'cannot restart itself') { Fail "an app still starting answered as a daemon: $said" }
    if ($exit -ne 0) { Fail "restart failed ($exit) after $([int]$clock.Elapsed.TotalSeconds) s: $said" }
    if ($said -notmatch 'running again: process (\d+)') { Fail "restart did not say the new run: $said" }
    $newA = [int]$Matches[1]
    if ($newA -eq $oldA) { Fail "the same process answers: $said" }
    $running = @(OnState $a)
    if ($running.Count -ne 1 -or $running[0].ProcessId -ne $newA -or $running[0].ExecutablePath -ne "$inst\Seedwell.exe") { Fail "not back as the installed copy on its state: $($running | ForEach-Object { "$($_.ProcessId) $($_.CommandLine)" })" }
    if ($pb.HasExited) { Fail 'the other Seedwell from the same folder was quit' }
    Write-Host ("  restart: back in {0:n1} s as process {1}" -f $clock.Elapsed.TotalSeconds, $newA)
    # d) update.log.
    $ulog = Get-ChildItem "$a\state" -Recurse -Filter 'update.log' | Select-Object -First 1
    if (-not $ulog) { Fail 'no update.log' }
    $u = Get-Content -Raw $ulog.FullName
    if ($u -notmatch "Restarting Seedwell \(process $oldA\)" -or $u -notmatch 'Started Seedwell again') { Fail "update.log has not the restart's lines: $u" }
    # The activity log across the restart.
    $act = (& $icli --socket "$a\state\control.sock" activity --limit 20 2>&1) -join ' '
    if ($act -notmatch 'restart Seedwell') { Fail "the activity log lost the restart: $act" }
    # --no-wait answers once accepted.
    Start-Sleep -Seconds 1
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $quick = (& $icli --socket "$a\state\control.sock" restart --no-wait 2>&1) -join ' '
    if ($LASTEXITCODE -ne 0) {
      # Within a moment of the last, the core refuses (a restart is under way): say so, wait, try again.
      if ($quick -notmatch 'under way') { Fail "restart --no-wait failed: $quick" }
      Start-Sleep -Seconds 20; $clock.Restart()
      $quick = (& $icli --socket "$a\state\control.sock" restart --no-wait 2>&1) -join ' '
      if ($LASTEXITCODE -ne 0) { Fail "restart --no-wait failed: $quick" }
    }
    if ($clock.Elapsed.TotalSeconds -gt 5) { Fail "--no-wait waited $([int]$clock.Elapsed.TotalSeconds) s: $quick" }
    WaitFor { $r = @(OnState $a); if ($r.Count -eq 1 -and $r[0].ProcessId -ne $newA) { & $icli --socket "$a\state\control.sock" status 2>&1 | Out-Null; $LASTEXITCODE -eq 0 } } 180000 'the app back after --no-wait' | Out-Null
    # Below Full: refused, exit 4, naming the way to ask.
    $pl = Start-App $low -QaControl; $procs += $pl
    Started $low
    $no = (& $Cli --socket "$low\state\control.sock" restart 2>&1) -join ' '
    if ($LASTEXITCODE -ne 4 -or $no -notmatch 'seedwell agents request full') { Fail "below Full: exit $LASTEXITCODE, $no" }
    if ($pl.HasExited) { Fail 'the app below Full was restarted' }
  } finally {
    foreach ($d in $a, $b) { foreach ($r in @(OnState $d)) { Start-Process "$inst\Seedwell.exe" -ArgumentList @('--state-dir', "`"$d\state`"", '--quit') -Wait; $q = Get-Process -Id $r.ProcessId -ErrorAction SilentlyContinue; if ($q -and -not $q.WaitForExit(30000)) { $q.Kill() } } }
    Stop-App $low $pl
    Remove-Item -Recurse -Force $a, $b, $low, $root -ErrorAction SilentlyContinue
  }
  # A build folder's copy: refused, with the reason.
  $dev = New-State 'full'
  $pd = Start-App $dev -QaControl
  try {
    # Asked as soon as the socket answers (Start-App waits for no more): the app's own answer, never a daemon's.
    $when = if (Log $dev | Where-Object { $_ -like '*launch: on the UI thread*' }) { 'after the app had started' } else { 'while the app was still starting' }
    $why = (& $Cli --socket "$dev\state\control.sock" restart 2>&1) -join ' '
    Write-Host "  restart: the build-folder copy asked at once ($when)"
    if ($why -match 'cannot restart itself') { Fail "an app still starting answered as a daemon: $why" }
    if ($LASTEXITCODE -eq 0 -or $why -notmatch 'no installer') { Fail "a build-folder copy: exit $LASTEXITCODE, $why" }
    if ($pd.HasExited) { Fail 'the build-folder copy quit' }
  } finally { Stop-App $dev $pd; Remove-Item -Recurse -Force $dev }
  @()
}

# New Channel / Join with a Link: the New side makes a channel (named as typed, Agents as chosen) and opens it; the
# Join side reads a link through the core and says why one cannot be joined (Start stays off).
$checks.newchannel = {
  $dir = New-State 'control'
  $p = Start-App $dir -QaControl
  $pictures = @()
  try {
    WaitForSync $dir
    Qa $dir @('window') | Out-Null
    $opened = Qa $dir @('new-channel', 'open')
    if ($opened -notmatch 'new channel New, can start False') { Fail "New Channel did not open empty: $opened" }
    Qa $dir @('new-channel', 'name', 'Family') | Out-Null
    $ready = Qa $dir @('new-channel', 'agents', 'yes')
    if ($ready -notmatch 'can start True') { Fail "a name did not enable Start: $ready" }
    Qa $dir @('shot', 'new-channel', 'dialog') | Out-Null
    Copy-Item "$dir\state\qa\new-channel.png" "$Out\new-channel.png"; $pictures += "$Out\new-channel.png"
    $started = Qa $dir @('new-channel', 'start')
    if ($started -notmatch 'started.*place Chat') { Fail "Start did not make and open the channel: $started" }
    $list = (& $Cli --socket "$dir\state\control.sock" chat list 2>&1) -join ' '
    if ($list -notmatch 'Family') { Fail "no channel Family: $list" }
    # A new channel's next step is its Share dialog (the Mac's), once New Channel has closed.
    # (It opens once New Channel has closed.)
    try { $share = WaitFor { $l = Qa $dir @('share', 'status'); if ($l -match 'share Share .Family.') { $l } } 10000 'the Share dialog after New Channel' }
    catch { Fail "no Share dialog after New Channel: $_; now: $(Qa $dir @('share', 'status'))" }
    Qa $dir @('shot', 'share-channel', 'dialog') | Out-Null
    Copy-Item "$dir\state\qa\share-channel.png" "$Out\share-channel.png"; $pictures += "$Out\share-channel.png"
    Qa $dir @('share', 'cancel') | Out-Null
    Qa $dir @('new-channel', 'open') | Out-Null
    Qa $dir @('new-channel', 'mode', 'join') | Out-Null
    $bad = Qa $dir @('new-channel', 'link', 'seedwell://chat/not-a-real-link')
    if ($bad -notmatch 'new channel Join, can start False, link line "[^"]+"') { Fail "a broken link was not refused with the core's line: $bad" }
    Qa $dir @('shot', 'new-channel-join', 'dialog') | Out-Null
    Copy-Item "$dir\state\qa\new-channel-join.png" "$Out\new-channel-join.png"; $pictures += "$Out\new-channel-join.png"
    Qa $dir @('new-channel', 'cancel') | Out-Null
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  $pictures
}

# seedwell:// links: through the single-instance hand-off (a second launch with the link, as Windows starts the app for
# a registered scheme) a channel link opens New Channel on Join with the link filled in; an unknown kind and a folder
# that is not synced here are the core's alerts; a pairing link opens Settings.
$checks.links = {
  $dir = New-State
  $p = Start-App $dir -QaControl
  try {
    Qa $dir @('window') | Out-Null
    Launch $dir @('seedwell://chat/not-a-real-link') | Out-Null
    WaitFor { Log $dir | Where-Object { $_ -like '*second launch: 1 to open*' } } 5000 'the hand-off' | Out-Null
    WaitFor { Log $dir | Where-Object { $_ -like '*new channel: open on Join*' } } 20000 'New Channel on Join' | Out-Null
    $join = Qa $dir @('new-channel', 'status')
    if ($join -notmatch 'link line "[^"]+"') { Fail "the link was not filled in and read: $join" }
    Qa $dir @('new-channel', 'cancel') | Out-Null
    Launch $dir @('seedwell://nonsense/x') | Out-Null
    $unknown = WaitFor { Log $dir | Where-Object { $_ -like '*alert: *' } | Select-Object -Last 1 } 5000 'the alert for an unknown link'
    if ($unknown -notmatch "Couldn't open the link") { Fail "an unknown link: $unknown" }
    $before = @(Log $dir | Where-Object { $_ -like '*alert: *' }).Count
    Qa $dir @('link', 'seedwell://folder/NOPE') | Out-Null
    $gone = WaitFor { $a = @(Log $dir | Where-Object { $_ -like '*alert: *' }); if ($a.Count -gt $before) { $a[-1] } } 5000 'the alert for a folder not synced'
    if ($gone -notmatch 'no longer synced') { Fail "a folder not synced here: $gone" }
    $pair = Qa $dir @('link', 'seedwell://devices/not-a-real-link')
    if ($pair -notmatch 'place Settings') { Fail "a pairing link did not open Settings: $pair" }
    WaitFor { Log $dir | Where-Object { $_ -like '*devices: pair with a link (filled in)*' } } 5000 'Pair with a Link, filled in' | Out-Null
    # Explorer's Share with Seedwell (item 5): the verb's launch, --share <folder> (a space in its name), opens that
    # folder's Share dialog through the core's command_line_routes (a folder not synced yet: a new one).
    New-Item -ItemType Directory -Force "$dir\To Share" | Out-Null
    Launch $dir @('--share', "$dir\To Share") | Out-Null
    WaitFor { Log $dir | Where-Object { $_ -like '*share: open on a new folder*' } } 5000 'the Share dialog of the folder' | Out-Null
    Qa $dir @('share', 'cancel') | Out-Null
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @()
}

# The tray's right-click menu from the core's app commands: Add Torrent File…, Add Magnet Link or URL…, Pause All,
# Resume All, Limit Speed, then Open and Quit; Limit Speed switches the alternative speed on and draws its check mark;
# Add Magnet Link or URL… opens the window's add sheet.
$checks.tray = {
  $dir = New-State
  $p = Start-App $dir -QaControl
  try {
    $menu = Qa $dir @('tray-menu')
    foreach ($t in 'Add Torrent File', 'Add Magnet Link or URL', 'Pause All', 'Resume All', 'Limit Speed', 'Quit') {
      if ($menu -notmatch [regex]::Escape($t)) { Fail "the tray menu has no $t`: $menu" }
    }
    Qa $dir @('app-command', 'SetLimitSpeed') | Out-Null
    $on = Qa $dir @('tray-menu')
    if ($on -notmatch 'Limit Speed \[x\]') { Fail "Limit Speed is not checked after it was switched on: $on" }
    Qa $dir @('app-command', 'AddLink') | Out-Null
    WaitFor { Log $dir | Where-Object { $_ -like '*app command AddLink*' } } 5000 'the add sheet' | Out-Null
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @()
}

# The seedwell: scheme's registration under the test seam (SEEDWELL_INSTALL_TEST=1: HKCU\Software\SeedwellInstallTest,
# never the real Classes key): --register writes it with the app's open command, --unregister removes it, and the
# real key is the same before and after.
$checks.scheme = {
  $real = 'HKCU:\Software\Classes\seedwell\shell\open\command'
  function RealValue { $k = Get-Item $real -ErrorAction SilentlyContinue; if ($k) { $k.GetValue('') } else { '' } }
  $realBefore = RealValue
  $test = 'HKCU:\Software\SeedwellInstallTest\Classes\seedwell'
  $env:SEEDWELL_INSTALL_TEST = '1'
  try {
    Start-Process $Exe -ArgumentList '--register' -Wait
    if (-not (Test-Path $test)) { Fail 'no seedwell: key after --register' }
    if ((Get-ItemProperty $test).'URL Protocol' -ne '') { Fail 'the seedwell: key is not a URL protocol' }
    $cmd = (Get-Item "$test\shell\open\command").GetValue('')
    if ($cmd -notlike "*Seedwell.exe*%1*") { Fail "its open command: $cmd" }
    # Explorer's Share with Seedwell (item 5): the core's title, the exe and --share "%1" (%V on a folder's background).
    $verb = 'HKCU:\Software\SeedwellInstallTest\Classes\Directory\shell\Seedwell.Share'
    if (-not (Test-Path $verb)) { Fail 'no Share with Seedwell verb after --register' }
    if ((Get-ItemProperty $verb).MUIVerb -notmatch 'Seedwell') { Fail "the verb's title: $((Get-ItemProperty $verb).MUIVerb)" }
    $vcmd = (Get-Item "$verb\command").GetValue('')
    if ($vcmd -notlike '"*Seedwell.exe" --share "%1"') { Fail "the verb's command: $vcmd" }
    $bcmd = (Get-Item 'HKCU:\Software\SeedwellInstallTest\Classes\Directory\Background\shell\Seedwell.Share\command').GetValue('')
    if ($bcmd -notlike '"*Seedwell.exe" --share "%V"') { Fail "the background verb's command: $bcmd" }
    Start-Process $Exe -ArgumentList '--unregister' -Wait
    if (Test-Path $test) { Fail 'the seedwell: key is left after --unregister' }
    if (Test-Path $verb) { Fail 'the Share verb is left after --unregister' }
  } finally {
    Remove-Item env:SEEDWELL_INSTALL_TEST
    Remove-Item -Recurse -Force 'HKCU:\Software\SeedwellInstallTest' -ErrorAction SilentlyContinue
  }
  $realAfter = RealValue
  if ($realAfter -ne $realBefore) { Fail 'the real seedwell: key changed' }
  @()
}

# Settings › Folder Sync › Cloudflare: Use with an address that does not answer (a closed port on loopback) runs in the
# background and ends with the core's problem sentence, drawn under the section.
$checks.rendezvous = {
  $dir = New-State
  $p = Start-App $dir -QaControl
  try {
    Qa $dir @('window') | Out-Null
    $off = Qa $dir @('rendezvous', 'status')
    if ($off -notmatch 'rendezvous Off') { Fail "not off at first: $off" }
    # The section is on this tab: on another it is not drawn, and a picture of it fails.
    Qa $dir @('settings', 'tab', 'FolderSync') | Out-Null
    $shot = Qa $dir @('shot', 'rendezvous-off', 'rendezvous')
    if (-not (Test-Path "$dir/state/qa/rendezvous-off.png")) { Fail "no picture: $shot" }
    Copy-Item "$dir\state\qa\rendezvous-off.png" "$Out\rendezvous-off.png"
    Qa $dir @('rendezvous', 'use', 'http://127.0.0.1:9') | Out-Null
    $done = WaitFor { $l = Qa $dir @('rendezvous', 'status'); if ($l -match 'problem True' -or $l -match 'refused "[^"]+"') { $l } } 60000 'the result'
    $shot = Qa $dir @('shot', 'rendezvous-result', 'rendezvous')
    if (-not (Test-Path "$dir/state/qa/rendezvous-result.png")) { Fail "no picture: $shot" }
    Copy-Item "$dir\state\qa\rendezvous-result.png" "$Out\rendezvous-result.png"
    if ($done -notmatch 'rendezvous Off') { Fail "an address that does not answer was taken: $done" }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @("$Out\rendezvous-off.png", "$Out\rendezvous-result.png")
}

# "On all my devices" (in a folder's Edit dialog): hidden while this PC is paired with no other device; turning it on
# anyway is refused by the core (Save fails in the dialog), and nothing changes.
$checks.follow = {
  $dir = New-State 'control'
  New-Item -ItemType Directory -Force "$dir\shared" | Out-Null
  $p = Start-App $dir -QaControl
  try {
    WaitForSync $dir
    & $Cli --socket "$dir\state\control.sock" sync add "$dir\shared" | Out-Null
    if ($LASTEXITCODE -ne 0) { Fail "the CLI could not add a folder (exit $LASTEXITCODE)" }
    Qa $dir @('window') | Out-Null
    $on = Qa $dir @('follow', 'on')
    if ($on -notmatch 'follow shown False, on False') { Fail "an unpaired PC follows: $on" }
    Qa $dir @('shot', 'foldersync') | Out-Null
    Copy-Item "$dir\state\qa\foldersync.png" "$Out\foldersync.png"
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @("$Out\foldersync.png")
}

# "Other devices can't reach this PC", with the core's QA files standing for the devices' verdict and the rule. The
# failures it names: no banner or button on a page that has no folders; a command not naming this exe whole; a banner
# left after the rule came; UAC's No taken for a policy (a recheck on it, which drops the button for good); a policy not
# told. The elevated command never runs here.
$checks.firewall = {
  $dir = New-State 'control'
  New-Item -ItemType Directory -Force "$dir\state\qa" | Out-Null
  function Files([string]$inbound, [string]$rule) { $inbound | Set-Content -NoNewline "$dir\state\qa\inbound.txt"; $rule | Set-Content -NoNewline "$dir\state\qa\firewall.txt" }
  Files 'blocked' 'missing|Public|False'
  $p = Start-App $dir -QaControl
  $pictures = @()
  try {
    WaitForSync $dir
    Qa $dir @('window') | Out-Null
    $exe = (Get-Process -Id $p.Id).Path
    $missing = WaitFor { $s = Qa $dir @('reach'); if ($s -match 'banner Visible') { $s } } 10000 'the banner on the empty page'
    if ($missing -notmatch 'Windows Firewall' -or $missing -notmatch 'Public') { Fail "the notice does not name Windows Firewall and the network: $missing" }
    if ($missing -notmatch 'program "netsh(\.exe)?"') { Fail "not netsh: $missing" }
    if ($missing -notlike "*program=`"$exe`"*") { Fail "the command does not name this exe whole ($exe): $missing" }
    if ($missing -notmatch 'buttons \[Allow Seedwell through Windows Firewall\S* on\]') { Fail "no button: $missing" }
    Qa $dir @('shot', 'reach-missing') | Out-Null
    Copy-Item "$dir\state\qa\reach-missing.png" "$Out\reach-missing.png"; $pictures += "$Out\reach-missing.png"

    # UAC's No: nothing changes, the button stays.
    $no = Qa $dir @('reach-allow', 'no')
    if ($no -notmatch 'buttons \[Allow Seedwell through Windows Firewall\S* on\]') { Fail "after UAC's No the button should stay: $no" }
    if (-not (Log $dir | Where-Object { $_ -like '*reach: allow through the firewall cancelled*' })) { Fail 'the No was not taken as a No' }

    # The rule came: the notice goes.
    Files 'blocked' 'allowed|Public|False'
    $gone = Qa $dir @('reach-recheck')
    if ($gone -notmatch 'notice none') { Fail "the notice stayed after the rule came: $gone" }
    if ($gone -notmatch 'banner Collapsed, buttons \[\]') { Fail "the banner stayed after the rule came: $gone" }

    # A block rule: the other command.
    Files 'blocked' 'blocked|Public|True'
    $blocked = WaitFor { $s = Qa $dir @('reach'); if ($s -match 'notice "') { $s } } 10000 'the blocked notice'
    if ($blocked -notmatch 'set rule') { Fail "a block rule should be turned round with set rule: $blocked" }

    # Ran, and the rule reads as before: an administrator's policy, no button.
    Files 'blocked' 'missing|Domain|False'
    $before = WaitFor { $s = Qa $dir @('reach'); if ($s -match 'action "') { $s } } 10000 'the action again'
    $ran = Qa $dir @('reach-allow', 'ran')
    $policy = WaitFor { $s = Qa $dir @('reach'); if ($s -match 'notice "' -and $s -match 'action none') { $s } } 20000 'the policy notice'
    if ($policy -notmatch 'administrator') { Fail "the notice should say an administrator's policy decides: $policy (before: $before; ran: $ran)" }
    Qa $dir @('shot', 'reach-policy') | Out-Null
    Copy-Item "$dir\state\qa\reach-policy.png" "$Out\reach-policy.png"; $pictures += "$Out\reach-policy.png"
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  $pictures
}

# The torrent proxy's drawing, with the Mac's loopback fake standing for NordVPN's servers (never a real key or server).
# The failures it names: a shield whose shape or colours differ from the Mac's sheet; a state or a click that does not
# follow the core; a password left in a field, or filled from an unrelated copy; a key shown; a Country list longer than
# 6 rows, or one an arrow alone applies; a call the switch, shield or country can run into.
$checks.proxy = {
  $a = New-State 'control'
  $qa = "$a\state\qa"
  New-Item -ItemType Directory -Force $qa | Out-Null
  $free = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0); $free.Start(); $port = $free.LocalEndpoint.Port; $free.Stop()
  "127.0.0.1:$port NL Amsterdam" | Set-Content -NoNewline -Encoding ascii "$qa\proxy-servers.txt"
  $script = Resolve-Path "$PSScriptRoot\..\..\app\qa\fake_socks5.py"
  $fake = Start-Process python -ArgumentList @('-I', "`"$script`"", "`"$qa`"", $port, 'fake-user', 'fake-pass') -PassThru -WindowStyle Hidden
  $theme = 'dark'
  $env:SEEDWELL_QA_THEME = $theme
  $p = Start-App $a -QaControl
  $pictures = @()
  function Shot([string]$name, [string[]]$how = @()) {
    Qa $a (@('shot', $name) + $how) | Out-Null
    Copy-Item "$qa\$name.png" "$Out\$name.png"
    "$Out\$name.png"
  }
  function Model { Qa $a @('proxy-model') }
  function Window([string[]]$words) { Qa $a (@('proxy-window') + $words) 30000 }
  # The state's pictures, in the theme this app was started with (an app's theme is fixed once it starts).
  function States([string]$state) {
    Qa $a @('settings', 'tab', 'Network') | Out-Null
    Shot "proxy-$state-settings-$theme"
    Qa $a @('proxy-window', 'open') | Out-Null
    Shot "proxy-$state-window-$theme" @('dialog')
    Window @('close') | Out-Null
  }
  try {
    WaitForSync $a
    if ($fake.HasExited) { Fail 'the fake SOCKS5 server did not start (python?)' }
    Qa $a @('window') | Out-Null

    # The shield against the Mac's sheet, cell by cell: the same layout, sampled on a grid, the same colour at nearly every point.
    Add-Type -AssemblyName System.Drawing
    $saved = Qa $a @('proxy-sheet') 20000
    if ($saved -notmatch 'sheet saved') { Fail "no contact sheet: $saved" }
    Copy-Item "$qa\proxy-shield-win.png" "$Out\proxy-shield-win.png"; $pictures += "$Out\proxy-shield-win.png"
    $mine = [Drawing.Bitmap]::new("$qa\proxy-shield-win.png"); $mac = [Drawing.Bitmap]::new("$PSScriptRoot\reference\proxy-shield-sheet.png")
    try {
      $scale = $mine.Width / 1806.0
      $macScale = $mac.Width / 1806.0
      $worst = 1.0; $where = ''
      foreach ($row in 0..4) { foreach ($dark in 0, 828) { foreach ($c in @(@(150, 134, 192), @(352, 102, 256), @(618, 70, 320))) {
        $same = 0; $all = 0
        foreach ($u in 0..39) { foreach ($v in 0..39) {
          $x = $c[0] + $dark + ($u + 0.5) * $c[2] / 40; $y = $c[1] + 330 * $row + ($v + 0.5) * $c[2] / 40
          $m = $mine.GetPixel([int]($x * $scale), [int]($y * $scale)); $r = $mac.GetPixel([int]($x * $macScale), [int]($y * $macScale))
          $all++
          if ([Math]::Abs($m.R - $r.R) + [Math]::Abs($m.G - $r.G) + [Math]::Abs($m.B - $r.B) -le 90) { $same++ }
        } }
        if ($same / $all -lt $worst) { $worst = $same / $all; $where = "row $row, x $($c[0] + $dark)" }
      } } }
      Write-Host ("  proxy: the shield against the Mac's sheet: the worst cell matches {0:p1} ({1})" -f $worst, $where)
      if ($worst -lt 0.9) { Fail ("the shield differs from the Mac's sheet: {0:p1} at {1}" -f $worst, $where) }
    } finally { $mine.Dispose(); $mac.Dispose() }

    # No key: the shield opens the window; no switch, the fields.
    $none = Model
    if ($none -notmatch 'icon NoKey, has key False' -or $none -notmatch 'opens window True') { Fail "not the no-key state: $none" }
    $pictures += @(States 'nokey')
    $click = Qa $a @('proxy-click')
    $open = Window @('status')
    if ($open -notmatch 'shows \[nord,nord-fields') { Fail "a click without a key should open the window with the fields: click $click; $open" }
    if ($open -match 'switch' -and $open -match 'shows \[[^\]]*switch') { Fail "a switch without a key: $open" }
    $guide = Window @('guide', 'on')
    if ($guide -notmatch 'guide') { Fail "the guide did not open: $guide" }
    Window @('guide', 'off') | Out-Null

    # Copies fill the fields in order, and nothing else does.
    $first = Window @('copied', 'fake-user')
    if ($first -notmatch 'user filled True, password filled False' -or $first -notmatch 'pasted') { Fail "the copied username: $first" }
    $second = Window @('copied', 'fake-pass')
    if ($second -notmatch 'user filled True, password filled True' -or $second -notmatch 'enable on True') { Fail "the copied password: $second" }
    Window @('fill', 'typed-user') | Out-Null
    $typed = Window @('copied', 'an-unrelated-secret')
    if ($typed -notmatch 'password filled False') { Fail "a copy filled the password beside a username that was typed: $typed" }
    if ((Log $a) -match 'an-unrelated-secret|fake-pass|wrong-pass') { Fail 'a credential reached the log' }

    # Wrong credentials: the core's sentence, nothing kept, the password gone from its field.
    Window @('fill', 'wrong-user', 'wrong-pass') | Out-Null
    $wrong = Window @('enable')
    if ($wrong -notmatch 'password filled False' -or $wrong -notmatch 'result "[^"]+"') { Fail "wrong credentials: $wrong" }
    if ((Model) -notmatch 'has key False') { Fail "wrong credentials were kept: $(Model)" }
    Shot 'proxy-badkey-window' @('dialog') | ForEach-Object { $pictures += $_ }

    # The key: on, the fields gone, the switch there; no field holds it, and Replace Key shows them with Cancel.
    Window @('fill', 'fake-user', 'fake-pass') | Out-Null
    $on = Window @('enable')
    if ($on -notmatch 'shows \[nord,nord-installed' -or $on -notmatch 'switch on True' -or $on -notmatch 'password filled False') { Fail "the key did not install: $on" }
    if ((Model) -notmatch 'icon (On|Slow), has key True, enabled True') { Fail "not on: $(Model)" }
    $replace = Window @('replace', 'on')
    if ($replace -notmatch 'nord-fields') { Fail "Replace Key shows no fields: $replace" }
    Shot 'proxy-replace-window' @('dialog') | ForEach-Object { $pictures += $_ }
    if ((Window @('replace', 'off')) -notmatch 'nord-installed') { Fail 'Cancel did not hide the fields' }
    $hover = Window @('hover', 'on')
    if ($hover -notmatch 'glow 0\.75') { Fail "no glow on hover: $hover" }
    if ((Window @('hover', 'off')) -notmatch 'glow 0[ ,]') { Fail 'the glow stayed' }

    # The Country list: 6 rows that scroll, an arrow or a letter moves and applies nothing, Return chooses.
    $list = Qa $a @('proxy-countries', 'open')
    if ($list -notmatch 'list height 216 rows (\d+)' -or [int]$Matches[1] -le 6) { Fail "not 6 rows of a longer list: $list" }
    if ($list -notmatch 'row height 36') { Fail "a row is not 36 tall: $list" }
    Qa $a @('proxy-countries', 'shot', 'proxy-country-list') | Out-Null
    Copy-Item "$qa\proxy-country-list.png" "$Out\proxy-country-list.png"; $pictures += "$Out\proxy-country-list.png"
    $letter = Qa $a @('proxy-countries', 'key', 'S')
    if ($letter -notmatch 'selected ([1-9]\d*), country ""') { Fail "a letter should move and apply nothing: $letter" }
    Qa $a @('proxy-countries', 'key', 'Enter') | Out-Null
    $chosen = WaitFor { $s = Model; if ($s -match 'country "[A-Z]{2}"') { $s } } 10000 'Return to choose the country'
    if ($chosen -notmatch 'busy False') { $chosen = Model }

    # The switch, and the shield with a key: off, then on, the states' pictures on the way.
    $off = Qa $a @('proxy-toggle')
    if ($off -notmatch 'icon Off, has key True, enabled False') { Fail "not off: $off" }
    Window @('close') | Out-Null
    $pictures += @(States 'off')
    $back = Qa $a @('proxy-click')
    if ($back -notmatch 'enabled True') { Fail "a click with a key should switch it on: $back" }
    $pictures += @(States 'on')
    $inFlyout = Qa $a @('proxy-flyout', 'open')
    if ($inFlyout -notmatch 'flyout open True, shield (On|Slow)') { Fail "the flyout's shield does not follow: $inFlyout" }
    Qa $a @('proxy-flyout', 'shot', "proxy-on-menu-$theme") | Out-Null
    Copy-Item "$qa\proxy-on-menu-$theme.png" "$Out\proxy-on-menu-$theme.png"; $pictures += "$Out\proxy-on-menu-$theme.png"
    Qa $a @('proxy-flyout', 'close') | Out-Null

    # Slow: the server answers late; failed: it is gone, torrents wait, and the reason shows.
    '2' | Set-Content -NoNewline -Encoding ascii "$qa\socks-delay-v4"
    Qa $a @('proxy-window', 'open') | Out-Null
    $faster = Window @('faster')
    if ($faster -notmatch 'result "[^"]+"') { Fail "Find a Faster Server said nothing: $faster" }
    Window @('close') | Out-Null
    try { WaitFor { $s = Model; if ($s -match 'icon Slow') { $s } } 60000 'the slow state' | Out-Null; $pictures += @(States 'slow') }
    catch { Write-Host "  proxy: the slow state did not come within 60 s, so it was not pictured: $(Model)" }
    Remove-Item "$qa\socks-delay-v4" -Force
    Stop-Process -Id $fake.Id -Force
    $failed = WaitFor { $s = Model; if ($s -match 'icon Failed') { $s } } 90000 'the failed state'
    if ($failed -notmatch 'problem "[^"]+"') { Fail "a failure should say why: $failed" }
    $pictures += @(States 'failed')

    # Remove Key: back to none.
    Qa $a @('proxy-window', 'open') | Out-Null
    Window @('remove') | Out-Null
    if ((Model) -notmatch 'icon NoKey, has key False') { Fail "the key was not removed: $(Model)" }
    Window @('close') | Out-Null
    if ((Get-Content "$a\state\logs\core.log" -Raw -ErrorAction SilentlyContinue) -match 'fake-pass') { Fail 'the password reached core.log' }
    # A core call or the window's own work holding the UI thread (a 2.2 s stall under load, 2026-11-05): named, so it is found.
    $slow = @(Log $a | Where-Object { $_ -like '*proxy: slow on the UI thread:*' })
    if ($slow.Count -gt 0) { Fail "a step held the UI thread: $(($slow | Select-Object -First 5 | ForEach-Object { $_ -replace '^.*UI thread: ', '' }) -join '; ')" }

    # The same states in the light theme, for the pictures: the app started in it, as a light Windows starts it.
    Stop-App $a $p
    $theme = 'light'
    $env:SEEDWELL_QA_THEME = $theme
    $fake = Start-Process python -ArgumentList @('-I', "`"$script`"", "`"$qa`"", $port, 'fake-user', 'fake-pass') -PassThru -WindowStyle Hidden
    $p = Start-App $a -QaControl
    WaitForSync $a
    Qa $a @('window') | Out-Null
    $pictures += @(States 'nokey')
    Qa $a @('proxy-window', 'open') | Out-Null
    Window @('fill', 'fake-user', 'fake-pass') | Out-Null
    $on = Window @('enable')
    if ($on -notmatch 'nord-installed') { Fail "the key did not install in the light run: $on" }
    Window @('close') | Out-Null
    $pictures += @(States 'on')
    Qa $a @('proxy-flyout', 'open') | Out-Null
    Qa $a @('proxy-flyout', 'shot', "proxy-on-menu-$theme") | Out-Null
    Copy-Item "$qa\proxy-on-menu-$theme.png" "$Out\proxy-on-menu-$theme.png"; $pictures += "$Out\proxy-on-menu-$theme.png"
    Qa $a @('proxy-flyout', 'close') | Out-Null
    Qa $a @('proxy-toggle') | Out-Null
    $pictures += @(States 'off')
    Qa $a @('proxy-toggle') | Out-Null
    '2' | Set-Content -NoNewline -Encoding ascii "$qa\socks-delay-v4"
    Qa $a @('proxy-window', 'open') | Out-Null
    Window @('faster') | Out-Null
    Window @('close') | Out-Null
    try { WaitFor { $s = Model; if ($s -match 'icon Slow') { $s } } 60000 'the slow state' | Out-Null; $pictures += @(States 'slow') }
    catch { Write-Host "  proxy: the slow state did not come within 60 s in the light run, so it was not pictured: $(Model)" }
    Remove-Item "$qa\socks-delay-v4" -Force
    Stop-Process -Id $fake.Id -Force
    WaitFor { $s = Model; if ($s -match 'icon Failed') { $s } } 90000 'the failed state in the light run' | Out-Null
    $pictures += @(States 'failed')
  } finally {
    Remove-Item Env:SEEDWELL_QA_THEME -ErrorAction SilentlyContinue
    Stop-App $a $p
    if (-not $fake.HasExited) { Stop-Process -Id $fake.Id -Force -ErrorAction SilentlyContinue }
    Remove-Item -Recurse -Force $a -ErrorAction SilentlyContinue
  }
  $pictures
}

# The SOCKS5 window and credentials, with no proxy needed. The failures it names (the review of 49aca0da): a window that
# failed to open going on reading the clipboard into a password field nobody clears; a copy filling a closed window; a
# copied username surviving a change of pane, so a later copy pairs with it; a credential in any log.
$checks.proxysecrets = {
  $dir = New-State 'control'
  $p = Start-App $dir -QaControl
  $secrets = 'SECRET-USER-1', 'SECRET-PASS-1', 'SECRET-FILLED-2', 'SECRET-FILLED-3', 'SECRET-LATE-4', 'SECRET-CUSTOM-5', 'SECRET-CUSTOM-6'
  function Window([string[]]$words) { Qa $dir (@('proxy-window') + $words) }
  try {
    WaitForSync $dir
    Qa $dir @('window') | Out-Null
    # Another dialog is open: this one's ShowAsync throws.
    Qa $dir @('add-folder', 'open') | Out-Null
    Window @('open') | Out-Null
    if (-not (Log $dir | Where-Object { $_ -like '*proxy window failed:*' })) { Fail 'the second dialog opened, so nothing was tested' }
    $late = Window @('last', 'SECRET-LATE-4')
    if ($late -notmatch 'user filled False') { Fail "a window that never opened took a copy: $late" }
    $left = Qa $dir @('proxy-model')
    if ($left -notmatch 'watchers 0') { Fail "a window that never opened goes on watching: $left" }
    Qa $dir @('add-folder', 'cancel') | Out-Null

    # A change of pane empties both and forgets the copied username.
    Window @('open') | Out-Null
    $user = Window @('copied', 'SECRET-USER-1')
    if ($user -notmatch 'user filled True, password filled False') { Fail "the copy did not fill the username: $user" }
    Window @('pane', 'custom') | Out-Null
    $back = Window @('pane', 'nord')
    if ($back -notmatch 'user filled False, password filled False') { Fail "a change of pane left the fields: $back" }
    $pair = Window @('copied', 'SECRET-PASS-1')
    if ($pair -notmatch 'user filled True, password filled False') { Fail "a copy paired with the username of before the change: $pair" }
    Window @('fill', 'SECRET-FILLED-2', 'SECRET-FILLED-3') | Out-Null
    Window @('custom', '127.0.0.1', '1080', 'SECRET-CUSTOM-5', 'SECRET-CUSTOM-6') | Out-Null

    # Closed: a copy that comes after it fills nothing.
    Window @('close') | Out-Null
    $after = Window @('last', 'SECRET-LATE-4')
    if ($after -notmatch 'user filled False') { Fail "a closed window took a copy: $after" }

    # No credential in any log, even once.
    $logs = @(Get-ChildItem "$dir\state\logs" -File | ForEach-Object { Get-Content -Raw -LiteralPath $_.FullName -ErrorAction SilentlyContinue }) -join "`n"
    foreach ($secret in $secrets) { if ($logs -match [regex]::Escape($secret)) { Fail "a credential reached a log: $secret" } }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @()
}

# The "Seedwell" entry in Explorer's navigation pane, under the test seam (never the real keys, which Explorer reads). The
# failures it names: an entry not naming the core's name, folder or this exe; one that does not follow a new folder; one
# left behind when the core says to remove it, or by an earlier crash (no orphans in Explorer); another program's entry
# swept away; the real keys touched by a QA run.
$checks.navpane = {
  $dir = New-State 'control'
  $id = '{7A1C0B52-6E44-4C1E-9D23-0F5B6A9E11C7}'
  $orphan = '{7A1C0B52-6E44-4C1E-9D23-0F5B6A9E11C8}'
  $foreign = '{7A1C0B52-6E44-4C1E-9D23-0F5B6A9E11C9}'
  $marker = 'SeedwellQaNavPane'
  $root = 'HKCU:\Software\SeedwellInstallTest'
  $clsids = "$root\Classes\CLSID"
  New-Item -Force "$clsids\$orphan" | Out-Null
  New-ItemProperty "$clsids\$orphan" -Name $marker -Value 1 -PropertyType DWord -Force | Out-Null
  New-Item -Force "$clsids\$foreign" | Out-Null
  # A crash between the removals: the list's key is left, the CLSID gone. And another program's, which stays.
  $desktop = "$root\Microsoft\Windows\CurrentVersion\Explorer\Desktop\NameSpace"
  $halfGone = '{7A1C0B52-6E44-4C1E-9D23-0F5B6A9E11CA}'
  New-Item -Force "$desktop\$halfGone" | Out-Null
  New-ItemProperty "$desktop\$halfGone" -Name $marker -Value 1 -PropertyType DWord -Force | Out-Null
  New-Item -Force "$desktop\$foreign" | Out-Null
  $env:SEEDWELL_INSTALL_TEST = '1'
  $p = $null
  try {
    $p = Start-App $dir -QaControl
    WaitForSync $dir
    $one = Qa $dir @('nav-pane', $id, $marker, 'on', 'Seedwell', "$dir\Seedwell")
    if ($one -notlike "*entry `"Seedwell`", target `"$dir\Seedwell`"*") { Fail "not the core's name and folder: $one" }
    if ($one -notlike "*icon `"$((Get-Process -Id $p.Id).Path),0`"*") { Fail "not this exe's icon: $one" }
    # An expanding value would open another folder, or none, for a folder with % in its name.
    $kind = (Get-Item "$clsids\$id\Instance\InitPropertyBag").GetValueKind('TargetFolderPath')
    if ($kind -ne 'String') { Fail "the target is a $kind, not a plain string" }
    if ($one -notmatch 'on the desktop list True, hidden from the desktop True') { Fail "not pinned as Microsoft's steps say: $one" }
    if (Test-Path "$clsids\$orphan") { Fail 'an entry left by an earlier run was not swept' }
    if (Test-Path "$desktop\$halfGone") { Fail 'a list entry left by a crash, its CLSID gone, was not swept' }
    if (-not (Test-Path "$desktop\$foreign")) { Fail "another program's list entry was swept" }
    if (-not (Test-Path "$clsids\$foreign")) { Fail "another program's entry was swept" }
    $moved = Qa $dir @('nav-pane', $id, $marker, 'on', 'Seedwell', "$dir\Moved here")
    if ($moved -notlike "*target `"$dir\Moved here`"*") { Fail "the entry did not follow the folder: $moved" }
    $gone = Qa $dir @('nav-pane', $id, $marker, 'off', 'Seedwell', "$dir\Moved here")
    if ($gone -notmatch 'entry none') { Fail "the entry stayed: $gone" }
    if (Test-Path "$root\Microsoft\Windows\CurrentVersion\Explorer\Desktop\NameSpace\$id") { Fail 'the desktop list kept it' }
    $hidden = Get-ItemProperty "$root\Microsoft\Windows\CurrentVersion\Explorer\HideDesktopIcons\NewStartPanel" -Name $id -ErrorAction SilentlyContinue
    if ($hidden) { Fail 'the hidden list kept it' }
    foreach ($k in "HKCU:\Software\Classes\CLSID\$id", "HKCU:\Software\Classes\CLSID\$orphan", "HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\Desktop\NameSpace\$id") {
      if (Test-Path $k) { Fail "a QA run touched a real key: $k" }
    }
    # A QA run's core offers no entry at all: the CLSID is one per user.
    if ((Log $dir | Where-Object { $_ -like '*navigation pane entry:*' }).Count -gt 0) { Fail 'the core gave a QA run the real entry' }
    # Uninstalling removes the entry by the core's marker (--unregister has no core), and leaves another program's.
    $coreMarker = 'SeedwellNavPane'
    New-Item -Force "$clsids\$id" | Out-Null
    New-ItemProperty "$clsids\$id" -Name $coreMarker -Value 1 -PropertyType DWord -Force | Out-Null
    Start-Process $Exe -ArgumentList '--unregister' -Wait
    if (Test-Path "$clsids\$id") { Fail '--unregister left the entry' }
    if (-not (Test-Path "$clsids\$foreign")) { Fail "--unregister took another program's entry" }
  } finally {
    Stop-App $dir $p
    Remove-Item Env:SEEDWELL_INSTALL_TEST -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
  }
  @()
}

# The sync port answers on IPv4 as well as IPv6. The failure it names: a listener on [::] is IPv6 only on Windows unless
# told otherwise (on a Mac it takes both), so no device can dial this PC at the IPv4 addresses a link gives or a router maps.
$checks.listen = {
  $dir = New-State 'control'
  $p = Start-App $dir -QaControl
  try {
    WaitForSync $dir
    $port = ((& $Cli --socket "$dir\state\control.sock" sync list --json 2>&1) -join '' | ConvertFrom-Json).port
    $answers = foreach ($ip in '::1', '127.0.0.1') {
      $t = [Net.Sockets.TcpClient]::new([Net.IPAddress]::Parse($ip).AddressFamily)
      try { $t.Connect($ip, $port); "$ip yes" } catch { "$ip no" } finally { $t.Dispose() }
    }
    if ($answers -notcontains '::1 yes' -or $answers -notcontains '127.0.0.1 yes') { Fail "the sync port $port answers: $($answers -join ', ')" }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @()
}

# "Read only", the folder owner's own setting (SPEC 7G.1), on temp devices A (the owner) and B (paired with A, an admin).
# The failures it names: a switch missing for an owner paired with nothing, or inside "On all my devices" (gone or stuck with
# it); a member that may switch it or is not told who can; a member's change reaching the owner while it is on, or lost
# when it goes off; the owner's own change held back; a role changed by it (the old design); the Share dialog's next link
# not following it.
$checks.readonly = {
  $a = New-State 'control'; $b = New-State 'control'; $alone = New-State 'control'
  $docs = "$a\Docs"
  New-Item -ItemType Directory -Force $docs, "$a\New", "$alone\New" | Out-Null
  Set-Content -NoNewline -Encoding ascii "$docs\from-a.txt" 'from A'
  $sa = "$a\state\control.sock"; $sb = "$b\state\control.sock"
  $env:SEEDWELL_QA_THEME = 'dark'
  $pa = Start-App $a -QaControl
  $pb = $null
  $pictures = @()
  function Shot([string]$dir, [string]$name) {
    Qa $dir @('shot', $name, 'dialog') | Out-Null
    Copy-Item "$dir\state\qa\$name.png" "$Out\$name.png"
    "$Out\$name.png"
  }
  function RoleOfB {
    $m = (& $Cli --socket $sa sync members Docs --json 2>&1) -join '' | ConvertFrom-Json
    @($m.members | Where-Object { -not $_.me } | ForEach-Object { $_.role }) -join ','
  }
  function ReadOnlyOn([string]$sock) { (& $Cli --socket $sock sync read-only Docs status --json 2>&1) -join '' | ConvertFrom-Json }
  # The Read only switch in each dialog of both devices (it is on, B follows), in the theme the apps started in.
  function Pictures([string]$theme) {
    Qa $a @('window') | Out-Null
    Qa $b @('window') | Out-Null
    Qa $a @('edit-folder', 'open', 'Docs') | Out-Null
    Shot $a "readonly-edit-$theme"
    Qa $a @('edit-folder', 'cancel') | Out-Null
    Qa $b @('edit-folder', 'open', 'Docs') | Out-Null
    Shot $b "readonly-member-$theme"
    Qa $b @('edit-folder', 'cancel') | Out-Null
    Qa $a @('share', 'open', 'Docs') | Out-Null
    Qa $a @('share', 'scroll-end') | Out-Null
    Shot $a "readonly-share-$theme"
    Qa $a @('share', 'cancel') | Out-Null
    # Add Folder on a PC paired with nothing: its own temp device.
    $pc = Start-App $alone -QaControl
    try {
      Qa $alone @('window') | Out-Null
      Qa $alone @('add-folder', 'open') | Out-Null
      Qa $alone @('add-folder', 'folder', "$alone\New") | Out-Null
      Qa $alone @('add-folder', 'readonly', 'on') | Out-Null
      Shot $alone "readonly-add-alone-$theme"
      Qa $alone @('add-folder', 'cancel') | Out-Null
    } finally { Stop-App $alone $pc }
  }
  try {
    # Paired with nothing, it is there and works: in Add Folder for a folder of this PC, not for one shared with me.
    Qa $a @('window') | Out-Null
    $open = Qa $a @('add-folder', 'open')
    if ($open -notmatch 'read only False enabled True line "[^"]+"') { Fail "An unpaired owner's Add Folder should offer Read only: $open" }
    if ($open -notmatch 'drawn \[[^\]]*Read only False') { Fail "Add Folder has no Read only switch on screen: $open" }
    Qa $a @('add-folder', 'folder', "$a\New") | Out-Null
    $chosen = Qa $a @('add-folder', 'readonly', 'on')
    if ($chosen -notmatch 'read only True enabled True line "[^"]+"' -or $chosen -notmatch 'drawn \[[^\]]*Read only True') { Fail "Add Folder did not draw the choice: $chosen" }
    $shared = Qa $a @('add-folder', 'mode', 'shared')
    if ($shared -notmatch 'read only none' -or $shared -match 'drawn \[[^\]]*Read only') { Fail "Shared with me should have no Read only: $shared" }
    Qa $a @('add-folder', 'cancel') | Out-Null
    $added = (& $Cli --socket $sa sync add $docs 2>&1) -join ' '
    if ($added -notlike 'Sharing Docs*') { Fail "sync add: $added" }
    Qa $a @('edit-folder', 'open', 'Docs') | Out-Null
    $chosen = Qa $a @('edit-folder', 'readonly', 'on')
    if ($chosen -notmatch 'read only True enabled True line "[^"]+"' -or $chosen -notmatch 'drawn \[[^\]]*Read only True') { Fail "An unpaired owner's Edit did not draw the choice: $chosen" }
    $saved = Qa $a @('edit-folder', 'save') 30000
    if ($saved -notmatch 'saved' -or $saved -match 'not saved') { Fail "Edit Folder did not save: $saved" }
    if (-not (ReadOnlyOn $sa).read_only) { Fail 'Save did not turn Read only on for an unpaired owner' }
    Qa $a @('edit-folder', 'open', 'Docs') | Out-Null
    Qa $a @('edit-folder', 'readonly', 'off') | Out-Null
    Qa $a @('edit-folder', 'save') 30000 | Out-Null
    if ((ReadOnlyOn $sa).read_only) { Fail 'Save did not turn Read only off for an unpaired owner' }

    # B paired and following the folder, an admin.
    $pb = Start-App $b -QaControl
    WaitForSync $a; WaitForSync $b
    Qa $b @('window') | Out-Null
    $link = [regex]::Match((Qa $a @('pair', 'link')), 'link (\S+) ;').Groups[1].Value
    if (-not $link) { Fail 'A made no pairing link' }
    Qa $b @('pair', 'join', $link) 30000 | Out-Null
    try { WaitFor { $s = Qa $a @('pair', 'status'); if ($s -match 'requests 1') { $s } } 30000 'B asking to pair on A' | Out-Null }
    catch { Fail "B did not reach A to pair; A: $(Qa $a @('pair', 'status')); B: $(Qa $b @('pair', 'status'))" }
    Qa $a @('pair', 'confirm') 30000 | Out-Null
    WaitFor { $s = Qa $b @('pair', 'status'); if ($s -match 'devices Paired') { $s } } 30000 'B to be paired' | Out-Null
    Qa $a @('edit-folder', 'open', 'Docs') | Out-Null
    Qa $a @('edit-folder', 'devices', 'on') | Out-Null
    Qa $a @('edit-folder', 'save') 30000 | Out-Null
    WaitFor { if ((RoleOfB) -eq 'admin') { $true } } 60000 'B in the folder as an admin' | Out-Null
    $inB = WaitFor {
      # A temp state's default location is its own Sync folder, never the real home.
      if (Test-Path "$b\state\Sync\Docs\from-a.txt") { "$b\state\Sync\Docs" }
    } 60000 'the folder on B'

    # A turns it on in Edit, beside "On all my devices".
    Qa $a @('edit-folder', 'open', 'Docs') | Out-Null
    $chosen = Qa $a @('edit-folder', 'readonly', 'on')
    if ($chosen -notmatch 'drawn \[[^\]]*On all my devices True, Read only True') { Fail "Edit Folder should draw both switches: $chosen" }
    $saved = Qa $a @('edit-folder', 'save') 30000
    if ($saved -notmatch 'saved' -or $saved -match 'not saved') { Fail "Edit Folder did not save: $saved" }

    # B: told, may not switch it, its changes held; its role the same.
    $on = WaitFor { $s = ReadOnlyOn $sb; if ($s.read_only) { $s } } 60000 'Read only to reach B'
    if ($on.may_switch -or -not $on.held) { Fail "B should hold its changes and may not switch: $($on | ConvertTo-Json -Compress)" }
    $edit = Qa $b @('edit-folder', 'open', 'Docs')
    if ($edit -notmatch 'read only True enabled False line "Read only: only the folder''s owner changes it') { Fail "B's Edit should show the state and who can switch it: $edit" }
    if ($edit -notmatch 'drawn \[[^\]]*Read only True \(off\)') { Fail "B's switch should be drawn disabled: $edit" }
    Qa $b @('edit-folder', 'cancel') | Out-Null
    if ((RoleOfB) -ne 'admin') { Fail "Read only changed B's role: $(RoleOfB)" }

    # B's file stays on B while A's goes on reaching it.
    Set-Content -NoNewline -Encoding ascii "$inB\from-b.txt" 'from B'
    $why = (& $Cli --socket $sb sync rescan Docs 2>&1) -join ' '
    Set-Content -NoNewline -Encoding ascii "$docs\from-a-2.txt" 'from A while read only'
    & $Cli --socket $sa sync rescan Docs 2>&1 | Out-Null
    WaitFor { Test-Path "$inB\from-a-2.txt" } 60000 'the owner''s change to reach B' | Out-Null
    Start-Sleep -Seconds 5
    if (Test-Path "$docs\from-b.txt") { Fail "a file made on B reached A while Read only (rescan: $why)" }

    # The pictures, dark as the run started, then light; the setting and the role stay over a restart.
    $pictures += @(Pictures 'dark')
    Stop-App $a $pa; $pa = $null
    Stop-App $b $pb; $pb = $null
    $env:SEEDWELL_QA_THEME = 'light'
    $pa = Start-App $a -QaControl
    $pb = Start-App $b -QaControl
    WaitForSync $a; WaitForSync $b
    $pictures += @(Pictures 'light')
    if (-not (ReadOnlyOn $sa).read_only -or -not (ReadOnlyOn $sb).held) { Fail 'Read only did not stay over a restart' }

    # Share: as the core has it, applied at once. Off: B's file comes in, B's role is what it was, the next link is not Read only.
    $share = Qa $a @('share', 'open', 'Docs')
    if ($share -notmatch 'role ReadOnly') { Fail "the Share dialog's next link should be Read only: $share" }
    if ($share -notmatch 'all devices True enabled True, read only True enabled True') { Fail "Share's model has not the switches: $share" }
    if ($share -notmatch 'drawn \[[^\]]*On all my devices True, Read only True') { Fail "Share has no Read only switch on screen: $share" }
    Qa $a @('share', 'readonly', 'off') | Out-Null
    $off = WaitFor { $s = Qa $a @('share', 'status'); if ($s -match 'setting devices False' -and $s -match 'read only False') { $s } } 30000 'Read only to go off in Share'
    if ($off -notmatch 'role ReadWrite') { Fail "the next link stayed Read only: $off" }
    if ($off -match 'failure "[^"]') { Fail "Share's Read only failed: $off" }
    WaitFor { Test-Path "$docs\from-b.txt" } 60000 'B''s held file to reach A once Read only is off' | Out-Null
    if ((RoleOfB) -ne 'admin') { Fail "B's role is not what it was: $(RoleOfB)" }
    WaitFor { $s = ReadOnlyOn $sb; if (-not $s.read_only -and -not $s.held) { $s } } 30000 'B to hear it is off' | Out-Null
    Qa $a @('share', 'readonly', 'on') | Out-Null
    $again = WaitFor { $s = Qa $a @('share', 'status'); if ($s -match 'setting devices False' -and $s -match 'read only True') { $s } } 30000 'Read only to come on in Share'
    if ($again -notmatch 'role ReadOnly') { Fail "the next link should follow Read only: $again" }

    # Its own switch: "On all my devices" going off neither changes it nor takes it away.
    Qa $a @('share', 'devices', 'off') | Out-Null
    $free = WaitFor { $s = Qa $a @('share', 'status'); if ($s -match 'setting devices False' -and $s -match 'all devices False') { $s } } 30000 'On all my devices to go off in Share'
    if ($free -notmatch 'read only True enabled True') { Fail "Read only should stay and stay switchable with On all my devices off: $free" }
    Qa $a @('share', 'cancel') | Out-Null
    if ((Log $a) -match 'seedwell://sync/') { Fail 'a folder link was printed' }
  } finally {
    Remove-Item Env:SEEDWELL_QA_THEME -ErrorAction SilentlyContinue
    Stop-App $a $pa
    if ($pb) { Stop-App $b $pb }
    Remove-Item -Recurse -Force $a, $b, $alone
  }
  $pictures
}

# An admin removes a user by asking (SPEC 7G.1), on temp devices A (the owner), B (an admin) and C (Read & write). The
# failures it names: the row not saying the request waits for the owner, the item still enabled so it is asked twice, an
# admin offered more than the removal or a menu on the owner's row, the request lost over a restart, and the owner's
# device not carrying it out when it comes back.
$checks.adminremove = {
  $a = New-State 'full'; $b = New-State 'control'; $c = New-State 'control'
  $docs = "$a\Docs"
  New-Item -ItemType Directory -Force $docs | Out-Null
  Set-Content -NoNewline -Encoding ascii "$docs\from-a.txt" 'from A'
  $sa = "$a\state\control.sock"; $sb = "$b\state\control.sock"; $sc = "$c\state\control.sock"
  $env:SEEDWELL_QA_THEME = 'dark'
  $pa = Start-App $a -QaControl
  $pb = $null; $pc = $null
  $pictures = @()
  function Members([string]$sock) { @(((& $Cli --socket $sock sync members Docs --json 2>&1) -join '' | ConvertFrom-Json).members) }
  function ChatMembers([string]$sock) { @(((& $Cli --socket $sock chat members 'Plans' --json 2>&1) -join '' | ConvertFrom-Json).members) }
  function Rows { Qa $b @('syncpage', 'members') }
  function Shot([string]$name) {
    Qa $b @('shot', $name) | Out-Null
    Copy-Item "$b\state\qa\$name.png" "$Out\$name.png"
    "$Out\$name.png"
  }
  # The channel's panel: its rows as the core has them once they show the request, and its picture.
  function Panel([string]$name) {
    Qa $b @('place', 'chat', 'Plans') | Out-Null
    Qa $b @('chatpage', 'members') | Out-Null
    $rows = WaitFor { $s = Qa $b @('chatpage', 'member-rows'); if ($s -match "$([regex]::Escape($user)) [^|]*removal `"Removal asked: waiting for the owner`"") { $s } } 15000 'the waiting line in the channel panel'
    if ($rows -notmatch "$([regex]::Escape($user)) [^|]*menu \[Remove from Channel disabled\]") { Fail "the channel's item should be disabled while it waits: $rows" }
    $shot = Qa $b @('chatpage', 'members-shot', $name)
    if ($shot -notmatch 'panel saved') { Fail "no picture of the panel: $shot" }
    Copy-Item "$b\state\qa\$name.png" "$Out\$name.png"
    "$Out\$name.png"
  }
  try {
    $pb = Start-App $b -QaControl
    $pc = Start-App $c -QaControl
    WaitForSync $a; WaitForSync $b; WaitForSync $c
    $added = (& $Cli --socket $sa sync add $docs 2>&1) -join ' '
    if ($added -notlike 'Sharing Docs*') { Fail "sync add: $added" }
    foreach ($join in @(@($sb, 'admin', "$b\state\Docs"), @($sc, 'read_write', "$c\state\Docs"))) {
      $shared = (& $Cli --socket $sa sync share Docs --role $join[1] --json 2>&1) -join ''
      $link = ($shared | ConvertFrom-Json).link
      if (-not $link) { Fail "no $($join[1]) link: $shared" }
      $joined = ($link | & $Cli --socket $join[0] sync join - --to $join[2] 2>&1) -join ' '
      if ($joined -notlike 'Joining Docs*') { Fail "sync join: $joined" }
    }
    WaitFor { if (@(Members $sa).Count -eq 3 -and (Test-Path "$b\state\Docs\from-a.txt") -and (Test-Path "$c\state\Docs\from-a.txt")) { $true } } 60000 'B and C in the folder' | Out-Null
    $user = (Members $sa | Where-Object { $_.role -eq 'read_write' }).device_id
    $owner = (Members $sa | Where-Object { $_.role -eq 'owner' }).device_id
    WaitFor { if (@(Members $sb | Where-Object { $_.device_id -eq $user -and $_.role -eq 'read_write' }).Count -eq 1) { $true } } 60000 'B to know C' | Out-Null

    # A channel with the same three, where the same request has its line in the members panel.
    & $Cli --socket $sa chat new 'Plans' | Out-Null
    foreach ($join in @(@($sb, 'admin'), @($sc, 'read_write'))) {
      $shared = (& $Cli --socket $sa chat share 'Plans' --role $join[1] --json 2>&1) -join ''
      $link = ($shared | ConvertFrom-Json).link
      if (-not $link) { Fail "no $($join[1]) channel link: $shared" }
      $link | & $Cli --socket $join[0] chat join - 2>&1 | Out-Null
    }
    WaitFor { if (@(ChatMembers $sb | Where-Object { $_.device_id -eq $user -and $_.role -eq 'read_write' }).Count -eq 1 -and @(ChatMembers $sb | Where-Object { $_.me -and $_.role -eq 'admin' }).Count -eq 1) { $true } } 60000 'B an admin of the channel, knowing C' | Out-Null

    # B's page: the removal alone on the user, its own question; the owner's row plain.
    Qa $b @('window') | Out-Null
    Qa $b @('syncpage', 'choose', 'Docs') | Out-Null
    $rows = WaitFor { $s = Rows; if ($s -match [regex]::Escape($user)) { $s } } 30000 'C on B''s page'
    if ($rows -notmatch "$([regex]::Escape($user)) [^|]*menu \[Remove from Folder\S* enabled\] question `"The folder's owner removes it") { Fail "B's menu on C should be the removal alone, with the admin's question: $rows" }
    if ($rows -notmatch "$([regex]::Escape($owner)) [^|]*, plain") { Fail "the owner's row should be plain text for an admin: $rows" }

    # The owner away: the request waits, the row says so, it is not asked twice.
    Stop-App $a $pa; $pa = $null
    $said = Qa $b @('syncpage', 'member', $user, 'Remove from Folder')
    if ($said -notmatch 'ran RemoveDevice') { Fail "the removal did not run: $said" }
    $rows = WaitFor { $s = Rows; if ($s -match 'Removal asked: waiting for the owner') { $s } } 15000 'the waiting line on C''s row'
    if ($rows -notmatch "$([regex]::Escape($user)) [^|]*Removal asked: waiting for the owner[^|]*menu \[Remove from Folder\S* disabled\]") { Fail "C's row should wait with its item disabled: $rows" }
    if ((Qa $b @('syncpage', 'member', $user, 'Remove from Folder')) -notmatch 'disabled') { Fail 'the removal could be asked twice' }
    if ((Members $sb | Where-Object { $_.device_id -eq $user }).removal -ne 'waiting') { Fail "the CLI should say waiting: $(Members $sb | ConvertTo-Json -Compress)" }
    if ($rows -match 'Waiting for 2 devices') { Fail "the folder should not wait for a device cut off pending its removal: $rows" }
    $pictures += Shot 'admin-removal-dark'

    # The channel: the panel's line under the name, the item disabled.
    Qa $b @('place', 'chat', 'Plans') | Out-Null
    Qa $b @('chatpage', 'members') | Out-Null
    $before = Qa $b @('chatpage', 'member-rows')
    if ($before -notmatch "$([regex]::Escape($user)) [^|]*removal `"`" role Read & write, menu \[Remove from Channel enabled\]") { Fail "B's menu on C in the channel should be the removal alone: $before" }
    $said = Qa $b @('chatpage', 'member', $user, 'Remove from Channel')
    if ($said -notmatch 'ran RemoveDevice') { Fail "the channel's removal did not run: $said" }
    $pictures += Panel 'admin-removal-channel-dark'

    # Over a restart, and in the light theme.
    Stop-App $b $pb; $pb = $null
    $env:SEEDWELL_QA_THEME = 'light'
    $pb = Start-App $b -QaControl
    WaitForSync $b
    Qa $b @('window') | Out-Null
    Qa $b @('syncpage', 'choose', 'Docs') | Out-Null
    WaitFor { $s = Rows; if ($s -match "$([regex]::Escape($user)) [^|]*Removal asked: waiting for the owner[^|]*disabled") { $s } } 30000 'the request to survive B''s restart' | Out-Null
    $pictures += Shot 'admin-removal-light'
    $pictures += Panel 'admin-removal-channel-light'

    # The owner back: its device carries it out by itself.
    $pa = Start-App $a -QaControl
    WaitFor { if (@(Members $sa | Where-Object { $_.device_id -eq $user }).Count -eq 0) { $true } } 90000 'A to remove C by itself' | Out-Null
    WaitFor { if (@(Members $sb | Where-Object { $_.device_id -eq $user }).Count -eq 0) { $true } } 60000 'C to leave B''s members' | Out-Null
    try { WaitFor { if (@(ChatMembers $sa | Where-Object { $_.device_id -eq $user }).Count -eq 0) { $true } } 90000 'A to remove C from the channel by itself' | Out-Null }
    catch { Fail "A did not remove C from the channel. A: $(ChatMembers $sa | ConvertTo-Json -Compress) B: $(ChatMembers $sb | ConvertTo-Json -Compress) C: $(ChatMembers $sc | ConvertTo-Json -Compress); A's core: $((CoreLog $a | Select-Object -Last 12) -join ' | '); B's core: $((CoreLog $b | Select-Object -Last 12) -join ' | ')" }
  } finally {
    Remove-Item Env:SEEDWELL_QA_THEME -ErrorAction SilentlyContinue
    if ($pa) { Stop-App $a $pa }
    if ($pb) { Stop-App $b $pb }
    if ($pc) { Stop-App $c $pc }
    Remove-Item -Recurse -Force $a, $b, $c
  }
  $pictures
}

# The Folder Sync page and its dialogs (W3), on a temp state with no other device: the empty page; Add Folder from this
# PC (made: the CLI lists it); the page's buttons; Edit renames it (on disk too); Share makes a link (never printed) and revokes it;
# Add Folder's Shared with me refuses a damaged link with the core's line; Stop Syncing asks under the header, then
# removes it (the CLI no longer lists it).
$checks.folders = {
  $dir = New-State 'control'
  New-Item -ItemType Directory -Force "$dir\Shared" | Out-Null
  $p = Start-App $dir -QaControl
  try {
    WaitForSync $dir
    Qa $dir @('window') | Out-Null
    $pictures = [System.Collections.Generic.List[string]]::new()
    function Shot([string]$name, [switch]$Dialog) {
      $said = Qa $dir (@('shot', $name) + $(if ($Dialog) { @('dialog') } else { @() }))
      if (-not (Test-Path "$dir\state\qa\$name.png")) { Fail "no picture ${name}: $said" }
      if ($said -match 'drawn only at try') { Write-Host "  folders: ${name}: $([regex]::Match($said, 'drawn only at try \d+').Value): $((Log $dir | Where-Object { $_ -like "*picture ${name}.png:*" }) -join ' | ')" }
      Copy-Item "$dir\state\qa\$name.png" "$Out\$name.png"
      $pictures.Add("$Out\$name.png")
    }
    function Folders { (& $Cli --socket "$dir\state\control.sock" sync list 2>&1) -join ' ' }

    $empty = Qa $dir @('syncpage')
    if ($empty -notmatch 'empty ".+", add ".+"') { Fail "no empty page with Add Folder: $empty" }
    Shot 'foldersync-empty'

    $open = Qa $dir @('add-folder', 'open')
    if ($open -notmatch 'add folder Local, can start False') { Fail "Add Folder did not open on this PC with Start off: $open" }
    $chosen = Qa $dir @('add-folder', 'folder', "$dir\Shared")
    if ($chosen -notmatch 'can start True') { Fail "a chosen folder cannot start: $chosen" }
    # Files on demand (HostFeature.OnDemandFolders): the switch is there, off; on, the core says what it means.
    if ($chosen -notmatch 'on demand "(?!none")[^"]+" False') { Fail "Add Folder has no Files on demand switch: $chosen" }
    $od = Qa $dir @('add-folder', 'ondemand', 'on')
    if ($od -notmatch 'on demand "[^"]+" True note "[^"]+"') { Fail "Files on demand on says nothing: $od" }
    Qa $dir @('add-folder', 'ondemand', 'off') | Out-Null
    Shot 'add-folder' -Dialog
    $added = Qa $dir @('add-folder', 'start')
    if ($added -notmatch 'started') { Fail "the folder was not added: $added" }
    WaitFor { (Folders) -match 'Shared' } 10000 'the CLI to list the folder' | Out-Null
    # The core may open its Share dialog next (share_after_start), once Add Folder has closed.
    $next = Qa $dir @('share', 'status')
    if ($next -notmatch 'not open') { Qa $dir @('share', 'cancel') | Out-Null }

    $page = Qa $dir @('syncpage')
    if ($page -notmatch 'folders \[Shared\]' -or $page -notmatch 'buttons \[[^\]]*Share[^\]]*Edit[^\]]*Stop') { Fail "the page lacks the folder or its buttons: $page" }
    Shot 'foldersync'

    $edit = Qa $dir @('edit-folder', 'open', 'Shared')
    if ($edit -notmatch 'changed False') { Fail "Edit did not open unchanged: $edit" }
    if ($edit -notmatch 'on demand "(?!none")[^"]+" False') { Fail "Edit has no Files on demand switch: $edit" }
    $od = Qa $dir @('edit-folder', 'ondemand', 'on')
    if ($od -notmatch 'changed True.*on demand "[^"]+" True note "[^"]+"') { Fail "Files on demand on in Edit says nothing: $od" }
    Qa $dir @('edit-folder', 'ondemand', 'off') | Out-Null
    Qa $dir @('edit-folder', 'name', 'Renamed') | Out-Null
    $opts = Qa $dir @('edit-folder', 'options', 'on')
    if ($opts -notmatch 'changed True, can save True') { Fail "a new name cannot be saved: $opts" }
    Shot 'edit-folder' -Dialog
    $saved = Qa $dir @('edit-folder', 'save')
    if ($saved -notmatch '^.*saved' -or $saved -match 'not saved') { Fail "Edit did not save: $saved" }
    WaitFor { (Folders) -match 'Renamed' } 10000 'the CLI to list the new name' | Out-Null
    # A new name renames the folder on disk too (sync_rename).
    if (-not (Test-Path "$dir\Renamed")) { Fail 'the folder was not renamed on disk' }

    $share = Qa $dir @('share', 'open', 'Renamed')
    if ($share -notmatch 'copy Copy Link') { Fail "Share offers no Copy Link: $share" }
    Qa $dir @('share', 'role', 'read_write') | Out-Null
    Qa $dir @('share', 'expires', 'on', '7') | Out-Null
    $copied = Qa $dir @('share', 'copy')
    if ($copied -notmatch '^.*copied') { Fail "no link was made: $copied" }
    $listed = WaitFor { $s = Qa $dir @('share', 'status'); if ($s -match 'links [1-9]') { $s } } 10000 'the new link in the list'
    Shot 'share' -Dialog
    if ($listed -match 'seedwell://') { Fail "the link was printed: $listed" }
    Qa $dir @('share', 'revoke') | Out-Null
    WaitFor { $s = Qa $dir @('share', 'status'); if ($s -match 'links 0') { $s } } 10000 'the link to be revoked' | Out-Null
    Qa $dir @('share', 'cancel') | Out-Null

    Qa $dir @('add-folder', 'open') | Out-Null
    Qa $dir @('add-folder', 'mode', 'shared') | Out-Null
    $bad = Qa $dir @('add-folder', 'link', 'seedwell://sync/not-a-real-link')
    if ($bad -notmatch 'can start False' -or $bad -match 'link line ""') { Fail "a damaged link could start, or no line says why: $bad" }
    Shot 'add-folder-shared' -Dialog
    Qa $dir @('add-folder', 'cancel') | Out-Null

    $ask = Qa $dir @('syncpage', 'press', 'Stop')
    if ($ask -notmatch 'ran AskRemove' -or $ask -notmatch 'removal ".+" trash False') { Fail "Stop Syncing did not ask under the header: $ask" }
    Shot 'foldersync-remove'
    $gone = Qa $dir @('syncpage', 'remove')
    WaitFor { (Folders) -notmatch 'Renamed' } 10000 'the folder to be removed' | Out-Null
    if (-not (Test-Path "$dir\Renamed")) { Fail 'Stop Syncing without the Trash option deleted the folder' }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  $pictures.ToArray()
}

# The conversation page (W6), on a channel the CLI made on a temp state: the empty line; a message typed and sent
# (Markdown drawn as the core's runs); an agent's message posted with the CLI; a reply (its quote); an image and a file
# attached and sent (the image here, the file shared, then Stop Sharing); a tag; the ⋯ menu (Mute, New Topic's
# dialog); the members panel.
$checks.chatpage = {
  $dir = New-State 'control'
  $p = Start-App $dir -QaControl
  $pictures = [System.Collections.Generic.List[string]]::new()
  try {
    WaitForSync $dir
    & $Cli --socket "$dir\state\control.sock" chat new 'QA-Chat' | Out-Null
    if ($LASTEXITCODE -ne 0) { Fail "the CLI could not make a channel (exit $LASTEXITCODE)" }
    function Shot([string]$name, [switch]$Dialog) {
      Qa $dir (@('shot', $name) + $(if ($Dialog) { @('dialog') } else { @() })) | Out-Null
      Copy-Item "$dir\state\qa\$name.png" "$Out\$name.png"
      $pictures.Add("$Out\$name.png")
    }
    Qa $dir @('window') | Out-Null
    Qa $dir @('place', 'chat', 'QA-Chat') | Out-Null
    $empty = Qa $dir @('chatpage')
    if ($empty -notmatch 'cards 0, empty "[^"]+"' -or $empty -notmatch 'can post True') { Fail "the empty channel is not drawn with its line and a field: $empty" }
    if ($empty -notmatch 'toolbar \[[^\]]*members[^\]]*menu\]') { Fail "no Members and ⋯ in the toolbar: $empty" }

    if ($empty -notmatch 'send enabled False') { Fail "Send is on with nothing to send: $empty" }
    Qa $dir @('chatpage', 'type', 'Hello **there** from the PC') | Out-Null
    # The field says it changed a moment later (TextChanged), and Send follows.
    WaitFor { (Qa $dir @('chatpage')) -match 'send enabled True' } 3000 'Send on with text to send' | Out-Null
    $sent = Qa $dir @('chatpage', 'send')
    if ($sent -notmatch 'sent; cards 1, empty "", last "Hello there from the PC"') { Fail "the message was not sent and drawn: $sent" }

    "## Status`n- one`n- two" | & $Cli --socket "$dir\state\control.sock" chat send 'QA-Chat' --as QA-Agent - | Out-Null
    $agent = WaitFor { $s = Qa $dir @('chatpage'); if ($s -match 'cards 2') { $s } } 10000 "the agent's message"
    if ($agent -notmatch 'last "Status / .+ one / .+ two"') { Fail "the agent's Markdown is not the core's blocks: $agent" }

    Qa $dir @('chatpage', 'reply', '0') | Out-Null
    Qa $dir @('chatpage', 'type', 'A reply') | Out-Null
    $reply = Qa $dir @('chatpage', 'send')
    if ($reply -notmatch 'reply quote "[^"]+: Hello there from the PC"') { Fail "the reply has no quote: $reply" }
    # Build 49: the run's meta line under its last message (yours here: "You", no device), a quote's click jumps to the
    # original, the controls show under the pointer, and Escape in the field cancels a reply (and goes on without one).
    if ($reply -notmatch 'bubbles False, last who Mine, last meta "You . \S+') { Fail "the reply is not yours with its meta line: $reply" }
    $jump = Qa $dir @('chatpage', 'quote', '2')
    if ($jump -notmatch 'jumped to 0') { Fail "the quote did not jump to the original: $jump" }
    $hover = Qa $dir @('chatpage', 'hover', '1', 'on')
    if ($hover -notmatch 'hover') { Fail "no controls under the pointer: $hover" }
    Qa $dir @('chatpage', 'hover', '1', 'off') | Out-Null
    Qa $dir @('chatpage', 'reply', '1') | Out-Null
    Shot 'chat-reply-bar'
    $esc = Qa $dir @('chatpage', 'escape')
    if ($esc -notmatch 'reply cancelled; .*replying none') { Fail "Escape did not cancel the reply: $esc" }
    $again = Qa $dir @('chatpage', 'escape')
    if ($again -notmatch 'passed on') { Fail "Escape without a reply was not passed on: $again" }

    # A picture and a text file of the temp state's own.
    Add-Type -AssemblyName System.Drawing
    $bmp = New-Object System.Drawing.Bitmap 320, 200
    $g = [System.Drawing.Graphics]::FromImage($bmp); $g.Clear([System.Drawing.Color]::SteelBlue); $g.FillEllipse([System.Drawing.Brushes]::Gold, 110, 50, 100, 100); $g.Dispose()
    $bmp.Save("$dir\picture.png", [System.Drawing.Imaging.ImageFormat]::Png); $bmp.Dispose()
    'Notes for the team' | Set-Content "$dir\notes.txt"
    $att = Qa $dir @('chatpage', 'attach', "$dir\picture.png")
    if ($att -notmatch 'attached 1') { Fail "the picture was not attached: $att" }
    Qa $dir @('chatpage', 'type', 'A picture') | Out-Null
    Shot 'chat-composer'
    Qa $dir @('chatpage', 'send') | Out-Null
    $img = WaitFor { $s = Qa $dir @('chatpage'); if ($s -match 'last images 1 \[Available\]') { $s } } 15000 'the picture in the conversation'
    Qa $dir @('chatpage', 'attach', "$dir\notes.txt") | Out-Null
    Qa $dir @('chatpage', 'send') | Out-Null
    $file = WaitFor { $s = Qa $dir @('chatpage'); if ($s -match 'last files 1 \[Shared:[^\]]*StopSharing') { $s } } 15000 'the shared file card'
    Shot 'chat-channel'
    $stopped = Qa $dir @('chatpage', 'file', '4', '0', 'StopSharing')
    $unshared = WaitFor { $s = Qa $dir @('chatpage'); if ($s -match 'last files 1 \[Unshared') { $s } } 10000 'Stop Sharing'

    $tag = Qa $dir @('chatpage', 'tag', '0', 'Seen')
    if ($tag -match 'no such tag') { Fail "no Seen in the tag menu: $tag" }
    $tagged = WaitFor { $s = Qa $dir @('chatpage'); if ($s -match 'tags total [1-9]') { $s } } 10000 'the tag on the first card'

    $menu = Qa $dir @('chatpage', 'menu-text')
    if ($menu -notmatch 'Mute' -or $menu -notmatch 'New Topic' -or $menu -notmatch 'Leave') { Fail "the ⋯ menu lacks Mute, New Topic or Leave: $menu" }
    Qa $dir @('chatpage', 'menu', 'Mute') | Out-Null
    $muted = Qa $dir @('chatpage', 'menu-text')
    if ($muted -notmatch 'Unmute') { Fail "Mute did not mute: $muted" }
    $topic = Qa $dir @('chatpage', 'menu', 'New Topic')
    if ($topic -notmatch 'ran NewTopic \(dialog\)') { Fail "New Topic… did not open its dialog: $topic" }
    Shot 'chat-new-topic' -Dialog
    Qa $dir @('dialog', 'cancel') | Out-Null

    # Someone typing keeps the field, the text and the caret whatever happens on the page: a message arrives, the page
    # redraws, a tag's note is sent. A message they send themselves empties the field and leaves them in it.
    Qa $dir @('chatpage', 'type', 'Half a thought') | Out-Null
    function Typing([string]$when, [string]$said) {
      if ($said -notmatch 'field "Half a thought" focus True caret 4;') { Fail "$when, the field lost the focus, its text or its caret: $said" }
    }
    Typing 'with the focus just given' (Qa $dir @('chatpage', 'focus', '4'))
    $before = [int]([regex]::Match((Qa $dir @('chatpage')), 'cards (\d+)').Groups[1].Value)
    'Someone else writes' | & $Cli --socket "$dir\state\control.sock" chat send 'QA-Chat' --as QA-Agent - | Out-Null
    Typing 'after a message arrived' (WaitFor { $s = Qa $dir @('chatpage'); if ($s -match "cards $($before + 1),") { $s } } 10000 'the message to arrive')
    Typing 'after a redraw' (Qa $dir @('chatpage', 'redraw'))
    $noted = Qa $dir @('chatpage', 'tag', '1', 'Seen')
    if ($noted -match 'no such tag') { Fail "no Seen in the second card's tag menu: $noted" }
    Typing 'after a tag was sent' $noted
    Qa $dir @('chatpage', 'type', 'Sent from the field') | Out-Null
    $own = Qa $dir @('chatpage', 'send')
    if ($own -notmatch 'field "" focus True caret 0;') { Fail "the field lost the focus when its message was sent: $own" }
    # A click on Send goes on in the field (as on the Mac, where a button never takes the keyboard focus).
    Qa $dir @('chatpage', 'type', 'Sent with a click') | Out-Null
    $clicked = Qa $dir @('chatpage', 'click-send')
    if ($clicked -notmatch 'sent; .*last "Sent with a click"') { Fail "the click did not send: $clicked" }
    if ($clicked -notmatch 'field "" focus True caret 0;') { Fail "the focus did not come back to the field after a click on Send: $clicked" }
    Qa $dir @('chatpage', 'focus', '0') | Out-Null

    $members = Qa $dir @('chatpage', 'members')
    if ($members -notmatch 'members panel [1-9] rows, open True') { Fail "the members panel has no rows: $members" }

    # The page's reads never hold the UI thread (CI saw a 2 s stall after a send): each read here takes longer than
    # the watchdog's fail_ms, so a read back on the UI thread fails this run every time (Stop-App reads the watchdog).
    Qa $dir @('chatpage', 'slow-read', '2500') 60000 | Out-Null
    Qa $dir @('chatpage', 'type', 'Sent while reads are slow') 60000 | Out-Null
    $slow = Qa $dir @('chatpage', 'send') 60000
    if ($slow -notmatch 'sent; .*last "Sent while reads are slow"') { Fail "the message was not sent and drawn while reads were slow: $slow" }
    Qa $dir @('chatpage', 'slow-read', '0') 60000 | Out-Null

    # Build 52: a picture dropped onto the field's text leaves its reference where it lands (the core's spacing).
    Qa $dir @('chatpage', 'type', 'Hello world') | Out-Null
    # The drop's hit-test at a point: just inside the leading edge of the 'w' of "world" (character 6) is index 6.
    $dropped = Qa $dir @('chatpage', 'drop-over', '6', "$dir\picture.png")
    if ($dropped -notmatch 'hit 6,') { Fail "the drop's point did not hit character 6: $dropped" }
    if ($dropped -notmatch 'draft "Hello !\[Image \d+\]\(att:\d+\) ?world"') { Fail "the dropped picture's reference is not where it landed: $dropped" }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  $pictures.ToArray()
}

# The failure: the UI thread waits for a save of chat's state, on the UI thread or for a lock held across it (CI saw
# 2 s after a send on 7152a8a). Every save here takes longer than the watchdog's fail_ms (SEEDWELL_QA_SLOW_SAVES, temp
# states only), so one such wait fails this run every time (Stop-App reads the watchdog): sending text, a picture and a
# file, and the read mark of a message that arrives. A core that ignored the switch would pass without having slowed
# anything, so its line in core.log comes first.
$checks.chatslowdisk = {
  $dir = New-State 'control'
  $env:SEEDWELL_QA_SLOW_SAVES = '3000'
  try { $p = Start-App $dir -QaControl }
  finally { Remove-Item env:SEEDWELL_QA_SLOW_SAVES -ErrorAction SilentlyContinue }
  try {
    WaitFor { CoreLog $dir | Where-Object { $_ -like '*QA: every save takes 3000 ms longer*' } } 60000 'the core to slow its saves (core.log)' | Out-Null
    WaitFor { & $Cli --socket "$dir\state\control.sock" sync settings 2>&1 | Out-Null; $LASTEXITCODE -eq 0 } 60000 'folder sync to start' | Out-Null
    & $Cli --socket "$dir\state\control.sock" chat new 'QA-Slow' | Out-Null
    if ($LASTEXITCODE -ne 0) { Fail "the CLI could not make a channel (exit $LASTEXITCODE)" }
    Qa $dir @('window') 60000 | Out-Null
    Qa $dir @('place', 'chat', 'QA-Slow') 60000 | Out-Null

    Qa $dir @('chatpage', 'type', 'Text on a slow disk') 60000 | Out-Null
    $text = Qa $dir @('chatpage', 'send') 120000
    if ($text -notmatch 'sent; .*last "Text on a slow disk"') { Fail "the text was not sent and drawn: $text" }

    Add-Type -AssemblyName System.Drawing
    $bmp = New-Object System.Drawing.Bitmap 64, 64
    $bmp.Save("$dir\picture.png", [System.Drawing.Imaging.ImageFormat]::Png); $bmp.Dispose()
    Qa $dir @('chatpage', 'attach', "$dir\picture.png") 60000 | Out-Null
    Qa $dir @('chatpage', 'send') 120000 | Out-Null
    WaitFor { (Qa $dir @('chatpage') 60000) -match 'last images 1 \[Available\]' } 120000 'the picture in the conversation' | Out-Null

    'Notes on a slow disk' | Set-Content "$dir\notes.txt"
    Qa $dir @('chatpage', 'attach', "$dir\notes.txt") 60000 | Out-Null
    Qa $dir @('chatpage', 'send') 120000 | Out-Null
    WaitFor { (Qa $dir @('chatpage') 60000) -match 'last files 1 \[Shared:' } 120000 'the shared file card' | Out-Null

    # A message from another device arrives while the page is on screen: it counts as read, and that is saved. (Only
    # another device's: this device's own agents write under its key, and those are never unread.)
    $other = New-State 'full'
    $po = $null
    try {
      $po = Start-App $other -QaControl
      WaitForSync $other
      $link = ((& $Cli --socket "$dir\state\control.sock" chat share 'QA-Slow' --role read_write --json 2>&1) -join '' | ConvertFrom-Json).link
      $link | & $Cli --socket "$other\state\control.sock" chat join - 2>&1 | Out-Null
      WaitFor { 'Read on a slow disk' | & $Cli --socket "$other\state\control.sock" chat send 'QA-Slow' --as QA-Agent - 2>&1 | Out-Null; $LASTEXITCODE -eq 0 } 120000 'the other device to be admitted' | Out-Null
      $read = WaitFor { $s = Qa $dir @('chatpage') 60000; if ($s -match 'last "Read on a slow disk"') { $s } } 120000 'the message from the other device'
      if ($read -notmatch 'last who TheirAgent') { Fail "the message is not another device's, so nothing was unread: $read" }
      WaitFor { @((& $Cli --socket "$dir\state\control.sock" chat list --json 2>&1 | Out-String | ConvertFrom-Json).channels | Where-Object { $_.name -eq 'QA-Slow' -and $_.unread -eq 0 }).Count -eq 1 } 120000 'the read mark' | Out-Null
    } finally { Stop-App $other $po; Remove-Item -Recurse -Force $other }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @()
}

# Build 42's platform parts on Windows: a channel's key goes into the DPAPI vault (<state>\sync\vault\*.sealed, no
# plain secrets file for a new state); another Seedwell on this PC is named (the process list: the user's app, which
# runs from its install folder); the players are found (the registry and PATH; the line names what was found).
$checks.platform = {
  $dir = New-State 'control'
  $p = Start-App $dir -QaControl
  try {
    WaitForSync $dir
    & $Cli --socket "$dir\state\control.sock" chat new 'QA-Vault' | Out-Null
    if ($LASTEXITCODE -ne 0) { Fail "the CLI could not make a channel (exit $LASTEXITCODE)" }
    WaitFor { Get-ChildItem "$dir\state\sync\vault" -Filter *.sealed -ErrorAction SilentlyContinue } 10000 'a sealed key in the vault' | Out-Null
    $plain = Get-Item "$dir\state\sync\secrets" -ErrorAction SilentlyContinue
    if ($plain -and $plain.Length -gt 0) { Fail "a new state keeps its keys unsealed in sync\secrets ($($plain.Length) bytes)" }
    $others = Qa $dir @('others')
    $mine = Get-Process Seedwell -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$env:LOCALAPPDATA\Programs\Seedwell\*" }
    if ($mine -and $others -notmatch 'Another Seedwell is running|other Seedwells are running') { Fail "the user's app runs, but no other Seedwell is named: $others" }
    $players = Qa $dir @('play-players')
    if ((Test-Path "$env:ProgramFiles\VideoLAN\VLC\vlc.exe") -and $players -notmatch '(?i)vlc') { Fail "VLC is installed, but not found: $players" }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @()
}

# A slow core never holds the UI thread (build 42, the app never hangs): the core starts off it, here made to take 4 s
# (SEEDWELL_QA_SLOW_CORE_MS, isolated runs only). The tray icon comes first; a second launch's command that arrives
# meanwhile waits and runs once the core is there; the watchdog notes no stall.
$checks.slowstart = {
  $dir = New-State
  $env:SEEDWELL_QA_SLOW_CORE_MS = '4000'
  try { $p = Start-Process $Exe -ArgumentList @('--state-dir', "$dir\state", '--qa-control') -PassThru }
  finally { Remove-Item env:SEEDWELL_QA_SLOW_CORE_MS -ErrorAction SilentlyContinue }
  try {
    WaitFor { Log $dir | Where-Object { $_ -like '*tray: icon created*' } } 10000 'the tray icon before the core' | Out-Null
    if (Log $dir | Where-Object { $_ -like '*launch: core ready*' }) { Fail 'the core was ready before the icon: nothing to show for a slow core' }
    Launch $dir @('--qa', 'window') | Out-Null
    WaitFor { Log $dir | Where-Object { $_ -like '*qa window: waits for the core*' } } 5000 'the command to wait for the core' | Out-Null
    $ready = WaitFor { Log $dir | Where-Object { $_ -like '*launch: core ready after*' } | Select-Object -First 1 } 20000 'the core'
    if ($ready -notmatch 'after (\d+) ms' -or [int]$Matches[1] -lt 4000) { Fail "the core did not take the 4 s it was made to: $ready" }
    WaitFor { Log $dir | Where-Object { $_ -like '*qa window: *' -and $_ -notlike '*waits for the core*' } } 10000 'the waiting command to run' | Out-Null
    if (-not (Log $dir | Where-Object { $_ -like '*launch: 1 waited for the core*' })) { Fail 'the waiting command was not run from the queue' }
    $stall = Log $dir | Where-Object { $_ -like '*ui stall:*' } | Select-Object -First 1
    if ($stall) { Fail "the UI thread stalled while the core started: $stall" }
    WaitFor { & $Cli --socket "$dir\state\control.sock" status 2>&1 | Out-Null; $LASTEXITCODE -eq 0 } 10000 'the core to answer' | Out-Null
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @()
}

# The failure: the start after the core is ready runs as one turn of the UI thread, so the app does not answer until all
# of it is done (2.3 s under a Rust compile). Each step is made to take 900 ms (SEEDWELL_QA_SLOW_START_STEP_MS, isolated
# runs only): the three before the flyout's (making that window pumps messages, so a beat gets through there) are past
# the watchdog's fail_ms together, one alone is not, so only steps in turns of their own pass. And what arrives between
# the steps finds no half-made app: a command waits for the end, a core event (the settings changing) too, and a quit
# between them ends the app cleanly.
$checks.startturns = {
  function Slow([string]$dir) {
    $env:SEEDWELL_QA_SLOW_START_STEP_MS = '900'
    try { Start-Process $Exe -ArgumentList @('--state-dir', "$dir\state", '--qa-control') -PassThru }
    finally { Remove-Item env:SEEDWELL_QA_SLOW_START_STEP_MS -ErrorAction SilentlyContinue }
  }
  $dir = New-State
  $p = Slow $dir
  try {
    WaitFor { Log $dir | Where-Object { $_ -like '*launch: core ready after*' } } 20000 'the core' | Out-Null
    Add-Content -Encoding utf8 "$dir\state\config.toml" '# changed while the app starts'
    Launch $dir @('--qa', 'window') | Out-Null
    $steps = WaitFor { Log $dir | Where-Object { $_ -like '*launch: on the UI thread, ms:*' } | Select-Object -First 1 } 20000 'the steps to end'
    if (-not (Log $dir | Where-Object { $_ -like '*qa window: waits for the core*' })) { Fail 'the command did not wait for the steps' }
    WaitFor { Log $dir | Where-Object { $_ -like '*qa window: *' -and $_ -notlike '*waits for the core*' } } 10000 'the waiting command to run' | Out-Null
    $took = @([regex]::Matches($steps, ' (\d+)(,|$)') | ForEach-Object { [int]$_.Groups[1].Value })
    if ($took.Count -lt 5 -or ($took | Measure-Object -Sum).Sum -lt 3000) { Fail "the steps did not take the time they were made to: $steps" }
    if ($p.HasExited) { Fail "the app ended with $($p.ExitCode): $((Log $dir | Select-Object -Last 5) -join ' | ')" }
    $window = Qa $dir @('shot', 'after-steps')
    if ($window -match 'failed') { Fail "the window after the steps: $window" }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue }

  $dir = New-State
  $p = Slow $dir
  try {
    WaitFor { Log $dir | Where-Object { $_ -like '*launch: core ready after*' } } 20000 'the core' | Out-Null
    Launch $dir @('--quit') | Out-Null
    if (-not $p.WaitForExit(30000)) { Fail 'a quit during the steps did not end the app within 30 s' }
    $done = Log $dir | Where-Object { $_ -like '*quit: shutdown*' } | Select-Object -Last 1
    if ($done -notmatch 'finished True') { Fail "a quit during the steps: no finished shutdown: $((Log $dir | Select-Object -Last 5) -join ' | ')" }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue }
  @()
}

# The failure: a QA shot encodes its PNG on the UI thread (it stalled 2.2 s twice under a Rust compile, which failed the
# run). Writing a picture is made to take 3 s (SEEDWELL_QA_SLOW_SHOT_MS, isolated runs only), past the watchdog's fail_ms:
# it passes only off the UI thread, and the picture must still be there, whole, when the command answers.
$checks.shotturns = {
  $dir = New-State
  $env:SEEDWELL_QA_SLOW_SHOT_MS = '3000'
  try { $p = Start-App $dir -QaControl } finally { Remove-Item env:SEEDWELL_QA_SLOW_SHOT_MS -ErrorAction SilentlyContinue }
  try {
    Qa $dir @('window') | Out-Null
    $said = Qa $dir @('shot', 'slow') 30000
    if ($said -match 'failed') { Fail "the shot: $said" }
    $bytes = [IO.File]::ReadAllBytes("$dir\state\qa\slow.png")
    if ($bytes.Length -lt 1000 -or $bytes[1] -ne 0x50 -or $bytes[-4] -ne 0xAE) { Fail "the picture is not a whole PNG ($($bytes.Length) bytes)" }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue }
  @()
}

# The failures: every core error tried again for a minute (or none), so a lasting one fails late and a file another
# program holds is lost. The core's Busy (held with no sharing, opened just after the host looked that it is closed) is
# offered again and works once let go, for a new file and for a change; Busy's answer to a callback is "in use"; a file
# that goes away before the core is asked (a lasting error) fails at once, said once, never offered again.
$checks.ondemandbusy = {
  $dir = New-State 'full'
  $docs = "$dir\state\od\Docs"
  New-Item -ItemType Directory -Force $docs | Out-Null
  'alpha text' | Set-Content -NoNewline "$docs\a.txt"
  'bravo text' | Set-Content -NoNewline "$docs\b.txt"
  $sock = "$dir\state\control.sock"
  $p = Start-App $dir -QaControl
  try {
    WaitForSync $dir
    $added = (& $Cli --socket $sock sync add $docs 2>&1) -join ' '
    if ($added -notlike 'Sharing Docs*') { throw "sync add: $added" }
    $mode = (& $Cli --socket $sock sync mode Docs on-demand --free-space 2>&1) -join ' '
    if ($mode -notlike 'Docs is on demand*') { throw "sync mode: $mode" }
    WaitFor { (Qa $dir @('ondemand')) -match 'connected True placed 2 files \[a\.txt:p, b\.txt:p\]' } 20000 'the placeholders' | Out-Null
    $read = Qa $dir @('ondemand', 'read', 'a.txt')
    if ($read -notmatch 'read "alpha text"') { throw "read a.txt: $read" }
    WaitFor { Log $dir | Where-Object { $_ -like '*on-demand: a.txt filled (*' } } 5000 'the core to adopt a.txt' | Out-Null
    $status = Qa $dir @('ondemand', 'busy-status')
    if ($status -notmatch 'busy status 0xC000CF14') { throw "Busy's answer to a callback is not STATUS_CLOUD_FILE_IN_USE: $status" }

    # A new file, held.
    Qa $dir @('ondemand', 'hold', 'new.txt', 'held text') | Out-Null
    WaitFor { Log $dir | Where-Object { $_ -like '*on-demand: new.txt offered again*' } } 10000 'a held new file to be offered again' | Out-Null
    Start-Sleep -Milliseconds 1500
    if (Log $dir | Where-Object { $_ -like '*on-demand: new.txt added here*' -or $_ -like '*on-demand: new.txt not added*' }) { Fail "a held file was added or given up: $((Log $dir | Select-Object -Last 5) -join ' | ')" }
    Qa $dir @('ondemand', 'release') | Out-Null
    WaitFor { Log $dir | Where-Object { $_ -like '*on-demand: new.txt added here*' } } 10000 'the new file to be added once let go' | Out-Null

    # A change, held.
    $before = (Log $dir).Count
    Qa $dir @('ondemand', 'hold', 'a.txt', 'changed while held') | Out-Null
    WaitFor { Log $dir | Select-Object -Skip $before | Where-Object { $_ -like '*on-demand: a.txt offered again*' } } 10000 'a held change to be offered again' | Out-Null
    Start-Sleep -Milliseconds 1500
    if (Log $dir | Select-Object -Skip $before | Where-Object { $_ -like '*on-demand: a.txt changed here*' -or $_ -like '*on-demand: a.txt not reported*' }) { Fail "a held change was reported or given up: $((Log $dir | Select-Object -Last 5) -join ' | ')" }
    Qa $dir @('ondemand', 'release') | Out-Null
    WaitFor { Log $dir | Select-Object -Skip $before | Where-Object { $_ -like '*on-demand: a.txt changed here*' } } 15000 'the change to be reported once let go' | Out-Null

    # A file overwritten (no placeholder any more), held.
    Qa $dir @('ondemand', 'read', 'b.txt') | Out-Null
    WaitFor { Log $dir | Where-Object { $_ -like '*on-demand: b.txt filled (*' } } 5000 'the core to adopt b.txt' | Out-Null
    Qa $dir @('ondemand', 'hold-overwrite', 'b.txt', 'overwritten while held') | Out-Null
    WaitFor { Log $dir | Where-Object { $_ -like '*on-demand: b.txt offered again*' } } 10000 'a held overwrite to be offered again' | Out-Null
    Start-Sleep -Milliseconds 1500
    if (Log $dir | Where-Object { $_ -like '*on-demand: b.txt changed here*' -or $_ -like '*on-demand: b.txt overwritten, not reported*' }) { Fail "a held overwrite was reported or given up: $((Log $dir | Select-Object -Last 5) -join ' | ')" }
    Qa $dir @('ondemand', 'release') | Out-Null
    WaitFor { Log $dir | Where-Object { $_ -like '*on-demand: b.txt changed here*' } } 15000 'the overwrite to be reported once let go' | Out-Null

    # A lasting error.
    Qa $dir @('ondemand', 'vanish', 'gone.txt') | Out-Null
    WaitFor { Log $dir | Where-Object { $_ -like '*on-demand: gone.txt not added*' } } 10000 'a vanished file to fail at once' | Out-Null
    Start-Sleep -Milliseconds 1500
    $tries = @(Log $dir | Where-Object { $_ -like '*on-demand: gone.txt*' })
    if ($tries.Count -ne 1) { Fail "a lasting error must be said once and never offered again: $($tries -join ' || ')" }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue }
  @()
}

# The failures: a big folder added "On all my devices" holds up the UI thread while it is added (the Mac froze on 914 GB
# in 407 files, on demand for the other devices, 2026-11-06), and the Share dialog opening by itself after that add (the
# user wants sharing from the right-click menu only). The same folder here, in sparse files, both PCs' watchdogs watching:
# the Add's Start, the page after it, and B taking it up. And a PC downloading all of it: B must get placeholders, a working
# Cloud Files root with nothing fetched and no "joined in full" on its page, which still works after a restart: a read
# fetches, the system frees it again (B stays capped at 1 MB/s, a safety net for this disk). An add without "On all my
# devices" still opens Share. Every stall the watchdog noted, short ones too, is printed.
$checks.interactive = {
  # The failure: a large folder's add leaves the app sluggish in its first seconds, in holds each too short for the watchdog
  # (a report at 1 s), or a core lock held for seconds that the UI happens not to wait on. The same 914 GB as addbigfolder,
  # added through the Add window on all my devices, on demand for the others, with B joining. A baseline of 10 s without an
  # add, then 15 s from the press of Start, both apps cycling through every surface throughout. During the add: no beat over
  # 100 ms outside the renderer's own captures, every surface command within 500 ms, every CLI round trip within 1 s, the core
  # probe under 500 ms. A miss is a finding: the limits are the user's ask, not to be tuned. First the verdict itself is proved
  # against a hold of the UI thread, a capture and a slow core answer.
  function Verdict($report, $cli, [string]$who) {
    $misses = @()
    if ($report.ui.over100 -gt 0) { $misses += "$who beat of $($report.ui.max) ms (last action: $($report.ui.maxAction)), $($report.ui.over100) over 100 ms" }
    $slow = @($report.commands | Where-Object { $_.ms -gt 500 })
    foreach ($c in ($slow | Sort-Object ms -Descending | Select-Object -First 5)) { $misses += "$who surface command '$($c.name)' took $($c.ms) ms at $($c.at) s" }
    if ($slow.Count -gt 5) { $misses += "$who $($slow.Count) surface commands over 500 ms in all" }
    foreach ($c in @($report.commands | Where-Object { $_.result -like 'failed:*' -or $_.result -like 'not *' -or $_.result -like 'no *' })) { $misses += "$who surface command '$($c.name)' answered: $($c.result)" }
    foreach ($c in @($cli | Where-Object { $_.ms -gt 1000 })) { $misses += "$who CLI $($c.what) took $($c.ms) ms at $($c.at) s" }
    if ($report.core.max -ge 500) { $misses += "$who core probe of $($report.core.max) ms (last action: $($report.core.worst[0].action))" }
    if ($report.core.failures -gt 0) { $misses += "$who core probe failed $($report.core.failures) times" }
    $misses
  }
  function ReadReport([string]$file) { Get-Content $file -Raw -Encoding utf8 | ConvertFrom-Json }
  function Worst($items) { if ($items) { ($items | Measure-Object -Maximum).Maximum } else { 0 } }
  # A phase on both apps: probes, the cycle, and the CLI timed from outside until both cycles are over.
  function Phase([string]$name, [double]$seconds, [string]$channel, [string]$firstOnA, [string]$folder, [scriptblock]$each) {
    $words = @('interactive', 'cycle', "$seconds", "channel=$channel") + $(if ($folder) { @("folder=$folder") } else { @() })
    Qa $a @('interactive', 'start') | Out-Null
    Qa $b @('interactive', 'start') | Out-Null
    Qa $b $words | Out-Null
    Qa $a ($words + $(if ($firstOnA) { @("first=$firstOnA") } else { @() })) | Out-Null
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $cliA = New-Object Collections.ArrayList; $cliB = New-Object Collections.ArrayList
    while ($clock.Elapsed.TotalSeconds -lt $seconds) {
      foreach ($d in @{ list = $cliA; sock = "$a\state\control.sock"; who = 'A' }, @{ list = $cliB; sock = "$b\state\control.sock"; who = 'B' }) {
        # (--version: the CLI's start alone, no connection: a slow sync list beside a slow --version is the process start.)
        foreach ($what in @('status'), @('sync', 'list'), @('--version')) {
          $at = $clock.Elapsed.TotalSeconds
          $one = [Diagnostics.Stopwatch]::StartNew()
          $said = & $Cli --socket $d.sock @what 2>&1 | Out-String
          $d.list.Add([pscustomobject]@{ what = $what -join ' '; ms = [math]::Round($one.Elapsed.TotalMilliseconds); at = [math]::Round($at, 1); ok = ($LASTEXITCODE -eq 0) }) | Out-Null
          if ($each) { & $each $d.who $what $said $at }
        }
      }
    }
    # A slow sync list: which call of the phase it was, and the CLI's start alone (--version) just before and after it.
    foreach ($c in @{ who = 'A'; list = $cliA }, @{ who = 'B'; list = $cliB }) {
      $all = @($c.list)
      $lists = @($all | Where-Object { $_.what -eq 'sync list' })
      for ($k = 0; $k -lt $lists.Count; $k++) {
        if ($lists[$k].ms -lt 300) { continue }
        $at = [array]::IndexOf($all, $lists[$k])
        $versions = @($all[[math]::Max(0, $at - 3)..[math]::Min($all.Count - 1, $at + 3)] | Where-Object { $_.what -eq '--version' } | ForEach-Object { "$($_.ms) ms" })
        $versionMax = @($all | Where-Object { $_.what -eq '--version' } | Measure-Object ms -Maximum).Maximum
        Write-Host ("  interactive: {0} {1}: slow sync list, call {2} of {3}, {4} ms at {5} s; --version beside it: {6}; --version max in the phase {7} ms" -f $name, $c.who, ($k + 1), $lists.Count, $lists[$k].ms, $lists[$k].at, ($versions -join ', '), $versionMax)
      }
    }
    foreach ($d in $a, $b) { WaitFor { (Qa $d @('interactive', 'done')) -match 'finished' } 30000 'the cycle to end' | Out-Null }
    $ra = (Qa $a @('interactive', 'report', "$name-a") 20000); $rb = (Qa $b @('interactive', 'report', "$name-b") 20000)
    foreach ($r in $ra, $rb) { if ($r -match 'no probe|failed') { Fail "the probe's report: $r" } }
    Copy-Item "$a\state\qa\$name-a.json", "$b\state\qa\$name-b.json" $Out
    @{ A = @{ report = ReadReport "$a\state\qa\$name-a.json"; cli = $cliA }; B = @{ report = ReadReport "$b\state\qa\$name-b.json"; cli = $cliB } }
  }
  function Row([string]$phase, [string]$who, $p) {
    $r = $p.report; $worstCommand = $r.commands | Sort-Object ms -Descending | Select-Object -First 1
    $status = Worst (@($p.cli | Where-Object { $_.what -eq 'status' }) | ForEach-Object { $_.ms })
    $list = Worst (@($p.cli | Where-Object { $_.what -eq 'sync list' }) | ForEach-Object { $_.ms })
    $version = Worst (@($p.cli | Where-Object { $_.what -eq '--version' }) | ForEach-Object { $_.ms })
    "| $phase | $who | $($r.ui.beats) | $($r.ui.max) ($($r.ui.maxAction -replace '^qa ', '')) | $($r.ui.p99) | $($r.ui.over50) | $($r.ui.over100) | $($r.ui.own.beats) / $($r.ui.own.max) | $($r.commands.Count) | $($worstCommand.ms) ($($worstCommand.name)) | $(@($r.commands | Where-Object { $_.ms -gt 500 }).Count) | $status | $list | $version | $($r.core.calls) | $($r.core.max) | $($r.core.p99) |"
  }

  $a = New-State 'control'; $b = New-State 'control'
  $movies = "$a\Movies"
  New-Item -ItemType Directory -Force $movies | Out-Null
  $size = [long](914GB / 407)
  foreach ($i in 1..407) { [QaCloud]::Sparse(("$movies\Film {0:000}.mkv" -f $i), $size) }
  'note text' | Set-Content -NoNewline "$movies\note.txt"
  $pa = Start-App $a -QaControl
  $env:SEEDWELL_QA_LIMIT_LAN = '1'
  try { $pb = Start-App $b -QaControl } finally { Remove-Item Env:SEEDWELL_QA_LIMIT_LAN }
  try {
    WaitForSync $a; WaitForSync $b
    & $Cli --socket "$b\state\control.sock" sync settings --down 1M 2>&1 | Out-Null
    foreach ($d in $a, $b) {
      & $Cli --socket "$d\state\control.sock" chat new 'QA-Live' | Out-Null
      if ($LASTEXITCODE -ne 0) { Fail "the CLI could not make a channel (exit $LASTEXITCODE)" }
      Qa $d @('window') | Out-Null
    }

    # The verdict sees what it is there to see: a hold of the UI thread under the watchdog's report, with what was done last;
    # the same hold as a capture, said apart; a slow core answer. And none of them is a miss when it did not happen.
    Qa $a @('interactive', 'start') | Out-Null
    Qa $a @('hangwatch', 'hold', '250', '1') | Out-Null
    Qa $a @('hangwatch', 'hold', '250', '1', 'capture') | Out-Null
    Qa $a @('interactive', 'core-delay', '700') | Out-Null
    WaitFor { Start-Sleep -Milliseconds 300; $true } 1000 'the core probe' | Out-Null
    Qa $a @('interactive', 'report', 'proof') 20000 | Out-Null
    $proof = ReadReport "$a\state\qa\proof.json"
    $said = (Verdict $proof @([pscustomobject]@{ what = 'status'; ms = 1500; at = 0 }) 'proof') -join ' | '
    if ($said -notmatch 'beat of .*hangwatch hold 250 1\)') { Fail "a hold of 250 ms is no miss, or not named: $said" }
    if ($proof.ui.maxAction -match 'capture') { Fail "a capture counted against the verdict: $($proof.ui | ConvertTo-Json -Compress)" }
    if ($proof.ui.own.max -lt 200) { Fail "the capture's hold is not said apart: $($proof.ui.own | ConvertTo-Json -Compress)" }
    if ($said -notmatch 'core probe of') { Fail "a slow core answer is no miss: $said" }
    if ($said -notmatch 'CLI status took') { Fail "a slow CLI answer is no miss: $said" }
    if ($proof.ui.beats -lt 20) { Fail "the probe hardly ran: $($proof.ui.beats) beats" }
    if ($proof.core.calls -lt 5) { Fail "the core probe hardly ran: $($proof.core.calls) calls" }

    # Paired before the baseline, as the add will be: On all my devices needs another device, and the two phases differ by the add alone.
    $link = [regex]::Match((Qa $a @('pair', 'link')), 'link (\S+) ;').Groups[1].Value
    if (-not $link) { Fail 'A made no pairing link' }
    Qa $b @('pair', 'join', $link) 30000 | Out-Null
    WaitFor { $s = Qa $a @('pair', 'status'); if ($s -match 'requests 1') { $s } } 30000 'B asking to pair on A' | Out-Null
    Qa $a @('pair', 'confirm') 30000 | Out-Null
    WaitFor { $s = Qa $b @('pair', 'status'); if ($s -match 'devices Paired') { $s } } 30000 'B to be paired' | Out-Null

    $base = Phase 'baseline' 10 'QA-Live' '' '' $null

    Qa $a @('place', 'foldersync') | Out-Null
    Qa $a @('add-folder', 'open') | Out-Null
    Qa $a @('add-folder', 'folder', $movies) | Out-Null
    Qa $a @('add-folder', 'ondemand', 'on') | Out-Null
    $form = Qa $a @('add-folder', 'devices', 'on')
    if ($form -notmatch 'all devices True enabled True') { Fail "On all my devices is not on: $form" }

    $script:seenOnA = $null; $script:seenOnB = $null
    $add = Phase 'add' 15 'QA-Live' 'add-folder,start' 'Movies' {
      param($who, $what, $said, $at)
      if ($what[0] -eq 'sync' -and $said -match 'Movies') {
        if ($who -eq 'A' -and $null -eq $script:seenOnA) { $script:seenOnA = [math]::Round($at, 1) }
        if ($who -eq 'B' -and $null -eq $script:seenOnB) { $script:seenOnB = [math]::Round($at, 1) }
      }
    }

    # What the window covered: it proves nothing unless the add and B's taking it up happened in it.
    $listed = WaitFor { $f = ((& $Cli --socket "$a\state\control.sock" sync list --json 2>&1) -join '' | ConvertFrom-Json).folders | Where-Object { $_.name -eq 'Movies' }; if ($f -and $f.files -ge 408) { $f } } 60000 'the films to be listed'
    try { $placed = WaitFor { $s = Qa $b @('ondemand'); if ($s -match 'roots 1 \[\S+ connected True placed 408 files') { $s } } 60000 'B to place the films' }
    catch { Fail "$_; B's log: $((Log $b | Select-Object -Last 12 | ForEach-Object { if ($_.Length -gt 300) { $_.Substring(0, 300) } else { $_ } }) -join ' | ')" }
    if ($placed -match 'Film \d+\.mkv:f') { Fail "B fetched a film: $([regex]::Match($placed, 'Film \d+\.mkv:f').Value)" }

    $table = @(
      "Movies seen by the CLI after the cycle's start: A at $script:seenOnA s, B at $script:seenOnB s.",
      '',
      '| phase | app | beats | beat max ms (last action) | p99 | >50 | >100 | own captures beats / max | commands | command max ms | >500 | CLI status max | CLI sync list max | CLI --version max | core calls | core max | core p99 |',
      '|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|',
      (Row 'baseline' 'A' $base.A), (Row 'add' 'A' $add.A), (Row 'baseline' 'B' $base.B), (Row 'add' 'B' $add.B)
    )
    foreach ($p in @{ n = 'baseline A'; r = $base.A.report }, @{ n = 'add A'; r = $add.A.report }, @{ n = 'baseline B'; r = $base.B.report }, @{ n = 'add B'; r = $add.B.report }) {
      $table += ''
      $table += "$($p.n), five worst beats: " + (($p.r.ui.worst | ForEach-Object { "$($_.ms) ms at $($_.at) s ($($_.action -replace '^qa ', ''), GC $($_.gc) ms)" }) -join '; ')
      $table += "$($p.n), collector: $($p.r.gc | ConvertTo-Json -Compress)"
      $table += "$($p.n), five worst core calls: " + (($p.r.core.worst | ForEach-Object { "$($_.ms) ms at $($_.at) s" }) -join '; ')
      $table += "$($p.n), five slowest commands: " + (($p.r.commands | Sort-Object ms -Descending | Select-Object -First 5 | ForEach-Object { "$($_.name) $($_.ms) ms at $($_.at) s" }) -join '; ')
    }
    $table | Set-Content -Encoding utf8 "$Out\interactive.md"
    $table | ForEach-Object { Write-Host "  $_" }

    $misses = @(Verdict $add.A.report $add.A.cli 'A') + @(Verdict $add.B.report $add.B.cli 'B')
    # The failure behind the pauses this check first found: what a surface made stays alive after it is left. Each visit to
    # Settings made a page its own events and timer kept for good; every collection's pause grew with the visits, with no
    # add at all (83 to 242 ms in a minute, the heap from 7 to 24 MB). One page for the run; the heap not several times over.
    foreach ($d in @{ n = 'A'; dir = $a; base = $base.A; add = $add.A }, @{ n = 'B'; dir = $b; base = $base.B; add = $add.B }) {
      $made = Qa $d.dir @('settings', 'made')
      if ($made -notmatch 'made 1;') { $misses += "$($d.n) made more than one Settings page: $made" }
      if ($d.add.report.gc.heapMb -gt [math]::Max(3 * $d.base.report.gc.heapMb, 12)) { $misses += "$($d.n) the heap grew from $($d.base.report.gc.heapMb) MB to $($d.add.report.gc.heapMb) MB: a surface stays alive after it is left" }
    }
    if ($null -eq $script:seenOnB) { $misses += "B did not have the folder within the window: the measure did not cover its taking it up" }
    if ($misses) { Fail "$($misses.Count) misses: $($misses -join ' || ')" }
  } finally {
    Stop-App $b $pb
    Stop-App $a $pa
    Remove-Item -Recurse -Force $a, $b -ErrorAction SilentlyContinue
  }
  @()
}

$checks.addbigfolder = {
  $a = New-State 'control'; $b = New-State 'control'
  $movies = "$a\Movies"
  New-Item -ItemType Directory -Force $movies, "$a\Plain" | Out-Null
  $size = [long](914GB / 407)
  foreach ($i in 1..407) { [QaCloud]::Sparse(("$movies\Film {0:000}.mkv" -f $i), $size) }
  # Small enough to fetch at the cap.
  'note text' | Set-Content -NoNewline "$movies\note.txt"
  $sa = "$a\state\control.sock"
  $pa = Start-App $a -QaControl
  # B at 1 MB/s, the LAN too: however it takes the folder up, it never fills this disk with 914 GB.
  $env:SEEDWELL_QA_LIMIT_LAN = '1'
  try { $pb = Start-App $b -QaControl } finally { Remove-Item Env:SEEDWELL_QA_LIMIT_LAN }
  try {
    WaitForSync $a; WaitForSync $b
    & $Cli --socket "$b\state\control.sock" sync settings --down 1M 2>&1 | Out-Null
    Qa $a @('window') | Out-Null
    Qa $b @('window') | Out-Null
    $link = [regex]::Match((Qa $a @('pair', 'link')), 'link (\S+) ;').Groups[1].Value
    if (-not $link) { Fail 'A made no pairing link' }
    Qa $b @('pair', 'join', $link) 30000 | Out-Null
    WaitFor { $s = Qa $a @('pair', 'status'); if ($s -match 'requests 1') { $s } } 30000 'B asking to pair on A' | Out-Null
    Qa $a @('pair', 'confirm') 30000 | Out-Null
    WaitFor { $s = Qa $b @('pair', 'status'); if ($s -match 'devices Paired') { $s } } 30000 'B to be paired' | Out-Null

    Qa $a @('place', 'foldersync') | Out-Null
    Qa $a @('add-folder', 'open') | Out-Null
    Qa $a @('add-folder', 'folder', $movies) | Out-Null
    Qa $a @('add-folder', 'ondemand', 'on') | Out-Null
    $form = Qa $a @('add-folder', 'devices', 'on')
    if ($form -notmatch 'all devices True enabled True') { Fail "On all my devices is not on: $form" }
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $started = Qa $a @('add-folder', 'start') 60000
    Write-Host "  addbigfolder: Start answered after $($clock.ElapsedMilliseconds) ms"
    if ($started -notmatch 'started') { Fail "the folder was not added: $started" }
    if ($started -match 'share open') { Fail "the Share dialog opened by itself after an add on all my devices: $started" }
    if (-not (Log $a | Where-Object { $_ -like '*add folder: added Movies*share after False*' })) { Fail "the core asked for Share after an add on all my devices: $((Log $a | Select-Object -Last 5) -join ' | ')" }
    # The page goes on answering while the first scan runs, and B takes the folder up.
    $listed = WaitFor {
      Qa $a @('syncpage') | Out-Null
      $f = ((& $Cli --socket $sa sync list --json 2>&1) -join '' | ConvertFrom-Json).folders | Where-Object { $_.name -eq 'Movies' }
      if ($f -and $f.files -ge 408) { $f }
    } 60000 'the films to be listed'
    Write-Host "  addbigfolder: listed $($listed.files) files after $($clock.ElapsedMilliseconds) ms"
    $sb = "$b\state\control.sock"
    function HostOfB { Qa $b @('ondemand') }
    function NoFilms([string]$line) { if ($line -match 'Film \d+\.mkv:f') { Fail "B fetched a film: $([regex]::Match($line, 'Film \d+\.mkv:f').Value)" } }
    try { $placed = WaitFor { $s = HostOfB; if ($s -match 'roots 1 \[\S+ connected True placed 408 files') { $s } } 60000 'B to place the films' }
    catch { Fail "$_; B's host: $((HostOfB) -replace ' files \[.*$', ' files'); B's folders: $((& $Cli --socket $sb sync list 2>&1) -join ' | '); B's log: $((Log $b | Select-Object -Last 6) -join ' | ')" }
    Write-Host "  addbigfolder: B placed them after $($clock.ElapsedMilliseconds) ms: $($placed -replace '^.*qa ondemand: ', '' -replace ' files \[.*$', ' files')"
    NoFilms $placed
    $onB = ((& $Cli --socket $sb sync list --json 2>&1) -join '' | ConvertFrom-Json).folders | Where-Object { $_.name -eq 'Movies' }
    if (-not $onB.on_demand) { Fail "B has the folder in full: $($onB | ConvertTo-Json -Compress)" }
    if (($onB.issues | ConvertTo-Json -Compress) -match 'in full') { Fail "B's page says it joined in full: $($onB.issues | ConvertTo-Json -Compress)" }
    # Both apps go on answering while B works on it.
    foreach ($i in 1..5) { Qa $a @('syncpage') | Out-Null; Qa $b @('place', 'foldersync') | Out-Null; Qa $b @('sync-flyout', 'open') | Out-Null }
    if ((Qa $a @('share', 'status')) -notmatch 'not open') { Fail 'the Share dialog is open after the add on all my devices' }

    # A working root after a restart: nothing placed twice, a read fetches, the system frees it again, nothing else came.
    Stop-App $b $pb
    $env:SEEDWELL_QA_LIMIT_LAN = '1'
    try { $pb = Start-App $b -QaControl } finally { Remove-Item Env:SEEDWELL_QA_LIMIT_LAN }
    WaitForSync $b
    try { WaitFor { (HostOfB) -match 'roots 1 \[\S+ connected True placed 408 files' } 30000 'the root again after a restart' | Out-Null }
    catch { Fail "$_; B's host: $((HostOfB) -replace ' files \[.*$', ' files'); B's log: $((Log $b | Select-Object -Last 6) -join ' | ')" }
    # B's connection to A comes back by itself (a read waits for it, within its time limit).
    $read = WaitFor { $r = Qa $b @('ondemand', 'read', 'note.txt') 60000; if ($r -match 'read "note text"') { $r } } 60000 'the note to be fetched after the restart'
    if ($read -notmatch 'note\.txt:f') { Fail "the note is not filled: $($read -replace ' files \[.*note', ' files [... note')" }
    $place = (((& $Cli --socket $sb sync list --json 2>&1) -join '' | ConvertFrom-Json).folders | Where-Object { $_.name -eq 'Movies' }).path
    WaitFor { Log $b | Where-Object { $_ -like '*on-demand: note.txt filled (*' } } 5000 'the core to adopt the note' | Out-Null
    # (Refused as not in sync for a moment: the host marks it just after it says "filled". One that stays so fails here.)
    try { WaitFor { $script:freed = CfDehydrate "$place\note.txt"; $script:freed -eq 'freed' } 10000 'the system to free the note' | Out-Null }
    catch { Fail "the system could not free the note: $script:freed" }
    WaitFor { (HostOfB) -match 'note\.txt:pU?\b' } 10000 'the note to be freed' | Out-Null
    NoFilms (HostOfB)

    # Without On all my devices: the Share dialog still opens.
    Qa $a @('add-folder', 'open') | Out-Null
    Qa $a @('add-folder', 'folder', "$a\Plain") | Out-Null
    $form = Qa $a @('add-folder', 'devices', 'off')
    if ($form -notmatch 'all devices False') { Fail "On all my devices is not off: $form" }
    Qa $a @('add-folder', 'start') 30000 | Out-Null
    WaitFor { (Qa $a @('share', 'status')) -notmatch 'not open' } 10000 'the Share dialog after an add without On all my devices' | Out-Null
    if (-not (Log $a | Where-Object { $_ -like '*add folder: added Plain*share after True*' })) { Fail 'the core did not ask for Share after a plain add' }
    Qa $a @('share', 'cancel') | Out-Null
    foreach ($d in @{ n = 'A'; dir = $a }, @{ n = 'B'; dir = $b }) {
      $stalls = @(Log $d.dir | Where-Object { $_ -like '*ui stall:*' -or $_ -like '*ui stutter:*' })
      Write-Host "  addbigfolder: $($d.n) noted $($stalls.Count) stalls$(if ($stalls) { ': ' + ($stalls -join ' || ') })"
      Log $d.dir | Where-Object { $_ -like '*launch: on the UI thread*' -or $_ -like '*on-demand folders:*' -or $_ -like '*launch: core ready*' -or $_ -like '*ui stall:*' -or $_ -like '*tray: icon*' } | ForEach-Object { Write-Host "    $($d.n) $_" }
    }
  } finally {
    Stop-App $b $pb
    Stop-App $a $pa
    Remove-Item -Recurse -Force $a, $b -ErrorAction SilentlyContinue
  }
  @()
}

# The failure: a syncing folder's time left is cut or runs under the buttons in the tray's Folder Sync rows. A real sync
# between two temp states at 1 MB/s: the row says the core's time left, whole, with the pointer on it too (the Edit and
# Delete buttons shown), and in a picture.
$checks.timeleft = {
  $a = New-State 'full'; $b = New-State 'full'
  $docs = "$a\Docs"
  New-Item -ItemType Directory -Force $docs | Out-Null
  $bytes = New-Object byte[] (2MB)
  $random = New-Object Random 11
  foreach ($i in 1..40) { $random.NextBytes($bytes); [IO.File]::WriteAllBytes("$docs\part$i.bin", $bytes) }
  $sa = "$a\state\control.sock"; $sb = "$b\state\control.sock"
  $pa = Start-App $a -QaControl
  $pb = $null
  $pictures = @()
  try {
    $env:SEEDWELL_QA_LIMIT_LAN = '1'
    try { $pb = Start-App $b -QaControl } finally { Remove-Item Env:SEEDWELL_QA_LIMIT_LAN }
    WaitForSync $a; WaitForSync $b
    & $Cli --socket $sb sync settings --down 1M 2>&1 | Out-Null
    $added = (& $Cli --socket $sa sync add $docs 2>&1) -join ' '
    if ($added -notlike 'Sharing Docs*') { throw "sync add: $added" }
    $link = ((& $Cli --socket $sa sync share Docs --json 2>&1) -join '' | ConvertFrom-Json).link
    $joined = ($link | & $Cli --socket $sb sync join - --to "$b\state\Sync\Docs" 2>&1) -join ' '
    if ($joined -notlike 'Joining Docs*') { throw "sync join: $joined" }
    Qa $b @('window') | Out-Null
    try { $line = WaitFor { $r = Qa $b @('sync-flyout', 'open'); if ($r -match '"[^"]*\d (sec|min|hour)[^"]* left"') { $r } } 60000 'a syncing row with its time left' }
    catch { Fail "$_; last: $(Qa $b @('sync-flyout', 'status')); sync list: $((& $Cli --socket $sb sync list 2>&1) -join ' | ')" }
    Write-Host "  timeleft: $($line -replace '^.*qa sync-flyout open: ', '' -replace '; place .*$', '')"
    if ($line -match 'trimmed True') { Fail "the row's line is cut: $line" }
    Qa $b @('sync-flyout', 'shot', 'sync-time-left') | Out-Null
    Copy-Item "$b\state\qa\sync-time-left.png" "$Out\sync-time-left.png"
    $pictures += "$Out\sync-time-left.png"
    $hover = Qa $b @('sync-flyout', 'hover', 'on')
    if ($hover -notmatch 'hover True') { Fail "the row is not hovered: $hover" }
    if ($hover -notmatch 'buttons shown True') { Fail "the row's buttons do not show under the pointer: $hover" }
    if ($hover -match 'trimmed True|over it True') { Fail "the row's line is cut, or under the Edit and Delete buttons: $hover" }
    Qa $b @('sync-flyout', 'shot', 'sync-time-left-hover') | Out-Null
    Copy-Item "$b\state\qa\sync-time-left-hover.png" "$Out\sync-time-left-hover.png"
    $pictures += "$Out\sync-time-left-hover.png"
    Write-Host "  timeleft: hover: $($hover -replace '^.*qa sync-flyout hover on: ', '' -replace '; place .*$', '')"
  } finally {
    if ($pb) { Stop-App $b $pb }
    Stop-App $a $pa
    Remove-Item -Recurse -Force $a, $b -ErrorAction SilentlyContinue
  }
  $pictures
}

# The failures: the UI thread held up again and again in stalls each under a second, the app frozen for the user while the
# watchdog says nothing; a run failing as a stutter while it only draws its pictures, or the exclusion hiding the app's own
# stutter. The detector's promises (ported from the Mac's tests), then the real watchdog: holds of 800 ms that are the
# renderer's captures pass, the same holds that are not fail the run with the core's stutter line, and never as a stall.
$checks.hangwatch = {
  $dir = New-State
  $p = Start-App $dir -QaControl
  try {
    $self = Qa $dir @('hangwatch', 'selftest')
    if ($self -notmatch 'detector ok') { Fail "the detector: $self" }
    Qa $dir @('hangwatch', 'hold', '800', '8', 'capture') 30000 | Out-Null
    $any = @(Log $dir | Where-Object { $_ -like '*ui stutter:*' })
    if ($any) { Fail "the renderer's captures were said as a stutter: $($any -join ' || ')" }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue }

  $dir = New-State
  $p = Start-App $dir -QaControl
  try {
    Qa $dir @('hangwatch', 'hold', '800', '8') 30000 | Out-Null
    $line = WaitFor { Log $dir | Where-Object { $_ -like '*ui stutter:*this run fails*' } | Select-Object -First 1 } 5000 'the stutter to fail the run'
    if ($line -notmatch 'busy for [\d.]+ s of 3\.0 s in short stalls \(last action: qa hangwatch hold 800 8\)') { Fail "the stutter's line: $line" }
    $stalls = @(Log $dir | Where-Object { $_ -like '*ui stall:*this run fails*' })
    if ($stalls) { Fail "a hold failed as a stall: $($stalls -join ' || ')" }
  } finally {
    # This run is meant to have failed: Stop-App would say so.
    if (-not $p.HasExited) { Launch $dir @('--quit') | Out-Null; if (-not $p.WaitForExit(30000)) { $p.Kill() } }
    Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
  }
  @()
}

# The failures: opening Share reads the folder's status on the UI thread (the Mac stuttered on it after a 914 GB add); Extend
# reading every folder on the UI thread; an open dialog redrawing each minute for nothing, or closing an open drop-down. The
# form is made to take 3 s (SEEDWELL_QA_SLOW_SHARE_FORM_MS, isolated runs only), past the watchdog's fail_ms: the dialog is
# there at once, opening, with no Copy Link, then real. Extend adds to the period. A refresh that finds nothing changed draws
# nothing; a change made elsewhere is drawn, but not while a drop-down is open.
$checks.shareopening = {
  $dir = New-State 'control'
  New-Item -ItemType Directory -Force "$dir\Shared" | Out-Null
  $sock = "$dir\state\control.sock"
  $env:SEEDWELL_QA_SLOW_SHARE_FORM_MS = '3000'
  try { $p = Start-App $dir -QaControl } finally { Remove-Item env:SEEDWELL_QA_SLOW_SHARE_FORM_MS -ErrorAction SilentlyContinue }
  $pictures = @()
  try {
    WaitForSync $dir
    & $Cli --socket $sock sync add "$dir\Shared" | Out-Null
    if ($LASTEXITCODE -ne 0) { Fail "the CLI could not add a folder (exit $LASTEXITCODE)" }
    Qa $dir @('window') | Out-Null
    $open = Qa $dir @('share', 'open', 'Shared', 'nowait')
    if ($open -notmatch 'opening True') { Fail "the dialog is not opening: $open" }
    Qa $dir @('shot', 'share-opening', 'dialog') | Out-Null
    Copy-Item "$dir\state\qa\share-opening.png" "$Out\share-opening.png"
    $pictures += "$Out\share-opening.png"
    $real = WaitFor { $s = Qa $dir @('share', 'status'); if ($s -match 'opening False') { $s } } 10000 'the form to be read'
    if ($real -notmatch 'copy Copy Link') { Fail "no Copy Link once open: $real" }

    # Extend, as its menu does: 7 days more.
    Qa $dir @('share', 'for', '7') | Out-Null
    function DaysLeft([string]$line) { $m = [regex]::Match($line, 'sharing "[^"]*?(\d+) days'); if ($m.Success) { [int]$m.Groups[1].Value } else { -1 } }
    $seven = WaitFor { $s = Qa $dir @('share', 'status'); if ((DaysLeft $s) -ge 6) { $s } } 10000 'a sharing period of 7 days'
    Qa $dir @('share', 'extend', '7') | Out-Null
    try { WaitFor { $s = Qa $dir @('share', 'status'); if ((DaysLeft $s) -ge (DaysLeft $seven) + 6) { $s } } 10000 'the period to be extended' | Out-Null }
    catch { Fail "$_; before: $seven; now: $(Qa $dir @('share', 'status'))" }

    # Nothing changed: nothing drawn.
    function Draws([string]$line) { [int][regex]::Match($line, 'draws (\d+)').Groups[1].Value }
    $before = Draws (Qa $dir @('share', 'status'))
    Qa $dir @('share', 'refresh') | Out-Null
    $after = Draws (Qa $dir @('share', 'status'))
    # (The countdown's words may change on a minute's turn: one drawing at most, then none.)
    if ($after -gt $before + 1) { Fail "a refresh drew $($after - $before) times" }
    $before = $after
    Qa $dir @('share', 'refresh') | Out-Null
    $after = Draws (Qa $dir @('share', 'status'))
    if ($after -ne $before) { Fail "a refresh that found nothing changed drew $($after - $before) times" }

    # A change made elsewhere, with a drop-down open: it stays open, and the change is drawn once it closes.
    $said = Qa $dir @('share', 'dropdown', 'open')
    if ($said -notmatch 'menu open True') { Fail "the drop-down did not open: $said" }
    & $Cli --socket $sock sync read-only Shared on 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) { Fail "the CLI could not set Read only (exit $LASTEXITCODE)" }
    Qa $dir @('share', 'refresh') | Out-Null
    $held = Qa $dir @('share', 'status')
    if ($held -notmatch 'menu open True') { Fail "a refresh closed the open drop-down: $held" }
    if ((Draws $held) -ne $after) { Fail "a refresh drew under an open drop-down: $held" }
    Qa $dir @('share', 'dropdown', 'close') | Out-Null
    try { WaitFor { $s = Qa $dir @('share', 'status'); if ($s -match 'read only True') { $s } } 10000 'the change made elsewhere to be drawn' | Out-Null }
    catch { Fail "$_; now: $(Qa $dir @('share', 'status'))" }

    # A tooltip open over the dialog (the pointer's, or the keyboard focus's, at any moment): the shot is of the dialog. It
    # took share.png and new-channel.png in some full runs: a tooltip is a popup with a size, and draws as nothing.
    Qa $dir @('share', 'tooltip', 'open') | Out-Null
    $said = Qa $dir @('shot', 'share-tooltip', 'dialog')
    if (-not (Test-Path "$dir\state\qa\share-tooltip.png")) { Fail "no picture with a tooltip open: $said" }
    if ((Get-Item "$dir\state\qa\share-tooltip.png").Length -lt 5000) { Fail "the picture is of the tooltip, not the dialog: $said" }
    Qa $dir @('share', 'tooltip', 'close') | Out-Null

    # A redraw keeps the keyboard's place: a change made elsewhere, drawn by a refresh, with a switch focused.
    $said = Qa $dir @('share', 'focus', 'Single')
    if ($said -notmatch 'focused' -or $said -notmatch 'focus "ToggleSwitch Single use"') { Fail "the switch did not take the focus: $said" }
    & $Cli --socket $sock sync read-only Shared off 2>&1 | Out-Null
    Qa $dir @('share', 'refresh') | Out-Null
    $kept = Qa $dir @('share', 'status')
    if ($kept -notmatch 'read only False') { Fail "the refresh did not draw the change: $kept" }
    if ($kept -notmatch 'focus "ToggleSwitch Single use"') { Fail "a redraw lost the keyboard's place: $kept" }
    Qa $dir @('share', 'cancel') | Out-Null

    # A folder not synced yet opens the same way: at once, opening, then real.
    New-Item -ItemType Directory -Force "$dir\To Share" | Out-Null
    Launch $dir @('--share', "$dir\To Share") | Out-Null
    $new = WaitFor { $s = Qa $dir @('share', 'status'); if ($s -notmatch 'not open') { $s } } 5000 'the Share dialog of a new folder'
    if ($new -notmatch 'opening True') { Fail "a new folder's dialog is not opening: $new" }
    $real = WaitFor { $s = Qa $dir @('share', 'status'); if ($s -match 'opening False') { $s } } 10000 'the new folder''s form to be read'
    if ($real -notmatch 'copy Copy Link') { Fail "no Copy Link once open: $real" }
    Qa $dir @('share', 'cancel') | Out-Null
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue }
  $pictures
}

# The failures: a click that starts another program holds the UI thread while the shell takes its time (Explorer busy, a
# handler asking), and the start reading the disk on it. A start is made to take 3 s (SEEDWELL_QA_SLOW_SHELL_MS, isolated runs
# only), past the watchdog's fail_ms: it passes only off the UI thread. The source is scanned so that a new Process.Start on
# the UI thread cannot come back unseen. What the start read off the UI thread reaches the app: the groups folded before.
$checks.shellstarts = {
  $direct = @(Get-ChildItem $App -Filter *.cs | Where-Object { $_.Name -ne 'Shell.cs' } | Select-String -Pattern 'Process\.Start\(' -SimpleMatch:$false |
    Where-Object { $_.Line -notmatch 'Task\.Run|reachStart' } | ForEach-Object { "$($_.Filename):$($_.LineNumber)" })
  if ($direct) { Fail "a program started on the UI thread (use Shell.Start, or Task.Run): $($direct -join ', ')" }
  $dir = New-State
  'QA-Folded' | Set-Content -Encoding ascii "$dir\state\app-windows-folds.txt"
  $env:SEEDWELL_QA_SLOW_SHELL_MS = '3000'
  try { $p = Start-App $dir -QaControl } finally { Remove-Item env:SEEDWELL_QA_SLOW_SHELL_MS -ErrorAction SilentlyContinue }
  try {
    $reads = Qa $dir @('start-reads')
    if ($reads -notmatch 'folds \[QA-Folded\]') { Fail "the folds read off the UI thread did not reach the app: $reads" }
    Qa $dir @('shell-start') | Out-Null
    WaitFor { Log $dir | Where-Object { $_ -like '*shell: started cmd.exe*' } } 10000 'the program to start' | Out-Null
    # The installer's start too: off the UI thread, and it still says whether it ran (Install's failure hangs on it).
    $ok = Qa $dir @('update-start', 'ok') 20000
    if ($ok -notmatch 'started True') { Fail "the detached start: $ok" }
    $bad = Qa $dir @('update-start', 'bad') 20000
    if ($bad -notmatch 'started False') { Fail "a detached start of no program: $bad" }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue }
  @()
}

# The failure: a pasted link that can never be joined. The core no longer looks at the disk when it is given a link: it says
# the places are pending, and Start stays off until the host has read them (Core.SyncAddPlaces). A host that does not read them
# leaves the location empty for ever. With a real link, both ways in (opened with the link, and pasted): Start is on, and the
# place is under this PC's default location.
$checks.addlink = {
  $a = New-State 'full'; $b = New-State 'control'
  New-Item -ItemType Directory -Force "$a\Docs" | Out-Null
  $sa = "$a\state\control.sock"
  $pa = Start-App $a -QaControl
  $pb = Start-App $b -QaControl
  try {
    WaitForSync $a; WaitForSync $b
    $added = (& $Cli --socket $sa sync add "$a\Docs" 2>&1) -join ' '
    if ($added -notlike 'Sharing Docs*') { throw "sync add: $added" }
    $link = ((& $Cli --socket $sa sync share Docs --json 2>&1) -join '' | ConvertFrom-Json).link
    if (-not $link) { Fail 'A made no link' }
    Qa $b @('window') | Out-Null
    function Placed([string]$how, [string]$line) {
      if ($line -notmatch 'can start True') { Fail "$how a real link, Start is off: $line" }
      $at = [regex]::Match($line, 'location "([^"]*)"').Groups[1].Value
      if (-not $at.StartsWith("$b\state", [StringComparison]::OrdinalIgnoreCase)) { Fail "$how a real link, the place is not under the default location: '$at'; $line" }
    }
    Placed 'opened with' (Qa $b @('add-folder', 'open', $link))
    Qa $b @('add-folder', 'cancel') | Out-Null
    Qa $b @('add-folder', 'open') | Out-Null
    Qa $b @('add-folder', 'mode', 'shared') | Out-Null
    Placed 'pasted' (Qa $b @('add-folder', 'link', $link))
    Qa $b @('add-folder', 'cancel') | Out-Null
    # Typed: its places arrive while the user types, and never take the focus or move the caret.
    Qa $b @('add-folder', 'open') | Out-Null
    Qa $b @('add-folder', 'mode', 'shared') | Out-Null
    $typed = Qa $b @('add-folder', 'typing', '5', $link)
    Placed 'typed' $typed
    if ($typed -notmatch 'focus "TextBox caret 5"') { Fail "the places took the focus from the link being typed: $typed" }
    Qa $b @('add-folder', 'cancel') | Out-Null
  } finally {
    Stop-App $b $pb
    Stop-App $a $pa
    Remove-Item -Recurse -Force $a, $b -ErrorAction SilentlyContinue
  }
  @()
}

# The failure: a dialog drawing itself anew on each key takes the focus from the box being typed in, so a name can't be typed.
# Edit's and New Channel's names, the caret left in the middle.
$checks.dialogfocus = {
  $dir = New-State 'control'
  New-Item -ItemType Directory -Force "$dir\Shared" | Out-Null
  $p = Start-App $dir -QaControl
  try {
    WaitForSync $dir
    & $Cli --socket "$dir\state\control.sock" sync add "$dir\Shared" | Out-Null
    Qa $dir @('window') | Out-Null
    Qa $dir @('edit-folder', 'open', 'Shared') | Out-Null
    $edit = Qa $dir @('edit-folder', 'typing', '2', 'Renamed')
    if ($edit -notmatch 'focus "TextBox caret 2"') { Fail "typing in Edit's name lost the focus or the caret: $edit" }
    Qa $dir @('edit-folder', 'cancel') | Out-Null
    Qa $dir @('new-channel', 'open') | Out-Null
    $new = Qa $dir @('new-channel', 'typing', '2', 'Family')
    if ($new -notmatch 'focus "TextBox caret 2"') { Fail "typing in New Channel's name lost the focus or the caret: $new" }
    Qa $dir @('new-channel', 'mode', 'join') | Out-Null
    $link = Qa $dir @('new-channel', 'typing-link', '2', 'seedwell')
    if ($link -notmatch 'focus "TextBox caret 2"') { Fail "typing in New Channel's link lost the focus or the caret: $link" }
    Qa $dir @('new-channel', 'cancel') | Out-Null
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue }
  @()
}

# The failure: a surface that draws itself anew takes the keyboard from the control the user is on. The guard, so a new surface
# cannot regress: every dialog and Settings tab, each of its text boxes, password boxes and links and the first of each other
# kind of control focused, a redraw forced, the same one must still have it. Every surface is walked before it says what lost it.
$checks.focuswalk = {
  $dir = New-State 'full'
  New-Item -ItemType Directory -Force "$dir\Shared" | Out-Null
  $p = Start-App $dir -QaControl
  $lost = [System.Collections.Generic.List[string]]::new()
  function Walk([string]$surface) {
    $said = (Qa $dir @('focuswalk', $surface) 180000) -replace '^.*qa focuswalk [^:]*: ', '' -replace '; place .*$', ''
    Write-Host "  focuswalk ${surface}: $($said.Substring(0, [Math]::Min(1100, $said.Length)))"
    if ($said -notmatch 'walk [1-9]') { $lost.Add("${surface}: nothing walked: $said") }
    foreach ($m in [regex]::Matches($said, '(\w+: )?walk \d+ controls, lost [1-9]\d* \[[^\]]*\]')) { $lost.Add("${surface}: $($m.Value)") }
  }
  try {
    WaitForSync $dir
    & $Cli --socket "$dir\state\control.sock" sync add "$dir\Shared" | Out-Null
    Qa $dir @('window') | Out-Null
    Walk 'settings'
    # A refresh with a drop-down open: it stays open (drawing makes the controls anew, which would close it).
    Qa $dir @('settings', 'tab', 'General') | Out-Null
    if ((Qa $dir @('settings', 'dropdown', 'open')) -notmatch 'dropdown open True') { $lost.Add('settings: the drop-down did not open') }
    $still = Qa $dir @('settings', 'redraw')
    if ($still -notmatch 'dropdown open True') { $lost.Add('settings: a refresh closed an open drop-down') }
    Qa $dir @('settings', 'dropdown', 'close') | Out-Null
    Qa $dir @('place', 'foldersync') | Out-Null
    Qa $dir @('add-folder', 'open') | Out-Null
    Walk 'add'
    Qa $dir @('add-folder', 'mode', 'shared') | Out-Null
    Walk 'add'
    Qa $dir @('add-folder', 'cancel') | Out-Null
    Qa $dir @('edit-folder', 'open', 'Shared') | Out-Null
    Walk 'edit'
    Qa $dir @('edit-folder', 'cancel') | Out-Null
    Qa $dir @('share', 'open', 'Shared') | Out-Null
    Walk 'share'
    Qa $dir @('share', 'cancel') | Out-Null
    Qa $dir @('new-channel', 'open') | Out-Null
    Walk 'new-channel'
    Qa $dir @('new-channel', 'mode', 'join') | Out-Null
    Walk 'new-channel'
    Qa $dir @('new-channel', 'cancel') | Out-Null
    Qa $dir @('proxy-window', 'open') | Out-Null
    Qa $dir @('proxy-window', 'guide', 'on') | Out-Null
    Walk 'proxy'
    Qa $dir @('proxy-window', 'close') | Out-Null
    if ($lost.Count -gt 0) { Fail "the keyboard's place was lost: $($lost -join ' || ')" }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue }
  @()
}

# A core that cannot start never leaves the app running without one: a temp state whose database cannot be opened (a
# folder where state.redb belongs; garbage in the file is not enough, the core moves that aside) ends the run with the core's sentence and the ways out the core offers (recovery_actions; the user's app
# shows them in its alert, an isolated run logs them), within seconds.
$checks.damaged = {
  $dir = New-State
  New-Item -ItemType Directory "$dir\state\state.redb" | Out-Null
  'not a database' | Set-Content "$dir\state\state.redb\x"
  $q = Start-Process $Exe -ArgumentList '--state-dir', "$dir\state", '--qa-control' -PassThru
  try {
    if (-not $q.WaitForExit(30000)) { $q.Kill(); Fail 'the app ran on with a damaged state' }
    if ($q.ExitCode -ne 2) { Fail "it ended with exit code $($q.ExitCode), not 2" }
    if (Log $dir | Where-Object { $_ -like '*launch: core ready*' }) { Fail 'the core started on a damaged state' }
    $refused = Log $dir | Where-Object { $_ -like '*launch refused:*' } | Select-Object -First 1
    if (-not $refused) { Fail 'no "launch refused" with the core''s sentence in app.log' }
    $offered = Log $dir | Where-Object { $_ -like '*launch: recovery offered:*' } | Select-Object -First 1
    if ($offered -notmatch 'Install the Release') { Fail "the core's ways out were not offered: $offered" }
  } finally { if (-not $q.HasExited) { $q.Kill() }; Remove-Item -Recurse -Force $dir }
  @()
}

# Notifications and the core's questions (W4), on temp states (a QA run posts no Windows notification: it keeps each
# notice, and a question waits for the QA command prompt):
# - a watch folder ("ask first") that already holds a .torrent at start: the backlog offer; Add adds it;
# - a new .torrent in it: the add request; Add adds it;
# - the same torrent, started, added again: the core's duplicate notice; a click on it opens its route;
# - a bad value in config.toml: the settings banner says which (its own state: the temp config check would refuse it).
$checks.notices = {
  $dir = New-State 'control'
  New-Item -ItemType Directory -Force "$dir\watch" | Out-Null
  Copy-Item $fixture.debian "$dir\watch\debian.torrent"
  $cfg = "$dir\state\config.toml"
  ((Get-Content $cfg -Raw) -replace "watch = \[\]\r?\n", '') + "`n[[folders.watch]]`npath = '$dir\watch'`nenabled = true`nmode = `"ask`"`nafter_add = `"leave`"`n" | Set-Content -NoNewline -Encoding utf8 $cfg
  $p = Start-App $dir -QaControl
  try {
    $offer = WaitFor { $s = Qa $dir @('prompt', 'status'); if ($s -notmatch 'waiting ""') { $s } } 10000 'the backlog offer'
    Qa $dir @('prompt', 'yes') | Out-Null
    WaitFor { List $dir | Where-Object { $_ -match 'Debian' } } 10000 'the offered torrent to be added' | Out-Null
    Copy-Item $fixture.ubuntu "$dir\watch\ubuntu.torrent"
    $ask = WaitFor { $s = Qa $dir @('prompt', 'status'); if ($s -notmatch 'waiting ""') { $s } } 15000 'the add request'
    Qa $dir @('prompt', 'yes') | Out-Null
    WaitFor { List $dir | Where-Object { $_ -match 'Ubuntu' } } 10000 'the asked torrent to be added' | Out-Null
    # Out of its countdown (an isolated run asks no real tracker), then added again: a duplicate (a held one is not).
    & $Cli --socket "$dir\state\control.sock" start Debian 2>&1 | Out-Null
    WaitFor { -not (List $dir | Where-Object { $_ -match 'pending' -and $_ -match 'Debian' }) } 10000 'Debian out of its countdown' | Out-Null
    & $Cli --socket "$dir\state\control.sock" add $fixture.debian 2>&1 | Out-Null
    $dup = WaitFor { $s = Qa $dir @('notices'); if ($s -match 'Duplicate') { $s } } 10000 'the duplicate notice'
    if ($dup -notmatch 'Duplicate: [^\[]*\[[^\]]*\] \{([^}]*)\}') { Fail "the duplicate notice has no click: $dup" }
    $opened = Qa $dir @('notice-click', $Matches[1])
    if (-not (Log $dir | Where-Object { $_ -like '*notice route:*' })) { Fail "a click on the notice opened nothing: $opened" }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }

  $dir = New-State
  $cfg = "$dir\state\config.toml"
  (Get-Content $cfg -Raw) -replace 'start_delay_secs = 600', 'start_delay_secs = "soon"' | Set-Content -NoNewline -Encoding utf8 $cfg
  $p = Start-App $dir -QaControl
  try {
    $banner = WaitFor { $s = Qa $dir @('banner'); if ($s -notmatch 'banner: none') { $s } } 10000 'the settings banner'
    if ($banner -notmatch 'Problem: ' -or $banner -notmatch '\[Open Settings\]') { Fail "the banner does not say the problem with Open Settings: $banner" }
    Qa $dir @('window') | Out-Null
    Qa $dir @('shot', 'settings-banner') | Out-Null
    Copy-Item "$dir\state\qa\settings-banner.png" "$Out\settings-banner.png"
  } finally {
    # Not Stop-App: its check refuses a temp config with a problem, which this one has on purpose.
    Launch $dir @('--quit') | Out-Null
    if (-not $p.WaitForExit(30000)) { $p.Kill() }
    Remove-Item -Recurse -Force $dir
  }
  @()
}

# Settings (W5), the core's model in one generic renderer, on a temp state: every tab draws (Devices as its section);
# a switch, a speed field with its unit and folder sync's device name are saved (read back from the core after the
# reload); a speed the core refuses changes nothing.
$checks.settingspage = {
  $dir = New-State 'control'
  $p = Start-App $dir -QaControl
  try {
    WaitForSync $dir
    Qa $dir @('window') | Out-Null
    foreach ($t in 'General', 'Transfers', 'Folders', 'Network', 'FolderSync', 'Agents', 'Advanced') {
      $d = Qa $dir @('settings', 'tab', $t)
      if ($d -notmatch "tab $t," -or $d -match 'rows 0,') { Fail "Settings › $t did not draw its rows: $d" }
    }
    $dev = Qa $dir @('settings', 'tab', 'Devices')
    if ($dev -notmatch 'blocks \[Devices\]') { Fail "Settings › Devices is not its section: $dev" }

    Qa $dir @('settings', 'tab', 'General') | Out-Null
    Qa $dir @('settings', 'set', 'notify.finished', 'off') | Out-Null
    $off = WaitFor { $s = Qa $dir @('settings', 'row', 'notify.finished'); if ($s -match ': off;') { $s } } 10000 'the switch saved and read back'

    Qa $dir @('settings', 'tab', 'Transfers') | Out-Null
    Qa $dir @('settings', 'set', 'speed.down', '5', 'MB/s') | Out-Null
    $down = WaitFor { $s = Qa $dir @('settings', 'row', 'speed.down'); if ($s -match '5 MB/s') { $s } } 10000 'the limit saved and read back'
    Qa $dir @('settings', 'set', 'speed.down', 'lots', 'MB/s') | Out-Null
    $still = Qa $dir @('settings', 'row', 'speed.down')
    if ($still -notmatch '5 MB/s') { Fail "a refused limit changed it: $still" }

    Qa $dir @('settings', 'tab', 'FolderSync') | Out-Null
    Qa $dir @('settings', 'set', 'sync.device-name', 'QA-PC') | Out-Null
    WaitFor { ((& $Cli --socket "$dir\state\control.sock" sync settings 2>&1) -join ' ') -match 'QA-PC' } 10000 'the device name in folder sync' | Out-Null
    Qa $dir @('shot', 'settings-foldersync') | Out-Null
    Copy-Item "$dir\state\qa\settings-foldersync.png" "$Out\settings-foldersync.png"
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @("$Out\settings-foldersync.png")
}

# An agent asks for access (build 46, SPEC 9A.8), on a temp state: an agent at control runs `agents request full`; the
# core's notice comes with Approve and Deny, the tray has its line and Settings › Agents its Requests row; Deny on the
# notice answers it (the CLI's `agents requests --all` says denied) and the line goes.
$checks.requests = {
  $dir = New-State 'control'
  $p = Start-App $dir -QaControl
  try {
    Qa $dir @('window') | Out-Null
    & $Cli --socket "$dir\state\control.sock" agents request full --why 'QA check' 2>&1 | Out-Null
    $notice = WaitFor { $s = Qa $dir @('notices'); if ($s -match 'AgentRequest: ') { $s } } 10000 "the request's notice"
    $line = Qa $dir @('requests')
    if ($line -notmatch 'line "[^"]+", rows 1') { Fail "no tray line and one request: $line" }
    # Build 50/51: the icon's dot comes with it at once (the agent activity), the tooltip says why.
    $icon = WaitFor { $s = Qa $dir @('requests'); if ($s -match 'dot True for AgentRequest') { $s } } 5000 "the icon's dot"
    if ($icon -notmatch 'icon "[^"]*waiting for your answer') { Fail "the tooltip does not say an agent waits: $icon" }
    Qa $dir @('settings', 'tab', 'Agents') | Out-Null
    $rows = Qa $dir @('settings', 'rows')
    if ($rows -notmatch 'request') { Fail "Settings › Agents has no Requests row: $rows" }
    $clicked = Qa $dir @('notice-action', 'deny')
    if ($clicked -notmatch 'clicked deny .*\[approve, deny\]') { Fail "the notice has no Approve and Deny: $clicked" }
    WaitFor { ((& $Cli --socket "$dir\state\control.sock" agents requests --all 2>&1) -join ' ') -match '(?i)denied' } 10000 'the request denied' | Out-Null
    $after = Qa $dir @('requests')
    if ($after -notmatch 'line "", rows 0') { Fail "the request is still waiting: $after" }
    WaitFor { (Qa $dir @('requests')) -match 'dot False for none' } 5000 "the icon's dot to go" | Out-Null
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @()
}

# A multi-file torrent of the temp state's own (the fixtures are single files): <count> small text files in a folder
# named <name>, three of them in a subfolder, bencoded here with their SHA-1 pieces. Returns the .torrent's path.
function New-MultiTorrent([string]$root, [string]$name, [int]$count) {
  $data = Join-Path $root $name
  New-Item -ItemType Directory -Force (Join-Path $data 'sub') | Out-Null
  $files = @()
  for ($i = 1; $i -le $count; $i++) {
    [string[]]$rel = if ($i -le 3) { @('sub', "part-$i.txt") } else { @(('file-{0:D2}.txt' -f $i)) }
    $bytes = [Text.Encoding]::ASCII.GetBytes(("Seedwell QA file $i. " * (20 + $i)))
    [IO.File]::WriteAllBytes((Join-Path $data ($rel -join '\')), $bytes)
    $files += , @{ Path = $rel; Bytes = $bytes }
  }
  $pieceLength = 16384
  $all = New-Object IO.MemoryStream
  foreach ($f in $files) { $all.Write($f.Bytes, 0, $f.Bytes.Length) }
  $blob = $all.ToArray()
  $sha = [Security.Cryptography.SHA1]::Create()
  $pieces = New-Object IO.MemoryStream
  for ($o = 0; $o -lt $blob.Length; $o += $pieceLength) {
    $h = $sha.ComputeHash($blob, $o, [Math]::Min($pieceLength, $blob.Length - $o))
    $pieces.Write($h, 0, 20)
  }
  $out = New-Object IO.MemoryStream
  function W([object]$v) {
    if ($v -is [byte[]]) { $p = [Text.Encoding]::ASCII.GetBytes("$($v.Length):"); $out.Write($p, 0, $p.Length); $out.Write($v, 0, $v.Length) }
    elseif ($v -is [string]) { W ([Text.Encoding]::UTF8.GetBytes($v)) }
    elseif ($v -is [int] -or $v -is [long]) { $p = [Text.Encoding]::ASCII.GetBytes("i$($v)e"); $out.Write($p, 0, $p.Length) }
    elseif ($v -is [System.Collections.IDictionary]) {
      $out.WriteByte([byte][char]'d')
      foreach ($k in ($v.Keys | Sort-Object -CaseSensitive { $_ })) { W ([string]$k); W $v[$k] }
      $out.WriteByte([byte][char]'e')
    }
    else { $out.WriteByte([byte][char]'l'); foreach ($x in $v) { W $x }; $out.WriteByte([byte][char]'e') }
  }
  $list = @(foreach ($f in $files) { @{ 'length' = [long]$f.Bytes.Length; 'path' = [string[]]$f.Path } })
  W @{ 'info' = @{ 'name' = $name; 'piece length' = $pieceLength; 'pieces' = $pieces.ToArray(); 'files' = $list } }
  $torrent = Join-Path $root "$name.torrent"
  [IO.File]::WriteAllBytes($torrent, $out.ToArray())
  $torrent
}

# The file list under a row (build 42 Part E, the core's FileLists), on a temp state with a 25-file torrent of its own:
# the triangle opens it (the core's summary, a filter for more than 20 files, every row checked); a file's check is
# sent to the engine and stays after the list is read again; the filter narrows the rows; a click selects a file row and
# Space skips it (the Library's key action); the tray's chevron opens its checklist with the core's footer.
$checks.files = {
  $dir = New-State 'control'
  $torrent = New-MultiTorrent $dir 'QA-Files' 25
  $p = Start-App $dir -QaControl -extra @($torrent)
  $pictures = @()
  try {
    WaitFor { List $dir | Where-Object { $_ -match 'QA-Files' } } 10000 'the torrent' | Out-Null
    # Started at once (the app's Start), so it is no longer held and has its file list.
    $started = (& $Cli --socket "$dir\state\control.sock" start 'QA-Files' --suggested 2>&1) -join ' '
    if ($LASTEXITCODE -ne 0) { Fail "the CLI could not start the torrent: $started" }
    WaitFor { -not ((List $dir | Where-Object { $_ -match 'QA-Files' }) -match '(?i)choose|held|pending') } 10000 'the torrent to start' | Out-Null
    Qa $dir @('window') | Out-Null
    Qa $dir @('place', 'downloads') | Out-Null
    WaitFor { (Qa $dir @('rowclick', 'QA-Files')) -notmatch 'no such row' } 10000 'the row in the window' | Out-Null
    Qa $dir @('files', 'toggle', 'QA-Files') | Out-Null
    $open = WaitFor { $s = Qa $dir @('files'); if ($s -match 'rows \d+ \[') { $s } } 10000 'the files read'
    if ($open -notmatch 'open 1' -or $open -notmatch '25 files') { Fail "the list did not open with its summary: $open" }
    if ($open -notmatch 'filter ""') { Fail "a 25-file list has no filter: $open" }
    if ($open -notmatch 'file-05\.txt:On') { Fail "the files are not all wanted: $open" }

    Qa $dir @('files', 'check', 'QA-Files', 'file-05.txt') | Out-Null
    $skipped = WaitFor { $s = Qa $dir @('files'); if ($s -match 'file-05\.txt:Off' -and $s -match 'pending False') { $s } } 5000 'the check sent to the engine'
    # Read again from the engine: closed and opened.
    Qa $dir @('files', 'toggle', 'QA-Files') | Out-Null
    Qa $dir @('files', 'toggle', 'QA-Files') | Out-Null
    $again = WaitFor { $s = Qa $dir @('files'); if ($s -match 'rows \d+ \[') { $s } } 10000 'the files read again'
    if ($again -notmatch 'file-05\.txt:Off') { Fail "the engine did not keep the skipped file: $again" }

    $filtered = Qa $dir @('files', 'filter', 'QA-Files', 'part')
    if ($filtered -notmatch 'filter "part"' -or $filtered -match 'file-0') { Fail "the filter did not narrow the rows: $filtered" }
    Qa $dir @('shot', 'files-filter') | Out-Null
    Copy-Item "$dir\state\qa\files-filter.png" "$Out\files-filter.png"
    $pictures += "$Out\files-filter.png"
    # No text: the filter cleared.
    Qa $dir @('files', 'filter', 'QA-Files') | Out-Null

    $clicked = Qa $dir @('files', 'click', 'QA-Files', 'file-07.txt')
    if ($clicked -notmatch 'selected 1' -or $clicked -notmatch 'file-07\.txt:On:[^,]*\*') { Fail "a click did not select the file row: $clicked" }
    $space = Qa $dir @('key', 'space', 'list')
    if ($space -notmatch 'handled True') { Fail "Space on a file row was not handled: $space" }
    WaitFor { (Qa $dir @('files')) -match 'file-07\.txt:Off' } 5000 'Space to skip the file' | Out-Null
    Qa $dir @('shot', 'files') | Out-Null
    Copy-Item "$dir\state\qa\files.png" "$Out\files.png"
    $pictures += "$Out\files.png"

    $menu = Qa $dir @('files', 'menu', 'QA-Files')
    if ($menu -notmatch 'menu [^|]*25 files[^|]* rows \d+ \[') { Fail "the tray's checklist did not open with the core's footer: $menu" }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  $pictures
}

# The chooser, the held list and the starting notifier (build 42 Part E), on a temp state: a torrent that waits for a
# choice (always ask) and one that counts down. The notifier shows both (its choice block opens its checklist); the
# held row's list and the chooser window show the same chooser (a tick in one is the other's); Start in the chooser
# starts the torrent and closes it; Start now in the notifier starts the countdown's.
$checks.chooser = {
  $dir = New-State 'control'
  $cfg = "$dir\state\config.toml"
  (Get-Content $cfg -Raw) -replace 'start_delay_secs = 600', "start_delay_secs = 600`nask_before_adding = `"always`"" | Set-Content -NoNewline -Encoding utf8 $cfg
  # Start makes it announce: a copy whose tracker is a closed port on loopback, never Fedora's.
  $latin = [Text.Encoding]::GetEncoding(28591)
  $text = $latin.GetString([IO.File]::ReadAllBytes($fixture.fedora))
  $tracker = 'http://127.0.0.1:9/announce'
  $text = $text -replace '^d8:announce\d+:https?://[^/]+/announce', "d8:announce$($tracker.Length):$tracker"
  [IO.File]::WriteAllBytes("$dir\fedora.torrent", $latin.GetBytes($text))
  $p = Start-App $dir -QaControl -extra @("$dir\fedora.torrent")
  $pictures = [System.Collections.Generic.List[string]]::new()
  function Shot([string]$name, [string]$what = '') {
    Qa $dir (@('shot', $name) + $(if ($what) { @($what) } else { @() })) | Out-Null
    Copy-Item "$dir\state\qa\$name.png" "$Out\$name.png"
    $pictures.Add("$Out\$name.png")
  }
  try {
    WaitFor { (List $dir) -join ' ' -match 'awaiting-choice' } 10000 'the torrent to wait for a choice' | Out-Null
    Qa $dir @('window') | Out-Null
    $shown = WaitFor { $s = Qa $dir @('starting', 'status'); if ($s -match 'starting: shown True, items 1 \[.*Choice') { $s } } 10000 'the notifier with the choice'
    Qa $dir @('starting', 'chevron', 'fedora') | Out-Null
    $open = Qa $dir @('starting', 'status')
    if ($open -notmatch '\| picked ') { Fail "the choice block did not open its checklist: $open" }
    Shot 'starting-choice' 'starting'

    $held = Qa $dir @('held', 'toggle', 'fedora')
    if ($held -notmatch 'held \[[0-9a-f]{8}: checked (\d+), rows (\d+) \[') { Fail "no checklist under the held row: $held" }
    $heldChecked = $Matches[1]
    Shot 'held-list'

    $chooser = Qa $dir @('chooser', 'show', 'fedora')
    if ($chooser -notmatch 'chooser [0-9a-f]{8}: checked (\d+), rows \d+ \[([^:\]]+):') { Fail "the chooser window did not open: $chooser" }
    if ($Matches[1] -ne $heldChecked) { Fail "the chooser and the held list differ: $chooser" }
    $first = $Matches[2]
    Shot 'chooser' 'dialog'
    $toggled = Qa $dir @('chooser', 'toggle', $first)
    if ($toggled -notmatch 'chooser [0-9a-f]{8}: checked (\d+)' -or $Matches[1] -eq $heldChecked) { Fail "a tick in the chooser did not change it: $toggled" }
    $now = $Matches[1]
    if ($toggled -notmatch "held \[[0-9a-f]{8}: checked $now,") { Fail "the held list does not show the chooser's tick: $toggled" }
    # Back, so Start has something to start.
    Qa $dir @('chooser', 'toggle', $first) | Out-Null

    # The guard's walk, in the two surfaces only a held torrent opens.
    foreach ($surface in 'chooser', 'starting') {
      $said = (Qa $dir @('focuswalk', $surface) 60000) -replace '^.*qa focuswalk [^:]*: ', '' -replace '; place .*$', ''
      Write-Host "  chooser: focuswalk ${surface}: $said"
      if ($said -notmatch 'walk [1-9]' -or $said -notmatch 'lost 0') { Fail "focuswalk ${surface}: $said" }
    }
    # A change from another surface redraws the chooser: the keyboard stays where it was.
    $was = [regex]::Match((Qa $dir @('chooser', 'focus-first')), 'focus "[^"]*"').Value
    if ($was -eq '' -or $was -eq 'focus "none"') { Fail "nothing in the chooser took the focus: $was" }
    Qa $dir @('chooser', 'toggle', $first) | Out-Null
    $now = [regex]::Match((Qa $dir @('chooser', 'focused')), 'focus "[^"]*"').Value
    if ($now -ne $was) { Fail "a change from elsewhere moved the keyboard in the chooser: $was, then $now" }
    Qa $dir @('chooser', 'toggle', $first) | Out-Null
    $started = Qa $dir @('chooser', 'start')
    WaitFor { -not ((List $dir) -join ' ' -match 'awaiting-choice') } 10000 'Start to start the torrent' | Out-Null
    WaitFor { (Qa $dir @('chooser', 'status')) -match 'no chooser open' } 5000 'the chooser to close' | Out-Null
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }

  # A countdown: the notifier's ring and line, Start now.
  $dir = New-State 'control'
  $p = Start-App $dir -QaControl -extra @($fixture.debian)
  try {
    WaitFor { List $dir | Where-Object { $_ -match 'Debian' } } 10000 'the torrent' | Out-Null
    $count = WaitFor { $s = Qa $dir @('starting', 'status'); if ($s -match 'starting: shown True, items 1 \[.*Countdown') { $s } } 10000 'the notifier with the countdown'
    Qa $dir @('starting', 'shift', 'on') | Out-Null
    $shift = Qa $dir @('starting', 'status')
    if ($shift -notmatch 'cancel caption: Keep') { Fail "Shift does not show Cancel's caption: $shift" }
    Shot 'starting-countdown' 'starting'
    Qa $dir @('starting', 'shift', 'auto') | Out-Null
    Qa $dir @('starting', 'start-now', 'debian') | Out-Null
    WaitFor { -not ((List $dir | Where-Object { $_ -match 'Debian' }) -match 'pending') } 10000 'Start now to start it' | Out-Null
    WaitFor { (Qa $dir @('starting', 'status')) -match 'shown False' } 10000 'the notifier to close' | Out-Null
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  $pictures.ToArray()
}

# Play (Part F): the players the core finds on this PC (the registry and PATH), and Play on a torrent with no video
# here: the core's calm note at the bottom of the window (a QA run starts no player).
$checks.play = {
  $dir = New-State
  $p = Start-App $dir -QaControl -extra @($fixture.debian)
  try {
    WaitFor { List $dir | Where-Object { $_ -match 'Debian' } } 10000 'the torrent' | Out-Null
    Qa $dir @('window') | Out-Null
    $players = Qa $dir @('play-players')
    Write-Host "  play-players: $players"
    $note = Qa $dir @('play', 'debian')
    if ($note -notmatch 'note "[^"]+"') { Fail "Play gave no note: $note" }
    Qa $dir @('shot', 'play-note') | Out-Null
    Copy-Item "$dir\state\qa\play-note.png" "$Out\play-note.png"
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @("$Out\play-note.png")
}

# Favorite trackers and the Info page (Part F), on a temp state: Debian's automatic tracker is saved as a favorite; its
# Info page shows the core's numbers, limits, folders and addresses; a download limit is saved; the favorite is renamed
# through its sidebar field; Remove from Favorites asks first (the strip), Cancel keeps it, Remove removes it.
$checks.favorites = {
  $dir = New-State
  $p = Start-App $dir -QaControl -extra @($fixture.debian)
  $pictures = [System.Collections.Generic.List[string]]::new()
  function Shot([string]$name) {
    Qa $dir @('shot', $name) | Out-Null
    Copy-Item "$dir\state\qa\$name.png" "$Out\$name.png"
    $pictures.Add("$Out\$name.png")
  }
  try {
    WaitFor { List $dir | Where-Object { $_ -match 'Debian' } } 10000 'the torrent' | Out-Null
    Qa $dir @('window') | Out-Null
    $saved = Qa $dir @('favorite', 'save', 'debian')
    $saved = WaitFor { $s = Qa $dir @('favorite', 'status', 'debian'); if ($s -match 'trackers \[[^\]]*\*') { $s } } 10000 'the tracker to become a favorite'
    $info = Qa $dir @('tracker-info', 'debian')
    if ($info -notmatch 'favorite True' -or $info -notmatch 'folders editable' -or $info -notmatch 'addresses \[[^\]]+\]') { Fail "the Info page of the favorite: $info" }
    $limited = Qa $dir @('tracker-info', 'debian', 'limits', '5000000', '0')
    if ($limited -notmatch 'limits \[[^\]]*5000000') { Fail "the download limit was not saved: $limited" }
    Shot 'tracker-info'
    $renamed = Qa $dir @('favorite', 'rename', 'debian', 'Debian QA')
    if ($renamed -notmatch 'trackers \[[^\]]*Debian QA\*') { Fail "the favorite was not renamed: $renamed" }
    $asked = Qa $dir @('favorite', 'remove', 'Debian QA')
    if ($asked -notmatch 'removal "[^"]+"') { Fail "Remove from Favorites did not ask: $asked" }
    Shot 'favorite-remove'
    $kept = Qa $dir @('favorite', 'cancel')
    if ($kept -notmatch 'removal ""' -or $kept -notmatch 'Debian QA\*') { Fail "Cancel did not keep the favorite: $kept" }
    Qa $dir @('favorite', 'remove', 'Debian QA') | Out-Null
    $removed = Qa $dir @('favorite', 'confirm')
    WaitFor { (Qa $dir @('favorite', 'status', 'debian')) -notmatch 'trackers \[[^\]]*\*' } 10000 'the favorite to be removed' | Out-Null
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  $pictures.ToArray()
}

# Settings › Rules (item 4): the core's rules editor on a temp state. The Default rule is shown with its footer; + adds
# a rule; its name is set through its row (the editor's draft, not settings_apply) and saved; a second rule is dragged
# before it (MoveRule); − removes it. Trackers: a new tracker for a host, its folder editor, saved into the list.
$checks.rules = {
  $dir = New-State
  $p = Start-App $dir -QaControl -extra @($fixture.debian)
  $pictures = [System.Collections.Generic.List[string]]::new()
  function Shot([string]$name) {
    Qa $dir @('shot', $name) | Out-Null
    Copy-Item "$dir\state\qa\$name.png" "$Out\$name.png"
    $pictures.Add("$Out\$name.png")
  }
  try {
    WaitFor { List $dir | Where-Object { $_ -match 'Debian' } } 10000 'the torrent' | Out-Null
    Qa $dir @('window') | Out-Null
    Qa $dir @('settings', 'tab', 'rules') | Out-Null
    $start = Qa $dir @('settings', 'rules', 'status')
    if ($start -notmatch 'section rules, rows \[\], default Default\*' -or $start -notmatch 'footer "[^"]+"') { Fail "the Default rule is not shown with its footer: $start" }
    $added = Qa $dir @('settings', 'rules', 'add')
    if ($added -notmatch 'rows \[1:[^\]]*\*') { Fail "+ did not add a rule: $added" }
    Qa $dir @('settings', 'set', 'rule.name', 'QA Rule') | Out-Null
    $saved = Qa $dir @('settings', 'rules', 'save')
    if ($saved -notmatch 'rows \[1:QA Rule\*') { Fail "the rule's name was not saved: $saved" }
    Shot 'settings-rules'
    Qa $dir @('settings', 'rules', 'add') | Out-Null
    $moved = Qa $dir @('settings', 'rules', 'move', '1', '0')
    if ($moved -notmatch 'rows \[1:New rule[^,]*, 2:QA Rule') { Fail "the rule was not moved before the other: $moved" }
    Qa $dir @('settings', 'rules', 'select', 'QA Rule') | Out-Null
    $removed = Qa $dir @('settings', 'rules', 'remove')
    WaitFor { (Qa $dir @('settings', 'rules', 'status')) -notmatch 'QA Rule' } 5000 'the rule to be removed' | Out-Null
    Qa $dir @('settings', 'rules', 'section', 'trackers') | Out-Null
    $new = Qa $dir @('settings', 'rules', 'new-tracker', 'tracker.example.org')
    if ($new -notmatch 'section trackers, rows \[[^\]]*\+:' -or $new -notmatch 'folders shown') { Fail "no new tracker with its folder editor: $new" }
    Shot 'settings-trackers'
    $trackerSaved = Qa $dir @('settings', 'rules', 'save')
    if ($trackerSaved -notmatch 'rows \[[^\]]*t:[^\]]*tracker\.example\.org') { Fail "the new tracker was not saved: $trackerSaved" }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  $pictures.ToArray()
}

# The Cloud Files plumbing (item 6) on this PC, before the host is wired to the core: a sync root in the run's temp state,
# placeholders for a file and a folder, reads served by FETCH_DATA (filled), pin, dehydrate and a second read; then the
# root is unregistered and removed. Nothing of the user's is touched.
$checks.cloudfiles = {
  $dir = New-State
  $p = Start-App $dir -QaControl
  try {
    $r = Qa $dir @('cloudfiles', 'selftest')
    Write-Host "  cloudfiles: $($r -replace '^.*qa cloudfiles selftest: ', '' -replace '; place .*$', '')"
    foreach ($want in 'registered', 'connected', 'placeholders True', 'sizes 35/10000', 'read matches', 'hydrated True', 'inner matches', 'pinned True', 'dehydrated True', 'read again matches') {
      if ($r -notmatch [regex]::Escape($want)) { Fail "Cloud Files: no '$want' in: $r" }
    }
    if ($r -match 'failed|cleanup') { Fail "Cloud Files: $r" }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @()
}

# On-demand folders end to end (item 6), on a temp state: the isolated run hosts the folder inside its own state folder
# as a Cloud Files sync root; placeholders appear, a read fills the file through the core and the core adopts it, and
# after a restart the host picks up where it left off (filled files stay filled, nothing placed twice).
$checks.ondemand = {
  $dir = New-State 'full'
  $docs = "$dir\state\od\Docs"
  New-Item -ItemType Directory -Force "$docs\sub" | Out-Null
  'alpha text' | Set-Content -NoNewline "$docs\a.txt"
  'bravo text' | Set-Content -NoNewline "$docs\b.txt"
  'charlie text' | Set-Content -NoNewline "$docs\sub\c.txt"
  'delta text' | Set-Content -NoNewline "$docs\d.txt"
  $sock = "$dir\state\control.sock"
  $p = Start-App $dir -QaControl
  try {
    WaitForSync $dir
    $added = (& $Cli --socket $sock sync add $docs 2>&1) -join ' '
    if ($added -notlike 'Sharing Docs*') { throw "sync add: $added" }
    # Freeing this PC's copies is the explicit choice (`on-demand` alone keeps a folder's files where they are).
    $mode = (& $Cli --socket $sock sync mode Docs on-demand --free-space 2>&1) -join ' '
    if ($mode -notlike 'Docs is on demand*') { throw "sync mode: $mode" }
    WaitFor { (Qa $dir @('ondemand')) -match 'roots 1 \[\S+ connected True placed 5 files \[a\.txt:p, b\.txt:p, d\.txt:p, sub/c\.txt:p\]\]' } 20000 'the placeholders' | Out-Null
    $read = Qa $dir @('ondemand', 'read', 'a.txt')
    if ($read -notmatch 'read "alpha text"; roots 1 \[.*a\.txt:f, b\.txt:p, d\.txt:p, sub/c\.txt:p\]') { throw "read a.txt: $read" }
    WaitFor { Log $dir | Where-Object { $_ -like '*on-demand: a.txt filled (*' } } 5000 'the core to adopt a.txt' | Out-Null
    $read = Qa $dir @('ondemand', 'read', 'sub/c.txt')
    if ($read -notmatch 'read "charlie text"; .*sub/c\.txt:f\]') { throw "read sub/c.txt: $read" }

    # Again after a restart: the anchor is kept, so nothing is placed again and the filled files stay filled.
    Stop-App $dir $p
    $p = Start-App $dir -QaControl
    WaitForSync $dir
    WaitFor { (Qa $dir @('ondemand')) -match 'roots 1 \[\S+ connected True placed 5 files \[a\.txt:f, b\.txt:p, d\.txt:p, sub/c\.txt:f\]\]' } 20000 'the folder again after a restart' | Out-Null
    $read = Qa $dir @('ondemand', 'read', 'b.txt')
    if ($read -notmatch 'read "bravo text"; .*b\.txt:f') { throw "read b.txt after the restart: $read" }

    # Back to full: the host lets the folder go (filled placeholders become plain files, the rest are removed, the sync
    # root is unregistered) and the core brings every file back, d.txt (never filled) too. Nothing is a placeholder.
    $full = (& $Cli --socket $sock sync mode Docs full 2>&1) -join ' '
    WaitFor { ((& $Cli --socket $sock sync list --json 2>&1) -join '' | ConvertFrom-Json).folders | Where-Object { $_.name -eq 'Docs' -and -not $_.on_demand } } 20000 "the folder to be full again ($full)" | Out-Null
    WaitFor { (Qa $dir @('ondemand')) -match 'roots 0 \[\]' } 10000 'the host to let the folder go' | Out-Null
    WaitFor { Log $dir | Where-Object { $_ -like '*on-demand folder released:*' } } 5000 'the sync root to be released' | Out-Null
    $want = @{ 'a.txt' = 'alpha text'; 'b.txt' = 'bravo text'; 'd.txt' = 'delta text'; 'sub\c.txt' = 'charlie text' }
    foreach ($f in $want.Keys) {
      WaitFor { (Test-Path "$docs\$f") -and (Get-Content -Raw "$docs\$f") -eq $want[$f] } 20000 "$f back in full" | Out-Null
    }
    foreach ($f in @($want.Keys) + @('sub', '')) {
      $is = Qa $dir @('ondemand', 'placeholder', (Join-Path $docs $f).TrimEnd('\'))
      if ($is -notmatch 'placeholder False') { throw "$(if ($f) { $f } else { 'the folder' }) is still a placeholder after the switch to full: $is" }
    }
    $failed = @(Log $dir | Where-Object { $_ -like '*on-demand fetch of * failed*' -or $_ -like '*on-demand folders: not hosted*' -or $_ -like '*: not in the snapshot*' -or $_ -like '*on-demand folder not released*' })
    if ($failed) { throw "the log: $($failed -join ' || ')" }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue }
  @()
}

# On-demand fetching from another device (item 6 step 2), two temp states on this PC: A shares a folder, B joins it on
# demand inside its own state folder, and a read on B fills the placeholder with A's bytes. With A gone, a read fails
# with the system's not-here-yet error (network unavailable), the core's notice is posted once (the Notifier, logged in
# a QA run), and B keeps running.
$checks.ondemandfetch = {
  $a = New-State 'full'
  $b = New-State 'full'
  $docs = "$a\state\od\Docs"
  New-Item -ItemType Directory -Force $docs | Out-Null
  $bytes = New-Object byte[] (3MB)
  (New-Object Random 7).NextBytes($bytes)
  [IO.File]::WriteAllBytes("$docs\big.bin", $bytes)
  $want = (Get-FileHash "$docs\big.bin" -Algorithm SHA256).Hash.ToLowerInvariant()
  'small text' | Set-Content -NoNewline "$docs\small.txt"
  $sa = "$a\state\control.sock"; $sb = "$b\state\control.sock"
  $pa = Start-App $a -QaControl
  $pb = $null
  try {
    # B with Folder Sync's limits on the LAN too (QA only, honoured for a temp state), so a fetch shows progress.
    $env:SEEDWELL_QA_LIMIT_LAN = '1'
    try { $pb = Start-App $b -QaControl } finally { Remove-Item Env:SEEDWELL_QA_LIMIT_LAN }
    WaitForSync $a; WaitForSync $b
    WaitFor { CoreLog $b | Where-Object { $_ -like '*QA: the speed limits apply to LAN sessions too*' } } 10000 'B''s core to limit the LAN (core.log)' | Out-Null
    $added = (& $Cli --socket $sa sync add $docs 2>&1) -join ' '
    if ($added -notlike 'Sharing Docs*') { throw "sync add: $added" }
    $link = ((& $Cli --socket $sa sync share Docs --json 2>&1) -join '' | ConvertFrom-Json).link
    $joined = ($link | & $Cli --socket $sb sync join - --on-demand --to "$b\state\od\Docs" 2>&1) -join ' '
    if ($joined -notlike 'Joining Docs*') { throw "sync join: $joined" }
    WaitFor { (Qa $b @('ondemand')) -match 'roots 1 \[\S+ connected True placed 2 files \[big\.bin:p, small\.txt:p\]\]' } 60000 'the placeholders on B' | Out-Null

    # At 500 kB/s the 3 MB take about 6 s: the read shows progress while it waits.
    & $Cli --socket $sb sync settings --down 500k 2>&1 | Out-Null
    $read = Qa $b @('ondemand', 'read', 'big.bin') 60000
    if ($read -notmatch "read 3145728 bytes, sha256 $want; .*big\.bin:f, small\.txt:p") { throw "read big.bin on B: $read" }
    $filled = WaitFor { Log $b | Where-Object { $_ -like '*on-demand: big.bin filled (Downloaded;*' } | Select-Object -Last 1 } 5000 'the core to adopt big.bin'
    if (-not ($filled -match 'progress shown (\d+) times') -or [int]$Matches[1] -lt 2) { throw "progress was not shown while fetching: $filled" }
    & $Cli --socket $sb sync settings --down unlimited 2>&1 | Out-Null

    # A gone: nobody can give small.txt, and the core says so at once (A's quit closed its sessions).
    Stop-App $a $pa
    $pa = $null
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $read = Qa $b @('ondemand', 'read', 'small.txt') 30000
    if ($clock.Elapsed.TotalSeconds -gt 10) { throw "the read with A gone took $([int]$clock.Elapsed.TotalSeconds) s to fail" }
    if ($read -notmatch 'read failed: The cloud sync provider failed to perform the operation due to network being unavailable\.') { throw "read small.txt with A gone: $read" }
    # Not left looking filled. The system clears the file's recall bit for under 1 ms as a failed fetch ends (seen from
    # another process), so the line above may catch it clear: what counts is where it comes to rest.
    try { WaitFor { (Qa $b @('ondemand')) -match 'small\.txt:p' } 5000 'small.txt to be a placeholder again' | Out-Null }
    catch { throw "$_; after the failed read: $read; now: $(Qa $b @('ondemand'))" }
    $told = @(Log $b | Where-Object { $_ -match 'notice: SyncProblem: .small\.txt. isn.t here yet' })
    if ($told.Count -ne 1) { throw "the not-here-yet notice was posted $($told.Count) times: $($told -join ' || ')" }
    if (-not @(Log $b | Where-Object { $_ -like '*on-demand fetch of * failed (NotHereYet)*' })) { throw 'no failed fetch in the log' }
    if ($pb.HasExited) { throw "B ended (exit code $($pb.ExitCode))" }
    $status = Qa $b @('ondemand')
    if ($status -notmatch 'roots 1 \[\S+ connected True') { throw "B after the failure: $status" }
  } finally {
    if ($pb) { Stop-App $b $pb }
    if ($pa) { Stop-App $a $pa }
    Remove-Item -Recurse -Force $a, $b -ErrorAction SilentlyContinue
  }
  @()
}

# Keeping and freeing on-demand files (item 6 step 3), two temp states: A shares a folder, B joins it on demand.
# Explorer's Always keep on this device and Free up space set the PINNED and UNPINNED attributes (attrib +P / +U does
# the same); B's watcher passes them to the core (sync_pin, sync_evict) and the feed fills or frees the placeholders.
# Storage Sense is another process dehydrating a placeholder: allowed only for what the core frees (A connected),
# denied otherwise (A gone), and never taken for the user's own Free up space.
$checks.ondemandpin = {
  $a = New-State 'full'
  $b = New-State 'full'
  $docs = "$a\state\od\Docs"
  New-Item -ItemType Directory -Force "$docs\sub" | Out-Null
  foreach ($n in 'a', 'b', 'd', 'e') { "$n text" | Set-Content -NoNewline "$docs\$n.txt" }
  'c text' | Set-Content -NoNewline "$docs\sub\c.txt"
  $sa = "$a\state\control.sock"; $sb = "$b\state\control.sock"
  $bd = "$b\state\od\Docs"
  $pa = Start-App $a -QaControl
  $pb = $null
  # B's files as its host sees them, and a top-level file's line in the core's own listing.
  $files = { (Qa $b @('ondemand')) -replace '^.*files \[([^\]]*)\].*$', '$1' }
  $core = { param($name) (& $Cli --socket $sb sync items Docs 2>&1 | Where-Object { $_ -match "\s$([regex]::Escape($name))$" }) -join '' }
  try {
    $pb = Start-App $b -QaControl
    WaitForSync $a; WaitForSync $b
    & $Cli --socket $sa sync add $docs 2>&1 | Out-Null
    $link = ((& $Cli --socket $sa sync share Docs --json 2>&1) -join '' | ConvertFrom-Json).link
    $link | & $Cli --socket $sb sync join - --on-demand --to $bd 2>&1 | Out-Null
    WaitFor { (& $files) -eq 'a.txt:p, b.txt:p, d.txt:p, e.txt:p, sub/c.txt:p' } 60000 'the placeholders on B' | Out-Null

    # Always keep on this device: a file, then a folder (what is in it).
    attrib +P -U "$bd\a.txt"
    WaitFor { (& $files) -match 'a\.txt:fP,' } 15000 'a.txt kept and filled' | Out-Null
    if ((& $core 'a.txt') -notmatch '^\s*here\s.*kept\s+a\.txt$') { Fail "the core's a.txt: $(& $core 'a.txt')" }
    attrib +P -U "$bd\sub"
    WaitFor { (& $files) -match 'sub/c\.txt:fP$' } 15000 'sub/c.txt kept and filled (its folder kept)' | Out-Null

    # Free up space: a kept file (no longer kept, freed), and one that was only read.
    attrib +U -P "$bd\a.txt"
    WaitFor { (& $files) -match 'a\.txt:pU?,' } 15000 'a.txt freed' | Out-Null
    if ((& $core 'a.txt') -notmatch '^\s*online only\s+\S+ \S+\s+a\.txt$') { Fail "the core's a.txt after Free up space: $(& $core 'a.txt')" }
    if (-not (Log $b | Where-Object { $_ -like '*on-demand: a.txt no longer kept*' })) { Fail 'a.txt was not unpinned first' }
    Qa $b @('ondemand', 'read', 'b.txt') | Out-Null
    attrib +U -P "$bd\b.txt"
    WaitFor { (& $files) -match 'b\.txt:pU?,' } 15000 'b.txt freed' | Out-Null
    WaitFor { Log $b | Where-Object { $_ -like '*on-demand: b.txt freed 1 files, 6 bytes; kept 0' } } 5000 'the core to free b.txt' | Out-Null

    # Storage Sense with A connected: allowed, and not taken for the user's choice.
    Qa $b @('ondemand', 'read', 'd.txt') | Out-Null
    Qa $b @('ondemand', 'read', 'e.txt') | Out-Null
    $sense = CfDehydrate "$bd\d.txt"
    if ($sense -ne 'freed') { Fail "Storage Sense on d.txt with A connected: $sense" }
    WaitFor { (& $files) -match 'd\.txt:pU?,' } 10000 'd.txt freed by the system' | Out-Null
    if (-not (Log $b | Where-Object { $_ -like '*on-demand: the system asked to free d.txt: allowed*' })) { Fail 'the system''s request for d.txt was not logged as allowed' }

    # A gone: Storage Sense is denied (nobody confirms it), the mark it left goes; the user's Free up space keeps it, says why.
    Stop-App $a $pa
    $pa = $null
    $sense = CfDehydrate "$bd\e.txt"
    if ($sense -eq 'freed') { Fail 'Storage Sense freed e.txt with A gone' }
    WaitFor { Log $b | Where-Object { $_ -like '*on-demand: the system asked to free e.txt: denied; no other device that has it is connected*' } } 5000 'the denial for e.txt' | Out-Null
    WaitFor { (& $files) -match 'e\.txt:f,' } 10000 'e.txt still here, unmarked' | Out-Null
    # Nothing told so far: a.txt and b.txt were freed whole, and Storage Sense stays silent.
    $kept = @(Log $b | Where-Object { $_ -like '*notice: SyncKept:*' })
    if ($kept) { Fail "a kept-files notice before the user's free of e.txt: $($kept -join ' || ')" }
    attrib +U -P "$bd\e.txt"
    WaitFor { Log $b | Where-Object { $_ -like '*on-demand: e.txt freed 0 files, 0 bytes; kept 1; e.txt: no other device that has it is connected' } } 10000 'Free up space to keep e.txt, with the reason' | Out-Null
    # The core's notice says it stays and why (Explorer's menu has no error to show).
    $kept = @(WaitFor { @(Log $b | Where-Object { $_ -like '*notice: SyncKept:*' }) } 5000 'the kept-files notice')
    if ($kept.Count -ne 1 -or $kept[0] -notmatch 'SyncKept: .e\.txt. stays on this PC') { Fail "the kept-files notice: $($kept -join ' || ')" }
    WaitFor { (& $files) -match 'e\.txt:f,' } 10000 'e.txt here and unmarked after Free up space' | Out-Null
    $system = @(Log $b | Where-Object { $_ -like '*on-demand: d.txt freed*' -or $_ -like '*on-demand: e.txt freed*' })
    if ($system.Count -ne 1) { Fail "the system's clean-ups were taken for the user's: $($system -join ' || ')" }
    if ($pb.HasExited) { Fail "B ended (exit code $($pb.ExitCode))" }
  } finally {
    if ($pb) { Stop-App $b $pb }
    if ($pa) { Stop-App $a $pa }
    Remove-Item -Recurse -Force $a, $b -ErrorAction SilentlyContinue
  }
  @()
}

# The user's own changes in an on-demand folder (item 6 step 4), two temp states: A shares a folder (full, the owner),
# B joins it on demand. On B, as any app would, from this process: rename, move, delete, edit a filled file, a new
# file, a new folder with a file, a folder moved in from outside, a folder deleted with what is in it. A's folder
# shows each. Changes made on A reach B's placeholders without being taken for B's user's. On a read-only folder a
# delete is refused and the file freed instead. Files made in B's folder while B was not running are taken at start.
$checks.ondemandchanges = {
  $a = New-State 'full'
  $b = New-State 'full'
  $docs = "$a\state\od\Docs"
  $ro = "$a\state\od\Ro"
  New-Item -ItemType Directory -Force "$docs\sub", "$docs\dd", $ro | Out-Null
  foreach ($n in 'ren', 'mov', 'del', 'edit', 'far', 'gone') { "$n text" | Set-Content -NoNewline "$docs\$n.txt" }
  'x text' | Set-Content -NoNewline "$docs\sub\x.txt"
  'y text' | Set-Content -NoNewline "$docs\dd\y.txt"
  'ro text' | Set-Content -NoNewline "$ro\ro.txt"
  $sa = "$a\state\control.sock"; $sb = "$b\state\control.sock"
  $bd = "$b\state\od\Docs"
  $bro = "$b\state\od\Ro"
  $pa = Start-App $a -QaControl
  $pb = $null
  # A folder's files and their text, for comparing (the folder's own .seedwell left out).
  $tree = { param($root) (Get-ChildItem -Recurse -File $root -ErrorAction SilentlyContinue | Where-Object { $_.FullName -notlike '*\.seedwell\*' } | Sort-Object FullName | ForEach-Object { "$($_.FullName.Substring($root.Length + 1).Replace('\', '/'))=$(ReadText $_.FullName)" }) -join ', ' }
  $mine = { @(Log $b | Where-Object { $_ -match 'on-demand: .* (moved here|deleted here|changed here|added here)' }) }
  try {
    $pb = Start-App $b -QaControl
    WaitForSync $a; WaitForSync $b
    & $Cli --socket $sa sync add $docs 2>&1 | Out-Null
    & $Cli --socket $sa sync add $ro 2>&1 | Out-Null
    $link = ((& $Cli --socket $sa sync share Docs --json 2>&1) -join '' | ConvertFrom-Json).link
    $link | & $Cli --socket $sb sync join - --on-demand --to $bd 2>&1 | Out-Null
    $link = ((& $Cli --socket $sa sync share Ro --role read_only --json 2>&1) -join '' | ConvertFrom-Json).link
    $link | & $Cli --socket $sb sync join - --on-demand --to $bro 2>&1 | Out-Null
    # The two roots in either order (as the core names the folders).
    WaitFor { $s = Qa $b @('ondemand'); $s -match 'placed 10 files' -and $s -match 'placed 1 files \[ro\.txt:p\]' } 60000 'the placeholders on B' | Out-Null

    # Rename, move, delete.
    Rename-Item "$bd\ren.txt" 'renamed.txt'
    Move-Item "$bd\mov.txt" "$bd\sub\mov.txt"
    Remove-Item "$bd\del.txt"
    WaitFor { (& $tree $docs) -eq 'dd/y.txt=y text, edit.txt=edit text, far.txt=far text, gone.txt=gone text, renamed.txt=ren text, sub/mov.txt=mov text, sub/x.txt=x text' } 15000 'the rename, move and delete on A' | Out-Null

    # An edit of a filled file, a new file, a new folder with a file, a folder moved in, a folder deleted.
    if ((Get-Content -Raw "$bd\edit.txt") -ne 'edit text') { Fail 'edit.txt did not read on B' }
    'edited on B' | Set-Content -NoNewline "$bd\edit.txt"
    'new on B' | Set-Content -NoNewline "$bd\new.txt"
    New-Item -ItemType Directory "$bd\nd" | Out-Null
    'z on B' | Set-Content -NoNewline "$bd\nd\z.txt"
    New-Item -ItemType Directory -Force "$b\outside\in" | Out-Null
    'w moved in' | Set-Content -NoNewline "$b\outside\in\w.txt"
    Move-Item "$b\outside\in" "$bd\in"
    Remove-Item -Recurse "$bd\dd"
    $want = 'edit.txt=edited on B, far.txt=far text, gone.txt=gone text, in/w.txt=w moved in, nd/z.txt=z on B, new.txt=new on B, renamed.txt=ren text, sub/mov.txt=mov text, sub/x.txt=x text'
    WaitFor { (& $tree $docs) -eq $want } 15000 'the edit, the new files and the folder delete on A' | Out-Null
    WaitFor { (Qa $b @('ondemand')) -match 'files \[edit\.txt:f, far\.txt:p, gone\.txt:p, new\.txt:f, renamed\.txt:p, in/w\.txt:f, nd/z\.txt:f, sub/mov\.txt:p, sub/x\.txt:p\]' } 10000 'B''s new files as placeholders in sync' | Out-Null

    # Changes on A: B follows (an edit of a file B holds, a rename, a delete), none taken for B's user's.
    $before = (& $mine).Count
    if ((Get-Content -Raw "$bd\sub\x.txt") -ne 'x text') { Fail 'sub/x.txt did not read on B' }
    'x changed on A' | Set-Content -NoNewline "$docs\sub\x.txt"
    Rename-Item "$docs\far.txt" 'farther.txt'
    Remove-Item "$docs\gone.txt"
    WaitFor { -not (Test-Path "$bd\gone.txt") -and (Test-Path "$bd\farther.txt") -and -not (Test-Path "$bd\far.txt") } 20000 'A''s rename and delete on B' | Out-Null
    WaitFor { (Get-Content -Raw "$bd\sub\x.txt") -eq 'x changed on A' } 20000 'A''s edit of sub/x.txt on B' | Out-Null
    $echo = @(& $mine | Select-Object -Skip $before)
    if ($echo) { Fail "A's changes were taken for B's user's: $($echo -join ' || ')" }

    # Names not synced (the core's rules): each stays a plain file on B, never reaches A, and the log says why.
    '*.bak' | Set-Content -NoNewline "$docs\.seedwellignore"
    WaitFor { (Test-Path "$bd\.seedwellignore") -and (Get-Content -Raw "$bd\.seedwellignore") -eq '*.bak' } 20000 'the ignore file on B' | Out-Null
    $junk = '~$edit.txt', '~WRD0001.tmp', 'Thumbs.db', 'desktop.ini', 'x.bak'
    foreach ($n in $junk) { 'junk' | Set-Content -NoNewline -LiteralPath "$bd\$n" }
    foreach ($n in $junk) {
      WaitFor { Log $b | Where-Object { $_.Contains("on-demand: $n not synced (") } } 10000 "$n to be left alone" | Out-Null
      if ((Qa $b @('ondemand', 'placeholder', "$bd\$n")) -notmatch 'placeholder False') { Fail "$n became a placeholder" }
      if (Test-Path -LiteralPath "$docs\$n") { Fail "$n reached A" }
    }

    # Save by replace. $id: an item's id in the core; $watch: A's file polled every few ms, stopped with $seen (True: it went missing).
    $id = { param($name) ((& $Cli --socket $sb sync items Docs --json 2>&1) -join '' | ConvertFrom-Json).items | Where-Object { $_.name -eq $name } | ForEach-Object { $_.id } }
    $watch = { param($path) $ps = [powershell]::Create().AddScript('param($p) while ([IO.File]::Exists($p)) { Start-Sleep -Milliseconds 5 }').AddArgument($path); @{ Ps = $ps; Run = $ps.BeginInvoke() } }
    $seen = { param($w) $gone = $w.Run.IsCompleted; $w.Ps.Stop(); $w.Ps.Dispose(); $gone }
    $word = { param($text)
      Rename-Item "$bd\edit.txt" '~WRL0001.tmp'
      $text | Set-Content -NoNewline "$bd\~WRD0000.tmp"
      Rename-Item "$bd\~WRD0000.tmp" 'edit.txt'
      Remove-Item "$bd\~WRL0001.tmp"
    }
    # Word's way: the document renamed aside, a temp file renamed to its name, the old one deleted. One edit, the same item.
    if ((Get-Content -Raw "$bd\edit.txt") -ne 'edited on B') { Fail 'edit.txt did not read on B before the save' }
    $before = & $id 'edit.txt'
    $w = & $watch "$docs\edit.txt"
    & $word 'saved by replace'
    WaitFor { (ReadText "$docs\edit.txt") -eq 'saved by replace' } 15000 'the save on A' | Out-Null
    if (& $seen $w) { Fail 'edit.txt went missing on A during the save' }
    WaitFor { (Qa $b @('ondemand', 'placeholder', "$bd\edit.txt")) -match 'placeholder True' } 10000 'edit.txt a placeholder again' | Out-Null
    if ((& $id 'edit.txt') -ne $before) { Fail "the save made a new item: $before, then $(& $id 'edit.txt')" }
    foreach ($n in '~WRL0001.tmp', '~WRD0000.tmp') { if ((Test-Path "$bd\$n") -or (Test-Path "$docs\$n")) { Fail "$n is left over" } }
    # With nothing changed: nothing is deleted, also after the window.
    $w = & $watch "$docs\edit.txt"
    & $word 'saved by replace'
    Start-Sleep 8
    if (& $seen $w) { Fail 'edit.txt went missing on A in a save with nothing changed' }
    if ((Get-Content -Raw "$docs\edit.txt") -ne 'saved by replace' -or (& $id 'edit.txt') -ne $before) { Fail 'a save with nothing changed changed something' }
    # Delete, then a new file at once: the same item too.
    $before = & $id 'new.txt'
    $w = & $watch "$docs\new.txt"
    Remove-Item "$bd\new.txt"
    'recreated' | Set-Content -NoNewline "$bd\new.txt"
    WaitFor { (ReadText "$docs\new.txt") -eq 'recreated' } 15000 'the recreated file on A' | Out-Null
    if (& $seen $w) { Fail 'new.txt went missing on A' }
    if ((& $id 'new.txt') -ne $before) { Fail 'delete and create at once made a new item' }
    # Not a save: later, it is a new item, and A deleted the old one in between.
    $before = & $id 'renamed.txt'
    Remove-Item "$bd\renamed.txt"
    WaitFor { -not (Test-Path "$docs\renamed.txt") } 15000 'the delete on A' | Out-Null
    Start-Sleep 1
    'again' | Set-Content -NoNewline "$bd\renamed.txt"
    WaitFor { (ReadText "$docs\renamed.txt") -eq 'again' } 15000 'the new file on A' | Out-Null
    if ((& $id 'renamed.txt') -eq $before) { Fail 'a file made after the window kept the old item' }
    # An online-only file renamed to a name not synced: refused, nothing changes, and the core's notice says why.
    if ((Qa $b @('ondemand')) -notmatch 'farther\.txt:p') { Fail 'farther.txt is not online only' }
    $refused = $false
    try { Rename-Item "$bd\farther.txt" '~farther.tmp' -ErrorAction Stop } catch { $refused = $true }
    if (-not $refused -or -not (Test-Path "$bd\farther.txt") -or -not (Test-Path "$docs\farther.txt")) { Fail 'the rename of an online-only file to a name not synced was not refused' }
    WaitFor { Log $b | Where-Object { $_ -match 'notice: .*farther\.txt. wasn.t renamed' } } 5000 'the notice for the refused rename' | Out-Null

    # A folder the feed deletes: its placeholders and what may go go; a file that may not stays, and the folder with it, plain.
    New-Item -ItemType Directory "$docs\kf" | Out-Null
    'k' | Set-Content -NoNewline "$docs\kf\k.txt"
    WaitFor { Test-Path "$bd\kf\k.txt" } 20000 'kf on B' | Out-Null
    foreach ($n in '~keep.tmp', 'Thumbs.db') { 'junk' | Set-Content -NoNewline "$bd\kf\$n" }
    WaitFor { Log $b | Where-Object { $_.Contains('on-demand: kf/~keep.tmp not synced (') } } 10000 '~keep.tmp to be left alone' | Out-Null
    Remove-Item -Recurse "$docs\kf"
    WaitFor { -not (Test-Path "$bd\kf\k.txt") } 20000 'kf/k.txt gone from B' | Out-Null
    WaitFor { -not (Test-Path "$bd\kf\Thumbs.db") } 5000 'kf/Thumbs.db gone from B' | Out-Null
    if ((Get-Content -Raw "$bd\kf\~keep.tmp") -ne 'junk') { Fail 'a file not synced was deleted with its folder' }
    if ((Qa $b @('ondemand', 'placeholder', "$bd\kf")) -notmatch 'placeholder False') { Fail 'the folder kept for it is still a placeholder' }

    # A read-only folder: a delete is refused, the file is freed instead and stays listed; A keeps it.
    if ((Get-Content -Raw "$bro\ro.txt") -ne 'ro text') { Fail 'ro.txt did not read on B' }
    $refused = $false
    try { Remove-Item "$bro\ro.txt" -ErrorAction Stop } catch { $refused = $true }
    if (-not $refused) { Fail 'the delete on a read-only folder was not refused' }
    WaitFor { Log $b | Where-Object { $_ -like '*on-demand: ro.txt not deleted (read-only here): freed instead*' } } 5000 'the read-only delete to free instead' | Out-Null
    WaitFor { (Qa $b @('ondemand')) -match 'files \[ro\.txt:pU?\]' } 10000 'ro.txt freed and still listed on B' | Out-Null
    if (-not (Test-Path "$ro\ro.txt")) { Fail 'the read-only delete reached A' }

    # Made while B was not running: taken at start.
    Stop-App $b $pb
    $pb = $null
    'made offline' | Set-Content -NoNewline "$bd\offline.txt"
    'deep offline' | Set-Content -NoNewline "$bd\sub\deep.txt"
    $pb = Start-App $b -QaControl
    WaitFor { (& $tree $docs) -match 'offline\.txt=made offline' -and (& $tree $docs) -match 'sub/deep\.txt=deep offline' } 30000 'the files made offline on A' | Out-Null

    # The folder kept above: A deleted it, so the sweep at each start leaves it here (one issue), and it never returns on A.
    $issues = { @(((& $Cli --socket $sb sync list --json 2>&1) -join '' | ConvertFrom-Json).folders | Where-Object { $_.name -eq 'Docs' } | ForEach-Object { $_.issues } | Where-Object { $_.path -eq 'kf' }) }
    $stayed = { foreach ($i in 1..5) { if (Test-Path "$docs\kf") { Fail 'the kept folder came back on A' }; Start-Sleep 3 } }
    WaitFor { Log $b | Where-Object { $_ -like '*on-demand: kf not synced (kept: *' } } 15000 'the sweep to leave the kept folder' | Out-Null
    & $stayed
    if (@(& $issues).Count -ne 1) { Fail "the kept folder's issues: $((& $issues | ForEach-Object { $_.reason }) -join ' || ')" }
    Stop-App $b $pb
    $pb = Start-App $b -QaControl
    WaitForSync $b
    & $stayed
    if (@(& $issues).Count -ne 1) { Fail "the kept folder's issues after another start: $((& $issues | ForEach-Object { $_.reason }) -join ' || ')" }
    # A synced file in it takes it up again; the file not synced stays here only.
    'n' | Set-Content -NoNewline "$bd\kf\new.txt"
    WaitFor { (ReadText "$docs\kf\new.txt") -eq 'n' } 15000 'the file in the kept folder on A' | Out-Null
    if (Test-Path "$docs\kf\~keep.tmp") { Fail 'the file not synced reached A' }
    WaitFor { (Qa $b @('ondemand', 'placeholder', "$bd\kf")) -match 'placeholder True' -and (Qa $b @('ondemand', 'placeholder', "$bd\kf\new.txt")) -match 'placeholder True' } 10000 'kf and new.txt as placeholders' | Out-Null
    if ((Qa $b @('ondemand', 'placeholder', "$bd\kf\~keep.tmp")) -notmatch 'placeholder False') { Fail '~keep.tmp became a placeholder' }
    WaitFor { @(& $issues).Count -eq 0 } 10000 'the kept folder''s issue to go' | Out-Null
    $copies = @(Get-ChildItem -Recurse "$docs\kf", "$bd\kf" | Where-Object { $_.Name -notin 'new.txt', '~keep.tmp' })
    if ($copies) { Fail "more in the folder taken up: $($copies.FullName -join ', ')" }
    $errors = @(Log $b | Where-Object { $_ -match 'on-demand \S+: .*convert' })
    if ($errors) { Fail "a file adopted twice: $($errors -join ' || ')" }
    # A new folder nobody deleted, holding only a file not synced, is a folder like any other.
    New-Item -ItemType Directory "$bd\fresh" | Out-Null
    'x' | Set-Content -NoNewline "$bd\fresh\~x.tmp"
    WaitFor { Test-Path "$docs\fresh" } 15000 'the new folder on A' | Out-Null
    if (Test-Path "$docs\fresh\~x.tmp") { Fail '~x.tmp reached A' }
    if ($pb.HasExited) { Fail "B ended (exit code $($pb.ExitCode))" }
  } finally {
    if ($pb) { Stop-App $b $pb }
    if ($pa) { Stop-App $a $pa }
    Remove-Item -Recurse -Force $a, $b -ErrorAction SilentlyContinue
  }
  @()
}

$checks.settings = {
  $dir = New-State
  $p = Start-App $dir -QaControl
  try {
    $cfg = "$dir\state\config.toml"
    $before = @(Log $dir | Where-Object { $_ -like '*update: *automatic check*' }).Count
    (Get-Content $cfg -Raw) -replace 'start_delay_secs = 600', 'start_delay_secs = 599' | Set-Content -NoNewline -Encoding utf8 $cfg
    WaitFor { Log $dir | Where-Object { $_ -like '*settings changed: config.toml*' } } 10000 'the settings change' | Out-Null
    # A change outside Settings (here a hand edit) reaches the updater too: it plans its next check again.
    WaitFor { @(Log $dir | Where-Object { $_ -like '*update: *automatic check*' }).Count -gt $before } 5000 'the updater to plan again' | Out-Null
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @()
}

# The failure: another program's save of a file in an on-demand folder is refused ("being used by another process")
# because of something Seedwell holds open while it fills, checks or adopts that file (CI saw it on f1f5245). Another
# process writes straight after reading a placeholder, and again and again into a new file while the host adopts it;
# a refused write fails this check, and says who held the file. Every last write reaches the other device.
$checks.ondemandwrites = {
  $a = New-State 'full'
  $b = New-State 'full'
  $docs = "$a\state\od\Docs"
  New-Item -ItemType Directory -Force $docs | Out-Null
  $count = 40
  1..$count | ForEach-Object { "text $_" | Set-Content -NoNewline "$docs\p$_.txt" }
  'one' | Set-Content -NoNewline "$docs\one.txt"
  $sa = "$a\state\control.sock"; $sb = "$b\state\control.sock"
  $bd = "$b\state\od\Docs"
  $pa = Start-App $a -QaControl
  $pb = $null
  $refused = [System.Collections.Generic.List[string]]::new()
  # One write as a program makes it (others may read while it writes); a refusal is noted with who held the file.
  $write = {
    param($path, $text)
    try { [IO.File]::WriteAllText($path, $text) }
    catch [IO.IOException] {
      $holder = [QaHolder]::Of($path)
      $at = [Diagnostics.Stopwatch]::StartNew()
      while ($at.ElapsedMilliseconds -lt 5000) { try { [IO.File]::WriteAllText($path, $text); break } catch [IO.IOException] { } }
      $refused.Add("$(Split-Path -Leaf $path): held by $holder; free after $($at.ElapsedMilliseconds) ms; $($_.Exception.Message)")
    }
  }
  try {
    $pb = Start-App $b -QaControl
    WaitForSync $a; WaitForSync $b
    $added = (& $Cli --socket $sa sync add $docs 2>&1) -join ' '
    if ($added -notlike 'Sharing Docs*') { throw "sync add: $added" }
    $link = ((& $Cli --socket $sa sync share Docs --json 2>&1) -join '' | ConvertFrom-Json).link
    $link | & $Cli --socket $sb sync join - --on-demand --to $bd 2>&1 | Out-Null
    WaitFor { (Qa $b @('ondemand')) -match "placed $($count + 1) files" } 60000 'the placeholders on B' | Out-Null

    # The last save lost: overwriting a file (CREATE_ALWAYS, as File.WriteAllText and many programs do) takes its
    # placeholder away, and a plain file sends no close notification, so with nothing else going on the write would never
    # leave this PC. With all quiet: it reaches A, the file is a placeholder again, and so does the next one.
    $one = "$bd\one.txt"
    if ([IO.File]::ReadAllText($one) -ne 'one') { Fail 'one.txt did not read on B' }
    Start-Sleep -Seconds 3
    foreach ($text in 'overwritten once', 'overwritten twice') {
      [IO.File]::WriteAllText($one, $text)
      try { WaitFor { (ReadText "$docs\one.txt") -eq $text } 20000 "'$text' on A" | Out-Null }
      catch { Fail "the overwrite was lost: B has '$([IO.File]::ReadAllText($one))', A has '$(Get-Content -Raw "$docs\one.txt")', placeholder on B: $(Qa $b @('ondemand', 'placeholder', $one))" }
      WaitFor { (Qa $b @('ondemand', 'placeholder', $one)) -match 'placeholder True' } 20000 "one.txt a placeholder again after '$text'" | Out-Null
      Start-Sleep -Seconds 3
    }

    # The same while the app is not running: no notification and no watcher, so it must look when it comes back. One
    # save that overwrites (a plain file by then) and one that writes in place (a placeholder still, its change unseen).
    if ([IO.File]::ReadAllText("$bd\p40.txt") -ne 'text 40') { Fail 'p40.txt did not read on B' }
    Start-Sleep -Seconds 3
    Stop-App $b $pb
    [IO.File]::WriteAllText($one, 'overwritten away')
    $inplace = [IO.File]::Open("$bd\p40.txt", 'Open', 'Write', 'Read')
    $bytes = [Text.Encoding]::ASCII.GetBytes('in place 40')
    $inplace.Write($bytes, 0, $bytes.Length); $inplace.Dispose()
    $pb = Start-App $b -QaControl
    foreach ($was in @{ f = 'one.txt'; t = 'overwritten away' }, @{ f = 'p40.txt'; t = 'in place 40' }) {
      try { WaitFor { (ReadText "$docs\$($was.f)") -eq $was.t } 30000 "'$($was.t)' on A" | Out-Null }
      catch { Fail "the save made while the app was not running was lost: $($was.f) on B has '$([IO.File]::ReadAllText("$bd\$($was.f)"))', on A '$(Get-Content -Raw "$docs\$($was.f)")'" }
    }
    WaitFor { (Qa $b @('ondemand', 'placeholder', $one)) -match 'placeholder True' } 20000 'one.txt a placeholder again after the restart' | Out-Null

    # A save cut short (CI on 61c0913: 'edited 5' became 'edited', the 6 bytes of 'text 5'): updating a placeholder from
    # the feed's item set the file's size to the item's, so a write that came just before was truncated, and the damage
    # then synced. A longer text written at that moment stays whole here and arrives whole.
    $whole = 'written at the worst moment of an update'
    $during = Qa $b @('ondemand', 'write-during-update', 'one.txt', $whole) 40000
    if ($during -notmatch "holds `"$whole`"") { Fail "a write during an update was damaged: $during. B: $((Log $b | Where-Object { $_ -like '*on-demand: one*' }) -join ' / ')" }
    try { WaitFor { (ReadText "$docs\one.txt") -eq $whole } 20000 'the write during an update on A' | Out-Null }
    catch { Fail "a write during an update did not arrive whole: B has '$([IO.File]::ReadAllText($one))', A has '$(Get-Content -Raw "$docs\one.txt")'" }

    # A read that fills the placeholder, and writes straight after it, on through the moment the feed says the file is
    # here and the host updates the placeholder (the save that was refused on CI).
    1..($count - 1) | ForEach-Object {
      $f = "$bd\p$_.txt"
      if ([IO.File]::ReadAllText($f) -ne "text $_") { Fail "p$_.txt did not read on B" }
      $for = [Diagnostics.Stopwatch]::StartNew()
      while ($for.ElapsedMilliseconds -lt 700) { & $write $f "edited $_ at $($for.ElapsedTicks)" }
      & $write $f "edited $_"
    }
    # A new file written again and again: the host looks at it and adopts it meanwhile.
    1..$count | ForEach-Object {
      $f = "$bd\n$_.txt"
      foreach ($w in 1..12) { & $write $f "new $_ write $w"; Start-Sleep -Milliseconds 25 }
    }
    if ($refused.Count -gt 0) { Fail "$($refused.Count) writes refused: $(($refused | Select-Object -First 8) -join ' || ')" }

    $missing = { @(1..$count | ForEach-Object { if ($_ -lt $count -and (ReadText "$docs\p$_.txt") -ne "edited $_") { "p$_" }; if ((ReadText "$docs\n$_.txt") -ne "new $_ write 12") { "n$_" } }) }
    try { WaitFor { (& $missing).Count -eq 0 } 60000 'every last write on A' | Out-Null }
    catch {
      $late = & $missing
      $said = $late | Select-Object -First 3 | ForEach-Object { "$_ on B says '$(Get-Content -Raw "$bd\$_.txt")', placeholder $((Qa $b @('ondemand', 'placeholder', "$bd\$_.txt")) -match 'placeholder True'); on A '$(ReadText "$docs\$_.txt")'" }
      Fail "not on A within 60 s: $($late -join ', '). $($said -join ' || ')"
    }  } finally {
    Stop-App $b $pb; Stop-App $a $pa
    Remove-Item -Recurse -Force $a, $b -ErrorAction SilentlyContinue
  }
  @()
}

# A save lost to a dehydrate: the core says a filled file is an earlier version (another device changed it), and a program
# writes before the host lets the content go: the update is refused, as nothing may write since the core looked, and the
# write survives as a conflict copy. Also conflict copies without end, below.
$checks.ondemandstale = {
  $a = New-State 'full'
  $b = New-State 'full'
  $docs = "$a\state\od\Docs"
  New-Item -ItemType Directory -Force $docs | Out-Null
  foreach ($name in 'one', 'two', 'three') { $name | Set-Content -NoNewline "$docs\$name.txt" }
  $sa = "$a\state\control.sock"; $sb = "$b\state\control.sock"
  $bd = "$b\state\od\Docs"
  $pa = Start-App $a -QaControl
  $pb = $null
  try {
    $pb = Start-App $b -QaControl
    WaitForSync $a; WaitForSync $b
    $added = (& $Cli --socket $sa sync add $docs 2>&1) -join ' '
    if ($added -notlike 'Sharing Docs*') { throw "sync add: $added" }
    $link = ((& $Cli --socket $sa sync share Docs --json 2>&1) -join '' | ConvertFrom-Json).link
    $link | & $Cli --socket $sb sync join - --on-demand --to $bd 2>&1 | Out-Null
    WaitFor { (Qa $b @('ondemand')) -match 'placed 3 files' } 60000 'the placeholders on B' | Out-Null
    foreach ($name in 'one', 'two', 'three') { if ([IO.File]::ReadAllText("$bd\$name.txt") -ne $name) { Fail "$name.txt did not read on B" } }
    Start-Sleep -Seconds 3

    $write = 'the write that landed as the file was let go of'
    Qa $b @('ondemand', 'write-when-stale', 'one.txt', $write) | Out-Null
    'from A' | Set-Content -NoNewline "$docs\one.txt"
    WaitFor { (Qa $b @('ondemand', 'wrote-when-stale')) -match 'wrote True' } 30000 'the write at the moment the file is let go of' | Out-Null

    $holders = { foreach ($d in $bd, $docs) { Get-ChildItem -LiteralPath $d -File -ErrorAction SilentlyContinue | Where-Object { (ReadText $_.FullName) -eq $write } | ForEach-Object { $_.FullName } } }
    try { WaitFor { @(& $holders).Count -gt 0 } 20000 'the write in a file' | Out-Null }
    catch {
      $there = foreach ($d in $bd, $docs) { Get-ChildItem -LiteralPath $d -File -ErrorAction SilentlyContinue | ForEach-Object { "$($_.FullName): '$(ReadText $_.FullName)'" } }
      Fail "the write was lost: $($there -join ' || ')"
    }
    # Both devices end up holding it.
    WaitFor { $h = @(& $holders); @($h | Where-Object { $_ -like "$bd\*" }).Count -gt 0 -and @($h | Where-Object { $_ -like "$docs\*" }).Count -gt 0 } 30000 'the write on both devices' | Out-Null

    # Conflict copies without end (a new one every sweep, on every device): the file keeps differing from the item, so each
    # look at it is a new conflict. One write is one copy, the devices agree on the file, and it stays that way past two sweeps.
    Start-Sleep -Seconds 25
    $settled = {
      param($name, $theirs)
      $copies = @(Get-ChildItem -LiteralPath $bd -File -Filter "$name (conflict*")
      $bHas = Get-Content -Raw -LiteralPath "$bd\$name.txt"; $aHas = Get-Content -Raw -LiteralPath "$docs\$name.txt"
      if ($copies.Count -ne 1 -or $bHas -ne $theirs -or $aHas -ne $theirs) {
        $held = ($copies | ForEach-Object { Get-Content -Raw -LiteralPath $_.FullName }) -join ', '
        $saidB = (Log $b | Where-Object { $_ -like "*on-demand: $name*" -and $_ -notlike '*filled (*' } | Select-Object -First 6) -join ' / '
        $copied = @(Get-ChildItem -LiteralPath $bd, $docs -File -Filter "$name (conflict*" | ForEach-Object { "$($_.Directory.Parent.Parent.Parent.Name)\$($_.Name) at $($_.LastWriteTime.ToString('HH:mm:ss.fff')), made $($_.CreationTime.ToString('HH:mm:ss.fff'))" }) -join ' / '
        "$name`: one write made $($copies.Count) conflict copies ($held); on B '$bHas', on A '$aHas'; B said: $saidB; copies: $copied"
      }
    }
    $wrong = @(& $settled 'one' 'from A')

    # The same with no hook: both devices edit while this one is not running, one save in place and one that overwrites
    # (no placeholder left). Each is one copy, and the devices agree on the file.
    Stop-App $b $pb
    $inplace = [IO.File]::Open("$bd\two.txt", 'Open', 'Write', 'Read')
    $bytes = [Text.Encoding]::ASCII.GetBytes('written here in place')
    $inplace.Write($bytes, 0, $bytes.Length); $inplace.Dispose()
    [IO.File]::WriteAllText("$bd\three.txt", 'overwritten here')
    # The later edit wins by its time, and a file's time moves in ticks of about 15 ms: within one, the higher device id wins.
    Start-Sleep -Milliseconds 1100
    'two from A' | Set-Content -NoNewline "$docs\two.txt"
    'three from A' | Set-Content -NoNewline "$docs\three.txt"
    Start-Sleep -Seconds 2
    $pb = Start-App $b -QaControl
    foreach ($name in 'two', 'three') {
      WaitFor { @(Get-ChildItem -LiteralPath $bd -File -Filter "$name (conflict*").Count -gt 0 } 40000 "$name's conflict copy" | Out-Null
    }
    Start-Sleep -Seconds 25
    $wrong += @('two', 'three' | ForEach-Object { & $settled $_ "$_ from A" })
    if ($wrong.Count -gt 0) { Fail ($wrong -join ' || ') }
  } finally {
    Stop-App $b $pb; Stop-App $a $pa
    Remove-Item -Recurse -Force $a, $b -ErrorAction SilentlyContinue
  }
  @()
}

# A folder listed first and hashed when first fetched (build 68), as the file manager meets it. The failures it names: a
# just-joined folder that makes Explorer wait for its owner, or never fills by itself; a call that waits for a scan or a
# stream; a first download of an unhashed file with no progress (Cloud Files cancels one silent for 60 s) or wrong bytes;
# a file only its first downloader still has that a second device cannot get. Listings go through the shell's namespace,
# as Explorer's do, with no window.
$checks.ondemandfirst = {
  $a = New-State 'full'; $b = New-State 'full'; $c = New-State 'full'
  $docs = "$a\state\od\Docs"; $bd = "$b\state\od\Docs"; $cd = "$c\state\od\Docs"
  New-Item -ItemType Directory -Force $docs, "$a\state\bulk" | Out-Null
  $bytes = New-Object byte[] (40MB)
  (New-Object Random 11).NextBytes($bytes)
  [IO.File]::WriteAllBytes("$docs\big.bin", $bytes)
  $want = (Get-FileHash "$docs\big.bin" -Algorithm SHA256).Hash.ToLowerInvariant()
  'small text' | Set-Content -NoNewline "$docs\small.txt"
  $huge = [IO.File]::Create("$a\state\bulk\huge.bin"); $huge.SetLength(1GB); $huge.Dispose()
  $sa = "$a\state\control.sock"; $sb = "$b\state\control.sock"; $sc = "$c\state\control.sock"
  $shell = New-Object -ComObject Shell.Application
  $pa = Start-App $a -QaControl
  $pb = $null; $pc = $null
  # How long a call took (ms); the slowest of each is kept.
  $slowest = @{}
  function Timed([string]$what, [scriptblock]$call) {
    $t = [Diagnostics.Stopwatch]::StartNew()
    $out = & $call
    $ms = $t.ElapsedMilliseconds
    if (-not $slowest.ContainsKey($what) -or $slowest[$what] -lt $ms) { $slowest[$what] = $ms }
    $out
  }
  function Status([string]$sock) { (& $Cli --socket $sock sync status Docs --json 2>&1) -join '' | ConvertFrom-Json }
  function OnDisk([string]$path) { @([IO.Directory]::GetFileSystemEntries($path) | ForEach-Object { [IO.Path]::GetFileName($_) } | Sort-Object) -join ',' }
  # A session with no shell has no namespace to ask.
  $noShell = @{}
  function Shown([string]$path) {
    $folder = if ($null -ne $shell) { $shell.NameSpace($path) }
    if ($null -eq $folder) {
      if (-not $noShell.Said) { Write-Host '  ondemandfirst: no shell namespace in this session, so listed through the file system only'; $noShell.Said = $true }
      return OnDisk $path
    }
    # By the file's name: an item's Name is what it is shown as (no .txt where known extensions are hidden, as on a CI runner).
    @($folder.Items() | ForEach-Object { [IO.Path]::GetFileName($_.Path) } | Sort-Object) -join ','
  }
  function NoStall([string]$during, [int]$limit) {
    $late = @($slowest.GetEnumerator() | Where-Object { $_.Value -gt $limit } | ForEach-Object { "$($_.Key) $($_.Value) ms" })
    if ($late.Count -gt 0) { Fail "a call waited during $during (limit $limit ms): $($late -join ', ')" }
    $slowest.Clear()
  }
  try {
    WaitForSync $a
    $added = (& $Cli --socket $sa sync add $docs --on-demand 2>&1) -join ' '
    if ($added -notlike 'Sharing Docs*') { Fail "sync add: $added" }
    $link = ((& $Cli --socket $sa sync share Docs --json 2>&1) -join '' | ConvertFrom-Json).link
    $linkC = ((& $Cli --socket $sa sync share Docs --json 2>&1) -join '' | ConvertFrom-Json).link
    $listed = (& $Cli --socket $sa sync list --json 2>&1) -join '' | ConvertFrom-Json
    $maker = $listed.device_name; $port = $listed.port
    Stop-App $a $pa; $pa = $null

    # One: the owner is away. B joins, the folder opens at once and empty, and says whom it waits for.
    $env:SEEDWELL_QA_LIMIT_LAN = '1'
    try { $pb = Start-App $b -QaControl } finally { Remove-Item Env:SEEDWELL_QA_LIMIT_LAN }
    WaitForSync $b
    $joined = ($link | & $Cli --socket $sb sync join - --on-demand --to $bd 2>&1) -join ' '
    if ($joined -notlike 'Joining Docs*') { Fail "sync join: $joined" }
    WaitFor { (Qa $b @('ondemand')) -match 'roots 1 \[\S+ connected True' } 10000 'the empty root on B' | Out-Null
    $empty = Timed 'the shell listing the just-joined folder' { Shown $bd }
    if ($empty) { Fail "the folder should be empty before its owner lists it: $empty" }
    $status = Status $sb
    # Not admitted yet: its progress line says it; admitted, with the owner away: its waiting line.
    if ("$($status.waiting) $($status.progress)" -notmatch [regex]::Escape($maker)) { Fail "the folder does not say it waits for $maker`: $($status | ConvertTo-Json -Compress)" }
    NoStall 'a join with the owner away' 2000

    # Its owner comes: the placeholders appear with nothing asked, and hold no bytes yet.
    $pa = Start-App $a -QaControl
    WaitForSync $a
    $portNow = ((& $Cli --socket $sa sync list --json 2>&1) -join '' | ConvertFrom-Json).port
    try { WaitFor { if ((OnDisk $bd) -eq 'big.bin,small.txt') { $true } } 60000 'the placeholders on B by themselves' | Out-Null }
    catch {
      $said = { param($dir) @(CoreLog $dir | Select-Object -Last 12) -join ' / ' }
      Fail "the placeholders did not come by themselves; on disk '$(OnDisk $bd)', in the shell '$(Shown $bd)'; A's port was $port and is $portNow; B: $((Status $sb) | ConvertTo-Json -Compress); host: $(Qa $b @('ondemand')); A's core: $(& $said $a); B's core: $(& $said $b)"
    }
    # The shell shows what the file system holds (its answer is not cached from the empty folder).
    WaitFor { if ((Shown $bd) -eq 'big.bin,small.txt') { $true } } 10000 "the placeholders in the shell (it says '$(Shown $bd)')" | Out-Null
    if ((Qa $b @('ondemand')) -notmatch 'big\.bin:p, small\.txt:p') { Fail "not placeholders: $(Qa $b @('ondemand'))" }

    # Two: a scan on A. Nothing on A waits for it, and it has to have really been running.
    $pc = Start-App $c -QaControl
    WaitForSync $c
    ($linkC | & $Cli --socket $sc sync join - --on-demand --to $cd 2>&1) | Out-Null
    WaitFor { (Qa $c @('ondemand')) -match 'big\.bin:p, small\.txt:p' } 60000 'the placeholders on C' | Out-Null
    (& $Cli --socket $sa sync add "$a\state\bulk" 2>&1) | Out-Null
    $scanning = 0
    $until = [DateTime]::Now.AddSeconds(30)
    do {
      $folders = Timed 'sync list on A' { (& $Cli --socket $sa sync list --json 2>&1) -join '' | ConvertFrom-Json }
      $bulk = @($folders.folders | Where-Object { $_.name -eq 'bulk' })
      $busy = $bulk.Count -gt 0 -and $bulk[0].state -eq 'scanning'
      if ($busy) { $scanning++ }
      Timed 'the app on A' { Qa $a @('ondemand') } | Out-Null
      Timed 'the shell listing the folder on A' { Shown $docs } | Out-Null
    } while (($busy -or $scanning -eq 0) -and [DateTime]::Now -lt $until)
    if ($scanning -eq 0) { Fail 'no scan was seen on A, so nothing was tested during one' }
    NoStall "a scan on A (seen scanning $scanning times)" 3000

    # Three: the first download of an unhashed file, slowed to 4 MB/s. Progress, A's bytes, and nothing waits for the stream.
    & $Cli --socket $sb sync settings --down 4m 2>&1 | Out-Null
    $before = (Log $b).Count
    Launch $b @('--qa', 'ondemand', 'read', 'big.bin') | Out-Null
    $reading = 0
    $until = [DateTime]::Now.AddSeconds(90)
    do {
      $done = @(Log $b | Select-Object -Skip $before | Where-Object { $_ -like '*qa ondemand read big.bin:*' })
      if ($done.Count -gt 0) { break }
      $reading++
      Timed 'sync status on B' { Status $sb } | Out-Null
      Timed 'sync status on A' { Status $sa } | Out-Null
      Timed 'the app on B' { Qa $b @('ondemand') } | Out-Null
      Timed 'the shell listing the folder on B' { Shown $bd } | Out-Null
    } while ([DateTime]::Now -lt $until)
    if ($done.Count -eq 0) { Fail 'the first download did not end within 90 s' }
    if ($reading -lt 2) { Fail "the download was over before anything was timed ($reading looks)" }
    if ($done[-1] -notmatch "read 41943040 bytes, sha256 $want") { Fail "the first download on B: $($done[-1])" }
    $filled = WaitFor { Log $b | Where-Object { $_ -like '*on-demand: big.bin filled (Downloaded;*' } | Select-Object -Last 1 } 5000 'the core to adopt big.bin'
    if (-not ($filled -match 'progress shown (\d+) times') -or [int]$Matches[1] -lt 2) { Fail "progress was not shown while fetching: $filled" }
    NoStall "a stream to B ($reading looks)" 3000
    & $Cli --socket $sb sync settings --down unlimited 2>&1 | Out-Null

    # A second device, with the first device's maker gone: the copy B made is enough. C has to have met B first, which
    # takes LAN discovery here (a network where it is blocked leaves this part untested, and says so).
    $met = $false
    $until = [DateTime]::Now.AddSeconds(30)
    do { $met = (Status $sc).connected -ge 2; if (-not $met) { Start-Sleep -Milliseconds 500 } } while (-not $met -and [DateTime]::Now -lt $until)
    if (-not $met) { Write-Host '  ondemandfirst: C never met B (no LAN discovery here?), so the second device was not tested' }
    else {
      Stop-App $a $pa; $pa = $null
      WaitFor { $r = Qa $c @('ondemand', 'read', 'big.bin') 60000; if ($r -match "read 41943040 bytes, sha256 $want") { $r } } 60000 'C to get big.bin from B' | Out-Null
    }
  } finally {
    Stop-App $b $pb
    if ($pc) { Stop-App $c $pc }
    if ($pa) { Stop-App $a $pa }
    if ($null -ne $shell) { [void][Runtime.InteropServices.Marshal]::ReleaseComObject($shell) }
    Remove-Item -Recurse -Force $a, $b, $c -ErrorAction SilentlyContinue
  }
  @()
}

# No notification area (Explorer not running, a session without a shell, as on a CI runner): the app runs on without
# its icon (the core answers, the window opens and draws), and makes the icon when the taskbar is created. The first
# icon fails by SEEDWELL_QA_NO_TRAY (a --state-dir run only); taskbar-created calls what TaskbarCreated calls.
$checks.notray = {
  $dir = New-State
  $env:SEEDWELL_QA_NO_TRAY = '1'
  try { $p = Start-App $dir -QaControl } finally { Remove-Item env:SEEDWELL_QA_NO_TRAY }
  try {
    WaitFor { Log $dir | Where-Object { $_ -like '*tray: no icon*' } } 5000 'the failed icon in app.log' | Out-Null
    if ($p.HasExited) { Fail 'the app ended without its icon' }
    Qa $dir @('window') | Out-Null
    Qa $dir @('shot', 'notray') | Out-Null
    Copy-Item "$dir\state\qa\notray.png" "$Out\notray.png"
    $made = Qa $dir @('taskbar-created')
    if ($made -notmatch 'tray created') { Fail "no icon after the taskbar was created: $made" }
    if (-not (Log $dir | Where-Object { $_ -like '*tray: icon created*' })) { Fail 'no "tray: icon created" in app.log' }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @("$Out\notray.png")
}

$checks.quit = {
  $dir = New-State
  $p = Start-App $dir -QaControl
  try {
    $ms = Launch $dir @('--quit')
    if (-not $p.WaitForExit(30000)) { Fail 'the app did not end within 30 s of --quit' }
    $done = Log $dir | Where-Object { $_ -like '*quit: shutdown*' } | Select-Object -Last 1
    if ($done -notmatch 'finished True') { Fail "no finished shutdown in app.log: $done" }
    $last = Get-Content "$dir\state\launches.log" | Select-Object -Last 1
    if ($last -notmatch 'clean') { Fail "the run is not marked clean in launches.log: $last" }
    # Again, with nothing left to quit (an installer asks each copy it found): it ends, it never starts an app.
    $before = (Log $dir).Count
    $again = Start-Process $Exe -ArgumentList @('--state-dir', "$dir\state", '--quit') -PassThru
    if (-not $again.WaitForExit(15000)) { $again.Kill(); $again.WaitForExit(5000) | Out-Null; Fail '--quit with no app running did not end within 15 s' }
    $started = Log $dir | Select-Object -Skip $before | Where-Object { $_ -like '*launch:*' }
    if ($started) { Fail "--quit with no app running started one: $started" }
    # A QA command for an app that is not running: it ends too, never starting an app without --qa-control.
    $qa = Start-Process $Exe -ArgumentList @('--state-dir', "$dir\state", '--qa', 'window') -PassThru
    if (-not $qa.WaitForExit(15000)) { $qa.Kill(); $qa.WaitForExit(5000) | Out-Null; Fail '--qa with no app running did not end within 15 s' }
    $started = Log $dir | Select-Object -Skip $before | Where-Object { $_ -like '*launch:*' }
    if ($started) { Fail "--qa with no app running started one: $started" }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @()
}

# ---------------------------------------------------------------------------------------------- run

$results = @()
$total = [Diagnostics.Stopwatch]::StartNew()
foreach ($n in $names) {
  $clock = [Diagnostics.Stopwatch]::StartNew()
  try {
    $pictures = @(& $checks[$n])
    # A picture a check reports that is not there: its shot failed (Copy-Item only says so and goes on).
    $missing = @($pictures | Where-Object { $_ -and -not (Test-Path $_) })
    if ($missing) { Fail "pictures missing: $($missing -join ', ')" }
    $results += [pscustomobject]@{ Check = $n; Result = 'PASS'; Seconds = [math]::Round($clock.Elapsed.TotalSeconds, 1); Note = "$($pictures.Count) pictures" }
  } catch {
    $results += [pscustomobject]@{ Check = $n; Result = 'FAIL'; Seconds = [math]::Round($clock.Elapsed.TotalSeconds, 1); Note = "$_" }
  }
  $r = $results[-1]
  Write-Host ("{0,-10} {1}  {2,5:n1} s  {3}" -f $r.Check, $r.Result, $r.Seconds, $r.Note)
}
$failed = @($results | Where-Object Result -eq 'FAIL').Count
Write-Host ("{0} of {1} passed in {2:n1} s; pictures in {3}" -f ($results.Count - $failed), $results.Count, $total.Elapsed.TotalSeconds, $Out)
exit ([int]($failed -gt 0))
