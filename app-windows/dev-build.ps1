<#
.SYNOPSIS
  A development build of the Windows app (SPEC 12A), the Windows half of tools/dev-build.sh.

.DESCRIPTION
  app-windows\dev-build.ps1 [-To <folder>]

  The release build of this tree (release-zip.ps1 -DevVersion: release peer id, full LTO, the same checks), numbered
  X.Y.Z.N: Cargo.toml's version and the next development number, counted in app-windows\bin\dev\counter from 1 for
  each version (only once a build succeeded). The zip, Seedwell-X.Y.Z.N-windows-x64.zip, lands in app-windows\bin\dev\
  and the script prints its path and SHA-256.

  -To <folder>   also put the zip in a folder Seedwell devices follow (seedwell update watch <folder>; the shared
                 builds folder): copied as .<zip>.part, then renamed, so a half-copied zip is never seen; older Windows
                 zips there are removed.

  A device installs a build from a folder only when the folder's build.json, signed with the build key, names it and
  its SHA-256. The key is on the Mac, so this script never writes build.json (nor latest.json): post the line it
  prints ("Seedwell-X.Y.Z.N-windows-x64.zip sha256 HEX") in the agent channel, and the Mac's agent signs it into the
  folder's build.json:
      tools/sysguard/heavy.sh tools/build-sign.sh add <folder> Seedwell-X.Y.Z.N-windows-x64.zip --sha256 HEX
  Installing it by hand on this PC needs "Allow development builds" on (seedwell update dev-builds on):
      seedwell update install app-windows\bin\dev\Seedwell-X.Y.Z.N-windows-x64.zip --sha256 HEX
  Uncommitted changes are allowed.
#>
param([string]$To)
$ErrorActionPreference = 'Stop'

$App = $PSScriptRoot
$Repo = Split-Path $App -Parent
$Out = Join-Path $App 'bin\dev'

if ($To) {
    if (-not (Test-Path $To -PathType Container)) { throw "no such folder: $To" }
    $To = (Resolve-Path $To).Path
}

$version = $null; $inPackage = $false
foreach ($line in Get-Content (Join-Path $Repo 'Cargo.toml')) {
    if ($line -match '^\[') { $inPackage = $line -eq '[workspace.package]' }
    elseif ($inPackage -and $line -match '^version\s*=\s*"(.+)"') { $version = $Matches[1]; break }
}
if (-not $version) { throw 'No version under [workspace.package] in Cargo.toml' }

New-Item -ItemType Directory -Force $Out | Out-Null
$counter = Join-Path $Out 'counter'
$n = 0
if (Test-Path $counter) {
    $parts = (Get-Content $counter -TotalCount 1) -split '\s+'
    if ($parts.Count -ge 2 -and $parts[0] -eq $version -and $parts[1] -match '^\d+$') { $n = [int]$parts[1] }
}
$n++
$dev = "$version.$n"
$commit = git -C $Repo rev-parse --short HEAD
if (git -C $Repo status --porcelain) { $commit = "$commit-dirty" }
Write-Host "==> Seedwell $dev (development build of $commit)"

& powershell -NoProfile -ExecutionPolicy Bypass -File (Join-Path $App 'release-zip.ps1') -DevVersion $dev
if ($LASTEXITCODE -ne 0) { throw "release-zip.ps1 failed (exit $LASTEXITCODE)" }
# Counted only once it is built: a failed build does not use up a number.
[IO.File]::WriteAllText($counter, "$version $n`n")

$zip = "Seedwell-$dev-windows-x64.zip"
Get-ChildItem $Out -Filter 'Seedwell-*-windows-x64.zip' | Remove-Item -Force
Move-Item (Join-Path $App "bin\release\$zip") (Join-Path $Out $zip)
$sha = (Get-FileHash (Join-Path $Out $zip) -Algorithm SHA256).Hash.ToLowerInvariant()

if ($To) {
    $part = Join-Path $To ".$zip.part"
    Copy-Item (Join-Path $Out $zip) $part -Force
    Move-Item $part (Join-Path $To $zip) -Force
    # Older Windows builds go: the newest is the one build.json will name.
    Get-ChildItem $To -Filter 'Seedwell-*-windows-x64.zip' | Where-Object { $_.Name -ne $zip } | Remove-Item -Force
    Write-Host "==> In $To (unsigned: devices install it once the Mac's agent signs it into build.json)"
}
Write-Host (Join-Path $Out $zip)
Write-Host "sha256 $sha"
Write-Host 'Post in the agent channel, for the Mac to sign:'
Write-Host "    $zip sha256 $sha"
Write-Host "Mac: tools/sysguard/heavy.sh tools/build-sign.sh add <folder> $zip --sha256 $sha"
