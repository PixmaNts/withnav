use super::*;

#[test]
fn first_changed_line_finds_the_first_insert() {
    let (line, text) = first_changed_line("a\nb\n", "a\nb\nc\nd\n");
    assert_eq!(line, 3);
    assert_eq!(text, "c");
    // A pure deletion quotes nothing.
    assert_eq!(first_changed_line("a\nb\n", "a\n"), (0, String::new()));
    assert_eq!(first_changed_line("a\n", "a\n"), (0, String::new()));
}

#[test]
fn live_state_renders_edits_newest_first_and_diagnostics() {
    let mut docs = Documents::default();
    docs.open("file:///ws/a.rs", "rust".to_string(), String::new(), 1);
    let ring = &mut docs.open.get_mut("file:///ws/a.rs").unwrap().edits;
    for (ts, line) in [
        ("2026-10-01T10:00:00.000Z", 1),
        ("2026-10-01T10:01:00.000Z", 2),
    ] {
        ring.push_back(EditSummary {
            ts: ts.to_string(),
            changed_lines: 4,
            first_line: line,
            excerpt: "let x = 1;".to_string(),
        });
    }
    docs.set_diagnostics(
        "file:///ws/a.rs",
        vec![
            Diag {
                line: 4,
                severity: "error".to_string(),
                message: "cannot find value `y`".to_string(),
            },
            Diag {
                line: 9,
                severity: "info".to_string(),
                message: "unrelated hint".to_string(),
            },
        ],
    );
    let text = docs.render_live_state();
    // Newest edit first.
    let first = text.lines().next().unwrap();
    assert!(first.starts_with("2026-10-01T10:01:00.000Z"), "{text}");
    // Only errors and warnings make it in.
    assert!(text.contains("[error] cannot find value `y`"), "{text}");
    assert!(!text.contains("unrelated hint"), "{text}");
}

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
