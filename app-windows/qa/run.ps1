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
  dialogs, newchannel, links, tray, scheme, rendezvous, follow, folders, chatpage, platform, slowstart, damaged, notray, settings, quit (all of them: all, the default).
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
$All = @('render', 'handoff', 'separate', 'gate', 'selection', 'keys', 'removal', 'info', 'alert', 'trash', 'choice', 'move', 'add', 'chat', 'dialogs', 'newchannel', 'links', 'tray', 'scheme', 'rendezvous', 'follow', 'folders', 'chatpage', 'platform', 'slowstart', 'damaged', 'notray', 'settings', 'quit')

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
'@

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
function Qa([string]$dir, [string[]]$words) {
  $before = (Log $dir).Count
  Launch $dir (@('--qa') + $words) | Out-Null
  $pattern = "qa $($words -join ' '):"
  WaitFor { Log $dir | Select-Object -Skip $before | Where-Object { $_ -like "*$pattern*" -or $_ -like '*qa refused*' } | Select-Object -Last 1 } 10000 "'$pattern' in app.log"
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
  foreach ($must in 'menu-dark', 'menu-sync-confirm-light', 'row-menu-dark', 'remove-light', 'library-downloads-dark', 'library-info-light', 'library-removal-dark', 'library-foldersync-light', 'library-settings-dark', 'channel-menu-light', 'tray-menu-dark', 'new-channel-light', 'new-channel-join-dark', 'dialog-new-topic-dark', 'dialog-leave-light', 'dialog-agents-dark', 'library-foldersync-dark', 'foldersync-remove-light', 'add-folder-dark', 'add-folder-shared-light', 'edit-folder-dark', 'share-light', 'library-chat-dark', 'library-direct-light', 'chat-menu-dark', 'chat-files-light', 'file-icons-dark', 'tray-both-32') {
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
    Start-Process $Exe -ArgumentList '--unregister' -Wait
    if (Test-Path $test) { Fail 'the seedwell: key is left after --unregister' }
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

    Qa $dir @('chatpage', 'type', 'Hello **there** from the PC') | Out-Null
    $sent = Qa $dir @('chatpage', 'send')
    if ($sent -notmatch 'sent; cards 1, empty "", last "Hello there from the PC"') { Fail "the message was not sent and drawn: $sent" }

    "## Status`n- one`n- two" | & $Cli --socket "$dir\state\control.sock" chat send 'QA-Chat' --as QA-Agent - | Out-Null
    $agent = WaitFor { $s = Qa $dir @('chatpage'); if ($s -match 'cards 2') { $s } } 10000 "the agent's message"
    if ($agent -notmatch 'last "Status / .+ one / .+ two"') { Fail "the agent's Markdown is not the core's blocks: $agent" }

    Qa $dir @('chatpage', 'reply', '0') | Out-Null
    Qa $dir @('chatpage', 'type', 'A reply') | Out-Null
    $reply = Qa $dir @('chatpage', 'send')
    if ($reply -notmatch 'reply quote "[^"]+"') { Fail "the reply has no quote: $reply" }

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
