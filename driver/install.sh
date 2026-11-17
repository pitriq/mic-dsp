#!/bin/sh
# Installs driver/build/MicDSP.driver and restarts coreaudiod, which cuts all audio briefly.
# Needs sudo. Run ./build.sh first.
set -eu
cd "$(dirname "$0")"
SRC="build/MicDSP.driver"
DST="/Library/Audio/Plug-Ins/HAL/MicDSP.driver"
[ -d "$SRC" ] || { echo "missing $SRC; run ./build.sh" >&2; exit 1; }
if [ -e "$DST" ]; then sudo mv "$DST" "$(mktemp -d)/"; fi
sudo cp -R "$SRC" "$DST"
sudo chown -R root:wheel "$DST"
sudo killall coreaudiod
