#!/bin/sh
# One command from a clean tree to a shippable artifact:
# build → sign → notarize → staple → dmg → verify.
#
# Produces dist/Trackpad-Companion-<version>.dmg, universal, signed with
# the Developer ID Application certificate, notarized by Apple, and
# stapled so first launch works offline. This is what users download;
# scripts/bundle.sh alone is NOT — an un-notarized bundle hits
# Gatekeeper's "cannot be opened" wall on every Mac but this one.
#
# Deliberately does NOT publish. scripts/publish.sh is separate, because
# a locally built dmg carries no quarantine bit and so proves nothing
# about Gatekeeper: the dmg wants downloading through a browser and
# launching before anyone else is offered it. Keeping them apart also
# means a re-publish costs nothing — no rebuild, no second trip to
# Apple.
#
# Prerequisites (one-time):
#   1. A "Developer ID Application" certificate in the login keychain.
#   2. Both toolchain targets:
#        rustup target add aarch64-apple-darwin x86_64-apple-darwin
#   3. A stored notarytool credential profile. An app-specific password,
#      NOT the Apple ID password:
#        xcrun notarytool store-credentials "trackpad-companion-notary" \
#          --apple-id guillermo@guemez.net --team-id DUT656TLYP
#      Any existing profile works too — it is the same Apple ID and team:
#        NOTARY_PROFILE=runic-notary scripts/release.sh
#
# Usage:  scripts/release.sh
#         ALLOW_DIRTY=1 scripts/release.sh   # skip the clean-tree check
#         SKIP_BUILD=1  scripts/release.sh   # resume without rebuilding
set -eu

cd "$(dirname "$0")/.."
ROOT=$(pwd)

NOTARY_PROFILE=${NOTARY_PROFILE:-trackpad-companion-notary}
APP="$ROOT/target/companion.app"

# -------------------------------------------------------------- preflight ---

# A release must be reproducible from a commit. A dirty tree means the
# shipped bytes match nothing in the history, and the version in
# Cargo.toml — which is the only place it lives — then describes a build
# nobody can get back to.
if [ "${ALLOW_DIRTY:-0}" != "1" ] && [ -n "$(git status --porcelain)" ]; then
	echo "ERROR: the working tree is dirty. Commit before releasing."
	echo "       Override with ALLOW_DIRTY=1 if you know what you're doing."
	exit 1
fi

if ! xcrun notarytool history --keychain-profile "$NOTARY_PROFILE" >/dev/null 2>&1; then
	echo "ERROR: no stored notarytool credential named '$NOTARY_PROFILE'."
	echo "       xcrun notarytool store-credentials \"$NOTARY_PROFILE\" \\"
	echo "         --apple-id guillermo@guemez.net --team-id DUT656TLYP"
	echo "       or point at an existing one:"
	echo "         NOTARY_PROFILE=runic-notary $0"
	exit 1
fi

# ------------------------------------------------------------------ build ---

# SKIP_BUILD=1 reuses the existing bundle untouched. Needed when
# resuming after an interrupted run: notarization happens on Apple's
# servers and survives a dead client, but rebuilding would re-sign the
# bundle and invalidate the ticket Apple already issued for those exact
# bytes, forcing a pointless second round trip.
if [ "${SKIP_BUILD:-0}" = "1" ]; then
	[ -d "$APP" ] || { echo "ERROR: SKIP_BUILD=1 but $APP does not exist."; exit 1; }
	echo "SKIP_BUILD=1 — reusing $APP (no rebuild, no re-sign)."
else
	RELEASE=1 scripts/bundle.sh
fi

VERSION=$(/usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" \
	"$APP/Contents/Info.plist")
DMG="$ROOT/dist/Trackpad-Companion-$VERSION.dmg"
mkdir -p "$ROOT/dist"
echo
echo "Releasing Trackpad Companion $VERSION"

# The version the updater compares against is compiled in from
# Cargo.toml, and the Info.plist is stamped from the same line. If those
# two ever disagree, an installed copy would go on offering itself an
# update it already has.
BUILT=$("$APP/Contents/MacOS/companion" --version 2>/dev/null | awk '{ print $NF }')
if [ "$BUILT" != "$VERSION" ]; then
	echo "ERROR: the binary reports $BUILT but the bundle says $VERSION."
	exit 1
fi

# Everything below is re-asserted here rather than trusted from
# bundle.sh, because SKIP_BUILD=1 does not run bundle.sh at all — and
# the whole point of resuming is that the bundle came from an earlier
# run whose flags are not visible from here. Each of these produces an
# artifact that signs, notarizes and installs perfectly and then fails
# somewhere the build machine cannot see.
ARCHS=$(lipo -archs "$APP/Contents/MacOS/companion")
for want in arm64 x86_64; do
	case " $ARCHS " in
	*" $want "*) ;;
	*)
		echo "ERROR: the binary has no $want slice (got: $ARCHS)."
		echo "       A single-arch build will not launch at all on the Macs"
		echo "       it is missing. Rebuild without SKIP_BUILD."
		exit 1
		;;
	esac
done

SIG=$(codesign -dv --verbose=4 "$APP" 2>&1)
case "$SIG" in
*"Signature=adhoc"*)
	echo "ERROR: the bundle is ad-hoc signed. Notarization will reject it."
	exit 1
	;;
esac
case "$SIG" in
*"flags=0x10000(runtime)"*) ;;
*)
	echo "ERROR: the bundle was not signed with the hardened runtime."
	echo "       Notarization requires it. Rebuild without SKIP_BUILD."
	exit 1
	;;
esac
case "$SIG" in
*"Timestamp="*) ;;
*)
	echo "ERROR: the signature carries no secure timestamp."
	echo "       Notarization requires one. Rebuild without SKIP_BUILD."
	exit 1
	;;
esac
echo "  universal ($ARCHS), hardened, timestamped, Developer ID."

# --------------------------------------------------------------- notarize ---

# Notarize the .app first and staple the ticket INTO the bundle, so it
# still validates after a user drags it out of the dmg onto a machine
# that is offline. The dmg is then built from the already-stapled app
# and notarized in turn.
notarize() { # $1 = path to submit
	echo "Submitting $(basename "$1") to Apple (this takes a few minutes)…"
	out=$(xcrun notarytool submit "$1" --keychain-profile "$NOTARY_PROFILE" --wait 2>&1)
	echo "$out"
	notary_status=$(echo "$out" | awk '/^ *status:/ { print $2; exit }')
	notary_id=$(echo "$out" | awk '/^ *id:/ { print $2; exit }')
	if [ "$notary_status" != "Accepted" ]; then
		echo "ERROR: notarization failed (status: ${notary_status:-unknown}). Apple's log:"
		[ -n "$notary_id" ] && xcrun notarytool log "$notary_id" \
			--keychain-profile "$NOTARY_PROFILE" || true
		exit 1
	fi
}

# Idempotent: a bundle that already validates carries Apple's ticket for
# exactly these bytes, so re-submitting would buy nothing. This is what
# makes resuming after an interrupted wait cost seconds rather than
# another round trip.
if xcrun stapler validate "$APP" >/dev/null 2>&1; then
	echo "$APP already carries a notarization ticket — not re-submitting."
else
	ZIP="$ROOT/dist/Trackpad-Companion-$VERSION.zip"
	# ditto --keepParent preserves the .app wrapper; `zip` would mangle
	# the bundle's internal structure.
	ditto -c -k --keepParent "$APP" "$ZIP"
	notarize "$ZIP"
	xcrun stapler staple "$APP"
	rm -f "$ZIP"
fi

# -------------------------------------------------------------------- dmg ---

echo "Building $(basename "$DMG")…"
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
cp -R "$APP" "$STAGE/Trackpad Companion.app"
ln -s /Applications "$STAGE/Applications"   # the drag-to-install target
# Also at the top level of the dmg, where someone can read them without
# opening the bundle. The copies inside Contents/Resources are the ones
# that survive the drag to /Applications; these are the ones that are
# visible at the moment of installing. Both are cheap.
cp "$ROOT/LICENSE-APACHE" "$ROOT/LICENSE-MIT" "$STAGE/"
rm -f "$DMG"
hdiutil create -volname "Trackpad Companion $VERSION" -srcfolder "$STAGE" \
	-ov -format UDZO "$DMG" >/dev/null

# A dmg carries no executable code, so no hardened runtime here — but it
# does need the identity and a secure timestamp to be notarizable.
# Retried for the same reason bundle.sh retries: one flaky second at the
# very end would otherwise throw away a completed build AND a completed
# notarization.
SIGN_ID=$(security find-identity -v -p codesigning 2>/dev/null \
	| awk -F'"' '/Developer ID Application/ { print $2; exit }')
attempt=1
while :; do
	if codesign --force --sign "$SIGN_ID" --timestamp "$DMG" 2>&1 &&
		codesign --verify "$DMG" 2>/dev/null; then
		break
	fi
	if [ "$attempt" -ge 3 ]; then
		echo "ERROR: could not sign $DMG after 3 attempts."
		exit 1
	fi
	echo "    dmg signing failed (attempt $attempt/3) — retrying"
	attempt=$((attempt + 1))
	sleep 2
done
notarize "$DMG"
xcrun stapler staple "$DMG"

# ----------------------------------------------------------------- verify ---

echo
echo "── Verification ──────────────────────────────────────────────"
codesign --verify --deep --strict --verbose=2 "$APP"
# The authoritative check: the exact evaluation Gatekeeper performs on a
# downloaded app. It must say "source=Notarized Developer ID".
spctl --assess --type execute --verbose=4 "$APP"
xcrun stapler validate "$APP"
xcrun stapler validate "$DMG"
echo "arch: $(lipo -archs "$APP/Contents/MacOS/companion")"

echo
echo "✅ $DMG ($(du -h "$DMG" | cut -f1)) — universal, signed, notarized, stapled."
echo
echo "Before publishing, test it the way a user will:"
echo "  1. Download the dmg through a browser. That sets the quarantine bit;"
echo "     a locally built dmg does NOT have it, so testing this copy proves"
echo "     nothing about Gatekeeper."
echo "  2. Open it, drag Trackpad Companion to /Applications, launch from Finder."
echo "  3. Ideally on a Mac that never built this, and note that Input"
echo "     Monitoring and Accessibility will prompt again: the grants are keyed"
echo "     to code identity, and a Developer ID signature is not the ad-hoc one"
echo "     a dev build carries."
echo
echo "Then:  . ~/.r2-creds && scripts/publish.sh"
