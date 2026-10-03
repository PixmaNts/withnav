//! Text measured and cut in terminal columns: a CJK character takes two, a combining mark none.
use ratatui::text::Span;

/// The columns `text` takes on screen.
pub fn columns(text: &str) -> usize {
    Span::raw(text).width()
}

/// `text` cut at its end to at most `width` columns, ending with `…` when cut.
pub fn fit_end(text: &str, width: usize) -> String {
    if columns(text) <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = columns(c.encode_utf8(&mut [0; 4]));
        if used + w + 1 > width {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

/// `text` cut to `width` columns, then padded with spaces to exactly `width`.
pub fn pad(text: &str, width: usize) -> String {
    let cut = fit_end(text, width);
    let gap = width.saturating_sub(columns(&cut));
    format!("{cut}{}", " ".repeat(gap))
}

/// `text` cut at its start to at most `width` columns, starting with `…` when cut: the end of a
/// path (the file name) stays.
pub fn fit_start(text: &str, width: usize) -> String {
    if columns(text) <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut kept = Vec::new();
    let mut used = 0;
    for c in text.chars().rev() {
        let w = columns(c.encode_utf8(&mut [0; 4]));
        if used + w + 1 > width {
            break;
        }
        used += w;
        kept.push(c);
    }
    // A combining mark kept without its base character would land on the `…`.
    while kept
        .last()
        .is_some_and(|c| columns(c.encode_utf8(&mut [0; 4])) == 0)
    {
        kept.pop();
    }
    std::iter::once('…').chain(kept.into_iter().rev()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cjk_and_combining_marks_are_measured_in_columns() {
        assert_eq!(columns("日本語"), 6);
        assert_eq!(columns("e\u{301}te\u{301}"), 3);
        assert_eq!(fit_end("日本語のパス", 7), "日本語…");
        assert_eq!(columns(&fit_end("日本語のパス", 7)), 7);
        assert_eq!(fit_start("/home/日本語/ファイル.rs", 12), "…ファイル.rs");
        assert!(columns(&fit_start("/home/日本語/ファイル.rs", 12)) <= 12);
        assert_eq!(fit_end("e\u{301}te\u{301}", 3), "e\u{301}te\u{301}");
        assert_eq!(fit_end("e\u{301}te\u{301}s", 3), "e\u{301}t…");
        assert_eq!(fit_start("abe\u{301}", 2), "…e\u{301}");
        assert_eq!(fit_end("abc", 0), "");
    }
}
