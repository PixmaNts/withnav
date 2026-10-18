//! SSE decoding, reconnect and cancellation; no UI state mutation.
//!
//! One subscription is one background task that owns the HTTP connection and
//! feeds a bounded channel; the returned stream reads that channel. Dropping
//! the stream or cancelling the token stops the task, including while it waits
//! on the network or sleeps between attempts.
use crate::client::{Client, OpenError};
use eventsource_stream::Eventsource;
use futures_util::{Stream, StreamExt};
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

/// Streams a session's events as [`StreamEvent`]s.
///
/// `cursor` is the last applied seq (`Some(0)` is a real cursor, `None` asks
/// for the full history). The stream reconnects on its own after a lost
/// connection, resuming after the highest seq it delivered, and ends after
/// `session.end`, after a fatal answer (401, 404, ...) or on `cancel`.
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
                    Ended::SessionEnd => {
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
        let mut frames = response.bytes_stream().eventsource();
        let mut delivered = false;
        loop {
            let frame = tokio::select! {
                _ = self.stop.cancelled() => return Ended::Stopped,
                frame = frames.next() => frame,
            };
            let Some(Ok(frame)) = frame else {
                return Ended::Lost { delivered };
            };
            delivered = true;
            let keep_going = match frame.event.as_str() {
                RESYNC_EVENT => {
                    // The server rejected the cursor; the consumer resets its
                    // projection, so the next connection replays everything.
                    self.cursor = None;
                    self.emit(StreamEvent::Resync).await
                }
                "message" => match serde_json::from_str::<EventEnvelope>(&frame.data) {
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
                    self.protocol_error(format!("unexpected event \"{other}\": {}", frame.data))
                        .await
                }
            };
            if !keep_going {
                return Ended::Stopped;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decodes `raw` the way `Connection::read` does, without a server.
    async fn decode(raw: &'static str) -> Vec<(String, String)> {
        let chunks = futures_util::stream::iter([Ok::<_, std::io::Error>(raw.as_bytes().to_vec())]);
        chunks
            .eventsource()
            .filter_map(|f| async { f.ok() })
            .map(|f| (f.event, f.data))
            .collect()
            .await
    }

    #[tokio::test]
    async fn decoder_distinguishes_resync_from_data_frames() {
        let frames = decode(
            ": keep-alive\n\nevent: resync\ndata: cursor out of range; reload\n\nid: 3\ndata: {}\n\n",
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
