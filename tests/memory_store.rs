//! The memory register through `MemoryStore`'s public functions: items and their append-only
//! versions, projects and their identity, groups, and the links that decide what a session reads.

use std::{path::Path, sync::Arc};

use withnav::memory::{
    Actor, AliasKind, ChangeKind, ChangeStatus, ChangedBy, Error, ItemFilter, ItemStatus, Kind,
    MAX_BODY_CHARS, MAX_REASON_CHARS, MAX_TITLE_CHARS, MAX_WHY_CHARS, MemoryItem, MemoryStore,
    NewItem, Project, ProjectIdentity, Provenance, Relation, Revision, Scope, ScopeKind,
    VersionChange, identity,
};

fn store() -> MemoryStore {
    MemoryStore::in_memory().unwrap()
}

fn captain() -> Provenance {
    Provenance::captain(Some("ses_source".into()), Some(7))
}

fn by_captain() -> ChangedBy {
    ChangedBy::new(Actor::Captain, None)
}

fn by_agent() -> ChangedBy {
    ChangedBy::new(Actor::Agent, Some("ses_agent".into()))
}

fn note(scope: Scope, body: &str) -> NewItem {
    NewItem {
        kind: Kind::Decision,
        scope,
        title: None,
        body: body.into(),
        why: None,
        quote: None,
        by: captain(),
    }
}

fn remember(store: &MemoryStore, scope: Scope, body: &str) -> MemoryItem {
    store.remember(note(scope, body)).unwrap()
}

fn reword(body: &str) -> Revision {
    Revision {
        title: None,
        body: Some(body.into()),
        why: None,
    }
}

/// A project known by a folder path, as one outside git is.
fn project(store: &MemoryStore, name: &str) -> Project {
    store
        .resolve_project(&ProjectIdentity {
            alias: format!("/work/{name}"),
            kind: AliasKind::Path,
            name: name.into(),
        })
        .unwrap()
        .project
}

fn bodies(items: &[MemoryItem]) -> Vec<&str> {
    let mut bodies: Vec<&str> = items
        .iter()
        .map(|item| item.current.body.as_str())
        .collect();
    bodies.sort_unstable();
    bodies
}

fn names(projects: &[Project]) -> Vec<&str> {
    let mut names: Vec<&str> = projects
        .iter()
        .map(|project| project.name.as_str())
        .collect();
    names.sort_unstable();
    names
}

fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// A repository with one commit, so that a worktree can be added and a clone has a branch.
fn repository(root: &Path, name: &str) -> std::path::PathBuf {
    let path = root.join(name);
    std::fs::create_dir(&path).unwrap();
    git(&path, &["init", "--quiet"]);
    git(
        &path,
        &["commit", "--quiet", "--allow-empty", "-m", "first"],
    );
    path
}

#[test]
fn a_remembered_item_is_active_at_once_with_its_source() {
    let store = store();

    let item = store
        .remember(NewItem {
            kind: Kind::Preference,
            scope: Scope::Global,
            title: None,
            body: "  Prefers tmux for long builds.\nOne session per role.  ".into(),
            why: Some("He attaches to watch them".into()),
            quote: None,
            by: captain(),
        })
        .unwrap();

    assert_eq!(item.status, ItemStatus::Active);
    assert_eq!(item.kind, Kind::Preference);
    assert_eq!(item.scope, Scope::Global);
    assert_eq!(item.current_version, 1);
    assert!(!item.pinned);
    let version = &item.current;
    assert_eq!(version.version, 1);
    assert_eq!(version.change, VersionChange::Created);
    assert_eq!(
        version.body,
        "Prefers tmux for long builds.\nOne session per role."
    );
    assert_eq!(version.title, "Prefers tmux for long builds.");
    assert_eq!(version.why.as_deref(), Some("He attaches to watch them"));
    assert_eq!(version.source_actor, Actor::Captain);
    assert_eq!(version.drafted_by, Actor::Captain);
    assert_eq!(version.confirmed_by, Some(Actor::Captain));
    assert_eq!(version.source_session_id.as_deref(), Some("ses_source"));
    assert_eq!(version.source_event_seq, Some(7));
    assert_eq!(version.reason, None);
    assert_eq!(version.at, item.created_at);
    assert_eq!(store.item(&item.id).unwrap(), Some(item));
}

#[test]
fn a_revision_keeps_every_earlier_version() {
    let store = store();
    let first = remember(&store, Scope::Global, "Release from develop");

    let second = store
        .revise(
            &first.id,
            1,
            Revision {
                title: Some("Release branch".into()),
                body: Some("Release from main".into()),
                why: Some("develop is the integration branch".into()),
            },
            Some("wrong branch"),
            &Provenance::captain(Some("ses_other".into()), None),
        )
        .unwrap();
    let third = store
        .revise(
            &first.id,
            2,
            reword("Release from main, tagged"),
            None,
            &captain(),
        )
        .unwrap();

    assert_eq!(second.current_version, 2);
    assert_eq!(third.current_version, 3);
    assert_eq!(third.created_at, first.created_at);
    let versions = store.versions(&first.id).unwrap();
    assert_eq!(
        versions
            .iter()
            .map(|version| (version.version, version.change, version.body.as_str()))
            .collect::<Vec<_>>(),
        [
            (1, VersionChange::Created, "Release from develop"),
            (2, VersionChange::Updated, "Release from main"),
            (3, VersionChange::Updated, "Release from main, tagged"),
        ]
    );
    assert_eq!(versions[0].title, "Release from develop");
    assert_eq!(versions[1].title, "Release branch");
    assert_eq!(versions[1].reason.as_deref(), Some("wrong branch"));
    assert_eq!(versions[1].source_session_id.as_deref(), Some("ses_other"));
    // What a revision leaves out stays as it was.
    assert_eq!(versions[2].title, "Release branch");
    assert_eq!(
        versions[2].why.as_deref(),
        Some("develop is the integration branch")
    );
    assert_eq!(versions[2], third.current);
}

#[test]
fn a_stale_expected_version_is_refused_and_writes_nothing() {
    let store = store();
    let item = remember(&store, Scope::Global, "first wording");
    let current = store
        .revise(&item.id, 1, reword("second wording"), None, &captain())
        .unwrap();

    let stale = [
        store.revise(&item.id, 1, reword("late wording"), None, &captain()),
        store.invalidate(&item.id, 1, None, &captain()),
        store.restore(&item.id, 1, Some(1), None, &captain()),
        store.rescope(
            &item.id,
            1,
            Scope::Session("ses_a".into()),
            true,
            None,
            &captain(),
        ),
        store.revise(&item.id, 3, reword("from the future"), None, &captain()),
    ];

    for refused in stale {
        assert!(
            matches!(refused, Err(Error::VersionConflict { current: 2 })),
            "{refused:?}"
        );
    }
    assert_eq!(store.item(&item.id).unwrap(), Some(current));
    assert_eq!(store.versions(&item.id).unwrap().len(), 2);
}

#[test]
fn two_writers_cannot_both_supersede_the_same_version() {
    let data = tempfile::tempdir().unwrap();
    let first = MemoryStore::open(data.path()).unwrap();
    let second = MemoryStore::open(data.path()).unwrap();
    let item = remember(&first, Scope::Global, "shared wording");
    let start = std::sync::Barrier::new(2);

    let outcomes = std::thread::scope(|scope| {
        let writers =
            [(&first, "first writer"), (&second, "second writer")].map(|(store, body)| {
                let (id, start) = (&item.id, &start);
                scope.spawn(move || {
                    start.wait();
                    store.revise(id, 1, reword(body), None, &captain())
                })
            });
        writers.map(|writer| writer.join().unwrap())
    });

    let won: Vec<&MemoryItem> = outcomes.iter().filter_map(|o| o.as_ref().ok()).collect();
    assert_eq!(won.len(), 1, "{outcomes:?}");
    assert!(
        outcomes
            .iter()
            .any(|outcome| matches!(outcome, Err(Error::VersionConflict { current: 2 }))),
        "{outcomes:?}"
    );
    let versions = second.versions(&item.id).unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[1].body, won[0].current.body);
}

#[test]
fn an_invalidated_item_leaves_search_and_keeps_its_history() {
    let store = store();
    let item = remember(&store, Scope::Global, "Deploy on Fridays");

    let invalidated = store
        .invalidate(&item.id, 1, Some("never again"), &captain())
        .unwrap();

    assert_eq!(invalidated.status, ItemStatus::Invalidated);
    assert_eq!(invalidated.current_version, 2);
    assert_eq!(invalidated.current.change, VersionChange::Invalidated);
    assert_eq!(invalidated.current.reason.as_deref(), Some("never again"));
    assert_eq!(invalidated.current.body, "Deploy on Fridays");
    assert!(store.search("fridays", None).unwrap().is_empty());
    assert!(store.eligible("ses_any").unwrap().is_empty());
    assert_eq!(store.versions(&item.id).unwrap().len(), 2);
    let kept = store
        .list(&ItemFilter {
            status: Some(ItemStatus::Invalidated),
            ..ItemFilter::default()
        })
        .unwrap();
    assert_eq!(kept, [invalidated]);
    // What is no longer current cannot be reworded, invalidated twice or moved.
    assert!(matches!(
        store.revise(&item.id, 2, reword("Deploy on Mondays"), None, &captain()),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        store.invalidate(&item.id, 2, None, &captain()),
        Err(Error::Invalid(_))
    ));
    assert_eq!(store.versions(&item.id).unwrap().len(), 2);
}

#[test]
fn restore_adds_a_version() {
    let store = store();
    let item = remember(&store, Scope::Global, "Squash before merging");
    store.invalidate(&item.id, 1, None, &captain()).unwrap();

    let restored = store
        .restore(&item.id, 2, None, Some("still true"), &captain())
        .unwrap();

    assert_eq!(restored.status, ItemStatus::Active);
    assert_eq!(restored.current_version, 3);
    assert_eq!(restored.current.change, VersionChange::Restored);
    assert_eq!(restored.current.body, "Squash before merging");
    assert_eq!(restored.current.reason.as_deref(), Some("still true"));
    assert_eq!(
        bodies(&store.search("squash", None).unwrap()),
        ["Squash before merging"]
    );
    assert_eq!(store.versions(&item.id).unwrap().len(), 3);
    // An item that is current has nothing to restore unless an earlier wording is named.
    assert!(matches!(
        store.restore(&item.id, 3, None, None, &captain()),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn restore_can_bring_back_an_earlier_wording() {
    let store = store();
    let item = remember(&store, Scope::Global, "Rebase, never merge");
    store
        .revise(&item.id, 1, reword("Merge, never rebase"), None, &captain())
        .unwrap();

    let undone = store
        .restore(&item.id, 2, Some(1), None, &captain())
        .unwrap();

    assert_eq!(undone.status, ItemStatus::Active);
    assert_eq!(undone.current_version, 3);
    assert_eq!(undone.current.change, VersionChange::Restored);
    assert_eq!(undone.current.body, "Rebase, never merge");
    assert!(matches!(
        store.restore(&item.id, 3, Some(9), None, &captain()),
        Err(Error::NotFound { .. })
    ));
}

#[test]
fn versions_cannot_be_updated_or_deleted() {
    let data = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(data.path()).unwrap();
    let item = remember(&store, Scope::Global, "as written");
    let raw = rusqlite::Connection::open(data.path().join("withnav.db")).unwrap();

    let update = raw.execute("UPDATE memory_versions SET body = 'rewritten'", []);
    let delete = raw.execute("DELETE FROM memory_versions", []);

    for refused in [update, delete] {
        let message = refused.unwrap_err().to_string();
        assert!(
            message.contains("memory versions are append-only"),
            "{message}"
        );
    }
    assert_eq!(store.versions(&item.id).unwrap()[0].body, "as written");
}

#[test]
fn search_reads_current_versions_only() {
    let store = store();
    let item = remember(&store, Scope::Global, "The staging server is alpha");
    let revised = store
        .revise(
            &item.id,
            1,
            reword("The staging server is BRAVO"),
            None,
            &captain(),
        )
        .unwrap();
    // A title that was only the first line follows the body, so the old wording is gone from
    // everything the current version shows.
    assert_eq!(revised.current.title, "The staging server is BRAVO");
    remember(
        &store,
        Scope::Global,
        "Décision : 100% des tests avant merge",
    );
    remember(&store, Scope::Global, "Use snake_case names");

    assert!(store.search("alpha", None).unwrap().is_empty());
    assert_eq!(
        bodies(&store.search("bravo", None).unwrap()),
        ["The staging server is BRAVO"]
    );
    // Case is ignored beyond ASCII: the Captain writes French.
    assert_eq!(
        bodies(&store.search("DÉCISION", None).unwrap()),
        ["Décision : 100% des tests avant merge"]
    );
    // The text is matched as written, never as a pattern.
    assert_eq!(
        bodies(&store.search("100%", None).unwrap()),
        ["Décision : 100% des tests avant merge"]
    );
    assert!(store.search("snake%case", None).unwrap().is_empty());
    assert_eq!(
        bodies(&store.search("snake_case", None).unwrap()),
        ["Use snake_case names"]
    );
    assert!(store.search("snakeXcase", None).unwrap().is_empty());
    assert!(matches!(store.search("  ", None), Err(Error::Invalid(_))));
}

#[test]
fn search_for_a_session_stays_in_what_it_may_read() {
    let store = store();
    let near = project(&store, "near");
    let far = project(&store, "far");
    store
        .attach_session("ses_a", &near.id, &by_captain())
        .unwrap();
    remember(
        &store,
        Scope::Project(near.id.clone()),
        "cache lives in near",
    );
    remember(&store, Scope::Project(far.id.clone()), "cache lives in far");

    assert_eq!(
        bodies(&store.search("cache", Some("ses_a")).unwrap()),
        ["cache lives in near"]
    );
    assert_eq!(store.search("cache", None).unwrap().len(), 2);
}

#[tokio::test]
async fn linked_worktrees_resolve_to_one_project() {
    let root = tempfile::tempdir().unwrap();
    let main = repository(root.path(), "main");
    let linked = root.path().join("linked");
    git(
        &main,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "side",
            linked.to_str().unwrap(),
        ],
    );
    let nested = main.join("src");
    std::fs::create_dir(&nested).unwrap();
    let store = store();

    let from_main = identity(&main).await.unwrap();
    let first = store.resolve_project(&from_main).unwrap();
    let second = store
        .resolve_project(&identity(&linked).await.unwrap())
        .unwrap();
    let third = store
        .resolve_project(&identity(&nested).await.unwrap())
        .unwrap();

    assert_eq!(from_main.kind, AliasKind::GitCommonDir);
    assert_eq!(
        from_main.alias,
        std::fs::canonicalize(main.join(".git"))
            .unwrap()
            .to_str()
            .unwrap()
    );
    assert!(first.created);
    assert!(!second.created && !third.created);
    assert_eq!(first.project.name, "main");
    assert_eq!(second.project.id, first.project.id);
    assert_eq!(third.project.id, first.project.id);
    assert_eq!(store.projects().unwrap().len(), 1);
    assert_eq!(first.project.aliases.len(), 1);
    assert_eq!(first.project.aliases[0].alias, from_main.alias);
}

#[tokio::test]
async fn a_separate_clone_is_another_project() {
    let root = tempfile::tempdir().unwrap();
    let main = repository(root.path(), "main");
    let clone = root.path().join("clone");
    git(
        root.path(),
        &[
            "clone",
            "--quiet",
            main.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    let store = store();

    let first = store
        .resolve_project(&identity(&main).await.unwrap())
        .unwrap();
    let second = store
        .resolve_project(&identity(&clone).await.unwrap())
        .unwrap();

    assert!(first.created && second.created);
    assert_ne!(first.project.id, second.project.id);
    assert_eq!(names(&store.projects().unwrap()), ["clone", "main"]);
}

#[tokio::test]
async fn a_folder_outside_git_is_its_own_project() {
    let root = tempfile::tempdir().unwrap();
    let folder = root.path().join("notes");
    let inner = folder.join("inner");
    std::fs::create_dir_all(&inner).unwrap();
    let store = store();

    let found = identity(&folder).await.unwrap();
    let first = store.resolve_project(&found).unwrap();
    let again = store
        .resolve_project(&identity(&folder).await.unwrap())
        .unwrap();
    let other = store
        .resolve_project(&identity(&inner).await.unwrap())
        .unwrap();

    assert_eq!(found.kind, AliasKind::Path);
    assert_eq!(
        found.alias,
        std::fs::canonicalize(&folder).unwrap().to_str().unwrap()
    );
    assert_eq!(first.project.name, "notes");
    assert!(first.created && !again.created && other.created);
    assert_eq!(again.project.id, first.project.id);
    assert_ne!(other.project.id, first.project.id);
    assert!(identity(&root.path().join("missing")).await.is_err());
}

#[test]
fn a_session_has_one_active_project() {
    let store = store();
    let first = project(&store, "first");
    let second = project(&store, "second");
    assert_eq!(store.session_project("ses_a").unwrap(), None);

    store
        .attach_session("ses_a", &first.id, &by_captain())
        .unwrap();
    store
        .attach_session("ses_a", &second.id, &by_captain())
        .unwrap();
    store
        .attach_session("ses_b", &first.id, &by_captain())
        .unwrap();

    assert_eq!(
        store.session_project("ses_a").unwrap().unwrap().id,
        second.id
    );
    let links = store.session_links("ses_a").unwrap();
    assert_eq!(links.len(), 2);
    assert_eq!(
        links
            .iter()
            .filter(|link| link.active)
            .map(|link| link.project_id.as_str())
            .collect::<Vec<_>>(),
        [second.id.as_str()]
    );
    // Going back to a project the session was linked to reuses that link.
    store
        .attach_session("ses_a", &first.id, &by_captain())
        .unwrap();
    assert_eq!(
        store.session_project("ses_a").unwrap().unwrap().id,
        first.id
    );
    assert_eq!(store.session_links("ses_a").unwrap().len(), 2);
    assert!(matches!(
        store.attach_session("ses_a", &first.id, &by_captain()),
        Err(Error::Invalid(_))
    ));
    let mut working: Vec<String> = store
        .project_sessions(&first.id)
        .unwrap()
        .into_iter()
        .filter(|link| link.active)
        .map(|link| link.session_id)
        .collect();
    working.sort_unstable();
    assert_eq!(working, ["ses_a", "ses_b"]);

    store.detach_session("ses_a", &by_captain()).unwrap();

    assert_eq!(store.session_project("ses_a").unwrap(), None);
    assert!(matches!(
        store.detach_session("ses_a", &by_captain()),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        store.attach_session("ses_a", "no-such-project", &by_captain()),
        Err(Error::NotFound { .. })
    ));
}

#[test]
fn a_session_reads_its_own_items_its_project_its_groups_and_global() {
    let store = store();
    let here = project(&store, "here");
    let elsewhere = project(&store, "elsewhere");
    let shared = store.create_group("kyber").unwrap();
    let foreign = store.create_group("infra").unwrap();
    store
        .add_project_to_group(&here.id, &shared.id, true, &by_captain())
        .unwrap();
    store
        .add_project_to_group(&elsewhere.id, &foreign.id, true, &by_captain())
        .unwrap();
    store
        .attach_session("ses_a", &here.id, &by_captain())
        .unwrap();
    remember(&store, Scope::Global, "global");
    remember(&store, Scope::Group(shared.id.clone()), "own group");
    remember(&store, Scope::Group(foreign.id.clone()), "foreign group");
    remember(&store, Scope::Project(here.id.clone()), "own project");
    remember(
        &store,
        Scope::Project(elsewhere.id.clone()),
        "other project",
    );
    remember(&store, Scope::Session("ses_a".into()), "own session");
    remember(&store, Scope::Session("ses_b".into()), "other session");

    assert_eq!(
        bodies(&store.eligible("ses_a").unwrap()),
        ["global", "own group", "own project", "own session"]
    );
    // A session without a project reads only what is its own or everybody's.
    assert_eq!(
        bodies(&store.eligible("ses_b").unwrap()),
        ["global", "other session"]
    );
}

#[test]
fn groups_of_an_inactive_project_are_not_eligible() {
    let store = store();
    let before = project(&store, "before");
    let now = project(&store, "now");
    let group = store.create_group("kyber").unwrap();
    store
        .add_project_to_group(&before.id, &group.id, true, &by_captain())
        .unwrap();
    remember(&store, Scope::Group(group.id.clone()), "kyber convention");
    remember(&store, Scope::Project(before.id.clone()), "before only");
    remember(&store, Scope::Project(now.id.clone()), "now only");
    store
        .attach_session("ses_a", &before.id, &by_captain())
        .unwrap();
    assert_eq!(
        bodies(&store.eligible("ses_a").unwrap()),
        ["before only", "kyber convention"]
    );

    store
        .attach_session("ses_a", &now.id, &by_captain())
        .unwrap();

    assert_eq!(store.session_links("ses_a").unwrap().len(), 2);
    assert_eq!(bodies(&store.eligible("ses_a").unwrap()), ["now only"]);
}

#[test]
fn a_membership_change_is_history_not_an_overwrite() {
    let store = store();
    let project = project(&store, "withnav");
    let group = store.create_group("tools").unwrap();
    let relation = Relation::ProjectGroup {
        project_id: project.id.clone(),
        group_id: group.id.clone(),
    };

    let added = store
        .add_project_to_group(&project.id, &group.id, true, &by_captain())
        .unwrap();
    let removed = store
        .remove_project_from_group(&project.id, &group.id, &by_agent())
        .unwrap();
    store
        .add_project_to_group(&project.id, &group.id, true, &by_captain())
        .unwrap();
    store
        .attach_session("ses_a", &project.id, &by_captain())
        .unwrap();
    store.detach_session("ses_a", &by_agent()).unwrap();

    assert_eq!(added.relation, relation);
    assert_eq!(
        (added.change, added.status, added.actor),
        (ChangeKind::Add, ChangeStatus::Applied, Actor::Captain)
    );
    assert!(added.decided_at.is_some());
    assert_eq!(
        (removed.change, removed.status, removed.actor),
        (ChangeKind::Remove, ChangeStatus::Applied, Actor::Agent)
    );
    assert_eq!(removed.session_id.as_deref(), Some("ses_agent"));
    let history = store.membership_history(None, 50).unwrap();
    assert_eq!(
        history
            .iter()
            .map(|change| (
                matches!(change.relation, Relation::ProjectGroup { .. }),
                change.change
            ))
            .collect::<Vec<_>>(),
        // Newest first.
        [
            (false, ChangeKind::Remove),
            (false, ChangeKind::Add),
            (true, ChangeKind::Add),
            (true, ChangeKind::Remove),
            (true, ChangeKind::Add),
        ]
    );
    assert_eq!(history[4], added);
    assert_eq!(store.membership_history(None, 2).unwrap().len(), 2);
    let other = self::project(&store, "other");
    assert!(
        store
            .membership_history(Some(&other.id), 50)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .membership_history(Some(&project.id), 50)
            .unwrap()
            .len(),
        5
    );
    assert_eq!(
        store.group(&group.id).unwrap().unwrap().members,
        std::slice::from_ref(&project.id)
    );
    assert!(matches!(
        store.add_project_to_group(&project.id, &group.id, true, &by_captain()),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        store.remove_project_from_group(&other.id, &group.id, &by_captain()),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn an_over_long_body_is_refused_not_cut() {
    let store = store();
    let long = |count: usize| "é".repeat(count);
    let with = |change: fn(&mut NewItem, String), text: String| {
        let mut new = note(Scope::Global, "kept");
        change(&mut new, text);
        store.remember(new)
    };

    let refused = [
        with(|new, text| new.body = text, long(MAX_BODY_CHARS + 1)),
        with(
            |new, text| new.title = Some(text),
            long(MAX_TITLE_CHARS + 1),
        ),
        with(|new, text| new.why = Some(text), long(MAX_WHY_CHARS + 1)),
        with(|new, text| new.quote = Some(text), long(MAX_BODY_CHARS + 1)),
        with(|new, text| new.body = text, " \n ".into()),
    ];

    let messages: Vec<String> = refused
        .into_iter()
        .map(|refused| match refused {
            Err(Error::Invalid(message)) => message,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        messages,
        [
            "the body is 4001 characters long; the limit is 4000",
            "the title is 201 characters long; the limit is 200",
            "the reason why is 2001 characters long; the limit is 2000",
            "the quote is 4001 characters long; the limit is 4000",
            "the body is empty",
        ]
    );
    assert!(store.list(&ItemFilter::default()).unwrap().is_empty());

    // The limits count characters, and a text at the limit is stored whole.
    let item = with(|new, text| new.body = text, long(MAX_BODY_CHARS)).unwrap();
    assert_eq!(item.current.body.chars().count(), MAX_BODY_CHARS);
    assert_eq!(item.current.title.chars().count(), MAX_TITLE_CHARS);
    let late = [
        store.revise(
            &item.id,
            1,
            reword(&long(MAX_BODY_CHARS + 1)),
            None,
            &captain(),
        ),
        store.invalidate(&item.id, 1, Some(&long(MAX_REASON_CHARS + 1)), &captain()),
        store.revise(
            &item.id,
            1,
            Revision {
                title: None,
                body: None,
                why: None,
            },
            None,
            &captain(),
        ),
    ];
    for refused in late {
        assert!(matches!(refused, Err(Error::Invalid(_))), "{refused:?}");
    }
    assert_eq!(store.item(&item.id).unwrap(), Some(item));
}

#[test]
fn reopening_the_database_keeps_everything() {
    let data = tempfile::tempdir().unwrap();
    let (item, project, group) = {
        let store = MemoryStore::open(data.path()).unwrap();
        let project = project(&store, "withnav");
        let group = store.create_group("tools").unwrap();
        store
            .add_project_to_group(&project.id, &group.id, true, &by_captain())
            .unwrap();
        store
            .attach_session("ses_a", &project.id, &by_captain())
            .unwrap();
        let item = remember(&store, Scope::Project(project.id.clone()), "first");
        store
            .revise(&item.id, 1, reword("second"), None, &captain())
            .unwrap();
        (item, project, group)
    };

    let store = MemoryStore::open(data.path()).unwrap();

    let read = store.item(&item.id).unwrap().unwrap();
    assert_eq!(read.current.body, "second");
    assert_eq!(read.scope, Scope::Project(project.id.clone()));
    assert_eq!(store.versions(&item.id).unwrap().len(), 2);
    let kept = store.project(&project.id).unwrap().unwrap();
    assert_eq!(kept.name, "withnav");
    assert_eq!(kept.groups, std::slice::from_ref(&group.id));
    assert_eq!(kept.aliases[0].alias, "/work/withnav");
    assert_eq!(
        store.session_project("ses_a").unwrap().unwrap().id,
        project.id
    );
    assert_eq!(bodies(&store.eligible("ses_a").unwrap()), ["second"]);
    assert_eq!(store.membership_history(None, 50).unwrap().len(), 2);
    assert!(!self::project(&store, "withnav").id.is_empty());
    assert_eq!(store.projects().unwrap().len(), 1);
}

#[tokio::test]
async fn the_store_answers_from_the_blocking_pool() {
    let store = Arc::new(store());
    let item = store
        .run(|store| store.remember(note(Scope::Global, "kept off the runtime threads")))
        .await
        .unwrap();

    let read = store.run(move |store| store.item(&item.id)).await.unwrap();

    assert_eq!(read.unwrap().current.body, "kept off the runtime threads");
}

#[test]
fn widening_a_scope_needs_confirmation_and_names_the_affected_projects() {
    let store = store();
    let origin = project(&store, "origin");
    let sibling = project(&store, "sibling");
    let outsider = project(&store, "outsider");
    let group = store.create_group("kyber").unwrap();
    for member in [&origin, &sibling] {
        store
            .add_project_to_group(&member.id, &group.id, true, &by_captain())
            .unwrap();
    }
    let item = remember(&store, Scope::Project(origin.id.clone()), "use cqfd");
    let affected = |outcome: Result<MemoryItem, Error>| match outcome {
        Err(Error::WideningRequiresConfirmation { affected_projects }) => affected_projects
            .into_iter()
            .map(|project| project.name)
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    let to_group = Scope::Group(group.id.clone());

    // Unconfirmed: nothing is written, and the answer names who would newly read the item.
    assert_eq!(
        affected(store.rescope(&item.id, 1, to_group.clone(), false, None, &captain())),
        ["sibling"]
    );
    let mut everyone_else =
        affected(store.rescope(&item.id, 1, Scope::Global, false, None, &captain()));
    everyone_else.sort_unstable();
    assert_eq!(everyone_else, ["outsider", "sibling"]);
    assert_eq!(
        affected(store.rescope(
            &item.id,
            1,
            Scope::Project(outsider.id.clone()),
            false,
            None,
            &captain()
        )),
        ["outsider"]
    );
    assert_eq!(store.versions(&item.id).unwrap().len(), 1);

    let widened = store
        .rescope(
            &item.id,
            1,
            to_group.clone(),
            true,
            Some("shared"),
            &captain(),
        )
        .unwrap();

    assert_eq!(widened.scope, to_group);
    assert_eq!(widened.current_version, 2);
    assert_eq!(widened.current.change, VersionChange::Rescoped);
    assert_eq!(widened.current.scope, to_group);
    assert_eq!(widened.current.reason.as_deref(), Some("shared"));
    assert_eq!(widened.current.body, "use cqfd");
    let versions = store.versions(&item.id).unwrap();
    assert_eq!(versions[0].scope, Scope::Project(origin.id.clone()));
    store
        .attach_session("ses_s", &sibling.id, &by_captain())
        .unwrap();
    assert_eq!(bodies(&store.eligible("ses_s").unwrap()), ["use cqfd"]);
}

#[test]
fn narrowing_a_scope_applies_without_confirmation() {
    let store = store();
    let origin = project(&store, "origin");
    let sibling = project(&store, "sibling");
    let group = store.create_group("kyber").unwrap();
    for member in [&origin, &sibling] {
        store
            .add_project_to_group(&member.id, &group.id, true, &by_captain())
            .unwrap();
    }
    let item = remember(&store, Scope::Global, "use cqfd");

    let grouped = store
        .rescope(
            &item.id,
            1,
            Scope::Group(group.id.clone()),
            false,
            None,
            &captain(),
        )
        .unwrap();
    let single = store
        .rescope(
            &item.id,
            2,
            Scope::Project(origin.id.clone()),
            false,
            None,
            &captain(),
        )
        .unwrap();
    let private = store
        .rescope(
            &item.id,
            3,
            Scope::Session("ses_a".into()),
            false,
            None,
            &captain(),
        )
        .unwrap();

    assert_eq!(grouped.scope, Scope::Group(group.id.clone()));
    assert_eq!(single.scope, Scope::Project(origin.id.clone()));
    assert_eq!(private.scope, Scope::Session("ses_a".into()));
    assert_eq!(private.current_version, 4);
    // Leaving the session for the project is a widening again.
    assert!(matches!(
        store.rescope(
            &item.id,
            4,
            Scope::Project(origin.id.clone()),
            false,
            None,
            &captain()
        ),
        Err(Error::WideningRequiresConfirmation { .. })
    ));
    assert!(matches!(
        store.rescope(
            &item.id,
            4,
            Scope::Session("ses_a".into()),
            true,
            None,
            &captain()
        ),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        store.rescope(
            &item.id,
            4,
            Scope::Group("no-such-group".into()),
            true,
            None,
            &captain()
        ),
        Err(Error::NotFound { .. })
    ));
}

#[test]
fn adding_a_project_to_a_group_needs_confirmation() {
    let store = store();
    let member = project(&store, "member");
    let newcomer = project(&store, "newcomer");
    let group = store.create_group("kyber").unwrap();
    store
        .add_project_to_group(&member.id, &group.id, true, &by_captain())
        .unwrap();

    let unconfirmed = store.add_project_to_group(&newcomer.id, &group.id, false, &by_captain());

    match unconfirmed {
        Err(Error::WideningRequiresConfirmation { affected_projects }) => {
            assert_eq!(names(&affected_projects), ["member"]);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        store.group(&group.id).unwrap().unwrap().members,
        std::slice::from_ref(&member.id)
    );
    assert_eq!(store.membership_history(None, 50).unwrap().len(), 1);
    assert!(matches!(
        store.add_project_to_group("no-such-project", &group.id, false, &by_captain()),
        Err(Error::NotFound { .. })
    ));
}

#[test]
fn a_proposed_membership_waits_for_the_captain() {
    let store = store();
    let project = project(&store, "withnav");
    let other = self::project(&store, "other");
    let group = store.create_group("tools").unwrap();

    let proposed = store
        .propose_project_group(&project.id, &group.id, &by_agent())
        .unwrap();
    let again = store
        .propose_project_group(&project.id, &group.id, &by_agent())
        .unwrap();
    let attach = store
        .propose_session_attach("ses_agent", &other.id, &by_agent())
        .unwrap();

    assert_eq!(
        (proposed.change, proposed.status, proposed.actor),
        (ChangeKind::Add, ChangeStatus::Proposed, Actor::Agent)
    );
    assert_eq!(proposed.decided_at, None);
    // Asking twice leaves one question.
    assert_eq!(again.id, proposed.id);
    assert!(store.group(&group.id).unwrap().unwrap().members.is_empty());
    assert_eq!(store.session_project("ses_agent").unwrap(), None);
    let pending = store.pending_membership_changes(5).unwrap();
    assert_eq!(
        pending.iter().map(|change| change.id).collect::<Vec<_>>(),
        [proposed.id, attach.id]
    );
    assert_eq!(store.pending_membership_changes(1).unwrap().len(), 1);

    let accepted = store.accept_membership_change(proposed.id).unwrap();
    let rejected = store.reject_membership_change(attach.id).unwrap();

    assert_eq!(accepted.status, ChangeStatus::Applied);
    assert!(accepted.decided_at.is_some());
    assert_eq!(rejected.status, ChangeStatus::Rejected);
    assert!(rejected.decided_at.is_some());
    assert_eq!(
        store.group(&group.id).unwrap().unwrap().members,
        std::slice::from_ref(&project.id)
    );
    assert_eq!(store.session_project("ses_agent").unwrap(), None);
    assert!(store.pending_membership_changes(5).unwrap().is_empty());
    assert_eq!(store.membership_history(None, 50).unwrap().len(), 2);
    // A question is answered once.
    for decided in [proposed.id, attach.id] {
        assert!(matches!(
            store.accept_membership_change(decided),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            store.reject_membership_change(decided),
            Err(Error::Invalid(_))
        ));
    }
    assert!(matches!(
        store.accept_membership_change(9_999),
        Err(Error::NotFound { .. })
    ));
    assert!(matches!(
        store.propose_project_group(&project.id, &group.id, &by_agent()),
        Err(Error::Invalid(_))
    ));

    let attach = store
        .propose_session_attach("ses_agent", &other.id, &by_agent())
        .unwrap();
    store.accept_membership_change(attach.id).unwrap();
    assert_eq!(
        store.session_project("ses_agent").unwrap().unwrap().id,
        other.id
    );
}

#[test]
fn a_new_project_keeps_its_group_question_open_until_answered() {
    let store = store();
    let alone = project(&store, "alone");
    let grouped = project(&store, "grouped");
    let proposed = project(&store, "proposed");
    let group = store.create_group("tools").unwrap();
    assert!(!alone.group_answered && !grouped.group_answered);

    // "No group" is an answer; so is being put in one.
    let answered = store.answer_group_question(&alone.id).unwrap();
    store
        .add_project_to_group(&grouped.id, &group.id, true, &by_captain())
        .unwrap();
    // A proposal the Captain has not accepted answers nothing.
    let waiting = store
        .propose_project_group(&proposed.id, &group.id, &by_agent())
        .unwrap();

    assert!(answered.group_answered);
    assert!(store.project(&grouped.id).unwrap().unwrap().group_answered);
    assert!(!store.project(&proposed.id).unwrap().unwrap().group_answered);
    store.accept_membership_change(waiting.id).unwrap();
    assert!(store.project(&proposed.id).unwrap().unwrap().group_answered);
    // Resolving the same folder again never reopens the question.
    assert!(project(&store, "alone").group_answered);
    assert!(matches!(
        store.answer_group_question("no-such-project"),
        Err(Error::NotFound { .. })
    ));
}

#[test]
fn a_retired_group_stops_sharing_and_frees_its_name() {
    let store = store();
    let project = project(&store, "withnav");
    let group = store.create_group("tools").unwrap();
    store
        .add_project_to_group(&project.id, &group.id, true, &by_captain())
        .unwrap();
    store
        .attach_session("ses_a", &project.id, &by_captain())
        .unwrap();
    let item = remember(&store, Scope::Group(group.id.clone()), "shared habit");

    let retired = store.retire_group(&group.id).unwrap();

    assert!(retired.retired_at.is_some());
    assert!(store.eligible("ses_a").unwrap().is_empty());
    assert!(
        store
            .project(&project.id)
            .unwrap()
            .unwrap()
            .groups
            .is_empty()
    );
    assert_eq!(store.group_by_name("tools").unwrap(), None);
    // The item and the group stay readable; nothing new joins them.
    assert_eq!(
        store.item(&item.id).unwrap().unwrap().status,
        ItemStatus::Active
    );
    assert_eq!(store.groups().unwrap(), std::slice::from_ref(&retired));
    for refused in [
        store
            .remember(note(Scope::Group(group.id.clone()), "late"))
            .err(),
        store.retire_group(&group.id).err(),
        store.rename_group(&group.id, "other").err(),
        store
            .propose_project_group(&project.id, &group.id, &by_agent())
            .err(),
    ] {
        assert!(matches!(refused, Some(Error::Invalid(_))), "{refused:?}");
    }
    let successor = store.create_group("tools").unwrap();
    assert_ne!(successor.id, group.id);
    assert_eq!(store.group_by_name("TOOLS").unwrap(), Some(successor));
}

#[test]
fn projects_and_groups_are_renamed_in_place() {
    let store = store();
    let project = project(&store, "navai");
    let group = store.create_group("  tools ").unwrap();
    store.create_group("infra").unwrap();
    assert_eq!(group.name, "tools");

    let renamed = store.rename_project(&project.id, " WithNav ").unwrap();
    let regrouped = store.rename_group(&group.id, "Tooling").unwrap();

    assert_eq!(
        (renamed.id.as_str(), renamed.name.as_str()),
        (project.id.as_str(), "WithNav")
    );
    assert_eq!(renamed.aliases, project.aliases);
    assert_eq!(
        (regrouped.id.as_str(), regrouped.name.as_str()),
        (group.id.as_str(), "Tooling")
    );
    assert_eq!(store.group_by_name("tooling").unwrap(), Some(regrouped));
    let long = "n".repeat(101);
    for refused in [
        store.rename_group(&group.id, "INFRA").err(),
        store.create_group("tooling").err(),
        store.create_group(" ").err(),
        store.create_group(&long).err(),
        store.rename_project(&project.id, "").err(),
        store.rename_project(&project.id, &long).err(),
    ] {
        assert!(matches!(refused, Some(Error::Invalid(_))), "{refused:?}");
    }
    assert!(matches!(
        store.rename_group("no-such-group", "x"),
        Err(Error::NotFound { .. })
    ));
    assert!(matches!(
        store.rename_project("no-such-project", "x"),
        Err(Error::NotFound { .. })
    ));
    // A group keeps its own name through a change of case.
    assert_eq!(
        store.rename_group(&group.id, "TOOLING").unwrap().name,
        "TOOLING"
    );
}

#[test]
fn an_item_is_found_by_a_unique_id_prefix() {
    let store = store();
    let item = remember(&store, Scope::Global, "found by its first characters");

    assert_eq!(store.resolve_id(&item.id).unwrap(), item.id);
    assert_eq!(store.resolve_id(&item.id[..6]).unwrap(), item.id);
    assert!(matches!(
        store.resolve_id(&item.id[..5]),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        store.resolve_id("zzzzzzzz"),
        Err(Error::NotFound { .. })
    ));
    // The prefix is matched as written, never as a pattern.
    assert!(matches!(
        store.resolve_id("%%%%%%"),
        Err(Error::NotFound { .. })
    ));
}

#[test]
fn an_item_in_an_unknown_scope_is_refused() {
    let store = store();

    for scope in [
        Scope::Project("no-such-project".into()),
        Scope::Group("no-such-group".into()),
    ] {
        assert!(matches!(
            store.remember(note(scope, "nowhere")),
            Err(Error::NotFound { .. })
        ));
    }
    assert!(matches!(
        store.remember(note(Scope::Session(" ".into()), "nowhere")),
        Err(Error::Invalid(_))
    ));
    assert!(store.list(&ItemFilter::default()).unwrap().is_empty());
    assert!(matches!(
        store.revise("no-such-item", 1, reword("x"), None, &captain()),
        Err(Error::NotFound { .. })
    ));
    assert_eq!(store.item("no-such-item").unwrap(), None);
    assert!(store.versions("no-such-item").unwrap().is_empty());
}

#[test]
fn the_list_filters_by_kind_scope_and_status() {
    let store = store();
    let project = project(&store, "withnav");
    let here = Scope::Project(project.id.clone());
    remember(&store, Scope::Global, "global decision");
    remember(&store, here.clone(), "project decision");
    let mut pitfall = note(here.clone(), "project pitfall");
    pitfall.kind = Kind::Pitfall;
    let pitfall = store.remember(pitfall).unwrap();
    let gone = remember(&store, here.clone(), "project mistake");
    store.invalidate(&gone.id, 1, None, &captain()).unwrap();
    let list = |filter: ItemFilter| store.list(&filter).unwrap();

    assert_eq!(list(ItemFilter::default()).len(), 4);
    assert_eq!(
        bodies(&list(ItemFilter {
            scope: Some(ScopeKind::Project),
            scope_ref: Some(project.id.clone()),
            status: Some(ItemStatus::Active),
            ..ItemFilter::default()
        })),
        ["project decision", "project pitfall"]
    );
    assert_eq!(
        list(ItemFilter {
            kind: Some(Kind::Pitfall),
            ..ItemFilter::default()
        }),
        [pitfall]
    );
    assert_eq!(
        bodies(&list(ItemFilter {
            scope: Some(ScopeKind::Global),
            ..ItemFilter::default()
        })),
        ["global decision"]
    );
    assert_eq!(
        bodies(&list(ItemFilter {
            text: Some("MISTAKE".into()),
            ..ItemFilter::default()
        })),
        ["project mistake"]
    );
    // Newest first, and a limit keeps the newest.
    let newest = list(ItemFilter {
        limit: Some(1),
        ..ItemFilter::default()
    });
    assert_eq!(bodies(&newest), ["project mistake"]);
    assert_eq!("pitfall".parse::<Kind>().unwrap(), Kind::Pitfall);
    assert_eq!(Kind::Pitfall.to_string(), "pitfall");
    assert_eq!(
        Scope::from_parts(ScopeKind::Project, &project.id).unwrap(),
        here
    );
    assert_eq!(
        (here.kind(), here.reference()),
        (ScopeKind::Project, project.id.as_str())
    );
    assert!("verdict".parse::<Kind>().is_err());
    assert!(Scope::from_parts(ScopeKind::Group, "").is_err());
}
