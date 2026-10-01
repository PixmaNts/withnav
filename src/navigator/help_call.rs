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

/// Whether the lines added between `old` and `new` include a comment that
/// addresses the navigator. Only inserted lines count: a call for help
/// already in the file at baseline has been answered or ignored.
pub fn called_for_help(old: &str, new: &str) -> bool {
    similar::TextDiff::from_lines(old, new)
        .iter_all_changes()
        .filter(|c| matches!(c.tag(), similar::ChangeTag::Insert))
        .filter_map(|c| comment_text(c.value()).map(str::to_owned))
        .any(|comment| addresses_navigator(&comment))
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
    fn only_added_lines_count() {
        let old = "// help\nfn a() {}\n";
        assert!(!called_for_help(old, "// help\nfn a() {}\nfn b() {}\n"));
        assert!(called_for_help(old, "// help\n// aide\nfn a() {}\n"));
    }
}
