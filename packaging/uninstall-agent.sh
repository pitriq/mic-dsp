#!/bin/sh
# Stops micd and removes its LaunchAgent. Leaves the app and the logs.
set -eu
PLIST="$HOME/Library/LaunchAgents/local.mic-dsp.micd.plist"
launchctl bootout "gui/$(id -u)/local.mic-dsp.micd" 2>/dev/null || true
if [ -e "$PLIST" ]; then trash "$PLIST"; fi
