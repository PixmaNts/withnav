//! SSE decoding, reconnect and cancellation; no UI state mutation.
//!
//! One subscription is one background task that owns the HTTP connection and
//! feeds a bounded channel; the returned stream reads that channel. Dropping
//! the stream or cancelling the token stops the task, including while it waits
//! on the network or sleeps between attempts.
use crate::client::{Client, OpenError};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use sse_stream::{Sse, SseByteStream};
use std::pin::Pin;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use withnav_client_api::event::EventEnvelope;

pub type EventStream = Pin<Box<dyn Stream<Item = StreamEvent> + Send>>;

/// What a subscription reports, in the order it happens.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// A connection was established (also after each reconnect).
    Connected,
    /// One decoded event of the session.
    Envelope(EventEnvelope),
    /// The server rejected the cursor: the consumer must reset what it built from earlier
    /// events, since the next connection replays the whole history.
    Resync,
    /// The stream ended for good (`session.end`, or a fatal answer).
    Disconnected,
    /// The connection was lost; another attempt follows after a wait.
    Reconnecting,
    /// A frame or an answer that could not be understood or accepted.
    ProtocolError(String),
}

/// First wait before a reconnect attempt.
const BACKOFF_START: Duration = Duration::from_millis(250);
/// Longest wait between reconnect attempts.
const BACKOFF_MAX: Duration = Duration::from_secs(5);
/// Events buffered between the connection task and the consumer.
const CHANNEL_CAPACITY: usize = 256;
/// The envelope kind after which the server closes the stream for good.
const SESSION_END_KIND: &str = "session.end";
/// The SSE event name of the cursor-rejected control frame.
const RESYNC_EVENT: &str = "resync";
/// The event name a frame without an `event:` field (or with an empty one) has, per the SSE spec.
const DEFAULT_EVENT: &str = "message";

/// Most bytes received for one SSE block not yet ended: everything since the last blank line
/// (fields, comments, line endings), across as many chunks as the block spans. `sse-stream`
/// bounds only the scratch space it keeps between blocks, not the size of one block.
///
/// Why a bound: a block only ends at a blank line. A body that never sends one (a broken proxy
/// answering with a file, one endless `data:` line) would otherwise grow the client's memory
/// without limit; timeouts do not catch it, since bytes keep arriving. Past the bound the
/// subscription ends with a [`StreamEvent::ProtocolError`] and nothing of that block is delivered.
///
/// Why 4 MiB, the same value as the provider's chat-stream cap: it is the client's policy limit, not
/// a size the server guarantees. The largest envelopes carry a whole assistant answer or a tool
/// call's arguments, bounded only by the model's output limit (a few hundred KiB at most today);
/// tool output in events is capped at 64 KiB and images travel as references. Every blank line
/// resets the count, including the one closing a comment-only keep-alive block, so a long-idle
/// stream never accumulates toward the bound.
pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Streams a session's events as [`StreamEvent`]s.
///
/// `cursor` is the last applied seq (`Some(0)` is a real cursor, `None` asks
/// for the full history). The stream reconnects on its own after a lost
/// connection, resuming after the highest seq it delivered, and ends after
/// `session.end`, after a fatal answer (401, 404, ...), after a frame larger
/// than [`MAX_FRAME_BYTES`] or on `cancel`.
pub fn subscribe(
    client: Client,
    session_id: String,
    cursor: Option<u64>,
    cancel: CancellationToken,
) -> EventStream {
    let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
    let stop = cancel.child_token();
    let guard = stop.clone().drop_guard();
    tokio::spawn(run(Connection {
        client,
        session_id,
        cursor,
        stop,
        tx,
    }));
    Box::pin(futures_util::stream::unfold(
        (rx, guard),
        |(mut rx, guard)| async move { rx.recv().await.map(|event| (event, (rx, guard))) },
    ))
}

struct Connection {
    client: Client,
    session_id: String,
    cursor: Option<u64>,
    stop: CancellationToken,
    tx: mpsc::Sender<StreamEvent>,
}

/// How one connection ended.
enum Ended {
    /// `session.end` was delivered: the session is over.
    SessionEnd,
    /// A frame broke the stream's limits; reconnecting would receive it again.
    Refused,
    /// The connection closed or failed; `delivered` tells whether any frame
    /// arrived on it.
    Lost { delivered: bool },
    /// Cancelled, or the consumer went away.
    Stopped,
}

async fn run(mut conn: Connection) {
    let mut backoff = BACKOFF_START;
    loop {
        let opened = tokio::select! {
            _ = conn.stop.cancelled() => return,
            opened = conn.client.open_events(&conn.session_id, conn.cursor) => opened,
        };
        match opened {
            Ok(response) => {
                if !conn.emit(StreamEvent::Connected).await {
                    return;
                }
                match conn.read(response).await {
                    Ended::SessionEnd | Ended::Refused => {
                        conn.emit(StreamEvent::Disconnected).await;
                        return;
                    }
                    Ended::Stopped => return,
                    Ended::Lost { delivered } => {
                        if delivered {
                            backoff = BACKOFF_START;
                        }
                    }
                }
            }
            Err(OpenError::Fatal(message)) => {
                conn.protocol_error(format!("event stream refused: {message}"))
                    .await;
                conn.emit(StreamEvent::Disconnected).await;
                return;
            }
            Err(OpenError::Transient(_)) => {}
        }
        if !conn.emit(StreamEvent::Reconnecting).await {
            return;
        }
        tokio::select! {
            _ = conn.stop.cancelled() => return,
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

impl Connection {
    /// Sends `event` to the consumer; false when it is gone or cancelled.
    async fn emit(&self, event: StreamEvent) -> bool {
        tokio::select! {
            _ = self.stop.cancelled() => false,
            sent = self.tx.send(event) => sent.is_ok(),
        }
    }

    async fn protocol_error(&self, message: String) -> bool {
        self.emit(StreamEvent::ProtocolError(message)).await
    }

    /// Decodes frames from `response` until it ends, delivering each one.
    async fn read(&mut self, response: reqwest::Response) -> Ended {
        let mut frames = std::pin::pin!(frames(response.bytes_stream()));
        let mut delivered = false;
        loop {
            let frame = tokio::select! {
                _ = self.stop.cancelled() => return Ended::Stopped,
                frame = frames.next() => frame,
            };
            let frame = match frame {
                Some(Ok(frame)) => frame,
                Some(Err(e)) if is_frame_too_large(&e) => {
                    // The server would send the same frame again after a reconnect.
                    self.protocol_error(format!(
                        "event stream closed: a frame exceeds {MAX_FRAME_BYTES} bytes"
                    ))
                    .await;
                    return Ended::Refused;
                }
                _ => return Ended::Lost { delivered },
            };
            delivered = true;
            let (event, data) = frame;
            let keep_going = match event.as_str() {
                RESYNC_EVENT => {
                    // The server rejected the cursor; the consumer resets its
                    // projection, so the next connection replays everything.
                    self.cursor = None;
                    self.emit(StreamEvent::Resync).await
                }
                DEFAULT_EVENT => match serde_json::from_str::<EventEnvelope>(&data) {
                    Ok(envelope) => {
                        let ended = envelope.kind == SESSION_END_KIND;
                        self.cursor =
                            Some(self.cursor.map_or(envelope.seq, |c| c.max(envelope.seq)));
                        let sent = self.emit(StreamEvent::Envelope(envelope)).await;
                        if sent && ended {
                            return Ended::SessionEnd;
                        }
                        sent
                    }
                    Err(e) => self.protocol_error(format!("undecodable event: {e}")).await,
                },
                other => {
                    self.protocol_error(format!("unexpected event \"{other}\": {data}"))
                        .await
                }
            };
            if !keep_going {
                return Ended::Stopped;
            }
        }
    }
}

/// Why a body stopped before its end.
#[derive(Debug, thiserror::Error)]
enum BodyError {
    #[error("SSE frame exceeds {MAX_FRAME_BYTES} bytes")]
    FrameTooLarge,
    #[error("transport error: {0}")]
    Transport(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Counts the bytes of the SSE block in progress, ending lines the way `sse-stream` does: CR,
/// LF or CRLF (a CRLF split across chunks included), so a CRLF line ending never reads as a blank
/// line and never resets the count of a block still open. A line holding only spaces is not
/// blank.
///
/// A block's count runs through the CR or LF that closes it, the byte on which the parser
/// dispatches; the LF completing a closing CRLF is charged to neither block. A leading byte-order
/// mark, which the parser drops, is counted, as is the line ending of a first line holding only
/// that mark: the first block may count up to five bytes more than the parser holds.
struct FrameGuard {
    pending: usize,
    at_line_start: bool,
    after_cr: bool,
}

impl FrameGuard {
    fn new() -> Self {
        Self {
            pending: 0,
            at_line_start: true,
            after_cr: false,
        }
    }

    /// Counts `chunk`; false once the open block has exceeded [`MAX_FRAME_BYTES`]. The byte that
    /// closes a block is counted before the reset, so a block cannot pass by ending on time.
    fn admit(&mut self, chunk: &[u8]) -> bool {
        for &byte in chunk {
            let lf_of_crlf = byte == b'\n' && self.after_cr;
            self.after_cr = byte == b'\r';
            if lf_of_crlf && self.pending == 0 {
                // The rest of the CRLF that closed the previous block.
                continue;
            }
            self.pending += 1;
            if self.pending > MAX_FRAME_BYTES {
                return false;
            }
            match byte {
                _ if lf_of_crlf => {}
                b'\n' | b'\r' if self.at_line_start => self.pending = 0,
                b'\n' | b'\r' => self.at_line_start = true,
                _ => self.at_line_start = false,
            }
        }
        true
    }
}

/// The dispatched frames of `body` as (event name, data), with each block bounded by
/// [`MAX_FRAME_BYTES`]. The stream ends after its first error.
fn frames<S, E>(body: S) -> impl Stream<Item = Result<(String, String), sse_stream::Error>>
where
    S: Stream<Item = Result<Bytes, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    let mut guard = FrameGuard::new();
    let guarded = body.map(move |chunk| match chunk {
        Ok(bytes) if guard.admit(bytes.as_ref()) => Ok(bytes),
        Ok(_) => Err(BodyError::FrameTooLarge),
        Err(e) => Err(BodyError::Transport(Box::new(e))),
    });
    // Blocks with no or empty `data` (a lone `id:` or `retry:`) are not dispatched.
    SseByteStream::new(guarded)
        .filter_map(|f| futures_util::future::ready(f.map(frame_parts).transpose()))
}

/// Whether `error` is the frame cap of [`frames`] rather than a transport or decoding failure.
fn is_frame_too_large(error: &sse_stream::Error) -> bool {
    matches!(error, sse_stream::Error::Body(e)
        if matches!(e.downcast_ref::<BodyError>(), Some(BodyError::FrameTooLarge)))
}

/// The event name and data of a frame, or `None` when its data is missing or empty. The SSE spec
/// does dispatch a block whose `data:` line is empty; WithNav skips it on purpose, since the server
/// never sends one (envelopes carry JSON, `resync` a message). A missing or empty `event:` field
/// reads as `message`, so data frames and `resync` are told apart as the spec says.
fn frame_parts(frame: Sse) -> Option<(String, String)> {
    let data = frame.data.filter(|d| !d.is_empty())?;
    let event = frame.event.filter(|e| !e.is_empty());
    Some((event.unwrap_or_else(|| DEFAULT_EVENT.to_string()), data))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decodes `raw` the way `Connection::read` does, without a server.
    async fn decode(raw: &'static str) -> Vec<(String, String)> {
        let chunks = futures_util::stream::iter([Ok::<_, std::io::Error>(raw.as_bytes())]);
        SseByteStream::new(chunks)
            .filter_map(|f| async { f.ok().and_then(frame_parts) })
            .collect()
            .await
    }

    #[tokio::test]
    async fn decoder_distinguishes_resync_from_data_frames() {
        let frames = decode(
            ": keep-alive\n\nid: 2\n\nevent: resync\ndata: cursor out of range; reload\n\nid: 3\ndata: {}\n\n",
        )
        .await;
        assert_eq!(
            frames,
            vec![
                (
                    "resync".to_string(),
                    "cursor out of range; reload".to_string()
                ),
                ("message".to_string(), "{}".to_string()),
            ]
        );
    }

    /// What [`frames`] yields for `chunks`: the frames, then whether it ended on the frame cap.
    async fn decode_chunks(chunks: Vec<Vec<u8>>) -> (Vec<(String, String)>, bool) {
        let body = futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, std::io::Error>(Bytes::from(c))),
        );
        let mut out = Vec::new();
        let mut frames = std::pin::pin!(frames(body));
        while let Some(item) = frames.next().await {
            match item {
                Ok(frame) => out.push(frame),
                Err(e) => return (out, is_frame_too_large(&e)),
            }
        }
        (out, false)
    }

    fn split(bytes: &[u8], size: usize) -> Vec<Vec<u8>> {
        bytes.chunks(size).map(<[u8]>::to_vec).collect()
    }

    /// One `data:` block of exactly `len` bytes, its closing blank line included.
    fn block_of(len: usize) -> Vec<u8> {
        let mut block = b"data: ".to_vec();
        block.resize(len - 2, b'x');
        block.extend_from_slice(b"\n\n");
        block
    }

    fn message(data: &str) -> (String, String) {
        ("message".to_string(), data.to_string())
    }

    #[tokio::test]
    async fn oversized_frame_split_across_chunks_fails_without_being_delivered() {
        let mut body = b"data: a\n\n".to_vec();
        body.extend(block_of(MAX_FRAME_BYTES + 1));
        body.extend_from_slice(b"data: after\n\n");
        let (frames, too_large) = decode_chunks(split(&body, 64 * 1024 + 7)).await;
        assert_eq!(frames, vec![message("a")]);
        assert!(too_large);
    }

    #[tokio::test]
    async fn the_cap_stops_reading_the_body_and_withholds_the_whole_chunk() {
        // A complete frame sharing the overflowing chunk is withheld with it; the subscription
        // ends there, so the body is never polled again.
        let mut chunk = b"data: a\n\n".to_vec();
        chunk.extend(block_of(MAX_FRAME_BYTES + 1));
        let body = futures_util::stream::iter([Ok::<_, std::io::Error>(Bytes::from(chunk))]).chain(
            futures_util::stream::once(async { panic!("body polled after the frame cap") }),
        );
        let mut frames = std::pin::pin!(frames(body));
        assert!(matches!(frames.next().await, Some(Err(e)) if is_frame_too_large(&e)));
        assert!(frames.next().await.is_none());
    }

    #[tokio::test]
    async fn a_closing_crlf_charges_its_lf_to_neither_block() {
        // Through its closing CR, this CRLF block is exactly the cap; its last LF is not counted.
        let mut crlf = b"data: ".to_vec();
        crlf.resize(MAX_FRAME_BYTES - 3, b'x');
        crlf.extend_from_slice(b"\r\n\r\n");
        for chunks in [vec![crlf.clone()], split(&crlf, MAX_FRAME_BYTES)] {
            let (frames, too_large) = decode_chunks(chunks).await;
            assert_eq!((frames.len(), too_large), (1, false));
        }
        let mut over = crlf.clone();
        over.insert(6, b'x');
        let (frames, too_large) = decode_chunks(vec![over]).await;
        assert_eq!((frames.len(), too_large), (0, true));
        // A CRLF-closed block does not eat into the next block's allowance.
        let mut body = b"data: a\r\n\r\n".to_vec();
        body.extend(block_of(MAX_FRAME_BYTES));
        for chunks in [vec![body.clone()], split(&body, 12)] {
            let (frames, too_large) = decode_chunks(chunks).await;
            assert_eq!((frames.len(), too_large), (2, false));
        }
    }

    #[tokio::test]
    async fn a_leading_byte_order_mark_split_across_chunks_still_decodes() {
        let body = "\u{feff}data: a\n\n".as_bytes();
        let (frames, too_large) = decode_chunks(split(body, 1)).await;
        assert_eq!((frames, too_large), (vec![message("a")], false));
    }

    #[tokio::test]
    async fn a_block_at_the_cap_passes_and_one_byte_more_fails() {
        let (frames, too_large) = decode_chunks(vec![block_of(MAX_FRAME_BYTES)]).await;
        assert_eq!((frames.len(), too_large), (1, false));
        let (frames, too_large) = decode_chunks(vec![block_of(MAX_FRAME_BYTES + 1)]).await;
        assert_eq!((frames.len(), too_large), (0, true));
    }

    #[tokio::test]
    async fn an_unterminated_block_past_the_cap_fails() {
        let mut body = b"data: ".to_vec();
        body.resize(MAX_FRAME_BYTES + 1, b'x');
        let (frames, too_large) = decode_chunks(split(&body, 1000)).await;
        assert_eq!((frames.len(), too_large), (0, true));
    }

    #[tokio::test]
    async fn keep_alives_and_small_frames_beyond_the_cap_pass() {
        let mut body = b":\n\n".repeat(MAX_FRAME_BYTES / 3 + 1);
        body.extend_from_slice(b"data: end\n\n");
        let (frames, too_large) = decode_chunks(split(&body, 4093)).await;
        assert_eq!((frames, too_large), (vec![message("end")], false));

        let count = MAX_FRAME_BYTES / 11 + 1;
        let body = b"data: s\r\n\r\n".repeat(count);
        let (frames, too_large) = decode_chunks(split(&body, 4093)).await;
        assert_eq!((frames.len(), too_large), (count, false));
    }

    #[tokio::test]
    async fn crlf_line_endings_do_not_close_a_block() {
        // Many lines of one block, each ended by CRLF: one event to the parser, so one count.
        let mut body = b"data: x\r\n".repeat(MAX_FRAME_BYTES / 9 + 1);
        body.extend_from_slice(b"\r\n");
        let (frames, too_large) = decode_chunks(split(&body, 4093)).await;
        assert_eq!((frames.len(), too_large), (0, true));
    }

    #[test]
    fn frame_guard_ends_lines_like_the_parser() {
        let pending_after = |chunks: &[&[u8]]| {
            let mut guard = FrameGuard::new();
            assert!(chunks.iter().all(|c| guard.admit(c)));
            guard.pending
        };
        // A CRLF split across chunks is one line ending, then the next CR is the blank line.
        assert_eq!(pending_after(&[b"data: x\r", b"\n", b"\r"]), 0);
        assert_eq!(pending_after(&[b"data: x\r\n"]), 9);
        assert_eq!(pending_after(&[b"data: x\r\r"]), 0);
        assert_eq!(pending_after(&[b"data: x\n\r"]), 0);
        assert_eq!(pending_after(&[b"data: x\n", b"\n"]), 0);
        // A line of spaces is not blank.
        assert_eq!(pending_after(&[b"data: x\n \n"]), 10);
    }

    /// A session envelope frame for `seq`, whose payload text has `filler` bytes.
    fn envelope_frame(seq: u64, kind: &str, filler: usize) -> Vec<u8> {
        let envelope = serde_json::json!({
            "schema_version": 1, "event_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "seq": seq,
            "session_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV", "actor_id": "root", "kind": kind,
            "occurred_at": "2026-11-09T00:00:00Z", "causation_id": null, "correlation_id": null,
            "payload": {"text": "x".repeat(filler)},
        });
        format!("id: {seq}\ndata: {envelope}\n\n").into_bytes()
    }

    /// Serves `bodies[n]` to the n-th request of the event route (the last one again after
    /// that) and reports each request's `Last-Event-ID`; an `Err` chunk aborts the response.
    async fn serve_events(
        bodies: Vec<Vec<Result<Vec<u8>, std::io::Error>>>,
    ) -> (Client, mpsc::UnboundedReceiver<Option<String>>) {
        use axum::{Router, body::Body, http::HeaderMap, routing::get};
        use std::sync::{Arc, Mutex};
        let (seen, requests) = mpsc::unbounded_channel();
        let bodies = Arc::new(Mutex::new(bodies));
        let app = Router::new().route(
            "/v1/sessions/{id}/events",
            get(move |headers: HeaderMap| {
                let bodies = Arc::clone(&bodies);
                let cursor = headers
                    .get("last-event-id")
                    .map(|v| v.to_str().unwrap().to_owned());
                seen.send(cursor).unwrap();
                async move {
                    let mut bodies = bodies.lock().unwrap();
                    let chunks = if bodies.len() > 1 {
                        bodies.remove(0)
                    } else {
                        bodies[0]
                            .iter()
                            .map(|c| match c {
                                Ok(bytes) => Ok(bytes.clone()),
                                Err(e) => Err(std::io::Error::other(e.to_string())),
                            })
                            .collect()
                    };
                    // An error sent at once would abort the response before its head is
                    // written; the wait lets the frames before it reach the client first.
                    Body::from_stream(futures_util::stream::iter(chunks).then(|c| async {
                        if c.is_err() {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                        c
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (Client::new(url, None), requests)
    }

    /// The events of a subscription until it ends, at most `limit`, as short labels.
    async fn labels(client: Client, limit: usize) -> Vec<String> {
        let events = subscribe(client, "s".into(), None, CancellationToken::new());
        let events = events
            .take(limit)
            .map(|event| match event {
                StreamEvent::Envelope(e) => format!("envelope {}", e.seq),
                StreamEvent::ProtocolError(m) => format!("error: {m}"),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>();
        tokio::time::timeout(Duration::from_secs(20), events)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn an_oversized_envelope_ends_the_subscription_without_being_delivered() {
        let mut body = vec![Ok(envelope_frame(1, "turn.start", 0))];
        let huge = envelope_frame(2, "turn.end", MAX_FRAME_BYTES);
        body.extend(split(&huge, 64 * 1024).into_iter().map(Ok));
        let (client, mut requests) = serve_events(vec![body]).await;
        assert_eq!(
            labels(client, 8).await,
            vec![
                "Connected".to_string(),
                "envelope 1".into(),
                format!("error: event stream closed: a frame exceeds {MAX_FRAME_BYTES} bytes"),
                "Disconnected".into(),
            ]
        );
        assert_eq!(requests.recv().await, Some(None));
        assert!(requests.try_recv().is_err(), "no reconnect after the cap");
    }

    #[tokio::test]
    async fn a_transport_error_still_reconnects_after_the_last_delivered_seq() {
        let broken = vec![
            Ok(envelope_frame(1, "turn.start", 0)),
            Err(std::io::Error::other("connection reset")),
        ];
        let ended = vec![Ok(envelope_frame(2, SESSION_END_KIND, 0))];
        let (client, mut requests) = serve_events(vec![broken, ended]).await;
        assert_eq!(
            labels(client, 8).await,
            [
                "Connected",
                "envelope 1",
                "Reconnecting",
                "Connected",
                "envelope 2",
                "Disconnected"
            ]
        );
        assert_eq!(requests.recv().await, Some(None));
        assert_eq!(requests.recv().await, Some(Some("1".to_string())));
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let mut wait = BACKOFF_START;
        let mut seen = vec![wait];
        for _ in 0..6 {
            wait = (wait * 2).min(BACKOFF_MAX);
            seen.push(wait);
        }
        assert_eq!(seen.last(), Some(&BACKOFF_MAX));
        assert!(seen.windows(2).all(|w| w[0] <= w[1]));
    }
}
