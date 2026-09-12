//! The text behind "Copy Diagnostics".
//!
//! Its own module because both the settings window and (historically)
//! the menu want it, and because the list of what matters when
//! something misbehaves is worth keeping in one readable place.

/// Everything worth pasting into a bug report.
pub fn text() -> String {
    let perms = crate::permissions::State::current();
    let os = std::process::Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into());

    format!(
        "macos-trackpad-companion {version}\n\
         macOS: {os}\n\
         executable: {exe}\n\
         \n\
         input monitoring: {im:?}\n\
         accessibility: {ax}\n\
         ignores built-in trackpad when external present: {ignore}\n\
         built-in trackpad seen: {builtin}\n\
         \n\
         device: {device}\n\
         paused: {paused}\n\
         start at login: {login}\n\
         \n\
         config: {config}\n\
         log: {log}\n",
        version = env!("CARGO_PKG_VERSION"),
        os = os,
        exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "?".into()),
        im = perms.input_monitoring,
        ax = perms.accessibility,
        ignore = crate::system_prefs::builtin_trackpad_ignored(),
        builtin = crate::hid::builtin_trackpad_present(),
        device = crate::hid::device_summary().unwrap_or_else(|| "none attached".into()),
        paused = crate::pause::is_paused(),
        login = crate::launch_agent::is_enabled(),
        config = crate::settings::config_path_display(),
        log = crate::config_watch::log_file_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "stderr".into()),
    )
}
