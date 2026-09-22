//! The single log file (`logs/infrared_bootstrap.log`), truncated once per
//! game launch. Everything else logs through `log_line`.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::state::LOG_PATH;

/// Hard cap on the verbose log. Every probe is individually budgeted, but a
/// runaway line source must not be able to fill the disk or stall the game: a
/// long session once passed 100 MB and crashed it. Past this the log stops,
/// with one final marker line so a truncated log is never mistaken for a
/// crashed plugin.
const LOG_BUDGET_BYTES: usize = 8 * 1024 * 1024;
static LOG_BYTES: AtomicUsize = AtomicUsize::new(0);
static LOG_CAPPED: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_log_path(path: PathBuf) {
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
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{message}");
            LOG_BYTES.fetch_add(message.len() + 1, Ordering::Relaxed);
        }
    }
}


// ---------------------------------------------------------------------------
// module + game function helpers
// ---------------------------------------------------------------------------
