//! Spotting an explicit call for help: a comment the driver just wrote that
//! addresses the navigator ("help", "aide", "navigator", or a question).
//! One such line is a whole conversation opener, so the pipeline lets it
//! past the small-change pre-filter.

use std::collections::HashSet;

/// Words that address the navigator, compared lowercase against the words of
/// a comment (whole words only: "helper" is not a call for help).
const ADDRESSING_WORDS: [&str; 8] = [
    "help",
    "aide",
    "aider",
    "aidez",
    "navigator",
    "navigateur",
    "sos",
    "assist",
];

/// The comment text of one line, when the line is a comment or ends with
/// one. Recognised markers: `//`, `/*`, `*` (block continuation), `--`,
/// `<!--`, `;;` and `# ` (a bare `#[` is a Rust attribute, not a comment).
fn comment_text(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    for marker in ["//", "/*", "<!--", "--", ";;"] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            return Some(rest);
        }
    }
    if let Some(rest) = trimmed.strip_prefix("* ") {
        return Some(rest);
    }
    if trimmed == "#" || trimmed.starts_with("# ") {
        return Some(&trimmed[1..]);
    }
    // A trailing comment after code: ` // ...`.
    trimmed.find(" // ").map(|at| &trimmed[at + 4..])
}

/// True when the comment text addresses the navigator.
fn addresses_navigator(comment: &str) -> bool {
    if comment.contains('?') {
        return true;
    }
    let lowered = comment.to_lowercase();
    let words: HashSet<&str> = lowered
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    ADDRESSING_WORDS.iter().any(|w| words.contains(w))
}

/// The comment, lowercased with its whitespace collapsed: the same call for
/// help stays the same when its line moves or its indentation changes.
fn normalize(comment: &str) -> String {
    comment
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The normalized help-call comments among `lines`, without repeats.
fn help_comments<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for comment in lines
        .filter_map(comment_text)
        .filter(|c| addresses_navigator(c))
    {
        let comment = normalize(comment);
        if !found.contains(&comment) {
            found.push(comment);
        }
    }
    found
}

/// The help-call comments among the lines added between `old` and `new`.
/// Only inserted lines count: a call for help already in the file at
/// baseline has been answered or ignored.
pub fn added_help_comments(old: &str, new: &str) -> Vec<String> {
    let diff = similar::TextDiff::from_lines(old, new);
    let added: Vec<_> = diff
        .iter_all_changes()
        .filter(|c| matches!(c.tag(), similar::ChangeTag::Insert))
        .collect();
    help_comments(added.iter().map(|c| c.value()))
}

/// Whether the lines added between `old` and `new` include a comment that
/// addresses the navigator.
pub fn called_for_help(old: &str, new: &str) -> bool {
    !added_help_comments(old, new).is_empty()
}

/// `text` without the lines whose comment is one of `gone`. A baseline still
/// holding a comment that was deleted would hide it from the diff when it is
/// written again.
pub fn without_comments(text: &str, gone: &[String]) -> String {
    text.split_inclusive('\n')
        .filter(|line| {
            !comment_text(line)
                .filter(|c| addresses_navigator(c))
                .is_some_and(|c| gone.contains(&normalize(c)))
        })
        .collect()
}

/// What one document remembers about its calls for help, keyed by the
/// normalized comment text: the ones a remark is being written for, and the
/// ones already answered. Without it the comment, which stays in the file,
/// is a fresh call on every save and idle, and each trigger cancels the
/// answer of the one before.
#[derive(Debug, Default)]
pub struct HelpCalls {
    in_flight: HashSet<String>,
    answered: HashSet<String>,
}

impl HelpCalls {
    /// Forgets the answered comments that are no longer in `text`, so
    /// writing one again later asks again. Returns the forgotten ones.
    pub fn forget_gone(&mut self, text: &str) -> Vec<String> {
        if self.answered.is_empty() {
            return Vec::new();
        }
        let present: HashSet<String> = help_comments(text.lines()).into_iter().collect();
        let gone: Vec<String> = self
            .answered
            .iter()
            .filter(|c| !present.contains(*c))
            .cloned()
            .collect();
        for comment in &gone {
            self.answered.remove(comment);
        }
        gone
    }

    /// The calls among `added` that no remark answered yet.
    pub fn unanswered(&self, added: Vec<String>) -> Vec<String> {
        added
            .into_iter()
            .filter(|c| !self.answered.contains(c))
            .collect()
    }

    /// True when a remark is being written for every one of `calls` (and
    /// there is at least one).
    pub fn covers(&self, calls: &[String]) -> bool {
        !calls.is_empty() && calls.iter().all(|c| self.in_flight.contains(c))
    }

    /// A remark for `calls` is being written (replacing any earlier one).
    pub fn begin(&mut self, calls: &[String]) {
        self.in_flight = calls.iter().cloned().collect();
    }

    /// No remark is being written any more.
    pub fn end(&mut self) {
        self.in_flight.clear();
    }

    /// `calls` got their remark.
    pub fn answer(&mut self, calls: &[String]) {
        self.answered.extend(calls.iter().cloned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn added(line: &str) -> bool {
        called_for_help("fn main() {}\n", &format!("fn main() {{}}\n{line}\n"))
    }

    #[test]
    fn comments_that_address_the_navigator_are_calls_for_help() {
        for line in [
            "// J'ai besoin d'aide",
            "// help",
            "    // Help me with this loop",
            "# navigator, what do you think",
            "/* Navigateur ? */",
            "let x = 1; // why does this fail?",
            "-- aidez-moi",
            "// SOS",
        ] {
            assert!(added(line), "{line}");
        }
    }

    #[test]
    fn ordinary_comments_and_code_are_not() {
        for line in [
            "// helper for the parser",
            "// TODO: rename",
            "#[derive(Debug)]",
            "let url = \"http://x/help\";",
            "let x = y?;",
            "fn help() {}",
            "",
        ] {
            assert!(!added(line), "{line}");
        }
    }

    #[test]
    fn the_same_call_is_the_same_wherever_it_sits() {
        let comments = |text: &str| added_help_comments("", text);
        assert_eq!(
            comments("fn a() {}\n    // Need   HELP here\n"),
            comments("// need help here\nfn a() {}\n")
        );
        assert_eq!(comments("// help\n// help\n"), vec!["help"]);
    }

    #[test]
    fn an_answered_call_is_forgotten_when_its_comment_is_gone() {
        let mut calls = HelpCalls::default();
        let text = "// need help\nfn a() {}\n";
        calls.answer(&["need help".to_string()]);
        assert!(calls.unanswered(vec!["need help".to_string()]).is_empty());
        assert!(calls.forget_gone(text).is_empty());
        assert!(calls.unanswered(vec!["need help".to_string()]).is_empty());
        assert_eq!(calls.forget_gone("fn a() {}\n"), vec!["need help"]);
        assert_eq!(calls.unanswered(vec!["need help".to_string()]).len(), 1);
    }

    #[test]
    fn a_deleted_comment_leaves_the_baseline_so_writing_it_again_is_added() {
        let gone = vec!["need help".to_string()];
        let baseline = without_comments("fn a() {}\n    // Need help\nfn b() {}\n", &gone);
        assert_eq!(baseline, "fn a() {}\nfn b() {}\n");
        assert!(called_for_help(
            &baseline,
            "fn a() {}\n// need help\nfn b() {}\n"
        ));
    }

    #[test]
    fn a_remark_in_flight_covers_only_the_calls_it_answers() {
        let mut calls = HelpCalls::default();
        let (a, b) = ("a help".to_string(), "b help".to_string());
        assert!(!calls.covers(std::slice::from_ref(&a)));
        calls.begin(std::slice::from_ref(&a));
        assert!(calls.covers(std::slice::from_ref(&a)));
        assert!(!calls.covers(&[a.clone(), b]));
        assert!(!calls.covers(&[]));
        calls.end();
        assert!(!calls.covers(&[a]));
    }

    #[test]
    fn only_added_lines_count() {
        let old = "// help\nfn a() {}\n";
        assert!(!called_for_help(old, "// help\nfn a() {}\nfn b() {}\n"));
        assert!(called_for_help(old, "// help\n// aide\nfn a() {}\n"));
    }
}
