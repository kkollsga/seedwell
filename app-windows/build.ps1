# Builds Seedwell for Windows from a clean clone, in one command:
#   1. the Rust core as a DLL (trs-ffi as a cdylib, release; debug with -CoreProfile Debug),
#   2. its C# bindings (uniffi-bindgen-cs, library mode),
#   3. the .ico from Assets/icon/*.png,
#   4. the WinUI 3 app (unpackaged, self-contained, x64).
# Generated/ and bin/ obj/ are build output (not committed), like the Mac app's app/Generated.
#
#   powershell -ExecutionPolicy Bypass -File app-windows\build.ps1 [-Configuration Debug|Release] [-CoreProfile Debug|Release]
#
# -CoreProfile Debug builds the core in cargo's dev profile, from what a `cargo test`/`nextest` build of the
# workspace already compiled (CI's app and QA job, as the Mac's app step reuses the test build); a release is always
# built with the release core (release-zip.ps1).
#
# Needs: Rust (rustup), the .NET 10 SDK, and
#   cargo install uniffi-bindgen-cs --git https://github.com/NordSecurity/uniffi-bindgen-cs --tag v0.11.1+v0.31.0
param(
    [ValidateSet('Debug', 'Release')] [string]$Configuration = 'Release',
    [ValidateSet('Debug', 'Release')] [string]$CoreProfile = 'Release'
)
$ErrorActionPreference = 'Stop'

$app = $PSScriptRoot
$repo = Split-Path $app -Parent
$gen = Join-Path $app 'Generated'
New-Item -ItemType Directory -Force $gen | Out-Null

function Step($what) { Write-Host "== $what" }
function Check($what) { if ($LASTEXITCODE -ne 0) { throw "$what failed (exit $LASTEXITCODE)" } }

$coreDir = if ($CoreProfile -eq 'Release') { 'release' } else { 'debug' }
Step "core DLL (cargo rustc -p trs-ffi --crate-type cdylib, $coreDir)"
Push-Location $repo
try {
    $profileArgs = if ($CoreProfile -eq 'Release') { @('--release') } else { @() }
    cargo rustc -p trs-ffi --lib @profileArgs --crate-type cdylib
    Check 'cargo rustc'
} finally { Pop-Location }
$dll = Join-Path $repo "target\$coreDir\trs_ffi.dll"
Copy-Item $dll (Join-Path $gen 'trs_ffi.dll') -Force

Step 'C# bindings (uniffi-bindgen-cs --library)'
if (-not (Get-Command uniffi-bindgen-cs -ErrorAction SilentlyContinue)) {
    throw 'uniffi-bindgen-cs is missing: cargo install uniffi-bindgen-cs --git https://github.com/NordSecurity/uniffi-bindgen-cs --tag v0.11.1+v0.31.0'
}
uniffi-bindgen-cs --library $dll --out-dir $gen
Check 'uniffi-bindgen-cs'

Step 'icon (Assets/icon/*.png -> Generated/Seedwell.ico)'
# An .ico whose entries are the PNGs as they are (Windows reads PNG entries since Vista), largest last.
$pngs = Get-ChildItem (Join-Path $app 'Assets\icon\icon_*.png') |
    Sort-Object { [int]($_.BaseName -replace '\D', '') } |
    Where-Object { [int]($_.BaseName -replace '\D', '') -le 256 }
$data = @($pngs | ForEach-Object { , [IO.File]::ReadAllBytes($_.FullName) })
$ms = New-Object IO.MemoryStream
$w = New-Object IO.BinaryWriter($ms)
$w.Write([UInt16]0); $w.Write([UInt16]1); $w.Write([UInt16]$data.Count)
$offset = 6 + 16 * $data.Count
for ($i = 0; $i -lt $data.Count; $i++) {
    $b = $data[$i]
    # Width and height from the PNG's IHDR (big-endian at bytes 16 and 20); 256 is written as 0.
    $px = ($b[16] -shl 24) -bor ($b[17] -shl 16) -bor ($b[18] -shl 8) -bor $b[19]
    $py = ($b[20] -shl 24) -bor ($b[21] -shl 16) -bor ($b[22] -shl 8) -bor $b[23]
    $w.Write([byte]($px % 256)); $w.Write([byte]($py % 256)); $w.Write([byte]0); $w.Write([byte]0)
    $w.Write([UInt16]1); $w.Write([UInt16]32); $w.Write([UInt32]$b.Length); $w.Write([UInt32]$offset)
    $offset += $b.Length
}
foreach ($b in $data) { $w.Write($b) }
$w.Flush()
[IO.File]::WriteAllBytes((Join-Path $gen 'Seedwell.ico'), $ms.ToArray())

Step "app (dotnet build, $Configuration, win-x64)"
dotnet build (Join-Path $app 'Seedwell.csproj') -c $Configuration -r win-x64 -p:Platform=x64
Check 'dotnet build'
$exe = Get-ChildItem (Join-Path $app "bin\x64\$Configuration") -Recurse -Filter Seedwell.exe | Select-Object -First 1
Write-Host "== done: $($exe.FullName)"
