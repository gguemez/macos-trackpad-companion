#!/bin/sh
# Assemble target/companion.app — the primary artifact.
#
# The companion is a menu-bar agent: LSUIElement keeps it out of the Dock
# and the app switcher, so the only UI is the status-bar icon. The plain
# `target/release/companion` binary still runs from a terminal for
# debugging, but it has nowhere to hang an icon.
#
# SIGNING: macOS keys Input Monitoring and Accessibility grants to code
# identity, so the signature decides whether permissions survive a
# rebuild. With a real certificate the designated requirement is based
# on the team and bundle id, and grants persist. Ad-hoc (`--sign -`)
# embeds the binary's cdhash instead, so every rebuild looks like a new
# app and re-prompts for both permissions.
#
# Identity is picked automatically: Developer ID Application, else Apple
# Development, else ad-hoc. Override with SIGN_IDENTITY.
#
# Hardened runtime is opt-in (HARDENED=1). It's required for
# notarization, but it also strips get-task-allow, which blocks lldb
# from attaching — not what you want day to day. TCC stability does not
# depend on it.
#
# RELEASE=1 switches everything to what actually ships: a universal
# binary, the hardened runtime, a secure timestamp, and a Developer ID
# certificate required rather than preferred. None of those are wanted
# day to day — see above for why hardened runtime in particular gets in
# the way — so the default stays the fast native build.
# scripts/release.sh sets it; you rarely need to.
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
APP="$ROOT/target/companion.app"
BUNDLE_ID=${BUNDLE_ID:-net.guemez.trackpad-companion}
RELEASE=${RELEASE:-0}

TARGET_ARM=aarch64-apple-darwin
TARGET_X86=x86_64-apple-darwin

if [ -z "${SIGN_IDENTITY:-}" ]; then
	SIGN_IDENTITY=$(security find-identity -v -p codesigning 2>/dev/null \
		| awk -F'"' '/Developer ID Application/ { print $2; exit }')
fi
if [ -z "$SIGN_IDENTITY" ] && [ "$RELEASE" != "1" ]; then
	SIGN_IDENTITY=$(security find-identity -v -p codesigning 2>/dev/null \
		| awk -F'"' '/Apple Development/ { print $2; exit }')
fi
if [ "$RELEASE" = "1" ] && [ -z "$SIGN_IDENTITY" ]; then
	echo "ERROR: no 'Developer ID Application' certificate in the keychain."
	echo "       Notarization requires one; Apple Development will not do."
	echo "       Xcode > Settings > Accounts > Manage Certificates > + >"
	echo "       Developer ID Application"
	exit 1
fi
SIGN_IDENTITY=${SIGN_IDENTITY:--}
VERSION=$(awk -F'"' '/^version *=/ { print $2; exit }' "$ROOT/Cargo.toml")

# ------------------------------------------------------------------ build ---

if [ "$RELEASE" = "1" ]; then
	# Rosetta translates Intel to ARM and never the reverse, so an
	# arm64-only build cannot run on an Intel Mac at all — there is no
	# degraded mode, it simply will not launch. A missing slice is not a
	# smaller release, it is one a whole class of machine can't open.
	for target in "$TARGET_ARM" "$TARGET_X86"; do
		if ! rustup target list --installed 2>/dev/null | grep -qx "$target"; then
			echo "ERROR: the $target toolchain target is not installed."
			echo "       rustup target add $target"
			exit 1
		fi
	done
	echo "Building $TARGET_ARM…"
	cargo build --release --target "$TARGET_ARM" --manifest-path "$ROOT/Cargo.toml"
	echo "Building $TARGET_X86…"
	cargo build --release --target "$TARGET_X86" --manifest-path "$ROOT/Cargo.toml"
else
	cargo build --release --manifest-path "$ROOT/Cargo.toml"
fi

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
if [ "$RELEASE" = "1" ]; then
	# Address the per-target directories explicitly. `target/release` is
	# the *host* build and says nothing about the cross-compiled one, so
	# reading from it here would quietly ship a single-arch binary.
	lipo -create \
		"$ROOT/target/$TARGET_ARM/release/companion" \
		"$ROOT/target/$TARGET_X86/release/companion" \
		-output "$APP/Contents/MacOS/companion"
else
	cp "$ROOT/target/release/companion" "$APP/Contents/MacOS/companion"
fi
cp "$ROOT/assets/icons/bridge.icns" "$APP/Contents/Resources/companion.icns"

# The license notices travel inside the bundle. Apache-2.0 section 4(a)
# asks that a copy of the license accompany the distributed copies, and
# the distributed copy is the .app a user drags out of the dmg, not this
# repository. Dropped here, before codesign, because signing seals the
# bundle: a file added afterwards invalidates the signature.
cp "$ROOT/LICENSE-APACHE" "$ROOT/LICENSE-MIT" "$APP/Contents/Resources/"

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

# Assert the slices BEFORE signing. Nothing downstream checks: a
# single-arch binary signs, notarizes, staples and installs perfectly,
# and then fails to launch on the machines it is missing.
if [ "$RELEASE" = "1" ]; then
	ARCHS=$(lipo -archs "$APP/Contents/MacOS/companion")
	for want in arm64 x86_64; do
		case " $ARCHS " in
		*" $want "*) ;;
		*)
			echo "ERROR: the binary has no $want slice (got: $ARCHS)."
			exit 1
			;;
		esac
	done
fi

# ------------------------------------------------------------------- sign ---

# --force so a rebuild replaces the previous signature rather than failing.
#
# --timestamp only on the release path: a secure timestamp is a hard
# notarization prerequisite, and it is also a network round trip to
# Apple on every single build, which is not something to pay for a debug
# cycle.
if [ "$RELEASE" = "1" ]; then
	SIGN_ARGS="--options runtime --timestamp"
elif [ "${HARDENED:-0}" = "1" ] && [ "$SIGN_IDENTITY" != "-" ]; then
	SIGN_ARGS="--options runtime"
else
	SIGN_ARGS=""
fi

# The Apple timestamp service intermittently fails with "A timestamp was
# expected but was not found" when IPv6 to 2620:149::/32 black-holes — a
# VPN or Private Relay route with no real IPv6 path. Retried rather than
# failed, because at this point a release has already paid for two full
# cross-compiles.
attempt=1
while :; do
	# shellcheck disable=SC2086  # SIGN_ARGS is deliberately word-split
	if codesign --force --sign "$SIGN_IDENTITY" --identifier "$BUNDLE_ID" \
		$SIGN_ARGS "$APP" && codesign --verify "$APP" 2>/dev/null; then
		break
	fi
	if [ "$attempt" -ge 3 ]; then
		echo "ERROR: could not sign $APP after 3 attempts."
		echo "       If it says 'A timestamp was expected but was not found',"
		echo "       check IPv6: curl -4 http://timestamp.apple.com/ts01"
		echo "       succeeding while the default route fails confirms it."
		exit 1
	fi
	echo "    signing failed (attempt $attempt/3) — retrying"
	attempt=$((attempt + 1))
	sleep 2
done
codesign --verify --deep --strict "$APP"

# Guard against shipping an ad-hoc bundle: notarization would reject it
# anyway, but failing here gives a comprehensible error rather than one
# of Apple's.
if [ "$RELEASE" = "1" ] && codesign -dv --verbose=4 "$APP" 2>&1 | grep -q "Signature=adhoc"; then
	echo "ERROR: the bundle is ad-hoc signed — the identity was not picked up."
	exit 1
fi

echo "built $APP"
echo "  bundle id : $BUNDLE_ID"
echo "  version   : $VERSION"
echo "  arch      : $(lipo -archs "$APP/Contents/MacOS/companion")"
echo "  signed by : $SIGN_IDENTITY"
# The designated requirement is what TCC matches on across rebuilds.
codesign -d -r- "$APP" 2>&1 | sed -n 's/^designated => /  requirement: /p'

# Optional install. The bundle in target/ is rebuilt (and rm -rf'd) on
# every run, so a copy under ~/Applications is what you actually launch
# day to day — and TCC grants follow the bundle id, not the path, so
# both copies share one set of permissions.
if [ "${INSTALL:-0}" = "1" ]; then
	INSTALL_DIR=${INSTALL_DIR:-$HOME/Applications}
	DEST="$INSTALL_DIR/Trackpad Companion.app"
	mkdir -p "$INSTALL_DIR"
	rm -rf "$DEST"
	cp -R "$APP" "$DEST"
	codesign --verify --strict "$DEST"
	echo "  installed : $DEST"
fi

echo
echo "run it:  open $APP"
echo "or CLI:  $ROOT/target/release/companion -v"
echo "install: INSTALL=1 $0"
