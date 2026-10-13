use std::path::Path;

pub fn file_uri(path: &Path) -> String {
    let text = path.to_string_lossy().into_owned();
    #[cfg(windows)]
    let text = {
        let mut text = text
            .strip_prefix(r"\\?\")
            .unwrap_or(&text)
            .replace('\\', "/");
        if let Some(drive) = text.get(..1) {
            text = drive.to_ascii_lowercase() + &text[1..];
        }
        text.insert(0, '/');
        text
    };
    let encoded: String = text
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~') {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect();
    format!("file://{encoded}")
}
