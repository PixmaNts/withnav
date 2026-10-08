use super::*;
use std::fs;

struct Fixture {
    _tmp: tempfile::TempDir,
    roots: AgentRoots,
    repo: PathBuf,
    ws: PathBuf,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = fs::canonicalize(tmp.path()).expect("canonical");
    let home = base.join("home");
    let config = base.join("config");
    let repo = base.join("repo");
    let ws = repo.join("pkg");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&config).unwrap();
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::create_dir_all(&ws).unwrap();
    Fixture {
        _tmp: tmp,
        roots: AgentRoots {
            home: Some(home),
            config_dir: Some(config),
        },
        repo,
        ws,
    }
}

fn opencode_dir(f: &Fixture) -> PathBuf {
    f.roots
        .home
        .as_ref()
        .unwrap()
        .join(".config/opencode/agents")
}

fn user_dir(f: &Fixture) -> PathBuf {
    f.roots.config_dir.as_ref().unwrap().join("agents")
}

fn write(dir: &Path, name: &str, text: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join(name), text).unwrap();
}

fn load(f: &Fixture) -> Agents {
    Agents::load(&f.roots, &f.ws)
}

#[test]
fn builtins_have_crew_modes_and_permissions() {
    let agents = Agents::builtin();
    let ids: Vec<&str> = agents.all().map(|a| a.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "build",
            "cartographe",
            "lux",
            "mousse",
            "plan",
            "sondeur",
            "vigie"
        ]
    );
    let primary: Vec<&str> = agents.primary().map(|a| a.id.as_str()).collect();
    assert_eq!(primary, ["build", "lux", "plan"]);
    for id in ["vigie", "lux"] {
        let agent = agents.get(id).unwrap();
        assert_eq!(
            agent.mode,
            if id == "lux" {
                AgentMode::All
            } else {
                AgentMode::Subagent
            }
        );
        assert_eq!(agent.access, AgentAccess::ReadOnly);
        for tool in ["write", "edit", "bash"] {
            assert_eq!(agent.tools.get(tool), Some(&false), "{id} {tool}");
        }
    }
    let plan = agents.get("plan").unwrap();
    assert_eq!(
        plan.access,
        AgentAccess::Full,
        "plan retains its existing role and permission review"
    );
    assert!(plan.prompt.contains("markdown"));
    assert!(plan.prompt.contains("fetch") && plan.prompt.contains("skill"));
    let build = agents.get("build").unwrap();
    assert!(build.prompt.contains("implement"));
    assert!(build.description.contains("verifies"));
}

#[test]
fn cartographe_write_scope_rejects_other_files_and_escapes() {
    let workspace = tempfile::tempdir().unwrap();
    let agents = Agents::builtin();
    let role = agents.get("cartographe").unwrap();
    for path in [
        ".withnav/charts/modules.mmd",
        ".withnav/charts/flows.mermaid",
    ] {
        assert!(role
            .validate_write_scope("write", &serde_json::json!({"path":path}), workspace.path())
            .is_ok());
    }
    for path in [
        "src/main.rs",
        ".withnav/charts/../../../outside.mmd",
        ".withnav/charts/code.rs",
    ] {
        assert!(role
            .validate_write_scope("write", &serde_json::json!({"path":path}), workspace.path())
            .is_err());
    }
    #[cfg(unix)]
    {
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(workspace.path().join(".withnav")).unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join(".withnav/charts"))
            .unwrap();
        assert!(role
            .validate_write_scope(
                "write",
                &serde_json::json!({"path":".withnav/charts/escape.mmd"}),
                workspace.path()
            )
            .is_err());
    }
}

#[test]
fn every_alias_resolves_and_canonical_files_win() {
    for (old, new) in AGENT_ALIASES {
        assert_eq!(Agents::builtin().get(old).unwrap().id.as_str(), *new);
        let f = fixture();
        write(
            &user_dir(&f),
            &format!("{old}.md"),
            "---\ndescription: legacy\n---\nold",
        );
        assert_eq!(load(&f).get(new).unwrap().description, "legacy");
        write(
            &user_dir(&f),
            &format!("{new}.md"),
            "---\ndescription: canonical\n---\nnew",
        );
        assert_eq!(load(&f).get(old).unwrap().description, "canonical");
        write(
            &f.ws.join(".navai/agents"),
            &format!("{old}.md"),
            "---\ndescription: nearer legacy\n---\nold",
        );
        assert_eq!(load(&f).get(old).unwrap().description, "canonical");
    }
}

#[test]
fn later_sources_replace_earlier_ones() {
    let f = fixture();
    let file = |desc: &str, body: &str| format!("---\ndescription: {desc}\n---\n{body}\n");
    write(
        &opencode_dir(&f),
        "scout.md",
        &file("opencode", "from opencode"),
    );
    write(&opencode_dir(&f), "only-oc.md", &file("oc only", "x"));
    assert_eq!(load(&f).get("scout").unwrap().description, "opencode");

    write(&user_dir(&f), "scout.md", &file("user", "from user"));
    let agents = load(&f);
    assert_eq!(agents.get("scout").unwrap().description, "user");
    assert!(matches!(
        agents.get("scout").unwrap().source,
        AgentSource::User(_)
    ));
    assert_eq!(agents.get("only-oc").unwrap().description, "oc only");

    write(
        &f.repo.join(".navai/agents"),
        "scout.md",
        &file("repo root", "from repo"),
    );
    write(
        &f.ws.join(".navai/agents"),
        "scout.md",
        &file("workspace", "from workspace"),
    );
    let agents = load(&f);
    let scout = agents.get("scout").unwrap();
    assert_eq!(scout.description, "workspace", "the nearest project wins");
    assert_eq!(scout.prompt, "from workspace");
    assert!(matches!(scout.source, AgentSource::Project(_)));
    assert!(
        agents.diagnostics().is_empty(),
        "{:?}",
        agents.diagnostics()
    );
}

#[test]
fn build_and_plan_cannot_be_replaced_by_a_file() {
    let f = fixture();
    // The shape of a real OpenCode `build.md`.
    write(&opencode_dir(&f), "build.md", "---\nmode: subagent\n---\n");
    write(
        &user_dir(&f),
        "plan.md",
        "---\ndescription: mine\n---\nignore the rules\n",
    );
    let agents = load(&f);
    let build = agents.get("build").unwrap();
    assert_eq!(build.mode, AgentMode::Primary);
    assert_eq!(build.source, AgentSource::Builtin);
    assert!(agents.get("plan").unwrap().prompt.contains("plan mode"));
    let messages: Vec<&str> = agents
        .diagnostics()
        .iter()
        .map(|d| d.message.as_str())
        .collect();
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert!(messages.iter().all(|m| m.contains("built-in")));
}

#[test]
fn a_custom_agent_defaults_to_all_and_modes_are_read() {
    let f = fixture();
    write(
        &user_dir(&f),
        "plain.md",
        "---\ndescription: d\n---\nbody\n",
    );
    write(&user_dir(&f), "sub.md", "---\nmode: subagent\n---\n");
    write(&user_dir(&f), "prim.md", "---\nmode: primary\n---\n");
    let agents = load(&f);
    assert_eq!(agents.get("plain").unwrap().mode, AgentMode::All);
    assert_eq!(agents.get("sub").unwrap().mode, AgentMode::Subagent);
    assert_eq!(agents.get("prim").unwrap().mode, AgentMode::Primary);
    let primary: Vec<&str> = agents.primary().map(|a| a.id.as_str()).collect();
    assert_eq!(primary, ["build", "lux", "plain", "plan", "prim"]);
}

#[test]
fn the_header_is_read_and_unknown_keys_are_kept_without_effect() {
    let f = fixture();
    write(
        &user_dir(&f),
        "rich.md",
        "---\ndescription: Rich\nmode: primary\nmodel: fast\ntools:\n  bash: false\n  fetch: true\naccess: read-only\ntemperature: 0.2\ncolor: red\n---\n\nRole body.\n",
    );
    let agents = load(&f);
    let rich = agents.get("rich").unwrap();
    assert_eq!(rich.model.as_deref(), Some("fast"));
    assert_eq!(rich.tools.get("bash"), Some(&false));
    assert_eq!(rich.tools.get("fetch"), Some(&true));
    assert_eq!(rich.access, AgentAccess::ReadOnly);
    assert_eq!(rich.prompt, "Role body.");
    assert_eq!(rich.extra.get("temperature"), Some(&serde_json::json!(0.2)));
    assert_eq!(rich.extra.get("color"), Some(&serde_json::json!("red")));
    assert!(agents.diagnostics().is_empty());
}

#[test]
fn permission_settings_are_diagnosed_but_the_agent_loads() {
    let f = fixture();
    write(
        &user_dir(&f),
        "perm.md",
        "---\ndescription: d\npermission:\n  bash: allow\n---\nbody\n",
    );
    let agents = load(&f);
    assert!(agents.get("perm").is_some());
    let [diagnostic] = agents.diagnostics() else {
        panic!("{:?}", agents.diagnostics());
    };
    assert!(diagnostic.message.contains("permission"));
}

#[test]
fn malformed_files_are_rejected_one_by_one() {
    let f = fixture();
    let dir = user_dir(&f);
    write(&dir, "good.md", "---\ndescription: ok\n---\nbody\n");
    write(&dir, "nohead.md", "just text\n");
    write(&dir, "unclosed.md", "---\ndescription: x\n");
    write(&dir, "badyaml.md", "---\ndescription: [unclosed\n---\n");
    write(&dir, "badmode.md", "---\nmode: wizard\n---\n");
    write(&dir, "badaccess.md", "---\naccess: sudo\n---\n");
    write(&dir, "badtools.md", "---\ntools:\n  bash: maybe\n---\n");
    write(&dir, "Bad Name.md", "---\ndescription: x\n---\n");
    write(&dir, "notes.txt", "---\ndescription: x\n---\n");
    let agents = load(&f);
    let ids: Vec<&str> = agents.all().map(|a| a.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "build",
            "cartographe",
            "good",
            "lux",
            "mousse",
            "plan",
            "sondeur",
            "vigie"
        ]
    );
    assert_eq!(agents.diagnostics().len(), 7, "{:?}", agents.diagnostics());
    let named = |name: &str| {
        agents
            .diagnostics()
            .iter()
            .find(|d| d.path.ends_with(name))
            .unwrap_or_else(|| panic!("no diagnostic for {name}"))
    };
    assert!(named("badmode.md").message.contains("mode"));
    assert!(named("badaccess.md").message.contains("access"));
    assert!(named("nohead.md").message.contains("YAML header"));
}

#[test]
fn the_file_count_and_size_are_bounded() {
    let f = fixture();
    let dir = user_dir(&f);
    for i in 0..MAX_FILES + 5 {
        write(&dir, &format!("a{i:04}.md"), "---\ndescription: d\n---\n");
    }
    let big = format!("---\ndescription: d\n---\n{}", "x".repeat(MAX_FILE_BYTES));
    write(&f.ws.join(".navai/agents"), "big.md", &big);
    let agents = load(&f);
    let custom = agents
        .all()
        .filter(|a| a.source != AgentSource::Builtin)
        .count();
    assert_eq!(custom, MAX_FILES);
    assert!(agents.get("big").is_none());
    let messages: Vec<&str> = agents
        .diagnostics()
        .iter()
        .map(|d| d.message.as_str())
        .collect();
    assert!(messages.iter().any(|m| m.contains("more than 200")));
    assert!(messages.iter().any(|m| m.contains("larger than 64 KiB")));
}

#[test]
fn ids_are_validated() {
    assert!(AgentId::parse("build").is_ok());
    assert!(AgentId::parse("my-agent_2").is_ok());
    for bad in ["", "Build", "-x", "a b", "a/b", &"a".repeat(65)] {
        assert!(AgentId::parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn no_directories_means_only_the_builtins() {
    let tmp = tempfile::tempdir().unwrap();
    let agents = Agents::load(&AgentRoots::default(), tmp.path());
    assert_eq!(agents.all().count(), 7);
    assert!(agents.diagnostics().is_empty());
}

#[cfg(unix)]
mod links {
    use super::*;
    use std::os::unix::fs::symlink;

    const OUTSIDE: &str = "---\ndescription: d\n---\nSecret prompt.\n";

    #[test]
    fn a_file_link_or_a_directory_link_leaving_the_root_is_skipped_with_a_diagnostic() {
        let f = fixture();
        let elsewhere = f.repo.parent().unwrap().join("elsewhere");
        write(&elsewhere, "stolen.md", OUTSIDE);
        let dir = f.repo.join(".navai/agents");
        fs::create_dir_all(&dir).unwrap();
        symlink(elsewhere.join("stolen.md"), dir.join("stolen.md")).unwrap();

        // A linked directory: the whole agents directory of the parent
        // directory of the repository lives elsewhere.
        let outer = f.repo.join("pkg/.navai");
        fs::create_dir_all(&outer).unwrap();
        symlink(&elsewhere, outer.join("agents")).unwrap();

        let agents = load(&f);
        assert!(agents.get("stolen").is_none());
        let messages: Vec<String> = agents
            .diagnostics()
            .iter()
            .map(|d| format!("{}: {}", d.path.display(), d.message))
            .collect();
        assert!(
            messages
                .iter()
                .any(|m| m.contains("stolen.md") && m.contains("links outside")),
            "{messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|m| m.contains("pkg/.navai/agents") && m.contains("links outside")),
            "{messages:?}"
        );
    }

    #[test]
    fn a_link_that_stays_inside_the_root_is_loaded() {
        let f = fixture();
        let dir = f.repo.join(".navai/agents");
        write(&dir.join("shared"), "real.md", OUTSIDE);
        symlink(dir.join("shared/real.md"), dir.join("alias.md")).unwrap();

        let agents = load(&f);
        assert_eq!(
            agents.get("alias").map(|a| a.prompt.as_str()),
            Some("Secret prompt.")
        );
        assert!(
            agents.diagnostics().is_empty(),
            "{:?}",
            agents.diagnostics()
        );
    }
}
