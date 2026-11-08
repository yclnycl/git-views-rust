#requires -Version 7.0
<##
.SYNOPSIS
Inspect normal and delay-load PE imports without loading the executable.
.EXAMPLE
pwsh -File scripts/check-pe.ps1 -Path dist/GitViews.exe -ReportPath dist/pe-report.json -AllowD3DCompiler47
##>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Path,
    [string]$ReportPath,
    [switch]$AllowD3DCompiler47,
    [switch]$PassThru
)

$ErrorActionPreference = 'Stop'

function Read-UInt16([byte[]]$Data, [long]$Offset) {
    if ($Offset -lt 0 -or $Offset + 2 -gt $Data.LongLength) { throw 'Truncated PE integer.' }
    [BitConverter]::ToUInt16($Data, [int]$Offset)
}
function Read-UInt32([byte[]]$Data, [long]$Offset) {
    if ($Offset -lt 0 -or $Offset + 4 -gt $Data.LongLength) { throw 'Truncated PE integer.' }
    [BitConverter]::ToUInt32($Data, [int]$Offset)
}
function Read-UInt64([byte[]]$Data, [long]$Offset) {
    if ($Offset -lt 0 -or $Offset + 8 -gt $Data.LongLength) { throw 'Truncated PE integer.' }
    [BitConverter]::ToUInt64($Data, [int]$Offset)
}
function Convert-RvaToOffset([uint32]$Rva, $Image) {
    if ($Rva -lt $Image.SizeOfHeaders -and $Rva -lt $Image.Data.LongLength) { return [long]$Rva }
    foreach ($section in $Image.Sections) {
        $delta = [long]$Rva - [long]$section.VirtualAddress
        if ($delta -ge 0 -and $delta -lt [Math]::Max([long]$section.VirtualSize, [long]$section.RawSize)) {
            if ($delta -ge $section.RawSize) { throw ('RVA 0x{0:X} points outside raw section data.' -f $Rva) }
            $offset = [long]$section.RawPointer + $delta
            if ($offset -ge $Image.Data.LongLength) { throw 'PE section points beyond the file.' }
            return $offset
        }
    }
    throw ('Unmapped PE RVA 0x{0:X}.' -f $Rva)
}
function Read-ImportName([uint32]$Rva, $Image) {
    $offset = Convert-RvaToOffset $Rva $Image
    $bytes = [Collections.Generic.List[byte]]::new()
    for ($index = 0; $index -lt 4096; $index++) {
        if ($offset + $index -ge $Image.Data.LongLength) { throw 'Unterminated PE import name.' }
        $value = $Image.Data[[int]($offset + $index)]
        if ($value -eq 0) {
            $name = [Text.Encoding]::ASCII.GetString($bytes.ToArray())
            if ([string]::IsNullOrWhiteSpace($name) -or $name -match '[/\\:]') { throw 'Invalid PE import module name.' }
            return $name.ToLowerInvariant()
        }
        if ($value -lt 0x20 -or $value -gt 0x7e) { throw 'Non-ASCII PE import module name.' }
        $bytes.Add($value)
    }
    throw 'PE import module name exceeds 4096 bytes.'
}
function Read-ImportDirectory($Image, [int]$DirectoryIndex, [bool]$Delay) {
    if ($DirectoryIndex -ge $Image.DirectoryCount) { return @() }
    $directory = $Image.DirectoryOffset + 8 * $DirectoryIndex
    $rva = Read-UInt32 $Image.Data $directory
    $size = Read-UInt32 $Image.Data ($directory + 4)
    if ($rva -eq 0) { return @() }
    $stride = if ($Delay) { 32 } else { 20 }
    if ($size -lt $stride) { throw 'PE import directory is smaller than one descriptor.' }
    $count = [Math]::Min([long]4096, [Math]::Floor($size / $stride))
    $names = [Collections.Generic.List[string]]::new()
    $terminated = $false
    for ($index = 0; $index -lt $count; $index++) {
        $descriptorRva = [uint32]([long]$rva + $index * $stride)
        $offset = Convert-RvaToOffset $descriptorRva $Image
        $empty = $true
        for ($field = 0; $field -lt $stride; $field += 4) {
            if ((Read-UInt32 $Image.Data ($offset + $field)) -ne 0) { $empty = $false }
        }
        if ($empty) { $terminated = $true; break }
        $nameAddress = Read-UInt32 $Image.Data ($offset + $(if ($Delay) { 4 } else { 12 }))
        if ($Delay -and ((Read-UInt32 $Image.Data $offset) -band 1) -eq 0) {
            if ([uint64]$nameAddress -lt $Image.ImageBase) { throw 'Invalid VA-based delay import descriptor.' }
            $nameAddress = [uint32]([uint64]$nameAddress - $Image.ImageBase)
        }
        $names.Add((Read-ImportName $nameAddress $Image))
    }
    if (-not $terminated) { throw 'PE import descriptors have no terminator within their directory.' }
    @($names | Sort-Object -Unique)
}
function Read-PeImage([string]$Executable) {
    $data = [IO.File]::ReadAllBytes($Executable)
    if ($data.Length -lt 64 -or $data[0] -ne 0x4d -or $data[1] -ne 0x5a) { throw 'The artifact is not a DOS/PE executable.' }
    $pe = [long](Read-UInt32 $data 0x3c)
    if ((Read-UInt32 $data $pe) -ne 0x00004550) { throw 'The artifact has no valid PE signature.' }
    $machine = Read-UInt16 $data ($pe + 4)
    $sectionCount = Read-UInt16 $data ($pe + 6)
    $optionalSize = Read-UInt16 $data ($pe + 20)
    $optional = $pe + 24
    $magic = Read-UInt16 $data $optional
    if ($sectionCount -eq 0 -or $sectionCount -gt 96) { throw 'Invalid PE section count.' }
    if ($magic -ne 0x20b -or $machine -ne 0x8664) { throw 'Expected an x86_64 Windows PE32+ executable.' }
    if ($optionalSize -lt 112 -or $optional + $optionalSize -gt $data.LongLength) { throw 'Invalid PE32+ optional header.' }
    $directoryCount = Read-UInt32 $data ($optional + 108)
    if ($directoryCount -gt [Math]::Floor(($optionalSize - 112) / 8)) { throw 'PE data directories exceed the optional header.' }
    $sections = @()
    for ($index = 0; $index -lt $sectionCount; $index++) {
        $offset = $optional + $optionalSize + 40 * $index
        $sections += [pscustomobject]@{
            VirtualSize = Read-UInt32 $data ($offset + 8)
            VirtualAddress = Read-UInt32 $data ($offset + 12)
            RawSize = Read-UInt32 $data ($offset + 16)
            RawPointer = Read-UInt32 $data ($offset + 20)
        }
    }
    $image = [pscustomobject]@{
        Data = $data; Sections = $sections
        SizeOfHeaders = Read-UInt32 $data ($optional + 60)
        DirectoryOffset = $optional + 112; DirectoryCount = $directoryCount
        ImageBase = Read-UInt64 $data ($optional + 24)
    }
    [pscustomobject]@{
        Architecture = 'x86_64'; Format = 'PE32+'
        Imports = @(Read-ImportDirectory $image 1 $false)
        DelayImports = @(Read-ImportDirectory $image 13 $true)
    }
}

function Find-Dumpbin {
    $command = Get-Command dumpbin.exe -ErrorAction SilentlyContinue
    if ($command) { return $command.Source }
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
    if (-not (Test-Path -LiteralPath $vswhere -PathType Leaf)) { return $null }
    $installation = @(& $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null) | Select-Object -First 1
    if (-not $installation) { return $null }
    $toolsets = Get-ChildItem -LiteralPath (Join-Path $installation 'VC/Tools/MSVC') -Directory -ErrorAction SilentlyContinue | Sort-Object Name -Descending
    foreach ($toolset in $toolsets) {
        foreach ($hostArchitecture in @('Hostx64', 'Hostx86')) {
            $candidate = Join-Path $toolset.FullName "bin/$hostArchitecture/x64/dumpbin.exe"
            if (Test-Path -LiteralPath $candidate -PathType Leaf) { return $candidate }
        }
    }
    $null
}

$executable = (Resolve-Path -LiteralPath $Path -ErrorAction Stop).ProviderPath
if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) { throw 'Provide a regular executable file.' }
$peImage = Read-PeImage $executable

# Explicit Windows 10/11 OS libraries. Being present on this developer machine
# alone is deliberately insufficient evidence that a DLL belongs to Windows.
$systemDlls = @(
    'advapi32.dll', 'authz.dll', 'avrt.dll', 'bcrypt.dll', 'bcryptprimitives.dll',
    'cabinet.dll', 'cfgmgr32.dll', 'combase.dll', 'comctl32.dll', 'comdlg32.dll',
    'crypt32.dll', 'cryptbase.dll', 'cryptsp.dll', 'd2d1.dll', 'd3d11.dll',
    'd3d12.dll', 'd3d9.dll', 'dcomp.dll', 'dbgcore.dll', 'dbghelp.dll',
    'dnsapi.dll', 'dsound.dll', 'dwmapi.dll', 'dwrite.dll', 'dxcore.dll', 'dxgi.dll',
    'gdi32.dll', 'gdi32full.dll', 'gdiplus.dll', 'hid.dll', 'icu.dll', 'icuin.dll', 'icuuc.dll', 'imagehlp.dll', 'imm32.dll',
    'iphlpapi.dll', 'kernel32.dll', 'kernelbase.dll', 'msimg32.dll', 'msvcrt.dll',
    'ncrypt.dll', 'netapi32.dll', 'normaliz.dll', 'ntdll.dll', 'ole32.dll',
    'oleacc.dll', 'oleaut32.dll', 'powrprof.dll', 'profapi.dll', 'propsys.dll',
    'psapi.dll', 'rpcrt4.dll', 'secur32.dll', 'setupapi.dll', 'shell32.dll',
    'shcore.dll', 'shlwapi.dll', 'sspicli.dll', 'uiautomationcore.dll', 'user32.dll', 'userenv.dll',
    'usp10.dll', 'uxtheme.dll', 'version.dll', 'websocket.dll', 'wevtapi.dll',
    'windowscodecs.dll', 'winhttp.dll', 'wininet.dll', 'winmm.dll', 'winnsi.dll',
    'winspool.drv', 'wintrust.dll', 'wldap32.dll', 'ws2_32.dll', 'wtsapi32.dll'
)
$blocked = [Collections.Generic.List[object]]::new()
$reviewed = [Collections.Generic.List[string]]::new()
$modules = @(@($peImage.Imports) + @($peImage.DelayImports) | Sort-Object -Unique)
foreach ($module in $modules) {
    if ($module -match '^(vcruntime|msvcp|msvcr[0-9]|concrt|vcomp)[^/\\]*\.dll$' -or $module -match '^ucrtbase[d]?\.dll$' -or $module -like 'api-ms-win-crt-*.dll') {
        $blocked.Add([pscustomobject]@{ module = $module; reason = 'Dynamic Visual C++/UCRT runtime; static-CRT packaging required.' })
    } elseif ($module -eq 'd3dcompiler_47.dll') {
        if ($AllowD3DCompiler47) { $reviewed.Add($module) }
        else { $blocked.Add([pscustomobject]@{ module = $module; reason = 'Requires explicit Windows 10/11 D3DCompiler assessment; use -AllowD3DCompiler47 after verification.' }) }
    } elseif ($module -notin $systemDlls -and $module -notmatch '^(api-ms-win-(core|security|service|shell|shcore|appmodel|eventing|power|ntuser|rtcore)|ext-ms-win-)[a-z0-9_.-]+\.dll$') {
        $blocked.Add([pscustomobject]@{ module = $module; reason = 'Not in the explicit Windows 10/11 system-DLL policy.' })
    }
}

$dumpbinEvidence = [ordered]@{ available = $false; path = $null; exitCode = $null; output = $null }
$dumpbin = Find-Dumpbin
if ($dumpbin) {
    $dumpbinEvidence.available = $true; $dumpbinEvidence.path = $dumpbin
    try {
        $dumpbinEvidence.output = (@(& $dumpbin /NOLOGO /DEPENDENTS $executable 2>&1) | Out-String).Trim()
        $dumpbinEvidence.exitCode = $LASTEXITCODE
    } catch { $dumpbinEvidence.output = $_.Exception.Message }
}
$report = [pscustomobject][ordered]@{
    schemaVersion = 1; checkedAtUtc = [DateTime]::UtcNow.ToString('o')
    path = $executable; architecture = $peImage.Architecture; format = $peImage.Format
    sha256 = (Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash.ToLowerInvariant()
    imports = @($peImage.Imports); delayImports = @($peImage.DelayImports)
    blockedDependencies = @($blocked.ToArray()); explicitlyReviewedDependencies = @($reviewed.ToArray())
    passed = ($blocked.Count -eq 0); policy = 'Windows 10 1803+/11 x64; system imports only; static Visual C++ CRT'
    dumpbin = [pscustomobject]$dumpbinEvidence
    scope = 'Static PE import and delay-import tables. Runtime LoadLibrary calls, drivers, external file access and clean-machine behavior require separate runtime validation.'
}
$json = $report | ConvertTo-Json -Depth 12
if ($ReportPath) {
    $absoluteReport = [IO.Path]::GetFullPath($ReportPath)
    if ($absoluteReport.Equals($executable, [StringComparison]::OrdinalIgnoreCase)) { throw 'The JSON report must not overwrite the inspected executable.' }
    $reportDirectory = Split-Path -Parent $absoluteReport
    if (-not (Test-Path -LiteralPath $reportDirectory)) { [IO.Directory]::CreateDirectory($reportDirectory) | Out-Null }
    [IO.File]::WriteAllText($absoluteReport, $json + [Environment]::NewLine, [Text.UTF8Encoding]::new($false))
}
if ($PassThru) { $report } else { $json }
if (-not $report.passed) { throw ('PE dependency verification failed: ' + (($blocked | ForEach-Object { $_.module }) -join ', ')) }
