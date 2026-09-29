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

/// Extracts the current text truncated around the changed region: a window
/// of `context` lines around every inserted/replaced new line.
pub fn text_around_changes(old: &str, new: &str, context: usize) -> String {
    let diff = similar::TextDiff::from_lines(old, new);
    let new_line_count = new.lines().count();
    // Mark which new lines are touched (inserts or replacements).
    let mut touched = vec![false; new_line_count];
    for change in diff.iter_all_changes() {
        if matches!(change.tag(), similar::ChangeTag::Insert) {
            let new_index = change.new_index();
            if let Some(mut idx) = new_index {
                // iter_all_changes yields per-line entries for line diffs;
                // a change's new_index is its first new line.
                touched[idx] = true;
                // Some entries span the moved region; mark conservatively
                // until the next old_index boundary (i.e. also mark the
                // remainder of the replacement run below via Replace tags).
                let _ = &mut idx;
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
    let lines: Vec<&str> = new.lines().collect();
    let selected: Vec<&str> = lines
        .iter()
        .zip(keep.iter())
        .filter(|(_, k)| **k)
        .map(|(l, _)| *l)
        .collect();
    if selected.is_empty() {
        new.to_string()
    } else {
        selected.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(win.contains("CHANGED"));
        // window = [l4, CHANGED, l6]: the far-away lines are excluded.
        assert_eq!(win, "l4\nCHANGED\nl6");
    }

    #[test]
    fn text_around_changes_all_inserts_keeps_neighborhood() {
        let old = "a\nz\n";
        let new = "a\nb\nc\nd\nz\n";
        // the diff inserts b, c, d between a and z
        assert_eq!(text_around_changes(old, new, 0), "b\nc\nd");
        assert_eq!(text_around_changes(old, new, 1), "a\nb\nc\nd\nz");
    }
}
