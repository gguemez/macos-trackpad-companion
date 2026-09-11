//! Menu-bar status item — the only UI the companion presents.
//!
//! The icon is a macOS *template* image: pure black on transparency,
//! with `setTemplate(true)` letting AppKit invert it for dark menu
//! bars, selection highlighting, and Reduce Transparency. The PNG is
//! compiled into the binary rather than loaded from the bundle's
//! Resources, so the status item works the same when running the plain
//! CLI binary out of `target/release`.
//!
//! Quit raises `SIGTERM` at itself rather than calling
//! `[NSApp terminate:]`. terminate: calls `exit()`, which skips every
//! Rust destructor — including `hid::DeviceState::drop`, the thing that
//! writes the firmware back to mouse mode. Raising the signal instead
//! lands in the existing sigwait worker, so Quit and Ctrl+C take the
//! exact same shutdown path.

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{AnyThread, MainThreadMarker, define_class, msg_send, sel};
use objc2_app_kit::{
    NSImage, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{NSData, NSSize, NSString};

use crate::app_kit;

/// 40x40 @2x template rendered from `assets/icons/bridge-template.svg`.
/// Drawn at 20pt; AppKit picks the right scale from the declared size.
const TEMPLATE_PNG: &[u8] = include_bytes!("../assets/icons/bridge-template-40.png");

/// Menu-bar icons are measured in points, independent of the PNG's
/// pixel dimensions.
const ICON_POINTS: f64 = 20.0;

thread_local! {
    /// The disabled menu line that reports device state. Lives in a
    /// thread-local because `Retained` is `!Send` and only the main
    /// thread ever touches AppKit; [`set_status`] is a no-op on any
    /// other thread, and on the main thread before `install`.
    static STATUS_LINE: RefCell<Option<Retained<NSMenuItem>>> = const { RefCell::new(None) };
}

/// Update the menu's device-state line. Safe to call before the status
/// item exists (the plain CLI path never installs one) and from the
/// HID layer as devices come and go.
pub fn set_status(text: &str) {
    STATUS_LINE.with(|cell| {
        if let Some(item) = cell.borrow().as_ref() {
            item.setTitle(&NSString::from_str(text));
        }
    });
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "TrackpadCompanionMenuTarget"]
    struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(openSetup:))]
        fn open_setup(&self, _sender: Option<&AnyObject>) {
            // Menu actions always arrive on the main thread.
            if let Some(mtm) = MainThreadMarker::new() {
                crate::onboarding::show(mtm);
            }
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            log::info!("quit requested from menu");
            // Straight into the sigwait worker installed by
            // `hid::Manager::run`, which stops the event loop and lets
            // `main` unwind so the firmware gets reverted.
            crate::hid::request_shutdown();
        }
    }
);

impl MenuTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let _ = mtm;
        unsafe { msg_send![Self::alloc(), init] }
    }
}

/// Live status item. Dropping it removes the icon from the menu bar, so
/// `main` holds it for the lifetime of the daemon.
pub struct StatusItem {
    item: Retained<NSStatusItem>,
    /// The menu's `target` is an unretained reference on the AppKit
    /// side, so the target has to outlive the menu.
    _target: Retained<MenuTarget>,
}

impl StatusItem {
    /// Install the status item. Must run on the main thread, before the
    /// event loop starts.
    pub fn install(mtm: MainThreadMarker) -> Self {
        app_kit::ensure_app(mtm);

        let status_bar = NSStatusBar::systemStatusBar();
        let item = status_bar.statusItemWithLength(NSVariableStatusItemLength);

        if let Some(button) = item.button(mtm) {
            let data = NSData::with_bytes(TEMPLATE_PNG);
            if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
                image.setSize(NSSize::new(ICON_POINTS, ICON_POINTS));
                image.setTemplate(true);
                button.setImage(Some(&image));
            } else {
                log::warn!("status item: embedded template PNG failed to decode");
            }
        } else {
            log::warn!("status item has no button; icon not shown");
        }

        let target = MenuTarget::new(mtm);
        let menu = Self::build_menu(mtm, &target);
        item.setMenu(Some(&menu));

        log::debug!("status item installed");
        Self {
            item,
            _target: target,
        }
    }

    fn build_menu(mtm: MainThreadMarker, target: &Retained<MenuTarget>) -> Retained<NSMenu> {
        let menu = NSMenu::initWithTitle(mtm.alloc::<NSMenu>(), &NSString::from_str(""));

        // Disabled header, so the menu says what it belongs to.
        let header = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc::<NSMenuItem>(),
                &NSString::from_str("Trackpad Companion"),
                None,
                &NSString::from_str(""),
            )
        };
        header.setEnabled(false);
        menu.addItem(&header);

        // Device state. Updated by the HID layer via `set_status`; the
        // initial text is what shows until the first open attempt
        // resolves.
        let status = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc::<NSMenuItem>(),
                &NSString::from_str("Starting…"),
                None,
                &NSString::from_str(""),
            )
        };
        status.setEnabled(false);
        menu.addItem(&status);
        STATUS_LINE.with(|cell| *cell.borrow_mut() = Some(status));

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        // Reopening matters: the setup window auto-opens once at launch,
        // and without this there is no way back to it.
        let setup = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc::<NSMenuItem>(),
                &NSString::from_str("Setup…"),
                Some(sel!(openSetup:)),
                &NSString::from_str(""),
            )
        };
        unsafe { setup.setTarget(Some(target.as_ref() as &AnyObject)) };
        menu.addItem(&setup);

        let quit = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc::<NSMenuItem>(),
                &NSString::from_str("Quit"),
                Some(sel!(quit:)),
                &NSString::from_str("q"),
            )
        };
        unsafe { quit.setTarget(Some(target.as_ref() as &AnyObject)) };
        menu.addItem(&quit);

        menu
    }
}

impl Drop for StatusItem {
    fn drop(&mut self) {
        STATUS_LINE.with(|cell| *cell.borrow_mut() = None);
        let status_bar = NSStatusBar::systemStatusBar();
        status_bar.removeStatusItem(&self.item);
    }
}
