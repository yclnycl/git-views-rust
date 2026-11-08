#requires -Version 7.0
<##
.SYNOPSIS
Build and package the portable Windows x64 executable with a target-only static CRT.
.EXAMPLE
pwsh -File scripts/release.ps1 -AllowD3DCompiler47
.EXAMPLE
pwsh -File scripts/release.ps1 -SkipBuild -AllowD3DCompiler47 -ValidationReport target/runtime-validation.json
##>
[CmdletBinding()]
param(
    [string]$OutputDirectory = 'dist',
    [switch]$SkipBuild,
    [switch]$AllowD3DCompiler47,
    [string]$ValidationReport
)

$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'This packaging script requires Windows and an MSVC Rust toolchain.' }
$projectRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$target = 'x86_64-pc-windows-msvc'
$buildDirectory = Join-Path $projectRoot 'target'
$sourceArtifact = Join-Path $buildDirectory "$target/release/gitviews.exe"
$devScript = Join-Path $PSScriptRoot 'dev.ps1'
$peScript = Join-Path $PSScriptRoot 'check-pe.ps1'
$targetFlags = 'target.x86_64-pc-windows-msvc.rustflags=["-C","target-feature=+crt-static"]'

function Resolve-SafeOutputDirectory([string]$Directory) {
    $absolute = if ([IO.Path]::IsPathRooted($Directory)) { [IO.Path]::GetFullPath($Directory) } else { [IO.Path]::GetFullPath((Join-Path $projectRoot $Directory)) }
    $prefix = $projectRoot.TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
    if (-not $absolute.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) { throw 'OutputDirectory must be a subdirectory of this project.' }
    $probe = $absolute
    while ($probe -and -not $probe.Equals($projectRoot, [StringComparison]::OrdinalIgnoreCase)) {
        if (Test-Path -LiteralPath $probe) {
            $item = Get-Item -LiteralPath $probe -Force
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw "The output path must not pass through a symbolic link or junction: $probe" }
            if (-not $item.PSIsContainer) { throw "Output path component is not a directory: $probe" }
        }
        $probe = Split-Path -Parent $probe
    }
    $absolute
}

function Assert-ArtifactWritable([string]$Artifact) {
    if (-not (Test-Path -LiteralPath $Artifact)) { return }
    $item = Get-Item -LiteralPath $Artifact -Force
    if ($item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw "The artifact destination must be a regular file: $Artifact" }
    try {
        $stream = [IO.File]::Open($Artifact, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
        $stream.Dispose()
    } catch { throw "Cannot replace $Artifact. GitViews may still be running or the file may be read-only. Close the application and retry. $($_.Exception.Message)" }
}

function Write-AtomicText([string]$Destination, [string]$Text) {
    $temporary = Join-Path (Split-Path -Parent $Destination) ('.gitviews-metadata-' + [Guid]::NewGuid().ToString('N') + '.tmp')
    try {
        $bytes = [Text.UTF8Encoding]::new($false).GetBytes($Text)
        $stream = [IO.File]::Open($temporary, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        try { $stream.Write($bytes, 0, $bytes.Length); $stream.Flush($true) } finally { $stream.Dispose() }
        [IO.File]::Move($temporary, $Destination, $true)
    } finally { if ([IO.File]::Exists($temporary)) { [IO.File]::Delete($temporary) } }
}

function Get-SourceEvidence([string]$RelativePath, [string]$Pattern) {
    $file = Join-Path $projectRoot $RelativePath
    $matched = (Test-Path -LiteralPath $file -PathType Leaf) -and ((Get-Content -LiteralPath $file -Raw) -match $Pattern)
    [pscustomobject]@{ path = $RelativePath; sourcePatternMatched = [bool]$matched; scope = 'Source inspection, not a clean-machine execution test.' }
}

$outputRoot = Resolve-SafeOutputDirectory $OutputDirectory
$artifact = Join-Path $outputRoot 'GitViews.exe'
if ($artifact.Equals([IO.Path]::GetFullPath($sourceArtifact), [StringComparison]::OrdinalIgnoreCase)) { throw 'The package destination must differ from the Cargo build artifact.' }
Assert-ArtifactWritable $artifact
$additionalValidation = $null
if ($ValidationReport) {
    $validationPath = if ([IO.Path]::IsPathRooted($ValidationReport)) { $ValidationReport } else { Join-Path $projectRoot $ValidationReport }
    $additionalValidation = Get-Content -LiteralPath $validationPath -Raw | ConvertFrom-Json -Depth 64
}

# A global RUSTFLAGS/ENCODED_RUSTFLAGS would override the target-specific config
# and can incorrectly apply crt-static to host proc-macros. Scope and restore
# these variables; dev.ps1 separately scopes the optional local toolchain.
$environmentNames = @('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'CARGO_BUILD_RUSTFLAGS', 'CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS', 'CARGO_INCREMENTAL')
$savedEnvironment = @{}
foreach ($name in $environmentNames) {
    $exists = Test-Path -LiteralPath "Env:$name"
    $savedEnvironment[$name] = [pscustomobject]@{
        Exists = $exists
        Value = if ($exists) { (Get-Item -LiteralPath "Env:$name").Value } else { $null }
    }
}
$cargoArguments = @('build', '--release', '--locked', '--target', $target, '--target-dir', $buildDirectory, '--package', 'gitviews-app', '--bin', 'gitviews', '--config', $targetFlags)
$copyTemporary = $null
Push-Location -LiteralPath $projectRoot
try {
    # .NET can retain an empty variable when SetEnvironmentVariable receives
    # null. An empty CARGO_ENCODED_RUSTFLAGS still overrides target rustflags.
    # Remove via the provider so Cargo actually sees these variables as absent.
    foreach ($name in $environmentNames) {
        if (Test-Path -LiteralPath "Env:$name") { Remove-Item -LiteralPath "Env:$name" }
        if (Test-Path -LiteralPath "Env:$name") { throw "Cannot clear build environment variable: $name" }
    }
    Set-Item -LiteralPath 'Env:CARGO_INCREMENTAL' -Value '0'
    if (-not $SkipBuild) {
        & $devScript @cargoArguments
        if ($LASTEXITCODE -ne 0) { throw "Cargo release build failed with exit code $LASTEXITCODE." }
    }
    if (-not (Test-Path -LiteralPath $sourceArtifact -PathType Leaf)) { throw "Release executable not found: $sourceArtifact" }
    [IO.Directory]::CreateDirectory($outputRoot) | Out-Null

    # Reject a dynamically linked or otherwise unapproved build before replacing
    # an existing package. No executable is launched by this verification.
    $sourcePe = & $peScript -Path $sourceArtifact -AllowD3DCompiler47:$AllowD3DCompiler47 -PassThru
    Assert-ArtifactWritable $artifact
    $copyTemporary = Join-Path $outputRoot ('.gitviews-executable-' + [Guid]::NewGuid().ToString('N') + '.tmp')
    [IO.File]::Copy($sourceArtifact, $copyTemporary, $false)
    $sourceHash = (Get-FileHash -LiteralPath $sourceArtifact -Algorithm SHA256).Hash.ToLowerInvariant()
    $copyHash = (Get-FileHash -LiteralPath $copyTemporary -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($copyHash -ne $sourceHash) { throw 'The package copy does not match the release build SHA-256.' }
    try { [IO.File]::Move($copyTemporary, $artifact, $true) }
    catch { throw "Cannot replace $artifact. Close any running GitViews executable and retry. $($_.Exception.Message)" }
    $copyTemporary = $null
    $finalHash = (Get-FileHash -LiteralPath $artifact -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($finalHash -ne $sourceHash) { throw 'The final package SHA-256 changed during replacement.' }

    $peReport = Join-Path $outputRoot 'pe-report.json'
    $sourcePe.path = $artifact
    $sourcePe.sha256 = $finalHash
    Write-AtomicText $peReport (($sourcePe | ConvertTo-Json -Depth 12) + [Environment]::NewLine)
    Write-AtomicText (Join-Path $outputRoot 'GitViews.exe.sha256') ($finalHash + '  GitViews.exe' + [Environment]::NewLine)
    $noticeSource = Join-Path $projectRoot 'crates/gitviews-app/assets/licenses/THIRD-PARTY-NOTICES.txt'
    Write-AtomicText (Join-Path $outputRoot 'THIRD-PARTY-NOTICES.txt') ([IO.File]::ReadAllText($noticeSource))
    $nativeGitEvidence = Get-SourceEvidence 'crates/gitviews-service/src/lib.rs' 'RepositoryService<B\s*=\s*Git2Backend>'
    $fontEvidence = Get-SourceEvidence 'crates/gitviews-app/src/app.rs' '(?s)add_fonts.*include_bytes!.*assets/fonts/'
    $nativeUiEvidence = Get-SourceEvidence 'crates/gitviews-app/src/app.rs' 'application\(\).*with_assets'
    $minimumWindows10 = if (@($sourcePe.imports) + @($sourcePe.delayImports) -contains 'icu.dll') { '1903' } else { '1803' }
    $buildInfo = [pscustomobject][ordered]@{
        schemaVersion = 1; packagedAtUtc = [DateTime]::UtcNow.ToString('o')
        artifact = [pscustomobject]@{ name = 'GitViews.exe'; sizeBytes = (Get-Item -LiteralPath $artifact).Length; sha256 = $finalHash; sourceArtifact = $sourceArtifact }
        build = [pscustomobject]@{ target = $target; profile = 'release'; locked = $true; skipped = [bool]$SkipBuild; cargoArguments = $cargoArguments; staticCrtAppliesTo = 'Explicit MSVC target only; host proc-macros are not targeted.'; toolchain = (Get-Content -LiteralPath (Join-Path $projectRoot 'rust-toolchain.toml') -Raw).Trim(); cargoLockSha256 = (Get-FileHash -LiteralPath (Join-Path $projectRoot 'Cargo.lock') -Algorithm SHA256).Hash.ToLowerInvariant() }
        platform = [pscustomobject]@{ architecture = 'x86_64'; windows10MinimumVersion = $minimumWindows10; windows11 = $true; graphics = 'Requires Windows Direct3D support and a graphics driver compatible with GPUI.'; verification = 'Declared project baseline plus known DLL requirements; cross-version runtime testing is recorded separately.' }
        distribution = [pscustomobject]@{ requiredRuntimeFiles = @('GitViews.exe'); checksumAndJsonFilesAreOptional = $true; staticImportPolicyPassed = $sourcePe.passed }
        prerequisiteEvidence = [pscustomobject]@{
            vcRedistributable = [pscustomobject]@{ requiredByPeImports = $false; evidence = 'PE normal/delay imports contain no Visual C++ runtime or UCRT DLL dependency.' }
            gitCommand = [pscustomobject]@{ notRequiredByDefaultBackendSource = $nativeGitEvidence.sourcePatternMatched; evidence = $nativeGitEvidence }
            externalFontFiles = [pscustomobject]@{ embeddedFontReferencesFound = $fontEvidence.sourcePatternMatched; evidence = $fontEvidence }
            nodeRuntime = [pscustomobject]@{ nativeGpuiEntryFound = $nativeUiEvidence.sourcePatternMatched; evidence = $nativeUiEvidence }
            rustToolchain = [pscustomobject]@{ notRequiredByNativeArtifactFormat = $true; evidence = 'The deliverable is an x86_64 native PE executable; Rust/Cargo are build tools.' }
        }
        runtimeValidation = if ($null -ne $additionalValidation) { $additionalValidation } else { [pscustomobject]@{ status = 'not-run-by-packaging-script'; scope = 'The packager does not launch the GUI, alter PATH, install/remove fonts, disable tools, or claim a clean Windows 10/11 test.' } }
        references = @('https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_6/nn-dxgi1_6-idxgifactory6', 'https://learn.microsoft.com/en-us/windows/win32/intl/international-components-for-unicode--icu-', 'https://www.nuget.org/packages/Microsoft.DXSDK.D3DX/9.29.952.7')
    }
    Write-AtomicText (Join-Path $outputRoot 'build-info.json') (($buildInfo | ConvertTo-Json -Depth 32) + [Environment]::NewLine)
    [pscustomobject]@{ executable = $artifact; sha256 = $finalHash; sizeBytes = $buildInfo.artifact.sizeBytes; peReport = $peReport; buildInfo = (Join-Path $outputRoot 'build-info.json') }
} finally {
    if ($copyTemporary -and [IO.File]::Exists($copyTemporary)) { [IO.File]::Delete($copyTemporary) }
    foreach ($name in $environmentNames) {
        $saved = $savedEnvironment[$name]
        if ($saved.Exists) { Set-Item -LiteralPath "Env:$name" -Value $saved.Value }
        elseif (Test-Path -LiteralPath "Env:$name") { Remove-Item -LiteralPath "Env:$name" }
    }
    Pop-Location
}
