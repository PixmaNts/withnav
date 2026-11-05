//! Turns a provider's chat-completion byte stream into the `data` of each SSE event.
//!
//! Parsing is `sse-stream`'s. This adapter adds two things it does not do:
//!
//! - it delivers an unterminated last event at end of stream: a provider that ends on
//!   `data: [DONE]` without the blank line still completes its turn;
//! - it bounds the bytes buffered for one event not yet delivered ([`MAX_FRAME_BYTES`]), so a
//!   stream that never closes an event fails the turn instead of exhausting the process's memory.
use std::io;
use std::sync::{Arc, atomic::AtomicUsize, atomic::Ordering::Relaxed};

use bytes::Bytes;
use futures_util::{Stream, StreamExt, TryStreamExt, future, stream};
use sse_stream::{Error, SseByteStream};

use crate::error::SseError;

/// Most bytes received for one SSE event not yet delivered: everything that arrives after the
/// last delivered event (data lines, other fields, comments, separators), across as many chunks
/// as it spans. `sse-stream` itself bounds only the scratch space it keeps between events, not the
/// size of one event.
///
/// Why a bound: an event only ends at a blank line. A stream that never sends one (a broken
/// gateway, a proxy answering with an HTML page or a file, one endless `data:` line) would
/// otherwise grow the process's memory until it is killed, taking every session and the server
/// down, not just the turn. Timeouts do not catch it: bytes keep arriving. Past the bound the
/// stream fails with [`SseError::MalformedFrame`], which fails that turn only.
///
/// Why 4 MiB: a chat-completion chunk is a few hundred bytes to a few KiB, and tool-call arguments
/// arrive in fragments, so no legitimate event comes near it. The value was not measured (it is
/// the plugin frame default) and is not a security boundary; raising it is safe. Bytes are counted
/// in slices of at most 64 KiB, so the bound holds to within one slice.
///
/// It is not a limit on the length of a response (a long answer is many events), and it is not
/// configurable.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Largest piece handed to the parser at once: the count restarts when an event is delivered, so
/// one network chunk carrying many small events must not count as one oversized event.
const SLICE_BYTES: usize = 64 * 1024;

/// Decodes `bytes` into the `data` of each event; blocks without `data` are skipped. After an
/// error the stream ends.
pub fn decode_events<S, E>(bytes: S) -> impl Stream<Item = Result<String, SseError>> + Unpin
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    let pending = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&pending);
    let fits = move |s: &Bytes| counted.fetch_add(s.len(), Relaxed) + s.len() <= MAX_FRAME_BYTES;
    let too_large = || io::Error::other(format!("SSE event exceeds {MAX_FRAME_BYTES} bytes"));
    let slice = |c: Bytes| move |i| Ok(c.slice(i..c.len().min(i + SLICE_BYTES)));
    let guarded = bytes
        .map_err(|_| io::Error::other("transport error in SSE stream"))
        .chain(stream::once(future::ok(Bytes::from_static(b"\n\n"))))
        .map_ok(move |c| stream::iter((0..c.len()).step_by(SLICE_BYTES).map(slice(c))))
        .try_flatten()
        .and_then(move |s| future::ready(fits(&s).then_some(s).ok_or_else(too_large)));
    SseByteStream::new(guarded).filter_map(move |item| {
        pending.store(0, Relaxed);
        future::ready(match item {
            Ok(sse) => sse.data.map(Ok),
            Err(Error::Utf8Parse(_)) => Some(Err(SseError::Utf8)),
            Err(Error::Body(e)) => Some(Err(SseError::MalformedFrame(e.to_string()))),
            Err(e) => Some(Err(SseError::MalformedFrame(e.to_string()))),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn decode(chunks: Vec<Result<Vec<u8>, io::Error>>) -> Vec<Result<String, SseError>> {
        let chunks = chunks.into_iter().map(|c| c.map(Bytes::from));
        decode_events(stream::iter(chunks)).collect().await
    }

    #[tokio::test]
    async fn eof_flush_with_partial_unterminated_event() {
        let out = decode(vec![Ok(b": c\n\nevent: x\n\ndata: tail".to_vec())]).await;
        assert!(matches!(out.as_slice(), [Ok(s)] if s == "tail"));
    }

    #[tokio::test]
    async fn done_without_blank_line_completes_the_turn() {
        let body = br#"data: {"choices":[{"index":0,"delta":{"content":"hi"}}]}"#;
        let body = [
            Ok::<_, io::Error>(Bytes::from_static(body)),
            Ok("\n\ndata: [DONE]".into()),
        ];
        let cancel = tokio_util::sync::CancellationToken::new();
        let events = decode_events(stream::iter(body));
        let msg = crate::consume_event_stream(events, &cancel, &mut |_| {}).await;
        assert_eq!(msg.unwrap().text, "hi");
    }

    #[tokio::test]
    async fn transport_error_surfaced() {
        let mut chunks = vec![Ok(b"data: a\n\n".to_vec()), Err(io::Error::other("boom"))];
        chunks.push(Ok(b"data: must not resume\n\n".to_vec()));
        let out = decode(chunks).await;
        let [Ok(a), Err(SseError::MalformedFrame(m))] = out.as_slice() else {
            panic!("{out:?}")
        };
        assert_eq!([a, m], ["a", "transport error in SSE stream"]);
    }

    #[tokio::test]
    async fn oversized_frame_rejected() {
        let mut line = b"data: ".to_vec();
        line.resize(MAX_FRAME_BYTES + SLICE_BYTES, b'x');
        let pieces = line.chunks(1000).map(|c| Ok(c.to_vec())).collect();
        for out in [decode(vec![Ok(line.clone())]).await, decode(pieces).await] {
            assert!(matches!(out.as_slice(), [Err(SseError::MalformedFrame(_))]));
        }
        let out = decode(vec![Ok(b"data: small\n\n".repeat(MAX_FRAME_BYTES / 6))]).await;
        assert!(out.len() == MAX_FRAME_BYTES / 6 && out.iter().all(Result::is_ok));
    }
}
