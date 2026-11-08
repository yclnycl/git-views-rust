#requires -Version 7.0
<#
.SYNOPSIS
Measure a built GitViews desktop executable against the service baseline's synthetic fixtures.
.EXAMPLE
pwsh -File scripts/app-performance-baseline.ps1 -Executable dist/GitViews.exe
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Executable,
    [string]$ServiceBaseline = 'dist/performance-baseline/current.json',
    [string]$OutputPath = 'dist/performance-baseline/app-current.json',
    [ValidateRange(3, 30)][int]$Iterations = 3,
    [ValidateRange(2, 30)][int]$IdleSeconds = 2,
    [ValidateRange(5, 120)][int]$ReadyTimeoutSeconds = 45
)

$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'This desktop-process sampler requires Windows.' }
$projectRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$projectPrefix = $projectRoot.TrimEnd('\') + '\'
function Full-Path([string]$Path) {
    if ([IO.Path]::IsPathRooted($Path)) { return [IO.Path]::GetFullPath($Path) }
    [IO.Path]::GetFullPath((Join-Path $projectRoot $Path))
}
function Assert-ProjectPath([string]$Path) {
    if (-not $Path.StartsWith($projectPrefix, [StringComparison]::OrdinalIgnoreCase)) { throw "Expected a path inside this project: $Path" }
    $probe = $Path
    while ($probe -and -not $probe.Equals($projectRoot, [StringComparison]::OrdinalIgnoreCase)) {
        if ((Test-Path -LiteralPath $probe) -and ((Get-Item -LiteralPath $probe -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw "Path must not traverse a symbolic link or junction: $probe" }
        $probe = Split-Path -Parent $probe
    }
}
function Summarize([double[]]$Values) {
    $sorted = @($Values | Sort-Object)
    $count = $sorted.Count
    [pscustomobject]@{
        median = if ($count % 2 -eq 0) { ($sorted[$count / 2 - 1] + $sorted[$count / 2]) / 2 } else { $sorted[[int][Math]::Floor($count / 2)] }
        p95 = $sorted[[int][Math]::Ceiling($count * 0.95) - 1]
        min = $sorted[0]; max = $sorted[-1]
    }
}
$binary = Full-Path $Executable
$output = Full-Path $OutputPath
Assert-ProjectPath $output
if ((Test-Path -LiteralPath $output) -and (Get-Item -LiteralPath $output).PSIsContainer) { throw 'OutputPath must be an ordinary file.' }
if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) { throw "Executable not found: $binary" }
$servicePath = Full-Path $ServiceBaseline
$baseline = Get-Content -LiteralPath $servicePath -Raw | ConvertFrom-Json -Depth 64
$fixtureRoot = Full-Path $baseline.fixture_directory
Assert-ProjectPath $fixtureRoot
$runId = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssZ') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 8)
$scratch = Join-Path $projectRoot "target/performance-baseline/app-$runId"
New-Item -ItemType Directory -Path $scratch -Force | Out-Null
$results = @()
foreach ($fixture in $baseline.fixtures) {
    $fixtureName = [string]$fixture.fixture.name
    if ($fixtureName -notin @('small', 'medium', 'large')) { throw 'Only generated baseline fixtures are supported.' }
    $repository = Join-Path $fixtureRoot $fixtureName
    if (-not (Test-Path -LiteralPath (Join-Path $repository '.git') -PathType Container)) { throw "Synthetic fixture is missing: $fixtureName" }
    $samples = @()
    for ($iteration = 1; $iteration -le $Iterations; $iteration++) {
        Write-Host "Desktop $fixtureName ($iteration/$Iterations)..."
        $session = Join-Path $scratch "$fixtureName-$iteration"
        New-Item -ItemType Directory -Path $session -Force | Out-Null
        $config = Join-Path $session 'config.toml'
        $state = Join-Path $session 'repositories.json'
        $logDirectory = Join-Path $session 'logs'
        # Fresh, isolated application preferences/state; intentionally choose
        # the built-in engine so onboarding does not gate repository loading.
        "version = 1`n[logging]`nlevel = `"info`"`n" | Set-Content -LiteralPath $config -Encoding utf8NoBOM
        "engine = `"built_in`"`n" | Set-Content -LiteralPath (Join-Path $session 'git-engine.toml') -Encoding utf8NoBOM
        $savedLogDirectory = [Environment]::GetEnvironmentVariable('GITVIEWS_LOG_DIR', 'Process')
        $savedRustLog = [Environment]::GetEnvironmentVariable('RUST_LOG', 'Process')
        $timer = [Diagnostics.Stopwatch]::StartNew()
        $startedAt = [DateTimeOffset]::UtcNow
        try {
            $env:GITVIEWS_LOG_DIR = $logDirectory
            $env:RUST_LOG = 'info'
            $process = Start-Process -FilePath $binary -WorkingDirectory $projectRoot -ArgumentList @('--config', ('"{0}"' -f $config), '--state-file', ('"{0}"' -f $state), '--', ('"{0}"' -f $repository)) -WindowStyle Hidden -PassThru
        } finally {
            [Environment]::SetEnvironmentVariable('GITVIEWS_LOG_DIR', $savedLogDirectory, 'Process')
            [Environment]::SetEnvironmentVariable('RUST_LOG', $savedRustLog, 'Process')
        }
        $memory = [Collections.Generic.List[object]]::new()
        $ready = $null
        $readyObservedMs = $null
        $shutdown = 'not_attempted'
        try {
            while (-not $process.HasExited) {
                $process.Refresh()
                $memory.Add([pscustomobject]@{
                    elapsed_ms = [Math]::Round($timer.Elapsed.TotalMilliseconds, 3)
                    working_set_bytes = $process.WorkingSet64
                    private_bytes = $process.PrivateMemorySize64
                    peak_working_set_bytes = $process.PeakWorkingSet64
                })
                if (-not $ready) {
                    $logPath = Join-Path $logDirectory 'gitviews.log'
                    if (Test-Path -LiteralPath $logPath -PathType Leaf) {
                        $ready = Get-Content -LiteralPath $logPath | Where-Object { $_ -match '\brepository_ready\b' } | Select-Object -First 1
                        if ($ready) { $readyObservedMs = $timer.Elapsed.TotalMilliseconds }
                    }
                    if (-not $ready -and $timer.Elapsed.TotalSeconds -gt $ReadyTimeoutSeconds) { throw "repository_ready not observed for $fixtureName; verify executable includes performance tracing. Isolated logs: $logDirectory" }
                }
                if ($ready -and $timer.Elapsed.TotalMilliseconds -ge $readyObservedMs + 1000 * $IdleSeconds) { break }
                Start-Sleep -Milliseconds 20
            }
            if ($process.HasExited) { throw "Desktop process exited before idle sampling for $fixtureName (exit $($process.ExitCode)). Logs: $logDirectory" }
            if ($ready -notmatch '\btree_loaded=true\b') { throw 'Repository readiness did not include a successfully loaded tree.' }
            if ($ready -notmatch '\bchanges=(?<changes>\d+)' -or [int]$Matches.changes -ne $fixture.fixture.modified_files + $fixture.fixture.untracked_files) { throw 'Desktop change count did not match the synthetic fixture.' }
            if ($ready -notmatch '\bfiles=(?<files>\d+)' -or [int]$Matches.files -ne $fixture.observations[0].tree_entries) { throw 'Desktop tree count did not match the service baseline fixture.' }
            if ($ready -notmatch '^(?<timestamp>\S+)') { throw 'Could not parse readiness timestamp.' }
            $readyAt = [DateTimeOffset]::Parse($Matches.timestamp, [Globalization.CultureInfo]::InvariantCulture)
            if ($ready -notmatch '\bload_ms=(?<load>\d+)') { throw 'Readiness event missing load_ms.' }
            $loadMs = [long]$Matches.load
            $idle = @($memory | Where-Object { $_.elapsed_ms -ge $readyObservedMs + 1000 * ($IdleSeconds - 1) })
            $sample = [ordered]@{
                iteration = $iteration
                launch_to_repository_ready_ms = ($readyAt - $startedAt).TotalMilliseconds
                launch_to_readiness_observed_ms = $readyObservedMs
                repository_load_apply_ms = $loadMs
                peak_working_set_bytes = ($memory | Measure-Object peak_working_set_bytes -Maximum).Maximum
                sampled_peak_private_bytes = ($memory | Measure-Object private_bytes -Maximum).Maximum
                idle_working_set_bytes = (Summarize @($idle.working_set_bytes)).median
                idle_private_bytes = (Summarize @($idle.private_bytes)).median
                memory_samples = $memory.ToArray()
                isolated_artifacts = $session.Substring($projectPrefix.Length).Replace('\', '/')
            }
        } finally {
            if (-not $process.HasExited) {
                if ($process.CloseMainWindow() -and $process.WaitForExit(5000)) { $shutdown = 'CloseMainWindow' }
                else { $process.Kill($true); $process.WaitForExit(); $shutdown = 'forced_after_close_attempt' }
            } else { $shutdown = 'already_exited' }
            $process.Dispose()
        }
        $sample.shutdown = $shutdown
        $samples += [pscustomobject]$sample
    }
    $summary = [ordered]@{}
    foreach ($metric in @('launch_to_repository_ready_ms', 'repository_load_apply_ms', 'peak_working_set_bytes', 'sampled_peak_private_bytes', 'idle_working_set_bytes', 'idle_private_bytes')) {
        $summary[$metric] = Summarize @($samples | ForEach-Object { $_.$metric })
    }
    $results += [pscustomobject]@{ fixture = $fixture.fixture; summary = $summary; samples = $samples }
}
$cpuName = 'unavailable'
$physicalMemory = $null
$graphics = @()
try {
    $cpuName = ((Get-CimInstance Win32_Processor | Select-Object -ExpandProperty Name) -join '; ').Trim()
    $physicalMemory = (Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory
    $graphics = @(Get-CimInstance Win32_VideoController | Select-Object Name, DriverVersion, CurrentHorizontalResolution, CurrentVerticalResolution)
} catch { Write-Verbose 'CIM machine metadata unavailable.' }
$report = [ordered]@{
    schema_version = 1
    measured_at_utc = [DateTime]::UtcNow.ToString('o')
    scope = 'Windows desktop process launch through repository model applied and idle; rendering/first GPU frame and GPU VRAM are not measured.'
    method = 'Hidden process per sample; isolated fresh preferences/state; built-in Git selected; OS cache is not flushed. Ready event timestamp excludes async log flush delay; observed timestamp includes it. Idle memory is median of final one-second samples.'
    machine = [ordered]@{
        os = [Runtime.InteropServices.RuntimeInformation]::OSDescription
        architecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
        logical_processors = [Environment]::ProcessorCount
        cpu = $cpuName; physical_memory_bytes = $physicalMemory
        graphics = $graphics
        powershell = $PSVersionTable.PSVersion.ToString()
    }
    executable_sha256 = (Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash.ToLowerInvariant()
    executable_bytes = (Get-Item -LiteralPath $binary).Length
    sampler_script_sha256 = (Get-FileHash -LiteralPath $PSCommandPath -Algorithm SHA256).Hash.ToLowerInvariant()
    service_baseline_sha256 = (Get-FileHash -LiteralPath $servicePath -Algorithm SHA256).Hash.ToLowerInvariant()
    idle_seconds = $IdleSeconds
    memory_sample_interval_requested_ms = 20
    fixtures = $results
}
New-Item -ItemType Directory -Path (Split-Path -Parent $output) -Force | Out-Null
$report | ConvertTo-Json -Depth 64 | Set-Content -LiteralPath $output -Encoding utf8NoBOM
Write-Host "Desktop baseline saved to $output"
$results | ForEach-Object {
    [pscustomobject]@{
        Fixture = $_.fixture.name
        'Ready median ms' = [Math]::Round($_.summary.launch_to_repository_ready_ms.median, 2)
        'Ready p95 ms' = [Math]::Round($_.summary.launch_to_repository_ready_ms.p95, 2)
        'Idle working MiB' = [Math]::Round($_.summary.idle_working_set_bytes.median / 1MB, 2)
        'Idle private MiB' = [Math]::Round($_.summary.idle_private_bytes.median / 1MB, 2)
    }
} | Format-Table -AutoSize
