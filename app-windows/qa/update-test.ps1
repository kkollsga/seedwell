<#
.SYNOPSIS
  Tests the in-app update end to end: an installed copy finds a newer release, installs it and reopens as it.

.DESCRIPTION
  app-windows\qa\update-test.ps1 -Old <release dir> -New <release dir>

  -Old and -New are release-zip.ps1 outputs of two versions (the newer one, for a test, built with the version in
  Cargo.toml raised). Serves them as GitHub does (releases/download/vX.Y.Z/ for both, releases/latest/download/ and
  latest.json for the newer), then, with install.ps1's test seam on (registry keys under
  HKCU:\Software\SeedwellInstallTest, never this user's) and everything in a temporary folder:
    1. install.ps1 -Version <old> into the temp folder
    2. the installed app started from there as its shortcut starts it (current directory = the install folder), on a
       temporary state with the network off and --qa-control, SEEDWELL_UPDATE_REPO = the local server
    3. --qa update-check: the model offers the newer version
    4. --qa update-install: the core's installer command quits the app, swaps the folder and reopens it as it was
    5. the reopened app: its version is the newer one and its model says it was updated; Apps & features says so too
  Then it quits the app, uninstalls and removes the test keys. PASS/FAIL per step, exit 1 on any failure.
#>
param([Parameter(Mandatory)][string]$Old, [Parameter(Mandatory)][string]$New)
$ErrorActionPreference = 'Continue'

function Zip($dir) { Get-ChildItem $dir -Filter 'Seedwell-*-windows-x64.zip' | Select-Object -First 1 }
$oldZip = Zip $Old; $newZip = Zip $New
if (-not $oldZip -or -not $newZip) { Write-Host 'Both -Old and -New need a release-zip.ps1 output'; exit 1 }
$oldVersion = $oldZip.Name -replace '^Seedwell-(.+)-windows-x64\.zip$', '$1'
$newVersion = $newZip.Name -replace '^Seedwell-(.+)-windows-x64\.zip$', '$1'

$root = Join-Path ([IO.Path]::GetTempPath()) "seedwell-update-test-$([Guid]::NewGuid().ToString('N').Substring(0, 8))"
$serve = Join-Path $root 'serve'
foreach ($r in @(@($Old, $oldZip, "releases\download\v$oldVersion"), @($New, $newZip, "releases\download\v$newVersion"), @($New, $newZip, 'releases\latest\download'))) {
    $d = Join-Path $serve $r[2]
    New-Item -ItemType Directory -Force $d | Out-Null
    Copy-Item $r[1].FullName, (Join-Path $r[0] 'install.ps1') $d
    Copy-Item (Join-Path $r[0] 'SHA256SUMS-windows') (Join-Path $d 'SHA256SUMS')
}
$port = Get-Random -Minimum 20000 -Maximum 40000
$url = "http://127.0.0.1:$port"
# latest.json as the release has it: the Mac's fields (made up here) and the Windows entry release-zip.ps1 wrote.
$windows = Get-Content (Join-Path $New 'latest-windows.json') -Raw | ConvertFrom-Json
$latest = [ordered]@{ version = $newVersion; tag = "v$newVersion"; notes_url = "$url/releases/tag/v$newVersion"; minimum_macos = '15.0'; zip = "Seedwell-$newVersion.zip"; sha256 = ('0' * 64); windows = $windows }
foreach ($d in 'releases\latest\download', "releases\download\v$newVersion") {
    [IO.File]::WriteAllText((Join-Path $serve "$d\latest.json"), ($latest | ConvertTo-Json -Compress -Depth 4) + "`n")
}
$server = Start-Process python -ArgumentList '-I', '-m', 'http.server', $port, '--bind', '127.0.0.1', '--directory', $serve -PassThru -WindowStyle Hidden

$dest = Join-Path $root 'Programs\Seedwell'
$state = Join-Path $root 'state'
New-Item -ItemType Directory -Force $state | Out-Null
@"
[network]
upnp = false
natpmp = false
dht = false
pex = false
lsd = false
listen_port = 0

[transfers]
start_delay_secs = 600

[folders]
download_dir = '$root\dl'
incomplete_dir = '$root\dl\.incomplete'
torrent_after_add = "leave"
# No watch folder: without one the app watches Downloads, the user's real one (and an isolated run is refused).
watch = []

[agents]
permission = "read_only"
"@ | Set-Content -Encoding utf8 (Join-Path $state 'config.toml')
$testKey = 'HKCU:\Software\SeedwellInstallTest'
# Every process from here on (install.ps1, the app, the installer the app starts) is in test mode.
$env:SEEDWELL_INSTALL_TEST = '1'; $env:SEEDWELL_REPO_URL = $url; $env:SEEDWELL_INSTALL_DIR = $dest
$env:SEEDWELL_DATA_DIR = (Join-Path $root 'data'); $env:SEEDWELL_UPDATE_REPO = $url

function WaitFor([scriptblock]$test, [int]$ms) {
    $clock = [Diagnostics.Stopwatch]::StartNew()
    while ($clock.ElapsedMilliseconds -lt $ms) { $r = & $test; if ($r) { return $r }; Start-Sleep -Milliseconds 250 }
    $null
}
function Log { $f = Join-Path $state 'logs\app.log'; if (Test-Path $f) { @(Get-Content $f -Encoding utf8) } else { @() } }
function Qa([string[]]$words) {
    $before = (Log).Count
    Start-Process (Join-Path $dest 'Seedwell.exe') -ArgumentList (@('--state-dir', "`"$state`"", '--qa') + $words) -Wait
    WaitFor { Log | Select-Object -Skip $before | Where-Object { $_ -like "*qa $($words -join ' '):*" } | Select-Object -Last 1 } 10000
}
function Answers { & (Join-Path $dest 'cli\seedwell.exe') --socket (Join-Path $state 'control.sock') status *> $null; $LASTEXITCODE -eq 0 }
function FromDest { @(Get-CimInstance Win32_Process -Filter "Name = 'Seedwell.exe'" | Where-Object { $_.ExecutablePath -and $_.ExecutablePath.StartsWith($dest, [StringComparison]::OrdinalIgnoreCase) }) }

$results = @()
function Step([string]$name, [scriptblock]$body) {
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $problems = @(& $body)
    $r = if ($problems.Count -eq 0) { 'PASS' } else { 'FAIL' }
    $script:results += $r
    Write-Host ("{0,-10} {1}  {2,5:n1} s  {3}" -f $name, $r, $clock.Elapsed.TotalSeconds, ($problems -join '; '))
}

try {
    Step 'install' {
        powershell -NoProfile -Command "& ([scriptblock]::Create((irm $url/releases/download/v$oldVersion/install.ps1))) -Version $oldVersion -NoLaunch" | Out-Null
        $said = & (Join-Path $dest 'cli\seedwell.exe') --version
        if ($said -ne "seedwell $oldVersion") { "installed CLI says '$said'" }
    }
    Step 'start' {
        # As the Start menu shortcut starts it: the current directory is the install folder.
        Start-Process (Join-Path $dest 'Seedwell.exe') -ArgumentList '--state-dir', "`"$state`"", '--qa-control' -WorkingDirectory $dest | Out-Null
        if (-not (WaitFor { Answers } 20000)) { 'the installed app did not answer' }
    }
    Step 'check' {
        Qa @('update-check') | Out-Null
        $line = WaitFor { Log | Where-Object { $_ -like '*update check:*' } | Select-Object -Last 1 } 20000
        if ($line -notlike "*$newVersion on offer*") { "the check found: $line" }
        $status = Qa @('update-status')
        if ($status -notlike "*on offer $newVersion*") { "the model: $status" }
    }
    Step 'install-new' {
        $old = FromDest | Select-Object -ExpandProperty ProcessId
        Qa @('update-install') | Out-Null
        # The installer quits the app, swaps the folder and reopens it: a new process from the install folder that answers.
        $back = WaitFor { $now = FromDest; $now -and ($now.ProcessId | Where-Object { $old -notcontains $_ }) -and (Answers) } 180000
        if (-not $back) { 'no reopened app (the logs follow)' }
    }
    Step 'updated' {
        $problems = @()
        $said = & (Join-Path $dest 'cli\seedwell.exe') --version
        if ($said -ne "seedwell $newVersion") { $problems += "the installed CLI says '$said'" }
        $status = Qa @('update-status')
        if ($status -notlike "*version $newVersion*") { $problems += "the app: $status" }
        Write-Host "           $status"
        $dv = (Get-ItemProperty "$testKey\Uninstall\Seedwell" -ErrorAction SilentlyContinue).DisplayVersion
        if ($dv -ne $newVersion) { $problems += "Apps & features says $dv" }
        if (Test-Path "$dest.new") { $problems += 'a .new folder was left' }
        if (Test-Path "$dest.old") { $problems += 'a .old folder was left' }
        $problems
    }
} finally {
    foreach ($p in FromDest) { Start-Process (Join-Path $dest 'Seedwell.exe') -ArgumentList '--state-dir', "`"$state`"", '--quit' -Wait; Get-Process -Id $p.ProcessId -ErrorAction SilentlyContinue | ForEach-Object { $_.WaitForExit(30000) | Out-Null } }
    Write-Host '--- app.log (update lines)'
    Log | Where-Object { $_ -like '*update*' } | Select-Object -Last 12 | ForEach-Object { "    $_" }
    # The installer's log: where the app said it would be (the core's log_path), else the state's logs folder.
    $installLog = Log | ForEach-Object { if ($_ -match '; log (.+)\)$') { $Matches[1] } } | Select-Object -Last 1
    if (-not $installLog) { $installLog = Join-Path $state 'logs\update.log' }
    Write-Host "--- $installLog"
    Get-Content $installLog -ErrorAction SilentlyContinue | Select-Object -Last 20 | ForEach-Object { "    $_" }
    powershell -NoProfile -Command "& ([scriptblock]::Create((irm $url/releases/latest/download/install.ps1))) -Uninstall" | Out-Null
    Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue
    foreach ($v in 'SEEDWELL_INSTALL_TEST', 'SEEDWELL_REPO_URL', 'SEEDWELL_INSTALL_DIR', 'SEEDWELL_DATA_DIR', 'SEEDWELL_UPDATE_REPO') { Remove-Item "env:$v" -ErrorAction SilentlyContinue }
    Remove-Item -Recurse -Force $testKey, (Join-Path ([IO.Path]::GetTempPath()) 'SeedwellInstallTest-StartMenu'), $root -ErrorAction SilentlyContinue
}
$failed = @($results | Where-Object { $_ -eq 'FAIL' }).Count
Write-Host ("{0} of {1} passed" -f ($results.Count - $failed), $results.Count)
exit ([int]($failed -gt 0))
