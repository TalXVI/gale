param(
    [Parameter(Mandatory)][ValidatePattern('^[a-zA-Z0-9_-]+$')][string]$Label,
    [ValidateRange(1, 100)][int]$Runs = 3,
    [ValidateRange(0, 1000)][int[]]$CommandDelayMs = @(0, 10),
    [ValidateRange(8, 1000)][int]$Mods = 32,
    [ValidateSet('ftp', 'ftps')][string]$Protocol = 'ftps',
    [ValidateSet('Standard', 'MirrorRoot')][string]$Layout = 'Standard',
    [string]$Binary,
    [string]$OutputDirectory = (Join-Path $PSScriptRoot '../src-tauri/target/server-sync-bench/results')
)

$ErrorActionPreference = 'Stop'

# Windows PowerShell turns redirected native stderr into errors, which 'Stop'
# makes fatal. Callers check $LASTEXITCODE instead.
function Invoke-Native([scriptblock]$Command) {
    $ErrorActionPreference = 'Continue'
    & $Command 2>&1
}
$repo = Split-Path $PSScriptRoot
$environmentNames = @('GALE_SYNC_URL', 'GALE_BENCH_MODS', 'GALE_BENCH_PROTOCOL', 'GALE_BENCH_LAYOUT', 'GALE_BENCH_COMMAND_DELAY_MS')
$previousEnvironment = @{}
foreach ($name in $environmentNames) {
    $previousEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
try {
    $env:GALE_SYNC_URL = 'http://127.0.0.1:1'
    $env:GALE_BENCH_MODS = "$Mods"
    $env:GALE_BENCH_PROTOCOL = $Protocol
    $env:GALE_BENCH_LAYOUT = $Layout
    New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
    $OutputDirectory = (Resolve-Path -LiteralPath $OutputDirectory).Path
    $revision = $null
    if ($Binary) {
        $executable = (Resolve-Path -LiteralPath $Binary).Path
    } else {
        $buildOutput = Invoke-Native { cargo test --manifest-path (Join-Path $repo 'src-tauri/Cargo.toml') --release --locked --features worker --lib --no-run --message-format=json }
        $buildOutput | Set-Content -LiteralPath (Join-Path $OutputDirectory "$Label-build.log")
        if ($LASTEXITCODE -ne 0) { throw "Benchmark build failed; see $Label-build.log" }
        $artifacts = @($buildOutput | ForEach-Object {
            if ($_.ToString().StartsWith('{')) { $_.ToString() | ConvertFrom-Json }
        } | Where-Object { $_.reason -eq 'compiler-artifact' -and $_.profile.test -and $_.executable })
        if ($artifacts.Count -ne 1) { throw "Expected one test binary, got $($artifacts.Count)" }
        $executable = $artifacts[0].executable
        $revision = & git -C $repo rev-parse HEAD
        & git -C $repo diff --binary HEAD | Set-Content -LiteralPath (Join-Path $OutputDirectory "$Label-changes.patch")
    }
    $revision | Set-Content -LiteralPath (Join-Path $OutputDirectory "$Label-revision.txt")
    $savedBinary = Join-Path $OutputDirectory "$Label-bench.exe"
    Copy-Item -LiteralPath $executable -Destination $savedBinary
    if ([Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT) {
        Copy-Item -LiteralPath "$executable.manifest" -Destination "$savedBinary.manifest"
    }
    # Invalidate Windows' cached activation context when reusing a label.
    [IO.File]::SetLastWriteTimeUtc($savedBinary, [DateTime]::UtcNow)
    $binaryHash = (Get-FileHash -LiteralPath $savedBinary -Algorithm SHA256).Hash
    [ordered]@{
        label = $Label; revision = $revision; binary_sha256 = $binaryHash
        source = $(if ($Binary) { 'prebuilt' } else { 'working_tree' })
        protocol = $Protocol; layout = $Layout; mods = $Mods
        runs = $Runs; command_delay_ms = $CommandDelayMs
    } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $OutputDirectory "$Label-metadata.json")
    $results = @()
    foreach ($delay in $CommandDelayMs) {
        $env:GALE_BENCH_COMMAND_DELAY_MS = "$delay"
        for ($run = 1; $run -le $Runs; $run++) {
            $log = "$Label-$Protocol-$($delay)ms-$run.log"
            Set-Content -LiteralPath (Join-Path $OutputDirectory $log) -Value ''
            $lines = Invoke-Native { & $savedBinary 'server_sync_benchmark' --ignored --nocapture --test-threads=1 } |
                Tee-Object -FilePath (Join-Path $OutputDirectory $log)
            $exitCode = $LASTEXITCODE
            if ($exitCode -ne 0) { throw "Benchmark exited with code $exitCode; see $log" }
            $samples = @($lines | ForEach-Object {
                if ($_.ToString() -match 'GALE_DEPLOY_BENCH (\{.*\})') {
                    $sample = $Matches[1] | ConvertFrom-Json
                    $sample | Add-Member -NotePropertyName label -NotePropertyValue $Label
                    $sample | Add-Member -NotePropertyName revision -NotePropertyValue $revision
                    $sample | Add-Member -NotePropertyName binary_sha256 -NotePropertyValue $binaryHash
                    $sample | Add-Member -NotePropertyName mods -NotePropertyValue $Mods
                    $sample | Add-Member -NotePropertyName run -NotePropertyValue $run
                    $sample
                }
            })
            if ($samples.Count -ne 11) { throw "Expected eleven completed cases, got $($samples.Count) in $log" }
            if (@($samples.case | Sort-Object -Unique).Count -ne 11) { throw "Duplicate benchmark cases in $log" }
            $results += $samples
            $results | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $OutputDirectory "$Label.json")
            Write-Output "$Label $Protocol $($delay)ms run $run/$Runs complete"
        }
    }
    $summary = @($results | Group-Object command_delay_ms, case | ForEach-Object {
        $samples = @($_.Group | Sort-Object ms)
        $stats = $samples | Measure-Object ms -Average -Minimum -Maximum
        [pscustomobject]@{
            case = $samples[0].case
            command_delay_ms = $samples[0].command_delay_ms
            runs = $samples.Count
            mean_ms = $stats.Average
            median_ms = ($samples[[math]::Floor(($samples.Count - 1) / 2)].ms + $samples[[math]::Floor($samples.Count / 2)].ms) / 2
            min_ms = $stats.Minimum
            max_ms = $stats.Maximum
            mean_commands = ($samples | Measure-Object ftp_commands -Average).Average
            mean_download_bytes = ($samples | Measure-Object download_bytes -Average).Average
            max_connections = ($samples | Measure-Object peak_connections -Maximum).Maximum
        }
    })
    $summary | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $OutputDirectory "$Label-summary.json")
    $summary | Format-Table -AutoSize
} finally {
    foreach ($name in $environmentNames) {
        [Environment]::SetEnvironmentVariable($name, $previousEnvironment[$name], 'Process')
    }
}
