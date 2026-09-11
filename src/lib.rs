//! Library facade so binaries beyond the main `companion` daemon can
//! reuse the gesture/output stack. Shared between `src/main.rs` (the
//! daemon) and `src/bin/gesture_tap.rs` (the read-only event tap used
//! to characterize real-trackpad event streams).

pub mod app_context;
pub mod app_kit;
pub mod config;
pub mod descriptor;
pub mod gesture;
pub mod hid;
pub mod instance_lock;
pub mod output;
pub mod overlay;
pub mod report;
pub mod scan_clock;
pub mod status_item;
pub mod time;
