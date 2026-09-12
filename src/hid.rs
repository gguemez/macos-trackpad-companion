//! IOHIDManager wrapper. Matches PTP-class digitizer interfaces
//! (DeviceUsagePage 0x0D / Usage 0x05) and pumps input reports into
//! a user-supplied callback on the main run loop.
//!
//! On macOS the Input Monitoring privacy bucket gates `IOHIDManagerOpen`
//! for any device we don't own; the first run will prompt the user to
//! grant it via System Settings. Returns a clear error message on the
//! known failure code (0xE00002C5).

#![allow(non_upper_case_globals)]

use crate::descriptor::{self, Layout};
use crate::report::{self, Frame};
use crate::scan_clock::ScanTimeClock;
use crate::time::Timestamp;
use anyhow::{Result, bail};
use core_foundation::base::{CFType, TCFType};
use core_foundation_sys::base::{CFGetTypeID, CFTypeRef};
use core_foundation_sys::number::{CFBooleanGetTypeID, CFBooleanGetValue, CFBooleanRef};
use core_foundation::data::CFData;
use core_foundation::date::CFDate;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::runloop::{
    CFRunLoop, CFRunLoopRun, CFRunLoopTimer, kCFRunLoopCommonModes,
};
use objc2::MainThreadMarker;
use core_foundation::string::CFString;
use core_foundation_sys::runloop::{
    CFRunLoopTimerContext, CFRunLoopTimerInvalidate, CFRunLoopTimerRef,
};
use std::ffi::c_void;
use std::os::raw::c_int;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};

// ---- IOHID types & constants ----

type IOHIDManagerRef = *mut c_void;
type IOHIDDeviceRef = *mut c_void;
type IOOptionBits = u32;
type IOReturn = c_int;
type IOHIDReportType = u32;

const kIOHIDOptionsTypeNone: IOOptionBits = 0;
const kIOReturnSuccess: IOReturn = 0;
const kIOHIDReportTypeFeature: IOHIDReportType = 2;

/// Spec Microsoft Precision Touchpad "Input Mode" Feature Report ID.
/// Universal — every PTP device exposes it. 1 byte: 0 = mouse,
/// 3 = multi-touch. We use this for third-party PTP devices and as a
/// fallback when the RMK vendor 0x10 path is unavailable.

/// Vendor Feature Report ID exposed by RMK firmware. One byte:
/// low nibble = mode (0 = mouse, 3 = PTP), bit 7 = heartbeat-required.
/// A single SET_FEATURE flips both flags atomically on the firmware
/// side, so the firmware can recover from companion SIGKILL by reverting
/// to mouse after a heartbeat timeout — no equivalent on the spec 0x08
/// path. Standard PTP devices don't expose this report; we detect that
/// case by trying 0x10 first and falling back to 0x08 on
/// `kIOReturnUnsupported`. See `rmk/rmk/src/hid.rs::PTP_REPORT_DEADLINE_TICKS`.
const PTP_CONTROL_REPORT_ID: isize = 0x10;
const PTP_CONTROL_PTP_HEARTBEAT: u8 = 0x83; // mode=3 + bit7
const PTP_CONTROL_MOUSE: u8 = 0x00;

/// How often to re-assert `PTP_CONTROL_PTP_HEARTBEAT` on devices that
/// accepted the vendor path. Sized comfortably under the firmware's
/// 12-s timeout (`PTP_HEARTBEAT_TIMEOUT_TICKS`); a couple of skipped
/// pulses (process pause, USB stack hiccup) still leaves headroom.
// Standard PTP devices don't get heartbeat pulses; there is no
// equivalent heartbeat mechanism in the standard Input Mode path.
const HEARTBEAT_INTERVAL_SECS: f64 = 5.0;
const PTP_SELECTIVE_REPORT_ALL: u8 = 0x03;
const PTP_LATENCY_NORMAL: u8 = 0x00;

/// Microsoft Precision Touchpad Input Mode feature values.
/// The report ID is device-specific and is discovered from the
/// HID descriptor via Digitizer Usage 0x52.
const PTP_INPUT_MODE_PTP: u8 = 0x03;
const PTP_INPUT_MODE_MOUSE: u8 = 0x00;

const KEY_VENDOR_ID: &str = "VendorID";
const KEY_PRODUCT_ID: &str = "ProductID";
const KEY_DEVICE_USAGE_PAGE: &str = "DeviceUsagePage";
const KEY_DEVICE_USAGE: &str = "DeviceUsage";
const KEY_PRODUCT: &str = "Product";
const KEY_REPORT_DESCRIPTOR: &str = "ReportDescriptor";

const PTP_USAGE_PAGE: i32 = 0x0D;
const PTP_USAGE: i32 = 0x05;

type IOHIDReportCallback = unsafe extern "C" fn(
    context: *mut c_void,
    result: IOReturn,
    sender: *mut c_void,
    report_type: IOHIDReportType,
    report_id: u32,
    report: *mut u8,
    report_length: isize,
);

type IOHIDDeviceCallback = unsafe extern "C" fn(
    context: *mut c_void,
    result: IOReturn,
    sender: *mut c_void,
    device: IOHIDDeviceRef,
);

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOHIDManagerCreate(allocator: *mut c_void, options: IOOptionBits) -> IOHIDManagerRef;
    fn IOHIDManagerSetDeviceMatching(manager: IOHIDManagerRef, matching: *const c_void);
    fn IOHIDManagerRegisterDeviceMatchingCallback(
        manager: IOHIDManagerRef,
        callback: IOHIDDeviceCallback,
        context: *mut c_void,
    );
    fn IOHIDManagerRegisterDeviceRemovalCallback(
        manager: IOHIDManagerRef,
        callback: IOHIDDeviceCallback,
        context: *mut c_void,
    );
    fn IOHIDManagerScheduleWithRunLoop(
        manager: IOHIDManagerRef,
        run_loop: *mut c_void,
        run_loop_mode: *const c_void,
    );
    fn IOHIDManagerOpen(manager: IOHIDManagerRef, options: IOOptionBits) -> IOReturn;
    fn IOHIDManagerClose(manager: IOHIDManagerRef, options: IOOptionBits) -> IOReturn;

    fn IOHIDDeviceGetProperty(device: IOHIDDeviceRef, key: *const c_void) -> *const c_void;
    fn IOHIDDeviceRegisterInputReportCallback(
        device: IOHIDDeviceRef,
        report: *mut u8,
        report_length: isize,
        callback: IOHIDReportCallback,
        context: *mut c_void,
    );
    /// `IOReturn IOHIDDeviceSetReport(IOHIDDeviceRef, IOHIDReportType,
    /// CFIndex reportID, const uint8_t *report, CFIndex reportLength)`.
    /// CFIndex is `long` → `isize` on 64-bit macOS.
    fn IOHIDDeviceSetReport(
        device: IOHIDDeviceRef,
        report_type: IOHIDReportType,
        report_id: isize,
        report: *const u8,
        report_length: isize,
    ) -> IOReturn;

    fn IOHIDDeviceScheduleWithRunLoop(
        device: IOHIDDeviceRef,
        run_loop: *mut c_void,
        run_loop_mode: *const c_void,
    );
}

/// Whether a device on the spec Input Mode path is currently attached.
///
/// Those devices go dormant when the companion stops — see the note on
/// `DeviceState::drop`. Vendor-path devices have a firmware watchdog
/// that reverts them automatically, so they don't need the warning.
static SPEC_PATH_DEVICE: AtomicBool = AtomicBool::new(false);

/// Whether a built-in (internal) HID device has been seen. Used to
/// decide whether there is any fallback pointer at all — on a desktop
/// Mac there isn't, so quitting strands the user regardless of how
/// Pointer Control is configured.
static BUILTIN_DEVICE_SEEN: AtomicBool = AtomicBool::new(false);

/// True when quitting would leave a trackpad unresponsive until the
/// companion runs again.
pub fn quit_would_strand_device() -> bool {
    SPEC_PATH_DEVICE.load(Ordering::Relaxed)
}

/// Whether this machine appears to have a built-in trackpad that could
/// serve as a fallback. Conservative: only true once one has actually
/// been seen, so an unknown answer errs toward warning the user.
pub fn builtin_trackpad_present() -> bool {
    BUILTIN_DEVICE_SEEN.load(Ordering::Relaxed)
}

/// Read a CoreFoundation boolean property off an IOHID device.
fn read_bool_property(device: IOHIDDeviceRef, key: &str) -> Option<bool> {
    let cfkey = CFString::new(key);
    let raw = unsafe { IOHIDDeviceGetProperty(device, cfkey.as_concrete_TypeRef() as *const _) };
    if raw.is_null() {
        return None;
    }
    unsafe {
        if CFGetTypeID(raw as CFTypeRef) == CFBooleanGetTypeID() {
            Some(CFBooleanGetValue(raw as CFBooleanRef))
        } else {
            None
        }
    }
}

// ---- Public API ----

#[derive(Clone, Copy, Debug)]
pub struct Filter {
    pub vid: Option<u16>,
    pub pid: Option<u16>,
}

pub struct Manager {
    raw: IOHIDManagerRef,
    filter: Filter,
    bridge: Option<Pin<Box<Bridge>>>,
}

/// Owns the user's per-frame callback and the per-device state. All
/// callbacks fire on the run-loop thread, so single-threaded `&mut`
/// access through raw pointers is safe.
struct Bridge {
    on_frame: Box<dyn FnMut(Frame, Timestamp)>,
    devices: Vec<Pin<Box<DeviceState>>>,
}

/// Which mode-control report this device responds to. Decided once at
/// match time by trying the vendor path first; the spec path is a
/// universal fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ControlPath {
    /// RMK vendor Report 0x10 — single-byte combined mode + heartbeat
    /// opt-in. Devices on this path get periodic heartbeat pulses so the
    /// firmware can recover from companion SIGKILL.
    Vendor,
    /// Spec Report 0x08 (Input Mode). What every standard PTP device
    /// (Microsoft Surface, Apple Magic Trackpad in PTP mode, etc.)
    /// exposes. No heartbeat support; companion crash leaves the device
    /// in PTP mode until something else flips it back, but on a
    /// third-party device that's the host's problem to manage anyway.
    SpecInputMode,
}

struct DeviceState {
    device: IOHIDDeviceRef,
    layout: Layout,
    buf: Vec<u8>,
    bridge: *mut Bridge,
    /// Per-device scan-time → host-time estimator. Each device has its
    /// own free-running scan_time counter, so each gets its own clock.
    scan_clock: ScanTimeClock,
    control_path: ControlPath,
}

impl Drop for DeviceState {
    /// Revert the device to mouse mode on the way out.
    ///
    /// Measured caveat on the spec Input Mode path (0x08 / whatever the
    /// descriptor declares — 0x25 on the pad this was tested against):
    /// the SET_FEATURE is acknowledged, but the device then sends
    /// *nothing*. It has stopped producing touch reports and does not
    /// start producing mouse reports, so there is no pointer from it at
    /// all until either it is re-enumerated (unplug/replug) or a
    /// companion process picks it up again and puts it back into PTP
    /// mode. Relaunching is enough; the cable is not required.
    ///
    /// A 300 ms settle delay between this write and closing the manager
    /// was tried on the theory that the close was racing the mode
    /// switch. It made no difference and was removed — the device needs
    /// re-acquisition, not time.
    ///
    /// The vendor path's heartbeat watchdog covers the crash case;
    /// devices on the spec path have no equivalent, so a SIGKILL leaves
    /// them dormant in exactly the same way.
    fn drop(&mut self) {
        // Revert the firmware to mouse mode on whichever report this
        // device actually responds to. Fires both on USB removal (after
        // the device is gone — the SET will fail, that's fine) and on
        // graceful companion shutdown when `Manager` drops the bridge
        // (device still attached, the SET takes effect and the user's
        // trackpad keeps working as a plain mouse). On SIGKILL we never
        // get here at all; on the vendor path the firmware's heartbeat
        // watchdog catches that case independently.
        match self.control_path {
            ControlPath::Vendor => set_ptp_control(self.device, PTP_CONTROL_MOUSE),
            ControlPath::SpecInputMode => {
                if let Some(report_id) = self.layout.input_mode_report_id {
                    set_input_mode(self.device, report_id, PTP_INPUT_MODE_MOUSE);
                }
            }
        }
    }
}

impl Manager {
    pub fn new(filter: Filter) -> Result<Self> {
        let raw = unsafe { IOHIDManagerCreate(std::ptr::null_mut(), kIOHIDOptionsTypeNone) };
        if raw.is_null() {
            bail!("IOHIDManagerCreate returned NULL");
        }
        Ok(Self {
            raw,
            filter,
            bridge: None,
        })
    }

    /// Open the manager and pump the run loop. Calls `on_frame` for every
    /// decoded touch report from any matched PTP device. Blocks until
    /// SIGINT or the run loop is stopped.
    pub fn run<F>(&mut self, on_frame: F) -> Result<()>
    where
        F: FnMut(Frame, Timestamp) + 'static,
    {
        let bridge = Box::pin(Bridge {
            on_frame: Box::new(on_frame),
            devices: Vec::new(),
        });
        self.bridge = Some(bridge);
        let bridge_ptr: *mut Bridge =
            unsafe { self.bridge.as_mut().unwrap().as_mut().get_unchecked_mut() };

        let matching = build_match_dict(&self.filter);

        unsafe {
            IOHIDManagerSetDeviceMatching(self.raw, matching.as_concrete_TypeRef() as *const _);
            IOHIDManagerRegisterDeviceMatchingCallback(
                self.raw,
                on_device_matched,
                bridge_ptr as *mut c_void,
            );
            IOHIDManagerRegisterDeviceRemovalCallback(
                self.raw,
                on_device_removed,
                bridge_ptr as *mut c_void,
            );
            // Common modes, not default mode. AppKit runs a nested run
            // loop in event-tracking mode while a menu is open or a
            // window is being dragged or resized; a source registered
            // only in the default mode is not serviced then, which
            // stalls input for as long as the menu is up.
            IOHIDManagerScheduleWithRunLoop(
                self.raw,
                CFRunLoop::get_current().as_concrete_TypeRef() as *mut _,
                kCFRunLoopCommonModes as *const _,
            );
        }

        // Opening the manager is deliberately non-fatal. The status
        // item is this app's only UI, so bailing here would flash the
        // icon up and tear it down again before the error could be
        // read. Report it in the menu and keep retrying instead — the
        // usual causes (missing Input Monitoring grant, another driver
        // holding the HID devices) are both fixable while we wait.
        let mut open_retry_timer = None;
        let rv = unsafe { IOHIDManagerOpen(self.raw, kIOHIDOptionsTypeNone) };
        if rv == kIOReturnSuccess {
            crate::status_item::set_status("Waiting for device…");
        } else {
            // Ask the permission API directly rather than inferring from
            // the return code: a denial and an unrelated failure such as
            // kIOReturnExclusiveAccess are indistinguishable otherwise.
            let access = crate::permissions::input_monitoring();
            log::error!(
                "IOHIDManagerOpen failed: {} (input monitoring: {access:?})",
                describe_open_failure(rv)
            );
            crate::status_item::set_status(if access.is_granted() {
                short_open_failure(rv)
            } else {
                "Needs Input Monitoring"
            });
            open_retry_timer = Some(install_open_retry_timer(self.raw));
        }
        // Held so the CFRunLoopTimer stays retained for the life of the
        // run loop; the callback invalidates it once the open succeeds.
        let _open_retry_timer = open_retry_timer;

        log::info!(
            "waiting for PTP device (vid={:?} pid={:?})",
            self.filter.vid,
            self.filter.pid
        );

        // Spawn the sigwait worker. `block_shutdown_signals()` must
        // have run from `main` before any thread-spawning framework
        // call (IOHIDManager / NSApp / …) so every helper thread
        // inherits the block; otherwise SIGINT can be delivered to a
        // Cocoa thread that has it unblocked, default action
        // (terminate) fires, and `DeviceState::drop` never runs —
        // leaving the firmware stuck in PTP mode. SIGKILL still skips
        // teardown; the firmware-side heartbeat watchdog covers that
        // case.
        install_shutdown_worker();

        // Heartbeat ticker: re-assert the PTP-control byte on every
        // matched device. Held in `_heartbeat_timer` so the
        // CFRunLoopTimer's CFRetain stays balanced for the duration of
        // the run loop.
        let _heartbeat_timer = install_heartbeat_timer(bridge_ptr);

        // `[NSApp run]` rather than `CFRunLoopRun()`: the menu-bar
        // status item needs `sendEvent:` dispatch to react to clicks,
        // and only NSApplication's loop provides it. IOHID sources are
        // scheduled on the main run loop, which NSApp pumps, so device
        // callbacks are unaffected.
        match MainThreadMarker::new() {
            Some(mtm) => crate::app_kit::run_event_loop(mtm),
            // Not reachable from `main`, but a caller on another thread
            // should get the old behaviour rather than a panic.
            None => unsafe { CFRunLoopRun() },
        }

        Ok(())
    }
}

/// Schedule a CFRunLoopTimer on the current run loop that pulses
/// `PTP_CONTROL_PTP_HEARTBEAT` to every matched device every
/// `HEARTBEAT_INTERVAL_SECS`. Runs on the same thread as the device
/// callbacks, so `bridge.devices` access is safely unsynchronized.
fn install_heartbeat_timer(bridge_ptr: *mut Bridge) -> CFRunLoopTimer {
    let mut context = CFRunLoopTimerContext {
        version: 0,
        info: bridge_ptr as *mut c_void,
        retain: None,
        release: None,
        copyDescription: None,
    };
    let now = CFDate::now().abs_time();
    let timer = CFRunLoopTimer::new(
        now + HEARTBEAT_INTERVAL_SECS,
        HEARTBEAT_INTERVAL_SECS,
        0,
        0,
        on_heartbeat_tick,
        &mut context,
    );
    // Common modes, so the heartbeat keeps firing while a menu is open
    // or a window is being dragged. The firmware reverts to mouse mode
    // if it misses heartbeats for ~12 s, and a menu can easily be open
    // that long.
    let mode = unsafe { kCFRunLoopCommonModes };
    CFRunLoop::get_current().add_timer(&timer, mode);
    timer
}

extern "C" fn on_heartbeat_tick(_timer: CFRunLoopTimerRef, info: *mut c_void) {
    let bridge = unsafe { &*(info as *const Bridge) };
    for state in &bridge.devices {
        // Spec 0x08 has no heartbeat semantics — pulsing it just causes
        // pointless USB traffic. Skip those devices.
        if state.control_path == ControlPath::Vendor {
            set_ptp_control(state.device, PTP_CONTROL_PTP_HEARTBEAT);
        }
    }
}

/// How often to retry `IOHIDManagerOpen` after a failed attempt.
const OPEN_RETRY_SECS: f64 = 3.0;

/// Full explanation for an `IOHIDManagerOpen` failure, for the log.
fn describe_open_failure(rv: IOReturn) -> String {
    match rv as u32 {
        0xE00002C5 => "denied (0xE00002C5) — grant Input Monitoring in System Settings → \
                       Privacy & Security → Input Monitoring"
            .to_string(),
        0xE00002E2 => "exclusive access (0xE00002E2) — another process has seized the HID \
                       devices; a third-party mouse/trackpad driver is the usual cause"
            .to_string(),
        other => format!("{other:#x}"),
    }
}

/// Menu-width version of the same, for the status line.
fn short_open_failure(rv: IOReturn) -> &'static str {
    match rv as u32 {
        0xE00002C5 => "Needs Input Monitoring",
        0xE00002E2 => "HID devices held by another app",
        _ => "HID open failed",
    }
}

/// Retry `IOHIDManagerOpen` on a timer until it succeeds, then stop.
/// Lets the companion recover without a restart once the user grants
/// the permission or quits whatever was holding the devices.
fn install_open_retry_timer(manager: IOHIDManagerRef) -> CFRunLoopTimer {
    // Leaked on purpose: the callback dereferences this for as long as
    // the timer can fire, which is the life of the process.
    let ctx = Box::into_raw(Box::new(manager));
    let mut context = CFRunLoopTimerContext {
        version: 0,
        info: ctx as *mut c_void,
        retain: None,
        release: None,
        copyDescription: None,
    };
    let now = CFDate::now().abs_time();
    let timer = CFRunLoopTimer::new(
        now + OPEN_RETRY_SECS,
        OPEN_RETRY_SECS,
        0,
        0,
        on_open_retry,
        &mut context,
    );
    let mode = unsafe { kCFRunLoopCommonModes };
    CFRunLoop::get_current().add_timer(&timer, mode);
    timer
}

extern "C" fn on_open_retry(timer: CFRunLoopTimerRef, info: *mut c_void) {
    let manager = unsafe { *(info as *const IOHIDManagerRef) };
    let rv = unsafe { IOHIDManagerOpen(manager, kIOHIDOptionsTypeNone) };
    if rv != kIOReturnSuccess {
        log::debug!("IOHIDManagerOpen retry: {:#x}", rv as u32);
        return;
    }

    // A success here is not proof of access. IOHIDManagerOpen returns
    // success on an already-open manager, and the first call can leave
    // it open even when it reported kIOReturnExclusiveAccess (which it
    // does when *any* matched device can't be opened — the internal
    // Apple trackpad matches the digitizer filter and is held by the
    // system). Report what the permission API says rather than
    // implying we can read the device.
    let access = crate::permissions::input_monitoring();
    if access.is_granted() {
        log::info!("IOHIDManagerOpen succeeded on retry");
        crate::status_item::set_status("Waiting for device…");
        unsafe { CFRunLoopTimerInvalidate(timer) };
    } else {
        log::warn!(
            "IOHIDManagerOpen returned success but Input Monitoring is {access:?} —              input reports will not arrive; continuing to retry"
        );
        crate::status_item::set_status("Needs Input Monitoring");
    }
}

/// Block SIGINT/SIGTERM in the calling thread, *and in every thread
/// spawned later by this process* (since spawned threads inherit the
/// caller's signal mask). Must be called from `main` before any other
/// thread-spawning work — IOHIDManager, NSApp, env_logger, anything
/// that might call into a framework that internally `pthread_create`s.
///
/// Without this discipline, a signal arriving after a Cocoa /
/// CoreGraphics helper thread starts will be delivered to that
/// thread instead of waited for by [`install_shutdown_worker`], and
/// the default action (process-wide terminate) skips
/// `DeviceState::drop` — leaving the firmware stuck in PTP mode after
/// the companion exits.
///
/// Idempotent: calling more than once is harmless. The sigwait worker
/// itself (spawned by `install_shutdown_worker`) inherits the block,
/// then unblocks via `sigwait` on the dedicated thread.
pub fn block_shutdown_signals() {
    use std::mem;
    use std::ptr;
    unsafe {
        let mut set: libc::sigset_t = mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, ptr::null_mut());
    }
}

/// Ask the process to shut down, exactly as Ctrl+C does.
///
/// Must be `kill(getpid(), …)` and NOT `raise(…)`. [`block_shutdown_signals`]
/// blocks SIGTERM in every thread, and `raise` is *thread-directed*: it
/// targets the calling thread, where the signal is blocked, so it stays
/// pending on that thread forever and the `sigwait` worker — parked on a
/// different thread — never receives it. The UI appears to do nothing.
///
/// `kill` on our own pid is process-directed, so the kernel hands it to
/// any thread that isn't blocking it, which is precisely the sigwait
/// worker.
pub fn request_shutdown() {
    unsafe { libc::kill(libc::getpid(), libc::SIGTERM) };
}

/// Spawn a sigwait worker that stops the main run loop when SIGINT /
/// SIGTERM arrives. Pairs with [`block_shutdown_signals`], which must
/// have run first. `sigwait` on a dedicated thread is the supported
/// way to handle signals from a CFRunLoop process (raw signal
/// handlers can't safely call CF APIs; most CF functions aren't
/// async-signal-safe).
///
/// Must be called on the main thread (the one that will run
/// `CFRunLoopRun`); the captured run loop is whichever
/// `CFRunLoop::get_current()` returns at this call site.
fn install_shutdown_worker() {
    use std::mem;

    std::thread::spawn(move || {
        unsafe {
            let mut set: libc::sigset_t = mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGINT);
            libc::sigaddset(&mut set, libc::SIGTERM);
            let mut sig: libc::c_int = 0;
            // sigwait removes the matching signal from the pending set
            // and returns it; safe to call CF APIs once we're back in
            // ordinary thread context (not a signal handler).
            let _ = libc::sigwait(&set, &mut sig);
            log::info!("received signal {sig}, shutting down");
        }
        // `CFRunLoopStop` is not enough now that the loop is
        // `[NSApp run]`: NSApplication's loop treats a stopped run loop
        // as "no event this time round" and keeps going. `request_stop`
        // hops to the main thread and does the `stop:` + dummy-event
        // dance that actually unwinds it.
        crate::app_kit::request_stop();
    });
}

impl Drop for Manager {
    fn drop(&mut self) {
        // Drop the bridge first so each `DeviceState::drop` (writing
        // Input Mode = 0 back to the firmware) fires while the
        // IOHIDManager is still open. Closing the manager closes every
        // opened device, after which `IOHIDDeviceSetReport` returns
        // kIOReturnNotOpen and the cleanup write would be wasted.
        self.bridge = None;
        unsafe {
            IOHIDManagerClose(self.raw, kIOHIDOptionsTypeNone);
        }
    }
}

unsafe extern "C" fn on_device_matched(
    context: *mut c_void,
    _result: IOReturn,
    _sender: *mut c_void,
    device: IOHIDDeviceRef,
) {
    let bridge = unsafe { &mut *(context as *mut Bridge) };

    let product = read_string_property(device, KEY_PRODUCT).unwrap_or_else(|| "<unknown>".into());
    let vid = read_number_property(device, KEY_VENDOR_ID);
    let pid = read_number_property(device, KEY_PRODUCT_ID);
    if read_bool_property(device, "Built-In") == Some(true) {
        // Seen before the descriptor check on purpose: the internal
        // trackpad matches the digitizer filter but is rejected below,
        // and it is exactly the fallback pointer we care about.
        BUILTIN_DEVICE_SEEN.store(true, Ordering::Relaxed);
    }

    let desc = match read_data_property(device, KEY_REPORT_DESCRIPTOR) {
        Some(d) => d,
        None => {
            log::warn!("matched \"{product}\" but couldn't read report descriptor");
            return;
        }
    };
    let layout = match descriptor::parse(&desc) {
        Ok(l) => l,
        Err(e) => {
            log::warn!(
                "matched \"{product}\" but descriptor parse failed: {e:#}; descriptor was {} bytes",
                desc.len()
            );
            return;
        }
    };
    log::info!(
        "matched \"{product}\" (vid={} pid={}): {} contacts, logical max {}×{} \
         ({:.1}×{:.1} mm), {} bytes/contact, payload {} bytes total",
        vid.map(|v| format!("{:#06x}", v as u16))
            .unwrap_or_else(|| "?".into()),
        pid.map(|v| format!("{:#06x}", v as u16))
            .unwrap_or_else(|| "?".into()),
        layout.contact_slots,
        layout.logical_x_max,
        layout.logical_y_max,
        layout.physical_x_max_mm,
        layout.physical_y_max_mm,
        layout.bytes_per_contact,
        layout.total_payload_bytes,
    );
    log::info!(
        "  layout offsets: report_id=0x{:02x} fingers@{} scan_time@{} contact_count@{} \
         button@{} bit{} (descriptor: {} bytes)",
        layout.report_id,
        layout.fingers_offset,
        layout.scan_time_offset,
        layout.contact_count_offset,
        layout.button_offset,
        layout.button_bit,
        desc.len(),
    );

    crate::status_item::set_status(&format!("Connected — {product}"));

    // Prefer the RMK vendor control path when supported. For standard
    // Precision Touchpads, discover the Input Mode and optional configuration
    // feature report IDs from the HID descriptor.
    let control_path = enter_ptp_mode(device, &product, &layout);
    if control_path == ControlPath::SpecInputMode {
        SPEC_PATH_DEVICE.store(true, Ordering::Relaxed);
    }

    let buf_len = layout.total_payload_bytes.max(64);
    let mut state = Box::pin(DeviceState {
        device,
        layout,
        buf: vec![0u8; buf_len],
        bridge: bridge as *mut Bridge,
        scan_clock: ScanTimeClock::new(),
        control_path,
    });

    unsafe {
        let s = state.as_mut().get_unchecked_mut();
        let buf_ptr = s.buf.as_mut_ptr();
        let buf_len_isize = s.buf.len() as isize;
        let ctx_ptr = s as *mut DeviceState as *mut c_void;

        // Common modes: input reports must keep arriving while a menu
        // is open or a window is being dragged. See the note on the
        // manager's scheduling above.
        IOHIDDeviceScheduleWithRunLoop(
            device,
            CFRunLoop::get_current().as_concrete_TypeRef() as *mut _,
            kCFRunLoopCommonModes as *const _,
        );

        IOHIDDeviceRegisterInputReportCallback(
            device,
            buf_ptr,
            buf_len_isize,
            on_input_report,
            ctx_ptr,
        );
    }

    bridge.devices.push(state);
}

/// Probe + enter PTP mode. Tries the RMK vendor 0x10 report first
/// (single-byte combined mode + heartbeat opt-in). On
/// `kIOReturnUnsupported` — what every standard PTP device returns for
/// our vendor report — silently falls back to the spec 0x08 path. Other
/// errors fall back too but are logged: typical case is a USB hiccup,
/// no point bailing the match flow when the spec path is universally
/// implemented.
fn enter_ptp_mode(device: IOHIDDeviceRef, product: &str, layout: &Layout) -> ControlPath {
    // Prefer the RMK vendor control path when available. It provides
    // heartbeat protection and preserves the original companion behavior.
    let rv = set_feature_byte(device, PTP_CONTROL_REPORT_ID, PTP_CONTROL_PTP_HEARTBEAT);

    if rv == kIOReturnSuccess {
        log::info!("\"{product}\": entered PTP via vendor Report 0x10 (heartbeat-protected)");
        return ControlPath::Vendor;
    }

    // Otherwise use the standard PTP feature reports discovered from
    // the device's HID descriptor.
    let Some(input_mode_id) = layout.input_mode_report_id else {
        log::warn!("\"{product}\": descriptor has no Input Mode feature report");
        return ControlPath::SpecInputMode;
    };

    let rv = set_feature_byte(device, input_mode_id as isize, PTP_INPUT_MODE_PTP);

    if rv != kIOReturnSuccess {
        log::warn!(
            "\"{product}\": Input Mode report {:#04x} SET failed ({:#x})",
            input_mode_id,
            rv as u32,
        );
        return ControlPath::SpecInputMode;
    }

    if let Some(id) = layout.selective_reporting_report_id {
        let rv = set_feature_byte(device, id as isize, PTP_SELECTIVE_REPORT_ALL);

        if rv != kIOReturnSuccess {
            log::warn!(
                "\"{product}\": Selective Reporting {:#04x} SET failed ({:#x})",
                id,
                rv as u32,
            );
        }
    }

    if let Some(id) = layout.latency_mode_report_id {
        let rv = set_feature_byte(device, id as isize, PTP_LATENCY_NORMAL);

        if rv != kIOReturnSuccess {
            log::warn!(
                "\"{product}\": Latency Mode {:#04x} SET failed ({:#x})",
                id,
                rv as u32,
            );
        }
    }

    log::info!(
        "\"{product}\": entered standard PTP mode via report {:#04x}",
        input_mode_id,
    );

    ControlPath::SpecInputMode
}

/// SET_FEATURE wrapper around a 1-byte vendor PTP control write.
/// Re-asserts the firmware's heartbeat deadline — used both to enter
/// PTP and as the heartbeat pulse.
fn set_ptp_control(device: IOHIDDeviceRef, byte: u8) {
    let rv = set_feature_byte(device, PTP_CONTROL_REPORT_ID, byte);
    if rv == kIOReturnSuccess {
        log::debug!("PTP control (0x10) set to {:#04x}", byte);
    } else {
        log::warn!(
            "SET_FEATURE PTP control={:#04x} failed: {:#x}",
            byte,
            rv as u32,
        );
    }
}

/// SET_FEATURE wrapper around a 1-byte spec Input Mode write. Used on
/// the third-party-device fallback path; no heartbeat semantics.
fn set_input_mode(device: IOHIDDeviceRef, report_id: u8, mode: u8) {
    let rv = set_feature_byte(device, report_id as isize, mode);

    if rv == kIOReturnSuccess {
        log::debug!("Input Mode report {:#04x} set to {:#04x}", report_id, mode);
    } else {
        log::warn!(
            "SET_FEATURE Input Mode report {:#04x}={:#04x} failed: {:#x}",
            report_id,
            mode,
            rv as u32,
        );
    }
}

fn set_feature_byte(device: IOHIDDeviceRef, report_id: isize, byte: u8) -> IOReturn {
    // For numbered reports, HID 1.11 §7.2.2 puts the Report ID byte at
    // the head of the SET_REPORT control payload. Linux's
    // `hid_output_report` builds the buffer that way; macOS
    // `IOHIDDeviceSetReport` does NOT prepend automatically — it puts
    // the `report_id` parameter in wValue and sends the buffer
    // verbatim. So we have to include the prefix ourselves to match
    // the wire format every other host produces. (hidapi follows the
    // same convention.)
    let payload = [report_id as u8, byte];
    unsafe {
        IOHIDDeviceSetReport(
            device,
            kIOHIDReportTypeFeature,
            report_id,
            payload.as_ptr(),
            payload.len() as isize,
        )
    }
}

unsafe extern "C" fn on_device_removed(
    context: *mut c_void,
    _result: IOReturn,
    _sender: *mut c_void,
    device: IOHIDDeviceRef,
) {
    let bridge = unsafe { &mut *(context as *mut Bridge) };
    bridge.devices.retain(|d| d.device != device);
    SPEC_PATH_DEVICE.store(
        bridge
            .devices
            .iter()
            .any(|d| d.control_path == ControlPath::SpecInputMode),
        Ordering::Relaxed,
    );
    log::info!("device removed");
    if bridge.devices.is_empty() {
        crate::status_item::set_status("Waiting for device…");
    }
}

unsafe extern "C" fn on_input_report(
    context: *mut c_void,
    _result: IOReturn,
    _sender: *mut c_void,
    _report_type: IOHIDReportType,
    _report_id: u32,
    report: *mut u8,
    report_length: isize,
) {
    let state = unsafe { &mut *(context as *mut DeviceState) };
    let bridge = unsafe { &mut *state.bridge };
    let bytes = unsafe { std::slice::from_raw_parts(report, report_length as usize) };

    if log::log_enabled!(log::Level::Trace) {
        log::trace!("input report ({} bytes): {}", bytes.len(), hex(bytes));
    }
    let Some(frame) = report::decode(&state.layout, bytes) else {
        log::debug!("decode failed for {}-byte report", bytes.len());
        return;
    };
    if log::log_enabled!(log::Level::Trace) {
        log::trace!(
            "  frame: contact_count={} scan_time={} button={} contacts={:?}",
            frame.contacts.len(),
            frame.scan_time_100us,
            frame.button,
            frame.contacts,
        );
    } else if log::log_enabled!(log::Level::Debug) {
        // Always log a one-line debug summary, even for empty frames:
        // `n=0` reports carry the lift transition (tip_switch=0 on the
        // last touching contact), and the silence-on-empty version of
        // this log made finger-up indistinguishable from the chip going
        // idle. All contacts are printed (not just the first) so 2F
        // gesture diagnosis doesn't have to back the second finger out
        // of centroid deltas.
        if frame.contacts.is_empty() {
            log::debug!("frame n=0 button={}", frame.button);
        } else {
            use std::fmt::Write;
            let mut s = String::with_capacity(32 * frame.contacts.len());
            for (i, c) in frame.contacts.iter().enumerate() {
                if i > 0 {
                    s.push(' ');
                }
                let _ = write!(
                    s,
                    "c{i} id={} at=({:>5.2},{:>5.2})mm tip={}",
                    c.id, c.x, c.y, c.tip,
                );
            }
            log::debug!(
                "frame n={} {} button={}",
                frame.contacts.len(),
                s,
                frame.button,
            );
        }
    }
    // Map the chip-side scan_time onto the host clock. Per-frame
    // deltas of `aligned_ts` track the device's scan-time deltas
    // (modulo MCU↔host clock drift), so any delivery jitter in
    // `Timestamp::now()` between the chip's scan instant and our
    // callback doesn't contaminate the dt the gesture engine reads.
    let aligned_ts = state
        .scan_clock
        .observe(frame.scan_time_100us, Timestamp::now());
    (bridge.on_frame)(frame, aligned_ts);
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && i % 4 == 0 {
            s.push(' ');
        }
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn build_match_dict(filter: &Filter) -> CFDictionary<CFString, CFType> {
    let mut pairs: Vec<(CFString, CFType)> = Vec::new();
    pairs.push((
        CFString::from_static_string(KEY_DEVICE_USAGE_PAGE),
        CFNumber::from(PTP_USAGE_PAGE).as_CFType(),
    ));
    pairs.push((
        CFString::from_static_string(KEY_DEVICE_USAGE),
        CFNumber::from(PTP_USAGE).as_CFType(),
    ));
    if let Some(v) = filter.vid {
        pairs.push((
            CFString::from_static_string(KEY_VENDOR_ID),
            CFNumber::from(v as i32).as_CFType(),
        ));
    }
    if let Some(p) = filter.pid {
        pairs.push((
            CFString::from_static_string(KEY_PRODUCT_ID),
            CFNumber::from(p as i32).as_CFType(),
        ));
    }
    CFDictionary::from_CFType_pairs(&pairs)
}

fn read_string_property(device: IOHIDDeviceRef, key: &str) -> Option<String> {
    let cfkey = CFString::new(key);
    let raw = unsafe { IOHIDDeviceGetProperty(device, cfkey.as_concrete_TypeRef() as *const _) };
    if raw.is_null() {
        return None;
    }
    let cfs: CFString = unsafe { CFString::wrap_under_get_rule(raw as *const _) };
    Some(cfs.to_string())
}

fn read_data_property(device: IOHIDDeviceRef, key: &str) -> Option<Vec<u8>> {
    let cfkey = CFString::new(key);
    let raw = unsafe { IOHIDDeviceGetProperty(device, cfkey.as_concrete_TypeRef() as *const _) };
    if raw.is_null() {
        return None;
    }
    let cfd: CFData = unsafe { CFData::wrap_under_get_rule(raw as *const _) };
    Some(cfd.bytes().to_vec())
}

fn read_number_property(device: IOHIDDeviceRef, key: &str) -> Option<i32> {
    let cfkey = CFString::new(key);
    let raw = unsafe { IOHIDDeviceGetProperty(device, cfkey.as_concrete_TypeRef() as *const _) };
    if raw.is_null() {
        return None;
    }
    let n: CFNumber = unsafe { CFNumber::wrap_under_get_rule(raw as *const _) };
    n.to_i32()
}
