use tokio::time::Instant;

use tokio_util::sync::CancellationToken;

/// Per-URI document state tracked by the navigator.
#[derive(Debug)]
pub struct DocumentState {
    /// Current text (full sync).
    pub text: String,
    /// Current document version (from didOpen/didChange).
    pub version: i32,
    /// Text at the last remark, or at open.
    pub baseline: String,
    /// Instant of the last edit (didOpen or didChange). Pausable clock
    /// (tokio Instant) so tests control cooldowns deterministically.
    pub last_edit: Instant,
    /// Instant of the last emitted remark, if any.
    pub last_remark: Option<Instant>,
    /// 1-based line of the last edit that inserted text (0: none yet).
    /// The conversation trigger shows the code around it.
    pub last_edit_line: usize,
    /// Version at which the in-flight request was started, if any.
    pub in_flight_version: Option<i32>,
    /// Cancel handle for the in-flight commenter call.
    pub in_flight_cancel: Option<CancellationToken>,
}

impl DocumentState {
    pub fn new(text: String, version: i32, now: Instant) -> Self {
        Self {
            text,
            version,
            baseline: String::new(),
            last_edit: now,
            last_remark: None,
            last_edit_line: 0,
            in_flight_version: None,
            in_flight_cancel: None,
        }
    }

    /// Full-sync update: replaces text, bumps version, stamps the edit time.
    pub fn apply_change(&mut self, text: String, version: i32, now: Instant) {
        self.text = text;
        self.version = version;
        self.last_edit = now;
    }

    /// Number of changed lines since the baseline (added + removed).
    pub fn changed_lines_since_baseline(&self) -> usize {
        count_changed_lines(&self.baseline, &self.text)
    }

    /// Rebases the baseline to the current text (after a remark or silence).
    pub fn rebase(&mut self) {
        self.baseline = self.text.clone();
    }
}

/// Counts added + removed lines between two texts using a line diff.
pub fn count_changed_lines(old: &str, new: &str) -> usize {
    let diff = similar::TextDiff::from_lines(old, new);
    diff.iter_all_changes()
        .filter(|c| {
            matches!(
                c.tag(),
                similar::ChangeTag::Insert | similar::ChangeTag::Delete
            )
        })
        .count()
}

/// Renders a unified diff of the changed region (3 context lines).
pub fn unified_diff(old: &str, new: &str) -> String {
    let diff = similar::TextDiff::from_lines(old, new);
    diff.unified_diff().context_radius(3).to_string()
}

/// The text around `line` (1-based; 0 means the top of the file): a window
/// of `context` lines on each side, every line numbered like
/// [`text_around_changes`]. This is what the conversation trigger shows,
/// as it has no diff to center on.
pub fn text_around_line(text: &str, line: usize, context: usize) -> String {
    let total = text.lines().count();
    let (start, end) = if line == 0 {
        (0, (2 * context + 1).min(total))
    } else {
        let center = (line - 1).min(total.saturating_sub(1));
        (
            center.saturating_sub(context),
            (center + context + 1).min(total),
        )
    };
    text.lines()
        .enumerate()
        .skip(start)
        .take(end.saturating_sub(start))
        .map(|(i, l)| format!("{:>4}| {l}", i + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extracts the current text truncated around the changed region: a window
/// of `context` lines around every inserted/replaced new line.
pub fn text_around_changes(old: &str, new: &str, context: usize) -> String {
    let diff = similar::TextDiff::from_lines(old, new);
    let new_line_count = new.lines().count();
    // Mark which new lines are touched (inserts or replacements).
    let mut touched = vec![false; new_line_count];
    for change in diff.iter_all_changes() {
        if matches!(change.tag(), similar::ChangeTag::Insert) {
            if let Some(idx) = change.new_index() {
                touched[idx] = true;
            }
        }
    }
    // Expand: any touched line taints its neighbors within `context`.
    let mut keep = vec![false; new_line_count];
    for (i, t) in touched.iter().enumerate() {
        if *t {
            for k in
                keep[i.saturating_sub(context)..(i + context + 1).min(new_line_count)].iter_mut()
            {
                *k = true;
            }
        }
    }
    // Each kept line carries its 1-based number: the model must answer with a
    // line in the current file, and without numbers it has to guess (observed
    // 2026-09-29: a remark about a `parse().unwrap()` landed one line below).
    let numbered = |(i, l): (usize, &str)| format!("{:>4}| {l}", i + 1);
    let selected: Vec<String> = new
        .lines()
        .enumerate()
        .zip(keep.iter())
        .filter(|(_, k)| **k)
        .map(|(il, _)| numbered(il))
        .collect();
    if selected.is_empty() {
        new.lines()
            .enumerate()
            .map(numbered)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        selected.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_around_line_windows_a_line_or_the_top() {
        let text = (1..=100)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let around = text_around_line(&text, 50, 2);
        assert_eq!(around.lines().count(), 5);
        assert!(around.starts_with("  48| l48"));
        assert!(around.ends_with("  52| l52"));
        let top = text_around_line(&text, 0, 2);
        assert!(top.starts_with("   1| l1"));
        assert_eq!(top.lines().count(), 5);
        // A line past the end clamps to the last line.
        assert!(text_around_line("a\nb", 9, 1).ends_with("   2| b"));
        assert_eq!(text_around_line("", 3, 2), "");
    }

    #[test]
    fn changed_lines_counts_inserts_and_deletes() {
        let old = "a\nb\nc\n";
        let new = "a\nX\nc\nd\n";
        // replaced b with X (1 del + 1 ins), added d (1 ins)
        assert_eq!(count_changed_lines(old, new), 3);
    }

    #[test]
    fn identical_texts_have_zero_changes() {
        assert_eq!(count_changed_lines("a\nb\n", "a\nb\n"), 0);
    }

    #[test]
    fn unified_diff_has_hunk_header() {
        let d = unified_diff("a\nb\nc\n", "a\nB\nc\n");
        assert!(d.contains("@@"));
    }

    #[test]
    fn text_around_changes_windows_the_region() {
        let old = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n";
        let new = "l1\nl2\nl3\nl4\nCHANGED\nl6\nl7\nl8\n";
        let win = text_around_changes(old, new, 1);
        // window = [l4, CHANGED, l6] with their real line numbers.
        assert_eq!(win, "   4| l4\n   5| CHANGED\n   6| l6");
    }

    #[test]
    fn text_around_changes_all_inserts_keeps_neighborhood() {
        let old = "a\nz\n";
        let new = "a\nb\nc\nd\nz\n";
        // the diff inserts b, c, d between a and z
        assert_eq!(
            text_around_changes(old, new, 0),
            "   2| b\n   3| c\n   4| d"
        );
        assert_eq!(
            text_around_changes(old, new, 1),
            "   1| a\n   2| b\n   3| c\n   4| d\n   5| z"
        );
    }
}
