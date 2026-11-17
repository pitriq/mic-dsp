#!/bin/sh
# Builds the "Francisco's Mic" driver from the unmodified BlackHole source into driver/build.
# Configuration is preprocessor definitions only (see PROVENANCE.md).
set -eu
cd "$(dirname "$0")"

BUNDLE_ID="local.mic-dsp.driver"
# Fixed so reinstalls replace the same plug-in. Differs from upstream's factory UUID.
FACTORY_UUID="6f3c1d52-8a4e-4b6f-9c2d-5e7a1b3c9d01"
OUT="$PWD/build"

# Visible device: input only. Device 2: hidden, output only (the daemon's feed).
# 2 channels: apps treat a stereo virtual input as a normal mic; the daemon writes both.
DEFS='$(inherited)
kDriver_Name=\"MicDSP\"
kHas_Driver_Name_Format=false
kPlugIn_BundleID=\"'"$BUNDLE_ID"'\"
kDevice_Name=\"Francisco\'"'"'s\ Mic\"
kDevice2_Name=\"Francisco\'"'"'s\ Mic\ Feed\"
kManufacturer_Name=\"mic-dsp\"
kDevice_IsHidden=false
kDevice_HasInput=true
kDevice_HasOutput=false
kDevice2_IsHidden=true
kDevice2_HasInput=false
kDevice2_HasOutput=true
kNumber_Of_Channels=2
kSampleRates=48000
kEnableVolumeControl=false
kCanBeDefaultDevice=true
kCanBeDefaultSystemDevice=false'

xcodebuild -quiet \
  -project BlackHole/BlackHole.xcodeproj \
  -configuration Release \
  -target BlackHole \
  CONFIGURATION_BUILD_DIR="$OUT" \
  SYMROOT="$OUT/sym" OBJROOT="$OUT/obj" \
  PRODUCT_BUNDLE_IDENTIFIER="$BUNDLE_ID" \
  DEVELOPMENT_TEAM= CODE_SIGNING_ALLOWED=NO \
  GCC_PREPROCESSOR_DEFINITIONS="$DEFS"

DRIVER="$OUT/MicDSP.driver"
if [ -e "$DRIVER" ]; then trash "$DRIVER"; fi
mv "$OUT/BlackHole.driver" "$DRIVER"
/usr/libexec/PlistBuddy -c "Delete :CFPlugInFactories:e395c745-4eea-4d94-bb92-46224221047c" \
  -c "Add :CFPlugInFactories:$FACTORY_UUID string BlackHole_Create" \
  -c "Delete :CFPlugInTypes:443ABAB8-E7B3-491A-B985-BEB9187030DB" \
  -c "Add :CFPlugInTypes:443ABAB8-E7B3-491A-B985-BEB9187030DB array" \
  -c "Add :CFPlugInTypes:443ABAB8-E7B3-491A-B985-BEB9187030DB:0 string $FACTORY_UUID" \
  "$DRIVER/Contents/Info.plist"

# Ad-hoc signature; pass SIGN_IDENTITY="Francisco Local Code Signing" if coreaudiod rejects it.
codesign --force --sign "${SIGN_IDENTITY:--}" "$DRIVER"
echo "$DRIVER"
