#!/bin/sh
# Installs driver/build/MicDSP.driver and MicDSPSpeakers.driver and restarts coreaudiod, which
# cuts all audio briefly. Needs sudo. Run ./build.sh first.
set -eu
cd "$(dirname "$0")"
HAL="/Library/Audio/Plug-Ins/HAL"
DRIVERS="MicDSP.driver MicDSPSpeakers.driver"
for driver in $DRIVERS; do
  [ -d "build/$driver" ] || { echo "missing build/$driver; run ./build.sh" >&2; exit 1; }
done
for driver in $DRIVERS; do
  if [ -e "$HAL/$driver" ]; then sudo mv "$HAL/$driver" "$(mktemp -d)/"; fi
  sudo cp -R "build/$driver" "$HAL/$driver"
  sudo chown -R root:wheel "$HAL/$driver"
done
sudo killall coreaudiod
