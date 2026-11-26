//! Append-only JSON-lines logs that sit beside the conversation database: the goal check log and
//! the Instinct call log. A line is written by one `write` on a file opened for append, so lines
//! from concurrent sessions do not interleave.

use std::io::Write;
use std::path::Path;

use serde::Serialize;

/// Appends `value` as one line, creating the parent directory on first use. A failure is logged
/// to tracing under `what` and never reaches the caller: these logs observe a session, and a full
/// disk or a read-only data dir must not change what the session does (the same rule as the
/// decision log in `navigator/log.rs`).
pub fn append(path: &Path, value: &impl Serialize, what: &str) {
    let mut line = serde_json::to_string(value).unwrap_or_default();
    line.push('\n');
    let result = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
        })
        .and_then(|mut file| file.write_all(line.as_bytes()));
    if let Err(error) = result {
        tracing::warn!(path = %path.display(), %error, "{what} append failed");
    }
}
