use super::*;

const VALUE: &str = "sk-test-0123456789abcdef";

fn workspace() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn a_session_secret_is_seen_by_its_tree_only_and_redacted() {
    let store = SecretStore::in_memory();
    let ws = workspace();
    let (owner, other) = (Ulid::generate(), Ulid::generate());
    let view = store
        .put(
            owner,
            ws.path(),
            "API_KEY",
            VALUE,
            SecretTtl::Day,
            SecretScope::Session,
        )
        .unwrap();
    assert_eq!(view.last4, "cdef");
    assert!(view.expires_at.is_some());
    assert_eq!(
        store.environment(owner, ws.path()),
        vec![("API_KEY".to_owned(), VALUE.to_owned())]
    );
    assert!(store.environment(other, ws.path()).is_empty());
    assert_eq!(store.list(owner, ws.path())[0].use_count, 1);
    assert_eq!(
        store.redact(owner, ws.path(), &format!("key={VALUE}\n")),
        "key=[secret API_KEY]\n"
    );
    assert_eq!(
        store.redact(other, ws.path(), VALUE),
        VALUE,
        "another conversation's secret is not its to hide"
    );
}

#[test]
fn short_values_are_stored_but_never_redacted_or_shown() {
    let store = SecretStore::in_memory();
    let ws = workspace();
    let owner = Ulid::generate();
    let view = store
        .put(
            owner,
            ws.path(),
            "PIN",
            "1234",
            SecretTtl::Hour,
            SecretScope::Session,
        )
        .unwrap();
    assert_eq!(view.last4, "");
    assert_eq!(store.redact(owner, ws.path(), "pin 1234"), "pin 1234");
}

#[test]
fn longest_value_wins_when_one_contains_another() {
    let store = SecretStore::in_memory();
    let ws = workspace();
    let owner = Ulid::generate();
    store
        .put(
            owner,
            ws.path(),
            "SHORT",
            "abcdefgh",
            SecretTtl::Day,
            SecretScope::Session,
        )
        .unwrap();
    store
        .put(
            owner,
            ws.path(),
            "LONG",
            "abcdefgh-ijkl",
            SecretTtl::Day,
            SecretScope::Session,
        )
        .unwrap();
    assert_eq!(
        store.redact(owner, ws.path(), "abcdefgh-ijkl abcdefgh"),
        "[secret LONG] [secret SHORT]"
    );
}

#[test]
fn invalid_names_and_values_are_refused() {
    let store = SecretStore::in_memory();
    let ws = workspace();
    let owner = Ulid::generate();
    let put = |name: &str, value: &str| {
        store.put(
            owner,
            ws.path(),
            name,
            value,
            SecretTtl::Day,
            SecretScope::Session,
        )
    };
    assert!(matches!(
        put("api_key", VALUE),
        Err(SecretError::InvalidName(_))
    ));
    for reserved in [
        "PATH",
        "LD_PRELOAD",
        "BASH_ENV",
        "WITHNAV_DATA_DIR",
        "NODE_OPTIONS",
    ] {
        assert!(
            matches!(put(reserved, VALUE), Err(SecretError::ReservedName(_))),
            "{reserved}"
        );
    }
    assert_eq!(put("API_KEY", ""), Err(SecretError::EmptyValue));
    assert_eq!(put("API_KEY", "a\0b"), Err(SecretError::Nul));
    assert_eq!(
        put("API_KEY", &"x".repeat(MAX_VALUE_BYTES + 1)),
        Err(SecretError::TooLarge)
    );
}

#[test]
fn revoke_and_session_end_remove_the_value() {
    let store = SecretStore::in_memory();
    let ws = workspace();
    let owner = Ulid::generate();
    store
        .put(
            owner,
            ws.path(),
            "A_KEY",
            VALUE,
            SecretTtl::Day,
            SecretScope::Session,
        )
        .unwrap();
    store
        .put(
            owner,
            ws.path(),
            "B_KEY",
            "other-value-123",
            SecretTtl::Session,
            SecretScope::Session,
        )
        .unwrap();
    assert!(store.revoke(owner, ws.path(), "A_KEY"));
    assert!(!store.revoke(owner, ws.path(), "A_KEY"));
    assert_eq!(store.redact(owner, ws.path(), VALUE), VALUE);
    store.end_session(owner);
    assert!(store.list(owner, ws.path()).is_empty());
}

#[test]
fn expired_secrets_are_gone() {
    let store = SecretStore::in_memory();
    let ws = workspace();
    let owner = Ulid::generate();
    store
        .put(
            owner,
            ws.path(),
            "API_KEY",
            VALUE,
            SecretTtl::Hour,
            SecretScope::Session,
        )
        .unwrap();
    store.lock()[0].expires_at = Some(now() - 1);
    assert!(store.list(owner, ws.path()).is_empty());
    assert!(store.environment(owner, ws.path()).is_empty());
    assert_eq!(store.redact(owner, ws.path(), VALUE), VALUE);
}

#[cfg(unix)]
#[test]
fn workspace_secrets_persist_privately_and_are_removed_on_revoke() {
    use std::os::unix::fs::PermissionsExt;
    let data = tempfile::tempdir().unwrap();
    let ws = workspace();
    let store = SecretStore::open(data.path());
    let path = store.file().unwrap().to_path_buf();
    assert!(
        crate::permission::floor::credential(&path),
        "the fixed floor treats the file as a credential"
    );
    store
        .put(
            Ulid::generate(),
            ws.path(),
            "API_KEY",
            VALUE,
            SecretTtl::Day,
            SecretScope::Workspace,
        )
        .unwrap();
    // A session-scoped or session-long secret never reaches the disk.
    store
        .put(
            Ulid::generate(),
            ws.path(),
            "S_KEY",
            "session-only-1",
            SecretTtl::Day,
            SecretScope::Session,
        )
        .unwrap();
    store
        .put(
            Ulid::generate(),
            ws.path(),
            "W_KEY",
            "workspace-run-1",
            SecretTtl::Session,
            SecretScope::Workspace,
        )
        .unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let dir_mode = std::fs::metadata(path.parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dir_mode, 0o700);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("API_KEY"));
    assert!(!text.contains("session-only-1"));
    assert!(!text.contains("workspace-run-1"));

    // Another conversation of the workspace, after a restart, finds it.
    let reopened = SecretStore::open(data.path());
    let views = reopened.list(Ulid::generate(), ws.path());
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].name, "API_KEY");
    assert!(reopened.revoke(Ulid::generate(), ws.path(), "API_KEY"));
    assert!(!path.exists(), "the last secret gone removes the file");
}

#[test]
fn a_session_secret_hides_a_workspace_one_of_the_same_name() {
    let store = SecretStore::in_memory();
    let ws = workspace();
    let (owner, other) = (Ulid::generate(), Ulid::generate());
    store
        .put(
            other,
            ws.path(),
            "API_KEY",
            "workspace-value",
            SecretTtl::Day,
            SecretScope::Workspace,
        )
        .unwrap();
    store
        .put(
            owner,
            ws.path(),
            "API_KEY",
            "session-value-1",
            SecretTtl::Day,
            SecretScope::Session,
        )
        .unwrap();
    assert_eq!(
        store.environment(owner, ws.path()),
        vec![("API_KEY".to_owned(), "session-value-1".to_owned())]
    );
    assert_eq!(
        store.environment(other, ws.path()),
        vec![("API_KEY".to_owned(), "workspace-value".to_owned())]
    );
}

#[test]
fn model_texts_carry_metadata_only() {
    let view = SecretView {
        name: "API_KEY".into(),
        scope: SecretScope::Workspace,
        ttl: SecretTtl::Hour,
        expires_at: Some("2026-11-06T11:00:00Z".into()),
        created_at: "2026-11-06T10:00:00Z".into(),
        use_count: 0,
        last4: "cdef".into(),
    };
    let text = stored_text(&view);
    assert!(
        text.starts_with(
            "stored as $API_KEY (expires 2026-11-06T11:00:00Z, scope: this workspace)"
        )
    );
    assert!(!text.contains("cdef"));
    assert!(declined_text("API_KEY").starts_with("declined by the Captain"));
}

#[test]
fn revoke_removes_the_session_secret_and_uncovers_the_workspace_one() {
    let store = SecretStore::in_memory();
    let ws = workspace();
    let owner = Ulid::generate();
    for (value, scope) in [
        ("workspace-value", SecretScope::Workspace),
        ("session-value-1", SecretScope::Session),
    ] {
        store
            .put(owner, ws.path(), "API_KEY", value, SecretTtl::Day, scope)
            .unwrap();
    }
    assert!(store.revoke(owner, ws.path(), "API_KEY"));
    assert_eq!(
        store.environment(owner, ws.path()),
        vec![("API_KEY".to_owned(), "workspace-value".to_owned())],
        "only the secret the conversation saw went"
    );
    assert!(store.revoke(owner, ws.path(), "API_KEY"));
    assert!(store.environment(owner, ws.path()).is_empty());
}

#[test]
fn an_unreadable_secrets_file_survives_the_start() {
    let data = tempfile::tempdir().unwrap();
    let path = data.path().join("credentials").join("secrets.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{ not json").unwrap();
    let store = SecretStore::open(data.path());
    assert!(store.list(Ulid::generate(), data.path()).is_empty());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
}

#[test]
fn using_a_secret_does_not_rewrite_the_file() {
    let data = tempfile::tempdir().unwrap();
    let ws = workspace();
    let owner = Ulid::generate();
    let store = SecretStore::open(data.path());
    store
        .put(
            owner,
            ws.path(),
            "API_KEY",
            VALUE,
            SecretTtl::Day,
            SecretScope::Workspace,
        )
        .unwrap();
    let path = store.file().unwrap().to_path_buf();
    let before = std::fs::read_to_string(&path).unwrap();
    assert_eq!(store.environment(owner, ws.path()).len(), 1);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
}

#[test]
fn a_workspace_secret_that_cannot_be_saved_is_reported() {
    let data = tempfile::tempdir().unwrap();
    let ws = workspace();
    // A file where the directory should be: the save cannot create it.
    std::fs::write(data.path().join("credentials"), "").unwrap();
    let store = SecretStore::open(data.path());
    let owner = Ulid::generate();
    let error = store
        .put(
            owner,
            ws.path(),
            "API_KEY",
            VALUE,
            SecretTtl::Day,
            SecretScope::Workspace,
        )
        .unwrap_err();
    assert!(matches!(error, SecretError::NotSaved(_)));
    assert!(
        store
            .put(
                owner,
                ws.path(),
                "S_KEY",
                VALUE,
                SecretTtl::Day,
                SecretScope::Session
            )
            .is_ok(),
        "a secret that is never saved does not care"
    );
}

fn put_workspace(
    store: &SecretStore,
    owner: Ulid,
    ws: &Path,
    name: &str,
    value: &str,
) -> Result<SecretView, SecretError> {
    store.put(
        owner,
        ws,
        name,
        value,
        SecretTtl::Day,
        SecretScope::Workspace,
    )
}

/// Makes the next save fail: a directory sits where the file goes, so the rename cannot
/// replace it.
fn break_saves(store: &SecretStore) {
    let path = store.file().unwrap();
    let _ = std::fs::remove_file(path);
    std::fs::create_dir_all(path).unwrap();
}

#[test]
fn a_failed_save_leaves_no_secret_active() {
    let data = tempfile::tempdir().unwrap();
    let ws = workspace();
    let owner = Ulid::generate();
    let store = SecretStore::open(data.path());
    break_saves(&store);
    let error = put_workspace(&store, owner, ws.path(), "API_KEY", VALUE).unwrap_err();
    assert!(matches!(error, SecretError::NotSaved(_)));
    assert!(store.list(owner, ws.path()).is_empty());
    assert!(store.environment(owner, ws.path()).is_empty());
    assert_eq!(store.redact(owner, ws.path(), VALUE), VALUE);
}

#[test]
fn a_failed_replacement_keeps_the_previous_secret() {
    let data = tempfile::tempdir().unwrap();
    let ws = workspace();
    let owner = Ulid::generate();
    let store = SecretStore::open(data.path());
    put_workspace(&store, owner, ws.path(), "API_KEY", VALUE).unwrap();
    let before = store.list(owner, ws.path());
    break_saves(&store);
    assert!(put_workspace(&store, owner, ws.path(), "API_KEY", "replacement-value-1").is_err());
    assert_eq!(
        store.environment(owner, ws.path()),
        vec![("API_KEY".to_owned(), VALUE.to_owned())]
    );
    let after = store.list(owner, ws.path());
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].created_at, before[0].created_at);
    assert_eq!(after[0].expires_at, before[0].expires_at);
}

#[test]
fn a_failed_save_does_not_bring_back_an_expired_secret() {
    let data = tempfile::tempdir().unwrap();
    let ws = workspace();
    let owner = Ulid::generate();
    let store = SecretStore::open(data.path());
    put_workspace(&store, owner, ws.path(), "OLD_KEY", VALUE).unwrap();
    store.lock()[0].expires_at = Some(now() - 1);
    break_saves(&store);
    assert!(put_workspace(&store, owner, ws.path(), "NEW_KEY", "another-value-12").is_err());
    assert!(store.list(owner, ws.path()).is_empty());
}

#[test]
fn an_expiry_whose_rewrite_fails_still_applies_in_memory() {
    let data = tempfile::tempdir().unwrap();
    let ws = workspace();
    let owner = Ulid::generate();
    let store = SecretStore::open(data.path());
    put_workspace(&store, owner, ws.path(), "API_KEY", VALUE).unwrap();
    store.lock()[0].expires_at = Some(now() - 1);
    break_saves(&store);
    assert!(store.environment(owner, ws.path()).is_empty());
    assert_eq!(store.redact(owner, ws.path(), VALUE), VALUE);
}

#[test]
fn invalid_saved_entries_are_dropped_from_memory_and_from_the_file() {
    let data = tempfile::tempdir().unwrap();
    let ws = workspace();
    let path = data.path().join("credentials").join("secrets.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let ttl = serde_json::to_value(SecretTtl::Day).unwrap();
    let entry = |name: &str, value: &str, created_at: i64, expires_at: i64| {
        serde_json::json!({
            "name": name, "value": value, "workspace": ws.path(), "ttl": ttl,
            "created_at": created_at, "expires_at": expires_at, "use_count": 0,
        })
    };
    let future = now() + 3600;
    let secrets = vec![
        entry("GOOD_KEY", "good-value-0001", 1, future),
        entry("bad_name", "bad-name-value-1", 1, future),
        entry("EMPTY_KEY", "", 1, future),
        entry("NUL_KEY", "nul\u{0}value-0001", 1, future),
        entry("BIG_KEY", &"x".repeat(MAX_VALUE_BYTES + 1), 1, future),
        entry("TIME_KEY", "far-future-value-1", 1, i64::MAX),
        entry("PATH", "reserved-value-1", 1, future),
    ];
    std::fs::write(
        &path,
        serde_json::json!({ "version": 1, "secrets": secrets }).to_string(),
    )
    .unwrap();
    let store = SecretStore::open(data.path());
    let views = store.list(Ulid::generate(), ws.path());
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].name, "GOOD_KEY");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("good-value-0001"));
    for dropped in ["bad-name-value-1", "far-future-value-1", "reserved-value-1"] {
        assert!(!text.contains(dropped), "{dropped} stayed on disk");
    }
    assert!(!text.contains("EMPTY_KEY") && !text.contains("NUL_KEY") && !text.contains("BIG_KEY"));
}

#[test]
fn a_use_count_reaches_the_file_with_the_next_save_only() {
    let data = tempfile::tempdir().unwrap();
    let ws = workspace();
    let owner = Ulid::generate();
    let store = SecretStore::open(data.path());
    put_workspace(&store, owner, ws.path(), "API_KEY", VALUE).unwrap();
    assert_eq!(store.environment(owner, ws.path()).len(), 1);
    // Lost on a restart: the count is written with the next save, not on every command.
    let reopened = SecretStore::open(data.path());
    assert_eq!(reopened.list(owner, ws.path())[0].use_count, 0);
    assert_eq!(store.environment(owner, ws.path()).len(), 1);
    put_workspace(&store, owner, ws.path(), "OTHER_KEY", "other-value-123").unwrap();
    let reopened = SecretStore::open(data.path());
    let counts: Vec<u64> = reopened
        .list(owner, ws.path())
        .iter()
        .map(|v| v.use_count)
        .collect();
    assert_eq!(
        counts,
        vec![2, 0],
        "API_KEY used twice, saved with the second put"
    );
}

#[cfg(unix)]
#[test]
fn a_permissive_file_is_tightened_before_it_is_read() {
    use std::os::unix::fs::PermissionsExt;
    let data = tempfile::tempdir().unwrap();
    let ws = workspace();
    let source = SecretStore::open(data.path());
    put_workspace(&source, Ulid::generate(), ws.path(), "API_KEY", VALUE).unwrap();
    let path = source.file().unwrap().to_path_buf();
    // Not readable by its owner: only a tighten-first load can read it.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    let store = SecretStore::open(data.path());
    assert_eq!(store.list(Ulid::generate(), ws.path()).len(), 1);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[cfg(unix)]
#[test]
fn a_save_syncs_the_directory_it_writes_into() {
    // Not observable from a test; this pins that the sync does not fail on a plain directory
    // and that removing the last secret (a synced removal) still succeeds.
    let data = tempfile::tempdir().unwrap();
    let ws = workspace();
    let owner = Ulid::generate();
    let store = SecretStore::open(data.path());
    put_workspace(&store, owner, ws.path(), "API_KEY", VALUE).unwrap();
    assert!(store.revoke(owner, ws.path(), "API_KEY"));
    assert!(!store.file().unwrap().exists());
}
