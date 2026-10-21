#!/usr/bin/env bash
#
# Builds the AirBridge host FFI library and regenerates the C# bindings.
# On Windows this produces core_ffi.dll; on Linux/macOS a .so/.dylib that CI
# can use to check that the bindings still generate.
#
# Usage: scripts/build-windows.sh [--debug] [--toolchain <name>] [--no-bindings] [--build-wrapper]

set -euo pipefail

usage() {
    sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'
}

profile=release
toolchain=${AIRBRIDGE_TOOLCHAIN:-}
no_bindings=0
build_wrapper=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        -d|--debug)         profile=debug ;;
        -r|--release)       profile=release ;;
        --toolchain)        toolchain=$2; shift ;;
        --no-bindings)      no_bindings=1 ;;
        --build-wrapper)    build_wrapper=1 ;;
        -h|--help)          usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage; exit 2 ;;
    esac
    shift
done

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
config_file="$repo_root/crates/libcore_ffi/uniffi.toml"
bindings_dir="$repo_root/target/bindings/csharp"

cargo_bin=$(command -v cargo || true)
if [[ -z $cargo_bin && -x $HOME/.cargo/bin/cargo ]]; then
    cargo_bin=$HOME/.cargo/bin/cargo
fi
if [[ -z $cargo_bin ]]; then
    echo "cargo was not found. Install Rust from https://rustup.rs first." >&2
    exit 1
fi

# --- 1. Build the host cdylib -----------------------------------------------

cargo_args=()
[[ -n $toolchain ]] && cargo_args+=("+$toolchain")
cargo_args+=(build --manifest-path "$repo_root/Cargo.toml" -p libcore_ffi)
[[ $profile == release ]] && cargo_args+=(--release)

echo "> cargo ${cargo_args[*]}"
"$cargo_bin" "${cargo_args[@]}"

case "$(uname -s)" in
    MINGW*|MSYS*|CYGWIN*) library="$repo_root/target/$profile/core_ffi.dll" ;;
    Darwin)               library="$repo_root/target/$profile/libcore_ffi.dylib" ;;
    *)                    library="$repo_root/target/$profile/libcore_ffi.so" ;;
esac

if [[ ! -f $library ]]; then
    echo "Expected the library at $library but it is not there (CARGO_TARGET_DIR is not honoured by this script)." >&2
    exit 1
fi
echo "Library: $library"

# --- 2. Regenerate the C# bindings -----------------------------------------

if [[ $no_bindings -eq 0 ]]; then
    bindgen_cs=$(command -v uniffi-bindgen-cs || true)
    if [[ -z $bindgen_cs && -x $HOME/.cargo/bin/uniffi-bindgen-cs ]]; then
        bindgen_cs=$HOME/.cargo/bin/uniffi-bindgen-cs
    fi
    if [[ -z $bindgen_cs ]]; then
        cat >&2 <<'EOF'
uniffi-bindgen-cs was not found. Install the version that targets UniFFI 0.31:

    cargo install uniffi-bindgen-cs --git https://github.com/NordSecurity/uniffi-bindgen-cs --tag v0.11.1+v0.31.0
EOF
        exit 1
    fi

    mkdir -p "$bindings_dir"
    echo "> uniffi-bindgen-cs --library $library --out-dir $bindings_dir"
    "$bindgen_cs" --library "$library" --out-dir "$bindings_dir" --config "$config_file" --no-format
    echo "Bindings: $bindings_dir/core_ffi.cs"
fi

# --- 3. Optionally build the C# wrapper ------------------------------------

if [[ $build_wrapper -eq 1 ]]; then
    if ! command -v dotnet >/dev/null 2>&1; then
        echo "dotnet was not found; install the .NET SDK to build the wrapper assembly." >&2
        exit 1
    fi
    echo "> dotnet build bindings/csharp/AirBridge.Core.csproj"
    dotnet build "$repo_root/bindings/csharp/AirBridge.Core.csproj" -c Release
fi

echo
echo "Done."
