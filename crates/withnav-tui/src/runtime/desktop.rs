//! The local desktop, through the programs it usually has: the image on the clipboard and the
//! system image viewer. Over SSH they are the remote machine's, so they usually fail, with a
//! message that says which program was missing.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

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

/// The PNG bytes of the image on the clipboard.
pub(super) fn clipboard_image() -> Result<Vec<u8>, String> {
    let mut tried = Vec::new();
    for (program, args) in CLIPBOARD_READERS {
        let output = match Command::new(program)
            .args(*args)
            .stdin(Stdio::null())
            .output()
        {
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
    let status = Command::new(program)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("{program}: {e}"))?;
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

    #[test]
    fn the_viewer_gets_an_extension_it_knows() {
        assert_eq!(extension("image/jpeg"), "jpg");
        assert_eq!(extension("image/png"), "png");
        assert_eq!(extension("text/html"), "img");
    }
}
