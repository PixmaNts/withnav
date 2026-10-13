//! Redact streaming bytes before ring eviction, retaining incomplete secret prefixes.
use std::collections::VecDeque;

pub(super) fn redact(text: &str, secrets: &[String]) -> String {
    let mut text = text.to_owned();
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        text = text.replace(secret, "[redacted]");
    }
    text
}

#[derive(Default)]
pub(super) struct StderrTail {
    pending: Vec<u8>,
    bytes: VecDeque<u8>,
}

impl StderrTail {
    pub fn push(&mut self, bytes: &[u8], secrets: &[String], eof: bool) {
        self.pending.extend_from_slice(bytes);
        let mut offset = 0;
        let mut safe = vec![];
        while offset < self.pending.len() {
            let rest = &self.pending[offset..];
            if !eof
                && secrets
                    .iter()
                    .any(|s| rest.len() < s.len() && s.as_bytes().starts_with(rest))
            {
                break;
            } else if let Some(secret) = secrets
                .iter()
                .find(|s| !s.is_empty() && rest.starts_with(s.as_bytes()))
            {
                safe.extend_from_slice(b"[redacted]");
                offset += secret.len();
            } else if !eof
                && secrets
                    .iter()
                    .any(|s| !s.is_empty() && s.as_bytes().starts_with(rest))
            {
                break;
            } else if eof
                && secrets
                    .iter()
                    .any(|s| !s.is_empty() && s.as_bytes().starts_with(rest))
            {
                safe.extend_from_slice(b"[redacted]");
                offset = self.pending.len();
            } else {
                safe.push(rest[0]);
                offset += 1;
            }
        }
        self.pending.drain(..offset);
        self.bytes.extend(safe);
        while self.bytes.len() > 64 * 1024 {
            self.bytes.pop_front();
        }
        let mut lines = self.bytes.iter().filter(|b| **b == b'\n').count();
        while lines > 200 {
            if self.bytes.pop_front() == Some(b'\n') {
                lines -= 1;
            }
        }
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes.iter().copied().collect::<Vec<_>>()).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn secrets_split_at_read_and_eviction_boundaries_never_enter_tail() {
        let secrets = vec!["abcdefgh".into()];
        let mut tail = StderrTail::default();
        tail.push(&vec![b'x'; 65534], &secrets, false);
        tail.push(b"abc", &secrets, false);
        assert!(!tail.text().ends_with("abc"));
        tail.push(b"defgh retained", &secrets, false);
        assert!(tail.text().ends_with("[redacted] retained"));
        assert!(!tail.text().contains("abcdefgh"));
        tail.push(b"abc", &secrets, true);
        assert!(tail.text().ends_with("[redacted]"));
    }
}
