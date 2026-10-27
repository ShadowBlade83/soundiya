#!/bin/sh
# Collects the ARMv7 build and the installer files into dist/soundiya-armv7/ (and a .tar.gz).
set -eu
ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
BIN="${CARGO_TARGET_DIR:-$ROOT/target}/armv7-unknown-linux-gnueabihf/release/soundiya"
[ -f "$BIN" ] || { echo 'Build first: scripts/build-arm.sh' >&2; exit 1; }
OUT="$ROOT/dist/soundiya-armv7"
rm -rf "$OUT"
mkdir -p "$OUT"
cp "$BIN" "$OUT/soundiya"
for f in install.sh run.sh start.sh stop.sh S99soundiya; do
    # The speaker's shell chokes on CRLF, which Windows checkouts may introduce.
    tr -d '\r' <"$ROOT/packaging/$f" >"$OUT/$f"
done
chmod 755 "$OUT/soundiya" "$OUT"/*.sh "$OUT/S99soundiya"
(cd "$OUT" && sha256sum soundiya install.sh run.sh start.sh stop.sh S99soundiya >SHA256SUMS)
(cd "$ROOT/dist" && tar -czf soundiya-armv7.tar.gz soundiya-armv7)
echo "Package: $OUT"
ls -l "$OUT"
