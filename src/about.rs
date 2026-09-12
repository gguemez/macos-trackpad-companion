//! The About window: what this build is, in one place you can point at.
//!
//! A menu-bar agent has no Dock tile and no application menu, so the
//! usual "About <App>" route doesn't exist. Without this the version is
//! only visible in the menu's disabled header, which you can't select or
//! copy, and in the diagnostics blob two windows deep.
//!
//! Deliberately thin. Everything a bug report needs — permissions, the
//! device, the config values in force — is already behind Settings >
//! Copy Diagnostics, and duplicating it here would give two places to
//! keep in step. This window answers one question: which build is
//! running.
//!
//! Unlike the setup window there is nothing here to poll, so the close
//! path is an `NSWindowDelegate` rather than a timer noticing that the
//! window went away: with no timer to cancel, a poll would exist only to
//! drop the activation policy a second late.

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{AnyThread, MainThreadMarker, define_class, msg_send, sel};
use objc2_app_kit::{
    NSBackingStoreType, NSButton, NSColor, NSFont, NSImageView, NSTextField, NSWindow,
    NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

use crate::app_kit;

const WINDOW_W: f64 = 420.0;
const WINDOW_H: f64 = 190.0;
/// Left edge of the text column, clear of the icon.
const TEXT_X: f64 = 112.0;
const TEXT_W: f64 = 284.0;

thread_local! {
    static ABOUT: RefCell<Option<About>> = const { RefCell::new(None) };
}

struct About {
    window: Retained<NSWindow>,
    _actions: Retained<Actions>,
    _delegate: Retained<crate::window_lifecycle::CloseDelegate>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "TrackpadCompanionAboutActions"]
    struct Actions;

    impl Actions {
        #[unsafe(method(closeAbout:))]
        fn close_about(&self, _s: Option<&AnyObject>) {
            close();
        }
    }
);

impl Actions {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let _ = mtm;
        unsafe { msg_send![Self::alloc(), init] }
    }
}

/// Open the About window, creating it on first use.
pub fn show(mtm: MainThreadMarker) {
    let needs_window = ABOUT.with(|cell| cell.borrow().is_none());
    if needs_window {
        let built = About::build(mtm);
        ABOUT.with(|cell| *cell.borrow_mut() = Some(built));
    }
    let window = ABOUT.with(|cell| cell.borrow().as_ref().map(|a| a.window.clone()));
    let Some(window) = window else {
        return;
    };
    app_kit::activate_for_window(mtm);
    window.center();
    window.makeKeyAndOrderFront(None);
}

/// Close the way the title bar's own button does, so the teardown path
/// — the delegate dropping the activation policy — is the one already
/// proven by the red button rather than a second copy of it.
fn close() {
    // The borrow ends before performClose:, which is a message send:
    // holding one across a message send is how a re-entrant callback
    // turns into a panic.
    let window = ABOUT.with(|cell| cell.borrow().as_ref().map(|a| a.window.clone()));
    if let Some(window) = window {
        window.performClose(None);
    }
}

/// Nothing to flush or cancel — the delegate is installed purely for
/// the activation-policy settle it does on the way out.
fn on_close() {}

impl About {
    fn build(mtm: MainThreadMarker) -> Self {
        app_kit::ensure_app(mtm);

        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WINDOW_W, WINDOW_H));
        let window: Retained<NSWindow> = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc::<NSWindow>(),
                rect,
                NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        window.setTitle(&NSString::from_str("About Trackpad Companion"));
        // Same trap as every other window here: the default frees the
        // window on close, dangling the `Retained` held in ABOUT.
        unsafe { window.setReleasedWhenClosed(false) };
        app_kit::register_window(window.clone());

        let content = window
            .contentView()
            .expect("NSWindow auto-creates a contentView");

        // The bundle's own icon, so this shows `bridge.icns` in the
        // shipped app. Running the bare binary out of `target/release`
        // has no bundle and so no icon of its own; macOS substitutes the
        // generic executable icon, which is the honest answer for a
        // build that isn't the app.
        if let Some(icon) = app_kit::ensure_app(mtm).applicationIconImage() {
            let view = NSImageView::initWithFrame(
                mtm.alloc(),
                NSRect::new(NSPoint::new(28.0, 94.0), NSSize::new(64.0, 64.0)),
            );
            view.setImage(Some(&icon));
            content.addSubview(&view);
        }

        let name = label(mtm, "Trackpad Companion", TEXT_X, 132.0, TEXT_W, 24.0);
        name.setFont(Some(&NSFont::boldSystemFontOfSize(18.0)));
        content.addSubview(&name);

        let version = label(
            mtm,
            &format!("Version {}", env!("CARGO_PKG_VERSION")),
            TEXT_X,
            112.0,
            TEXT_W,
            18.0,
        );
        version.setFont(Some(&NSFont::systemFontOfSize(12.0)));
        version.setTextColor(Some(&NSColor::secondaryLabelColor()));
        content.addSubview(&version);

        let blurb = label(
            mtm,
            "Precision Touchpad gestures for macOS.",
            TEXT_X,
            90.0,
            TEXT_W,
            18.0,
        );
        blurb.setFont(Some(&NSFont::systemFontOfSize(12.0)));
        blurb.setTextColor(Some(&NSColor::secondaryLabelColor()));
        content.addSubview(&blurb);

        // The attribution the shipped binary owes its upstream. The
        // notices themselves ride along in Contents/Resources, where
        // Apache-2.0 section 4(a) wants them; this is the part a user
        // can actually find, and it names the copyright holder rather
        // than this fork, because the LICENSE-MIT we ship is his.
        //
        // Full width from the left margin rather than the text column:
        // these two lines sit below the icon, so the column's inset
        // would only make them wrap sooner for no alignment gained.
        for (i, line) in [
            "© 2026 Scott Lamb — MIT or Apache-2.0, at your option",
            "A fork of scottlamb/macos-trackpad-companion",
        ]
        .iter()
        .enumerate()
        {
            let note = label(
                mtm,
                line,
                24.0,
                68.0 - 18.0 * i as f64,
                WINDOW_W - 48.0,
                16.0,
            );
            note.setFont(Some(&NSFont::systemFontOfSize(11.0)));
            note.setTextColor(Some(&NSColor::secondaryLabelColor()));
            content.addSubview(&note);
        }

        let actions = Actions::new(mtm);
        let close_btn = unsafe {
            NSButton::buttonWithTitle_target_action(
                &NSString::from_str("Close"),
                Some(actions.as_ref() as &AnyObject),
                Some(sel!(closeAbout:)),
                mtm,
            )
        };
        close_btn.setFrame(NSRect::new(
            NSPoint::new(WINDOW_W - 24.0 - 96.0, 20.0),
            NSSize::new(96.0, 28.0),
        ));
        // The only button, and dismissal is the only thing to do here.
        close_btn.setKeyEquivalent(&NSString::from_str("\r"));
        content.addSubview(&close_btn);

        let delegate = crate::window_lifecycle::CloseDelegate::install(&window, on_close);
        Self {
            window,
            _actions: actions,
            _delegate: delegate,
        }
    }
}

fn label(
    mtm: MainThreadMarker,
    text: &str,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> Retained<NSTextField> {
    let field = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    field.setFrame(NSRect::new(NSPoint::new(x, y), NSSize::new(w, h)));
    field
}
