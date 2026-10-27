#!/bin/sh
set -eu
SPOTIYA_ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
SPOTIYA_CACHE=${SPOTIYA_CACHE:-"${XDG_CACHE_HOME:-$HOME/.cache}/spotiya"}
SPOTIYA_TOOLCHAIN=${SPOTIYA_TOOLCHAIN:-"$SPOTIYA_CACHE/gcc-linaro-6.5.0-2018.12-x86_64_arm-linux-gnueabihf"}
SPOTIYA_ALSA_SYSROOT=${SPOTIYA_ALSA_SYSROOT:-"$SPOTIYA_CACHE/alsa-sysroot-glibc223"}
if [ ! -x "$SPOTIYA_TOOLCHAIN/bin/arm-linux-gnueabihf-gcc" ] || [ ! -f "$SPOTIYA_ALSA_SYSROOT/.spotiya-alsa-glibc223-no-versioned" ]; then
    echo 'Run scripts/bootstrap-arm.sh first' >&2
    exit 1
fi
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER="$SPOTIYA_TOOLCHAIN/bin/arm-linux-gnueabihf-gcc"
export CC_armv7_unknown_linux_gnueabihf="$SPOTIYA_TOOLCHAIN/bin/arm-linux-gnueabihf-gcc"
export AR_armv7_unknown_linux_gnueabihf="$SPOTIYA_TOOLCHAIN/bin/arm-linux-gnueabihf-ar"
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_SYSROOT_DIR="$SPOTIYA_ALSA_SYSROOT"
export PKG_CONFIG_LIBDIR="$SPOTIYA_ALSA_SYSROOT/usr/lib/pkgconfig"
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_RUSTFLAGS="-C target-cpu=${SPOTIYA_TARGET_CPU:-cortex-a53} -C link-arg=-Wl,--gc-sections -C link-arg=-Wl,-z,max-page-size=4096"
cd "$SPOTIYA_ROOT"
cargo build --release --target armv7-unknown-linux-gnueabihf
"$SPOTIYA_ROOT/scripts/check-arm-abi.sh" "${CARGO_TARGET_DIR:-target}/armv7-unknown-linux-gnueabihf/release/soundiya"
