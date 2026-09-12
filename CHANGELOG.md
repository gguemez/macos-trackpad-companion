# Changelog

Notable changes per version. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

The version lives in one place, `Cargo.toml`, and reaches everything else
from there: `companion --version`, the menu-bar header, the About window,
the diagnostics blob, and `CFBundleShortVersionString` /
`CFBundleVersion` in the bundle that `scripts/bundle.sh` stamps.

What is understood but *not* done — and what is not understood — stays in
[`docs/known-gaps.md`](docs/known-gaps.md). That file is a standing record
rather than a per-version one, so it is not duplicated here.

## [0.9.3] — 2026-09-12

### Added

- **Check for Updates…** in the menu. It fetches a small static JSON feed
  (`appcast.json`), compares its version against this build, and reports
  what it found.
- **Download** fetches the dmg to
  `~/Library/Application Support/macos-trackpad-companion/Updates/` and
  verifies it against the `size` and `sha256` the feed published, then
  offers to reveal it in the Finder. An artifact that fails either check
  is deleted rather than left on disk for someone to find later and
  trust. The menu line carries the progress, since an agent has nowhere
  to put a progress bar.
- The companion still never replaces itself. Installing over a running
  app means quitting the process doing the installing, and here it would
  also have to survive a `KeepAlive` LaunchAgent that restarts what it
  just deleted — worth doing properly or not at all.
- A quiet check at launch, on by default. Failures and up-to-date results
  reach the log only — the one visible effect of finding something is the
  menu item renaming itself to **Update to X.Y.Z…**. A menu-bar agent has
  no window to hang a banner on, and an update has no deadline that would
  justify a notification.
- `[update]` in the config: `check_at_launch` and `feed_url`. Setting
  `feed_url = ""` turns checking off entirely, for anyone who would
  rather the machine not reach out at all. The feed is a plain `GET` of a
  static file carrying no identifier, so a check discloses nothing beyond
  what any HTTP request does.

### Added — distribution

- Dual MIT / Apache-2.0 licensing, carried from upstream
  (`scottlamb/macos-trackpad-companion`, which added it in `519edcd`).
  This fork is a derivative work and has to ship those notices;
  `LICENSE-APACHE`, `LICENSE-MIT`, a README section and
  `license = "MIT OR Apache-2.0"` in `Cargo.toml` now do.
- Those notices now reach the *artifact*, not just the repository.
  `bundle.sh` copies both into `Contents/Resources` before codesign
  seals the bundle, and `release.sh` puts a pair at the top level of the
  dmg. Apache-2.0 section 4(a) asks that a copy accompany the
  distributed copies, and what is distributed here is a dmg — the
  license living only in git satisfied nothing for the person
  downloading it.
- The About window names the copyright holder and the upstream it forks,
  which it previously did not: it showed a name, a version and a blurb
  and no attribution of any kind.
- A Homebrew cask in a new `gguemez/tap`:
  `brew install --cask trackpad-companion`. A cask rather than a
  formula, and not only for convenience — a formula builds from source,
  and a source build is ad-hoc signed, so its designated requirement
  embeds a cdhash and Input Monitoring plus Accessibility re-prompt on
  every upgrade. The cask installs the notarized Developer ID bundle,
  and the grants persist.
- `scripts/publish.sh` bumps the cask from the artifact it just
  uploaded, with an anchored and *counted* substitution — a rewrite that
  silently matches nothing is the failure that section exists to
  prevent. A cask failure does not fail the publish: the dmg and feed
  are already live by then, and aborting would read as a failed release
  when the cask merely lags a version.

### Added — the release pipeline

- `scripts/release.sh` — build, sign, notarize, staple, dmg, verify. One
  command from a clean tree to `dist/Trackpad-Companion-X.Y.Z.dmg`.
- `scripts/publish.sh` — the dmg to Cloudflare R2, then `appcast.json` a
  minute later. It ends by fetching the live feed and failing if it does
  not name this exact build: `dist/` looks identical whether or not
  anything was ever uploaded, so the build machine is the wrong place to
  judge whether a release happened.
- `RELEASE=1` in `scripts/bundle.sh` — universal binary, hardened
  runtime, secure timestamp, and a Developer ID certificate required
  rather than merely preferred. The day-to-day default is unchanged: a
  fast native build with no hardened runtime, because that strips
  `get-task-allow` and blocks lldb.

### Changed

- `scripts/bundle.sh` signs with `--timestamp` on the release path. A
  secure timestamp is a hard notarization prerequisite and was missing;
  it is deliberately still absent day to day, since it is a network
  round trip to Apple on every build.
- Signing retries three times. The Apple timestamp service intermittently
  fails with "A timestamp was expected but was not found" when IPv6 to
  `2620:149::/32` black-holes, and at that point a release has already
  paid for two full cross-compiles.

### Notes

- Version comparison is by numeric component, never lexical: `"0.9.0"`
  sorts above `"0.40.0"` as a string, so a string compare would stop
  offering updates the moment the minor version reached double digits —
  silently, and permanently. A single leading `v` is stripped, since a
  feed authored from a git tag name is an easy mistake and being strict
  would fail closed.
- The feed parser accepts unknown keys, unlike `config.rs`, which rejects
  them. A typo in the config file is the user's own and should surface at
  once; a feed that grew a field in a later release must not stop parsing
  in an older copy.
- `NSURLSession` rather than an HTTP crate — AppKit is already linked, so
  this adds the system's TLS trust store and proxy handling at no new
  dependency tree. The AppKit test exercises the real path over a
  loopback socket: available, up to date, older, HTTP 404, malformed
  body, connection refused, and an invalid feed URL; and for downloads,
  a verified artifact, a mismatched checksum, a mismatched size and an
  HTTP 503 — asserting each rejection also leaves nothing on disk.
- Verifying the checksum is not decoration. A dmg fetched in-app carries
  no quarantine bit, so macOS runs no notarization check on first launch;
  the feed's SHA-256, served over HTTPS beside the bytes it describes, is
  what stands in for that.
- SHA-256 comes from CommonCrypto, declared by hand the way `app_kit`
  declares its two dispatch symbols — it is in libSystem, already linked,
  and the surface is three functions. The NIST vectors in the tests are
  what hold the hand-written `CC_SHA256_CTX` layout honest.
- The filename an artifact is staged under comes from the server's
  suggestion, then the URL, then the version — and has to end in `.dmg`,
  so a redirect to an error page cannot leave an `.html` in the updates
  directory looking like an installer. It is then reduced to ASCII
  alphanumerics and `.-_`, because a name arriving from the network
  containing `..` or `/` would be a path traversal out of that
  directory.
- `release.sh` re-checks the architecture, hardened runtime, timestamp and
  ad-hoc signature itself rather than trusting `bundle.sh`, because
  `SKIP_BUILD=1` does not run `bundle.sh` at all. Each of those failures
  produces an artifact that signs, notarizes and installs perfectly and
  then fails where the build machine cannot see — an arm64-only build
  does not run slowly on an Intel Mac, it does not launch.
- The cask's `uninstall` stops the LaunchAgent before removing the
  bundle, and deletes the agent plist rather than leaving it to `zap`.
  Both matter here more than they would for an ordinary app: removing
  the bundle from under a running daemon can leave the machine with no
  pointer at all, and a plist left behind points launchd at a binary
  that no longer exists, to retry at every login.
- Publishing is two phases with a minute between them: the bytes, then
  the pointer. R2 is strongly consistent, but the public hostname is
  fronted by Cloudflare and a PoP asked for a key before it existed can
  still be holding a cached 404 — so the feed moves last, and only after
  the artifact has been verified over its public URL and given time to
  settle.

## [0.9.2] — 2026-09-12

### Fixed

- The settings window drew its **Troubleshooting** heading on top of the
  first row of buttons. The section needs 92 points below its heading —
  an 8-point gap, two 28-point button rows, the gap between them and the
  window's own bottom margin — and the 656-point window left it 74. The
  window is now 676 points and everything above that section moved up by
  20, so the heading clears the buttons and every other gap is unchanged.
- The AppKit test now asserts that no control in the settings or scope
  window overlaps another. Absolute hand-placed frames have no layout
  engine to catch a collision, which is why this one shipped; the test
  fails on the old coordinates and passes on the new.

### Changed

- The startup log line carries the version:
  `macos-trackpad-companion 0.9.2 starting (config=…)`. A log someone
  mails you is often all you get, and which build produced it is the
  first question. `Copy Diagnostics` already had it, but that is a copy
  the user has to think to make.
- The menu item is **About** rather than **About Trackpad Companion**.
  The application menu names the app because the app name is that menu's
  own title; a status-bar menu whose header already reads "Trackpad
  Companion 0.9.2" has no such excuse. The window it opens keeps the
  full name.

## [0.9.1] — 2026-09-12

First numbered version. The 70 commits before it carried a placeholder
`0.1.0` that was never built into an artifact anyone ran, so there is no
earlier release to diff against; this entry describes what 0.9.1 *is*
rather than what changed. 0.9.x rather than 1.0 because the gesture
constants have only ever been tuned against one physical trackpad.

### The bridge

- Decodes Windows Precision Touchpad HID input reports and synthesizes
  native macOS gestures: cursor, click, drag, scroll with inertia,
  pinch, rotate, and 3-/4-finger swipe.
- The touch-report layout is read from the device's HID report
  descriptor rather than assumed, so contact geometry, field offsets and
  per-axis density come from the hardware. Coordinates are converted to
  millimeters before recognition, which keeps the engine firmware-
  agnostic.
- PTP mode is entered by feature report and **read back** to verify the
  device acted, not merely acknowledged, and reverted on shutdown.
  Contact Count Maximum (usage `0x55`) is discovered and queried.
- Parallel and hybrid multi-report frames are assembled per device and
  bounded by the advertised maximum. Partial frames, duplicate contact
  IDs and over-capacity frames never reach recognition.
- A contact that loses its confidence bit cancels the gesture it was
  part of and stays excluded until it physically lifts, so a palm cannot
  become a tap on its way off the pad.

### Configuration

- All tuning lives in a TOML file at
  `~/.config/macos-trackpad-companion/config.toml`. Unknown keys are
  rejected. The file is watched and re-applied live; one that fails to
  parse is logged and the running settings stay in force.
- Per-gesture policy can be scoped to the focused app (`only` /
  `except`).
- The swipe progress reference is derived from the pad's own span rather
  than a fixed 50 mm, so the Spaces animation tracks the fingers
  proportionally on pads of any size.

### Interface

- Menu-bar agent (`LSUIElement`): no Dock tile, no app-switcher entry.
  The menu reports device state and carries Pause/Resume, Settings,
  Gesture Scope, About and Quit.
- **Settings** window: sliders and toggles that write the config file,
  start-at-login, and a Troubleshooting row with Permissions, Copy
  Diagnostics, Reveal Log, Reveal Config and Reset to Defaults.
- **Gesture Scope** window: the recognizer's decision while your fingers
  are still on the pad — contacts and their tracks on a pad drawn to its
  real aspect ratio, the three normalized scores against their shared
  threshold, every gate that is deciding the outcome, and the lock
  frozen at the frame it fired. Carries a live-tuning column for the
  cursor and scroll curves, applied on release rather than mid-drag.
- **About** window: icon, name and the running version.
- First-run setup window opens automatically when either privacy grant
  is missing, shows live state, and links to the right System Settings
  pane.
- Quit and Pause warn first when stopping would leave the machine with
  no working pointer.

### Diagnosis

- `companion --record FILE` writes the device's frame stream as plain
  text; the `replay` binary feeds it back through the same engine with
  no HID, no CGEvents and no permissions. Replays are byte-identical
  across runs, so a capture is a valid regression fixture and a trackpad
  nobody here owns can be diagnosed from a file. `replay FILE --scope`
  watches one through the scope window, with play, pause, step and
  scrub.
- `companion --dump-descriptors` prints every matching device's HID
  report descriptor and exits without changing any device's mode — safe
  to run while the companion is running.
- Copy Diagnostics puts the version, both permission states, the pointer
  policy, the attached device's geometry, the config and log paths, and
  the config values actually in force onto the clipboard.
