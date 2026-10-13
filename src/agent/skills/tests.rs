use super::*;
use crate::agent::tests::WorkspaceFixture as Fixture;
use crate::tools::{Registry, ToolContext, ToolStatus};
use serde_json::json;
use std::fs;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn fixture() -> Fixture {
    Fixture::new("packages/app")
}

#[test]
fn skill_authority_and_selection_queue_follow_the_logical_turn() {
    let fx = fixture();
    add(&fx.home.join(".agents/skills/live"), "live", "Live skill");
    let mut actor = actor(&fx, Registry::new());
    actor.set_skills(load(&fx));
    actor.push_user("first request");
    let generation = actor.skill_turn_generation;
    let snapshot = actor.skills.snapshot("live").unwrap();
    actor.skills.validation.approve(&snapshot, false).unwrap();
    actor
        .selected_skill_snapshots
        .extend([(generation, "first".into()), (generation, "second".into())]);
    assert!(actor.active_validated_skill().is_some());
    // Permission and compaction continuations leave the logical generation intact.
    assert_eq!(actor.skill_turn_generation, generation);
    actor.end_skill_turn();
    assert!(actor.active_validated_skill().is_none());
    assert!(actor.skills.validation.approved.lock().unwrap().is_empty());
    actor.push_user_with_provenance("wake", crate::provider::openai::UserProvenance::Navigator);
    actor.begin_skill_turn();
    assert_ne!(actor.skill_turn_generation, generation);
    assert!(actor.selected_skill_snapshots.is_empty());
    actor.skills.validation.approve(&snapshot, false).unwrap();
    actor
        .selected_skill_snapshots
        .push((actor.skill_turn_generation, "cancelled selection".into()));
    actor.push_user("replacement request");
    assert!(actor.active_validated_skill().is_none());
    assert!(actor.selected_skill_snapshots.is_empty());
}

#[tokio::test]
async fn compaction_continuation_keeps_the_same_skill_generation() {
    let fx = fixture();
    add(&fx.home.join(".agents/skills/live"), "live", "Live skill");
    let mut actor = actor(&fx, Registry::new());
    actor.set_skills(load(&fx));
    actor.push_user("use this skill");
    let generation = actor.skill_turn_generation;
    let snapshot = actor.skills.snapshot("live").unwrap();
    actor.skills.validation.approve(&snapshot, false).unwrap();
    actor.input_budget = Some(0);
    let first = crate::agent::run_loop::run_agent(&mut actor, &mut |_| {})
        .await
        .unwrap();
    assert!(matches!(
        first,
        crate::agent::run_loop::RunOutcome::NeedsCompaction { .. }
    ));
    assert_eq!(actor.skill_turn_generation, generation);
    assert!(actor.active_validated_skill().is_some());
    let continued = crate::agent::run_loop::continue_run(&mut actor, &mut |_| {}, 1)
        .await
        .unwrap();
    assert!(matches!(
        continued,
        crate::agent::run_loop::RunOutcome::NeedsCompaction { .. }
    ));
    assert!(actor.active_validated_skill().is_some());
}

#[tokio::test]
async fn provider_failure_and_interruption_clear_once_before_a_wake() {
    let fx = fixture();
    add(&fx.home.join(".agents/skills/live"), "live", "Live skill");
    let mut actor = actor(&fx, Registry::new());
    actor.set_skills(load(&fx));
    for interrupted in [false, true] {
        actor.push_user("use skill");
        let snapshot = actor.skills.snapshot("live").unwrap();
        actor.skills.validation.approve(&snapshot, false).unwrap();
        if interrupted {
            actor.cancel.cancel();
        }
        let result = crate::agent::run_loop::run_agent(&mut actor, &mut |_| {}).await;
        assert!(
            result.is_err()
                || matches!(result, Ok(crate::agent::run_loop::RunOutcome::Interrupted))
        );
        assert!(actor.active_validated_skill().is_none());
        actor.push_user_with_provenance("wake", crate::provider::openai::UserProvenance::Navigator);
        assert!(actor.active_validated_skill().is_none());
    }
}

#[test]
fn validation_hash_covers_every_file_and_admits_the_same_snapshot() {
    let fx = fixture();
    let directory = fx.home.join(".agents/skills/validated");
    add(&directory, "validated", "A complete skill");
    fs::write(directory.join("script.sh"), "first bytes").unwrap();
    let mut skills = load(&fx);
    skills.validation = Arc::new(validation::SkillValidation {
        data_dir: Some(fx.home.join("data")),
        ..Default::default()
    });
    let first = skills.snapshot("validated").unwrap();
    assert!(!skills.validation.validated(&first).unwrap());
    skills.validation.approve(&first, false).unwrap();
    assert!(!skills.validation.validated(&first).unwrap());
    skills.validation.begin_turn();
    assert!(skills.active_validated_skill().is_none());
    assert!(skills.validation.approved.lock().unwrap().is_empty());
    skills.validation.approve(&first, true).unwrap();
    assert!(skills
        .validation
        .validated(&skills.snapshot("validated").unwrap())
        .unwrap());
    fs::write(directory.join("script.sh"), "changed bytes").unwrap();
    let changed = skills.snapshot("validated").unwrap();
    assert_ne!(first.hash, changed.hash);
    assert!(!skills.validation.validated(&changed).unwrap());
    fs::write(
        directory.join("SKILL.md"),
        skill_text("validated", "Changed body").replace("Body of validated.", "NEW BODY"),
    )
    .unwrap();
    assert!(!first.output.output.contains("NEW BODY"));
    let store = crate::permission::store::PermissionStore::open(fx.home.join("data")).unwrap();
    store
        .revoke_validation(store.list_validations().unwrap()[0].id)
        .unwrap();
    assert!(!skills.validation.validated(&first).unwrap());
}

#[test]
fn validation_never_hashes_a_truncated_inventory() {
    let fx = fixture();
    let directory = fx.home.join(".agents/skills/large");
    add(&directory, "large", "Large skill");
    let skills = load(&fx);
    for i in 0..256 {
        fs::write(directory.join(format!("file-{i}")), "x").unwrap();
    }
    assert!(skills.snapshot("large").unwrap_err().contains("256 files"));
    for i in 0..256 {
        fs::remove_file(directory.join(format!("file-{i}"))).unwrap();
    }
    fs::write(directory.join("large.bin"), vec![0; 8 * 1024 * 1024]).unwrap();
    assert!(skills.snapshot("large").unwrap_err().contains("8 MiB"));
}

fn skill_text(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n\nBody of {name}.\n")
}

fn add(dir: &Path, name: &str, description: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join("SKILL.md");
    fs::write(&path, skill_text(name, description)).unwrap();
    path
}

fn load(fx: &Fixture) -> Skills {
    Skills::load(
        &RulesRoots {
            home: Some(fx.home.clone()),
        },
        &fx.ws,
    )
}

fn catalog_lines(skills: &Skills) -> Vec<String> {
    skills
        .catalog()
        .map(|c| c.lines().skip(1).map(String::from).collect())
        .unwrap_or_default()
}

fn has(skills: &Skills, f: impl Fn(&DiagnosticKind) -> bool) -> bool {
    skills.diagnostics().iter().any(|d| f(&d.kind))
}

fn path_of(skills: &Skills, name: &str) -> PathBuf {
    match skills.resolve(name) {
        Resolution::Found(s) => s.path.clone(),
        _ => panic!("{name} not found"),
    }
}

// Given a library shaped like the real one: nested two levels, a flat skill,
//       a stray AGENTS.md at the root of the skills directory
// When skills are discovered
// Then every SKILL.md counts and the stray file does not
#[test]
fn the_nested_real_layout_is_discovered() {
    let fx = fixture();
    let root = fx.home.join(".agents/skills");
    add(&root.join("matt/ask-matt"), "ask-matt", "Ask Matt.");
    add(&root.join("matt/grill-me"), "grill-me", "Be grilled.");
    add(
        &root.join("coding/rust/review"),
        "rust-review",
        "Review Rust.",
    );
    add(&root.join("flat"), "flat", "A flat one.");
    fs::write(root.join("AGENTS.md"), "# not a skill\n").unwrap();
    fs::write(root.join("matt/README.md"), "# not a skill\n").unwrap();

    let skills = load(&fx);
    assert_eq!(skills.len(), 4);
    assert_eq!(
        catalog_lines(&skills),
        [
            "ask-matt — Ask Matt.",
            "flat — A flat one.",
            "grill-me — Be grilled.",
            "rust-review — Review Rust."
        ]
    );
    assert!(
        skills.diagnostics().is_empty(),
        "{:?}",
        skills.diagnostics()
    );
    assert_eq!(skills.readable_roots(), [root.canonicalize().unwrap()]);
}

// Given a skill directory with a SKILL.md and a nested directory with another
// When discovered
// Then the descent stops at the first
#[test]
fn the_walk_stops_at_a_skill_directory() {
    let fx = fixture();
    let root = fx.home.join(".claude/skills");
    add(&root.join("outer"), "outer", "Outer.");
    add(&root.join("outer/inner"), "inner", "Inner.");
    let skills = load(&fx);
    assert_eq!(catalog_lines(&skills), ["outer — Outer."]);
}

// Given skills buried too deep, or under .git, node_modules and target
// When discovered
// Then only those within depth 8 and outside the skipped directories count
#[test]
fn depth_and_skipped_directories_are_respected() {
    let fx = fixture();
    let root = fx.home.join(".agents/skills");
    let mut deep = root.clone();
    for i in 0..MAX_DEPTH {
        deep = deep.join(format!("d{i}"));
    }
    add(&deep, "deepest", "At the limit, depth 8.");
    add(&deep.join("d8"), "too-deep", "One level too far.");
    add(&root.join("a/node_modules/x"), "nm", "No.");
    add(&root.join("a/target/x"), "tg", "No.");
    add(&root.join("a/.git/x"), "gt", "No.");
    // `deep` has a SKILL.md, so nothing is below it: use a sibling chain.
    let mut chain = root.join("chain");
    for i in 0..MAX_DEPTH {
        chain = chain.join(format!("c{i}"));
    }
    add(&chain.join("c8"), "chain-too-deep", "No.");

    let names: Vec<_> = catalog_lines(&load(&fx));
    assert_eq!(names, ["deepest — At the limit, depth 8."]);
}

// Given the walk meets more entries than the bound
// Then it stops with a diagnostic and never counts past the bound
#[test]
fn the_entry_count_is_bounded() {
    let fx = fixture();
    let root = fx.home.join(".agents/skills");
    add(&root.join("aaa"), "first", "Found first.");
    for i in 0..30 {
        fs::create_dir_all(root.join(format!("zz{i}"))).unwrap();
    }
    let mut walk = Walk::default();
    walk.add_root(
        root,
        Rank {
            user: true,
            distance: 0,
            source: 0,
        },
    );
    walk.entries = MAX_ENTRIES - 3;
    walk.run();
    // The count never passes the bound: the directory was cut before it was
    // collected, not after.
    assert_eq!(walk.entries, MAX_ENTRIES);
    let skills = walk.finish();
    // A directory cut by the bound is read in directory order, so which of
    // its entries survive is not fixed: only the bound and the diagnostic are.
    assert!(skills.len() <= 1);
    assert!(has(&skills, |k| matches!(
        k,
        DiagnosticKind::LimitReached(_)
    )));
}

// Given a directory larger than the entry bound, and a second root after it
// When the walk runs
// Then no more than the bound is enumerated, in all, and the second root is
//      not read at all
#[test]
fn a_directory_over_the_bound_is_cut_while_it_is_read() {
    let fx = fixture();
    let first = fx.home.join(".agents/skills");
    let second = fx.home.join(".claude/skills");
    fs::create_dir_all(&first).unwrap();
    for i in 0..MAX_ENTRIES + 50 {
        fs::create_dir(first.join(format!("d{i:05}"))).unwrap();
    }
    add(&second.join("later"), "later", "Never read.");
    let rank = |source| Rank {
        user: true,
        distance: 0,
        source,
    };
    let mut walk = Walk::default();
    walk.add_root(first, rank(0));
    walk.add_root(second, rank(1));
    walk.run();
    assert_eq!(walk.entries, MAX_ENTRIES);
    let skills = walk.finish();
    assert!(skills.is_empty(), "the second root was skipped");
    let limits = skills
        .diagnostics()
        .iter()
        .filter(|d| matches!(d.kind, DiagnosticKind::LimitReached(_)))
        .count();
    assert_eq!(limits, 1);
}

// Given multi-line descriptions (folded and literal) and unknown fields
// When discovered
// Then the skill loads, flattened, and the extra keys are ignored
#[test]
fn multiline_yaml_and_unknown_fields_are_tolerated() {
    let fx = fixture();
    let root = fx.home.join(".agents/skills");
    fs::create_dir_all(root.join("a")).unwrap();
    fs::write(
        root.join("a/SKILL.md"),
        "---\nname: folded\ndescription: >\n  First line\n  second line.\nversion: 1.2.3\nauthor: me\nlicense: MIT\nplatforms: [linux, macos]\nmetadata:\n  tags:\n    - a\n    - b\n  nested: {x: 1}\n---\nBody\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("b")).unwrap();
    fs::write(
        root.join("b/SKILL.md"),
        "---\nname: literal\ndescription: |\n  Line one\n  Line two\n---\nBody\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("c")).unwrap();
    fs::write(
        root.join("c/SKILL.md"),
        "---\nname: \"quoted:colon\"\ndescription: 'It''s quoted'\n---\nBody\n",
    )
    .unwrap();
    let lines = catalog_lines(&load(&fx));
    assert_eq!(
        lines,
        [
            "folded — First line second line.",
            "literal — Line one Line two",
            "quoted:colon — It's quoted",
        ]
        .map(String::from)
    );
    let manifest = root.join("formats/SKILL.md");
    fs::create_dir_all(manifest.parent().unwrap()).unwrap();
    let base = "---\nname: formats\ndescription: Valid.\n";
    let at_limit = format!(
        "{base}#{}\n---",
        "x".repeat(frontmatter::MAX_HEADER_BYTES - base.len() - 5)
    );
    let long_body = "x".repeat(frontmatter::MAX_HEADER_BYTES + 1);
    for (text, expected_body) in [
        (
            format!("{base}---\nBody\n").replace('\n', "\r\n"),
            "Body\r\n",
        ),
        (format!("{base}---"), ""),
        (at_limit, ""),
        (format!("{base}---\n{long_body}"), long_body.as_str()),
    ] {
        fs::write(&manifest, &text).unwrap();
        let (meta, body) = frontmatter::parse(&text).unwrap();
        assert_eq!(meta.name, "formats");
        assert_eq!(body, expected_body);
        assert!(matches!(load(&fx).resolve("formats"), Resolution::Found(_)));
    }
}

// Given malformed files next to a good one
// When discovered
// Then each bad one is a diagnostic and the good one stays
#[test]
fn a_malformed_file_never_hides_the_others() {
    let fx = fixture();
    let root = fx.home.join(".agents/skills");
    add(&root.join("good"), "good", "Fine.");
    let oversized = format!(
        "---\nname: x\ndescription: y\n#{}\n---\n",
        "x".repeat(frontmatter::MAX_HEADER_BYTES)
    );
    let prefix = "---\nname: x\ndescription: y\n#";
    let truncated_delimiter = format!(
        "{prefix}{}\n---x\n---\n",
        "x".repeat(frontmatter::MAX_HEADER_BYTES - prefix.len() - 4)
    );
    for (dir, text) in [
        ("nohead", "no header here\n"),
        ("unclosed", "---\nname: x\ndescription: y\n"),
        ("badyaml", "---\nname: [unclosed\ndescription: y\n---\n"),
        ("noname", "---\ndescription: y\n---\n"),
        ("nodesc", "---\nname: x\n---\n"),
        ("emptyname", "---\nname: ''\ndescription: y\n---\n"),
        ("spaced", "---\nname: has space\ndescription: y\n---\n"),
        ("wrongtype", "---\nname: [a, b]\ndescription: y\n---\n"),
        ("oversized", oversized.as_str()),
        ("truncated-delimiter", truncated_delimiter.as_str()),
    ] {
        fs::create_dir_all(root.join(dir)).unwrap();
        fs::write(root.join(dir).join("SKILL.md"), text).unwrap();
        assert!(frontmatter::parse(text).is_err(), "{dir}");
    }
    let skills = load(&fx);
    assert_eq!(catalog_lines(&skills), ["good — Fine."]);
    let invalid = skills
        .diagnostics()
        .iter()
        .filter(|d| matches!(d.kind, DiagnosticKind::Invalid(_)))
        .count();
    assert_eq!(invalid, 10, "{:?}", skills.diagnostics());
    assert!(skills.diagnostics().iter().all(Diagnostic::is_problem));
}

// Given the same id at every priority level
// When discovered
// Then the nearer project scope wins, then .agents > .claude > .opencode,
//      then user .agents > user .claude
#[test]
fn duplicate_names_follow_the_precedence() {
    let fx = fixture();
    let user_agents = add(&fx.home.join(".agents/skills/u"), "dup", "user agents");
    let user_claude = add(&fx.home.join(".claude/skills/u"), "dup", "user claude");
    assert_eq!(
        path_of(&load(&fx), "dup"),
        user_agents.canonicalize().unwrap()
    );

    let root_oc = add(&fx.repo.join(".opencode/skills/d"), "dup", "root opencode");
    assert_eq!(path_of(&load(&fx), "dup"), root_oc.canonicalize().unwrap());

    let root_claude = add(&fx.repo.join(".claude/skills/d"), "dup", "root claude");
    assert_eq!(
        path_of(&load(&fx), "dup"),
        root_claude.canonicalize().unwrap()
    );

    let root_agents = add(&fx.repo.join(".agents/skills/d"), "dup", "root agents");
    assert_eq!(
        path_of(&load(&fx), "dup"),
        root_agents.canonicalize().unwrap()
    );

    // The workspace scope is nearer than the worktree root, whatever the source.
    let ws_oc = add(&fx.ws.join(".opencode/skills/d"), "dup", "ws opencode");
    let skills = load(&fx);
    assert_eq!(path_of(&skills, "dup"), ws_oc.canonicalize().unwrap());
    assert_eq!(skills.len(), 1);
    assert!(has(&skills, |k| matches!(
        k,
        DiagnosticKind::Shadowed { .. }
    )));
    assert!(!skills.diagnostics().iter().any(Diagnostic::is_problem));
    let _ = user_claude;
}

// Given two skills with one id at the same priority
// When discovered
// Then the id is ambiguous: not offered, and the tool says so
#[tokio::test]
async fn equal_priority_duplicates_are_ambiguous() {
    let fx = fixture();
    let a = add(&fx.home.join(".agents/skills/x/one"), "twin", "First.");
    let b = add(&fx.home.join(".agents/skills/y/two"), "twin", "Second.");
    add(&fx.home.join(".agents/skills/z"), "solo", "Alone.");
    let skills = load(&fx);
    assert_eq!(catalog_lines(&skills), ["solo — Alone."]);
    assert!(matches!(skills.resolve("twin"), Resolution::Ambiguous(p) if p.len() == 2));
    let ambiguous = skills
        .diagnostics()
        .iter()
        .filter(|d| matches!(d.kind, DiagnosticKind::Ambiguous { .. }))
        .count();
    assert_eq!(ambiguous, 2);

    let (_, cx) = context(&fx);
    let res = call(&skills, "twin", &cx).await;
    let err = res.error.expect("error");
    assert_eq!(err.code, "skill_ambiguous");
    assert!(err
        .message
        .contains(&a.canonicalize().unwrap().display().to_string()));
    assert!(err
        .message
        .contains(&b.canonicalize().unwrap().display().to_string()));
}

// Given a skill with disable-model-invocation: true
// Then it is not in the catalog and the tool refuses it by that code
#[tokio::test]
async fn disabled_skills_stay_out_of_the_catalog() {
    let fx = fixture();
    let root = fx.home.join(".agents/skills");
    add(&root.join("on"), "on", "Visible.");
    fs::create_dir_all(root.join("off")).unwrap();
    fs::write(
        root.join("off/SKILL.md"),
        "---\nname: off\ndescription: Hidden.\ndisable-model-invocation: true\n---\nBody\n",
    )
    .unwrap();
    let skills = load(&fx);
    assert_eq!(catalog_lines(&skills), ["on — Visible."]);
    let (_, cx) = context(&fx);
    assert_eq!(
        call(&skills, "off", &cx).await.error.unwrap().code,
        "skill_disabled"
    );
    assert_eq!(call(&skills, "on", &cx).await.status, ToolStatus::Ok);
}

// Given only disabled skills, or none
// Then there is no catalog message
#[test]
fn no_skills_means_no_message() {
    let fx = fixture();
    assert!(load(&fx).catalog().is_none());
    assert!(load(&fx).is_empty());
    let root = fx.home.join(".agents/skills/off");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("SKILL.md"),
        "---\nname: off\ndescription: d\ndisable-model-invocation: true\n---\n",
    )
    .unwrap();
    assert!(load(&fx).catalog().is_none());
}

// Given a link looping back to an ancestor, and a link out of the roots
// When discovered
// Then the loop is cut once, the escape is rejected, nothing hangs
#[test]
fn cycles_and_escaping_links_are_rejected() {
    let fx = fixture();
    let root = fx.home.join(".agents/skills");
    add(&root.join("real"), "real", "Real.");
    let outside = fx.home.join("elsewhere");
    add(&outside.join("secret"), "secret", "Outside the roots.");
    fs::create_dir_all(root.join("loop")).unwrap();
    std::os::unix::fs::symlink(&root, root.join("loop/back")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("out")).unwrap();
    std::os::unix::fs::symlink(outside.join("secret"), root.join("single")).unwrap();

    let skills = load(&fx);
    assert_eq!(catalog_lines(&skills), ["real — Real."]);
    assert!(has(&skills, |k| matches!(k, DiagnosticKind::Cycle { .. })));
    let escapes = skills
        .diagnostics()
        .iter()
        .filter(|d| matches!(d.kind, DiagnosticKind::Escapes { .. }))
        .count();
    assert_eq!(escapes, 2, "{:?}", skills.diagnostics());
    assert!(!skills
        .readable_roots()
        .iter()
        .any(|r| r.starts_with(&outside)));
}

// Given a link to a directory inside another skill root, and a root reached
//       twice
// Then the skill counts once
#[test]
fn a_second_path_to_the_same_skill_counts_once() {
    let fx = fixture();
    add(&fx.home.join(".agents/skills/one"), "one", "One.");
    fs::create_dir_all(fx.home.join(".claude")).unwrap();
    std::os::unix::fs::symlink(
        fx.home.join(".agents/skills"),
        fx.home.join(".claude/skills"),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        fx.home.join(".agents/skills/one"),
        fx.home.join(".agents/skills/alias"),
    )
    .unwrap();
    let skills = load(&fx);
    assert_eq!(skills.len(), 1);
    assert!(
        skills.diagnostics().is_empty(),
        "{:?}",
        skills.diagnostics()
    );
}

// Given a SKILL.md that is a link to another SKILL.md in the same root
// When discovered
// Then it is one skill, not an ambiguous id
#[test]
fn a_manifest_linked_to_another_manifest_is_one_skill() {
    let fx = fixture();
    let root = fx.home.join(".agents/skills");
    let real = add(&root.join("b"), "twin", "Twin.");
    fs::create_dir_all(root.join("a")).unwrap();
    std::os::unix::fs::symlink(&real, root.join("a/SKILL.md")).unwrap();
    let skills = load(&fx);
    assert_eq!(skills.len(), 1);
    assert_eq!(catalog_lines(&skills), ["twin — Twin."]);
    assert!(!has(&skills, |k| matches!(
        k,
        DiagnosticKind::Ambiguous { .. }
    )));
    assert_eq!(path_of(&skills, "twin"), real);
}

// Given the same manifest reached from a project root and, through a link,
//       from a user root
// Then it is one skill at the better rank, shadowing nothing
#[test]
fn a_manifest_reached_from_two_roots_is_one_skill() {
    let fx = fixture();
    let real = add(&fx.repo.join(".agents/skills/shared"), "shared", "Shared.");
    fs::create_dir_all(fx.home.join(".agents/skills/u")).unwrap();
    std::os::unix::fs::symlink(&real, fx.home.join(".agents/skills/u/SKILL.md")).unwrap();
    let skills = load(&fx);
    assert_eq!(skills.len(), 1);
    assert_eq!(path_of(&skills, "shared"), real);
    assert!(
        skills.diagnostics().is_empty(),
        "{:?}",
        skills.diagnostics()
    );
}

// Given a project that is not a Git repository
// Then only the workspace scope is searched, never its parents
#[test]
fn without_git_only_the_workspace_scope_is_searched() {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let ws = base.join("a/b");
    add(&base.join(".agents/skills/up"), "up", "Above.");
    add(&ws.join(".agents/skills/here"), "here", "Here.");
    let skills = Skills::load(&RulesRoots { home: None }, &ws);
    assert_eq!(catalog_lines(&skills), ["here — Here."]);
}

// Given project and user skills
// Then the project ones are listed first, each group in a stable order
#[test]
fn project_skills_come_first_in_a_stable_order() {
    let fx = fixture();
    add(&fx.home.join(".agents/skills/a"), "aaa-user", "User.");
    add(&fx.repo.join(".agents/skills/z"), "zzz-root", "Root.");
    add(&fx.ws.join(".claude/skills/m"), "mmm-ws", "Workspace.");
    add(&fx.ws.join(".agents/skills/b"), "bbb-ws", "Workspace.");
    let first = catalog_lines(&load(&fx));
    assert_eq!(
        first,
        [
            "bbb-ws — Workspace.",
            "mmm-ws — Workspace.",
            "zzz-root — Root.",
            "aaa-user — User."
        ]
    );
    assert_eq!(first, catalog_lines(&load(&fx)));
}

// ---- catalog caps ----

#[test]
fn descriptions_are_flattened_and_capped() {
    let flat = catalog::flatten("one\n  two\t three");
    assert_eq!(flat, "one two three");
    let long = "é".repeat(400);
    let cut = catalog::flatten(&long);
    assert_eq!(cut.chars().count(), MAX_DESCRIPTION_CHARS);
    assert!(cut.ends_with('…'));
    let exact = "x".repeat(MAX_DESCRIPTION_CHARS);
    assert_eq!(catalog::flatten(&exact), exact);
    for description in [
        String::new(),
        " \t\r\n\u{a0}\u{2003}".into(),
        "  one\u{a0}two\u{2003}三\r\nfour  ".into(),
        "é 三\t".repeat(100),
        format!("{} last", "x".repeat(MAX_DESCRIPTION_CHARS - 2)),
        format!("{} last", "x".repeat(MAX_DESCRIPTION_CHARS - 1)),
        "x".repeat(MAX_DESCRIPTION_CHARS + 1),
    ] {
        let flat = description.split_whitespace().collect::<Vec<_>>().join(" ");
        let expected = if flat.chars().count() <= MAX_DESCRIPTION_CHARS {
            flat
        } else {
            let mut cut: String = flat.chars().take(MAX_DESCRIPTION_CHARS - 1).collect();
            cut.truncate(cut.trim_end().len());
            cut.push('…');
            cut
        };
        assert_eq!(
            catalog::flatten(&description).as_bytes(),
            expected.as_bytes()
        );
    }
}

#[test]
fn the_catalog_is_bounded_and_counts_what_it_omits() {
    let fx = fixture();
    let root = fx.home.join(".agents/skills");
    let description = "d".repeat(250);
    for i in 0..200 {
        add(
            &root.join(format!("s{i:03}")),
            &format!("skill-{i:03}"),
            &description,
        );
    }
    let skills = load(&fx);
    assert_eq!(skills.len(), 200);
    let catalog = skills.catalog().unwrap();
    // The ceiling is asserted on its own number, not through the constant, so
    // changing the constant breaks this test on purpose.
    assert_eq!(MAX_TOKENS, 8_192);
    assert!(catalog.len() <= 8_192 * 4, "{} bytes", catalog.len());
    let last = catalog.lines().last().unwrap();
    let omitted: usize = last
        .strip_suffix(" additional skills omitted")
        .unwrap_or_else(|| panic!("{last}"))
        .parse()
        .unwrap();
    let listed = catalog.lines().count() - 2;
    assert_eq!(listed + omitted, 200);
    assert!(omitted > 0 && listed > 0);
    assert_eq!(skills.listed().len(), listed);
    for skill in skills.listed() {
        assert!(catalog.contains(&format!("{} —", skill.name)));
    }
    // Nothing is cut when everything fits.
    let (_small_fixture, small) = load_small();
    assert!(!small.catalog().unwrap().contains("omitted"));
}

fn load_small() -> (Fixture, Skills) {
    let fx = fixture();
    add(&fx.home.join(".agents/skills/a"), "a", "A.");
    let skills = load(&fx);
    (fx, skills)
}

// ---- the tool ----

fn context(fx: &Fixture) -> (Registry, ToolContext) {
    (
        Registry::new(),
        ToolContext {
            binding: None,
            workspace: fx.ws.clone(),
            writable_roots: vec![fx.ws.clone()],
            readable_roots: Vec::new(),
        },
    )
}

async fn call(skills: &Skills, name: &str, cx: &ToolContext) -> crate::tools::ToolResult {
    match skills.snapshot(name) {
        Ok(snapshot) => {
            skills.validation.approve(&snapshot, false).unwrap();
        }
        Err(error) => {
            let code = if error.contains("skill_unknown") {
                "skill_unknown"
            } else if error.contains("skill_disabled") {
                "skill_disabled"
            } else if error.contains("skill_ambiguous") {
                "skill_ambiguous"
            } else if error.contains("exceeds 32 KiB") {
                "skill_oversized"
            } else {
                "skill_unreadable"
            };
            return crate::tools::ToolResult::error(code, error);
        }
    }
    let mut registry = Registry::new();
    registry.register(Box::new(SkillTool::new(Arc::new(skills.clone()))));
    registry
        .dispatch("skill", json!({"name":name}), CancellationToken::new(), cx)
        .await
        .unwrap()
}

#[tokio::test]
async fn the_tool_returns_body_paths_and_resources() {
    let fx = fixture();
    let dir = fx.home.join(".agents/skills/matt/ask-matt");
    let manifest = add(&dir, "ask-matt", "Ask Matt.");
    fs::create_dir_all(dir.join("scripts")).unwrap();
    fs::write(dir.join("scripts/run.sh"), "echo hi").unwrap();
    fs::write(dir.join("notes.md"), "n").unwrap();
    fs::write(dir.join(".hidden"), "hidden evidence").unwrap();
    fs::create_dir_all(dir.join("node_modules/x")).unwrap();
    fs::write(dir.join("node_modules/x/y.js"), "n").unwrap();
    let skills = load(&fx);
    let (_, cx) = context(&fx);

    let res = call(&skills, "ask-matt", &cx).await;
    assert_eq!(res.status, ToolStatus::Ok, "{res:?}");
    assert!(res.output.contains("Body of ask-matt."));
    assert!(
        !res.output.contains("description:"),
        "the header is not repeated"
    );
    let canonical = manifest.canonicalize().unwrap();
    assert_eq!(res.metadata["path"], json!(canonical));
    assert_eq!(res.metadata["base_dir"], json!(canonical.parent().unwrap()));
    assert_eq!(
        res.metadata["resources"],
        json!([
            ".hidden",
            "node_modules/x/y.js",
            "notes.md",
            "scripts/run.sh"
        ])
    );
    assert!(res.output.contains("- scripts/run.sh"));
    assert!(res.output.contains(&canonical.display().to_string()));
}

#[tokio::test]
async fn the_tool_takes_the_exact_id_only() {
    let fx = fixture();
    add(&fx.home.join(".agents/skills/a"), "ask-matt", "Ask.");
    let skills = load(&fx);
    let (_, cx) = context(&fx);
    for wrong in ["ask", "ASK-MATT", "ask-matt ", "", "../a", "ask-*"] {
        let res = call(&skills, wrong, &cx).await;
        assert_eq!(res.error.expect("error").code, "skill_unknown", "{wrong:?}");
    }
    let mut registry = Registry::new();
    registry.register(Box::new(SkillTool::new(Arc::new(skills))));
    let err = registry
        .dispatch("skill", json!({}), CancellationToken::new(), &cx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("invalid_args"));
}

#[tokio::test]
async fn the_body_cap_is_an_explicit_error_not_a_cut() {
    let fx = fixture();
    let dir = fx.home.join(".agents/skills/big");
    fs::create_dir_all(&dir).unwrap();
    let at_limit = "x".repeat(MAX_BODY_BYTES);
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: big\ndescription: d\n---\n{at_limit}"),
    )
    .unwrap();
    let over = fx.home.join(".agents/skills/bigger");
    fs::create_dir_all(&over).unwrap();
    fs::write(
        over.join("SKILL.md"),
        format!("---\nname: bigger\ndescription: d\n---\n{at_limit}y"),
    )
    .unwrap();
    let skills = load(&fx);
    let (_, cx) = context(&fx);
    assert_eq!(call(&skills, "big", &cx).await.status, ToolStatus::Ok);
    let res = call(&skills, "bigger", &cx).await;
    assert_eq!(res.error.expect("error").code, "skill_oversized");
    assert!(!res.output.is_empty());
}

#[tokio::test]
async fn resources_are_complete_within_the_validation_bound() {
    let fx = fixture();
    let dir = fx.home.join(".agents/skills/many");
    add(&dir, "many", "Many files.");
    for i in 0..(MAX_RESOURCES + 6) {
        fs::write(dir.join(format!("f{i:03}.txt")), "x").unwrap();
    }
    let skills = load(&fx);
    let (_, cx) = context(&fx);
    let res = call(&skills, "many", &cx).await;
    assert_eq!(
        res.metadata["resources"].as_array().unwrap().len(),
        MAX_RESOURCES + 6
    );
    assert_eq!(res.metadata["resources_omitted"], 0);
}

#[tokio::test]
async fn a_file_that_vanished_or_broke_is_unreadable() {
    let fx = fixture();
    let manifest = add(&fx.home.join(".agents/skills/gone"), "gone", "Soon gone.");
    let skills = load(&fx);
    let (_, cx) = context(&fx);
    fs::write(&manifest, "no longer a skill").unwrap();
    assert_eq!(
        call(&skills, "gone", &cx).await.error.unwrap().code,
        "skill_unreadable"
    );
    fs::remove_file(&manifest).unwrap();
    assert_eq!(
        call(&skills, "gone", &cx).await.error.unwrap().code,
        "skill_unreadable"
    );
}

// Given a skill found at discovery, then its SKILL.md replaced by a link to a
//       file outside the skill roots
// When the tool loads it
// Then it is unreadable and nothing of the outside file is returned
#[tokio::test]
async fn a_manifest_replaced_by_an_escaping_link_is_refused() {
    let fx = fixture();
    let manifest = add(&fx.home.join(".agents/skills/swap"), "swap", "Swapped.");
    let skills = load(&fx);
    let (_, cx) = context(&fx);
    let outside = fx.home.join("outside.md");
    fs::write(
        &outside,
        "---\nname: swap\ndescription: d\n---\nOUTSIDE SECRET\n",
    )
    .unwrap();
    fs::remove_file(&manifest).unwrap();
    std::os::unix::fs::symlink(&outside, &manifest).unwrap();
    let res = call(&skills, "swap", &cx).await;
    assert_eq!(res.error.expect("error").code, "skill_unreadable");
    assert!(!res.output.contains("OUTSIDE SECRET"));
}

// Given a skill, then its directory (and separately an ancestor) replaced by
//       a link to a place outside the skill roots
// When the tool loads it
// Then it is refused
#[tokio::test]
async fn a_directory_or_ancestor_replaced_by_an_escaping_link_is_refused() {
    let fx = fixture();
    let root = fx.home.join(".agents/skills");
    add(&root.join("team/one"), "one", "One.");
    add(&root.join("two"), "two", "Two.");
    let skills = load(&fx);
    let (_, cx) = context(&fx);

    let elsewhere = fx.home.join("elsewhere");
    add(&elsewhere.join("one"), "one", "Planted.");
    fs::write(elsewhere.join("one/loot.txt"), "x").unwrap();
    fs::remove_dir_all(root.join("team")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, root.join("team")).unwrap();
    let res = call(&skills, "one", &cx).await;
    assert_eq!(res.error.expect("error").code, "skill_unreadable");

    let planted = fx.home.join("planted");
    add(&planted, "two", "Planted.");
    fs::remove_dir_all(root.join("two")).unwrap();
    std::os::unix::fs::symlink(&planted, root.join("two")).unwrap();
    let res = call(&skills, "two", &cx).await;
    assert_eq!(res.error.expect("error").code, "skill_unreadable");
}

// Given a skill whose directory is replaced by a link to another directory
//       inside the roots
// When the tool loads it
// Then it is refused too: the file is no longer the one that was found
#[tokio::test]
async fn a_directory_swapped_for_another_one_in_the_roots_is_refused() {
    let fx = fixture();
    let root = fx.home.join(".agents/skills");
    add(&root.join("a"), "a", "A.");
    add(&root.join("b"), "b", "B.");
    let skills = load(&fx);
    let (_, cx) = context(&fx);
    fs::remove_dir_all(root.join("a")).unwrap();
    std::os::unix::fs::symlink(root.join("b"), root.join("a")).unwrap();
    let res = call(&skills, "a", &cx).await;
    assert_eq!(res.error.expect("error").code, "skill_unreadable");
}

// Given a SKILL.md that became a directory
// Then it is unreadable
#[tokio::test]
async fn a_manifest_that_is_no_longer_a_file_is_unreadable() {
    let fx = fixture();
    let manifest = add(&fx.home.join(".agents/skills/x"), "x", "X.");
    let skills = load(&fx);
    let (_, cx) = context(&fx);
    fs::remove_file(&manifest).unwrap();
    fs::create_dir(&manifest).unwrap();
    let res = call(&skills, "x", &cx).await;
    assert_eq!(res.error.expect("error").code, "skill_unreadable");
}

// ---- readable roots ----

fn actor(fx: &Fixture, registry: Registry) -> crate::agent::actor::AgentActor {
    use crate::agent::actor::AgentActor;
    use crate::provider::config::{ProviderConfig, ReasoningHistory};

    let (_, cx) = context(fx);
    AgentActor::new(
        registry,
        cx,
        ProviderConfig {
            base_url: "http://127.0.0.1:1".into(),
            api_key_env: None,
            headers: Default::default(),
            connect_timeout_ms: 100,
            request_timeout_ms: 100,
            catalog_format: None,
        },
        "k".into(),
        "m".into(),
        ReasoningHistory::Preserve,
        None,
        CancellationToken::new(),
    )
}

// Given an actor with a skill in the user's library
// Then read and grep open it, while write, edit and bash roots stay as they were
#[tokio::test]
async fn skill_roots_are_readable_but_never_writable() {
    use crate::tools::{bash::BashTool, native::*};

    let fx = fixture();
    let dir = fx.home.join(".agents/skills/matt/ask-matt");
    let manifest = add(&dir, "ask-matt", "Ask Matt.");
    fs::write(dir.join("ref.txt"), "needle in a skill").unwrap();
    let mut registry = Registry::new();
    registry.register(Box::new(ReadTool));
    registry.register(Box::new(WriteTool));
    registry.register(Box::new(EditTool));
    registry.register(Box::new(GrepTool));
    registry.register(Box::new(BashTool));
    let mut actor = actor(&fx, registry);
    let denied_before = actor
        .registry
        .dispatch(
            "read",
            json!({"path": manifest}),
            CancellationToken::new(),
            &actor.tool_context,
        )
        .await;
    assert!(
        denied_before.is_err(),
        "not readable before the skills are installed"
    );

    actor.set_skills(load(&fx));
    assert!(actor.registry.get("skill").is_some());
    let cx = &actor.tool_context;
    assert_eq!(
        cx.writable_roots,
        std::slice::from_ref(&fx.ws),
        "writes keep their roots"
    );

    let read = actor
        .registry
        .dispatch(
            "read",
            json!({"path": dir.join("ref.txt")}),
            CancellationToken::new(),
            cx,
        )
        .await
        .expect("read");
    assert_eq!(read.output, "needle in a skill");
    let grep = actor
        .registry
        .dispatch(
            "grep",
            json!({"pattern": "needle", "path": dir}),
            CancellationToken::new(),
            cx,
        )
        .await
        .expect("grep");
    assert!(grep.output.contains("needle in a skill"), "{grep:?}");

    for (tool, args) in [
        (
            "write",
            json!({"path": dir.join("new.txt"), "content": "x"}),
        ),
        ("write", json!({"path": manifest, "content": "x"})),
        (
            "edit",
            json!({"path": dir.join("ref.txt"), "old_string": "needle", "new_string": "pin"}),
        ),
    ] {
        let res = actor
            .registry
            .dispatch(tool, args, CancellationToken::new(), cx)
            .await;
        assert!(
            matches!(res, Err(crate::error::ToolError::PathTraversal(_))),
            "{tool}: {res:?}"
        );
    }
    assert_eq!(
        fs::read_to_string(dir.join("ref.txt")).unwrap(),
        "needle in a skill"
    );
    assert!(!dir.join("new.txt").exists());

    // Outside every root stays outside.
    let outside = fx.home.join("secret.txt");
    fs::write(&outside, "s").unwrap();
    assert!(actor
        .registry
        .dispatch(
            "read",
            json!({"path": outside}),
            CancellationToken::new(),
            cx
        )
        .await
        .is_err());
}

// Given no skills
// Then the `skill` tool is not registered and the roots stay empty
#[test]
fn without_skills_nothing_is_registered() {
    let fx = fixture();
    let mut actor = actor(&fx, Registry::new());
    actor.set_skills(load(&fx));
    assert!(actor.registry.get("skill").is_none());
    assert!(actor.tool_context.readable_roots.is_empty());
    assert!(actor.skills.catalog().is_none());
}

// Given a skill whose SKILL.md passed the path check, then swapped for a link
//       to a file outside the skill roots before it is opened
// When the manifest is opened for reading
// Then the opened file itself is checked and refused, so nothing outside is read
#[test]
fn a_manifest_swapped_between_check_and_open_is_refused() {
    let fx = fixture();
    let root = fx.home.join(".agents/skills");
    let manifest = add(&root.join("race"), "race", "Race.");
    let skills = load(&fx);
    let manifest = fs::canonicalize(&manifest).unwrap();
    assert!(skills.snapshot("race").is_ok());

    let outside = fx.home.join("outside-race.md");
    fs::write(&outside, "---\nname: race\ndescription: d\n---\nOUTSIDE\n").unwrap();
    fs::remove_file(&manifest).unwrap();
    std::os::unix::fs::symlink(&outside, &manifest).unwrap();
    let err = skills.snapshot("race").unwrap_err();
    assert!(err.contains("symbolic link"), "{err}");
}
