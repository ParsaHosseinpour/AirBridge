<#
.SYNOPSIS
    Cross-compiles the AirBridge FFI library for Android, regenerates the
    Kotlin bindings, and optionally assembles the AAR.

.DESCRIPTION
    Steps, in order:

      1. build a host copy of the library for the bindings generator;
      2. cross-compile for every requested ABI with cargo-ndk, writing the
         .so files straight into android/libcore-ffi/src/main/jniLibs/<abi>/;
      3. regenerate the Kotlin bindings into target/bindings/kotlin;
      4. run `:libcore-ffi:assembleRelease` (or the debug variant) through the
         Gradle wrapper or a Gradle on PATH.

    Prerequisites: the Rust Android targets (the script adds them through
    rustup when available), cargo-ndk, an Android SDK with an NDK
    (ANDROID_NDK_HOME / ANDROID_NDK_ROOT, or -NdkHome), and Gradle.

.PARAMETER Profile
    Cargo profile: 'release' (default) or 'debug'.

.PARAMETER Toolchain
    Cargo toolchain, e.g. 'stable-x86_64-pc-windows-gnu'. Defaults to
    $env:AIRBRIDGE_TOOLCHAIN, or the machine default when unset.

.PARAMETER Abis
    Android ABIs to build. Defaults to all four supported ABIs.

.PARAMETER ApiLevel
    Android API level passed to cargo-ndk as --platform. Defaults to the
    module's minSdk (24).

.PARAMETER NdkHome
    Android NDK path. Defaults to $env:ANDROID_NDK_HOME; cargo-ndk also reads
    ANDROID_NDK_ROOT and ANDROID_HOME.

.PARAMETER NoAar
    Skip Gradle: cross-compile and generate bindings only.

.EXAMPLE
    pwsh scripts/build-android.ps1

.EXAMPLE
    pwsh scripts/build-android.ps1 -Profile debug -Abis arm64-v8a -NoAar
#>
[CmdletBinding()]
param(
    [ValidateSet('debug', 'release')]
    [string] $Profile = 'release',

    [string] $Toolchain = $env:AIRBRIDGE_TOOLCHAIN,

    [ValidateSet('arm64-v8a', 'armeabi-v7a', 'x86_64', 'x86')]
    [string[]] $Abis = @('arm64-v8a', 'armeabi-v7a', 'x86_64', 'x86'),

    [int] $ApiLevel = 24,

    [string] $NdkHome = $env:ANDROID_NDK_HOME,

    [switch] $NoAar
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repoRoot = Split-Path -Parent $PSScriptRoot
$androidDir = Join-Path $repoRoot 'android'
$jniLibsDir = Join-Path $androidDir 'libcore-ffi/src/main/jniLibs'
$bindingsDir = Join-Path $repoRoot 'target/bindings/kotlin'
$configFile = Join-Path $repoRoot 'crates/libcore_ffi/uniffi.toml'
$profileDir = if ($Profile -eq 'release') { 'release' } else { 'debug' }

$abiTargets = @{
    'arm64-v8a'   = 'aarch64-linux-android'
    'armeabi-v7a' = 'armv7-linux-androideabi'
    'x86_64'      = 'x86_64-linux-android'
    'x86'         = 'i686-linux-android'
}

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

# --- 0. Prerequisites -------------------------------------------------------

$ndkVersionCode = Invoke-Native $cargo ($cargoPrefix + @('ndk', '--version'))
if ($ndkVersionCode -ne 0) {
    throw 'cargo-ndk was not found. Install it with: cargo install cargo-ndk --locked'
}

if ($NdkHome) { $env:ANDROID_NDK_HOME = $NdkHome }
if (-not $env:ANDROID_NDK_HOME -and -not $env:ANDROID_NDK_ROOT -and -not $env:ANDROID_HOME) {
    Write-Warning 'No ANDROID_NDK_HOME, ANDROID_NDK_ROOT or ANDROID_HOME is set; cargo-ndk will fail unless it can locate the NDK.'
}

$rustup = Resolve-Tool 'rustup'
if ($rustup) {
    $targets = $Abis | ForEach-Object { $abiTargets[$_] }
    Write-Host "> rustup target add $($targets -join ' ')"
    $code = Invoke-Native $rustup ($cargoPrefix + @('target', 'add') + $targets)
    if ($code -ne 0) { throw "rustup target add failed with exit code $code" }
} else {
    Write-Warning 'rustup was not found; make sure the Android targets are installed for your toolchain.'
}

$cargoProfileArgs = if ($Profile -eq 'release') { @('--release') } else { @() }

# --- 1. Host library for the bindings generator -----------------------------

Write-Host '> cargo build -p libcore_ffi (host)'
$code = Invoke-Native $cargo ($cargoPrefix + @('build', '--manifest-path', (Join-Path $repoRoot 'Cargo.toml'), '-p', 'libcore_ffi') + $cargoProfileArgs)
if ($code -ne 0) { throw "the host build failed with exit code $code" }

$hostLibrary = Join-Path $repoRoot "target/$profileDir/core_ffi.dll"
if (-not (Test-Path $hostLibrary)) {
    throw "Expected the host library at $hostLibrary but it is not there. Note that CARGO_TARGET_DIR is not honoured by this script."
}

# --- 2. Cross-compile for Android -------------------------------------------

New-Item -ItemType Directory -Force $jniLibsDir | Out-Null

$ndkArgs = @('ndk')
foreach ($abi in $Abis) { $ndkArgs += @('-t', $abi) }
$ndkArgs += @('-o', $jniLibsDir, '--platform', "$ApiLevel", 'build', '--package', 'libcore_ffi')
$ndkArgs += $cargoProfileArgs

Write-Host "> cargo $($cargoPrefix -join ' ') $($ndkArgs -join ' ')"
$code = Invoke-Native $cargo ($cargoPrefix + $ndkArgs)
if ($code -ne 0) { throw "cargo ndk failed with exit code $code" }

foreach ($abi in $Abis) {
    $so = Join-Path $jniLibsDir "$abi/libcore_ffi.so"
    if (-not (Test-Path $so)) { throw "Expected $so; cargo-ndk did not produce it." }
    Write-Host "JNI library: $so"
}

# --- 3. Kotlin bindings -----------------------------------------------------

New-Item -ItemType Directory -Force $bindingsDir | Out-Null

Write-Host '> uniffi-bindgen generate --language kotlin (host library)'
$bindgenArgs = $cargoPrefix + @(
    'run', '--manifest-path', (Join-Path $repoRoot 'Cargo.toml'),
    '-p', 'libcore_ffi', '--features', 'cli', '--bin', 'uniffi-bindgen', '--',
    'generate', '--library', $hostLibrary, '--language', 'kotlin',
    '--out-dir', $bindingsDir, '--config', $configFile, '--no-format'
)
$code = Invoke-Native $cargo $bindgenArgs
if ($code -ne 0) { throw "uniffi-bindgen failed with exit code $code" }

$generated = Join-Path $bindingsDir 'com/airbridge/core/core_ffi.kt'
if (-not (Test-Path $generated)) { throw "Expected the Kotlin bindings at $generated but they are not there." }

# UniFFI 0.31 emits `val message: String` inside FfiException subclasses next
# to an override of Throwable.message; Kotlin 2.2+ rejects that combination.
# The Gradle module patches its copy as well, but doing it here keeps -NoAar
# output compilable on its own.
$bindingsText = [System.IO.File]::ReadAllText($generated)
$bindingsPatched = $bindingsText -replace '(?<!override )val `message`: kotlin\.String(\s*\)\s*:\s*FfiException\(\))', 'override val `message`: kotlin.String$1'
$bindingsPatched = $bindingsPatched -replace '\s*override val message\s*\r?\n\s*get\(\) = "message=\$\{ `message` \}"', ''
if ($bindingsPatched -ne $bindingsText) {
    [System.IO.File]::WriteAllText($generated, $bindingsPatched, [System.Text.UTF8Encoding]::new($false))
    Write-Host 'Normalised the generated error classes for Kotlin 2.2+.'
}
Write-Host "Bindings: $generated"

# --- 4. Assemble the AAR ----------------------------------------------------

if ($NoAar) {
    Write-Host 'Skipping the Gradle build (-NoAar).'
    Write-Host 'Done.'
    exit 0
}

$wrapper = Join-Path $androidDir 'gradlew.bat'
$gradle = if (Test-Path $wrapper) { $wrapper } else { Resolve-Tool 'gradle' }
if (-not $gradle) {
    throw 'Neither android/gradlew.bat nor gradle on PATH was found. Create the wrapper with `gradle wrapper` inside android/, or install Gradle.'
}

$task = if ($Profile -eq 'release') { ':libcore-ffi:assembleRelease' } else { ':libcore-ffi:assembleDebug' }
Write-Host "> $gradle -p $androidDir $task"
$code = Invoke-Native $gradle @('-p', $androidDir, $task)
if ($code -ne 0) { throw "the Gradle build failed with exit code $code" }

$variant = if ($Profile -eq 'release') { 'release' } else { 'debug' }
$aar = Join-Path $androidDir "libcore-ffi/build/outputs/aar/libcore-ffi-$variant.aar"
if (-not (Test-Path $aar)) { throw "Expected the AAR at $aar but it is not there." }

Write-Host ''
Write-Host "AAR: $aar"
