#!/bin/sh
# Publish an already-built release: the dmg, then appcast.json, to
# Cloudflare R2.
#
# Deliberately SEPARATE from scripts/release.sh, which ends by telling
# you to download and launch the dmg first — a locally built one carries
# no quarantine bit and so proves nothing about Gatekeeper. Publishing
# automatically at the end of a build would contradict that. Keeping
# them apart also means a re-publish costs nothing: no rebuild, and no
# second notarization round trip.
#
# The feed and the dmg go to the SAME bucket by design. If the feed
# lived anywhere else, an outage of that host would make every installed
# copy fail its update check while the download it names is perfectly
# reachable.
#
# Prerequisites:
#   1. scripts/release.sh has produced a signed, notarized, stapled dmg.
#   2. R2 *S3* credentials in the environment. Create them at
#        Cloudflare → R2 → Manage API Tokens → Object Read & Write,
#      then keep them out of shell history and out of the repo:
#        cat > ~/.r2-creds <<'EOF'
#        export AWS_ACCESS_KEY_ID=…
#        export AWS_SECRET_ACCESS_KEY=…
#        EOF
#        chmod 600 ~/.r2-creds
#      and run:  . ~/.r2-creds && scripts/publish.sh
#
# Usage:  scripts/publish.sh
#         DRY_RUN=1 scripts/publish.sh    # preflight and print, upload nothing
set -eu

cd "$(dirname "$0")/.."
ROOT=$(pwd)

R2_ACCOUNT_ID=${R2_ACCOUNT_ID:-1eae95e644e792224b62fdc521c01398}
R2_BUCKET=${R2_BUCKET:-trackpad-companion-releases}
PUBLIC_BASE=${PUBLIC_BASE:-https://dl.trackpad-companion.guemez.net}

# Seconds between the artifact going up and the feed that points at it.
# See the two-phase note below for why this is not zero. Overridable for
# a re-publish of a version whose bytes are already live and verified
# (FEED_DELAY=0), never as a way to make a first publish finish sooner.
FEED_DELAY=${FEED_DELAY:-60}
APP="$ROOT/target/companion.app"

# -------------------------------------------------------------- preflight ---

# The version comes from the BUILT bundle, never a literal in this file.
# A hardcoded version here could drift from the artifact and publish a
# feed advertising bytes nobody built.
[ -d "$APP" ] || { echo "ERROR: $APP not found. Run scripts/release.sh first."; exit 1; }
VERSION=$(/usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" \
	"$APP/Contents/Info.plist")
DMG_NAME="Trackpad-Companion-$VERSION.dmg"
DMG="$ROOT/dist/$DMG_NAME"

[ -f "$DMG" ] || { echo "ERROR: $DMG not found. Run scripts/release.sh first."; exit 1; }

# Never publish an un-notarized dmg. Gatekeeper would block it on every
# machine but the one that built it, and the failure is invisible here.
if ! xcrun stapler validate "$DMG" >/dev/null 2>&1; then
	echo "ERROR: $DMG has no stapled notarization ticket."
	echo "       Publishing it would hand users a dmg macOS refuses to open."
	exit 1
fi

DMG_SHA=$(shasum -a 256 "$DMG" | cut -d' ' -f1)
DMG_SIZE=$(stat -f%z "$DMG")

echo "Publishing Trackpad Companion $VERSION"
echo "  dmg:    $DMG_NAME ($DMG_SIZE bytes)"
echo "  bucket: $R2_BUCKET"
echo "  public: $PUBLIC_BASE"
echo "  delay:  ${FEED_DELAY}s between the artifact and the feed"

# ---------------------------------------------------------------- appcast ---

# Generated, never hand-edited: version, URL, checksum and size all come
# from the artifact above, so the feed cannot advertise bytes that do
# not exist. The shape must match `Feed` in src/update.rs —
# {version, url, notes?, sha256?, size?}.
APPCAST="$ROOT/dist/appcast.json"
cat > "$APPCAST" <<JSON
{
  "version": "$VERSION",
  "url": "$PUBLIC_BASE/$DMG_NAME",
  "notes": "https://github.com/gguemez/macos-trackpad-companion/blob/main/CHANGELOG.md",
  "sha256": "$DMG_SHA",
  "size": $DMG_SIZE
}
JSON
echo "  feed:   $(tr -d '\n ' < "$APPCAST")"

if [ -n "${DRY_RUN:-}" ]; then
	echo
	echo "DRY_RUN set — nothing uploaded."
	exit 0
fi

# Credentials are checked HERE rather than in the preflight above, so a
# dry run works before you have them. A dry run exists precisely to show
# what would be published, which is what you want to look at BEFORE
# going to fetch a secret.
if [ -z "${AWS_ACCESS_KEY_ID:-}" ] || [ -z "${AWS_SECRET_ACCESS_KEY:-}" ]; then
	echo "ERROR: AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY are not set."
	echo "       These are the R2 *S3* credentials (Access Key ID + Secret), not"
	echo "       a Cloudflare API token — a cfat_/cfut_ value will NOT work."
	echo "       Run:  . ~/.r2-creds && scripts/publish.sh"
	exit 1
fi

python3 -c 'import boto3' 2>/dev/null || {
	echo "ERROR: python3 with boto3 is required (pip install boto3)."; exit 1; }

# ----------------------------------------------------------------- upload ---

R2_ACCOUNT_ID="$R2_ACCOUNT_ID" R2_BUCKET="$R2_BUCKET" PUBLIC_BASE="$PUBLIC_BASE" \
DMG="$DMG" DMG_NAME="$DMG_NAME" APPCAST="$APPCAST" VERSION="$VERSION" \
FEED_DELAY="$FEED_DELAY" \
python3 <<'PY'
import hashlib, os, sys, time, urllib.request
import boto3
from botocore.config import Config

ACCOUNT = os.environ["R2_ACCOUNT_ID"]
BUCKET  = os.environ["R2_BUCKET"]
PUBLIC  = os.environ["PUBLIC_BASE"].rstrip("/")
DMG     = os.environ["DMG"]
DMG_KEY = os.environ["DMG_NAME"]
APPCAST = os.environ["APPCAST"]
FEED_DELAY = int(os.environ["FEED_DELAY"])

# Cloudflare's bot protection 403s some default user-agents, python-urllib
# among them. The app itself is unaffected — NSURLSession sends a CFNetwork
# UA — but this verification would fail confusingly without an explicit one.
UA = {"User-Agent": f"trackpad-companion-publish/{os.environ['VERSION']}"}

def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for chunk in iter(lambda: fh.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()

s3 = boto3.client(
    "s3",
    endpoint_url=f"https://{ACCOUNT}.r2.cloudflarestorage.com",
    region_name="auto",
    config=Config(signature_version="s3v4", retries={"max_attempts": 3}),
)

# The upload is deliberately TWO phases with a wait between them: the bytes,
# then the pointer at the bytes.
#
#   phase 1  the dmg — what a download actually fetches
#   (wait)   FEED_DELAY seconds
#   phase 2  appcast.json — the only file that tells the world any of it exists
#
# Ordering alone is not enough, which is the part that is easy to get wrong.
# Even with the dmg uploaded first, a feed published a few hundred milliseconds
# later advertises an object whose availability is not yet uniform: R2 is
# strongly consistent for a read of the object, but the public hostname is
# fronted by Cloudflare, and a PoP that was asked for that key BEFORE it landed
# can still be holding a cached 404. Publish the feed into that window and the
# first users to check — the eager ones, within seconds — get a download that
# 404s at their edge while working perfectly from the build machine. That is
# the worst shape of bug to be handed: unreproducible by the only person who
# can fix it.
#
# So the feed goes up only after the artifact is uploaded, verified over its
# public URL, and given FEED_DELAY seconds to settle.
ARTIFACTS = [(DMG, DMG_KEY, "application/x-apple-diskimage")]
FEED = [(APPCAST, "appcast.json", "application/json")]

digests = {}

def upload(group):
    for path, key, ctype in group:
        digests[key] = sha256(path)
        s3.upload_file(path, BUCKET, key, ExtraArgs={"ContentType": ctype})
        print(f"  put {key:<36} {os.path.getsize(path):>12,} bytes  {ctype}")

def verify(group):
    ok = True
    for _, key, _ in group:
        # Never request a key before it is uploaded. Asking the CDN for a key
        # that does not exist yet is not a free check — the 404 is itself
        # cacheable, and a probe run seconds before the upload is one of the
        # ways a PoP ends up serving 404 for an object that is present.
        req = urllib.request.Request(f"{PUBLIC}/{key}", headers=UA)
        with urllib.request.urlopen(req, timeout=300) as r:
            body = r.read()
        got = hashlib.sha256(body).hexdigest()
        same = got == digests[key]
        ok &= same
        print(f"  {key:<36} HTTP {r.status}  {len(body):>12,} bytes  "
              f"sha256 {'match' if same else 'MISMATCH'}")
    if not ok:
        sys.exit("\nERROR: what the CDN serves does not match what was uploaded.")

print("\n== upload artifacts ==")
upload(ARTIFACTS)

print("\n== verify artifacts over the public URL ==")
verify(ARTIFACTS)

# Only now is it true that every byte the feed is about to point at is fetchable.
if FEED_DELAY > 0:
    print(f"\n== settle ({FEED_DELAY}s before publishing the pointer) ==")
    print("  the artifact is live and verified; the feed still advertises the")
    print("  previous version, so nothing is half-published in this window.")
    time.sleep(FEED_DELAY)

print("\n== upload feed ==")
upload(FEED)

print("\n== verify feed over the public URL ==")
verify(FEED)
PY

# ----------------------------------------------------------- homebrew cask ---

# The cask is the one place a version lives that cannot be derived at
# read time: Homebrew requires a LITERAL sha256, so `livecheck` can
# correctly report that a version exists while `brew install` still
# fetches the previous dmg. Hence: bumped by the same command that
# publishes that dmg, from the same artifact, never by hand.
#
# A failure here does NOT fail the publish. The dmg and the feed are
# already live and verified above; aborting now would read as "the
# release failed" when what actually happened is that the cask lags by
# one version.

TAP_DIR=${TAP_DIR:-$(brew --repository 2>/dev/null)/Library/Taps/gguemez/homebrew-tap}
CASK="$TAP_DIR/Casks/trackpad-companion.rb"

echo
echo "== homebrew cask =="
if [ ! -f "$CASK" ]; then
	echo "  ⚠️  tap not found at $TAP_DIR"
	echo "     Run 'brew tap gguemez/tap' and re-run, or edit the cask by hand:"
	echo "       version \"$VERSION\"  sha256 \"$DMG_SHA\""
elif ! (
	set -eu

	CASK="$CASK" VERSION="$VERSION" DMG_SHA="$DMG_SHA" python3 <<'CASKPY'
import os, re, sys

path    = os.environ["CASK"]
version = os.environ["VERSION"]
sha     = os.environ["DMG_SHA"]

src = open(path).read()

# Anchored AND counted. A substitution that silently matches nothing is
# precisely the failure this section exists to prevent, so anything other than
# exactly one match per field is an error rather than a no-op.
src, n_ver = re.subn(r'^(  version ")[^"]+(")$', rf'\g<1>{version}\g<2>', src, flags=re.M)
src, n_sha = re.subn(r'^(  sha256 ")[^"]+(")$',  rf'\g<1>{sha}\g<2>',     src, flags=re.M)
if (n_ver, n_sha) != (1, 1):
    sys.exit(f"ERROR: expected 1 version and 1 sha256 line, matched {n_ver} and {n_sha}.")

open(path, "w").write(src)
CASKPY

	git -C "$TAP_DIR" add Casks/trackpad-companion.rb
	if git -C "$TAP_DIR" diff --cached --quiet; then
		echo "  already at $VERSION — nothing to push"
	else
		git -C "$TAP_DIR" commit -q -m "trackpad-companion $VERSION"
		git -C "$TAP_DIR" push -q origin HEAD
		echo "  pushed cask $VERSION  sha256 $DMG_SHA"
	fi

	# Verify what GitHub SERVES, not what was written locally — the same
	# reason the uploads above are re-fetched over the CDN before the run
	# is called good.
	RAW="https://raw.githubusercontent.com/gguemez/homebrew-tap/main/Casks/trackpad-companion.rb"
	SERVED=$(curl -fsS "$RAW" | sed -n 's/^  version "\(.*\)"$/\1/p')
	[ "$SERVED" = "$VERSION" ] || {
		echo "  ⚠️  tap serves '$SERVED', expected $VERSION"; exit 1; }
	echo "  tap serves $SERVED ✓"
); then
	echo "  ⚠️  cask update failed — the release IS published; bump the tap by hand:"
	echo "       version \"$VERSION\"  sha256 \"$DMG_SHA\""
fi

# ----------------------------------------------------------------- landing ---

# The end-to-end check that matters, and the only one that says a release
# happened: fetch the feed the way the app will, and confirm it advertises
# this exact build. Everything before this point looks identical whether or
# not anything ever reached a user.
echo
echo "== feed as served =="
curl -fsS "$PUBLIC_BASE/appcast.json" | sed 's/^/  /'

SERVED=$(curl -fsS "$PUBLIC_BASE/appcast.json" |
	sed -n 's/.*"version" *: *"\([^"]*\)".*/\1/p')
if [ "$SERVED" != "$VERSION" ]; then
	echo
	echo "ERROR: the live feed advertises '$SERVED', not '$VERSION'."
	echo "       The release is NOT published until it does."
	exit 1
fi

echo
echo "✅ $VERSION is live. An installed copy will offer it on its next check."
echo
echo "   brew:     brew install --cask gguemez/tap/trackpad-companion"
echo
echo "Remaining, by hand:"
echo "  git tag v$VERSION && git push --tags"
echo "  gh release create v$VERSION '$DMG' --title 'v$VERSION' --notes-from-tag"
