#!/bin/sh
# Builds two drivers from the unmodified BlackHole source into driver/build: MicDSP.driver
# ("Francisco's Mic") and MicDSPSpeakers.driver ("Francisco's Speakers").
# Configuration is preprocessor definitions only (see PROVENANCE.md).
set -eu
cd "$(dirname "$0")"
OUT="$PWD/build"

# build NAME BUNDLE_ID FACTORY_UUID, with the device definitions on stdin, written the way
# xcodebuild takes them. NAME is the driver name, which the device UIDs start with: NAME_UID for
# the visible device and NAME_2_UID for the hidden one. The factory UUID is fixed so reinstalls
# replace the same plug-in; each driver has its own, and neither is upstream's.
build() {
  name=$1
  bundle_id=$2
  factory_uuid=$3
  work="$OUT/work/$name"
  defs='$(inherited)
kDriver_Name=\"'"$name"'\"
kHas_Driver_Name_Format=false
kPlugIn_BundleID=\"'"$bundle_id"'\"
kManufacturer_Name=\"mic-dsp\"
kNumber_Of_Channels=2
kSampleRates=48000
'"$(cat)"

  xcodebuild -quiet \
    -project BlackHole/BlackHole.xcodeproj \
    -configuration Release \
    -target BlackHole \
    CONFIGURATION_BUILD_DIR="$work" \
    SYMROOT="$work/sym" OBJROOT="$work/obj" \
    PRODUCT_BUNDLE_IDENTIFIER="$bundle_id" \
    DEVELOPMENT_TEAM= CODE_SIGNING_ALLOWED=NO \
    GCC_PREPROCESSOR_DEFINITIONS="$defs"

  driver="$OUT/$name.driver"
  if [ -e "$driver" ]; then trash "$driver"; fi
  mv "$work/BlackHole.driver" "$driver"
  /usr/libexec/PlistBuddy -c "Delete :CFPlugInFactories:e395c745-4eea-4d94-bb92-46224221047c" \
    -c "Add :CFPlugInFactories:$factory_uuid string BlackHole_Create" \
    -c "Delete :CFPlugInTypes:443ABAB8-E7B3-491A-B985-BEB9187030DB" \
    -c "Add :CFPlugInTypes:443ABAB8-E7B3-491A-B985-BEB9187030DB array" \
    -c "Add :CFPlugInTypes:443ABAB8-E7B3-491A-B985-BEB9187030DB:0 string $factory_uuid" \
    "$driver/Contents/Info.plist"

  # Ad-hoc signature; pass SIGN_IDENTITY="Francisco Local Code Signing" if coreaudiod rejects it.
  codesign --force --sign "${SIGN_IDENTITY:--}" "$driver"
  echo "$driver"
}

# Francisco's Mic. Visible device: input only. Device 2: hidden, output only (the daemon's feed).
# 2 channels: apps treat a stereo virtual input as a normal mic; the daemon writes both.
# No volume control, so apps' auto gain can't rescale the mic.
build MicDSP local.mic-dsp.driver 6f3c1d52-8a4e-4b6f-9c2d-5e7a1b3c9d01 <<'EOF'
kDevice_Name=\"Francisco\'s\ Mic\"
kDevice2_Name=\"Francisco\'s\ Mic\ Feed\"
kDevice_IsHidden=false
kDevice_HasInput=true
kDevice_HasOutput=false
kDevice2_IsHidden=true
kDevice2_HasInput=false
kDevice2_HasOutput=true
kEnableVolumeControl=false
kCanBeDefaultDevice=true
kCanBeDefaultSystemDevice=false
EOF

# Francisco's Speakers. Visible device: output only, the Mac's default output. Device 2: hidden,
# input only (the loopback the daemon reads what was played from).
# BlackHole applies the volume where the input side is read, so the volume keys work and the
# daemon reads what it should play.
build MicDSPSpeakers local.mic-dsp.speakers 56b18a31-ab1b-4248-80ab-1977113152b7 <<'EOF'
kDevice_Name=\"Francisco\'s\ Speakers\"
kDevice2_Name=\"Francisco\'s\ Speakers\ Loopback\"
kDevice_IsHidden=false
kDevice_HasInput=false
kDevice_HasOutput=true
kDevice2_IsHidden=true
kDevice2_HasInput=true
kDevice2_HasOutput=false
kEnableVolumeControl=true
kCanBeDefaultDevice=true
kCanBeDefaultSystemDevice=true
EOF
