#!/bin/sh
# Builds micd in release, wraps it in ~/Applications/mic-dsp.app and signs it with the
# self-signed "Francisco Local Code Signing" identity, so the mic permission survives rebuilds.
set -eu
cd "$(dirname "$0")/.."
APP="$HOME/Applications/mic-dsp.app"
IDENTITY="${SIGN_IDENTITY:-Francisco Local Code Signing}"

cargo build --release -p micd
STAGE="target/mic-dsp.app"
if [ -e "$STAGE" ]; then trash "$STAGE"; fi
mkdir -p "$STAGE/Contents/MacOS"
cp packaging/Info.plist "$STAGE/Contents/Info.plist"
cp target/release/micd "$STAGE/Contents/MacOS/micd"
codesign --force --sign "$IDENTITY" --identifier local.mic-dsp.micd "$STAGE"
codesign --verify --strict "$STAGE"

mkdir -p "$HOME/Applications"
if [ -e "$APP" ]; then trash "$APP"; fi
cp -R "$STAGE" "$APP"
echo "$APP"
