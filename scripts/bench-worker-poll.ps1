param(
    [Parameter(Mandatory)][string]$Label,
    [string]$OutputDirectory = (Join-Path $PSScriptRoot '../src-tauri/target/worker-poll-bench/results')
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path $PSScriptRoot
$env:GALE_SYNC_URL = 'http://127.0.0.1:1'
New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
$buildOutput = & cargo test --manifest-path (Join-Path $repo 'src-tauri/Cargo.toml') --release --locked --features worker --lib --no-run --message-format=json 2>&1
$buildOutput | Set-Content -LiteralPath (Join-Path $OutputDirectory "$Label-build.log")
if ($LASTEXITCODE -ne 0) { throw "Benchmark build failed; see $Label-build.log" }
$artifacts = @($buildOutput | ForEach-Object {
    if ($_.ToString().StartsWith('{')) { $_.ToString() | ConvertFrom-Json }
} | Where-Object { $_.reason -eq 'compiler-artifact' -and $_.profile.test -and $_.executable })
if ($artifacts.Count -ne 1) { throw "Expected one test binary, got $($artifacts.Count)" }
$binary = $artifacts[0].executable
$results = @()
for ($run = 1; $run -le 5; $run++) {
    $lines = & $binary 'poll_benchmark' --ignored --nocapture --test-threads=1 2>&1
    $exitCode = $LASTEXITCODE
    $lines | Set-Content -LiteralPath (Join-Path $OutputDirectory "$Label-$run.log")
    if ($exitCode -ne 0) { throw "Benchmark failed; see $Label-$run.log" }
    $samples = @($lines | ForEach-Object {
        if ($_.ToString() -match 'GALE_BENCH (\{.*\})') {
            $sample = $Matches[1] | ConvertFrom-Json
            $sample | Add-Member -NotePropertyName variant -NotePropertyValue $Label
            $sample | Add-Member -NotePropertyName run -NotePropertyValue $run
            $sample
        }
    })
    if ($samples.Count -ne 6) { throw "Expected six benchmark cases, got $($samples.Count)" }
    $results += $samples
    Write-Output "$Label run $run/5 complete"
}
$results | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $OutputDirectory "$Label.json")
$summary = @($results | Group-Object case | ForEach-Object {
    $row = [ordered]@{ variant = $Label; case = $_.Name; runs = $_.Count }
    foreach ($property in $_.Group[0].PSObject.Properties) {
        if ($property.Value -is [ValueType] -and $property.Value -isnot [bool] -and $property.Name -ne 'run') {
            $stats = $_.Group | Measure-Object -Property $property.Name -Average -Minimum -Maximum
            $row["mean_$($property.Name)"] = $stats.Average
            if ($property.Name -in @('ms','virtual_ms')) {
                $row["min_$($property.Name)"] = $stats.Minimum
                $row["max_$($property.Name)"] = $stats.Maximum
            }
        }
    }
    if ($_.Group[0].PSObject.Properties.Name -contains 'completed') {
        $row['completed_runs'] = @($_.Group | Where-Object completed).Count
    }
    [pscustomobject]$row
})
$summary | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $OutputDirectory "$Label-summary.json")
$summary | Select-Object case, runs, mean_ms, mean_virtual_ms, mean_ftp_commands, mean_archive_requests, mean_journal_rewrites | Format-Table -AutoSize
