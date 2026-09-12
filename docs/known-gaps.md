# Known gaps

Things that are understood but not done, and things that are not
understood. Kept here so they survive outside anyone's memory.

## Device compatibility

### Tuning constants are not derived from the device

`gesture.rs` holds ~17 tuning constants. Three families of them are
device-dependent, and only the fourth is genuinely universal:

| Family | Constants | Status |
| --- | --- | --- |
| Size-scaled | `SWIPE_PROGRESS_REF_MM` 50, `SWIPE_AXIS_LOCK_MM` 3, `PARTIAL_LIFT_REJOIN_DRIFT_MM` 10 | **Should come from the descriptor — the data is already parsed and ignored** |
| Rate-dependent | `DEFAULT_FRAME_DT` 8 ms, `SCROLL_VELOCITY_ALPHA` 0.4, `PARTIAL_LIFT_REJOIN_WINDOW` 80 ms | Measurable from scan time; `scan_clock` already derives real timing |
| Noise-scaled | `MOTION_DEAD_ZONE_MM` 0.04, `PAN_LOCK_MM` 0.4, `ANCHORED_FINGER_FLOOR_MM` 0.3, `PHYSICAL_DRAG_SELECT_MM` 0.3, `TAP_MAX_MOVE_MM` 1.0 | Needs new measurement (resting-contact variance) |
| Human | `TAP_MAX_DURATION` 150 ms, `PINCH_ROTATE_HYSTERESIS`, `PINCH_LOCK_RATIO`, `ROTATE_LOCK_RAD` | Correctly device-independent |

The first row is the clearest defect. `SWIPE_PROGRESS_REF_MM = 50` means
"a full swipe is 50 mm" — a quarter of the surface on a 209 mm pad, most
of it on the 65 mm reference. Physical dimensions are parsed from the
descriptor and then unused by every threshold.

Suggested order: derive the size- and rate-scaled families first
(deterministic, unit-testable against the two descriptors already in the
tests), and treat noise adaptation as a later, separate step.

If noise adaptation is attempted, measure once per device and freeze it;
persist it per vid/pid where it is visible and overridable; log what was
measured and what changed; clamp hard so a bad measurement degrades
instead of breaking; never re-tune mid-gesture. Continuously drifting
thresholds make behaviour irreproducible and every bug report
unfalsifiable.

### Only one real device has ever been tested

There is one third-party pad (vid `0x258a` pid `0x0010`) plus the
reference firmware's descriptor. Building adaptation against a single
device produces something tuned to make *that* device work, untested
elsewhere — arguably more fragile than honest constants, because the
failure hides inside a derivation.

### No capture/replay

Recording raw frames plus the descriptor, and replaying them through the
engine offline, would let a device be characterised without owning it
and turn each one into a regression fixture. A `scroll_replay.rs`
existed at some point; only a stale doc comment survived.

Descriptor parsing is already testable this way — `--dump-descriptors`
output is enough to reproduce a parse — but nothing covers the gesture
engine against a real frame stream.

### Feature reports are only ever written, never read

There is no `IOHIDDeviceGetReport` binding. So the companion cannot:

- verify that a mode switch actually took effect (this would have
  settled "did the revert work?" immediately),
- read Contact Count Maximum to cross-check the descriptor,
- detect a device left in PTP mode by a crashed run.

## Permissions

### Input Monitoring never prompts and never self-registers

Observed on macOS 26.6.2 with a Developer ID-signed `LSUIElement`
bundle launched via `open`:

- no dialog ever appeared for the app;
- `IOHIDCheckAccess` went `Unknown` → `Denied` within ~70 ms of
  `IOHIDRequestAccess`, i.e. macOS recorded a refusal without asking;
- the app never appeared in the Input Monitoring list, so "Open
  Settings" landed on a pane with nothing to toggle;
- adding the bundle by hand with `+` worked, and the grant persisted.

`AXIsProcessTrustedWithOptions` (Accessibility) prompted correctly first
try, so this is specific to the HID path. Full notes and the
experiments worth running are in the `TODO(permissions)` comment on
`request_input_monitoring()` in `permissions.rs`.

Current workaround: the setup window tells the user to add the app with
`+`.

## Platform behaviour

### A spec-path device goes dormant when nothing drives it

Reverting to mouse mode on shutdown is acknowledged, but the device then
sends nothing at all — not touch reports, not mouse reports. It returns
when a companion process acquires it again (relaunching is enough; no
replug needed). A settle delay between the revert and closing the
manager was tried and made no difference: the device needs
re-acquisition, not time.

Mitigations in place: the quit/pause guard, and start-at-login with
crash-only `KeepAlive`.

### App Nap throttles timers

A windowless agent gets its timers coalesced — a 3 s retry interval was
observed firing at ~9 s. `ProcessType = Interactive` in the LaunchAgent
plist addresses it when running under launchd;
`NSProcessInfo beginActivityWithOptions` would cover the general case.

### macOS can disable the built-in trackpad

`USBMouseStopsTrackpad` in `com.apple.AppleMultitouchTrackpad` (mirrored
in `com.apple.driver.AppleBluetoothMultitouch.trackpad`), stored as a
number. **Not** `mouseDriverIgnoreTrackpad` under
`com.apple.universalaccess`, which is what the pane's location in System
Settings suggests and what older references describe; that key does not
exist on macOS 26. Read by `system_prefs.rs`.

Finding it took four attempts. What worked was toggling the switch and
looking for which plist changed:
`find ~/Library/Preferences -name "*.plist" -mmin -5`.

## UI

### The settings window is partial

Not exposed: `cursor.accel_ref`, `[log]`, `[device]`, and the
`only` / `except` app lists (a checkbox can't represent a list, so those
gestures show a disabled checkbox rather than being flattened to on/off
and silently losing the list).

Values are read when the window opens, not continuously. Editing the
file in a text editor while the window is open leaves the controls
stale until it is closed and reopened. Live-refreshing would risk
yanking a slider mid-drag.

### No gesture scope

The engine computes lock scores and contact geometry that are currently
only observable by reading log lines. A live view of contacts and the
2F lock decision is the one remaining piece aimed at what this project
is actually for — refining recognition.

The seam to build it on: keep `gesture.rs` pure. It depends only on
`Frame` and an `Output` sink, which is what makes it unit-testable. A
second optional observer alongside `Output`, fed what the engine already
computes, keeps the engine honest and the scope additive.
