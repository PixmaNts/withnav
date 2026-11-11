//! JSON-RPC framing for tests that drive a spawned `nav lsp` over stdio.
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

/// Reads one `Content-Length`-framed JSON-RPC message.
pub async fn read_message<R: tokio::io::AsyncBufRead + Unpin>(r: &mut R) -> serde_json::Value {
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        assert!(r.read_line(&mut line).await.expect("header") > 0, "EOF");
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some(n) = line.strip_prefix("Content-Length:") {
            len = n.trim().parse().expect("length");
        }
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body).await.expect("body");
    serde_json::from_slice(&body).expect("json")
}

/// One `Content-Length`-framed JSON-RPC message.
pub fn frame(value: serde_json::Value) -> Vec<u8> {
    let body = value.to_string();
    format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
}
