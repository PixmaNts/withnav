use bytes::Bytes;
use futures_util::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::error::SseError;

pub const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

pub struct SseDecoder {
    byte_buf: Vec<u8>,
    data_lines: Vec<String>,
    data_bytes: usize,
    saw_data: bool,
    max_frame: usize,
}

impl Default for SseDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::with_max_frame(MAX_FRAME_BYTES)
    }

    pub fn with_max_frame(max_frame: usize) -> Self {
        Self {
            byte_buf: Vec::new(),
            data_lines: Vec::new(),
            data_bytes: 0,
            saw_data: false,
            max_frame,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<String>, SseError> {
        self.byte_buf.extend_from_slice(chunk);
        let mut events = Vec::new();

        let mut byte_buf = std::mem::take(&mut self.byte_buf);
        let mut consumed = 0;
        let result = (|| {
            while let Some((line_end, next_start)) = find_line_terminator(&byte_buf[consumed..]) {
                let raw_line = &byte_buf[consumed..consumed + line_end];
                consumed += next_start;
                let line = std::str::from_utf8(raw_line).map_err(|_| SseError::Utf8)?;

                if line.is_empty() {
                    if let Some(event) = self.take_event() {
                        events.push(event);
                    }
                } else {
                    self.consume_field_line(line)?;
                }
            }
            Ok(())
        })();
        byte_buf.drain(..consumed);
        self.byte_buf = byte_buf;
        result?;

        if self.byte_buf.len() > self.max_frame {
            return Err(SseError::MalformedFrame(format!(
                "unterminated frame exceeds max size {} bytes",
                self.max_frame
            )));
        }
        Ok(events)
    }

    pub fn finish(&mut self) -> Result<Option<String>, SseError> {
        if !self.byte_buf.is_empty() {
            let raw = std::mem::take(&mut self.byte_buf);
            let line = std::str::from_utf8(&raw)
                .map_err(|_| SseError::Utf8)?
                .to_string();
            if !line.is_empty() {
                self.consume_field_line(&line)?;
            }
        }
        Ok(self.take_event())
    }

    fn consume_field_line(&mut self, line: &str) -> Result<(), SseError> {
        if line.starts_with(':') {
            return Ok(());
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        if field == "data" {
            if value.len() > self.max_frame {
                return Err(SseError::MalformedFrame(format!(
                    "data line exceeds max size {} bytes",
                    self.max_frame
                )));
            }
            let separator_bytes = usize::from(self.saw_data);
            let data_bytes = self
                .data_bytes
                .saturating_add(separator_bytes)
                .saturating_add(value.len());
            if data_bytes > self.max_frame {
                return Err(SseError::MalformedFrame(format!(
                    "event data exceeds max size {} bytes",
                    self.max_frame
                )));
            }
            self.data_lines.push(value.to_string());
            self.data_bytes = data_bytes;
            self.saw_data = true;
        }
        Ok(())
    }

    fn take_event(&mut self) -> Option<String> {
        self.data_bytes = 0;
        if !self.saw_data {
            self.data_lines.clear();
            return None;
        }
        let event = self.data_lines.join("\n");
        self.data_lines.clear();
        self.saw_data = false;
        Some(event)
    }
}

fn find_line_terminator(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i < buf.len() {
        match buf[i] {
            b'\n' => return Some((i, i + 1)),
            b'\r' => {
                if i + 1 < buf.len() {
                    if buf[i + 1] == b'\n' {
                        return Some((i, i + 2));
                    }
                    return Some((i, i + 1));
                }
                return None;
            }
            _ => i += 1,
        }
    }
    None
}

pub struct SseEventStream<S> {
    inner: S,
    decoder: SseDecoder,
    pending: std::collections::VecDeque<String>,
    finished: bool,
}

impl<S, E> SseEventStream<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            decoder: SseDecoder::new(),
            pending: std::collections::VecDeque::new(),
            finished: false,
        }
    }
}

impl<S, E> Stream for SseEventStream<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    type Item = Result<String, SseError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            if let Some(ev) = this.pending.pop_front() {
                return Poll::Ready(Some(Ok(ev)));
            }
            if this.finished {
                return Poll::Ready(None);
            }
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => match this.decoder.push(&bytes) {
                    Ok(events) => this.pending.extend(events),
                    Err(e) => return Poll::Ready(Some(Err(e))),
                },
                Poll::Ready(Some(Err(_))) => {
                    return Poll::Ready(Some(Err(SseError::MalformedFrame(
                        "transport error in SSE stream".to_string(),
                    ))));
                }
                Poll::Ready(None) => {
                    this.finished = true;
                    match this.decoder.finish() {
                        Ok(Some(ev)) => return Poll::Ready(Some(Ok(ev))),
                        Ok(None) => return Poll::Ready(None),
                        Err(e) => return Poll::Ready(Some(Err(e))),
                    }
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(chunks: &[&[u8]]) -> Vec<String> {
        let mut dec = SseDecoder::new();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(dec.push(c).unwrap());
        }
        if let Some(ev) = dec.finish().unwrap() {
            out.push(ev);
        }
        out
    }

    #[test]
    fn split_frame_across_chunks() {
        let out = decode_all(&[b"data: hel", b"lo\n\n"]);
        assert_eq!(out, vec!["hello".to_string()]);
    }

    #[test]
    fn crlf_boundaries() {
        let out = decode_all(&[b"data: a\r\ndata: b\r\n\r\n"]);
        assert_eq!(out, vec!["a\nb".to_string()]);
    }

    #[test]
    fn crlf_crlf_event_delimiter() {
        let out = decode_all(&[b"data: one\r\n\r\ndata: two\r\n\r\n"]);
        assert_eq!(out, vec!["one".to_string(), "two".to_string()]);
    }

    #[test]
    fn lone_cr() {
        let out = decode_all(&[b"data: a\rdata: b\r\r"]);
        assert_eq!(out, vec!["a\nb".to_string()]);
    }

    #[test]
    fn multi_data_lines_joined_with_lf() {
        let out = decode_all(&[b"data: line1\ndata: line2\ndata: line3\n\n"]);
        assert_eq!(out, vec!["line1\nline2\nline3".to_string()]);
    }

    #[test]
    fn comment_ignored() {
        let out = decode_all(&[b": keep-alive\ndata: real\n\n"]);
        assert_eq!(out, vec!["real".to_string()]);
    }

    #[test]
    fn event_and_id_fields_ignored() {
        let out = decode_all(&[b"event: message\nid: 42\ndata: payload\n\n"]);
        assert_eq!(out, vec!["payload".to_string()]);
    }

    #[test]
    fn utf8_split_across_chunks() {
        let s = "café — ok";
        let bytes = s.as_bytes();
        let mid = 5;
        let mut dec = SseDecoder::new();
        let mut out = Vec::new();
        out.extend(dec.push(b"data: ").unwrap());
        out.extend(dec.push(&bytes[..mid]).unwrap());
        out.extend(dec.push(&bytes[mid..]).unwrap());
        out.extend(dec.push(b"\n\n").unwrap());
        assert_eq!(out, vec![s.to_string()]);
    }

    #[test]
    fn eof_flush_with_partial_unterminated_event() {
        let out = decode_all(&[b"data: trailing-no-blank"]);
        assert_eq!(out, vec!["trailing-no-blank".to_string()]);
    }

    #[test]
    fn eof_clean_no_partial() {
        let out = decode_all(&[b"data: done\n\n"]);
        assert_eq!(out, vec!["done".to_string()]);
        let out2 = decode_all(&[b"data: x\n\n"]);
        assert_eq!(out2.len(), 1);
    }

    #[test]
    fn empty_stream() {
        let out = decode_all(&[]);
        assert!(out.is_empty());
        let out2 = decode_all(&[b""]);
        assert!(out2.is_empty());
    }

    #[tokio::test]
    async fn transport_error_surfaced() {
        use futures_util::StreamExt;
        let items: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(b"data: a\n\n")),
            Err(std::io::Error::other("boom")),
        ];
        let src = futures_util::stream::iter(items);
        let mut ev = SseEventStream::new(src);
        let first = ev.next().await.unwrap();
        assert_eq!(first.unwrap(), "a");
        let second = ev.next().await.unwrap();
        assert!(matches!(second, Err(SseError::MalformedFrame(_))));
    }

    #[test]
    fn oversized_frame_rejected() {
        let mut dec = SseDecoder::with_max_frame(64);
        let big = vec![b'x'; 200];
        let mut input = b"data: ".to_vec();
        input.extend_from_slice(&big);
        let err = dec.push(&input).unwrap_err();
        assert!(matches!(err, SseError::MalformedFrame(_)));

        for chunks in [
            vec![b"data: ab\ndata: cd\n".as_slice()],
            vec![b"data: ab\n".as_slice(), b"data: cd\n".as_slice()],
            vec![b"data:\ndata:\ndata:\ndata:\ndata:\ndata:\n".as_slice()],
        ] {
            let mut dec = SseDecoder::with_max_frame(4);
            let err = chunks
                .into_iter()
                .find_map(|chunk| dec.push(chunk).err())
                .expect("combined data including separators must be bounded");
            assert!(matches!(err, SseError::MalformedFrame(_)));
        }

        let mut dec = SseDecoder::with_max_frame(5);
        assert_eq!(dec.push(b"data: ab\ndata: cd\n\n").unwrap(), ["ab\ncd"]);
        assert_eq!(dec.push(b"data: ef\ndata: gh\n\n").unwrap(), ["ef\ngh"]);
    }

    proptest::proptest! {
        #[test]
        fn prop_arbitrary_chunk_boundaries(split_at in 0usize..64) {
            let corpus: &[u8] =
                b": comment\ndata: alpha\ndata: beta\n\nevent: x\ndata: gamma\r\n\r\ndata: delta\n\n";

            let whole = decode_all(&[corpus]);

            let cut = split_at.min(corpus.len());
            let (a, b) = corpus.split_at(cut);
            let split = decode_all(&[a, b]);

            proptest::prop_assert_eq!(whole, split);
        }
    }
}
