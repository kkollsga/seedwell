<#
.SYNOPSIS
  The Windows release: Seedwell-X.Y.Z-windows-x64.zip for install.ps1, the Windows half of app/release-zip.sh.

.DESCRIPTION
  app-windows\release-zip.ps1 [-AllowDirty]

  Builds the release (the Rust core and the seedwell CLI in release mode with SEEDWELL_RELEASE=1, full LTO; the app
  built self-contained for win-x64, Windows App SDK included, so nothing else needs installing), zips it, and
  checks the zip. Everything lands in app-windows\bin\release\ ; nothing is published (release.yml uploads it).

  Seedwell-X.Y.Z-windows-x64.zip   the app at the top (Seedwell.exe and what it needs), the CLI in cli\seedwell.exe
                                   (one folder cannot hold both: Windows file names ignore case)
  install.ps1                      the installer, from the repo root
  SHA256SUMS-windows               the zip's and install.ps1's lines (release.yml appends them to SHA256SUMS)
  latest-windows.json              {"zip", "sha256", "minimum_windows"} (release.yml adds it to latest.json as "windows")

  The checks: the zip's SHA-256, unzipped into a temporary folder, the CLI there says this version, the app there
  starts from that folder and draws a quiet render (no window activated), and install.ps1 parses.
#>
param([switch]$AllowDirty)
$ErrorActionPreference = 'Stop'

$App = $PSScriptRoot
$Repo = Split-Path $App -Parent
$Out = Join-Path $App 'bin\release'

function Step($what) { Write-Host "==> $what" }
function Check($what) { if ($LASTEXITCODE -ne 0) { throw "$what failed (exit $LASTEXITCODE)" } }

if (git -C $Repo status --porcelain) {
    if (-not $AllowDirty) { throw 'Refusing to release a dirty git tree. Commit first, or pass -AllowDirty for a test build.' }
    Write-Host 'WARNING: dirty git tree (-AllowDirty): a test build, not a release.'
}

# One source for the version ([workspace.package] in Cargo.toml) and one for the minimum Windows (the csproj).
$version = $null; $inPackage = $false
foreach ($line in Get-Content (Join-Path $Repo 'Cargo.toml')) {
    if ($line -match '^\[') { $inPackage = $line -eq '[workspace.package]' }
    elseif ($inPackage -and $line -match '^version\s*=\s*"(.+)"') { $version = $Matches[1]; break }
}
if (-not $version) { throw 'No version under [workspace.package] in Cargo.toml' }
$minWindows = ([xml](Get-Content (Join-Path $App 'Seedwell.csproj'))).Project.PropertyGroup.TargetPlatformMinVersion | Where-Object { $_ } | Select-Object -First 1
if (-not $minWindows) { throw 'No TargetPlatformMinVersion in Seedwell.csproj' }
$minWindows = ($minWindows -split '\.')[0..2] -join '.'
$zip = "Seedwell-$version-windows-x64.zip"
Step "Seedwell $version (v$version), commit $(git -C $Repo rev-parse --short HEAD), Windows $minWindows or later"

# Release peer id -SW<ver>- instead of the development one (trs-metainfo PEER_ID_PREFIX), as the Mac's release.
$env:SEEDWELL_RELEASE = '1'

Step 'Core, bindings and app (build.ps1, release)'
& powershell -NoProfile -File (Join-Path $App 'build.ps1') -Configuration Release
Check 'build.ps1'

Step 'seedwell CLI (cargo build --release --bin seedwell)'
Push-Location $Repo
try { cargo build --release --bin seedwell; Check 'cargo build seedwell' } finally { Pop-Location }

Step 'The app into a fresh folder (self-contained, win-x64)'
$stage = Join-Path $Out 'stage'
Remove-Item -Recurse -Force $Out -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $stage | Out-Null
# A build, not dotnet publish: publish leaves out the compiled XAML (*.xbf) and the resource index (Seedwell.pri) of an
# unpackaged WinUI app, and the published app stopped at start (0xC000027B). The project is self-contained, so the
# build output has the .NET runtime and the Windows App SDK in it.
# The trailing \: the XAML compiler joins its file names to the output folder without a separator.
# SeedwellRelease: compiled in as SEEDWELL_RELEASE, the core's release_build (the updater checks by itself).
dotnet build (Join-Path $App 'Seedwell.csproj') -c Release -r win-x64 -o "$stage\" -p:SeedwellRelease=true -nologo -v quiet
Check 'dotnet build'
New-Item -ItemType Directory -Force (Join-Path $stage 'cli') | Out-Null
Copy-Item (Join-Path $Repo 'target\release\seedwell.exe') (Join-Path $stage 'cli\seedwell.exe')
# Not shipped: debug symbols.
Get-ChildItem $stage -Recurse -Filter *.pdb | Remove-Item -Force

Step "Zip ($zip)"
Add-Type -AssemblyName System.IO.Compression.FileSystem
[IO.Compression.ZipFile]::CreateFromDirectory($stage, (Join-Path $Out $zip), [IO.Compression.CompressionLevel]::Optimal, $false)
Copy-Item (Join-Path $Repo 'install.ps1') (Join-Path $Out 'install.ps1')
$zipSha = (Get-FileHash (Join-Path $Out $zip) -Algorithm SHA256).Hash.ToLowerInvariant()
$installSha = (Get-FileHash (Join-Path $Out 'install.ps1') -Algorithm SHA256).Hash.ToLowerInvariant()
# The format of shasum -a 256 (two spaces), so it joins the Mac's SHA256SUMS.
[IO.File]::WriteAllText((Join-Path $Out 'SHA256SUMS-windows'), "$zipSha  $zip`n$installSha  install.ps1`n")
[IO.File]::WriteAllText((Join-Path $Out 'latest-windows.json'), (@{ zip = $zip; sha256 = $zipSha; minimum_windows = $minWindows } | ConvertTo-Json -Compress) + "`n")

Step 'Check the zip (local, no network)'
$check = Join-Path ([IO.Path]::GetTempPath()) "seedwell-release-check-$([Guid]::NewGuid().ToString('N').Substring(0, 8))"
try {
    if ((Get-FileHash (Join-Path $Out $zip) -Algorithm SHA256).Hash.ToLowerInvariant() -ne $zipSha) { throw 'the zip changed after hashing' }
    [IO.Compression.ZipFile]::ExtractToDirectory((Join-Path $Out $zip), "$check\app")
    foreach ($f in 'Seedwell.exe', 'trs_ffi.dll', 'Seedwell.pri', 'App.xbf', 'MainWindow.xbf', 'FlyoutWindow.xbf', 'cli\seedwell.exe') { if (-not (Test-Path "$check\app\$f")) { throw "the zip has no $f" } }
    $said = & "$check\app\cli\seedwell.exe" --version
    if ($said -ne "seedwell $version") { throw "the CLI says '$said', not 'seedwell $version'" }
    # The app starts from the unzipped folder with what the zip holds, and draws (quiet: no window is activated).
    $p = Start-Process "$check\app\Seedwell.exe" -ArgumentList '--render-snapshots', "$check\render", '--theme', 'dark' -PassThru
    if (-not $p.WaitForExit(120000)) { $p.Kill(); throw 'the unzipped app did not finish a render within 120 s' }
    if ($p.ExitCode -ne 0) { throw "the unzipped app's render ended with $($p.ExitCode)" }
    if (-not (Test-Path "$check\render\library-downloads-dark.png")) { throw 'the unzipped app drew nothing' }
    $errors = $null
    [Management.Automation.Language.Parser]::ParseFile((Join-Path $Out 'install.ps1'), [ref]$null, [ref]$errors) | Out-Null
    if ($errors) { throw "install.ps1 does not parse: $($errors[0])" }
    # ASCII only: Invoke-RestMethod in Windows PowerShell 5.1 does not read GitHub's application/octet-stream as UTF-8.
    if ([IO.File]::ReadAllBytes((Join-Path $Out 'install.ps1')) | Where-Object { $_ -gt 127 } | Select-Object -First 1) { throw 'install.ps1 has non-ASCII characters' }
    $null = Get-Content (Join-Path $Out 'latest-windows.json') -Raw | ConvertFrom-Json
} finally {
    Remove-Item -Recurse -Force $check -ErrorAction SilentlyContinue
}
Remove-Item -Recurse -Force $stage

Step "Assets in $Out"
Get-ChildItem $Out | ForEach-Object { '    {0,-38} {1,12:n0}' -f $_.Name, $_.Length }
Write-Host "    $zip sha256 $zipSha"
