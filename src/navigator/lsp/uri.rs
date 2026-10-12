//! URI handling for the Language Server: documents are keyed by the string
//! form of their URI, and `file://` URIs convert to filesystem paths here.

use std::path::PathBuf;

use tower_lsp_server::ls_types::Uri;

/// Normalizes a URI for use as a state key (documents are keyed by their
/// string form).
pub fn uri_key(uri: &Uri) -> String {
    uri.as_str().to_string()
}

/// Extracts a percent-decoded path from a `file://` URI string (the form
/// documents are keyed by). Returns `None` for other schemes and for a
/// malformed escape.
pub fn uri_to_path_str(uri: &str) -> Option<String> {
    platform_path(uri)
}

#[cfg(unix)]
fn platform_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    // `file://host/path`: the host is dropped, only the path locates the file.
    let rest = &rest[rest.find('/')?..];
    let decoded = percent_decode(rest)?;
    if decoded.starts_with('/') {
        Some(decoded)
    } else {
        None
    }
}

#[cfg(windows)]
fn platform_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let slash = rest.find('/')?;
    let authority = &rest[..slash];
    // Network paths are rejected until checked native I/O supports their security contract.
    if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
        return None;
    }
    let decoded = percent_decode(&rest[slash..])?;
    let path = decoded.strip_prefix('/')?;
    let bytes = path.as_bytes();
    if bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || bytes[2] != b'/'
        || path[2..].contains([':', '\\', '\0'])
    {
        return None;
    }
    Some(path.replace('/', "\\"))
}

/// Extracts a filesystem path from a `file://` URI, percent-decoded.
pub fn uri_to_path(uri: &Uri) -> Option<PathBuf> {
    uri_to_path_str(uri.as_str()).map(PathBuf::from)
}

/// Percent-decodes a URI component (the `%XX` escapes of the path).
/// Returns `None` when a `%` is not followed by two hex digits: guessing a
/// path from a malformed URI would key the document under the wrong file.
fn percent_decode(s: &str) -> Option<String> {
    let hex = |b: u8| -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    };
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = hex(*bytes.get(i + 1)?)?;
            let lo = hex(*bytes.get(i + 2)?)?;
            out.push(hi * 16 + lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn uri_paths_decode() {
        assert_eq!(
            uri_to_path_str("file:///ws/src/main.rs").as_deref(),
            Some("/ws/src/main.rs")
        );
        assert_eq!(
            uri_to_path_str("file:///ws/a%20b/c%C3%A9.rs").as_deref(),
            Some("/ws/a b/cé.rs")
        );
        assert_eq!(uri_to_path_str("file:///bad%2"), None, "incomplete escape");
        assert_eq!(uri_to_path_str("untitled:Untitled-1"), None);
        // A host-bearing file URI keeps its path (no host stripping: VS Code
        // never sends one for local files).
        assert_eq!(uri_to_path_str("file://localhost/x").as_deref(), Some("/x"));
    }

    #[test]
    #[cfg(windows)]
    fn uri_paths_decode() {
        assert_eq!(
            uri_to_path_str("file:///C:/a%20b/c%C3%A9.rs").as_deref(),
            Some("C:\\a b\\cé.rs")
        );
        assert_eq!(
            uri_to_path_str("file://localhost/d:/workspace/main.rs").as_deref(),
            Some("d:\\workspace\\main.rs")
        );
        for uri in [
            "file://server/share/file.rs",
            "file:///C:relative.rs",
            "file:///workspace/file.rs",
            "file:///C:/file:stream",
            "file:///C:/bad%2",
            "file:///C:/bad%00",
            "file:///C:/bad%5Cname",
            "untitled:Untitled-1",
        ] {
            assert_eq!(uri_to_path_str(uri), None, "{uri}");
        }
    }
}
