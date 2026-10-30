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

  Checks: render, handoff, separate, gate, selection, removal, info, alert, trash, choice, settings, quit (all of them:
  all, the default).
  -Build builds the app first (build.ps1). -Out keeps the pictures in <dir> (default: a temp folder, printed).
  Needs the seedwell CLI: target\debug\seedwell.exe (built with cargo when missing).
#>
param([Parameter(Position = 0, ValueFromRemainingArguments = $true)][string[]]$Names = @('all'), [switch]$Build, [string]$Out)

# The CLI writes to stderr while the app starts; PowerShell 5.1 would make that a terminating error. Checks throw their own.
$ErrorActionPreference = 'Continue'
$App = Split-Path $PSScriptRoot -Parent
$Repo = Split-Path $App -Parent
$Exe = Join-Path $App 'bin\x64\Release\net10.0-windows10.0.19041.0\win-x64\Seedwell.exe'
$Cli = Join-Path $Repo 'target\debug\seedwell.exe'
$Fixtures = Join-Path $Repo 'crates\metainfo\tests\fixtures'
$All = @('render', 'handoff', 'separate', 'gate', 'selection', 'removal', 'info', 'alert', 'trash', 'choice', 'settings', 'quit')

if ($Build) {
  & powershell -NoProfile -File (Join-Path $App 'build.ps1') | Out-Host
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
function New-State {
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
permission = "read_only"
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
  try {
    WaitFor { & $Cli --socket "$dir\state\control.sock" status *> $null; $LASTEXITCODE -eq 0 } 20000 'the core to answer' | Out-Null
  } catch {
    # Never leave an instance behind: one that does not answer is ended here.
    if (-not $p.HasExited) { $p.Kill(); $p.WaitForExit(5000) | Out-Null }
    throw
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

function Stop-App([string]$dir, $p) {
  if ($p -and -not $p.HasExited) {
    Launch $dir @('--quit') | Out-Null
    if (-not $p.WaitForExit(30000)) { $p.Kill(); Fail 'the app did not quit within 30 s' }
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
    if ($p.ExitCode -ne 0) { Fail "the $theme render ended with $($p.ExitCode): $(Get-Content "$dir\error-$theme.txt" -TotalCount 1)" }
    if ($front -gt 0) { Fail "the $theme render held the foreground ($front samples)" }
  }
  $pictures = @(Get-ChildItem $dir -Filter *.png | ForEach-Object FullName)
  foreach ($must in 'menu-dark', 'menu-sync-confirm-light', 'row-menu-dark', 'remove-light', 'library-downloads-dark', 'library-info-light', 'library-removal-dark', 'library-foldersync-light', 'file-icons-dark', 'tray-both-32') {
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
    WaitFor { Log $dir | Where-Object { $_ -like '*second launch: 1 to add*' } } 5000 'the hand-off in app.log' | Out-Null
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

$checks.settings = {
  $dir = New-State
  $p = Start-App $dir -QaControl
  try {
    $cfg = "$dir\state\config.toml"
    (Get-Content $cfg -Raw) -replace 'start_delay_secs = 600', 'start_delay_secs = 599' | Set-Content -NoNewline -Encoding utf8 $cfg
    WaitFor { Log $dir | Where-Object { $_ -like '*settings changed: config.toml*' } } 10000 'the settings change' | Out-Null
  } finally { Stop-App $dir $p; Remove-Item -Recurse -Force $dir }
  @()
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
