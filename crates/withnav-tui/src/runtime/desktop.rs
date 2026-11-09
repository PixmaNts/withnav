//! The local desktop, through the programs it usually has: the image on the clipboard and the
//! system image viewer. Over SSH they are the remote machine's, so they usually fail, with a
//! message that says which program was missing.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use crate::state::MAX_ATTACHMENT_BYTES;

/// A program and its arguments.
type Program = (&'static str, &'static [&'static str]);

/// The programs that print the clipboard image as PNG, tried in order.
const CLIPBOARD_READERS: &[Program] = if cfg!(target_os = "macos") {
    &[("pngpaste", &["-"])]
} else {
    &[
        ("wl-paste", &["--no-newline", "--type", "image/png"]),
        (
            "xclip",
            &["-selection", "clipboard", "-target", "image/png", "-out"],
        ),
    ]
};

/// How long a clipboard reader may take; a clipboard owner that never answers would otherwise
/// hold the paste forever.
const CLIPBOARD_DEADLINE: Duration = Duration::from_secs(5);

/// How long the viewer launcher is waited for. Some launchers return at once, others only when
/// the viewer closes; past this the viewer is left open and counted as started.
const OPEN_DEADLINE: Duration = Duration::from_secs(3);

/// How old a file this module wrote must be before a later call removes it: by then the paste
/// was sent and the viewer has read it.
const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// How often a running program is checked for exit. Shorter wakes the thread more for nothing;
/// longer adds up to this much latency to every paste and viewer launch.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How much of a clipboard reader's stderr is kept for the error message: enough for its one-line
/// reason, small enough that a reader looping on warnings cannot grow the message without bound.
const STDERR_KEPT: u64 = 4096;

/// The file name prefix of [`private_file`]'s files: `withnav-<process id>-<counter>.<extension>`.
const FILE_PREFIX: &str = "withnav-";

/// The extensions [`extension`] gives, the only ones [`private_file`] writes.
const EXTENSIONS: [&str; 5] = ["png", "jpg", "gif", "webp", "img"];

/// Waits for `child` until `deadline`; `None` once it is past.
fn wait_until(child: &mut Child, deadline: Instant) -> std::io::Result<Option<ExitStatus>> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// What a clipboard reader printed: at most one byte past the attachment limit on stdout (enough
/// to know it is over) and the start of stderr.
struct Output {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Reads the first `limit` bytes of `pipe`, then reads and drops the rest so the program is never
/// blocked on a full pipe. A read error at either stage is returned: bytes cut short by it would
/// otherwise pass for the whole clipboard and fail later as an invalid image.
fn drain(pipe: impl Read, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut pipe = pipe.take(limit);
    pipe.read_to_end(&mut bytes)?;
    std::io::copy(&mut pipe.into_inner(), &mut std::io::sink())?;
    Ok(bytes)
}

/// What a [`drain`] thread read, or why it read nothing. A thread that panicked is an error, never
/// an empty pipe.
fn joined(
    drain: std::thread::JoinHandle<std::io::Result<Vec<u8>>>,
    pipe: &str,
) -> std::io::Result<Vec<u8>> {
    drain.join().unwrap_or_else(|_| {
        Err(std::io::Error::other(format!(
            "the reader of its {pipe} stopped unexpectedly"
        )))
    })
}

/// The error of a reader past [`CLIPBOARD_DEADLINE`], given how stopping it went. It stays a
/// timeout either way; a reader that could not be stopped says so, since it may still be running.
fn timed_out(stopped: std::io::Result<()>) -> std::io::Error {
    let mut message = format!("no answer after {} s", CLIPBOARD_DEADLINE.as_secs());
    if let Err(e) = stopped {
        message.push_str(&format!("; could not stop it: {e}"));
    }
    std::io::Error::new(std::io::ErrorKind::TimedOut, message)
}

/// Runs a clipboard reader, killed past [`CLIPBOARD_DEADLINE`].
fn read_program(program: &str, args: &[&str]) -> std::io::Result<Output> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let start = |pipe: Option<Box<dyn Read + Send>>, limit: u64| {
        std::thread::spawn(move || match pipe {
            Some(pipe) => drain(pipe, limit),
            None => Ok(Vec::new()),
        })
    };
    let stdout = start(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        MAX_ATTACHMENT_BYTES + 1,
    );
    let stderr = start(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        STDERR_KEPT,
    );
    let status = match wait_until(&mut child, Instant::now() + CLIPBOARD_DEADLINE)? {
        Some(status) => status,
        None => {
            // Waiting after a failed kill could block for as long as the reader runs.
            return Err(timed_out(
                child.kill().and_then(|()| child.wait().map(drop)),
            ));
        }
    };
    // Both threads are joined before either result is looked at, so neither is left running.
    let stdout = joined(stdout, "output");
    let stderr = joined(stderr, "error output");
    Ok(Output {
        status,
        stdout: stdout?,
        stderr: stderr?,
    })
}

/// The PNG bytes of the image on the clipboard.
pub(super) fn clipboard_image() -> Result<Vec<u8>, String> {
    let mut tried = Vec::new();
    for (program, args) in CLIPBOARD_READERS {
        let output = match read_program(program, args) {
            Ok(output) => output,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tried.push(format!("{program} is not installed"));
                continue;
            }
            Err(e) => return Err(format!("{program}: {e}")),
        };
        if output.status.success() && !output.stdout.is_empty() {
            if output.stdout.len() as u64 > MAX_ATTACHMENT_BYTES {
                return Err(format!(
                    "the clipboard image is larger than {}",
                    withnav_client_api::attachment::human_size(MAX_ATTACHMENT_BYTES)
                ));
            }
            return Ok(output.stdout);
        }
        let reason = String::from_utf8_lossy(&output.stderr);
        let reason = crate::reducer::strip_controls(reason.trim());
        tried.push(if reason.is_empty() {
            format!("{program} found no image")
        } else {
            format!("{program}: {reason}")
        });
    }
    Err(format!("no image on the clipboard ({})", tried.join("; ")))
}

/// Writes `bytes` to a new file only this user can read, in the temporary folder. The file must
/// outlive the call (the upload or the viewer reads it later), so it is not removed here: each
/// call first sweeps this module's files older than [`STALE_AFTER`]. A sweep that fails is logged
/// and never fails the call; a write that fails removes the file it started.
pub(super) fn private_file(extension: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    for (path, e) in remove_stale(&std::env::temp_dir(), SystemTime::now()) {
        tracing::debug!("cannot remove the old image file {}: {e}", path.display());
    }
    let path = std::env::temp_dir().join(format!(
        "{FILE_PREFIX}{}-{}.{extension}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let written = file.write_all(bytes);
    drop(file);
    finish_write(path, written)
}

/// The written file, or the write error once the partial file is removed: a cut-short image left
/// behind would only be swept a day later.
fn finish_write(path: PathBuf, written: std::io::Result<()>) -> Result<PathBuf, String> {
    let Err(e) = written else {
        return Ok(path);
    };
    let mut message = format!("{}: {e}", path.display());
    if let Err(removal) = std::fs::remove_file(&path) {
        message.push_str(&format!(" (the partial file is left: {removal})"));
    }
    Err(message)
}

/// Whether `name` is exactly a name [`private_file`] gives: `withnav-<digits>-<digits>.<extension>`.
/// Anything looser would sweep the user's own files, such as `withnav-notes.png`.
fn is_generated_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(FILE_PREFIX) else {
        return false;
    };
    let Some((stem, extension)) = rest.rsplit_once('.') else {
        return false;
    };
    let Some((process, counter)) = stem.split_once('-') else {
        return false;
    };
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    digits(process) && digits(counter) && EXTENSIONS.contains(&extension)
}

/// Removes the files [`private_file`] wrote in `folder` more than [`STALE_AFTER`] before `now`, and
/// returns what could not be read or removed. Another user's files are left alone (the temporary
/// folder refuses), as are files of other names.
fn remove_stale(folder: &Path, now: SystemTime) -> Vec<(PathBuf, std::io::Error)> {
    let mut failures = Vec::new();
    let entries = match std::fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(e) => return vec![(folder.to_path_buf(), e)],
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                failures.push((folder.to_path_buf(), e));
                continue;
            }
        };
        if !entry.file_name().to_str().is_some_and(is_generated_name) {
            continue;
        }
        let path = entry.path();
        let modified = match entry
            .metadata()
            .and_then(|m| Ok((m.is_file(), m.modified()?)))
        {
            Ok((true, modified)) => modified,
            Ok((false, _)) => continue,
            Err(e) => {
                failures.push((path, e));
                continue;
            }
        };
        let stale = now
            .duration_since(modified)
            .is_ok_and(|age| age > STALE_AFTER);
        if stale && let Err(e) = std::fs::remove_file(&path) {
            failures.push((path, e));
        }
    }
    failures
}

/// The file extension of a stored image type; the viewer goes by it.
pub(super) fn extension(media_type: &str) -> &'static str {
    match media_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "img",
    }
}

/// Opens `path` in the program the desktop gives its type.
pub(super) fn open(path: &Path) -> Result<(), String> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    let mut child = Command::new(program)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{program}: {e}"))?;
    let status = match wait_until(&mut child, Instant::now() + OPEN_DEADLINE) {
        Ok(Some(status)) => status,
        Ok(None) => {
            std::thread::spawn(move || child.wait());
            return Ok(());
        }
        Err(e) => return Err(format!("{program}: {e}")),
    };
    if status.success() || cfg!(windows) {
        Ok(())
    } else {
        Err(format!("{program} could not open {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_private_file_holds_the_bytes_and_is_never_reused() {
        let a = private_file("png", b"one").unwrap();
        let b = private_file("png", b"two").unwrap();
        assert_ne!(a, b);
        assert_eq!(std::fs::read(&a).unwrap(), b"one");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&a).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(b);
    }

    // Given a folder with this module's files and someone else's
    // When files are swept a day and a half later
    // Then only this module's files are gone
    #[test]
    fn only_old_files_of_ours_are_swept() {
        let folder = tempfile::tempdir().unwrap();
        for name in [
            "withnav-1-0.png",
            "withnav-1-1.jpg",
            "notes.png",
            "withnav-config.toml",
        ] {
            std::fs::write(folder.path().join(name), b"x").unwrap();
        }
        assert!(remove_stale(folder.path(), SystemTime::now()).is_empty());
        assert_eq!(std::fs::read_dir(folder.path()).unwrap().count(), 4);
        assert!(
            remove_stale(
                folder.path(),
                SystemTime::now() + STALE_AFTER + STALE_AFTER / 2,
            )
            .is_empty()
        );
        let mut left: Vec<_> = std::fs::read_dir(folder.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["notes.png", "withnav-config.toml"]);
    }

    // Given old files whose names only start like this module's
    // When files are swept a day and a half later
    // Then only the exact generated name is removed and the user's files are kept
    #[test]
    fn an_unrelated_withnav_file_survives_the_sweep() {
        let folder = tempfile::tempdir().unwrap();
        let kept = [
            "withnav-notes.png",
            "withnav-1.png",
            "withnav-1-x.png",
            "withnav--0.png",
            "withnav-1-0.png.png",
            "withnav-1-0.tiff",
        ];
        for name in kept.iter().chain(&["withnav-12-3.webp"]) {
            std::fs::write(folder.path().join(name), b"x").unwrap();
        }
        remove_stale(
            folder.path(),
            SystemTime::now() + STALE_AFTER + STALE_AFTER / 2,
        );
        let mut left: Vec<_> = std::fs::read_dir(folder.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        let mut kept = kept.to_vec();
        kept.sort();
        assert_eq!(left, kept);
    }

    #[test]
    fn every_viewer_extension_is_swept() {
        for media_type in ["image/png", "image/jpeg", "image/gif", "image/webp", "x/y"] {
            assert!(is_generated_name(&format!(
                "withnav-1-2.{}",
                extension(media_type)
            )));
        }
    }

    // Given an old generated file in a folder this user may not change
    // When files are swept
    // Then the file is kept and its removal error is returned
    #[cfg(unix)]
    #[test]
    fn a_failed_removal_is_reported() {
        use std::os::unix::fs::PermissionsExt;
        struct Restore<'a>(&'a Path);
        impl Drop for Restore<'_> {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o700));
            }
        }
        let folder = tempfile::tempdir().unwrap();
        let file = folder.path().join("withnav-1-0.png");
        std::fs::write(&file, b"x").unwrap();
        std::fs::set_permissions(folder.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let _restore = Restore(folder.path());
        let failures = remove_stale(
            folder.path(),
            SystemTime::now() + STALE_AFTER + STALE_AFTER / 2,
        );
        if !file.exists() {
            // Root ignores the folder's mode, so there is no failure to force.
            return;
        }
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, file);
        assert_eq!(failures[0].1.kind(), std::io::ErrorKind::PermissionDenied);
    }

    // Given a write that failed after the file was created
    // Then the error is returned and the partial file is gone
    #[test]
    fn a_failed_write_leaves_no_partial_file() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("withnav-1-0.png");
        std::fs::write(&path, b"half").unwrap();
        let error =
            finish_write(path.clone(), Err(std::io::Error::other("disk full"))).unwrap_err();
        assert!(error.contains("disk full"), "{error}");
        assert!(!path.exists());
    }

    /// A pipe that yields `good` bytes, then fails.
    struct Failing {
        good: usize,
    }

    impl Read for Failing {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.good == 0 {
                return Err(std::io::Error::other("pipe broke"));
            }
            let n = self.good.min(buf.len());
            buf[..n].fill(1);
            self.good -= n;
            Ok(n)
        }
    }

    // Given a reader whose pipe fails before, inside or past the kept part
    // Then the drain is an error, never the bytes read so far
    #[test]
    fn a_failing_pipe_is_an_error() {
        for good in [0, 10, 100] {
            let error = drain(Failing { good }, 50).unwrap_err();
            assert_eq!(error.to_string(), "pipe broke", "after {good} bytes");
        }
        assert_eq!(drain(&b"abc"[..], 2).unwrap(), b"ab");
    }

    #[test]
    fn a_panicked_drain_is_an_error() {
        let thread = std::thread::spawn(|| -> std::io::Result<Vec<u8>> { panic!("drain") });
        let error = joined(thread, "output").unwrap_err();
        assert!(error.to_string().contains("output"), "{error}");
    }

    #[test]
    fn a_timeout_says_when_the_reader_could_not_be_stopped() {
        let plain = timed_out(Ok(()));
        assert_eq!(plain.kind(), std::io::ErrorKind::TimedOut);
        assert!(!plain.to_string().contains("could not stop"));
        let stuck = timed_out(Err(std::io::Error::other("no such process")));
        assert_eq!(stuck.kind(), std::io::ErrorKind::TimedOut);
        assert!(stuck.to_string().contains("no answer after"), "{stuck}");
        assert!(
            stuck
                .to_string()
                .contains("could not stop it: no such process"),
            "{stuck}"
        );
    }

    // Given a reader that prints far more than an attachment may hold
    // Then what is kept stops one byte past the limit, and the reader still finishes
    #[cfg(unix)]
    #[test]
    fn a_reader_output_is_kept_only_up_to_the_limit() {
        let count = format!("count={}", MAX_ATTACHMENT_BYTES / 65536 + 4);
        let output = read_program("dd", &["if=/dev/zero", "bs=65536", &count]).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len() as u64, MAX_ATTACHMENT_BYTES + 1);
    }

    #[test]
    fn the_viewer_gets_an_extension_it_knows() {
        assert_eq!(extension("image/jpeg"), "jpg");
        assert_eq!(extension("image/png"), "png");
        assert_eq!(extension("text/html"), "img");
    }
}
