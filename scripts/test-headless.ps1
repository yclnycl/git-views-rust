#requires -Version 7.0
<#
.SYNOPSIS
Run real GPUI interaction tests using simulated windows without opening the desktop app.
.EXAMPLE
.\scripts\test-headless.ps1 -Offline
#>
[CmdletBinding()]
param([switch]$Offline)

$ErrorActionPreference = 'Stop'
$projectRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$cargoArguments = @('test', '--package', 'gitviews-app', '--locked')
if ($Offline) { $cargoArguments += '--offline' }
$cargoArguments += @('headless_tests', '--', '--test-threads=1')
Push-Location -LiteralPath $projectRoot
try {
    & (Join-Path $PSScriptRoot 'dev.ps1') @cargoArguments
    $headlessExit = $LASTEXITCODE
} finally {
    Pop-Location
}
exit $headlessExit
