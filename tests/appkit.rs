//! Runs on the process main thread with a temporary config and local
//! AppKit events. No HID devices are opened or global CGEvents posted.

use std::cell::Cell;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use core_foundation::runloop::{CFRunLoop, kCFRunLoopDefaultMode};
use macos_trackpad_companion::{about, app_kit, config, scope, settings, status_item, update};
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

/// Serve one HTTP response on a loopback port, then close.
///
/// A real socket rather than a mocked `NSURLSession`: the point of the
/// exercise is the path the shipped app actually takes — status line,
/// headers, body, and Foundation's own parsing of all three.
fn serve_once(status: &'static str, body: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            // The request is read but not inspected; the client will
            // not look at the response until its own write completes.
            let _ = stream.read(&mut [0u8; 1024]);
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    port
}

/// Run one check to completion. `update::fetch` reports on a session
/// queue, so the result crosses back through a lock rather than being
/// returned.
fn fetch_blocking(url: &str) -> update::Outcome {
    let slot: Arc<Mutex<Option<update::Outcome>>> = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&slot);
    update::fetch(url, move |outcome| *sink.lock().unwrap() = Some(outcome));

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(outcome) = slot.lock().unwrap().take() {
            return outcome;
        }
        assert!(Instant::now() < deadline, "update check never finished: {url}");
        pump(25);
    }
}

/// Run one download to completion, the same way `fetch_blocking` does.
fn download_blocking(feed: &update::Feed) -> Result<std::path::PathBuf, String> {
    let slot: Arc<Mutex<Option<Result<std::path::PathBuf, String>>>> = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&slot);
    update::download(feed, move |result| *sink.lock().unwrap() = Some(result));

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(result) = slot.lock().unwrap().take() {
            return result;
        }
        assert!(Instant::now() < deadline, "update download never finished");
        pump(25);
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
    // The controls build() places are the window's direct subviews; a
    // recursive walk would drag in each slider's own internals, which do
    // legitimately sit on top of their slider. Absolute frames on a
    // hand-laid-out window have no layout engine to catch a collision,
    // so this is the only thing that will.
    // A bare NSView is a container — the scope's tuning column is one,
    // and its buttons are meant to sit inside it. Every control is a
    // subclass, so this excludes exactly the groups. "Has subviews"
    // would not: NSSlider has internals of its own.
    let assert_no_overlap = |w: &NSWindow| {
        let placed: Vec<_> = w
            .contentView()
            .unwrap()
            .subviews()
            .into_iter()
            .filter(|v| v.class().name().to_bytes() != b"NSView")
            .collect();
        for (i, a) in placed.iter().enumerate() {
            for b in placed.iter().skip(i + 1) {
                let (fa, fb) = (a.frame(), b.frame());
                let overlap = fa.origin.x < fb.origin.x + fb.size.width
                    && fb.origin.x < fa.origin.x + fa.size.width
                    && fa.origin.y < fb.origin.y + fb.size.height
                    && fb.origin.y < fa.origin.y + fa.size.height;
                assert!(
                    !overlap,
                    "{} overlaps {} in \"{}\": {fa:?} vs {fb:?}",
                    a.class().name().to_string_lossy(),
                    b.class().name().to_string_lossy(),
                    w.title(),
                );
            }
        }
    };
    for w in [&settings_window, &scope_window] {
        assert_no_overlap(w);
    }
    println!("passed: no control overlaps another in the settings or scope window");

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
    // The attribution is a licensing obligation, not decoration, so it
    // is asserted rather than left to whoever next edits the layout.
    // The notices themselves ship in Contents/Resources; this is the
    // line that tells a user whose work this is and under what terms.
    assert!(
        labels.iter().any(|l| l.contains("Scott Lamb")),
        "About must name the copyright holder: {labels:?}"
    );
    assert!(
        labels.iter().any(|l| l.contains("Apache-2.0")),
        "About must name the license: {labels:?}"
    );
    // Six hand-placed controls now, and the two notices sit between the
    // blurb and the Close button with no layout engine to referee.
    assert_no_overlap(&about_window);
    // A label narrower than its text truncates to an ellipsis in
    // silence, which for an attribution line means shipping half a
    // copyright notice. Overlap checking would not notice: the frames
    // are fine, it is the string that does not fit inside one.
    for v in descendants(&about_window.contentView().unwrap())
        .into_iter()
        .filter_map(|v| v.downcast::<NSTextField>().ok())
    {
        let (text, frame, fitting) = (v.stringValue().to_string(), v.frame(), v.fittingSize());
        assert!(
            fitting.width <= frame.size.width,
            "\"{text}\" needs {}pt but its label is {}pt wide",
            fitting.width,
            frame.size.width,
        );
    }
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

    // ---- update checking, over a real loopback socket ----------------

    let port = serve_once(
        "200 OK",
        r#"{"version":"99.0.0","url":"https://example.invalid/x.dmg",
            "sha256":"0123","size":1024}"#,
    );
    match fetch_blocking(&format!("http://127.0.0.1:{port}/appcast.json")) {
        update::Outcome::Available(feed) => {
            assert_eq!(feed.version, "99.0.0");
            assert_eq!(feed.url, "https://example.invalid/x.dmg");
            assert_eq!(feed.size, Some(1024));
        }
        other => panic!("a newer feed must read as available, got {other:?}"),
    }

    // The version this build reports is the one compared against, so a
    // feed naming it exactly is not an update.
    let body: &'static str = Box::leak(
        format!(
            r#"{{"version":"{}","url":"https://example.invalid/x.dmg"}}"#,
            env!("CARGO_PKG_VERSION")
        )
        .into_boxed_str(),
    );
    let port = serve_once("200 OK", body);
    assert!(
        matches!(
            fetch_blocking(&format!("http://127.0.0.1:{port}/appcast.json")),
            update::Outcome::UpToDate
        ),
        "a feed naming this exact version is not an update"
    );

    let port = serve_once("200 OK", r#"{"version":"0.0.1","url":"https://e.invalid/x.dmg"}"#);
    assert!(
        matches!(
            fetch_blocking(&format!("http://127.0.0.1:{port}/appcast.json")),
            update::Outcome::UpToDate
        ),
        "an older feed must never offer a downgrade"
    );

    // A missing feed must say so rather than reaching the JSON parser:
    // an error page served with a 404 would otherwise be reported as a
    // malformed feed, which points at the wrong thing.
    let port = serve_once("404 Not Found", "<html>nope</html>");
    match fetch_blocking(&format!("http://127.0.0.1:{port}/appcast.json")) {
        update::Outcome::Failed(why) => assert!(why.contains("404"), "{why}"),
        other => panic!("a 404 must fail the check, got {other:?}"),
    }

    let port = serve_once("200 OK", "{not json at all");
    assert!(
        matches!(
            fetch_blocking(&format!("http://127.0.0.1:{port}/appcast.json")),
            update::Outcome::Failed(_)
        ),
        "a malformed feed must fail rather than parse to something"
    );

    // Nothing listening: the failure a machine offline at launch hits.
    let dead = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_port = dead.local_addr().unwrap().port();
    drop(dead);
    assert!(
        matches!(
            fetch_blocking(&format!("http://127.0.0.1:{dead_port}/appcast.json")),
            update::Outcome::Failed(_)
        ),
        "an unreachable feed must fail the check, not hang"
    );

    assert!(
        matches!(fetch_blocking("not a url at all"), update::Outcome::Failed(_)),
        "a malformed feed URL must fail before any request"
    );
    println!("passed: update checking reports available, up-to-date and every failure");

    // ---- downloading and verifying -----------------------------------

    const PAYLOAD: &str = "trackpad companion update payload";
    const PAYLOAD_SHA: &str = "828d47e187c2d8dfd90be95c7c67730c064e57bf4db9ffae83235b0672ffdaa9";
    let staged_at = std::path::PathBuf::from(std::env::var("HOME").unwrap())
        .join("Library/Application Support/macos-trackpad-companion/Updates")
        .join("Trackpad-Companion-99.0.0.dmg");
    let _ = std::fs::remove_file(&staged_at);

    let artifact = |port: u16, sha: Option<&str>, size: Option<u64>| update::Feed {
        version: "99.0.0".into(),
        url: format!("http://127.0.0.1:{port}/Trackpad-Companion-99.0.0.dmg"),
        notes: None,
        sha256: sha.map(str::to_string),
        size,
    };

    let port = serve_once("200 OK", PAYLOAD);
    let landed = download_blocking(&artifact(port, Some(PAYLOAD_SHA), Some(PAYLOAD.len() as u64)))
        .expect("an artifact matching its feed must verify");
    assert_eq!(landed, staged_at, "staged under the name the feed implies");
    assert_eq!(std::fs::read_to_string(&landed).unwrap(), PAYLOAD);

    // A checksum that does not match must fail *and* take the file with
    // it — a rejected artifact left on disk is one someone finds later
    // and trusts.
    let port = serve_once("200 OK", PAYLOAD);
    let why = download_blocking(&artifact(port, Some(&"0".repeat(64)), None))
        .expect_err("a mismatched checksum must fail the download");
    assert!(why.contains("SHA-256"), "{why}");
    assert!(!staged_at.exists(), "a rejected download must not be left on disk");

    // Same for a size that disagrees, which is the cheap check that
    // catches a truncated transfer before anything is hashed.
    let port = serve_once("200 OK", PAYLOAD);
    let why = download_blocking(&artifact(port, None, Some(999_999)))
        .expect_err("a mismatched size must fail the download");
    assert!(why.contains("999999"), "{why}");
    assert!(!staged_at.exists());

    // An HTTP error is reported as one rather than being saved.
    let port = serve_once("503 Service Unavailable", "down for maintenance");
    let why = download_blocking(&artifact(port, None, None))
        .expect_err("a 503 must fail the download");
    assert!(why.contains("503"), "{why}");
    assert!(!staged_at.exists());

    let _ = std::fs::remove_dir(staged_at.parent().unwrap());
    println!("passed: downloads verify their checksum and size, and leave nothing behind when they don't");

    // The quiet check's entire UI is this rename, so assert it against
    // the menu AppKit is really holding.
    let status = status_item::StatusItem::install(mtm);
    let menu = status.menu(mtm).expect("the status item carries its menu");
    let titles = || -> Vec<String> {
        (0..menu.numberOfItems())
            .filter_map(|i| menu.itemAtIndex(i))
            .map(|item| item.title().to_string())
            .collect()
    };
    assert!(
        titles().iter().any(|t| t == "Check for Updates…"),
        "{:?}",
        titles()
    );
    status_item::set_update_title(Some("Update to 99.0.0…"));
    assert!(
        titles().iter().any(|t| t == "Update to 99.0.0…"),
        "a found update must be visible in the menu: {:?}",
        titles()
    );
    // …and back, which is what a finished download restores it to.
    status_item::set_update_title(None);
    assert!(
        titles().iter().any(|t| t == "Check for Updates…"),
        "{:?}",
        titles()
    );
    drop(status);
    println!("passed: the menu offers a check and renames it when one is found");

    std::fs::remove_dir_all(dir).unwrap();
}
