<#
.SYNOPSIS
  The Rust tests as CI's "Windows tests" job runs them, with a time limit and a summary to paste into a chat message.

.DESCRIPTION
  app-windows\qa\rust-tests.ps1 [-Minutes 90] [-Log <file>] [<more cargo nextest arguments>]
  From bash: powershell.exe -NoProfile -ExecutionPolicy Bypass -File app-windows/qa/rust-tests.ps1

  Runs `cargo nextest run --workspace --locked --profile ci` in the repository with CI's environment (the windows job
  of .github/workflows/ci.yml: TRS_FORBID_REAL_HOME=1, CARGO_INCREMENTAL=0; only the colours are off, for the log), so
  a test that would fail on Windows CI fails here first. Keep the command the same as CI's when ci.yml changes.
  Run it on every bundle from the Mac, beside qa\run.ps1 all (.claude/skills/windows-bridge).

  The whole output goes to a log file (default: a temp file, printed) and to the screen. A run that takes longer than
  -Minutes (CI's limit, 90) is stopped with every process it started. At the end a short summary for the
  "Seedwell Development" message: the source (bridge ref, commit, local changes), PASS or FAIL with the time,
  nextest's summary line and each failing, flaky or timed-out test.
  Extra arguments go to cargo nextest (e.g. -E 'test(/ondemand/)'); the summary then says the run was narrowed.
  Exit code: nextest's (0: every test passed), 1 when the time limit stopped it.
  Needs cargo-nextest: cargo install cargo-nextest --locked
#>
param(
  [int]$Minutes = 90,
  [string]$Log,
  [Parameter(ValueFromRemainingArguments = $true)][string[]]$Extra = @()
)

$ErrorActionPreference = 'Continue'
# Below normal, for this and all it starts: QA runs beside it (AGENTS.md: no waiting for a quiet machine), and the suite's
# many processes must not starve the UI thread of the app under test, whose stall of fail_ms fails that check.
(Get-Process -Id $PID).PriorityClass = 'BelowNormal'
# What it shows is UTF-8 (nextest's progress, test names): Windows PowerShell 5.1 writes the OEM code page otherwise.
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$Repo = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
Set-Location $Repo

& cargo nextest --version *> $null
if ($LASTEXITCODE -ne 0) { Write-Host 'cargo-nextest is missing: cargo install cargo-nextest --locked'; exit 1 }

# What is being tested: the newest bundle's ref in HEAD, the commit, and what the PC added (its scratch commits for
# patches, changes not committed).
$head = (& git rev-parse --short HEAD).Trim()
$ref = @(& git for-each-ref --merged HEAD --sort=-creatordate --count=1 --format='%(refname:short)' refs/bridge) | Select-Object -First 1
$source = "HEAD $head"
if ($ref) {
  $ahead = [int](& git rev-list --count "$ref..HEAD")
  $source = if ($ahead) { "$ref + $ahead commit(s) of the PC (HEAD $head)" } else { "$ref = $head (the bundle as it is)" }
}
$changed = @(& git status --porcelain --untracked-files=no)
if ($changed.Count) { $source += " + $($changed.Count) changed file(s) not committed" }

$stamp = [DateTime]::Now.ToString('yyyyMMdd-HHmmss')
if (-not $Log) { $Log = Join-Path ([IO.Path]::GetTempPath()) "seedwell-rust-tests-$stamp.log" }
$OutFile = "$Log.stdout"

# CI's environment (ci.yml: env), for this process and cargo's.
$env:TRS_FORBID_REAL_HOME = '1'
$env:CARGO_INCREMENTAL = '0'
$env:CARGO_TERM_COLOR = 'never'
# GitHub's Windows runner sets no HOME, but Git Bash does: a run started from it would hide what fails there.
Remove-Item Env:HOME -ErrorAction SilentlyContinue

$cargoArgs = @('nextest', 'run', '--workspace', '--locked', '--profile', 'ci') + $Extra
$quoted = $cargoArgs | ForEach-Object { if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ } }
Write-Host "rust-tests: cargo $($cargoArgs -join ' ')  (limit $Minutes min; log $Log)"

$clock = [Diagnostics.Stopwatch]::StartNew()
$p = Start-Process cargo -ArgumentList $quoted -NoNewWindow -PassThru -RedirectStandardError $Log -RedirectStandardOutput $OutFile
$null = $p.Handle  # kept, so ExitCode is there after the process ends (Windows PowerShell 5.1)

# Shows what the run writes as it comes (nextest writes its progress to stderr).
$shown = @{ $Log = 0L; $OutFile = 0L }
function Show-New {
  foreach ($path in @($Log, $OutFile)) {
    if (-not (Test-Path $path)) { continue }
    $fs = [IO.File]::Open($path, 'Open', 'Read', 'ReadWrite')
    try {
      if ($fs.Length -gt $shown[$path]) {
        $fs.Seek($shown[$path], 'Begin') | Out-Null
        $buffer = New-Object byte[] ($fs.Length - $shown[$path])
        $read = $fs.Read($buffer, 0, $buffer.Length)
        $shown[$path] += $read
        Write-Host -NoNewline ([Text.Encoding]::UTF8.GetString($buffer, 0, $read))
      }
    } finally { $fs.Dispose() }
  }
}

$timedOut = $false
while (-not $p.WaitForExit(5000)) {
  Show-New
  if ($clock.Elapsed.TotalMinutes -ge $Minutes) {
    $timedOut = $true
    & taskkill /PID $p.Id /T /F *> $null
    $p.WaitForExit(30000) | Out-Null
    break
  }
}
Show-New
$code = if ($timedOut) { 1 } else { $p.ExitCode }
$minutes = [math]::Round($clock.Elapsed.TotalMinutes, 1)

# The summary: nextest's last Summary line and the tests it names at the end (FAIL, FLAKY, TIMEOUT, SIGSEGV and the like).
$lines = @(Get-Content $Log -Encoding UTF8 -ErrorAction SilentlyContinue) + @(Get-Content $OutFile -Encoding UTF8 -ErrorAction SilentlyContinue)
$summary = $lines | Where-Object { $_ -match '^\s*Summary \[' } | Select-Object -Last 1
$named = @($lines | Where-Object { $_ -match '^\s+(TRY \d+ )?(FAIL|FLAKY|TIMEOUT|SIG[A-Z]+|ABORT|LEAK-FAIL)\b' } | ForEach-Object { $_.Trim() } | Select-Object -Unique | Select-Object -First 25)
$buildErrors = @($lines | Where-Object { $_ -match '^error(\[E\d+\])?:' } | Select-Object -First 5)

$result = if ($timedOut) { "STOPPED after $Minutes min (the time limit)" } elseif ($code -eq 0) { 'PASS' } else { 'FAIL' }
Write-Host ''
Write-Host '----- for the message -----'
Write-Host "Rust tests on Windows (as CI's Windows tests job): $result in $minutes min"
Write-Host "Source: $source"
if ($Extra.Count) { Write-Host "Narrowed: $($Extra -join ' ') (not the whole CI run)" }
if ($summary) { Write-Host $summary.Trim() }
elseif ($buildErrors.Count) { Write-Host 'The build failed:'; $buildErrors | ForEach-Object { Write-Host "  $_" } }
$named | ForEach-Object { Write-Host "  $_" }
Write-Host "Log: $Log"
if (Test-Path $OutFile) { Get-Content $OutFile -Encoding UTF8 | Add-Content $Log -Encoding UTF8; Remove-Item $OutFile }
exit $code
