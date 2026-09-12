//! Replay a recorded frame stream through the gesture engine.
//!
//! `companion --record FILE` captures what a trackpad reported; this
//! replays it offline and prints what the engine would have done. No
//! HID, no CGEvents, no permissions — so a capture from hardware nobody
//! here owns can still be diagnosed, and a misclassification can be
//! reproduced as many times as it takes.
//!
//! Usage: `replay FILE [--pad WxH]`

use std::cell::RefCell;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use macos_trackpad_companion::capture;
use macos_trackpad_companion::gesture::{CursorAccel, PadGeometry, State};
use macos_trackpad_companion::output::{Config, MouseButton, Output, Phase, SwipeAxis};
use macos_trackpad_companion::time::Timestamp;

/// Prints what the engine emits instead of posting it.
#[derive(Default)]
struct Printer {
    /// Cursor moves are by far the most common event; summarised rather
    /// than printed per frame so the interesting events stay visible.
    cursor_px: RefCell<(i64, i64)>,
    cursor_events: RefCell<u64>,
}

impl Printer {
    fn flush_cursor(&self) {
        let n = *self.cursor_events.borrow();
        if n == 0 {
            return;
        }
        let (x, y) = *self.cursor_px.borrow();
        println!("  cursor: {n} moves totalling ({x:+}, {y:+}) px");
        *self.cursor_events.borrow_mut() = 0;
        *self.cursor_px.borrow_mut() = (0, 0);
    }
}

// Implemented for the reference so `main` keeps the printer after
// handing it to the engine — the same shape the engine's own test
// recorder uses.
impl Output for &Printer {
    fn move_cursor_by(&self, dx_px: i32, dy_px: i32) {
        let mut acc = self.cursor_px.borrow_mut();
        acc.0 += dx_px as i64;
        acc.1 += dy_px as i64;
        *self.cursor_events.borrow_mut() += 1;
    }
    fn click(&self, button: MouseButton) {
        self.flush_cursor();
        println!("  click {button:?}");
    }
    fn set_left_button_held(&self, held: bool) {
        self.flush_cursor();
        println!("  left button {}", if held { "down" } else { "up" });
    }
    fn scroll(&self, dx_mm: f64, dy_mm: f64, phase: Phase) {
        self.flush_cursor();
        println!("  scroll {phase:?} d=({dx_mm:+.2},{dy_mm:+.2})mm");
    }
    fn scroll_inertia(&self, vx: f64, vy: f64) {
        self.flush_cursor();
        println!("  inertia v=({vx:+.0},{vy:+.0})mm/s");
    }
    fn cancel_inertia(&self) -> bool {
        false
    }
    fn pinch(&self, delta: f64, phase: Phase) {
        self.flush_cursor();
        println!("  pinch {phase:?} delta={delta:+.4}");
    }
    fn rotate(&self, delta_degrees: f64, phase: Phase) {
        self.flush_cursor();
        println!("  rotate {phase:?} delta={delta_degrees:+.2}deg");
    }
    fn swipe(&self, axis: SwipeAxis, progress: f64, velocity: f64, phase: Phase) {
        self.flush_cursor();
        println!("  swipe {axis:?} {phase:?} progress={progress:+.3} v={velocity:+.0}mm/s");
    }
    fn set_config(&self, _cfg: Config) {}
}

fn main() -> Result<()> {
    // The engine's own reasoning — the 2F lock scores especially — is
    // logged, and it is most of the value of replaying something.
    // RUST_LOG=debug for the per-frame detail.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp(None)
        .format_target(false)
        .init();

    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .map(PathBuf::from)
        .context("usage: replay FILE [--pad WxH]")?;
    let mut pad_override = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--pad" => {
                let spec = args.next().context("--pad needs WxH in millimetres")?;
                let (w, h) = spec.split_once('x').context("--pad wants WxH")?;
                pad_override = Some((w.parse()?, h.parse()?));
            }
            other => bail!("unknown argument {other:?}"),
        }
    }

    let capture = capture::read(&path)?;
    println!("device: {}", capture.device.as_deref().unwrap_or("unknown"));
    let pad = pad_override.or(capture.pad);
    match pad {
        Some((w, h)) => println!("pad: {w:.1}x{h:.1} mm"),
        None => println!("pad: unknown — engine will use its unscaled defaults"),
    }
    println!("frames: {}", capture.frames.len());
    if let (Some((_, first)), Some((_, _))) = (capture.frames.first(), capture.frames.last()) {
        let _ = first;
        let span = capture
            .frames
            .last()
            .zip(capture.frames.first())
            .map(|((b, _), (a, _))| b.saturating_duration_since(*a))
            .unwrap_or_default();
        println!("duration: {:.2}s", span.as_secs_f64());
    }
    println!("---");

    let printer = Printer::default();
    let mut state = State::new(&printer, CursorAccel::default());
    state.set_pad_geometry(pad.map(|(w, h)| PadGeometry {
        width_mm: w,
        height_mm: h,
    }));

    for (ts, frame) in capture.frames {
        let _: Timestamp = ts;
        state.on_frame_at(frame, ts);
    }
    printer.flush_cursor();
    println!("---");
    Ok(())
}
