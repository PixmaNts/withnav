use super::*;

use std::os::unix::fs::PermissionsExt;

struct Fixture {
    _tmp: tempfile::TempDir,
    layout: Layout,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let layout = Layout {
        data: root.join("share/navai"),
        config: root.join("config/navai"),
        navigator_data: root.join("share/navigator"),
        rust_agent_data: root.join("share/rust-agent"),
        rust_agent_config: root.join("config/rust-agent"),
    };
    Fixture { _tmp: tmp, layout }
}

fn write(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, body).expect("write");
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).expect("read")
}

fn write_private(path: &Path, body: &str) {
    write(path, body);
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("chmod");
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("stat").permissions().mode() & 0o777
}

// Given the former `navigator` data dir with a database, its wal and shm,
//       the decision log and the terminal log
// When Navai starts
// Then each file is under the navai name, the old ones are gone
#[test]
fn the_navigator_data_dir_moves_to_navai_names() {
    let f = fixture();
    let nav = &f.layout.navigator_data;
    write(&nav.join("navigator.db"), "db");
    write(&nav.join("navigator.db-wal"), "wal");
    write(&nav.join("navigator.db-shm"), "shm");
    write(&nav.join("navigator.jsonl"), "log");
    write(&nav.join("tui.log"), "tui");

    let report = migrate_in(&f.layout);

    let new = &f.layout.data;
    assert_eq!(read(&new.join("navai.db")), "db");
    assert_eq!(read(&new.join("navai.db-wal")), "wal");
    assert_eq!(read(&new.join("navai.db-shm")), "shm");
    assert_eq!(read(&new.join("navai.jsonl")), "log");
    assert_eq!(read(&new.join("tui.log")), "tui");
    assert!(
        fs::read_dir(nav).expect("dir").next().is_none(),
        "old files are gone"
    );
    assert_eq!(report.moved.len(), 5);
    assert!(report.warnings.is_empty());
    assert_eq!(report.kept_history_db, None);
}

// Given the oldest layout: `rust-agent` data dir with its nested database
// When Navai starts
// Then the database, the log and the terminal log move
#[test]
fn the_rust_agent_data_dir_moves_too() {
    let f = fixture();
    let old = &f.layout.rust_agent_data;
    write(&old.join("navigator/sessions.db"), "db");
    write(&old.join("navigator/sessions.db-wal"), "wal");
    write(&old.join("navigator.jsonl"), "log");

    migrate_in(&f.layout);

    assert_eq!(read(&f.layout.data.join("navai.db")), "db");
    assert_eq!(read(&f.layout.data.join("navai.db-wal")), "wal");
    assert_eq!(read(&f.layout.data.join("navai.jsonl")), "log");
    assert!(!old.join("navigator.jsonl").exists());
    assert!(!old.join("navigator/sessions.db").exists());
}

// Given both former data dirs holding a database
// When Navai starts
// Then the `navigator` one wins and the `rust-agent` one is left alone
#[test]
fn the_navigator_dir_has_priority_over_the_rust_agent_dir() {
    let f = fixture();
    write(&f.layout.navigator_data.join("navigator.db"), "newer");
    write(
        &f.layout.rust_agent_data.join("navigator/sessions.db"),
        "older",
    );
    write(&f.layout.rust_agent_data.join("navigator.jsonl"), "log");

    migrate_in(&f.layout);

    assert_eq!(read(&f.layout.data.join("navai.db")), "newer");
    assert_eq!(
        read(&f.layout.rust_agent_data.join("navigator/sessions.db")),
        "older"
    );
    // The log only exists in the older layout: it still comes over.
    assert_eq!(read(&f.layout.data.join("navai.jsonl")), "log");
}

// Given the former config dir with a private auth.json and models.json
// When Navai starts
// Then both move and auth.json is still readable by its owner only
#[test]
fn the_config_moves_and_auth_json_stays_private() {
    let f = fixture();
    let old = &f.layout.rust_agent_config;
    write_private(&old.join("auth.json"), r#"{"p":{"api_key":"k"}}"#);
    write(&old.join("models.json"), "{}");
    write(&old.join("navigator/pipeline.toml"), "# user pipeline");

    migrate_in(&f.layout);

    let new = &f.layout.config;
    assert_eq!(mode(&new.join("auth.json")), 0o600);
    assert_eq!(read(&new.join("models.json")), "{}");
    assert_eq!(
        read(&new.join("navigator/pipeline.toml")),
        "# user pipeline"
    );
    assert!(!old.join("auth.json").exists());
    let store = crate::provider::auth::load_store(new).expect("auth.json still loads");
    assert_eq!(store.providers["p"].api_key, "k");
}

// Given files already present in the new dirs
// When Navai starts
// Then they are never overwritten and the old files stay
#[test]
fn nothing_is_overwritten() {
    let f = fixture();
    write(&f.layout.navigator_data.join("navigator.db"), "old db");
    write(&f.layout.navigator_data.join("navigator.jsonl"), "old log");
    write_private(&f.layout.rust_agent_config.join("auth.json"), "old auth");
    write(&f.layout.data.join("navai.db"), "new db");
    write(&f.layout.data.join("navai.jsonl"), "new log");
    write_private(&f.layout.config.join("auth.json"), "new auth");

    let report = migrate_in(&f.layout);

    assert_eq!(report, MigrationReport::default());
    assert_eq!(read(&f.layout.data.join("navai.db")), "new db");
    assert_eq!(read(&f.layout.data.join("navai.jsonl")), "new log");
    assert_eq!(read(&f.layout.config.join("auth.json")), "new auth");
    assert_eq!(
        read(&f.layout.navigator_data.join("navigator.db")),
        "old db"
    );
    assert_eq!(
        read(&f.layout.rust_agent_config.join("auth.json")),
        "old auth"
    );
}

// Given a layout that already migrated
// When Navai starts again
// Then nothing happens
#[test]
fn a_second_start_does_nothing() {
    let f = fixture();
    write(&f.layout.navigator_data.join("navigator.db"), "db");
    write(&f.layout.navigator_data.join("navigator.jsonl"), "log");
    write_private(&f.layout.rust_agent_config.join("auth.json"), "auth");

    assert!(!migrate_in(&f.layout).moved.is_empty());
    assert_eq!(migrate_in(&f.layout), MigrationReport::default());
    assert_eq!(read(&f.layout.data.join("navai.db")), "db");
}

// Given no former dir at all
// When Navai starts
// Then nothing is created
#[test]
fn a_fresh_install_creates_nothing() {
    let f = fixture();
    assert_eq!(migrate_in(&f.layout), MigrationReport::default());
    assert!(!f.layout.data.exists());
    assert!(!f.layout.config.exists());
}

// Given a database whose shm is locked by an open WAL connection
// When Navai starts
// Then the move is refused with a warning, every old file stays and the old
//      path is the one to keep using
#[test]
fn a_database_in_use_is_not_moved() {
    let f = fixture();
    let db = f.layout.navigator_data.join("navigator.db");
    fs::create_dir_all(&f.layout.navigator_data).expect("mkdir");
    let conn = rusqlite::Connection::open(&db).expect("open");
    conn.pragma_update(None, "journal_mode", "WAL")
        .expect("wal");
    conn.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1);")
        .expect("write");
    assert!(
        with_suffix(&db, "-shm").exists(),
        "the connection holds a shm file"
    );

    let report = migrate_in(&f.layout);

    assert!(report.moved.is_empty());
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert_eq!(report.kept_history_db, Some(db.clone()));
    assert!(db.exists());
    assert!(!f.layout.data.join("navai.db").exists());
    let rows: i64 = conn
        .query_row("SELECT count(*) FROM t", [], |row| row.get(0))
        .expect("still readable");
    assert_eq!(rows, 1);
}

// Given a real database nobody holds any more, with a wal left behind
// When Navai starts
// Then the files move and the history opens intact at the new path
#[test]
fn a_released_database_moves_and_still_opens() {
    let f = fixture();
    let db = f.layout.navigator_data.join("navigator.db");
    fs::create_dir_all(&f.layout.navigator_data).expect("mkdir");
    {
        let conn = rusqlite::Connection::open(&db).expect("open");
        conn.pragma_update(None, "journal_mode", "WAL")
            .expect("wal");
        conn.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (7);")
            .expect("write");
    }
    // A stray, unlocked wal as a crash leaves it.
    write(&with_suffix(&db, "-wal"), "");

    let report = migrate_in(&f.layout);

    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert!(!db.exists());
    let conn = rusqlite::Connection::open(f.layout.data.join("navai.db")).expect("open");
    let value: i64 = conn
        .query_row("SELECT x FROM t", [], |row| row.get(0))
        .expect("history intact");
    assert_eq!(value, 7);
}

// Given a database whose destination -shm is occupied
// When Navai starts
// Then the move is refused and the old files stay whole
#[test]
fn a_blocked_database_move_leaves_every_old_file_in_place() {
    let f = fixture();
    let nav = &f.layout.navigator_data;
    write(&nav.join("navigator.db"), "db");
    write(&nav.join("navigator.db-wal"), "wal");
    write(&f.layout.data.join("navai.db-wal"), "stray");

    let report = migrate_in(&f.layout);

    assert!(report.moved.is_empty());
    assert_eq!(report.warnings.len(), 1);
    assert_eq!(report.kept_history_db, Some(nav.join("navigator.db")));
    assert_eq!(read(&nav.join("navigator.db")), "db");
    assert_eq!(read(&nav.join("navigator.db-wal")), "wal");
    assert!(!f.layout.data.join("navai.db").exists());
}
