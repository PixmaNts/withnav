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

/// Waits for `child` until `deadline`; `None` once it is past.
fn wait_until(child: &mut Child, deadline: Instant) -> std::io::Result<Option<ExitStatus>> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// What a clipboard reader printed: at most one byte past the attachment limit on stdout (enough
/// to know it is over) and the start of stderr.
struct Output {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Runs a clipboard reader, killed past [`CLIPBOARD_DEADLINE`].
fn read_program(program: &str, args: &[&str]) -> std::io::Result<Output> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let drain = |pipe: Option<Box<dyn Read + Send>>, limit: u64| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(pipe) = pipe {
                let mut pipe = pipe.take(limit);
                let _ = pipe.read_to_end(&mut bytes);
                // The rest is read and dropped, so the program is never blocked on a full pipe.
                let _ = std::io::copy(&mut pipe.into_inner(), &mut std::io::sink());
            }
            bytes
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        MAX_ATTACHMENT_BYTES + 1,
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        4096,
    );
    let status = match wait_until(&mut child, Instant::now() + CLIPBOARD_DEADLINE)? {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("no answer after {} s", CLIPBOARD_DEADLINE.as_secs()),
            ));
        }
    };
    Ok(Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
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

/// Writes `bytes` to a new file only this user can read, in the temporary folder.
pub(super) fn private_file(extension: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    remove_stale(&std::env::temp_dir(), SystemTime::now());
    let path = std::env::temp_dir().join(format!(
        "withnav-{}-{}.{extension}",
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
    file.write_all(bytes)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// Removes the files [`private_file`] wrote in `folder` more than [`STALE_AFTER`] before `now`.
/// Another user's files are left alone (the temporary folder refuses), as are files of other names.
fn remove_stale(folder: &Path, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let ours = name.starts_with("withnav-")
            && ["png", "jpg", "gif", "webp", "img"]
                .iter()
                .any(|ext| name.ends_with(&format!(".{ext}")));
        let stale = entry
            .metadata()
            .ok()
            .filter(|m| m.is_file())
            .and_then(|m| m.modified().ok())
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > STALE_AFTER);
        if ours && stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
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
        remove_stale(folder.path(), SystemTime::now());
        assert_eq!(std::fs::read_dir(folder.path()).unwrap().count(), 4);
        remove_stale(
            folder.path(),
            SystemTime::now() + STALE_AFTER + STALE_AFTER / 2,
        );
        let mut left: Vec<_> = std::fs::read_dir(folder.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["notes.png", "withnav-config.toml"]);
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
