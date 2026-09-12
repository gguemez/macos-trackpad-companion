//! Runs on the process main thread with a temporary config and local
//! AppKit events. No HID devices are opened or global CGEvents posted.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use core_foundation::runloop::{CFRunLoop, kCFRunLoopDefaultMode};
use macos_trackpad_companion::{about, app_kit, config, scope, settings};
use objc2::rc::Retained;
use objc2::{ClassType, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSButton, NSEvent, NSEventModifierFlags,
    NSEventType, NSSlider, NSTextField, NSView, NSWindow,
};
use objc2_foundation::{NSPoint, NSSize, NSString};

thread_local! {
    static KEYBOARD_NAVIGATION: Cell<bool> = const { Cell::new(false) };
}

// Exercise both keyboard-navigation modes inside this process without
// editing the user's system preferences.
define_class!(
    #[unsafe(super(NSApplication))]
    #[thread_kind = MainThreadOnly]
    #[name = "TrackpadCompanionTestApplication"]
    struct TestApplication;

    impl TestApplication {
        #[unsafe(method(isFullKeyboardAccessEnabled))]
        fn full_keyboard_access(&self) -> bool { KEYBOARD_NAVIGATION.with(Cell::get) }
    }
);

fn pump(ms: u64) {
    let deadline = std::time::Instant::now() + Duration::from_millis(ms);
    while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
        CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, remaining, false);
    }
}

fn descendants(view: &NSView) -> Vec<Retained<NSView>> {
    let mut result = Vec::new();
    for child in view.subviews() {
        result.extend(descendants(&child));
        result.push(child);
    }
    result
}

fn window(app: &NSApplication, title: &str) -> Retained<NSWindow> {
    app.windows()
        .into_iter()
        .find(|w| w.title().to_string().contains(title))
        .unwrap()
}

fn sensitivity(window: &NSWindow) -> Retained<NSSlider> {
    descendants(&window.contentView().unwrap())
        .into_iter()
        .filter_map(|v| v.downcast::<NSSlider>().ok())
        .find(|s| s.minValue() == 5.0 && s.maxValue() == 80.0)
        .unwrap()
}

fn send_action(slider: &NSSlider) {
    let target = slider.target();
    unsafe {
        slider.sendAction_to(slider.action(), target.as_deref());
    }
}

fn click_slider(app: &NSApplication, window: &NSWindow, slider: &NSSlider) {
    let point =
        slider.convertPoint_toView(NSPoint::new(slider.bounds().size.width / 2.0, 10.0), None);
    let mouse = |kind| {
        NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
        kind, point, NSEventModifierFlags::empty(), 0.0, window.windowNumber(), None, 1, 1, 0.0,
    ).unwrap()
    };
    app.postEvent_atStart(&mouse(NSEventType::LeftMouseUp), true);
    window.sendEvent(&mouse(NSEventType::LeftMouseDown));
}

fn right_arrow(window: &NSWindow) {
    let arrow = NSString::from_str("\u{F703}");
    let key = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
        NSEventType::KeyDown, NSPoint::ZERO, NSEventModifierFlags::empty(), 0.0,
        window.windowNumber(), None, &arrow, &arrow, false, 124,
    ).unwrap();
    window.sendEvent(&key);
}

struct Transport {
    reenter: Rc<Cell<bool>>,
}
impl scope::Transport for Transport {
    fn toggle_play(&self) {}
    fn step(&self, _: i64) {}
    fn seek(&self, _: usize) {}
    fn position(&self) -> (usize, usize, bool) {
        if self.reenter.replace(false) {
            scope::show(MainThreadMarker::new().unwrap());
        }
        (0, 10, false)
    }
}

fn main() {
    let mtm = MainThreadMarker::new().expect("AppKit tests require the process main thread");
    let app: Retained<TestApplication> =
        unsafe { msg_send![TestApplication::class(), sharedApplication] };
    let dir = std::env::temp_dir().join(format!("tpc-appkit-tests-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    std::fs::write(&path, "[cursor]\nsensitivity = 25.0\n").unwrap();
    settings::set_config_path(path.clone());
    let calls = Rc::new(Cell::new(0));
    let seen = calls.clone();
    scope::set_live_apply(move |_| seen.set(seen.get() + 1));
    let reenter = Rc::new(Cell::new(false));
    scope::set_transport(Box::new(Transport {
        reenter: reenter.clone(),
    }));
    scope::show(mtm);
    let scope_window = window(&app, "Scope");
    let slider = sensitivity(&scope_window);

    for enabled in [false, true] {
        KEYBOARD_NAVIGATION.with(|v| v.set(enabled));
        assert_eq!(app.isFullKeyboardAccessEnabled(), enabled);
        calls.set(0);
        click_slider(&app, &scope_window, &slider);
        assert_eq!(
            calls.get(),
            1,
            "tracking must commit exactly once, at the end"
        );
        let before = slider.doubleValue();
        right_arrow(&scope_window);
        assert!(
            slider.doubleValue() > before,
            "clicked slider must accept arrow keys"
        );
        assert_eq!(calls.get(), 2, "keyboard changes apply immediately");
    }
    println!("passed: tracking commits once and keyboard focus works in both navigation modes");

    let content = scope_window.contentView().unwrap();
    let label = descendants(&content)
        .into_iter()
        .filter_map(|v| v.downcast::<NSTextField>().ok())
        .find(|v| v.stringValue().to_string() == "Speed")
        .unwrap();
    let label_before = label.convertPoint_toView(NSPoint::ZERO, None);
    let slider_before = slider.convertPoint_toView(NSPoint::ZERO, None);
    let size = content.frame().size;
    // NSWindow may clamp to a small test display; exercise the real view
    // autoresizing machinery without depending on that display's size.
    content.setFrameSize(NSSize::new(size.width + 200.0, size.height + 100.0));
    let label_after = label.convertPoint_toView(NSPoint::ZERO, None);
    let slider_after = slider.convertPoint_toView(NSPoint::ZERO, None);
    assert_eq!(label_after.x - label_before.x, 200.0);
    assert_eq!(slider_after.x - slider_before.x, 200.0);
    assert_eq!(label_after.y - label_before.y, 100.0);
    assert_eq!(slider_after.y - slider_before.y, 100.0);
    content.setFrameSize(size);
    println!("passed: tuning labels and sliders resize together");

    reenter.set(true);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while reenter.get() && std::time::Instant::now() < deadline {
        pump(50);
    }
    assert!(
        !reenter.get(),
        "transport position must be called outside the window borrow"
    );
    scope::set_live_apply(move |_| {
        // Re-enter both the window and hook registries from the callback.
        scope::show(MainThreadMarker::new().unwrap());
        scope::set_live_apply(|_| {});
    });
    slider.setDoubleValue(60.0);
    send_action(&slider);
    scope_window.performClose(None);
    assert!(!scope::is_open(), "close cleanup must be immediate");
    assert_eq!(
        config::load(Some(&path)).unwrap().0.cursor.sensitivity,
        60.0
    );
    std::fs::write(&path, "[cursor]\nsensitivity = 77.0\n").unwrap();
    // A final tracking action arriving after close must not re-arm a write.
    slider.setDoubleValue(12.0);
    send_action(&slider);
    pump(350);
    assert_eq!(
        config::load(Some(&path)).unwrap().0.cursor.sensitivity,
        77.0
    );
    scope::show(mtm);
    assert_eq!(slider.doubleValue(), 77.0);
    assert!(scope_window.isVisible());
    println!("passed: scope callbacks can re-enter; close flushes once; window reopens");

    settings::show(mtm);
    let settings_window = window(&app, "Settings");
    let settings_slider = sensitivity(&settings_window);
    click_slider(&app, &settings_window, &settings_slider);
    let before = settings_slider.doubleValue();
    right_arrow(&settings_window);
    assert!(settings_slider.doubleValue() > before);
    settings_slider.setDoubleValue(55.0);
    send_action(&settings_slider);
    settings_window.performClose(None);
    assert_eq!(
        config::load(Some(&path)).unwrap().0.cursor.sensitivity,
        55.0
    );
    assert_eq!(
        app.activationPolicy(),
        NSApplicationActivationPolicy::Regular,
        "other window remains visible"
    );
    app_kit::ensure_app(mtm);
    assert_eq!(
        app.activationPolicy(),
        NSApplicationActivationPolicy::Regular,
        "setup must not reset policy"
    );
    std::fs::write(&path, "[cursor]\nsensitivity = 66.0\n").unwrap();
    settings_slider.setDoubleValue(12.0);
    send_action(&settings_slider);
    pump(350);
    assert_eq!(
        config::load(Some(&path)).unwrap().0.cursor.sensitivity,
        66.0
    );
    settings::show(mtm);
    assert_eq!(settings_slider.doubleValue(), 66.0);
    settings_window.performClose(None);
    scope_window.performClose(None);
    assert_eq!(
        app.activationPolicy(),
        NSApplicationActivationPolicy::Accessory
    );
    println!(
        "passed: settings focus, close/reopen, timer cancellation and multi-window activation"
    );
    about::show(mtm);
    let about_window = window(&app, "About Trackpad Companion");
    assert_eq!(
        app.activationPolicy(),
        NSApplicationActivationPolicy::Regular,
        "About must be able to take focus"
    );
    // The version on screen is the crate's, not a second copy someone
    // has to remember to bump.
    let labels: Vec<String> = descendants(&about_window.contentView().unwrap())
        .into_iter()
        .filter_map(|v| v.downcast::<NSTextField>().ok())
        .map(|v| v.stringValue().to_string())
        .collect();
    assert!(
        labels.contains(&format!("Version {}", env!("CARGO_PKG_VERSION"))),
        "{labels:?}"
    );
    // The button, not performClose: directly — a Close that stopped
    // reaching the window would otherwise pass this test.
    let about_close = descendants(&about_window.contentView().unwrap())
        .into_iter()
        .filter_map(|v| v.downcast::<NSButton>().ok())
        .find(|b| b.title().to_string() == "Close")
        .expect("About has a Close button");
    unsafe { about_close.performClick(None) };
    assert!(!about_window.isVisible());
    assert_eq!(
        app.activationPolicy(),
        NSApplicationActivationPolicy::Accessory,
        "closing the last window settles back to an agent"
    );
    about::show(mtm);
    assert!(about_window.isVisible(), "About reopens the same window");
    about_window.performClose(None);
    println!("passed: about shows the crate version, closes by button, and reopens");

    std::fs::remove_dir_all(dir).unwrap();
}
