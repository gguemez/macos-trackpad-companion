//! On-demand USB lifetime: launchd starts us; IOKit tells us when to exit.
//!
//! Consume com.apple.iokit.matching events before exiting, otherwise the
//! pending event can cause launchd to restart us repeatedly. Device discovery
//! and touch reports remain owned by hid::Manager.

use std::cell::Cell;
use std::collections::HashSet;
use std::ffi::{c_char, c_void};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use anyhow::{Result, bail};
use block2::{Block, RcBlock};
use core_foundation::base::TCFType;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes};
use core_foundation::string::CFString;
use core_foundation_sys::base::CFRetain;
use core_foundation_sys::runloop::{CFRunLoopAddSource, CFRunLoopRemoveSource, CFRunLoopSourceRef};

use crate::run_loop_timer::Timer;

unsafe extern "C" {
    fn xpc_set_event_stream_handler(
        stream: *const c_char,
        queue: *mut c_void,
        handler: &Block<dyn Fn(*mut c_void)>,
    );
    fn xpc_dictionary_get_uint64(event: *mut c_void, key: *const c_char) -> u64;
    fn IONotificationPortCreate(port: u32) -> *mut c_void;
    fn IONotificationPortDestroy(port: *mut c_void);
    fn IONotificationPortGetRunLoopSource(port: *mut c_void) -> CFRunLoopSourceRef;
    fn IOServiceAddMatchingNotification(
        port: *mut c_void,
        kind: *const c_char,
        matching: *const c_void,
        callback: unsafe extern "C" fn(*mut c_void, u32),
        context: *mut c_void,
        iterator: *mut u32,
    ) -> i32;
    fn IOIteratorNext(iterator: u32) -> u32;
    fn IOObjectRelease(object: u32) -> i32;
    fn IORegistryEntryGetRegistryEntryID(entry: u32, id: *mut u64) -> i32;
}

/// Register once, before config loading and instance locking, so even an
/// early error or duplicate launch can consume its activation event.
pub struct LaunchEvents {
    received: Receiver<()>,
}

impl LaunchEvents {
    pub fn listen() -> Self {
        let (sender, received) = mpsc::sync_channel(1);
        let handler = RcBlock::new(move |event: *mut c_void| {
            let id =
                unsafe { xpc_dictionary_get_uint64(event, c"IOMatchLaunchServiceID".as_ptr()) };
            log::debug!("consumed USB launch event for registry entry {id}");
            let _ = sender.try_send(());
        });
        // XPC copies the escaping block. NULL selects its default queue,
        // which also consumes events before the AppKit loop has started.
        unsafe {
            xpc_set_event_stream_handler(
                c"com.apple.iokit.matching".as_ptr(),
                std::ptr::null_mut(),
                &handler,
            );
        }
        Self { received }
    }
}

impl Drop for LaunchEvents {
    fn drop(&mut self) {
        // In the quick "already unplugged" / duplicate / config-error case,
        // let XPC deliver the pending activation before the process exits.
        // A normal session already received it, so this returns immediately.
        let _ = self.received.recv_timeout(Duration::from_secs(2));
    }
}

struct Presence {
    entries: HashSet<u64>,
    connected: Rc<Cell<bool>>,
    exit_timer: Option<Timer>,
    on_unplugged: Rc<dyn Fn()>,
}

impl Presence {
    fn new(on_unplugged: impl Fn() + 'static) -> Self {
        Self {
            entries: HashSet::new(),
            connected: Rc::new(Cell::new(false)),
            exit_timer: None,
            on_unplugged: Rc::new(on_unplugged),
        }
    }

    fn update(&mut self, id: u64, attached: bool) {
        let changed = if attached {
            self.entries.insert(id)
        } else {
            self.entries.remove(&id)
        };
        if !changed {
            return;
        }
        self.connected.set(!self.entries.is_empty());
        self.exit_timer = None;
        if self.connected.get() {
            log::info!("USB device present (registry entry {id})");
        } else {
            let connected = self.connected.clone();
            let on_unplugged = self.on_unplugged.clone();
            // One-shot grace period, cancelled on reattach. No polling.
            self.exit_timer = Some(Timer::new(0.5, 0.0, move || {
                if !connected.get() {
                    log::info!("USB device unplugged; exiting until the next connection");
                    on_unplugged();
                }
            }));
        }
    }
}

/// Owns physical USB notifications, independent of HID permissions or the
/// supported-interface callbacks. Registering notifications opens no device.
pub struct Watch {
    port: *mut c_void,
    source: CFRunLoopSourceRef,
    run_loop: CFRunLoop,
    matched: u32,
    removed: u32,
    state: Box<Presence>,
}

impl Watch {
    pub fn new(vid: u16, pid: u16) -> Result<Self> {
        let port = unsafe { IONotificationPortCreate(0) };
        if port.is_null() {
            bail!("could not create USB notification port");
        }
        let mut watch = Self {
            port,
            source: unsafe { IONotificationPortGetRunLoopSource(port) },
            run_loop: CFRunLoop::get_current(),
            matched: 0,
            removed: 0,
            state: Box::new(Presence::new(crate::app_kit::request_stop)),
        };
        if watch.source.is_null() {
            bail!("could not create USB notification source");
        }
        // Subscribe to removal first, so an entry that disappears while
        // setting up the initial match iterator cannot be left in our set.
        for (kind, callback, iterator) in [
            (
                c"IOServiceTerminate",
                detached as unsafe extern "C" fn(*mut c_void, u32),
                &mut watch.removed,
            ),
            (
                c"IOServiceFirstMatch",
                attached as unsafe extern "C" fn(*mut c_void, u32),
                &mut watch.matched,
            ),
        ] {
            let matching = CFDictionary::from_CFType_pairs(&[
                (
                    CFString::new("IOProviderClass"),
                    CFString::new("IOUSBHostDevice").as_CFType(),
                ),
                (
                    CFString::new("idVendor"),
                    CFNumber::from(i32::from(vid)).as_CFType(),
                ),
                (
                    CFString::new("idProduct"),
                    CFNumber::from(i32::from(pid)).as_CFType(),
                ),
            ]);
            // IOKit consumes one reference to the matching dictionary.
            let result = unsafe {
                CFRetain(matching.as_CFTypeRef());
                IOServiceAddMatchingNotification(
                    watch.port,
                    kind.as_ptr(),
                    matching.as_concrete_TypeRef().cast(),
                    callback,
                    (&mut *watch.state as *mut Presence).cast(),
                    iterator,
                )
            };
            if result != 0 {
                bail!("could not register USB notification: {result:#x}");
            }
        }
        // Drain both iterators to arm notifications and discover a pad
        // already attached. Delivery starts only after adding the source.
        unsafe {
            attached((&mut *watch.state as *mut Presence).cast(), watch.matched);
            detached((&mut *watch.state as *mut Presence).cast(), watch.removed);
            CFRunLoopAddSource(
                watch.run_loop.as_concrete_TypeRef(),
                watch.source,
                kCFRunLoopCommonModes,
            );
        }
        Ok(watch)
    }

    pub fn is_present(&self) -> bool {
        self.state.connected.get()
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.state.exit_timer = None;
        unsafe {
            if !self.source.is_null() {
                CFRunLoopRemoveSource(
                    self.run_loop.as_concrete_TypeRef(),
                    self.source,
                    kCFRunLoopCommonModes,
                );
            }
            if self.matched != 0 {
                IOObjectRelease(self.matched);
            }
            if self.removed != 0 {
                IOObjectRelease(self.removed);
            }
            IONotificationPortDestroy(self.port);
        }
    }
}

unsafe extern "C" fn attached(context: *mut c_void, iterator: u32) {
    unsafe { drain(context, iterator, true) };
}

unsafe extern "C" fn detached(context: *mut c_void, iterator: u32) {
    unsafe { drain(context, iterator, false) };
}

unsafe fn drain(context: *mut c_void, iterator: u32, attached: bool) {
    loop {
        let entry = unsafe { IOIteratorNext(iterator) };
        if entry == 0 {
            break;
        }
        let mut id = 0;
        let result = unsafe { IORegistryEntryGetRegistryEntryID(entry, &mut id) };
        unsafe { IOObjectRelease(entry) };
        if result == 0 {
            unsafe { &mut *context.cast::<Presence>() }.update(id, attached);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_waits_for_the_last_device_and_reattach_cancels_it() {
        let calls = Rc::new(Cell::new(0));
        let seen = calls.clone();
        let mut state = Presence::new(move || seen.set(seen.get() + 1));
        state.update(1, true);
        state.update(1, true); // duplicate notification
        state.update(2, true);
        state.update(1, false);
        assert!(state.connected.get());
        assert!(state.exit_timer.is_none());
        state.update(2, false);
        assert!(!state.connected.get());
        assert!(state.exit_timer.is_some());
        state.update(3, true);
        assert!(state.connected.get());
        assert!(state.exit_timer.is_none());
        state.update(99, false); // unrelated removal
        assert!(state.connected.get());
        CFRunLoop::run_in_mode(
            unsafe { core_foundation::runloop::kCFRunLoopDefaultMode },
            Duration::from_millis(600),
            false,
        );
        assert_eq!(calls.get(), 0, "reattach must cancel the exit callback");
        state.update(3, false);
        CFRunLoop::run_in_mode(
            unsafe { core_foundation::runloop::kCFRunLoopDefaultMode },
            Duration::from_millis(600),
            false,
        );
        assert_eq!(calls.get(), 1, "last removal must exit exactly once");
    }

    #[test]
    fn physical_usb_watch_can_be_created_and_dropped_without_hid_access() {
        // Impossible test VID/PID: exercises real IOKit registration and
        // initial iterator draining, without touching the user's devices.
        let watch = Watch::new(0xffff, 0xffff).unwrap();
        assert!(!watch.is_present());
    }
}
