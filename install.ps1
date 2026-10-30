<#
Seedwell installer for Windows: downloads a release from GitHub, checks its SHA-256, installs Seedwell for this user
(no administrator rights). The Windows half of install.sh.

  irm https://github.com/kkollsga/seedwell/releases/latest/download/install.ps1 | iex
  & ([scriptblock]::Create((irm <that url>))) -Version 0.1.0     a specific release instead of the latest
  & ([scriptblock]::Create((irm <that url>))) -Cli               also put the seedwell command on your PATH
  & ([scriptblock]::Create((irm <that url>))) -NoLaunch          don't start the app afterwards
  & ([scriptblock]::Create((irm <that url>))) -Uninstall         remove it (asks before deleting your data)
  install.ps1 -Zip Seedwell-X-windows-x64.zip [-Sha256 HEX]      install a zip you already have (a test build)

What it touches, all for this user: %LOCALAPPDATA%\Programs\Seedwell, a Start menu shortcut, the .torrent and magnet:
handlers (Seedwell.exe --register), an Apps & features entry, your PATH with -Cli, and a temporary folder. Running it
again updates in place. Your downloaded files are never touched.

Environment: SEEDWELL_REPO_URL (another repo), SEEDWELL_INSTALL_DIR (another folder), SEEDWELL_DATA_DIR (the data the
uninstall asks about), SEEDWELL_INSTALL_TEST=1 (tests: registry keys under HKCU:\Software\SeedwellInstallTest, the
shortcut in a temporary folder, the app not started unless one it quit is reopened). Only a Seedwell running from the
install folder is quit, through its own instance, and it is reopened with the arguments it had.
#>
param(
    [string]$Version,
    [switch]$Cli,
    [switch]$NoLaunch,
    [switch]$Uninstall,
    [string]$Zip,
    [string]$Sha256
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'   # Windows PowerShell's progress bar makes downloads many times slower
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$AppName = 'Seedwell'
$RepoUrl = if ($env:SEEDWELL_REPO_URL) { $env:SEEDWELL_REPO_URL.TrimEnd('/') } else { 'https://github.com/kkollsga/seedwell' }
$Test = $env:SEEDWELL_INSTALL_TEST -eq '1'
$Dest = if ($env:SEEDWELL_INSTALL_DIR) { $env:SEEDWELL_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\Seedwell' }
$DataDir = if ($env:SEEDWELL_DATA_DIR) { $env:SEEDWELL_DATA_DIR } else { Join-Path $env:LOCALAPPDATA 'Seedwell' }
# Where this user's settings go: the real places, or one test key for all of them.
$Root = if ($Test) { 'HKCU:\Software\SeedwellInstallTest' } else { 'HKCU:\Software\Microsoft\Windows\CurrentVersion' }
$UninstallKey = if ($Test) { "$Root\Uninstall\Seedwell" } else { "$Root\Uninstall\Seedwell" }
$EnvKey = if ($Test) { "$Root\Environment" } else { 'HKCU:\Environment' }
$StartMenu = if ($Test) { Join-Path ([IO.Path]::GetTempPath()) 'SeedwellInstallTest-StartMenu' } else { Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs' }
$Shortcut = Join-Path $StartMenu 'Seedwell.lnk'
$MinBuild = 17763   # Windows 10 1809: the app's TargetPlatformMinVersion (release-zip.ps1 writes it to latest.json)

function Say($text) { Write-Host "==> $text" }
function Die($text) { throw "seedwell-install: $text" }

# A question when someone can answer it; none (a script, CI, a test) is no.
function Ask($question) {
    if ($Test -or -not [Environment]::UserInteractive -or [Console]::IsInputRedirected) { return $false }
    $answer = Read-Host "$question [y/N]"
    return $answer -match '^(y|yes)$'
}

# A proper quit (the trackers are told 'stopped'), only of a Seedwell that runs from the install folder: each one through
# its own instance (the same --state-dir, so a test instance is asked, never another Seedwell). Returns how each was
# started, to reopen it the same way.
function Stop-Running($dir) {
    $running = @(Get-CimInstance Win32_Process -Filter "Name = 'Seedwell.exe'" -ErrorAction SilentlyContinue | Where-Object { $_.ExecutablePath -and $_.ExecutablePath.StartsWith($dir, [StringComparison]::OrdinalIgnoreCase) })
    if ($running.Count -eq 0) { return @() }
    Say "Quitting the running $AppName (trackers are told 'stopped', up to two minutes)"
    $started = @()
    foreach ($r in $running) {
        $state = if ($r.CommandLine -match '--state-dir\s+("([^"]+)"|(\S+))') { if ($Matches[2]) { $Matches[2] } else { $Matches[3] } } else { $null }
        $quitArgs = @(if ($state) { '--state-dir', "`"$state`"" }) + '--quit'
        Start-Process (Join-Path $dir 'Seedwell.exe') -ArgumentList $quitArgs -Wait
        $p = Get-Process -Id $r.ProcessId -ErrorAction SilentlyContinue
        if ($p -and -not $p.WaitForExit(130000)) { Die "$AppName did not quit within 130 s; quit it from its tray menu and run this again" }
        # Its arguments after the program, to start it again as it was.
        $started += ($r.CommandLine -replace '^\s*("[^"]*"|\S+)\s*', '')
    }
    $started
}

# The user's PATH with the CLI folder added or removed (REG_EXPAND_SZ kept).
function Set-CliPath($dir, [bool]$present) {
    if (-not (Test-Path $EnvKey)) { New-Item -Path $EnvKey -Force | Out-Null }
    $key = Get-Item $EnvKey
    $old = $key.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
    $parts = @($old -split ';' | Where-Object { $_ -and $_.TrimEnd('\') -ne $dir.TrimEnd('\') })
    if ($present) { $parts += $dir }
    Set-ItemProperty -Path $EnvKey -Name Path -Value ($parts -join ';') -Type ExpandString
}

function Remove-Install {
    $null = Stop-Running $Dest
    $exe = Join-Path $Dest 'Seedwell.exe'
    # The .torrent and magnet: handlers: the app removes exactly what it registered.
    if (Test-Path $exe) { Start-Process $exe -ArgumentList '--unregister' -Wait }
    Set-CliPath (Join-Path $Dest 'cli') $false
    Remove-Item -Force $Shortcut -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $UninstallKey -ErrorAction SilentlyContinue
    if (Test-Path $Dest) { Remove-Item -Recurse -Force $Dest; Say "Removed $Dest" } else { Say "$AppName is not installed in $Dest" }
}

if ($Uninstall) {
    Remove-Install
    if (Test-Path $DataDir) {
        if (Ask "Also delete your torrent list, resume data and settings ($DataDir)?") { Remove-Item -Recurse -Force $DataDir; Say "Removed $DataDir" }
        else { Say "Kept $DataDir" }
    }
    Say 'Done. Your downloaded files were not touched.'
    return
}

# Requirements: 64-bit Windows 10 1809 or later.
if (-not [Environment]::Is64BitOperatingSystem) { Die "$AppName needs 64-bit Windows" }
if ([Environment]::OSVersion.Version.Build -lt $MinBuild) { Die "$AppName needs Windows 10 version 1809 or later (this PC has $([Environment]::OSVersion.Version))" }

$base = if ($Version) { "$RepoUrl/releases/download/v$($Version.TrimStart('v'))" } else { "$RepoUrl/releases/latest/download" }
$tmp = Join-Path ([IO.Path]::GetTempPath()) "seedwell-install-$([Guid]::NewGuid().ToString('N').Substring(0, 8))"
New-Item -ItemType Directory -Force $tmp | Out-Null
# Out of the install folder first: Windows does not rename a folder that is a process's current directory, and the
# app's own updater starts this from the app (whose shortcut starts it in the install folder).
$here = Get-Location
Set-Location $tmp
try {
    if ($Zip) {
        if (-not (Test-Path $Zip)) { Die "no such file: $Zip" }
        $zipName = Split-Path $Zip -Leaf
        Copy-Item $Zip (Join-Path $tmp $zipName)
        $have = (Get-FileHash (Join-Path $tmp $zipName) -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($Sha256) {
            if ($have -ne $Sha256.ToLowerInvariant()) { Die "checksum mismatch: $zipName is $have, not $Sha256; nothing was installed" }
            Say "Checked the SHA-256 checksum of $zipName"
        } else { Say "Installing $zipName (SHA-256 $have, not checked: no -Sha256 given)" }
        $script = if ($PSCommandPath) { $PSCommandPath } else { $null }
    } else {
        Say "Downloading $AppName$(if ($Version) { " $Version" })"
        try { Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS" -OutFile (Join-Path $tmp 'SHA256SUMS') } catch { Die "could not download $base/SHA256SUMS" }
        $line = Get-Content (Join-Path $tmp 'SHA256SUMS') | Where-Object { $_ -match '^([0-9a-fA-F]{64})\s+\*?(Seedwell-.*-windows-x64\.zip)$' } | Select-Object -First 1
        if (-not $line) { Die 'SHA256SUMS lists no Windows zip (Seedwell-*-windows-x64.zip)' }
        $null = $line -match '^([0-9a-fA-F]{64})\s+\*?(\S+)$'
        $want = $Matches[1].ToLowerInvariant(); $zipName = $Matches[2]
        try { Invoke-WebRequest -UseBasicParsing -Uri "$base/$zipName" -OutFile (Join-Path $tmp $zipName) } catch { Die "could not download $base/$zipName" }
        Say 'Checking the SHA-256 checksum'
        if ((Get-FileHash (Join-Path $tmp $zipName) -Algorithm SHA256).Hash.ToLowerInvariant() -ne $want) { Die 'checksum mismatch: the download is damaged or not the published file; nothing was installed' }
        # The installer itself is kept with the app, for Apps & features' Uninstall.
        try { Invoke-WebRequest -UseBasicParsing -Uri "$base/install.ps1" -OutFile (Join-Path $tmp 'install.ps1'); $script = Join-Path $tmp 'install.ps1' } catch { $script = $null }
    }
    if ($zipName -notmatch '^Seedwell-(.+)-windows-x64\.zip$') { Die "not a Seedwell Windows zip: $zipName" }
    $newVersion = $Matches[1]

    # Unpacked next to the target first, then swapped in, so a failure leaves the installed version as it was.
    $parent = Split-Path $Dest -Parent
    New-Item -ItemType Directory -Force $parent | Out-Null
    $staged = "$Dest.new"
    Remove-Item -Recurse -Force $staged -ErrorAction SilentlyContinue
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [IO.Compression.ZipFile]::ExtractToDirectory((Join-Path $tmp $zipName), $staged)
    if (-not (Test-Path (Join-Path $staged 'Seedwell.exe'))) { Remove-Item -Recurse -Force $staged; Die 'the zip does not contain Seedwell.exe' }
    if ($script) { Copy-Item $script (Join-Path $staged 'install.ps1') }
    # Downloaded files carry the internet mark (SmartScreen asks about each); this copy was checked above.
    Get-ChildItem $staged -Recurse -File | Unblock-File

    $reopen = @(Stop-Running $Dest)
    $old = "$Dest.old"
    Remove-Item -Recurse -Force $old -ErrorAction SilentlyContinue
    if (Test-Path $Dest) {
        $was = (Get-ItemProperty $UninstallKey -ErrorAction SilentlyContinue).DisplayVersion
        Say "Replacing $AppName $(if ($was) { $was } else { '?' })"
        Rename-Item $Dest (Split-Path $old -Leaf)
    }
    try { Rename-Item $staged (Split-Path $Dest -Leaf) }
    catch { if (Test-Path $old) { Rename-Item $old (Split-Path $Dest -Leaf) }; Die "could not put the new version in place ($_); the old one is back" }
    Remove-Item -Recurse -Force $old -ErrorAction SilentlyContinue
    Say "Installed $AppName $newVersion in $Dest"
    $exe = Join-Path $Dest 'Seedwell.exe'

    # Per user: the Start menu shortcut, the file and link handlers (the app's own rule), Apps & features.
    New-Item -ItemType Directory -Force $StartMenu | Out-Null
    $link = (New-Object -ComObject WScript.Shell).CreateShortcut($Shortcut)
    $link.TargetPath = $exe; $link.WorkingDirectory = $Dest; $link.IconLocation = "$exe,0"; $link.Save()
    Start-Process $exe -ArgumentList '--register' -Wait
    if (-not (Test-Path $UninstallKey)) { New-Item -Path $UninstallKey -Force | Out-Null }
    $size = [int]((Get-ChildItem $Dest -Recurse -File | Measure-Object Length -Sum).Sum / 1KB)
    $values = @{
        DisplayName = $AppName; DisplayVersion = $newVersion; Publisher = $AppName; InstallLocation = $Dest
        DisplayIcon = "$exe,0"; URLInfoAbout = $RepoUrl
        UninstallString = "powershell.exe -NoProfile -ExecutionPolicy Bypass -File `"$(Join-Path $Dest 'install.ps1')`" -Uninstall"
    }
    foreach ($k in $values.Keys) { Set-ItemProperty -Path $UninstallKey -Name $k -Value $values[$k] }
    foreach ($k in 'NoModify', 'NoRepair') { Set-ItemProperty -Path $UninstallKey -Name $k -Value 1 -Type DWord }
    Set-ItemProperty -Path $UninstallKey -Name EstimatedSize -Value $size -Type DWord

    if ($Cli) {
        Set-CliPath (Join-Path $Dest 'cli') $true
        Say "Added $(Join-Path $Dest 'cli') to your PATH: open a new terminal to use the seedwell command"
    }

    if ($reopen.Count -gt 0) {
        # The Seedwell this quit (its own updater, or one that was running) opens again as it was started.
        foreach ($a in $reopen) { if ($a) { Start-Process $exe -ArgumentList $a -WorkingDirectory $Dest } else { Start-Process $exe -WorkingDirectory $Dest } }
        Say "Done. $AppName $newVersion is running."
    } elseif (-not $NoLaunch -and -not $Test) { Start-Process $exe -WorkingDirectory $Dest; Say "Done. $AppName is in the notification area." }
    else { Say 'Done.' }
} finally {
    Set-Location $here
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
