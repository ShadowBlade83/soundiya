#!/bin/sh
set -eu

SPOTIYA_CACHE=${SPOTIYA_CACHE:-"${XDG_CACHE_HOME:-$HOME/.cache}/spotiya"}
SPOTIYA_TOOLCHAIN_NAME=gcc-linaro-6.5.0-2018.12-x86_64_arm-linux-gnueabihf
SPOTIYA_TOOLCHAIN=${SPOTIYA_TOOLCHAIN:-"$SPOTIYA_CACHE/$SPOTIYA_TOOLCHAIN_NAME"}
SPOTIYA_ALSA_SYSROOT=${SPOTIYA_ALSA_SYSROOT:-"$SPOTIYA_CACHE/alsa-sysroot-glibc223"}
SPOTIYA_JOBS=${SPOTIYA_JOBS:-4}
mkdir -p "$SPOTIYA_CACHE/downloads"

download() {
    archive=$1
    url=$2
    checksum=$3
    if [ ! -f "$archive" ]; then
        curl --fail --location --retry 3 --output "$archive.part" "$url"
        mv "$archive.part" "$archive"
    fi
    printf '%s  %s\n' "$checksum" "$archive" | sha256sum --check --status
}

if [ ! -x "$SPOTIYA_TOOLCHAIN/bin/arm-linux-gnueabihf-gcc" ]; then
    download "$SPOTIYA_CACHE/downloads/$SPOTIYA_TOOLCHAIN_NAME.tar.xz" \
        "https://developer.arm.com/-/cdn-downloads/permalink/legacy-linaro-gnu-toolchains/6.5-2018.12/$SPOTIYA_TOOLCHAIN_NAME.tar.xz" \
        a84b1239765617f3192014cb259c05e32771a64a4c831cf89577fcd87918eacd
    mkdir -p "$SPOTIYA_TOOLCHAIN"
    tar -xJf "$SPOTIYA_CACHE/downloads/$SPOTIYA_TOOLCHAIN_NAME.tar.xz" --strip-components=1 -C "$SPOTIYA_TOOLCHAIN"
fi

if [ ! -f "$SPOTIYA_ALSA_SYSROOT/.spotiya-alsa-glibc223-no-versioned" ]; then
    download "$SPOTIYA_CACHE/downloads/alsa-lib-1.1.8.tar.bz2" \
        https://www.alsa-project.org/files/pub/lib/alsa-lib-1.1.8.tar.bz2 \
        3cdc3a93a6427a26d8efab4ada2152e64dd89140d981f6ffa003e85be707aedf
    work=$(mktemp -d "$SPOTIYA_CACHE/alsa-build.XXXXXX")
    trap 'rm -rf "$work"' EXIT HUP INT TERM
    tar -xjf "$SPOTIYA_CACHE/downloads/alsa-lib-1.1.8.tar.bz2" -C "$work"
    (
        cd "$work/alsa-lib-1.1.8"
        CC="$SPOTIYA_TOOLCHAIN/bin/arm-linux-gnueabihf-gcc" \
        AR="$SPOTIYA_TOOLCHAIN/bin/arm-linux-gnueabihf-ar" \
        ./configure --host=arm-linux --prefix=/usr --disable-python --disable-static --without-versioned
        make -j "$SPOTIYA_JOBS"
        make DESTDIR="$SPOTIYA_ALSA_SYSROOT" install
    )
    touch "$SPOTIYA_ALSA_SYSROOT/.spotiya-alsa-glibc223-no-versioned"
    rm -rf "$work"
    trap - EXIT HUP INT TERM
fi

printf 'Toolchain: %s\nALSA sysroot: %s\n' "$SPOTIYA_TOOLCHAIN" "$SPOTIYA_ALSA_SYSROOT"
