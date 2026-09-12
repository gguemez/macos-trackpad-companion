//! "Check for Updates…" — fetch a small static JSON feed, compare its
//! version against this build, and say what it found.
//!
//! Deliberately minimal, and deliberately stops short: no background
//! polling beyond one check at launch, no silent install, no Sparkle.
//! Replacing a running binary is a far larger trust and security
//! surface than comparing two version strings, so this module ends by
//! handing you the download URL and getting out of the way.
//!
//! The feed carries no identifier. It is a static file fetched with a
//! plain GET, so a check reveals nothing beyond the IP and user-agent
//! inherent in any HTTP request. Keep it that way.
//!
//! `NSURLSession` rather than an HTTP crate: AppKit is already linked
//! for the whole UI, so this costs no new dependency tree and brings
//! the system's TLS trust store and proxy configuration with it. The
//! completion handler lands on one of the session's own queues, so
//! everything downstream of it that touches AppKit hops through
//! [`app_kit::on_main`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, RwLock};

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSAlert, NSAlertFirstButtonReturn, NSAlertSecondButtonReturn, NSWorkspace};
use objc2_foundation::{
    NSArray, NSData, NSError, NSHTTPURLResponse, NSString, NSURL, NSURLRequest,
    NSURLRequestCachePolicy, NSURLResponse, NSURLSession,
};
use serde::Deserialize;

use crate::app_kit;

/// Seconds before a feed fetch gives up. Long enough for a slow link,
/// short enough that the launch check has resolved one way or another
/// before anyone goes looking in the menu.
const TIMEOUT: f64 = 15.0;

/// Seconds before a *download* gives up. A dmg over a slow link is a
/// different proposition from a few hundred bytes of JSON, and the
/// user asked for this one and is waiting on it.
const DOWNLOAD_TIMEOUT: f64 = 300.0;

/// One download at a time. Two clicks on the menu item would otherwise
/// put two writers on one destination path, and the loser would be a
/// half-written dmg that passes nothing and explains nothing.
static DOWNLOADING: AtomicBool = AtomicBool::new(false);

/// The version this build reports everywhere else — the menu header,
/// `--version`, the About window, the bundle's `Info.plist`. Comparing
/// against the same constant means the feed can never disagree with
/// what the rest of the UI claims to be running.
const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// Where to look, set from `[update].feed_url` at startup and re-set by
/// the config watcher. A lock rather than a `OnceLock` because the
/// config is re-applied live and a URL that silently kept its startup
/// value would be the one setting in the file that quietly does
/// nothing.
static FEED_URL: RwLock<String> = RwLock::new(String::new());

pub fn set_feed_url(url: &str) {
    if let Ok(mut slot) = FEED_URL.write() {
        if *slot != url {
            log::debug!("update: feed is {url}");
        }
        slot.clear();
        slot.push_str(url);
    }
}

fn feed_url() -> String {
    FEED_URL.read().map(|u| u.clone()).unwrap_or_default()
}

/// The feed's shape, matching what `scripts/publish.sh` writes.
///
/// Unknown keys are accepted on purpose — unlike [`crate::config`],
/// which rejects them because a typo there is the user's own file. This
/// one is written by a future release of the publish script, and a feed
/// that grew a field must not stop parsing in an old copy: the failure
/// mode of being strict here is that updates silently stop being
/// offered, which has no symptom anyone would notice.
///
/// `sha256` and `size` are what a download is verified against. They
/// are carried but unused until that lands; a feed without them still
/// parses.
#[derive(Deserialize, Debug, Clone)]
pub struct Feed {
    pub version: String,
    pub url: String,
    pub notes: Option<String>,
    pub sha256: Option<String>,
    pub size: Option<u64>,
}

/// Compare dotted numeric versions.
///
/// Deliberately not a string comparison: "0.9.0" sorts above "0.40.0"
/// lexically but is older, so a string compare would stop offering
/// updates the moment the minor version hit double digits — silently,
/// and permanently.
///
/// Non-numeric components count as 0 and missing trailing components
/// count as 0, so "0.10" beats "0.9.7" and "1.0" beats "1.0.0-rc1".
/// A single leading "v" is stripped because git tags are conventionally
/// "v0.9.3" and a feed authored from a tag name is an easy mistake to
/// make; being strict about it would fail closed, and an update that is
/// never offered again looks exactly like no update existing.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    fn parts(s: &str) -> Vec<u64> {
        let t = s.trim();
        let t = t.strip_prefix('v').or_else(|| t.strip_prefix('V')).unwrap_or(t);
        t.split('.').map(|p| p.parse().unwrap_or(0)).collect()
    }
    let (a, b) = (parts(candidate), parts(current));
    for i in 0..a.len().max(b.len()) {
        let (l, r) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if l != r {
            return l > r;
        }
    }
    false
}

/// How loudly a check reports itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Presentation {
    /// The user picked the menu item. Always reports an outcome —
    /// silence after choosing a menu item reads as breakage.
    Modal,
    /// The check at launch. Quiet about failures and about being up to
    /// date; only a real update reaches the UI, as a menu title.
    Quiet,
}

/// What a check concluded. Built on the session's queue, so it holds
/// plain data only — nothing here may touch AppKit.
#[derive(Debug)]
pub enum Outcome {
    Failed(String),
    UpToDate,
    Available(Feed),
}

/// Start a check against the configured feed, reporting through the
/// UI. Returns immediately; the fetch and its reporting happen on their
/// own.
pub fn check(presentation: Presentation) {
    let url_string = feed_url();
    if url_string.is_empty() {
        // An empty feed URL is how the config turns checking off, so
        // it is not an error — but a manual check must still say why
        // nothing happened.
        log::debug!("update: no feed configured; not checking");
        if presentation == Presentation::Modal && let Some(mtm) = MainThreadMarker::new() {
            report(
                mtm,
                Outcome::Failed(
                    "No update feed is configured. Set `feed_url` under [update] in the \
                     config file."
                        .into(),
                ),
                presentation,
            );
        }
        return;
    }

    log::debug!("update: checking {url_string}");
    fetch(&url_string, move |outcome| {
        app_kit::on_main(move |mtm| report(mtm, outcome, presentation));
    });
}

/// Fetch the feed and hand back what it says.
///
/// Split out from [`check`] so the whole network path — request,
/// response, status, parse, comparison — can be exercised against a
/// local server without an alert to dismiss. `done` runs on one of the
/// session's queues, not the main thread.
pub fn fetch<F>(feed_url: &str, done: F)
where
    F: FnOnce(Outcome) + Send + 'static,
{
    let Some(url) = NSURL::URLWithString(&NSString::from_str(feed_url)) else {
        log::warn!("update: feed_url is not a valid URL: {feed_url}");
        done(Outcome::Failed(format!("“{feed_url}” is not a valid URL.")));
        return;
    };

    // Ignore any cached copy: a stale 200 would report an old version
    // as the latest one, which is the one wrong answer a version check
    // must never give.
    let request = NSURLRequest::requestWithURL_cachePolicy_timeoutInterval(
        &url,
        NSURLRequestCachePolicy::ReloadIgnoringLocalCacheData,
        TIMEOUT,
    );

    // `NSURLSession` calls a completion handler exactly once, but the
    // block type is `Fn` and must therefore be callable more than once.
    // Parking the `FnOnce` somewhere it can be taken squares the two,
    // and makes a hypothetical second call a no-op rather than a
    // double-report.
    let done = Mutex::new(Some(done));
    let handler = RcBlock::new(
        move |data: *mut NSData, response: *mut NSURLResponse, error: *mut NSError| {
            let outcome = unsafe { interpret(data.as_ref(), response.as_ref(), error.as_ref()) };
            if let Some(done) = done.lock().ok().and_then(|mut slot| slot.take()) {
                done(outcome);
            }
        },
    );

    let session = NSURLSession::sharedSession();
    // SAFETY: the block only reads its three arguments and hands plain
    // data onward, so it is sound to call from whichever queue the
    // session picks.
    let task = unsafe { session.dataTaskWithRequest_completionHandler(&request, &handler) };
    task.resume();
}

/// Download the artifact the feed names, verify it, and hand back where
/// it landed.
///
/// Verification is not optional decoration. A dmg fetched here does not
/// carry the quarantine bit a browser download would set, so macOS will
/// not run its notarization check on first launch — the feed's SHA-256,
/// served over HTTPS alongside the bytes it describes, is what stands
/// in for that. A feed that omits the hash gets the size check and a
/// line in the log saying so.
///
/// `done` runs on one of the session's queues, not the main thread.
pub fn download<F>(feed: &Feed, done: F)
where
    F: FnOnce(Result<PathBuf, String>) + Send + 'static,
{
    let Some(url) = NSURL::URLWithString(&NSString::from_str(&feed.url)) else {
        done(Err(format!("The update URL is invalid: {}", feed.url)));
        return;
    };
    let request = NSURLRequest::requestWithURL_cachePolicy_timeoutInterval(
        &url,
        NSURLRequestCachePolicy::ReloadIgnoringLocalCacheData,
        DOWNLOAD_TIMEOUT,
    );

    let feed = feed.clone();
    let done = Mutex::new(Some(done));
    let handler = RcBlock::new(
        move |location: *mut NSURL, response: *mut NSURLResponse, error: *mut NSError| {
            // SAFETY: the three pointers are the ones NSURLSession hands
            // a download completion handler — null or valid for this
            // call.
            let result = unsafe {
                collect(
                    location.as_ref(),
                    response.as_ref(),
                    error.as_ref(),
                    &feed,
                )
            };
            if let Some(done) = done.lock().ok().and_then(|mut slot| slot.take()) {
                done(result);
            }
        },
    );

    let session = NSURLSession::sharedSession();
    // SAFETY: the block moves a path and plain data onward, so it is
    // sound on whichever queue the session picks.
    let task = unsafe { session.downloadTaskWithRequest_completionHandler(&request, &handler) };
    task.resume();
}

/// Move the downloaded temporary file somewhere durable and check it.
///
/// Everything here must finish before the handler returns: the file
/// `NSURLSession` hands over lives only for the duration of the call,
/// and is deleted underneath us the moment we come back.
///
/// # Safety
///
/// The pointers must be those `NSURLSession` passed the completion
/// handler.
unsafe fn collect(
    location: Option<&NSURL>,
    response: Option<&NSURLResponse>,
    error: Option<&NSError>,
    feed: &Feed,
) -> Result<PathBuf, String> {
    if let Some(error) = error {
        return Err(error.localizedDescription().to_string());
    }
    if let Some(code) = response
        .and_then(|r| r.downcast_ref::<NSHTTPURLResponse>())
        .map(|http| http.statusCode())
        && !(200..300).contains(&code)
    {
        return Err(format!("The update download failed with HTTP {code}."));
    }
    let Some(location) = location else {
        return Err("The downloaded update could not be found.".into());
    };
    let Some(temporary) = location.path().map(|p| PathBuf::from(p.to_string())) else {
        return Err("The downloaded update had no readable location.".into());
    };

    let suggested = response.and_then(|r| r.suggestedFilename()).map(|n| n.to_string());
    let from_url = response
        .and_then(|r| r.URL())
        .and_then(|u| u.lastPathComponent())
        .map(|n| n.to_string())
        .or_else(|| feed.url.rsplit('/').next().map(str::to_string));
    let name = staged_name(&feed.version, suggested.as_deref(), from_url.as_deref());

    let dir = updates_dir().ok_or_else(|| "HOME is not set.".to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
    let destination = dir.join(&name);
    let _ = std::fs::remove_file(&destination);

    // `rename` is the cheap path and the usual one; the fallback is for
    // the case where the session staged its temporary file on another
    // volume, which `rename` reports as a cross-device link rather than
    // handling.
    if std::fs::rename(&temporary, &destination).is_err() {
        std::fs::copy(&temporary, &destination)
            .map_err(|e| format!("Could not save the update to {}: {e}", destination.display()))?;
        let _ = std::fs::remove_file(&temporary);
    }

    // A file that fails verification is removed rather than left for
    // someone to find later and trust.
    if let Err(why) = verify(&destination, feed) {
        let _ = std::fs::remove_file(&destination);
        return Err(why);
    }
    Ok(destination)
}

/// Check a staged artifact against what the feed promised.
fn verify(path: &Path, feed: &Feed) -> Result<(), String> {
    if let Some(expected) = feed.size {
        let actual = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
        if actual != expected {
            return Err(format!(
                "The update size did not match the feed. Expected {expected} bytes, got {actual}."
            ));
        }
    }

    let Some(expected) = feed.sha256.as_deref() else {
        log::warn!("update: the feed published no sha256; the download is unverified");
        return Ok(());
    };
    let expected = expected.trim().to_ascii_lowercase();
    if expected.len() != 64 || !expected.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("The update feed contains an invalid SHA-256 checksum.".into());
    }

    let actual = sha256::of_file(path).map_err(|e| format!("Could not read the update: {e}"))?;
    if actual != expected {
        log::warn!("update: sha256 {actual} does not match the published {expected}");
        return Err("The downloaded update did not match the published SHA-256 checksum.".into());
    }
    Ok(())
}

/// Where a downloaded update is staged. Under the crate name, matching
/// the config directory and the log file rather than inventing a third
/// spelling of the same app.
fn updates_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| {
        PathBuf::from(home).join("Library/Application Support/macos-trackpad-companion/Updates")
    })
}

/// Pick the filename to stage under: the server's suggestion, then the
/// URL's own last component, then one built from the version. Whichever
/// is used has to end in `.dmg`, so a redirect to an error page cannot
/// leave an `.html` sitting in the updates directory looking like an
/// installer.
fn staged_name(version: &str, suggested: Option<&str>, from_url: Option<&str>) -> String {
    let fallback = format!("Trackpad-Companion-{version}.dmg");
    let candidate = [suggested, from_url, Some(fallback.as_str())]
        .into_iter()
        .flatten()
        .find(|name| !name.is_empty() && name.to_ascii_lowercase().ends_with(".dmg"))
        .unwrap_or(&fallback);
    safe_filename(candidate)
}

/// Reduce a server-supplied name to something safe to join onto a path.
///
/// ASCII only, and deliberately stricter than "characters a filesystem
/// would accept": the name arrives from the network, and `..` or a `/`
/// in it would be a path traversal out of the updates directory.
fn safe_filename(name: &str) -> String {
    let mapped: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = mapped.trim_matches(['.', '-']);
    if trimmed.is_empty() {
        "Trackpad-Companion.dmg".to_string()
    } else {
        trimmed.to_string()
    }
}

/// SHA-256, via CommonCrypto.
///
/// Declared by hand rather than adding a hashing crate, the same way
/// [`crate::app_kit`] declares its two dispatch symbols: these are in
/// libSystem, which is already linked, and the surface is three
/// functions. The NIST vectors in the tests are what hold the struct
/// layout honest — a wrong one does not fail quietly.
mod sha256 {
    use std::fs::File;
    use std::io::{self, Read};
    use std::path::Path;

    /// `CC_SHA256_CTX` as `<CommonCrypto/CommonDigest.h>` declares it:
    /// `CC_LONG count[2]; CC_LONG hash[8]; CC_LONG wbuf[16];`, where
    /// `CC_LONG` is `uint32_t`.
    #[repr(C)]
    struct Ctx {
        count: [u32; 2],
        hash: [u32; 8],
        wbuf: [u32; 16],
    }

    unsafe extern "C" {
        fn CC_SHA256_Init(ctx: *mut Ctx) -> i32;
        fn CC_SHA256_Update(ctx: *mut Ctx, data: *const u8, len: u32) -> i32;
        fn CC_SHA256_Final(digest: *mut u8, ctx: *mut Ctx) -> i32;
    }

    pub fn of_file(path: &Path) -> io::Result<String> {
        of_reader(File::open(path)?)
    }

    /// Hashes in 1 MiB chunks: there is no reason to hold a whole dmg
    /// in memory, and it keeps every `Update` call well inside the
    /// 32-bit length `CC_LONG` can express.
    pub fn of_reader<R: Read>(mut reader: R) -> io::Result<String> {
        let mut ctx = Ctx {
            count: [0; 2],
            hash: [0; 8],
            wbuf: [0; 16],
        };
        let mut buf = vec![0u8; 1 << 20];
        // SAFETY: `ctx` is a live, correctly sized context for the whole
        // of this function, and each Update is handed a pointer valid
        // for the `n` bytes it is told about.
        unsafe {
            CC_SHA256_Init(&mut ctx);
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                CC_SHA256_Update(&mut ctx, buf.as_ptr(), n as u32);
            }
            let mut digest = [0u8; 32];
            CC_SHA256_Final(digest.as_mut_ptr(), &mut ctx);
            Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
        }
    }
}

/// Collapse the `(data, response, error)` triple into one outcome.
///
/// # Safety
///
/// The three pointers must be the ones `NSURLSession` handed the
/// completion handler: either null or a valid object for this call.
unsafe fn interpret(
    data: Option<&NSData>,
    response: Option<&NSURLResponse>,
    error: Option<&NSError>,
) -> Outcome {
    if let Some(error) = error {
        return Outcome::Failed(error.localizedDescription().to_string());
    }

    // A non-200 is reported rather than parsed. Cloudflare and friends
    // serve an HTML error page with a 4xx, and letting that reach the
    // JSON parser would turn "the feed is missing" into "the feed is
    // malformed" — a true statement that points at the wrong thing.
    match response
        .and_then(|r| r.downcast_ref::<NSHTTPURLResponse>())
        .map(|http| http.statusCode())
    {
        Some(200) => {}
        Some(code) => return Outcome::Failed(format!("The update feed returned HTTP {code}.")),
        None => return Outcome::Failed("The update feed gave no HTTP response.".into()),
    }

    let Some(data) = data else {
        return Outcome::Failed("The update feed was empty.".into());
    };
    let feed: Feed = match serde_json::from_slice(&data.to_vec()) {
        Ok(feed) => feed,
        Err(e) => {
            // The parse error names a byte offset, which is useful in
            // the log and meaningless in an alert.
            log::warn!("update: feed did not parse: {e}");
            return Outcome::Failed("The update feed was malformed.".into());
        }
    };

    if is_newer(&feed.version, CURRENT) {
        Outcome::Available(feed)
    } else {
        Outcome::UpToDate
    }
}

/// Report an outcome. Runs on the main thread.
fn report(mtm: MainThreadMarker, outcome: Outcome, presentation: Presentation) {
    // The log line happens either way, so a quiet check still leaves a
    // trace for a bug report to lean on.
    match &outcome {
        Outcome::Available(feed) => {
            log::info!("update: {} is available (running {CURRENT})", feed.version);
            crate::status_item::set_update_title(Some(&format!(
                "Update to {}…",
                feed.version
            )));
        }
        Outcome::UpToDate => log::debug!("update: {CURRENT} is current"),
        Outcome::Failed(why) => log::info!("update: check failed: {why}"),
    }
    if presentation == Presentation::Quiet {
        return;
    }

    // An accessory app's modal can otherwise open behind whatever is
    // frontmost, which for a menu-bar action reads as nothing having
    // happened at all.
    app_kit::activate_for_window(mtm);
    let alert = NSAlert::new(mtm);
    match outcome {
        Outcome::Failed(why) => {
            alert.setMessageText(&NSString::from_str("Could Not Check for Updates"));
            alert.setInformativeText(&NSString::from_str(&why));
            alert.addButtonWithTitle(&NSString::from_str("OK"));
            alert.runModal();
        }
        Outcome::UpToDate => {
            alert.setMessageText(&NSString::from_str("You're Up to Date"));
            alert.setInformativeText(&NSString::from_str(&format!(
                "Trackpad Companion {CURRENT} is the latest version."
            )));
            alert.addButtonWithTitle(&NSString::from_str("OK"));
            alert.runModal();
        }
        Outcome::Available(feed) => {
            alert.setMessageText(&NSString::from_str(&format!(
                "Trackpad Companion {} is Available",
                feed.version
            )));
            alert.setInformativeText(&NSString::from_str(&format!("You have {CURRENT}.")));
            alert.addButtonWithTitle(&NSString::from_str("Download"));
            let has_notes = feed.notes.is_some();
            if has_notes {
                alert.addButtonWithTitle(&NSString::from_str("Release Notes"));
            }
            alert.addButtonWithTitle(&NSString::from_str("Later"));

            let response = alert.runModal();
            if response == NSAlertFirstButtonReturn {
                start_download(feed.clone());
            } else if has_notes
                && response == NSAlertSecondButtonReturn
                && let Some(notes) = feed.notes.as_deref()
            {
                open(notes);
            }
        }
    }
    app_kit::settle_activation(mtm);
}

/// Fetch the update the user approved, then say where it went.
///
/// The menu item carries the progress. A menu-bar agent has no window
/// to put a progress bar in, and the line the user just clicked is
/// exactly where they will look to find out whether anything is
/// happening.
fn start_download(feed: Feed) {
    if DOWNLOADING.swap(true, Ordering::SeqCst) {
        log::info!("update: a download is already running");
        return;
    }
    crate::status_item::set_update_title(Some("Downloading update…"));

    let version = feed.version.clone();
    download(&feed, move |result| {
        app_kit::on_main(move |mtm| {
            DOWNLOADING.store(false, Ordering::SeqCst);
            // Back to naming the version either way: a failed download
            // does not make the update stop existing.
            crate::status_item::set_update_title(Some(&format!("Update to {version}…")));
            match result {
                Ok(path) => {
                    log::info!("update: {version} downloaded and verified to {}", path.display());
                    present_downloaded(mtm, &path);
                }
                Err(why) => {
                    log::warn!("update: download failed: {why}");
                    present_failure(mtm, "Could Not Download Update", &why);
                }
            }
        });
    });
}

/// Say where the verified dmg is, and offer to reveal it.
///
/// Stops at revealing it. Replacing a running app is a different kind
/// of operation from downloading a file — it has to quit the very
/// process doing the replacing, and here it also has to survive a
/// LaunchAgent that restarts on crash — and doing it badly is worse
/// than not doing it.
fn present_downloaded(mtm: MainThreadMarker, path: &Path) {
    app_kit::activate_for_window(mtm);
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("Update Downloaded"));
    alert.setInformativeText(&NSString::from_str(&format!(
        "{} was downloaded and its checksum matches the one published with it.\n\n\
         Open it, drag Trackpad Companion to Applications replacing the old copy, \
         and launch it again.",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "The update".into()),
    )));
    alert.addButtonWithTitle(&NSString::from_str("Show in Finder"));
    alert.addButtonWithTitle(&NSString::from_str("Later"));
    if alert.runModal() == NSAlertFirstButtonReturn {
        reveal(path);
    }
    app_kit::settle_activation(mtm);
}

fn present_failure(mtm: MainThreadMarker, title: &str, message: &str) {
    app_kit::activate_for_window(mtm);
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(message));
    alert.addButtonWithTitle(&NSString::from_str("OK"));
    alert.runModal();
    app_kit::settle_activation(mtm);
}

/// Select a file in the Finder.
fn reveal(path: &Path) {
    let Some(url) = NSURL::URLWithString(&NSString::from_str(&format!(
        "file://{}",
        path.display()
    ))) else {
        log::warn!("update: could not build a file URL for {}", path.display());
        return;
    };
    let urls = NSArray::from_slice(&[&*url]);
    NSWorkspace::sharedWorkspace().activateFileViewerSelectingURLs(&urls);
}

/// Hand a URL to the default browser. Used for release notes, which are
/// a web page and belong in a browser.
fn open(url: &str) {
    let Some(url) = NSURL::URLWithString(&NSString::from_str(url)) else {
        log::warn!("update: refusing to open an invalid URL: {url}");
        return;
    };
    NSWorkspace::sharedWorkspace().openURL(&url);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_by_component_not_lexically() {
        // The whole reason this is not a string comparison.
        assert!(is_newer("0.40.0", "0.9.0"));
        assert!(!is_newer("0.9.0", "0.40.0"));
        assert!(is_newer("0.10", "0.9.7"));
    }

    #[test]
    fn equal_versions_are_not_newer() {
        assert!(!is_newer("0.9.2", "0.9.2"));
        assert!(!is_newer("0.9", "0.9.0"));
        assert!(!is_newer("0.9.0", "0.9"));
    }

    #[test]
    fn missing_components_count_as_zero() {
        assert!(is_newer("0.9.1", "0.9"));
        assert!(!is_newer("0.9", "0.9.1"));
    }

    #[test]
    fn strips_one_leading_v() {
        assert!(is_newer("v0.9.3", "0.9.2"));
        assert!(!is_newer("v0.9.2", "0.9.2"));
        assert!(is_newer("0.9.3", "v0.9.2"));
    }

    #[test]
    fn non_numeric_components_count_as_zero() {
        // "1.0.0-rc1" parses as [1, 0, 0], so the real 1.0.0 is not
        // newer than it — and neither is it newer than 1.0.0. Ties go
        // to "no update", which is the safe direction.
        assert!(!is_newer("1.0.0-rc1", "1.0.0"));
        assert!(!is_newer("1.0.0", "1.0.0-rc1"));
        assert!(is_newer("1.0.1", "1.0.0-rc1"));
    }

    #[test]
    fn tolerates_whitespace() {
        assert!(is_newer("  0.9.3  ", "0.9.2"));
    }

    // ---- SHA-256, against the NIST vectors ---------------------------
    //
    // These are what hold the hand-declared `CC_SHA256_CTX` layout
    // honest: a wrong size or field order does not produce a subtly
    // wrong digest, it produces these failing.

    #[test]
    fn hashes_the_nist_vectors() {
        let of = |b: &[u8]| sha256::of_reader(b).unwrap();
        assert_eq!(
            of(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            of(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            of(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn hashes_across_the_chunk_boundary() {
        // 2 MB through a 1 MiB buffer, so the multi-`Update` path is
        // the one under test rather than a single shot.
        let big = vec![b'a'; 2_000_000];
        assert_eq!(
            sha256::of_reader(&big[..]).unwrap(),
            "bcf7f9d1b4311c3352e60502255ce09a6744df84e8f2c89f79c4b5d74933a95a"
        );
    }

    #[test]
    fn hashes_a_file_the_same_as_the_bytes_in_it() {
        let path = std::env::temp_dir().join("companion-sha256-test.bin");
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(
            sha256::of_file(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        std::fs::remove_file(&path).unwrap();
    }

    // ---- verification ------------------------------------------------

    fn staged(bytes: &[u8], name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn feed_for(sha: Option<&str>, size: Option<u64>) -> Feed {
        Feed {
            version: "9.9.9".into(),
            url: "https://e.invalid/x.dmg".into(),
            notes: None,
            sha256: sha.map(str::to_string),
            size,
        }
    }

    #[test]
    fn verification_accepts_what_the_feed_describes() {
        let path = staged(b"abc", "companion-verify-ok.dmg");
        let feed = feed_for(
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
            Some(3),
        );
        assert!(verify(&path, &feed).is_ok());
        // Case and surrounding whitespace in the feed are not a reason
        // to reject bytes that are otherwise correct.
        let shouty = feed_for(
            Some("  BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD  "),
            None,
        );
        assert!(verify(&path, &shouty).is_ok());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn verification_rejects_a_wrong_hash_and_a_wrong_size() {
        let path = staged(b"abc", "companion-verify-bad.dmg");

        let wrong_hash = feed_for(Some(&"0".repeat(64)), None);
        assert!(verify(&path, &wrong_hash).is_err());

        let wrong_size = feed_for(None, Some(999));
        let why = verify(&path, &wrong_size).unwrap_err();
        assert!(why.contains("999") && why.contains('3'), "{why}");

        // A checksum that is not a checksum is a broken feed, not a
        // reason to accept the bytes.
        for bad in ["not-hex", &"z".repeat(64), &"ab".repeat(20)] {
            assert!(
                verify(&path, &feed_for(Some(bad), None)).is_err(),
                "accepted {bad:?} as a checksum"
            );
        }
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn verification_passes_a_feed_that_published_no_checksum() {
        // Older feeds predate the integrity fields. They still install;
        // the log says they were unverified.
        let path = staged(b"abc", "companion-verify-none.dmg");
        assert!(verify(&path, &feed_for(None, None)).is_ok());
        std::fs::remove_file(&path).unwrap();
    }

    // ---- staging names -----------------------------------------------

    #[test]
    fn prefers_the_suggested_name_then_the_url_then_the_version() {
        assert_eq!(
            staged_name("1.2.3", Some("Nice-1.2.3.dmg"), Some("from-url.dmg")),
            "Nice-1.2.3.dmg"
        );
        assert_eq!(
            staged_name("1.2.3", None, Some("from-url.dmg")),
            "from-url.dmg"
        );
        assert_eq!(
            staged_name("1.2.3", None, None),
            "Trackpad-Companion-1.2.3.dmg"
        );
    }

    #[test]
    fn a_non_dmg_name_never_wins() {
        // A redirect to an error page must not leave something called
        // .html in the updates directory looking like an installer.
        assert_eq!(
            staged_name("1.2.3", Some("login.html"), Some("index.php")),
            "Trackpad-Companion-1.2.3.dmg"
        );
        assert_eq!(staged_name("1.2.3", Some(""), None), "Trackpad-Companion-1.2.3.dmg");
    }

    #[test]
    fn a_server_supplied_name_cannot_escape_the_updates_directory() {
        // The name arrives from the network. Separators and traversal
        // must not survive it.
        let name = staged_name("1.2.3", Some("../../../../tmp/evil.dmg"), None);
        assert!(!name.contains('/'), "{name}");
        assert!(!name.starts_with('.'), "{name}");
        assert_eq!(name, "tmp-evil.dmg");

        assert!(!safe_filename("a/b/c.dmg").contains('/'));
        assert_eq!(safe_filename("..."), "Trackpad-Companion.dmg");
        assert_eq!(safe_filename(""), "Trackpad-Companion.dmg");
        // Spaces and non-ASCII become separators rather than being
        // trusted through to the filesystem.
        assert_eq!(safe_filename("Trackpad Companion 1.dmg"), "Trackpad-Companion-1.dmg");
    }

    #[test]
    fn parses_the_feed_publish_writes() {
        // Byte-for-byte the heredoc in scripts/publish.sh. If that
        // script's shape ever changes, this is what notices — the
        // alternative is a feed that publishes fine and that no
        // installed copy can read.
        let feed: Feed = serde_json::from_str(
            r#"{
  "version": "0.9.3",
  "url": "https://dl.trackpad-companion.guemez.net/Trackpad-Companion-0.9.3.dmg",
  "notes": "https://github.com/gguemez/macos-trackpad-companion/blob/main/CHANGELOG.md",
  "sha256": "8f1ccd68f0d96b2e0f9b5b9f1a2c3d4e5f60718293a4b5c6d7e8f9012345678a",
  "size": 4194304
}"#,
        )
        .expect("the shape publish.sh writes must parse");
        assert_eq!(feed.version, "0.9.3");
        assert_eq!(feed.size, Some(4_194_304));
        assert!(feed.notes.is_some());
        assert!(feed.url.ends_with(".dmg"));

        // And a feed that omits the optional fields, which is what an
        // older publish script produced.
        let bare: Feed =
            serde_json::from_str(r#"{"version":"0.9.3","url":"https://e.net/a.dmg"}"#).unwrap();
        assert!(bare.notes.is_none() && bare.sha256.is_none() && bare.size.is_none());
    }

    #[test]
    fn tolerates_a_feed_that_grew_a_field() {
        // A newer publish script adding a key must not stop an older
        // copy from seeing the update.
        let feed: Feed = serde_json::from_str(
            r#"{"version":"1.0.0","url":"https://e.net/a.dmg","minimumSystemVersion":"14.0"}"#,
        )
        .expect("unknown keys must be ignored");
        assert_eq!(feed.version, "1.0.0");
    }

    #[test]
    fn requires_the_two_fields_a_download_needs() {
        assert!(serde_json::from_str::<Feed>(r#"{"version":"1.0.0"}"#).is_err());
        assert!(serde_json::from_str::<Feed>(r#"{"url":"https://e.net/a.dmg"}"#).is_err());
    }
}
