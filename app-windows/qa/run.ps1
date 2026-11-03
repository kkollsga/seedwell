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
  dialogs, newchannel, links, tray, scheme, rendezvous, follow, folders, chatpage, platform, slowstart, damaged, notices, settingspage, requests, files, chooser, play, favorites, rules, cloudfiles, ondemand, ondemandfetch, ondemandpin, ondemandchanges, notray, settings, quit (all of them: all, the default).
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
$All = @('render', 'handoff', 'separate', 'gate', 'selection', 'keys', 'removal', 'info', 'alert', 'trash', 'choice', 'move', 'add', 'chat', 'dialogs', 'newchannel', 'links', 'tray', 'scheme', 'rendezvous', 'follow', 'folders', 'chatpage', 'platform', 'slowstart', 'damaged', 'notices', 'settingspage', 'requests', 'files', 'chooser', 'play', 'favorites', 'rules', 'cloudfiles', 'ondemand', 'ondemandfetch', 'ondemandpin', 'ondemandchanges', 'notray', 'settings', 'quit')

if ($Build) {
  & powershell -NoProfile -File (Join-Path $App 'build.ps1') -CoreProfile $CoreProfile | Out-Host
  if ($LASTEXITCODE -ne 0) { Write-Host 'The build failed (is the app running from bin\? Quit it with Seedwell.exe --quit).'; exit 1 }
}
if (-not (Test-Path $Exe)) { throw "No app at ${Exe}: run with -Build" }
if (-not (Test-Path $Cli)) { Push-Location $Repo; cargo build -q --bin seedwell; Pop-Location }
if (-not $Out) { $Out = Join-Path ([IO.Path]::GetTempPath()) "seedwell-qa-$([DateTime]::Now.ToString('yyyyMMdd-HHmmss'))" }
New-Item -ItemType Directory -Force $Out | Out-Null
$names = if ($Names -contains 'all') { $All } else { $Names }
foreach ($n in $names) { if ($All -notcontains $n) { throw "Unknown check '$n'. Checks: $($All -join ', ')" } }

# ---------------------------------------------------------------------------------------------- helpers

Add-Type @'
using System; using System.Runtime.InteropServices;
public static class QaForeground {
  [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  public static uint Pid() { uint p; GetWindowThreadProcessId(GetForegroundWindow(), out p); return p; }
}
// Another process freeing a placeholder's space, as Storage Sense does (the app's NOTIFY_DEHYDRATE decides).
public static class QaCloud {
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
  $pattern = "qa $($words -join ' '):"
  WaitFor { Log $dir | Select-Object -Skip $before | Where-Object { $_ -like "*$pattern*" -or $_ -like '*qa refused*' } | Select-Object -Last 1 } $ms "'$pattern' in app.log"
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
  $hang = Log $dir | Where-Object { $_ -like '*ui stall:*this run fails*' } | Select-Object -First 1
  if ($hang) { Fail "the UI thread stalled: $hang" }
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
    if ($p.ExitCode -ne 0) { Fail "the $theme render ended with $($p.ExitCode): $(Get-Content "$dir\error-$theme.txt" -TotalCount 1)" }
    if ($front -gt 0) { Fail "the $theme render held the foreground ($front samples)" }
  }
  $pictures = @(Get-ChildItem $dir -Filter *.png | ForEach-Object FullName)
  foreach ($must in 'library-files-dark', 'library-files-light', 'menu-files-dark', 'library-chat-notes-dark', 'library-chat-notes-light', 'library-chat-hover-light', 'library-chat-reply-dark', 'chat-images-light', 'chat-gallery-dark', 'menu-dark', 'menu-sync-confirm-light', 'row-menu-dark', 'remove-light', 'library-downloads-dark', 'library-info-light', 'library-removal-dark', 'library-foldersync-light', 'library-settings-dark', 'channel-menu-light', 'tray-menu-dark', 'new-channel-light', 'new-channel-join-dark', 'dialog-new-topic-dark', 'dialog-leave-light', 'dialog-agents-dark', 'library-foldersync-dark', 'foldersync-remove-light', 'add-folder-dark', 'add-folder-shared-light', 'edit-folder-dark', 'share-light', 'library-chat-dark', 'library-direct-light', 'chat-menu-dark', 'chat-files-light', 'prompt-add-request-dark', 'prompt-location-move-light', 'prompt-backlog-dark', 'settings-general-dark', 'settings-transfers-light', 'settings-folders-dark', 'settings-network-light', 'settings-agents-dark', 'settings-advanced-light', 'file-icons-dark', 'tray-both-32') {
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
    $reached = @(Log $real | Select-Object -Skip $before)
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
    $share = Qa $dir @('share', 'status')
    if ($share -notmatch 'share Share .Family.') { Fail "no Share dialog after New Channel: $share" }
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
    WaitFor { Log $dir | Where-Object { $_ -like '*new channel: open on Join*' } } 5000 'New Channel on Join' | Out-Null
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
    Qa $dir @('shot', 'rendezvous-off', 'rendezvous') | Out-Null
    Copy-Item "$dir\state\qa\rendezvous-off.png" "$Out\rendezvous-off.png"
    Qa $dir @('rendezvous', 'use', 'http://127.0.0.1:9') | Out-Null
    $done = WaitFor { $l = Qa $dir @('rendezvous', 'status'); if ($l -match 'problem True' -or $l -match 'refused "[^"]+"') { $l } } 60000 'the result'
    Qa $dir @('shot', 'rendezvous-result', 'rendezvous') | Out-Null
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
      Qa $dir (@('shot', $name) + $(if ($Dialog) { @('dialog') } else { @() })) | Out-Null
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

    $members = Qa $dir @('chatpage', 'members')
    if ($members -notmatch 'members panel [1-9] rows, open True') { Fail "the members panel has no rows: $members" }

    # Build 52: a picture dropped onto the field's text leaves its reference where it lands (the core's spacing).
    Qa $dir @('chatpage', 'type', 'Hello world') | Out-Null
    # The drop's hit-test at a point: just inside the leading edge of the 'w' of "world" (character 6) is index 6.
    $dropped = Qa $dir @('chatpage', 'drop-over', '6', "$dir\picture.png")
    if ($dropped -notmatch 'hit 6,') { Fail "the drop's point did not hit character 6: $dropped" }
    if ($dropped -notmatch 'draft "Hello !\[Image \d+\]\(att:\d+\) ?world"') { Fail "the dropped picture's reference is not where it landed: $dropped" }
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  $pictures.ToArray()
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
    $mode = (& $Cli --socket $sock sync mode Docs on-demand 2>&1) -join ' '
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
    if ($read -notmatch 'read failed: The cloud sync provider failed to perform the operation due to network being unavailable\..*small\.txt:p') { throw "read small.txt with A gone: $read" }
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
  $tree = { param($root) (Get-ChildItem -Recurse -File $root -ErrorAction SilentlyContinue | Where-Object { $_.FullName -notlike '*\.seedwell\*' } | Sort-Object FullName | ForEach-Object { "$($_.FullName.Substring($root.Length + 1).Replace('\', '/'))=$(Get-Content -Raw $_.FullName)" }) -join ', ' }
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
