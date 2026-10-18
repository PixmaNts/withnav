//! Text measured and cut in terminal columns: a CJK character takes two, a combining mark none,
//! and an emoji sequence (a heart with its presentation selector, a family joined by ZWJ) takes
//! the width of the whole sequence, which is not the sum of its characters. So text is only ever
//! cut between grapheme clusters, and every candidate is measured whole.
use ratatui::text::Span;
use unicode_segmentation::UnicodeSegmentation;

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
    let mut kept = String::new();
    for cluster in text.graphemes(true) {
        let candidate = format!("{kept}{cluster}…");
        if columns(&candidate) > width {
            break;
        }
        kept.push_str(cluster);
    }
    kept.push('…');
    kept
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
    let mut kept = String::new();
    for cluster in text.graphemes(true).rev() {
        let candidate = format!("…{cluster}{kept}");
        if columns(&candidate) > width {
            break;
        }
        kept.insert_str(0, cluster);
    }
    format!("…{kept}")
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

    // An emoji with its presentation selector takes two columns although its characters add up
    // to one: a cut per character would overflow the budget.
    #[test]
    fn emoji_sequences_never_exceed_the_budget() {
        let samples = [
            "❤\u{fe0f}❤\u{fe0f}❤\u{fe0f}❤\u{fe0f}❤\u{fe0f}❤\u{fe0f}",
            "👨\u{200d}👩\u{200d}👧\u{200d}👦 family 👩\u{200d}💻 at work",
            "a❤\u{fe0f}b日本e\u{301}🇫🇷c",
        ];
        for text in samples {
            for width in 0..=columns(text) + 2 {
                let end = fit_end(text, width);
                let start = fit_start(text, width);
                assert!(columns(&end) <= width, "{text:?} end {width}: {end:?}");
                assert!(
                    columns(&start) <= width,
                    "{text:?} start {width}: {start:?}"
                );
                assert_eq!(columns(&pad(text, width)), width, "{text:?} pad {width}");
            }
        }
        assert_eq!(
            fit_end("❤\u{fe0f}❤\u{fe0f}❤\u{fe0f}❤\u{fe0f}", 5),
            "❤\u{fe0f}❤\u{fe0f}…"
        );
    }
}
