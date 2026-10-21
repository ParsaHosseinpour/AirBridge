#!/usr/bin/env bash
#
# Cross-compiles the AirBridge FFI library for Android, regenerates the Kotlin
# bindings, and optionally assembles the AAR.
#
# Prerequisites: the Rust Android targets (added through rustup when
# available), cargo-ndk, an Android SDK with an NDK (ANDROID_NDK_HOME /
# ANDROID_NDK_ROOT / ANDROID_HOME), and either android/gradlew or a gradle on
# PATH.
#
# Usage: scripts/build-android.sh [--debug] [--toolchain <name>]
#            [--abis arm64-v8a,armeabi-v7a,x86_64,x86] [--api-level 24]
#            [--ndk-home <path>] [--no-aar]

set -euo pipefail

usage() {
    sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'
}

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
android_dir="$repo_root/android"
jni_libs_dir="$android_dir/libcore-ffi/src/main/jniLibs"
bindings_dir="$repo_root/target/bindings/kotlin"
config_file="$repo_root/crates/libcore_ffi/uniffi.toml"

profile=release
toolchain=${AIRBRIDGE_TOOLCHAIN:-}
abis=("arm64-v8a" "armeabi-v7a" "x86_64" "x86")
api_level=24
ndk_home=${ANDROID_NDK_HOME:-}
no_aar=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        -d|--debug)       profile=debug ;;
        -r|--release)     profile=release ;;
        --toolchain)      toolchain=$2; shift ;;
        --abis)           IFS=',' read -r -a abis <<< "$2"; shift ;;
        --api-level)      api_level=$2; shift ;;
        --ndk-home)       ndk_home=$2; shift ;;
        --no-aar)         no_aar=1 ;;
        -h|--help)        usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage; exit 2 ;;
    esac
    shift
done

abi_target() {
    case "$1" in
        arm64-v8a)   echo aarch64-linux-android ;;
        armeabi-v7a) echo armv7-linux-androideabi ;;
        x86_64)      echo x86_64-linux-android ;;
        x86)         echo i686-linux-android ;;
        *) echo "unknown ABI: $1" >&2; exit 2 ;;
    esac
}

cargo_bin=$(command -v cargo || true)
if [[ -z $cargo_bin && -x $HOME/.cargo/bin/cargo ]]; then
    cargo_bin=$HOME/.cargo/bin/cargo
fi
if [[ -z $cargo_bin ]]; then
    echo "cargo was not found. Install Rust from https://rustup.rs first." >&2
    exit 1
fi

cargo_prefix=()
[[ -n $toolchain ]] && cargo_prefix+=("+$toolchain")

cargo_profile_args=()
[[ $profile == release ]] && cargo_profile_args+=(--release)

# --- 0. Prerequisites -------------------------------------------------------

if ! "$cargo_bin" "${cargo_prefix[@]}" ndk --version >/dev/null 2>&1; then
    echo 'cargo-ndk was not found. Install it with: cargo install cargo-ndk --locked' >&2
    exit 1
fi

if [[ -n $ndk_home ]]; then
    export ANDROID_NDK_HOME=$ndk_home
fi
if [[ -z ${ANDROID_NDK_HOME:-} && -z ${ANDROID_NDK_ROOT:-} && -z ${ANDROID_HOME:-} ]]; then
    echo 'warning: no ANDROID_NDK_HOME, ANDROID_NDK_ROOT or ANDROID_HOME is set; cargo-ndk may fail to locate the NDK.' >&2
fi

if command -v rustup >/dev/null 2>&1; then
    targets=()
    for abi in "${abis[@]}"; do targets+=("$(abi_target "$abi")"); done
    echo "> rustup target add ${targets[*]}"
    rustup "${cargo_prefix[@]}" target add "${targets[@]}"
else
    echo 'warning: rustup was not found; make sure the Android targets are installed for your toolchain.' >&2
fi

# --- 1. Host library for the bindings generator -----------------------------

echo '> cargo build -p libcore_ffi (host)'
"$cargo_bin" "${cargo_prefix[@]}" build --manifest-path "$repo_root/Cargo.toml" -p libcore_ffi "${cargo_profile_args[@]}"

case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) host_library="$repo_root/target/$profile/core_ffi.dll" ;;
    Darwin)               host_library="$repo_root/target/$profile/libcore_ffi.dylib" ;;
    *)                    host_library="$repo_root/target/$profile/libcore_ffi.so" ;;
esac

if [[ ! -f $host_library ]]; then
    echo "Expected the host library at $host_library but it is not there (CARGO_TARGET_DIR is not honoured by this script)." >&2
    exit 1
fi

# --- 2. Cross-compile for Android -------------------------------------------

mkdir -p "$jni_libs_dir"

ndk_args=(ndk)
for abi in "${abis[@]}"; do ndk_args+=(-t "$abi"); done
ndk_args+=(-o "$jni_libs_dir" --platform "$api_level" build --package libcore_ffi)
ndk_args+=("${cargo_profile_args[@]}")

echo "> cargo ${cargo_prefix[*]} ${ndk_args[*]}"
"$cargo_bin" "${cargo_prefix[@]}" "${ndk_args[@]}"

for abi in "${abis[@]}"; do
    so="$jni_libs_dir/$abi/libcore_ffi.so"
    if [[ ! -f $so ]]; then
        echo "Expected $so; cargo-ndk did not produce it." >&2
        exit 1
    fi
    echo "JNI library: $so"
done

# --- 3. Kotlin bindings -----------------------------------------------------

mkdir -p "$bindings_dir"
echo '> uniffi-bindgen generate --language kotlin (host library)'
"$cargo_bin" "${cargo_prefix[@]}" run --manifest-path "$repo_root/Cargo.toml" \
    -p libcore_ffi --features cli --bin uniffi-bindgen -- \
    generate --library "$host_library" --language kotlin \
    --out-dir "$bindings_dir" --config "$config_file" --no-format

generated="$bindings_dir/com/airbridge/core/core_ffi.kt"
if [[ ! -f $generated ]]; then
    echo "Expected the Kotlin bindings at $generated but they are not there." >&2
    exit 1
fi

# UniFFI 0.31 emits `val message: String` inside FfiException subclasses next
# to an override of Throwable.message; Kotlin 2.2+ rejects that combination.
# The Gradle module patches its copy as well, but doing it here keeps
# --no-aar output compilable on its own.
if command -v perl >/dev/null 2>&1; then
    perl -0pi -e 's/(?<!override )val `message`: kotlin\.String(\s*\)\s*:\s*FfiException\(\))/override val `message`: kotlin.String$1/g; s/\s*override val message\s*\r?\n\s*get\(\) = "message=\$\{ `message` \}"//g' "$generated"
    echo 'Normalised the generated error classes for Kotlin 2.2+.'
else
    echo 'Warning: perl not found; the Kotlin bindings may not compile on Kotlin 2.2+.' >&2
fi
echo "Bindings: $generated"

# --- 4. Assemble the AAR ----------------------------------------------------

if [[ $no_aar -eq 1 ]]; then
    echo 'Skipping the Gradle build (--no-aar).'
    echo 'Done.'
    exit 0
fi

if [[ -x $android_dir/gradlew ]]; then
    gradle_cmd=("$android_dir/gradlew")
elif command -v gradle >/dev/null 2>&1; then
    gradle_cmd=(gradle)
else
    echo 'Neither android/gradlew nor gradle on PATH was found. Create the wrapper with `gradle wrapper` inside android/, or install Gradle.' >&2
    exit 1
fi

task=:libcore-ffi:assembleRelease
variant=release
if [[ $profile == debug ]]; then
    task=:libcore-ffi:assembleDebug
    variant=debug
fi

echo "> ${gradle_cmd[*]} -p $android_dir $task"
"${gradle_cmd[@]}" -p "$android_dir" "$task"

aar="$android_dir/libcore-ffi/build/outputs/aar/libcore-ffi-$variant.aar"
if [[ ! -f $aar ]]; then
    echo "Expected the AAR at $aar but it is not there." >&2
    exit 1
fi

echo
echo "AAR: $aar"
