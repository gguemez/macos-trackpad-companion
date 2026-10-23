# HongMall P-T066 on macOS 27: setup and troubleshooting

This records a working path for the HongMall P-T066 (`048D:8911`, reported
as `HTX HID Device`) and the failure modes encountered while bringing it up.
It also explains the implementation so another developer or coding agent
can reuse the work without having to rediscover the device protocol.

The code and automated tests were generated with an AI coding assistant.
A human tested the physical device on an Apple Silicon Mac running macOS
27.0.1. Treat that as one tested setup, not broad hardware certification.

## Minimal configuration

Save this as `hongmall.toml`:

```toml
[device]
vid = 0x048d
pid = 0x8911

[gestures.swipe.horizontal]
enable = "on"
backend = "synthetic"

[gestures.swipe.vertical]
enable = "on"
backend = "synthetic"
```

Build and start the companion with the pad connected:

```sh
cargo build --release --locked
./target/release/companion --config ./hongmall.toml -v
```

Grant Input Monitoring and Accessibility to the executable/app being used.
Keep only one companion instance running. The instance lock deliberately
rejects duplicates because one process shutting down can change the pad's
mode underneath another process.

## Only heartbeats, no movement or touch frames

The misleading startup log is:

```text
entered PTP via vendor Report 0x10 (heartbeat-protected)
PTP control (0x10) set to 0x83
```

The pad can acknowledge an unknown feature write without implementing that
protocol. A successful `SET_FEATURE` is not proof of RMK vendor support.
The former early return selected the vendor path and never sent the
standard mode/reporting commands. A pad left in PTP mode by a previous test
could work until a power reset, hiding the initialization problem.

This device's standard feature reports are:

| Report | Value | Meaning |
| --- | --- | --- |
| `03` | `03` | Select Precision Touchpad mode |
| `05` | `03` | Enable contact and button reports |
| `07` | `00` | Normal latency |

The feature-write buffer includes the report ID followed by the value.
`hid.rs` skips the RMK probe for this VID/PID and uses IDs discovered from
the descriptor. This model's mode reads have returned an unreliable
single-byte zero, so initialization does not use them to verify success.
Incoming touch frames are the operational check. Other devices retain
their existing vendor-probe and standard-fallback behavior.

The device also exposes a keyboard interface. Firmware-generated shortcuts
while touch reports are absent are consistent with the wrong device mode;
the keyboard interface does not by itself mean macOS selected the wrong
interface for the companion.

The captured 628-byte descriptor is preserved in
`tests/fixtures/hongmall-pt066-descriptor.hex`. It describes five contact
slots, 30-byte touch report `04`, and a physical area around 117 × 88 mm.
The fixture contains no device serial number or personal input capture.

## Pinch and scroll work, but three-finger swipes do nothing

First inspect recognition. If the log contains `swipe: began` and
`swipe: ended` on both axes, finger recognition is already working.
Changing distance thresholds does not address a failure farther downstream.
Event-posting details require `-vv` rather than `-v`.

On macOS 27, the Dock consumes an embedded IOHID DockSwipe representation.
Setting the old CGEvent fields alone is insufficient. `dock_swipe.rs`
encodes the queue header, DockPrimary swipe record, and child velocity
record for terminal phases; `output.rs` appends it as serialized field
4205 and reconstructs the event with CoreGraphics.

The queue is encoded explicitly to avoid Rust padding and uninitialized
bytes. Tests check record boundaries, phase bits, axis, progress, and
velocity. A real CoreGraphics round-trip verifies both the embedded and
legacy formats without posting input. Selection is based on the running
OS version. Progress and velocity use consistent native coordinates, with
the system natural-scroll preference sampled at gesture start.

Pinch uses a separate digitizer/vendor payload builder; successful pinch
does not validate the DockSwipe envelope. The existing vertical notification
backend was a useful diagnostic fallback, but the working final setup uses
continuous `synthetic` output on both axes. No temporary discrete backend
is required.

References: Apple's [IOHID wire structures](https://github.com/apple-oss-distributions/IOHIDFamily/blob/main/IOHIDFamily/IOHIDEventData.h),
the field-4205 approach in [iss](https://github.com/joshuarli/iss/blob/master/iss.c)
(0BSD), and the independent macOS 27 IOHID path in
[Mac Mouse Fix](https://github.com/noah-nuebling/mac-mouse-fix/blob/master/Helper/Core/Touch/TouchSimulator.m).

## Select text without a physical click

Tap once, lift, then touch again nearby within 300 ms and keep that finger
down while dragging. The second touch must land within 3 mm of the first.
A quick second tap remains a normal double-click; movement beyond the tap
budget or holding the second touch beyond the tap window starts a drag.
Lifting releases immediately, without a drag lock or repositioning delay.
This behavior is built into the companion and requires no config change.

`tap_drag.rs` recognizes the follow-up touch; `gesture.rs` drives the
existing held-button output. The drag press uses click count 1 so selecting
text works at character level instead of being interpreted as a word-selecting
double-click. Pause, disconnect, rejected contacts, and shutdown release the
button. Adding a finger ends the drag; ordinary two-finger scrolling and
three-finger swipes remain available. Regression tests cover these paths,
and the user confirmed text selection and immediate release on this setup.

## Start on USB connection, stop when unplugged

For infrequent use, follow the [USB startup instructions](../README.md#start-only-when-a-usb-trackpad-is-attached).
They stage reviewable files and install a per-user job, without root or
extra Apple entitlements. The installed app uses the selected config by
absolute path; moving that file requires preparing/installing again.

The job uses `LaunchEvents/com.apple.iokit.matching`, not a polling shell
script. Its `RunAtLoad` and `KeepAlive` values are false. The companion
consumes the XPC event stream so a pending activation does not repeatedly
restart it. Physical USB notifications control its lifetime independently
of HID permissions. A brief removal grace period allows a quick reattach.

An unplug check must inspect processes, not merely whether the cursor
stopped. In the tested setup, two checks ten seconds apart found no
companion process, a clean exit status, and an unchanged launch count.
This verifies absence of the companion during that interval; it is not
a battery-power measurement.

The installed app needs its own privacy grants. Add the `.app` in the
user's `~/Applications` folder, not the staged bundle or its inner binary.
After a local ad-hoc-signed rebuild, macOS may require both grants to be
renewed even when the app remains at the same path.
After a privacy dialog's **Quit & Reopen**, macOS can reopen a separate
copy without the config and USB-lifetime arguments. If that copy holds the
lock, the USB job exits as a duplicate and the app may remain after unplug.
Use `python3 scripts/usb-autostart.py restart` to hand control back to
launchd. It verifies the lock owner's command before signalling it.

Apple's [USB launch-event guidance](https://developer.apple.com/forums/thread/108927)
explains why consuming the launch event is necessary.

## Validation and limits

Reproduce the automated checks from the repository root:

```sh
cargo test --locked --lib
cargo clippy --locked --lib --bins -- -D warnings
python3 scripts/test_usb_autostart.py
cargo build --release --locked --bin companion
```

Human testing covered pointer movement, tap-then-drag text selection with
immediate release, two-finger scroll, pinch, continuous three-finger swipes
in all four directions, and USB activation/removal on
one HongMall and one macOS 27.0.1 host. Reboot persistence is configured but
has not been physically tested. Other hardware, older macOS runtimes, both
natural-scroll settings, extended sleep/wake use, and long-term stability
remain areas for validation.

The companion still calculates pointer movement using its configurable
custom acceleration curve. These changes do not add a virtual HID pointer
or inherit the built-in trackpad's native acceleration profile.
