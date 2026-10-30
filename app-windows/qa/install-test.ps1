<#
.SYNOPSIS
  Tests install.ps1 end to end against a local web server, never touching this user's real install, registry or data.

.DESCRIPTION
  app-windows\qa\install-test.ps1 [-Release <dir>]

  Serves the release assets from app-windows\bin\release (release-zip.ps1's output, or -Release) the way GitHub does
  (releases/latest/download/ and releases/download/vX.Y.Z/), then runs install.ps1 as a user would, with the test
  seam on (SEEDWELL_INSTALL_TEST=1: registry keys under HKCU:\Software\SeedwellInstallTest, the shortcut in a temp
  folder, the app not started), into a temporary folder:
    1. irm .../latest/download/install.ps1 | iex, with -Cli      2. again (an update in place)
    3. a pinned -Version                                         4. -Uninstall
  and checks after each step what is there, and after the uninstall that nothing is left. It also checks that the
  real places (HKCU:\Software\Classes, the real Uninstall key, the real PATH, the real Start menu) are the same
  before and after. PASS/FAIL per step and exit 1 on any failure.
#>
param([string]$Release)
$ErrorActionPreference = 'Continue'
$App = Split-Path $PSScriptRoot -Parent
if (-not $Release) { $Release = Join-Path $App 'bin\release' }
$zip = Get-ChildItem $Release -Filter 'Seedwell-*-windows-x64.zip' | Select-Object -First 1
if (-not $zip) { Write-Host "No release zip in ${Release}: run app-windows\release-zip.ps1 first"; exit 1 }
$version = $zip.Name -replace '^Seedwell-(.+)-windows-x64\.zip$', '$1'

$root = Join-Path ([IO.Path]::GetTempPath()) "seedwell-install-test-$([Guid]::NewGuid().ToString('N').Substring(0, 8))"
$serve = Join-Path $root 'serve'
foreach ($d in "releases\latest\download", "releases\download\v$version") {
    New-Item -ItemType Directory -Force (Join-Path $serve $d) | Out-Null
    Copy-Item $zip.FullName, (Join-Path $Release 'install.ps1') (Join-Path $serve $d)
    Copy-Item (Join-Path $Release 'SHA256SUMS-windows') (Join-Path $serve "$d\SHA256SUMS")
}
$port = Get-Random -Minimum 20000 -Maximum 40000
$server = Start-Process python -ArgumentList '-I', '-m', 'http.server', $port, '--bind', '127.0.0.1', '--directory', $serve -PassThru -WindowStyle Hidden
$url = "http://127.0.0.1:$port"

$dest = Join-Path $root 'Programs\Seedwell'
$data = Join-Path $root 'data'
New-Item -ItemType Directory -Force $data | Out-Null
'kept' | Set-Content (Join-Path $data 'config.toml')
$testKey = 'HKCU:\Software\SeedwellInstallTest'
$startMenu = Join-Path ([IO.Path]::GetTempPath()) 'SeedwellInstallTest-StartMenu'

# The real places, before.
function Real {
    [ordered]@{
        ProgId = Test-Path 'HKCU:\Software\Classes\Seedwell.torrent'
        Application = Test-Path 'HKCU:\Software\Classes\Applications\Seedwell.exe'
        Uninstall = Test-Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Seedwell'
        Path = (Get-Item 'HKCU:\Environment').GetValue('Path', '', 'DoNotExpandEnvironmentNames')
        Shortcut = Test-Path (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\Seedwell.lnk')
        Magnet = (Get-Item 'HKCU:\Software\Classes\magnet\shell\open\command' -ErrorAction SilentlyContinue).GetValue('')
    } | ConvertTo-Json -Compress
}
$realBefore = Real

# Runs install.ps1 the way a user does, in its own PowerShell 5.1, with the test environment.
function Install([string]$how) {
    $env:SEEDWELL_INSTALL_TEST = '1'; $env:SEEDWELL_REPO_URL = $url; $env:SEEDWELL_INSTALL_DIR = $dest; $env:SEEDWELL_DATA_DIR = $data
    try { powershell -NoProfile -Command $how 2>&1 | ForEach-Object { "      $_" } }
    finally { foreach ($v in 'SEEDWELL_INSTALL_TEST', 'SEEDWELL_REPO_URL', 'SEEDWELL_INSTALL_DIR', 'SEEDWELL_DATA_DIR') { Remove-Item "env:$v" -ErrorAction SilentlyContinue } }
}

function Installed {
    $u = Get-ItemProperty "$testKey\Uninstall\Seedwell" -ErrorAction SilentlyContinue
    $path = (Get-Item "$testKey\Environment" -ErrorAction SilentlyContinue).GetValue('Path', '', 'DoNotExpandEnvironmentNames')
    $problems = @()
    foreach ($f in 'Seedwell.exe', 'trs_ffi.dll', 'cli\seedwell.exe', 'install.ps1') { if (-not (Test-Path (Join-Path $dest $f))) { $problems += "no $f" } }
    if (Test-Path "$dest.new") { $problems += 'a .new folder was left' }
    if (Test-Path "$dest.old") { $problems += 'a .old folder was left' }
    if (-not (Test-Path (Join-Path $startMenu 'Seedwell.lnk'))) { $problems += 'no Start menu shortcut' }
    if (-not $u) { $problems += 'no Apps & features entry' }
    elseif ($u.DisplayName -ne 'Seedwell' -or $u.DisplayVersion -ne $version -or $u.InstallLocation -ne $dest -or $u.UninstallString -notlike '*install.ps1*-Uninstall') { $problems += "Apps & features entry: $($u | Select-Object DisplayName, DisplayVersion, InstallLocation, UninstallString | ConvertTo-Json -Compress)" }
    if (-not (Test-Path "$testKey\Classes\Seedwell.torrent\shell\open\command")) { $problems += 'no .torrent handler (Seedwell.exe --register)' }
    if (($path -split ';') -notcontains (Join-Path $dest 'cli')) { $problems += 'the CLI folder is not on the (test) PATH' }
    $problems
}

$results = @()
function Step([string]$name, [scriptblock]$body) {
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $problems = @(& $body)
    $r = if ($problems.Count -eq 0) { 'PASS' } else { 'FAIL' }
    $script:results += $r
    Write-Host ("{0,-10} {1}  {2,5:n1} s  {3}" -f $name, $r, $clock.Elapsed.TotalSeconds, ($problems -join '; '))
}

try {
    Step 'install' { Install "irm $url/releases/latest/download/install.ps1 | iex; & ([scriptblock]::Create((irm $url/releases/latest/download/install.ps1))) -Cli -NoLaunch" | Out-Host; Installed }
    Step 'update' { Install "& ([scriptblock]::Create((irm $url/releases/latest/download/install.ps1))) -Cli -NoLaunch" | Out-Host; Installed }
    Step 'version' { Install "& ([scriptblock]::Create((irm $url/releases/download/v$version/install.ps1))) -Version $version -Cli -NoLaunch" | Out-Host; Installed }
    Step 'cli' { $said = & (Join-Path $dest 'cli\seedwell.exe') --version; if ($said -ne "seedwell $version") { "the installed CLI says '$said'" } }
    Step 'uninstall' {
        Install "& ([scriptblock]::Create((irm $url/releases/latest/download/install.ps1))) -Uninstall" | Out-Host
        $left = @()
        if (Test-Path $dest) { $left += "$dest" }
        if (Test-Path (Join-Path $startMenu 'Seedwell.lnk')) { $left += 'the shortcut' }
        if (Test-Path "$testKey\Uninstall\Seedwell") { $left += 'the Apps & features entry' }
        foreach ($k in 'Seedwell.torrent', 'Applications\Seedwell.exe') { if (Test-Path "$testKey\Classes\$k") { $left += "Classes\$k" } }
        if ((Get-ItemProperty "$testKey\Classes\.torrent\OpenWithProgids" -ErrorAction SilentlyContinue).'Seedwell.torrent' -ne $null) { $left += '.torrent OpenWithProgids entry' }
        if (Test-Path "$testKey\Classes\magnet") { $left += 'the magnet handler' }
        $path = (Get-Item "$testKey\Environment" -ErrorAction SilentlyContinue).GetValue('Path', '', 'DoNotExpandEnvironmentNames')
        if (($path -split ';') -contains (Join-Path $dest 'cli')) { $left += 'the CLI folder on the (test) PATH' }
        if (-not (Test-Path (Join-Path $data 'config.toml'))) { $left += 'the data folder was deleted without a yes' }
        $left
    }
    Step 'real' { $after = Real; if ($after -ne $realBefore) { "the real places changed: before $realBefore after $after" } }
} finally {
    Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $testKey, $startMenu, $root -ErrorAction SilentlyContinue
}
$failed = @($results | Where-Object { $_ -eq 'FAIL' }).Count
Write-Host ("{0} of {1} passed" -f ($results.Count - $failed), $results.Count)
exit ([int]($failed -gt 0))
