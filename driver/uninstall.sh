#!/bin/sh
# Moves the installed driver out of the HAL folder and restarts coreaudiod. Needs sudo.
set -eu
DST="/Library/Audio/Plug-Ins/HAL/MicDSP.driver"
if [ -e "$DST" ]; then sudo mv "$DST" "$(mktemp -d)/"; fi
sudo killall coreaudiod
