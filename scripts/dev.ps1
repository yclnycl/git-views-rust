# Run Cargo with the optional project-local toolchain, without changing system PATH.
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$localCargo = Join-Path $projectRoot '.tools\cargo\bin\cargo.exe'
$savedCargoHome = $env:CARGO_HOME
$savedRustupHome = $env:RUSTUP_HOME
$savedPath = $env:PATH
$nativeBuildVariables = @('OPENSSL_SRC_PERL', 'OPENSSL_RUST_USE_NASM', 'X86_64_PC_WINDOWS_MSVC_OPENSSL_NO_VENDOR', 'X86_64_PC_WINDOWS_MSVC_OPENSSL_STATIC', 'VCPKGRS_DISABLE', 'LIBSSH2_SYS_USE_PKG_CONFIG')
$savedNativeBuild = @{}
foreach ($name in $nativeBuildVariables) {
    $savedNativeBuild[$name] = @{ Exists = Test-Path -LiteralPath "Env:$name"; Value = [Environment]::GetEnvironmentVariable($name, 'Process') }
}
Push-Location -LiteralPath $projectRoot
try {
    $localPerl = Join-Path $projectRoot '.tools\perl\perl\bin\perl.exe'
    if (-not $env:OPENSSL_SRC_PERL -and (Test-Path -LiteralPath $localPerl)) { $env:OPENSSL_SRC_PERL = $localPerl }
    $env:OPENSSL_RUST_USE_NASM = '0'
    $env:X86_64_PC_WINDOWS_MSVC_OPENSSL_NO_VENDOR = '0'
    $env:X86_64_PC_WINDOWS_MSVC_OPENSSL_STATIC = '1'
    $env:VCPKGRS_DISABLE = '1'
    Remove-Item -LiteralPath 'Env:LIBSSH2_SYS_USE_PKG_CONFIG' -ErrorAction SilentlyContinue
    if (Test-Path -LiteralPath $localCargo) {
        $env:CARGO_HOME = Join-Path $projectRoot '.tools\cargo'
        $env:RUSTUP_HOME = Join-Path $projectRoot '.tools\rustup'
        $env:PATH = "$(Split-Path -Parent $localCargo);$env:PATH"
        $cargoCommand = $localCargo
    } else {
        $cargoCommand = (Get-Command cargo -ErrorAction Stop).Source
    }
    $cargoArguments = $args
    if ($cargoArguments.Count -eq 0) { $cargoArguments = @('run') }
    & $cargoCommand @cargoArguments
    $cargoExitCode = $LASTEXITCODE
} finally {
    Pop-Location
    $env:CARGO_HOME = $savedCargoHome
    $env:RUSTUP_HOME = $savedRustupHome
    $env:PATH = $savedPath
    foreach ($name in $nativeBuildVariables) {
        if ($savedNativeBuild[$name].Exists) {
            Set-Item -LiteralPath "Env:$name" -Value $savedNativeBuild[$name].Value
        } else {
            Remove-Item -LiteralPath "Env:$name" -ErrorAction SilentlyContinue
        }
    }
}
exit $cargoExitCode
