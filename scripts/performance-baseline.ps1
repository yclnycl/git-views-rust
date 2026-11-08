#requires -Version 7.0
<#
.SYNOPSIS
Measure deterministic local fixture workloads and Windows process memory.
.EXAMPLE
pwsh -File scripts/performance-baseline.ps1 -Offline
.EXAMPLE
pwsh -File scripts/performance-baseline.ps1 -Offline -CompareTo docs/performance/2026-11-08-baseline.json
#>
[CmdletBinding()]
param(
    [ValidateRange(3, 100)][int]$Iterations = 7,
    [ValidateSet('debug', 'release')][string]$Profile = 'release',
    [string]$OutputPath = 'dist/performance-baseline/current.json',
    [string]$CompareTo,
    [string]$FixtureDirectory,
    [switch]$Offline,
    [switch]$SkipBuild,
    [ValidateRange(1, 50)][int]$SampleIntervalMs = 10
)

$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'This process-memory sampler requires Windows; the Rust example is portable.' }
if ($env:CARGO_TARGET_DIR -or $env:CARGO_BUILD_TARGET) { throw 'Clear CARGO_TARGET_DIR/CARGO_BUILD_TARGET so the measured artifact matches the requested host build.' }
$projectRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$output = if ([IO.Path]::IsPathRooted($OutputPath)) { [IO.Path]::GetFullPath($OutputPath) } else { [IO.Path]::GetFullPath((Join-Path $projectRoot $OutputPath)) }
$projectPrefix = $projectRoot.TrimEnd('\') + '\'
if (-not $output.StartsWith($projectPrefix, [StringComparison]::OrdinalIgnoreCase)) { throw 'OutputPath must be inside this project.' }
$probe = Split-Path -Parent $output
while ($probe -and -not $probe.Equals($projectRoot, [StringComparison]::OrdinalIgnoreCase)) {
    if (Test-Path -LiteralPath $probe) {
        $item = Get-Item -LiteralPath $probe -Force
        if (-not $item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw "Output path must use ordinary directories: $probe" }
    }
    $probe = Split-Path -Parent $probe
}
if (Test-Path -LiteralPath $output) {
    $item = Get-Item -LiteralPath $output -Force
    if ($item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'OutputPath must be an ordinary file.' }
}
$git = (Get-Command git -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source
# Capture just the measured crates and build inputs, before and after the run,
# so concurrent source edits cannot silently be attributed to the executable.
function Source-Fingerprint {
    $inputs = @(Get-ChildItem -LiteralPath (Join-Path $projectRoot 'crates/gitviews-git'), (Join-Path $projectRoot 'crates/gitviews-service') -Recurse -File | Where-Object { $_.Extension -eq '.rs' -or $_.Name -eq 'Cargo.toml' })
    $inputs += Get-Item -LiteralPath (Join-Path $projectRoot 'Cargo.toml'), (Join-Path $projectRoot 'Cargo.lock'), (Join-Path $projectRoot 'rust-toolchain.toml'), (Join-Path $projectRoot '.cargo/config.toml'), $PSCommandPath
    @($inputs | Sort-Object FullName | ForEach-Object { '{0} {1}' -f $_.FullName.Substring($projectPrefix.Length).Replace('\', '/'), (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant() })
}
$sourceBefore = Source-Fingerprint
$arguments = @('build', '--package', 'gitviews-service', '--example', 'performance_baseline', '--locked')
$target = 'host default'
if ($Profile -eq 'release') {
    $target = 'x86_64-pc-windows-msvc'
    if ($env:RUSTFLAGS -or $env:CARGO_ENCODED_RUSTFLAGS -or $env:CARGO_BUILD_RUSTFLAGS -or $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS) { throw 'Clear custom Cargo/Rust flags before the standardized portable release baseline.' }
    $arguments += @('--release', '--target', $target, '--config', 'target.x86_64-pc-windows-msvc.rustflags=["-C","target-feature=+crt-static"]')
}
if ($Offline) { $arguments += '--offline' }
if (-not $SkipBuild) {
    & (Join-Path $PSScriptRoot 'dev.ps1') @arguments
    if ($LASTEXITCODE -ne 0) { throw 'Baseline example build failed.' }
}
$binaryDirectory = if ($Profile -eq 'release') { "target/$target/$Profile/examples" } else { "target/$Profile/examples" }
$binary = Join-Path $projectRoot "$binaryDirectory/performance_baseline.exe"
if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) { throw "Build artifact missing: $binary" }
$runId = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssZ') + '-' + [Guid]::NewGuid().ToString('N').Substring(0, 8)
$scratch = Join-Path $projectRoot "target/performance-baseline/$runId"
New-Item -ItemType Directory -Path $scratch -Force | Out-Null
if ($FixtureDirectory) {
    $fixtureRoot = if ([IO.Path]::IsPathRooted($FixtureDirectory)) { [IO.Path]::GetFullPath($FixtureDirectory) } else { [IO.Path]::GetFullPath((Join-Path $projectRoot $FixtureDirectory)) }
    $fixturePrefix = (Join-Path $projectRoot 'target/performance-baseline').TrimEnd('\') + '\'
    if (-not $fixtureRoot.StartsWith($fixturePrefix, [StringComparison]::OrdinalIgnoreCase)) { throw 'Reused fixtures must come from target/performance-baseline inside this project.' }
    $probe = $fixtureRoot
    while ($probe -and -not $probe.Equals($projectRoot, [StringComparison]::OrdinalIgnoreCase)) {
        if (-not (Test-Path -LiteralPath $probe -PathType Container) -or ((Get-Item -LiteralPath $probe -Force).Attributes -band [IO.FileAttributes]::ReparsePoint)) { throw 'Reused fixture directory must exist and must not traverse a link or junction.' }
        $probe = Split-Path -Parent $probe
    }
    foreach ($name in @('small', 'medium', 'large')) {
        if (-not (Test-Path -LiteralPath (Join-Path $fixtureRoot "$name.json") -PathType Leaf)) { throw "Generated fixture manifest missing: $name.json" }
    }
    Write-Host 'Reusing previously generated synthetic fixtures; measured workload validation remains enabled.'
} else {
    $fixtureRoot = Join-Path $scratch 'fixtures'
    Write-Host 'Preparing synthetic fixtures (100 / 1,000 / 10,000 tracked files); no network access.'
    & $binary prepare $fixtureRoot
    if ($LASTEXITCODE -ne 0) { throw 'Fixture preparation failed.' }
}

function Measure-Fixture([string]$Name) {
    $reportPath = Join-Path $scratch "$Name.json"
    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $binary
    $startInfo.WorkingDirectory = $projectRoot
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    foreach ($argument in @('run', (Join-Path $fixtureRoot $Name), "$Iterations", $reportPath)) { $startInfo.ArgumentList.Add($argument) }
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    $timer = [Diagnostics.Stopwatch]::StartNew()
    $memory = [Collections.Generic.List[object]]::new()
    try {
        if (-not $process.Start()) { throw 'Could not start baseline process.' }
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        while (-not $process.HasExited) {
            if ($timer.Elapsed.TotalMinutes -gt 10) { $process.Kill($true); throw 'Baseline workload exceeded ten minutes.' }
            try {
                $process.Refresh()
                $memory.Add([pscustomobject]@{
                    elapsed_ms = [Math]::Round($timer.Elapsed.TotalMilliseconds, 3)
                    working_set_bytes = $process.WorkingSet64
                    private_bytes = $process.PrivateMemorySize64
                    peak_working_set_bytes = $process.PeakWorkingSet64
                })
            } catch [InvalidOperationException] { if (-not $process.HasExited) { throw } }
            Start-Sleep -Milliseconds $SampleIntervalMs
        }
        $process.WaitForExit()
        $stderr = $stderrTask.GetAwaiter().GetResult()
        $null = $stdoutTask.GetAwaiter().GetResult()
        if ($process.ExitCode -ne 0) { throw "Fixture $Name failed: $stderr" }
        if ($memory.Count -eq 0) { throw "No memory observations collected for $Name." }
        $report = Get-Content -LiteralPath $reportPath -Raw | ConvertFrom-Json -Depth 64
        $report | Add-Member -NotePropertyName memory -NotePropertyValue ([pscustomobject]@{
            scope = 'Whole standalone service benchmark process, including serial default-budget/full-budget search workloads and harness/results. Default content index dropped before full-budget allocation. Excludes fixture creation, Git CLI, Cargo and GPUI.'
            sample_interval_requested_ms = $SampleIntervalMs
            sample_count = $memory.Count
            process_elapsed_ms = [Math]::Round($timer.Elapsed.TotalMilliseconds, 3)
            peak_working_set_bytes = ($memory | Measure-Object peak_working_set_bytes -Maximum).Maximum
            sampled_peak_working_set_bytes = ($memory | Measure-Object working_set_bytes -Maximum).Maximum
            sampled_peak_private_bytes = ($memory | Measure-Object private_bytes -Maximum).Maximum
            samples = $memory.ToArray()
        })
        # Preserve enriched per-fixture data even if later metadata/comparison
        # validation fails. No workload process is running during this write.
        $report | ConvertTo-Json -Depth 64 | Set-Content -LiteralPath $reportPath -Encoding utf8NoBOM
        $report
    } finally { $process.Dispose() }
}

$fixtures = @()
foreach ($name in @('small', 'medium', 'large')) {
    Write-Host "Measuring $name ($Iterations iterations)..."
    $fixtures += Measure-Fixture $name
}

$sourceHashes = Source-Fingerprint
if (($sourceBefore -join "`n") -cne ($sourceHashes -join "`n")) { throw 'Measured source/build inputs changed during this run. Rerun after edits finish; raw workload reports remain in target/performance-baseline.' }
$hasher = [Security.Cryptography.SHA256]::Create()
try { $sourceDigest = [BitConverter]::ToString($hasher.ComputeHash([Text.Encoding]::UTF8.GetBytes(($sourceHashes -join "`n")))).Replace('-', '').ToLowerInvariant() }
finally { $hasher.Dispose() }
$cpuName = 'unavailable'
$physicalMemory = $null
try {
    $cpuName = ((Get-CimInstance Win32_Processor | Select-Object -ExpandProperty Name) -join '; ').Trim()
    $physicalMemory = (Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory
} catch { Write-Verbose 'CIM machine metadata unavailable; OS and logical CPU metadata are still recorded.' }
$comparison = $null
if ($CompareTo) {
    $previousPath = if ([IO.Path]::IsPathRooted($CompareTo)) { $CompareTo } else { Join-Path $projectRoot $CompareTo }
    $previous = Get-Content -LiteralPath $previousPath -Raw | ConvertFrom-Json -Depth 64
    $comparison = @()
    foreach ($fixture in $fixtures) {
        $old = @($previous.fixtures | Where-Object { $_.fixture.name -eq $fixture.fixture.name })
        if ($old.Count -ne 1) { throw "Cannot find exactly one matching fixture: $($fixture.fixture.name)" }
        foreach ($field in @('version', 'head', 'tracked_files', 'commits', 'modified_files', 'untracked_files', 'working_text_bytes')) {
            if ($old[0].fixture.$field -ne $fixture.fixture.$field) { throw "Cannot compare changed fixture $($fixture.fixture.name): $field differs." }
        }
        foreach ($metric in $fixture.metrics.PSObject.Properties) {
            $priorMetric = $old[0].metrics.PSObject.Properties[$metric.Name]
            if ($null -eq $priorMetric) { continue }
            $prior = $priorMetric.Value
            $comparison += [pscustomobject]@{
                fixture = $fixture.fixture.name; metric = $metric.Name
                median_change_percent = if ($prior.median_ms -gt 0) { [Math]::Round(100 * ($metric.Value.median_ms / $prior.median_ms - 1), 2) } else { $null }
                p95_change_percent = if ($prior.p95_ms -gt 0) { [Math]::Round(100 * ($metric.Value.p95_ms / $prior.p95_ms - 1), 2) } else { $null }
            }
        }
        foreach ($metric in @('peak_working_set_bytes', 'sampled_peak_private_bytes')) {
            $before = $old[0].memory.$metric
            $comparison += [pscustomobject]@{
                fixture = $fixture.fixture.name; metric = $metric
                change_percent = if ($before -gt 0) { [Math]::Round(100 * ($fixture.memory.$metric / $before - 1), 2) } else { $null }
            }
        }
    }
    if ($previous.build.profile -ne $Profile -or $previous.machine.cpu -ne $cpuName) { Write-Warning 'Comparison machine/profile differ; treat percentage changes as descriptive, not regression gates.' }
}

$result = [ordered]@{
    schema_version = 1
    measured_at_utc = [DateTime]::UtcNow.ToString('o')
    scope = 'Built-in Git service and search; synthetic local-only fixtures. UI startup, rendering, watchers, remote/network and system-Git backend are excluded.'
    method = 'Fresh service and empty caches per iteration; repeated calls use the same service; OS filesystem cache is not flushed. Median uses middle/average; p95 is nearest rank. Memory values are process observations, not memory caps.'
    machine = [ordered]@{
        os = [Runtime.InteropServices.RuntimeInformation]::OSDescription
        architecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
        logical_processors = [Environment]::ProcessorCount
        cpu = $cpuName
        physical_memory_bytes = $physicalMemory
        powershell = $PSVersionTable.PSVersion.ToString()
    }
    build = [ordered]@{
        profile = $Profile; skipped_build = [bool]$SkipBuild
        build_command = "scripts/dev.ps1 $($arguments -join ' ')"
        build_arguments = $arguments
        target = $target; static_crt_requested = $Profile -eq 'release'
        rust_toolchain = (Get-Content -LiteralPath (Join-Path $projectRoot 'rust-toolchain.toml') -Raw).Trim()
        executable_sha256 = (Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash.ToLowerInvariant()
        executable_bytes = (Get-Item -LiteralPath $binary).Length
        source_sha256 = $sourceDigest; source_inputs = $sourceHashes
        fixture_git_version = (& $git --version)
        rustflags = $env:RUSTFLAGS; encoded_rustflags = $env:CARGO_ENCODED_RUSTFLAGS
    }
    fixture_directory = $fixtureRoot.Substring($projectPrefix.Length).Replace('\', '/')
    reused_fixtures = [bool]$FixtureDirectory
    fixtures = $fixtures
    comparison = $comparison
    comparison_reference = if ($CompareTo) {
        [ordered]@{
            report_sha256 = (Get-FileHash -LiteralPath $previousPath -Algorithm SHA256).Hash.ToLowerInvariant()
            measured_at_utc = $previous.measured_at_utc
            source_sha256 = $previous.build.source_sha256
            executable_sha256 = $previous.build.executable_sha256
        }
    } else { $null }
}
New-Item -ItemType Directory -Path (Split-Path -Parent $output) -Force | Out-Null
$result | ConvertTo-Json -Depth 64 | Set-Content -LiteralPath $output -Encoding utf8NoBOM
Write-Host "Baseline saved to $output"
$fixtures | ForEach-Object {
    [pscustomobject]@{
        Fixture = $_.fixture.name
        'Load median ms' = [Math]::Round($_.metrics.load_scan_fresh_service.median_ms, 3)
        'Load p95 ms' = [Math]::Round($_.metrics.load_scan_fresh_service.p95_ms, 3)
        'Full content miss median ms' = [Math]::Round($_.metrics.content_search_full_cache_miss.median_ms, 3)
        'Peak working MiB' = [Math]::Round($_.memory.peak_working_set_bytes / 1MB, 2)
        'Sampled private MiB' = [Math]::Round($_.memory.sampled_peak_private_bytes / 1MB, 2)
    }
} | Format-Table -AutoSize
