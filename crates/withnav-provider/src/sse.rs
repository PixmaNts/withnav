//! Turns a provider's chat-completion byte stream into the `data` of each SSE event.
//!
//! Parsing is `sse-stream`'s. This adapter adds three things it does not do:
//!
//! - it delivers an unterminated last event at end of stream: a provider that ends on
//!   `data: [DONE]` without the blank line still completes its turn;
//! - it bounds the wire bytes of one SSE block ([`MAX_FRAME_BYTES`]), so a stream that never
//!   closes a block fails the turn instead of exhausting the process's memory;
//! - it keeps a failure of the byte stream itself ([`SseError::Transport`], with its source)
//!   apart from a stream that arrived but is not valid SSE.
use std::error::Error as StdError;
use std::fmt;

use bytes::Bytes;
use futures_util::{Stream, StreamExt, future, stream};
use sse_stream::{Error, SseByteStream};

use crate::error::SseError;

/// Most wire bytes of one SSE block: everything from the end of one blank line to the end of the
/// next (data lines, other fields, comments and the line endings), counted as the bytes arrive,
/// across as many chunks as the block spans. Every blank line closes a block, so a run of
/// comment-only keep-alive blocks never adds up, however long it lasts. `sse-stream` itself bounds
/// only the scratch space it keeps between events, not the size of one event.
///
/// Why a bound: an event only ends at a blank line. A stream that never sends one (a broken
/// gateway, a proxy answering with an HTML page or a file, one endless `data:` line) would
/// otherwise grow the process's memory until it is killed, taking every session and the server
/// down, not just the turn. Timeouts do not catch it: bytes keep arriving. Past the bound the
/// stream fails with [`SseError::MalformedFrame`], which fails that turn only; the events
/// completed before the oversized block are still delivered.
///
/// Why 4 MiB: a chat-completion chunk is a few hundred bytes to a few KiB, and tool-call arguments
/// arrive in fragments, so no legitimate event comes near it. The value was not measured (it is
/// the plugin frame default) and is not a security boundary; raising it is safe. Field names and
/// line endings count, so a payload within a few bytes of the bound can fail.
///
/// It is not a limit on the length of a response (a long answer is many events), and it is not
/// configurable.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Decodes `bytes` into the `data` of each event; blocks without `data` are skipped. After an
/// error the stream ends.
pub fn decode_events<S, E>(bytes: S) -> impl Stream<Item = Result<String, SseError>> + Unpin
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: StdError + Send + Sync + 'static,
{
    let mut meter = BlockMeter::default();
    let guarded = bytes
        .flat_map(move |chunk| {
            stream::iter(match chunk {
                Err(e) => vec![Err(WireError::Transport(Box::new(e)))],
                Ok(c) => match meter.overflow_at(&c) {
                    None => vec![Ok(c)],
                    Some(fit) => vec![Ok(c.slice(..fit)), Err(WireError::TooLarge)],
                },
            })
        })
        // Closes an unterminated last event; added after the meter, so it never counts.
        .chain(stream::once(future::ok(Bytes::from_static(b"\n\n"))));
    SseByteStream::new(guarded).filter_map(|item| {
        future::ready(match item {
            Ok(sse) => sse.data.map(Ok),
            Err(Error::Utf8Parse(_)) => Some(Err(SseError::Utf8)),
            Err(Error::Body(e)) => Some(Err(match e.downcast::<WireError>() {
                Ok(wire) => match *wire {
                    WireError::Transport(e) => SseError::Transport(e),
                    too_large => SseError::MalformedFrame(too_large.to_string()),
                },
                Err(e) => SseError::Transport(e),
            })),
            Err(e) => Some(Err(SseError::MalformedFrame(e.to_string()))),
        })
    })
}

/// Why the byte stream handed to the parser ended early. It travels through `sse-stream` as its
/// body error and is unwrapped again into the matching [`SseError`].
#[derive(Debug)]
enum WireError {
    Transport(Box<dyn StdError + Send + Sync>),
    TooLarge,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(e) => e.fmt(f),
            Self::TooLarge => write!(f, "SSE block exceeds {MAX_FRAME_BYTES} bytes"),
        }
    }
}

impl StdError for WireError {}

/// Counts the wire bytes of the current SSE block, with the line-ending rules of the SSE format
/// (CRLF, LF or CR end a line; an empty line ends the block).
#[derive(Default)]
struct BlockMeter {
    bytes: usize,
    mid_line: bool,
    after_cr: bool,
}

impl BlockMeter {
    /// Where in `chunk` the current block passes [`MAX_FRAME_BYTES`], or `None` if every block
    /// fits. A byte counts toward the block it ends; the LF of a CRLF blank line opens the next.
    fn overflow_at(&mut self, chunk: &[u8]) -> Option<usize> {
        for (i, &b) in chunk.iter().enumerate() {
            self.bytes += 1;
            if self.bytes > MAX_FRAME_BYTES {
                return Some(i);
            }
            let crlf_tail = self.after_cr && b == b'\n';
            self.after_cr = b == b'\r';
            match b {
                _ if crlf_tail => {}
                b'\n' | b'\r' if self.mid_line => self.mid_line = false,
                b'\n' | b'\r' => self.bytes = 0,
                _ => self.mid_line = true,
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    async fn decode(chunks: Vec<Result<Vec<u8>, io::Error>>) -> Vec<Result<String, SseError>> {
        let chunks = chunks.into_iter().map(|c| c.map(Bytes::from));
        decode_events(stream::iter(chunks)).collect().await
    }

    fn is_too_large(out: &[Result<String, SseError>]) -> bool {
        matches!(out.last(), Some(Err(SseError::MalformedFrame(m))) if m.contains("exceeds"))
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
    async fn transport_error_keeps_its_source() {
        let mut chunks = vec![Ok(b"data: a\n\n".to_vec()), Err(io::Error::other("boom"))];
        chunks.push(Ok(b"data: must not resume\n\n".to_vec()));
        let out = decode(chunks).await;
        let [Ok(a), Err(SseError::Transport(e))] = out.as_slice() else {
            panic!("{out:?}")
        };
        assert_eq!(a, "a");
        assert_eq!(e.downcast_ref::<io::Error>().unwrap().to_string(), "boom");
    }

    #[tokio::test]
    async fn keep_alive_comments_do_not_count_toward_the_cap() {
        for heartbeat in [&b": keep-alive\n\n"[..], b": keep-alive\r\n\r\n", b":\r\r"] {
            let mut body = heartbeat.repeat(MAX_FRAME_BYTES / heartbeat.len() + 2);
            body.extend_from_slice(b"data: [DONE]\n\n");
            let pieces = body.chunks(999).map(|c| Ok(c.to_vec())).collect();
            for out in [decode(vec![Ok(body.clone())]).await, decode(pieces).await] {
                assert!(
                    matches!(out.as_slice(), [Ok(d)] if d == "[DONE]"),
                    "{out:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn oversized_frame_rejected() {
        let mut line = b"data: ".to_vec();
        line.resize(MAX_FRAME_BYTES + 1, b'x');
        let pieces = line.chunks(1000).map(|c| Ok(c.to_vec())).collect();
        for out in [decode(vec![Ok(line.clone())]).await, decode(pieces).await] {
            assert!(out.len() == 1 && is_too_large(&out), "{out:?}");
        }
        let out = decode(vec![Ok(b"data: small\n\n".repeat(MAX_FRAME_BYTES / 6))]).await;
        assert!(out.len() == MAX_FRAME_BYTES / 6 && out.iter().all(Result::is_ok));
    }

    #[tokio::test]
    async fn the_cap_counts_one_block_including_its_blank_line() {
        // `data: ` (6) + payload + `\n\n` (2) is exactly the cap; one more byte is over it.
        for extra in [0, 1] {
            let mut block = b"data: ".to_vec();
            block.resize(MAX_FRAME_BYTES - 2 + extra, b'x');
            block.extend_from_slice(b"\n\n");
            let out = decode(vec![Ok(b": ping\n\n".to_vec()), Ok(block)]).await;
            match extra {
                0 => assert!(matches!(out.as_slice(), [Ok(d)] if d.len() == MAX_FRAME_BYTES - 8)),
                _ => assert!(out.len() == 1 && is_too_large(&out), "{out:?}"),
            }
        }
    }

    #[tokio::test]
    async fn events_before_an_oversized_block_in_the_same_chunk_are_delivered() {
        let mut body = b"data: a\n\ndata: [DONE]\n\ndata: ".to_vec();
        body.resize(body.len() + MAX_FRAME_BYTES, b'x');
        let out = decode(vec![Ok(body)]).await;
        assert!(matches!(&out[..2], [Ok(a), Ok(d)] if a == "a" && d == "[DONE]"));
        assert!(out.len() == 3 && is_too_large(&out), "{out:?}");
    }

    #[tokio::test]
    async fn transport_parse_and_json_failures_stay_distinct() {
        use crate::error::ProviderError;
        let cancel = tokio_util::sync::CancellationToken::new();
        let bodies: [Vec<Result<Bytes, io::Error>>; 3] = [
            vec![Err(io::Error::other("reset"))],
            vec![Ok(Bytes::from_static(b"data: \xff\n\n"))],
            vec![Ok(Bytes::from_static(b"data: {not json\n\n"))],
        ];
        let mut kinds = Vec::new();
        for body in bodies {
            let events = decode_events(stream::iter(body));
            kinds.push(crate::consume_event_stream(events, &cancel, &mut |_| {}).await);
        }
        let [Err(transport), Err(parse), Err(json)] = kinds.as_slice() else {
            panic!("{kinds:?}")
        };
        assert!(matches!(
            transport,
            ProviderError::Sse(SseError::Transport(_))
        ));
        assert!(matches!(parse, ProviderError::Sse(SseError::Utf8)));
        assert!(matches!(json, ProviderError::Api { status: 0, message, .. }
            if message.starts_with("malformed SSE JSON")));
    }
}
