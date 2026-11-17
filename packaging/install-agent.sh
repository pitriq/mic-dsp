#!/bin/sh
# Installs and loads the LaunchAgent that keeps micd running. Run build-app.sh first.
set -eu
cd "$(dirname "$0")"
APP="$HOME/Applications/mic-dsp.app"
LOGS="$HOME/Library/Logs/mic-dsp"
PLIST="$HOME/Library/LaunchAgents/local.mic-dsp.micd.plist"
[ -x "$APP/Contents/MacOS/micd" ] || { echo "missing $APP; run build-app.sh" >&2; exit 1; }
mkdir -p "$LOGS" "$HOME/Library/LaunchAgents"
sed -e "s|@APP@|$APP|" -e "s|@LOGS@|$LOGS|" local.mic-dsp.micd.plist > "$PLIST"
launchctl bootout "gui/$(id -u)/local.mic-dsp.micd" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$PLIST"
launchctl print "gui/$(id -u)/local.mic-dsp.micd" | grep -E "state|pid"
