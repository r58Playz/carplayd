#!/usr/bin/env bash
set -euo pipefail

usage() {
    echo "Usage: $0 [--build-only] [user@host:path]"
}

build_only=false
if [[ ${1:-} == --build-only ]]; then
    build_only=true
    shift
fi
if [[ ${1:-} == --help || ${1:-} == -h ]]; then
    usage
    exit 0
fi
if (( $# > 1 )) || [[ ${1:-} == -* ]]; then
    usage >&2
    exit 2
fi

default_destination='user@pi-carplayd.lan:~'
destination=${1:-${CARPLAYD_DESTINATION:-$default_destination}}
target=aarch64-unknown-linux-gnu
compiler=${CARPLAYD_LINKER:-aarch64-linux-gnu-gcc}
cd -- "$(dirname -- "${BASH_SOURCE[0]}")"

# Derive the sysroot from the same compiler Cargo uses to link the binary.
sysroot=${CARPLAYD_SYSROOT:-$("$compiler" -print-sysroot)}
include_dir=
for candidate in "$sysroot/usr/include" "$sysroot/include"; do
    if [[ -f $candidate/linux/videodev2.h ]]; then
        include_dir=$candidate
        break
    fi
done
if [[ -z $sysroot || -z $include_dir ]]; then
    echo "No linux/videodev2.h found in sysroot '$sysroot'; set CARPLAYD_SYSROOT." >&2
    exit 1
fi

export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER="$compiler"
# v4l2r expects the directory containing videodev2.h, not the file itself.
export V4L2R_VIDEODEV2_H_PATH="$include_dir/linux"
# Target-specific flags leave any host-side bindgen builds alone. The explicit
# include directory also supports this toolchain's sysroot/include layout.
clang_args=${BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_linux_gnu:-${BINDGEN_EXTRA_CLANG_ARGS:-}}
export BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_linux_gnu="$clang_args --target=$target --sysroot=\"$sysroot\" -isystem \"$include_dir\""

echo "Building $target with V4L2 headers from $include_dir/linux"
cargo build --release --locked --target "$target"

if [[ $build_only == false ]]; then
    binary=${CARGO_TARGET_DIR:-target}/$target/release/carplayd
    echo "Copying $binary to $destination"
    scp -o ConnectTimeout=10 -- "$binary" "$destination"
fi
