<#
.SYNOPSIS
    Builds the AirBridge Windows FFI library and regenerates the C# bindings.

.DESCRIPTION
    Produces target/<profile>/core_ffi.dll (the C-ABI UniFFI library) and the
    C# bindings in target/bindings/csharp, optionally building the
    AirBridge.Core wrapper assembly on top.

    The MSVC linker is not installed on every machine. If the build fails with
    "link.exe not found", install the Visual Studio C++ build tools or pass
    -Toolchain stable-x86_64-pc-windows-gnu (which needs a MinGW/TDM-GCC
    distribution).

.PARAMETER Profile
    Cargo profile: 'release' (default) or 'debug'.

.PARAMETER Toolchain
    Cargo toolchain to use, e.g. 'stable-x86_64-pc-windows-gnu'. Defaults to
    $env:AIRBRIDGE_TOOLCHAIN, or the machine default when unset.

.PARAMETER NoBindings
    Build the library only; do not regenerate the C# bindings.

.PARAMETER BuildWrapper
    Also run `dotnet build bindings/csharp/AirBridge.Core.csproj`.

.EXAMPLE
    pwsh scripts/build-windows.ps1

.EXAMPLE
    pwsh scripts/build-windows.ps1 -Toolchain stable-x86_64-pc-windows-gnu -BuildWrapper
#>
[CmdletBinding()]
param(
    [ValidateSet('debug', 'release')]
    [string] $Profile = 'release',

    [string] $Toolchain = $env:AIRBRIDGE_TOOLCHAIN,

    [switch] $NoBindings,

    [switch] $BuildWrapper
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repoRoot = Split-Path -Parent $PSScriptRoot
$profileDir = if ($Profile -eq 'release') { 'release' } else { 'debug' }
$library = Join-Path $repoRoot "target/$profileDir/core_ffi.dll"
$bindingsDir = Join-Path $repoRoot 'target/bindings/csharp'
$configFile = Join-Path $repoRoot 'crates/libcore_ffi/uniffi.toml'

function Resolve-Tool([string] $Name) {
    $command = Get-Command $Name -ErrorAction SilentlyContinue
    if ($command) { return $command.Source }
    $fallback = Join-Path $env:USERPROFILE ".cargo/bin/$Name.exe"
    if (Test-Path $fallback) { return $fallback }
    return $null
}

# Runs a native program with error-action preference relaxed: cargo prints
# linker warnings to stderr, and under $ErrorActionPreference = 'Stop' that
# would otherwise be treated as a terminating error. The exit code is the
# contract; output goes straight to the host so it cannot pollute the return
# value.
function Invoke-Native([string] $Command, [string[]] $Arguments) {
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $Command @Arguments | Out-Host
        return $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $previous
    }
}

$cargo = Resolve-Tool 'cargo'
if (-not $cargo) {
    throw "cargo was not found on PATH or in $env:USERPROFILE\.cargo\bin. Install Rust from https://rustup.rs first."
}

$cargoPrefix = @()
if ($Toolchain) { $cargoPrefix += "+$Toolchain" }

# --- 1. Build the cdylib ----------------------------------------------------

$cargoArgs = $cargoPrefix + @('build', '--manifest-path', (Join-Path $repoRoot 'Cargo.toml'), '-p', 'libcore_ffi')
if ($Profile -eq 'release') { $cargoArgs += '--release' }

Write-Host "> cargo $($cargoArgs -join ' ')"
$code = Invoke-Native $cargo $cargoArgs
if ($code -ne 0) {
    throw "cargo build failed with exit code $code. If the linker is missing, retry with -Toolchain stable-x86_64-pc-windows-gnu."
}

if (-not (Test-Path $library)) {
    throw "Expected the library at $library but it is not there. Note that CARGO_TARGET_DIR is not honoured by this script."
}
Write-Host "Library: $library"

# --- 2. Regenerate the C# bindings ------------------------------------------

if (-not $NoBindings) {
    $bindgen = Resolve-Tool 'uniffi-bindgen-cs'
    if (-not $bindgen) {
        throw @"
uniffi-bindgen-cs was not found. Install the version that targets UniFFI 0.31:

    cargo install uniffi-bindgen-cs --git https://github.com/NordSecurity/uniffi-bindgen-cs --tag v0.11.1+v0.31.0
"@
    }

    New-Item -ItemType Directory -Force $bindingsDir | Out-Null

    Write-Host "> uniffi-bindgen-cs --library $library --out-dir $bindingsDir"
    $code = Invoke-Native $bindgen @('--library', $library, '--out-dir', $bindingsDir, '--config', $configFile, '--no-format')
    if ($code -ne 0) { throw "uniffi-bindgen-cs failed with exit code $code" }

    Write-Host "Bindings: $(Join-Path $bindingsDir 'core_ffi.cs')"
}

# --- 3. Optionally build the C# wrapper -------------------------------------

if ($BuildWrapper) {
    $dotnet = Resolve-Tool 'dotnet'
    if (-not $dotnet) { throw 'dotnet was not found; install the .NET SDK to build the wrapper assembly.' }

    $project = Join-Path $repoRoot 'bindings/csharp/AirBridge.Core.csproj'
    Write-Host "> dotnet build $project"
    $code = Invoke-Native $dotnet @('build', $project, '-c', 'Release')
    if ($code -ne 0) { throw "dotnet build failed with exit code $code" }
}

Write-Host ''
Write-Host 'Done.'
