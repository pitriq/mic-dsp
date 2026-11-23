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
SERVICE="gui/$(id -u)/local.mic-dsp.micd"
launchctl bootout "$SERVICE" 2>/dev/null || true
# bootout returns before micd has stopped; bootstrap fails with error 5 until it has.
tries=0
while launchctl print "$SERVICE" >/dev/null 2>&1; do
  tries=$((tries + 1))
  [ "$tries" -le 50 ] || { echo "$SERVICE did not unload" >&2; exit 1; }
  sleep 0.2
done
launchctl bootstrap "gui/$(id -u)" "$PLIST"
launchctl print "$SERVICE" | grep -E "state|pid"
