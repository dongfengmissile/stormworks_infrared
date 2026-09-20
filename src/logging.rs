//! The single log file (`logs/infrared_bootstrap.log`), truncated once per
//! game launch. Everything else logs through `log_line`.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use crate::state::LOG_PATH;

pub(crate) fn set_log_path(path: PathBuf) {
    // Re-initialise the log every launch so old sessions do not mix in.
    let _ = fs::write(&path, b"");
    if let Ok(mut slot) = LOG_PATH.lock() {
        *slot = Some(path);
    }
}

pub(crate) fn log_line(message: &str) {
    // Recover from a poisoned mutex: silently dropping every later line is far
    // worse than continuing to log.
    let slot = LOG_PATH.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(path) = slot.as_ref() {
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{message}");
        }
    }
}


// ---------------------------------------------------------------------------
// module + game function helpers
// ---------------------------------------------------------------------------
