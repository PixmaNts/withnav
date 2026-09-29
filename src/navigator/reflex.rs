//! The reflex layer's service: turns an inline-completion request (document
//! text, version, UTF-16 cursor) into a suggestion, with the reflex context
//! rendered as a comment block, prefix/suffix capping, a 150 ms debounce and
//! per-document cancellation of the superseded request.
//!
//! Pure text work (UTF-16 → byte conversion, capping, context block) is a
//! plain function so tests cover it without any model; [`ReflexService`]
//! adds the stateful parts (debounce, version staleness, cancellation) over
//! an injected [`FimModel`] seam.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::navigator::ports::{FimModel, FimRequest};
use crate::navigator::protocol::{InlineCompletionItem, InlineCompletionResult, Position, Range};

/// Debounce after the last keystroke before a suggestion is asked for.
pub const DEBOUNCE_MS: u64 = 150;

/// Cap on the text before the cursor sent as the FIM prompt.
pub const MAX_PREFIX_BYTES: usize = 16 * 1024;
/// Cap on the text after the cursor sent as the FIM suffix.
pub const MAX_SUFFIX_BYTES: usize = 8 * 1024;
/// Cap on the rendered reflex context block, ~300 tokens (4 chars/token).
pub const MAX_CONTEXT_BYTES: usize = 1200;
/// Token budget of one suggestion.
pub const MAX_SUGGESTION_TOKENS: u64 = 128;

/// A suggestion the service produced, with what the caller needs to log and
/// to answer the inline-completion request.
#[derive(Debug, Clone)]
pub struct Suggestion {
    pub item: InlineCompletionItem,
    /// The id echoed back by the extension in `navigator/reflexShown`.
    pub request_id: String,
}

/// Language-aware line comment prefix for the reflex context block.
fn line_comment_prefix(language_id: &str) -> &'static str {
    match language_id {
        "rust" | "swift" | "kotlin" | "go" | "java" | "scala" | "c" | "cpp" | "javascript"
        | "typescript" | "json" | "css" | "scss" | "php" | "zig" => "//",
        "python" | "ruby" | "shellscript" | "bash" | "yaml" | "toml" | "perl" | "r" | "elixir" => "#",
        "lua" | "sql" => "--",
        "erlang" => "%",
        "html" | "xml" | "markdown" | "plaintext" => "<!--",
        _ => "//",
    }
}

/// Caps a text from the left (what follows the cursor: the FIM suffix).
/// Cuts on a char boundary only: a partial line is fine — the cursor
/// itself is usually mid-line.
fn cap_head(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut cut = max_bytes;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text[..cut].to_string()
}

/// Caps a text from the right (what precedes the cursor: the FIM prompt).
/// Cuts on a char boundary only, keeping the lines nearest the cursor.
fn cap_tail(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut start = text.len() - max_bytes;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}

/// Caps the rendered context block: char-boundary cut, then back off to
/// the last complete comment line so the block always ends with a newline
/// (the code prefix must start on its own line).
fn cap_context_block(block: &str, max_bytes: usize) -> String {
    if block.len() <= max_bytes {
        return block.to_string();
    }
    let mut cut = max_bytes;
    while cut > 0 && !block.is_char_boundary(cut) {
        cut -= 1;
    }
    match block[..cut].rfind('\n') {
        Some(nl) => block[..=nl].to_string(),
        None => String::new(),
    }
}

/// The reflex context block: the driver's intent and nearby signatures,
/// rendered as comments in the file's language and capped at ~300 tokens.
/// Returns an empty string when there is no context.
pub fn render_context_block(reflex_context: &str, language_id: &str) -> String {
    let trimmed = reflex_context.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let prefix = line_comment_prefix(language_id);
    let suffix = if prefix == "<!--" { " -->" } else { "" };
    let header = " navigator context (intent + nearby signatures):";
    let mut block = format!("{prefix}{header}\n");
    for line in trimmed.lines() {
        let rendered = if prefix == "<!--" {
            format!("{prefix} {line}{suffix}")
        } else {
            format!("{prefix} {line}")
        };
        block.push_str(&rendered);
        block.push('\n');
    }
    cap_context_block(&block, MAX_CONTEXT_BYTES)
}

/// Converts an LSP position (line, UTF-16 character) to a byte offset in
/// the document text. Returns `None` when the position is out of bounds.
pub fn utf16_position_to_offset(text: &str, line: u32, character: u32) -> Option<usize> {
    for (i, l) in text.lines().enumerate() {
        if (i as u32) == line {
            let in_line = char_utf16_to_byte_offset(l, character as usize)?;
            // Byte offset of the line start: sum of the preceding lines
            // plus their newlines.
            let line_start: usize = text
                .lines()
                .take(i)
                .map(|prev| prev.len() + 1)
                .sum();
            return Some(line_start + in_line);
        }
    }
    None
}

/// Converts a UTF-16 code-unit offset within one line to a byte offset.
fn char_utf16_to_byte_offset(line: &str, units: usize) -> Option<usize> {
    let mut seen = 0usize;
    for (byte_idx, ch) in line.char_indices() {
        if seen >= units {
            return Some(byte_idx);
        }
        seen += ch.len_utf16();
    }
    if seen == units {
        Some(line.len())
    } else {
        None
    }
}

/// Builds the FIM request for a document position: capped prefix and
/// suffix, optionally preceded by the rendered reflex context block.
pub fn build_fim_request(
    doc_text: &str,
    offset: usize,
    reflex_context: &str,
    language_id: &str,
) -> FimRequest {
    // The prefix keeps its TAIL (the lines nearest the cursor); the suffix
    // keeps its HEAD (the lines just after it).
    let prefix = cap_tail(&doc_text[..offset], MAX_PREFIX_BYTES);
    let suffix = cap_head(&doc_text[offset..], MAX_SUFFIX_BYTES);
    let context = render_context_block(reflex_context, language_id);
    let prompt = if context.is_empty() {
        prefix
    } else {
        // The context block is comments: it must sit above the prefix's
        // code, never between the last (usually partial) prefix line and
        // the cursor, or it would break the syntax being completed.
        format!("{context}{prefix}")
    };
    FimRequest {
        prompt,
        suffix,
        max_tokens: MAX_SUGGESTION_TOKENS,
    }
}

/// Computes the end position of a suggestion: the cursor plus the text
/// inserted, advancing lines and UTF-16 characters as the text dictates.
fn end_position(start: Position, text: &str) -> Position {
    let mut line = start.line;
    let mut character = start.character;
    for ch in text.chars() {
        if ch == '\n' {
            line += 1;
            character = 0;
        } else {
            character += ch.len_utf16() as u32;
        }
    }
    Position { line, character }
}

/// Everything the caller must know about a suggestion attempt that did not
/// produce one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuggestOutcome {
    /// The debounced position was superseded before the model was called.
    Debounced,
    /// The document version moved while the model was answering.
    StaleVersion,
    /// The model returned nothing insertable (error or empty text).
    ModelSilent,
}

/// The reflex service. Holds one in-flight request per document; a newer
/// request for the same document cancels the older one.
pub struct ReflexService {
    model: Arc<dyn FimModel>,
    per_document: Mutex<std::collections::HashMap<String, InFlight>>,
}

#[derive(Default)]
struct InFlight {
    /// Debounce timer target: (version, position) of the latest request
    /// that has not been sent yet.
    pending: Option<(i32, Position)>,
    cancel: Option<CancellationToken>,
}

impl ReflexService {
    pub fn new(model: Arc<dyn FimModel>) -> Self {
        Self {
            model,
            per_document: Mutex::new(Default::default()),
        }
    }

    /// Requests a suggestion at the cursor. `doc_text`/`version` come from
    /// the full-sync document state; `position` is the LSP position
    /// (UTF-16 code units). The returned future resolves after the
    /// debounce with either a suggestion or why there is none.
    ///
    /// Cancellation contract: the caller drops this future only on
    /// document close; a newer call on the same uri cancels the older
    /// call's model request.
    pub async fn suggest(
        &self,
        uri: &str,
        doc_text: &str,
        version: i32,
        position: Position,
        reflex_context: &str,
        language_id: &str,
    ) -> Result<Suggestion, SuggestOutcome> {
        let line = position.line;
        let character = position.character;
        let position = Position { line, character };
        // Register as the latest request for this document; whichever
        // request is superseded sees its token cancelled.
        let cancel = {
            let mut per_doc = self.per_document.lock().await;
            let entry = per_doc.entry(uri.to_string()).or_default();
            if let Some(old) = entry.cancel.take() {
                old.cancel();
            }
            let token = CancellationToken::new();
            entry.cancel = Some(token.clone());
            entry.pending = Some((version, position));
            token
        };
        // Debounce: the last keystroke wins. If a newer request arrived
        // while we slept, our token is cancelled and we stand down without
        // calling the model.
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(SuggestOutcome::Debounced),
            _ = tokio::time::sleep(Duration::from_millis(DEBOUNCE_MS)) => {}
        }

        let Some(offset) = utf16_position_to_offset(doc_text, line, character) else {
            return Err(SuggestOutcome::ModelSilent);
        };
        let request = build_fim_request(doc_text, offset, reflex_context, language_id);
        let answer = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(SuggestOutcome::Debounced),
            a = self.model.complete(request, &cancel) => a,
        };
        let answer = match answer {
            Ok(a) if !a.text.is_empty() => a,
            _ => return Err(SuggestOutcome::ModelSilent),
        };
        // The document may have moved while the model answered (a newer
        // request would have cancelled us; a version bump without a new
        // inline-completion request can only be detected by the caller's
        // version check on the answer — this service checks its own
        // registration).
        {
            let per_doc = self.per_document.lock().await;
            if let Some(entry) = per_doc.get(uri) {
                let is_latest = entry
                    .cancel
                    .as_ref()
                    .is_some_and(|t| !t.is_cancelled());
                let same_version = entry
                    .pending
                    .map(|(v, _)| v == version)
                    .unwrap_or(false);
                if !is_latest || !same_version {
                    return Err(SuggestOutcome::StaleVersion);
                }
            }
        }
        let request_id = crate::navigator::new_ulid_string();
        let text = answer.text;
        let end = end_position(position, &text);
        Ok(Suggestion {
            item: InlineCompletionItem {
                text,
                range: Range {
                    start: position,
                    end,
                },
            },
            request_id,
        })
    }
}

/// Wraps a [`Suggestion`] into the wire answer of
/// `navigator/inlineCompletion`.
pub fn to_result(suggestion: &Suggestion) -> InlineCompletionResult {
    InlineCompletionResult {
        items: vec![suggestion.item.clone()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_offset_lands_after_a_non_ascii_char() {
        // "héllo": é is one char, two UTF-16 units. Position 3 must be the
        // byte offset of the first 'l' (bytes: h=1, é=2), not the byte count.
        let text = "héllo";
        let off = utf16_position_to_offset(text, 0, 3).unwrap();
        assert_eq!(&text[..off], "hél");
    }

    #[test]
    fn utf16_offset_counts_emoji_as_two_units() {
        // 😀 is one char, two UTF-16 units.
        let text = "a😀b";
        assert_eq!(utf16_position_to_offset(text, 0, 1).unwrap(), 1);
        assert_eq!(utf16_position_to_offset(text, 0, 3).unwrap(), 5);
    }

    #[test]
    fn utf16_offset_multi_line() {
        let text = "ab\n😀x\nz";
        // Line 1 starts at byte 3; the emoji is 4 bytes and two UTF-16
        // units, so position (1,3) is just after 'x' (byte 3+4+1=8).
        assert_eq!(utf16_position_to_offset(text, 1, 3).unwrap(), 8);
        // Line 2 starts at byte 9 (3 + "😀x\n" is 6 bytes).
        assert_eq!(utf16_position_to_offset(text, 2, 0).unwrap(), 9);
        // End of the last line is the end of the text.
        assert_eq!(utf16_position_to_offset(text, 2, 1).unwrap(), text.len());
    }

    #[test]
    fn utf16_offset_out_of_bounds() {
        assert!(utf16_position_to_offset("abc", 0, 9).is_none());
        assert!(utf16_position_to_offset("abc", 7, 0).is_none());
        // Position on a non-existent line is out of bounds.
        assert!(utf16_position_to_offset("a\nb", 5, 0).is_none());
    }

    #[test]
    fn cap_head_cuts_on_char_boundary() {
        assert_eq!(cap_head("ab\ncd", 100), "ab\ncd");
        // 4 bytes max on "héllo": "hél" is 4 bytes (h + 2-byte é + l) on a
        // char boundary — partial lines are fine, the cursor is mid-line.
        assert_eq!(cap_head("héllo", 4), "hél");
        // A multi-byte char straddling the cut backs off to its boundary.
        assert_eq!(cap_head("héllo", 2), "h");
    }

    #[test]
    fn cap_tail_keeps_the_text_nearest_the_cursor() {
        assert_eq!(cap_tail("ab\ncd", 100), "ab\ncd");
        // A long prefix is capped from the left: the bytes nearest the
        // cursor survive (a leading partial line or newline is harmless).
        let text = "aaaa\nbbbb\ncccc";
        assert_eq!(cap_tail(text, 10), "\nbbbb\ncccc");
        assert_eq!(cap_tail(text, 5), "\ncccc");
        // Char-boundary safety with a multi-byte char at the cut.
        assert_eq!(cap_tail("xéy\nz", 1), "z");
        assert_eq!(cap_tail("xéy\nz", 2), "\nz");
    }

    #[test]
    fn context_block_renders_language_aware_comments() {
        let block = render_context_block("intent: fix parser\nsig: parse(&str)", "rust");
        assert!(block.starts_with("// navigator context"));
        assert!(block.contains("// intent: fix parser"));
        let py = render_context_block("intent", "python");
        assert!(py.starts_with("# "));
        let md = render_context_block("intent", "markdown");
        assert!(md.contains("<!-- intent -->"));
    }

    #[test]
    fn context_block_is_capped() {
        let long = "x".repeat(10_000);
        let block = render_context_block(&long, "rust");
        assert!(block.len() <= MAX_CONTEXT_BYTES);
        // The cap keeps whole comment lines: the block ends with a newline
        // so the code prefix starts on its own line.
        assert!(block.ends_with('\n'));
        assert!(block.lines().all(|l| l.starts_with("//")));
    }

    #[test]
    fn empty_context_block_is_empty() {
        assert_eq!(render_context_block("   \n  ", "rust"), "");
    }

    #[test]
    fn fim_request_prefix_suffix_and_context() {
        let text = "line1\nline2\nline3";
        // Offset 11 is one char before the end of "line2".
        let req = build_fim_request(text, 11, "", "rust");
        assert_eq!(req.prompt, "line1\nline2");
        assert_eq!(req.suffix, "\nline3");
        assert_eq!(req.max_tokens, MAX_SUGGESTION_TOKENS);

        let req_ctx = build_fim_request(text, 11, "do it", "rust");
        assert!(req_ctx.prompt.starts_with("// navigator context"));
        // The whole capped prefix survives below the context block.
        assert!(req_ctx.prompt.ends_with("line1\nline2"));
    }

    #[test]
    fn end_position_advances_lines_and_utf16_units() {
        let start = Position { line: 2, character: 4 };
        // "ab\n😀x": after the newline the character resets; 😀 counts two
        // UTF-16 units, x one: final character = 3.
        let end = end_position(start, "ab\n😀x");
        assert_eq!(end, Position { line: 3, character: 3 });
    }
}
