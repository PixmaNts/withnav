use super::*;
use std::fs;

struct Fixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    root: PathBuf,
}

/// A temp home next to a temp repository (`<tmp>/repo/.git/`).
fn fixture() -> Fixture {
    fixture_with_prefix("instructions-")
}

fn fixture_with_prefix(prefix: &str) -> Fixture {
    let tmp = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .expect("tempdir");
    let base = fs::canonicalize(tmp.path()).expect("canonical");
    let home = base.join("home");
    let root = base.join("repo");
    fs::create_dir_all(home.join(".agents")).unwrap();
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::create_dir_all(root.join(".git")).unwrap();
    Fixture {
        _tmp: tmp,
        home,
        root,
    }
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn load(fx: &Fixture, workspace: &Path) -> Instructions {
    Instructions::load(
        &RulesRoots {
            home: Some(fx.home.clone()),
        },
        workspace,
    )
}

fn status_of<'a>(rules: &'a Instructions, path: &Path) -> &'a SourceStatus {
    &rules
        .sources()
        .iter()
        .find(|s| s.path == path)
        .unwrap_or_else(|| panic!("{} among {:?}", path.display(), rules.sources()))
        .status
}

// Given no rules file anywhere
// When the instructions load
// Then there is no message and no source
#[test]
fn nothing_found_gives_no_message() {
    let fx = fixture();
    let rules = load(&fx, &fx.root);
    assert!(rules.message().is_none());
    assert!(rules.sources().is_empty(), "{:?}", rules.sources());
}

// Given a home without AGENTS.md but with CLAUDE.md
// When the instructions load
// Then the global CLAUDE.md is used
#[test]
fn global_falls_back_to_claude_md() {
    let fx = fixture();
    write(&fx.home.join(".claude/CLAUDE.md"), "claude global");
    let rules = load(&fx, &fx.root);
    assert!(rules.message().unwrap().contains("claude global"));
    assert_eq!(rules.sources().len(), 1);
}

// Given a home with both global files
// When the instructions load
// Then AGENTS.md is sent and CLAUDE.md is reported as shadowed
#[test]
fn global_agents_md_shadows_claude_md() {
    let fx = fixture();
    write(&fx.home.join(".agents/AGENTS.md"), "agents global");
    write(&fx.home.join(".claude/CLAUDE.md"), "claude global");
    let rules = load(&fx, &fx.root);
    let message = rules.message().unwrap();
    assert!(message.contains("agents global"));
    assert!(!message.contains("claude global"));
    assert!(matches!(
        status_of(&rules, &fx.home.join(".claude/CLAUDE.md")),
        SourceStatus::Shadowed { .. }
    ));
}

// Given a project CLAUDE.md next to an AGENTS.md
// When the instructions load
// Then only AGENTS.md is read and CLAUDE.md is shadowed
#[test]
fn project_claude_md_is_shadowed_by_agents_md() {
    let fx = fixture();
    write(&fx.root.join("AGENTS.md"), "project agents");
    write(&fx.root.join("CLAUDE.md"), "project claude");
    let rules = load(&fx, &fx.root);
    assert!(!rules.message().unwrap().contains("project claude"));
    assert_eq!(
        status_of(&rules, &fx.root.join("CLAUDE.md")),
        &SourceStatus::Shadowed {
            by: fx.root.join("AGENTS.md")
        }
    );
}

// Given global, root and nested rules, each with a rules.md supplement
// When the instructions load from the nested directory
// Then the order is global, global rules.md, root, root rules.md, nested,
//      nested rules.md, and each file carries its own label
#[test]
fn order_runs_from_global_to_the_closest_directory() {
    // Source labels can contain the same tokens as the file contents.
    let fx = fixture_with_prefix("G2-R2-N2-");
    let nested = fx.root.join("crates/inner");
    write(&fx.home.join(".agents/AGENTS.md"), "G1");
    write(&fx.home.join(".agents/rules.md"), "G2");
    write(&fx.root.join("AGENTS.md"), "R1");
    write(&fx.root.join("rules.md"), "R2");
    write(&nested.join("CLAUDE.md"), "N1");
    write(&nested.join("rules.md"), "N2");
    let rules = load(&fx, &nested);
    let expected = [
        (fx.home.join(".agents/AGENTS.md"), "G1"),
        (fx.home.join(".agents/rules.md"), "G2"),
        (fx.root.join("AGENTS.md"), "R1"),
        (fx.root.join("rules.md"), "R2"),
        (nested.join("CLAUDE.md"), "N1"),
        (nested.join("rules.md"), "N2"),
    ];
    assert_eq!(
        rules.sources().iter().map(|s| &s.path).collect::<Vec<_>>(),
        expected.iter().map(|(path, _)| path).collect::<Vec<_>>()
    );
    let mut message = FRAMING.to_string();
    for (path, text) in expected {
        message.push_str(&format!(
            "\n\nInstructions from {}:\n{text}",
            path.display()
        ));
    }
    assert_eq!(rules.message().unwrap(), message);
}

// Given a rules.md and no AGENTS.md or CLAUDE.md in a directory
// When the instructions load
// Then rules.md is read on its own
#[test]
fn rules_md_alone_is_read() {
    let fx = fixture();
    write(&fx.root.join("rules.md"), "only rules");
    let rules = load(&fx, &fx.root);
    assert!(rules.message().unwrap().contains("only rules"));
}

// Given a worktree whose `.git` is a file
// When the instructions load from a subdirectory
// Then that directory is the root: its AGENTS.md is read, the parent's is not
#[test]
fn a_git_file_marks_the_worktree_root() {
    let fx = fixture();
    let outer = fx.root.clone();
    fs::remove_dir_all(outer.join(".git")).unwrap();
    write(&outer.join("AGENTS.md"), "outer");
    let wt = outer.join("wt");
    write(&wt.join(".git"), "gitdir: /elsewhere\n");
    write(&wt.join("AGENTS.md"), "worktree root");
    let sub = wt.join("src");
    fs::create_dir_all(&sub).unwrap();
    let rules = load(&fx, &sub);
    let message = rules.message().unwrap();
    assert!(message.contains("worktree root"));
    assert!(!message.contains("outer"));
}

// Given a workspace with no Git above it and an AGENTS.md in its parent
// When the instructions load
// Then only the workspace's own files are read
#[test]
fn without_git_only_the_workspace_is_read() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = fs::canonicalize(tmp.path()).unwrap();
    write(&base.join("AGENTS.md"), "parent");
    write(&base.join("ws/AGENTS.md"), "workspace");
    let rules = Instructions::load(&RulesRoots::default(), &base.join("ws"));
    let message = rules.message().unwrap();
    assert!(message.contains("workspace"));
    assert!(!message.contains("parent"));
}

// Given an AGENTS.md above the worktree root
// When the instructions load
// Then it is not read
#[test]
fn nothing_above_the_root_is_read() {
    let fx = fixture();
    let above = fx.root.parent().unwrap().to_path_buf();
    write(&above.join("AGENTS.md"), "above the root");
    write(&fx.root.join("AGENTS.md"), "root");
    let rules = load(&fx, &fx.root);
    assert!(!rules.message().unwrap().contains("above the root"));
}

// Given a nested AGENTS.md that is a link to the root's
// When the instructions load from the nested directory
// Then the text appears once and the second path is reported as a duplicate
#[cfg(unix)]
#[cfg(unix)]
#[test]
fn a_linked_file_is_read_once() {
    let fx = fixture();
    write(&fx.root.join("AGENTS.md"), "shared text");
    let nested = fx.root.join("sub");
    fs::create_dir_all(&nested).unwrap();
    std::os::unix::fs::symlink(fx.root.join("AGENTS.md"), nested.join("AGENTS.md")).unwrap();
    let rules = load(&fx, &nested);
    assert_eq!(rules.message().unwrap().matches("shared text").count(), 1);
    assert!(matches!(
        status_of(&rules, &nested.join("AGENTS.md")),
        SourceStatus::Duplicate { .. }
    ));
}

// Given a workspace reached through a link
// When the instructions load
// Then discovery uses the canonical path (the root is found behind the link)
#[cfg(unix)]
#[cfg(unix)]
#[test]
fn a_linked_workspace_is_resolved_first() {
    let fx = fixture();
    write(&fx.root.join("AGENTS.md"), "root rules");
    let real = fx.root.join("pkg");
    fs::create_dir_all(&real).unwrap();
    let elsewhere = fx.home.join("alias");
    std::os::unix::fs::symlink(&real, &elsewhere).unwrap();
    let rules = load(&fx, &elsewhere);
    assert!(rules.message().unwrap().contains("root rules"));
}

// Given a file over the per-file cap, made of multi-byte characters
// When the instructions load
// Then the text is cut on a character boundary, with a visible marker, and
//      the source is reported truncated with the omitted byte count
#[test]
fn a_big_file_is_cut_on_a_character_boundary() {
    let fx = fixture();
    // 3-byte characters: 16 KiB is not a multiple of 3, so the cut must back up.
    let text = "€".repeat(MAX_FILE_BYTES);
    write(&fx.root.join("AGENTS.md"), &text);
    let rules = load(&fx, &fx.root);
    let message = rules.message().unwrap();
    assert!(message.contains("[… truncated: "));
    let source = &rules.sources()[0];
    let SourceStatus::Truncated { omitted_bytes, cap } = source.status else {
        panic!("{:?}", source.status);
    };
    assert_eq!(cap, Cap::File);
    assert_eq!(source.loaded_bytes % 3, 0, "whole characters only");
    assert!(source.loaded_bytes <= MAX_FILE_BYTES);
    assert_eq!(
        source.loaded_bytes as u64 + omitted_bytes,
        text.len() as u64
    );
    assert!(message.contains(&format!("[… truncated: {omitted_bytes} bytes omitted]")));
}

// Given enough full-size files to pass the total cap
// When the instructions load
// Then later ones are omitted, and the kept bytes never exceed the total
#[test]
fn the_total_cap_truncates_then_omits() {
    let fx = fixture();
    let big = "é".repeat(MAX_FILE_BYTES);
    write(&fx.home.join(".agents/AGENTS.md"), &big);
    write(&fx.home.join(".agents/rules.md"), &big);
    write(&fx.root.join("AGENTS.md"), &big);
    let deep = fx.root.join("a");
    write(&deep.join("AGENTS.md"), &big);
    write(&deep.join("rules.md"), &big);
    let deeper = deep.join("b");
    write(&deeper.join("AGENTS.md"), "zzmarker");
    // Each big file is cut at 16 KiB, so four of them fill the total exactly
    // and the fifth is omitted.
    let rules = load(&fx, &deeper);
    assert!(rules.loaded_bytes() <= MAX_TOTAL_BYTES);
    assert!(!rules.message().unwrap().contains("zzmarker"));
    assert_eq!(
        status_of(&rules, &deep.join("rules.md")),
        &SourceStatus::OmittedForTotalCap
    );
    assert!(rules.sources().iter().any(RuleSource::is_problem));
}

// Given a total with less room left than a file needs
// When the instructions load
// Then that file is truncated and blamed on the total cap
#[test]
fn a_file_crossing_the_total_cap_is_truncated_by_it() {
    let fx = fixture();
    let big = "a".repeat(MAX_FILE_BYTES);
    write(&fx.home.join(".agents/AGENTS.md"), &big);
    write(&fx.home.join(".agents/rules.md"), &big);
    write(&fx.root.join("AGENTS.md"), &big);
    write(&fx.root.join("rules.md"), &"b".repeat(MAX_FILE_BYTES - 10));
    let deep = fx.root.join("a");
    write(&deep.join("AGENTS.md"), &big);
    let rules = load(&fx, &deep);
    assert_eq!(rules.loaded_bytes(), MAX_TOTAL_BYTES);
    let last = rules.sources().last().unwrap();
    assert!(matches!(
        last.status,
        SourceStatus::Truncated {
            cap: Cap::Total,
            ..
        }
    ));
    assert_eq!(last.loaded_bytes, 10);
}

// Given an AGENTS.md that cannot be read as a file (a directory)
// When the instructions load
// Then the failure is reported with its reason, and other files still load
#[test]
fn a_read_error_is_reported() {
    let fx = fixture();
    fs::create_dir_all(fx.root.join("AGENTS.md")).unwrap();
    write(&fx.root.join("rules.md"), "still here");
    let rules = load(&fx, &fx.root);
    assert!(matches!(
        status_of(&rules, &fx.root.join("AGENTS.md")),
        SourceStatus::ReadError(reason) if reason.contains("regular file")
    ));
    assert!(rules.message().unwrap().contains("still here"));
}

// Given a file that is not valid UTF-8
// When the instructions load
// Then it loads with replacement characters instead of failing
#[test]
fn invalid_utf8_loads_lossily() {
    let fx = fixture();
    fs::write(fx.root.join("AGENTS.md"), b"ok \xFF\xFE end").unwrap();
    let rules = load(&fx, &fx.root);
    assert!(rules.message().unwrap().contains("ok"));
    assert_eq!(rules.sources()[0].status, SourceStatus::Loaded);
}

// Given an empty or blank AGENTS.md
// When the instructions load
// Then no message is produced and the source is reported empty
#[test]
fn a_blank_file_gives_no_message() {
    let fx = fixture();
    write(&fx.root.join("AGENTS.md"), "  \n\n");
    let rules = load(&fx, &fx.root);
    assert!(rules.message().is_none());
    assert_eq!(rules.sources()[0].status, SourceStatus::Empty);
}

// Given a loaded rules file
// When the message renders
// Then it opens with the framing line, then one labelled section per file
#[test]
fn the_message_is_framed_and_labelled() {
    let fx = fixture();
    write(&fx.root.join("AGENTS.md"), "be brief");
    let rules = load(&fx, &fx.root);
    let expected = format!(
        "{FRAMING}\n\nInstructions from {}:\nbe brief",
        fx.root.join("AGENTS.md").display()
    );
    assert_eq!(rules.message().unwrap(), expected);
}
