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
