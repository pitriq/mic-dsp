#!/bin/sh
# Moves the installed drivers out of the HAL folder and restarts coreaudiod. Needs sudo.
# If Francisco's Speakers was the default output, macOS picks another one.
set -eu
HAL="/Library/Audio/Plug-Ins/HAL"
for driver in MicDSP.driver MicDSPSpeakers.driver; do
  if [ -e "$HAL/$driver" ]; then sudo mv "$HAL/$driver" "$(mktemp -d)/"; fi
done
sudo killall coreaudiod
