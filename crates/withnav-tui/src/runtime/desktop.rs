//! The local desktop, through the programs it usually has: the image on the clipboard and the
//! system image viewer. Over SSH they are the remote machine's, so they usually fail, with a
//! message that says which program was missing.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use crate::state::MAX_ATTACHMENT_BYTES;

/// Stands for the image type in a reader's arguments; see [`Reader::read`].
const TYPE: &str = "{type}";

/// A clipboard program, and how to ask it for an image.
struct Reader {
    program: &'static str,
    /// Arguments that print the clipboard as the image type [`TYPE`] stands for. A program that
    /// only knows PNG has no [`TYPE`] and no `list`.
    read: &'static [&'static str],
    /// Arguments that print, one per line, the types the clipboard offers.
    list: Option<&'static [&'static str]>,
}

/// The programs that print the clipboard image, tried in order.
const CLIPBOARD_READERS: &[Reader] = if cfg!(target_os = "macos") {
    &[Reader {
        program: "pngpaste",
        read: &["-"],
        list: None,
    }]
} else {
    &[
        Reader {
            program: "wl-paste",
            read: &["--no-newline", "--type", TYPE],
            list: Some(&["--list-types"]),
        },
        Reader {
            program: "xclip",
            read: &["-selection", "clipboard", "-target", TYPE, "-out"],
            list: Some(&["-selection", "clipboard", "-target", "TARGETS", "-out"]),
        },
    ]
};

/// PNG is asked for first; a clipboard that does not offer it is asked what it offers, and the
/// first of these it has is taken. Each is a type the server accepts as an attachment.
const PNG: &str = "image/png";
const OTHER_TYPES: [&str; 3] = ["image/jpeg", "image/webp", "image/gif"];

/// How long a clipboard reader may take; a clipboard owner that never answers would otherwise
/// hold the paste forever.
const CLIPBOARD_DEADLINE: Duration = Duration::from_secs(5);

/// How long the pipes of a reader that has exited are waited for. Past it a descendant that kept
/// them open is killed, and they are waited for once more for as long again.
const DRAIN_GRACE: Duration = Duration::from_millis(500);

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

/// A running program, as far as waiting for it needs.
trait Process {
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>>;
    fn kill(&mut self) -> std::io::Result<()>;
    fn wait(&mut self) -> std::io::Result<ExitStatus>;
}

impl Process for Child {
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        Child::try_wait(self)
    }
    fn kill(&mut self) -> std::io::Result<()> {
        Child::kill(self)
    }
    fn wait(&mut self) -> std::io::Result<ExitStatus> {
        Child::wait(self)
    }
}

/// Waits for `process` until `deadline`; `None` once it is past. A failed poll stops and reaps
/// the process before the error is returned: nobody is left holding it.
fn wait_until(
    process: &mut impl Process,
    deadline: Instant,
) -> std::io::Result<Option<ExitStatus>> {
    loop {
        match process.try_wait() {
            Ok(Some(status)) => return Ok(Some(status)),
            Ok(None) => {}
            Err(e) => {
                let _ = process.kill();
                let _ = process.wait();
                return Err(e);
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        std::thread::sleep(left.min(POLL_INTERVAL));
    }
}

/// A clipboard reader started as the leader of its own process group, so that whatever it
/// started goes down with it.
struct Grouped(Child);

impl Grouped {
    fn spawn(command: &mut Command) -> std::io::Result<Self> {
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(command, 0);
        command.spawn().map(Self)
    }

    /// Kills every process of the group; the reader itself too where there are no groups.
    fn kill_group(&mut self) -> std::io::Result<()> {
        #[cfg(unix)]
        if let Ok(pid) = i32::try_from(self.0.id()) {
            use nix::errno::Errno;
            return match nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            ) {
                Ok(()) | Err(Errno::ESRCH) => Ok(()),
                Err(e) => Err(e.into()),
            };
        }
        self.0.kill()
    }
}

impl Process for Grouped {
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.0.try_wait()
    }
    fn kill(&mut self) -> std::io::Result<()> {
        let group = self.kill_group();
        let reader = self.0.kill();
        group.and(reader)
    }
    fn wait(&mut self) -> std::io::Result<ExitStatus> {
        self.0.wait()
    }
}

/// What a clipboard reader printed: at most one byte past the attachment limit on stdout (enough
/// to know it is over) and the start of stderr.
struct Output {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Reads `pipe` on its own thread: the first `limit` bytes, which arrive on the returned channel.
fn drain(pipe: Option<Box<dyn Read + Send>>, limit: u64) -> mpsc::Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(pipe) = pipe {
            let mut pipe = pipe.take(limit);
            let _ = pipe.read_to_end(&mut bytes);
            // The rest is read and dropped, so the program is never blocked on a full pipe.
            let _ = std::io::copy(&mut pipe.into_inner(), &mut std::io::sink());
        }
        let _ = sender.send(bytes);
    });
    receiver
}

/// What a [`drain`] has read, if its pipe closes by `until`.
fn drained(receiver: &mpsc::Receiver<Vec<u8>>, until: Instant) -> Option<Vec<u8>> {
    match receiver.recv_timeout(until.saturating_duration_since(Instant::now())) {
        Ok(bytes) => Some(bytes),
        Err(mpsc::RecvTimeoutError::Timeout) => None,
        // The thread died without an answer: nothing was read.
        Err(mpsc::RecvTimeoutError::Disconnected) => Some(Vec::new()),
    }
}

/// Runs a clipboard reader, and everything it starts, killed past `deadline`. The pipes are
/// waited for only up to `deadline` as well: a descendant that holds one open after the reader
/// exited is killed rather than waited for.
fn read_program(program: &str, args: &[&str], deadline: Instant) -> std::io::Result<Output> {
    if Instant::now() >= deadline {
        return Err(timed_out());
    }
    let mut reader = Grouped::spawn(
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )?;
    let stdout = drain(
        reader
            .0
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        MAX_ATTACHMENT_BYTES + 1,
    );
    let stderr = drain(
        reader
            .0
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        STDERR_KEPT,
    );
    let status = match wait_until(&mut reader, deadline)? {
        Some(status) => status,
        None => {
            // A kill that failed would leave the wait blocking for as long as the reader runs.
            if reader.kill().is_ok() {
                let _ = reader.wait();
            } else {
                std::thread::spawn(move || reader.wait());
            }
            return Err(timed_out());
        }
    };
    // What each pipe gave is kept across the two waits, so a pipe that closed in time is not
    // read again once the other is cut.
    let (mut out, mut err) = (None, None);
    for kill_first in [false, true] {
        if kill_first {
            let _ = reader.kill_group();
        }
        let until = (Instant::now() + DRAIN_GRACE).min(deadline);
        out = out.or_else(|| drained(&stdout, until));
        err = err.or_else(|| drained(&stderr, until));
        if out.is_some() && err.is_some() {
            break;
        }
    }
    let (Some(stdout), Some(stderr)) = (out, err) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "a process it started kept its output open",
        ));
    };
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn timed_out() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("no answer after {} s", CLIPBOARD_DEADLINE.as_secs()),
    )
}

/// An image read from the clipboard.
pub(super) struct ClipboardImage {
    pub media_type: &'static str,
    pub bytes: Vec<u8>,
}

/// `reader`'s clipboard as the image type `media_type`: the bytes, or why there are none.
fn read_type(
    reader: &Reader,
    media_type: &str,
    deadline: Instant,
) -> std::io::Result<Result<Vec<u8>, String>> {
    let args: Vec<&str> = reader
        .read
        .iter()
        .map(|a| if *a == TYPE { media_type } else { a })
        .collect();
    let output = read_program(reader.program, &args, deadline)?;
    if output.status.success() && !output.stdout.is_empty() {
        return Ok(Ok(output.stdout));
    }
    let reason = String::from_utf8_lossy(&output.stderr);
    let reason = crate::reducer::strip_controls(reason.trim());
    Ok(Err(if reason.is_empty() {
        format!("{} found no image", reader.program)
    } else {
        format!("{}: {reason}", reader.program)
    }))
}

/// The first of [`OTHER_TYPES`] that `reader`'s clipboard offers.
fn offered_type(reader: &Reader, deadline: Instant) -> std::io::Result<Option<&'static str>> {
    let Some(list) = reader.list else {
        return Ok(None);
    };
    let output = read_program(reader.program, list, deadline)?;
    if !output.status.success() {
        return Ok(None);
    }
    let offered = String::from_utf8_lossy(&output.stdout);
    Ok(OTHER_TYPES
        .into_iter()
        .find(|t| offered.lines().any(|line| line.trim() == *t)))
}

/// The image on the clipboard: PNG if it holds one, else the first of JPEG, WebP and GIF.
pub(super) fn clipboard_image() -> Result<ClipboardImage, String> {
    read_clipboard(CLIPBOARD_READERS, Instant::now() + CLIPBOARD_DEADLINE)
}

fn read_clipboard(readers: &[Reader], deadline: Instant) -> Result<ClipboardImage, String> {
    let mut tried = Vec::new();
    for reader in readers {
        let program = reader.program;
        let attempt: std::io::Result<Result<(&'static str, Vec<u8>), String>> = (|| {
            let png = match read_type(reader, PNG, deadline)? {
                Ok(bytes) => return Ok(Ok((PNG, bytes))),
                Err(reason) => reason,
            };
            Ok(match offered_type(reader, deadline)? {
                Some(other) => read_type(reader, other, deadline)?.map(|bytes| (other, bytes)),
                None => Err(png),
            })
        })();
        match attempt {
            Ok(Ok((media_type, bytes))) => {
                if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
                    return Err(format!(
                        "the clipboard image is larger than {}",
                        withnav_client_api::attachment::human_size(MAX_ATTACHMENT_BYTES)
                    ));
                }
                return Ok(ClipboardImage { media_type, bytes });
            }
            Ok(Err(reason)) => tried.push(reason),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tried.push(format!("{program} is not installed"));
            }
            Err(e) => return Err(format!("{program}: {e}")),
        }
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

    // Given a reader that prints far more than an attachment may hold
    // Then what is kept stops one byte past the limit, and the reader still finishes
    #[cfg(unix)]
    #[test]
    fn a_reader_output_is_kept_only_up_to_the_limit() {
        let count = format!("count={}", MAX_ATTACHMENT_BYTES / 65536 + 4);
        let deadline = Instant::now() + CLIPBOARD_DEADLINE;
        let output = read_program("dd", &["if=/dev/zero", "bs=65536", &count], deadline).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len() as u64, MAX_ATTACHMENT_BYTES + 1);
    }

    /// A reader whose clipboard holds only `offered`, as a shell script: it answers `list` with
    /// that type and prints `bytes` for it, and fails for any other.
    #[cfg(unix)]
    fn script_reader(offered: &'static str, bytes: &'static str) -> Reader {
        let read: Vec<&'static str> = vec![
            "-c",
            r#"[ "$1" = "$2" ] && printf %s "$3" || { echo "no $1" >&2; exit 1; }"#,
            "sh",
            TYPE,
            offered,
            bytes,
        ];
        let list: Vec<&'static str> = vec!["-c", r#"echo text/plain; echo "$1""#, "sh", offered];
        // Leaked: a reader's arguments are `'static`, and a test process is short-lived.
        Reader {
            program: "sh",
            read: Vec::leak(read),
            list: Some(Vec::leak(list)),
        }
    }

    // Given a clipboard that holds a JPEG and no PNG
    // When the image is read
    // Then the JPEG is returned as such
    #[cfg(unix)]
    #[test]
    fn a_clipboard_with_only_a_jpeg_gives_the_jpeg() {
        let image = read_clipboard(
            &[script_reader("image/jpeg", "JPEGDATA")],
            Instant::now() + CLIPBOARD_DEADLINE,
        )
        .unwrap();
        assert_eq!(image.media_type, "image/jpeg");
        assert_eq!(image.bytes, b"JPEGDATA");
    }

    // Given a clipboard that holds a PNG and a JPEG
    // Then the PNG is taken, and the offered types are never listed
    #[cfg(unix)]
    #[test]
    fn png_is_preferred() {
        let folder = tempfile::tempdir().unwrap();
        let listed = folder.path().join("listed");
        let reader = Reader {
            program: "sh",
            read: Vec::leak(vec![
                "-c",
                r#"[ "$1" = image/png ] && printf PNGDATA"#,
                "sh",
                TYPE,
            ]),
            list: Some(Vec::leak(vec![
                "-c",
                r#"touch "$1"; echo image/jpeg"#,
                "sh",
                Box::leak(listed.display().to_string().into_boxed_str()),
            ])),
        };
        let image = read_clipboard(&[reader], Instant::now() + CLIPBOARD_DEADLINE).unwrap();
        assert_eq!(image.media_type, "image/png");
        assert_eq!(image.bytes, b"PNGDATA");
        assert!(!listed.exists());
    }

    // Given a clipboard that offers only a type the server does not accept
    // Then no image is reported, with the reader's own reason
    #[cfg(unix)]
    #[test]
    fn an_unsupported_type_is_no_image() {
        let error = read_clipboard(
            &[script_reader("image/bmp", "BMP")],
            Instant::now() + CLIPBOARD_DEADLINE,
        )
        .err()
        .unwrap();
        assert!(error.starts_with("no image on the clipboard"), "{error}");
        assert!(error.contains("no image/png"), "{error}");
    }

    /// A process that fails to be polled and records what was done to it.
    #[derive(Default)]
    struct Broken {
        killed: bool,
        reaped: bool,
    }

    impl Process for Broken {
        fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
            Err(std::io::Error::other("poll failed"))
        }
        fn kill(&mut self) -> std::io::Result<()> {
            self.killed = true;
            Ok(())
        }
        fn wait(&mut self) -> std::io::Result<ExitStatus> {
            assert!(self.killed, "reaped before it was killed");
            self.reaped = true;
            Err(std::io::Error::other("no status"))
        }
    }

    // Given a process whose state cannot be read
    // When it is waited for
    // Then the error is returned only after the process was killed and reaped
    #[test]
    fn a_failed_poll_kills_and_reaps_the_process() {
        let mut process = Broken::default();
        let error = wait_until(&mut process, Instant::now() + CLIPBOARD_DEADLINE).unwrap_err();
        assert_eq!(error.to_string(), "poll failed");
        assert!(process.killed && process.reaped);
    }

    /// Whether the process `pid` is gone (a zombie is gone too: nothing runs).
    #[cfg(target_os = "linux")]
    fn is_gone(pid: u32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat
                .rsplit(')')
                .next()
                .is_some_and(|rest| rest.trim_start().starts_with('Z')),
            Err(_) => true,
        }
    }

    #[cfg(target_os = "linux")]
    fn wait_gone(pid: u32) {
        let until = Instant::now() + Duration::from_secs(3);
        while !is_gone(pid) {
            assert!(Instant::now() < until, "process {pid} is still running");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    // Given a reader that exits but leaves a descendant holding its output pipes
    // When it is read
    // Then the call returns well before the descendant would end, with what the reader printed,
    //      and the descendant is killed
    #[cfg(target_os = "linux")]
    #[test]
    fn a_descendant_holding_the_pipes_does_not_hold_the_call() {
        let started = Instant::now();
        let output = read_program(
            "sh",
            &["-c", "sleep 30 & echo $!"],
            started + CLIPBOARD_DEADLINE,
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(4));
        let pid: u32 = String::from_utf8(output.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        wait_gone(pid);
    }

    // Given a reader whose output is complete while a descendant keeps only its error pipe open
    // Then the output is kept
    #[cfg(target_os = "linux")]
    #[test]
    fn output_is_kept_when_only_the_error_pipe_stays_open() {
        let output = read_program(
            "sh",
            &["-c", "sleep 30 >/dev/null & printf PNGDATA"],
            Instant::now() + CLIPBOARD_DEADLINE,
        )
        .unwrap();
        assert_eq!(output.stdout, b"PNGDATA");
    }

    // Given a deadline that has passed
    // Then no program is started
    #[test]
    fn nothing_is_started_past_the_deadline() {
        let error = read_program("sh", &["-c", "true"], Instant::now())
            .err()
            .unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    // Given a reader that never answers and has started a descendant
    // When the deadline passes
    // Then the call returns an error at the deadline and the whole group is gone
    #[cfg(target_os = "linux")]
    #[test]
    fn a_hung_reader_is_killed_with_its_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let script = format!("sleep 30 & echo $! > {}; wait", pid_file.display());
        let started = Instant::now();
        let error = read_program("sh", &["-c", &script], started + Duration::from_millis(500))
            .err()
            .unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(3));
        let pid: u32 = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        wait_gone(pid);
    }

    #[test]
    fn the_viewer_gets_an_extension_it_knows() {
        assert_eq!(extension("image/jpeg"), "jpg");
        assert_eq!(extension("image/png"), "png");
        assert_eq!(extension("text/html"), "img");
    }
}
