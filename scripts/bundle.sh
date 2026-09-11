#!/bin/sh
# Assemble target/companion.app — the primary artifact.
#
# The companion is a menu-bar agent: LSUIElement keeps it out of the Dock
# and the app switcher, so the only UI is the status-bar icon. The plain
# `target/release/companion` binary still runs from a terminal for
# debugging, but it has nowhere to hang an icon.
#
# TCC WARNING: macOS keys Input Monitoring and Accessibility grants to
# code identity. This script ad-hoc signs (`--sign -`), and an ad-hoc
# signature's designated requirement embeds the binary's cdhash — which
# changes on every rebuild. Expect to re-grant both permissions after
# each `bundle.sh` run. To make grants stick across builds, sign with a
# self-signed certificate in the login keychain and pass its name as
# SIGN_IDENTITY (see README).
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
APP="$ROOT/target/companion.app"
BUNDLE_ID=${BUNDLE_ID:-io.github.gguemez.macos-trackpad-companion}
SIGN_IDENTITY=${SIGN_IDENTITY:--}
VERSION=$(awk -F'"' '/^version *=/ { print $2; exit }' "$ROOT/Cargo.toml")

cargo build --release --manifest-path "$ROOT/Cargo.toml"

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$ROOT/target/release/companion" "$APP/Contents/MacOS/companion"
cp "$ROOT/assets/icons/bridge.icns" "$APP/Contents/Resources/companion.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key>            <string>Trackpad Companion</string>
	<key>CFBundleDisplayName</key>     <string>Trackpad Companion</string>
	<key>CFBundleIdentifier</key>      <string>${BUNDLE_ID}</string>
	<key>CFBundleExecutable</key>      <string>companion</string>
	<key>CFBundleIconFile</key>        <string>companion</string>
	<key>CFBundlePackageType</key>     <string>APPL</string>
	<key>CFBundleShortVersionString</key> <string>${VERSION}</string>
	<key>CFBundleVersion</key>         <string>${VERSION}</string>
	<key>CFBundleInfoDictionaryVersion</key> <string>6.0</string>
	<key>LSMinimumSystemVersion</key>  <string>13.0</string>
	<key>LSUIElement</key>             <true/>
	<key>NSHighResolutionCapable</key> <true/>
</dict>
</plist>
PLIST

plutil -lint "$APP/Contents/Info.plist" >/dev/null

# --force so a rebuild replaces the previous signature rather than failing.
codesign --force --sign "$SIGN_IDENTITY" --identifier "$BUNDLE_ID" "$APP"
codesign --verify --deep --strict "$APP"

echo "built $APP"
echo "  bundle id : $BUNDLE_ID"
echo "  version   : $VERSION"
echo "  signed by : $SIGN_IDENTITY"
echo
echo "run it:  open $APP"
echo "or CLI:  $ROOT/target/release/companion -v"
