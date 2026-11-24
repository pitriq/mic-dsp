#!/bin/sh
# Moves the installed drivers out of the HAL folder and restarts coreaudiod. Needs sudo.
# Before running it, set the Mac's output back to the Komplete and delete the Multi-Output
# Device in Audio MIDI Setup: Francisco's Speakers is one of its two devices.
set -eu
HAL="/Library/Audio/Plug-Ins/HAL"
for driver in MicDSP.driver MicDSPSpeakers.driver; do
  if [ -e "$HAL/$driver" ]; then sudo mv "$HAL/$driver" "$(mktemp -d)/"; fi
done
sudo killall coreaudiod
