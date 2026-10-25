//! The single log file (`logs/infrared_bootstrap.log`), truncated once per
//! game launch. Everything else logs through `log_line`.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
#[cfg(feature = "logging")]
use std::sync::OnceLock;
#[cfg(feature = "logging")]
use std::time::Instant;

use crate::state::LOG_PATH;

/// Bootstrap instant for the verbose log's per-line millisecond prefix.
#[cfg(feature = "logging")]
static START: OnceLock<Instant> = OnceLock::new();

#[cfg(feature = "logging")]
fn elapsed_ms() -> u64 {
    START
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis() as u64
}

/// Hard cap on the verbose log. Every probe is individually budgeted, but a
/// runaway line source must not be able to fill the disk or stall the game: a
/// long session once passed 100 MB and crashed it. Past this the log stops,
/// with one final marker line so a truncated log is never mistaken for a
/// crashed plugin.
const LOG_BUDGET_BYTES: usize = 8 * 1024 * 1024;
static LOG_BYTES: AtomicUsize = AtomicUsize::new(0);
static LOG_CAPPED: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_log_path(path: PathBuf) {
    // The verbose log's millisecond clock starts when the bootstrap log is set
    // up, so its first line is 0 ms.
    #[cfg(feature = "logging")]
    START.get_or_init(Instant::now);
    // Re-initialise the log every launch so old sessions do not mix in.
    let _ = fs::write(&path, b"");
    LOG_BYTES.store(0, Ordering::Relaxed);
    LOG_CAPPED.store(false, Ordering::Relaxed);
    if let Ok(mut slot) = LOG_PATH.lock() {
        *slot = Some(path);
    }
}

pub(crate) fn log_line(message: &str) {
    // Recover from a poisoned mutex: silently dropping every later line is far
    // worse than continuing to log.
    let slot = LOG_PATH.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(path) = slot.as_ref() {
        if LOG_BYTES.load(Ordering::Relaxed) >= LOG_BUDGET_BYTES {
            if !LOG_CAPPED.swap(true, Ordering::Relaxed) {
                if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
                    #[cfg(feature = "logging")]
                    let _ = writeln!(
                        file,
                        "[{} ms] log budget of {} MB reached; further logging is disabled for this \
launch. This is the verbose build - the player build logs almost nothing.",
                        elapsed_ms(),
                        LOG_BUDGET_BYTES / (1024 * 1024)
                    );
                    #[cfg(not(feature = "logging"))]
                    let _ = writeln!(
                        file,
                        "log budget of {} MB reached; further logging is disabled for this \
launch. This is the verbose build - the player build logs almost nothing.",
                        LOG_BUDGET_BYTES / (1024 * 1024)
                    );
                }
            }
            return;
        }
        // One open + one write + one close per line, and `File` is unbuffered,
        // so the line has already reached the OS by the time this returns: the
        // last line before a crash is on disk. The explicit flush documents
        // that requirement (it is a no-op for an unbuffered `File`).
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            // Every verbose line carries the time since bootstrap. One place,
            // so no call site can forget it (or drift on its own clock).
            #[cfg(feature = "logging")]
            let _ = writeln!(file, "[{} ms] {message}", elapsed_ms());
            #[cfg(not(feature = "logging"))]
            let _ = writeln!(file, "{message}");
            let _ = file.flush();
            LOG_BYTES.fetch_add(message.len() + 24, Ordering::Relaxed);
        }
    }
}

// ---------------------------------------------------------------------------
// One-shot diagnostics (verbose builds only)
// ---------------------------------------------------------------------------

static LOG_ONCE_BITS: AtomicU32 = AtomicU32::new(0);

/// Emit `message` the first time `key` is used this launch.
///
/// Keys are bits of one process-wide word and every call site owns its own bit
/// (`blocktemp` owns 0..8, `flir` 8..32): two messages sharing a key means the
/// second one is silently never logged. Compiled out of the player build, like
/// every other verbose diagnostic (`logging` is on in debug builds, see
/// `build.rs`).
#[cfg(feature = "logging")]
pub(crate) fn log_once(key: u32, message: impl FnOnce() -> String) {
    // Plain load first: callers sit on per-draw paths, and an RMW on every
    // call would bounce the cache line.
    if LOG_ONCE_BITS.load(Ordering::Relaxed) & key == 0
        && LOG_ONCE_BITS.fetch_or(key, Ordering::Relaxed) & key == 0
    {
        log_line(&message());
    }
}

/// Player build: the diagnostic and its message formatting are dropped.
#[cfg(not(feature = "logging"))]
pub(crate) fn log_once(_key: u32, _message: impl FnOnce() -> String) {}


// ---------------------------------------------------------------------------
// module + game function helpers
// ---------------------------------------------------------------------------
