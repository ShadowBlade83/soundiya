#!/bin/sh
set -eu
SPOTIYA_CACHE=${SPOTIYA_CACHE:-"${XDG_CACHE_HOME:-$HOME/.cache}/spotiya"}
SPOTIYA_TOOLCHAIN=${SPOTIYA_TOOLCHAIN:-"$SPOTIYA_CACHE/gcc-linaro-6.5.0-2018.12-x86_64_arm-linux-gnueabihf"}
python3 - "$SPOTIYA_TOOLCHAIN/bin/arm-linux-gnueabihf-readelf" "$1" <<'PY'
import re
import subprocess
import sys

text = subprocess.check_output([sys.argv[1], "--version-info", sys.argv[2]], text=True)
versions = set(re.findall(r"\bGLIBC_[A-Za-z0-9_.]+", text))
unsupported = []
for version in versions:
    number = version.removeprefix("GLIBC_")
    if not re.fullmatch(r"\d+(?:\.\d+)+", number) or tuple(map(int, number.split("."))) > (2, 23):
        unsupported.append(version)
if unsupported:
    raise SystemExit("Mini 1 requires glibc <= 2.23; binary imports " + ", ".join(sorted(unsupported)))
if not versions:
    raise SystemExit("No glibc version requirements found in ARM binary")
dynamic = subprocess.check_output([sys.argv[1], "--dynamic", sys.argv[2]], text=True)
if not re.search(r"\(RPATH\).*\[/system/vendor/quasar/libs\]", dynamic) or "(RUNPATH)" in dynamic:
    raise SystemExit("ARM binary needs transitive RPATH for vendor ALSA plugin dependencies")
symbols = subprocess.check_output([sys.argv[1], "--dyn-syms", "--wide", sys.argv[2]], text=True)
legacy = re.findall(r"\bUND (snd_pcm_hw_params_(?:get_(?:buffer|period)_size(?:_min|_max)?|set_(?:buffer|period)_size_near))\b", symbols)
# get_format and get_rate stay imported on purpose: controls/alsa_compat.c calls whichever ABI
# the linker picked and normalises the answer.
if legacy:
    raise SystemExit("Unversioned ALSA calls select the obsolete Mini 1 ABI: " + ", ".join(sorted(legacy)))
print("ARM ABI verified: all glibc imports are compatible with 2.23")
PY
