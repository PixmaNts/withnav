//! Integration tests for the database owner process lifecycle, record publishing,
//! security checks, hello proof verification, and client connectivity.
//!
//! Unix only: they kill owners with signals, check file modes and build symbolic links. The
//! Windows owner (file lock, owner-only access list, detached start) has no runtime test here.
#![cfg(unix)]
use std::path::PathBuf;

use withnav::app::connect::{Wanted, ensure_server, find_server};
use withnav::server::owner::{OwnerLock, OwnerPaths, Record};

mod common;

#[tokio::test]
async fn concurrent_ensure_server_calls_elect_one_owner() {
    let env = common::TestEnv::new();
    let data_dir = env.root.join("data/withnav");
    let config_dir = env.root.join("config/withnav");
    let history_db = data_dir.join("withnav.db");
    let guard = common::owner::OwnerGuard::new(&data_dir);

    let wanted = Wanted {
        config_dir,
        data_dir,
        history_db,
        models: None,
        exe: PathBuf::from(env!("CARGO_BIN_EXE_nav")),
    };

    let mut tasks = Vec::new();
    for _ in 0..4 {
        let w = wanted.clone();
        tasks.push(tokio::spawn(async move {
            ensure_server(&w).await.expect("ensure server")
        }));
    }

    let mut owners = Vec::new();
    for t in tasks {
        owners.push(t.await.expect("task join"));
    }

    let first = &owners[0];
    for other in &owners[1..] {
        assert_eq!(first.instance, other.instance);
        assert_eq!(first.record.pid, other.record.pid);
        assert_eq!(first.url, other.url);
    }

    drop(guard);
}

#[tokio::test]
async fn permissions_and_token_redaction() {
    let env = common::TestEnv::new();
    let data_dir = env.root.join("data/withnav");
    let config_dir = env.root.join("config/withnav");
    let history_db = data_dir.join("withnav.db");
    let guard = common::owner::OwnerGuard::new(&data_dir);

    let wanted = Wanted {
        config_dir,
        data_dir: data_dir.clone(),
        history_db: history_db.clone(),
        models: None,
        exe: PathBuf::from(env!("CARGO_BIN_EXE_nav")),
    };

    let owner = ensure_server(&wanted).await.expect("ensure server");
    let paths = OwnerPaths::resolve(&history_db).expect("resolve paths");

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let dir_meta = std::fs::symlink_metadata(&paths.dir).expect("dir meta");
        assert_eq!(dir_meta.mode() & 0o777, 0o700);

        let rec_meta = std::fs::symlink_metadata(&paths.record).expect("rec meta");
        assert_eq!(rec_meta.mode() & 0o777, 0o600);
    }

    if paths.log.exists() {
        let log = std::fs::read_to_string(&paths.log).expect("read log");
        assert!(!log.contains(&owner.token));
    }

    let debug = format!("{owner:?}");
    assert!(!debug.contains(&owner.token));

    let record_debug = format!("{:?}", owner.record);
    assert!(!record_debug.contains(&owner.token));

    drop(guard);
}

#[tokio::test]
async fn stale_record_from_sigkilled_owner_and_concurrent_ensure_server() {
    let env = common::TestEnv::new();
    let data_dir = env.root.join("data/withnav");
    let config_dir = env.root.join("config/withnav");
    let history_db = data_dir.join("withnav.db");
    let _guard = common::owner::OwnerGuard::new(&data_dir);

    let wanted = Wanted {
        config_dir,
        data_dir,
        history_db: history_db.clone(),
        models: None,
        exe: PathBuf::from(env!("CARGO_BIN_EXE_nav")),
    };

    let owner1 = ensure_server(&wanted).await.expect("ensure server 1");
    let pid1 = owner1.record.pid;
    let paths = OwnerPaths::resolve(&history_db).expect("resolve paths");

    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid1.to_string()])
            .status();
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while OwnerLock::is_held(&paths).unwrap_or(false) {
        assert!(
            std::time::Instant::now() < deadline,
            "the killed owner still holds the lock after 15 s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    assert!(paths.record.exists());

    let mut tasks = Vec::new();
    for _ in 0..4 {
        let w = wanted.clone();
        tasks.push(tokio::spawn(async move {
            ensure_server(&w).await.expect("concurrent ensure server")
        }));
    }

    let mut owners = Vec::new();
    for t in tasks {
        owners.push(t.await.expect("join"));
    }

    let first = &owners[0];
    assert_ne!(first.record.pid, pid1);
    assert_ne!(first.instance, owner1.instance);
    for other in &owners[1..] {
        assert_eq!(first.instance, other.instance);
        assert_eq!(first.record.pid, other.record.pid);
        assert_eq!(first.url, other.url);
    }
}

#[tokio::test]
async fn client_probe_racing_single_starter_never_makes_it_exit() {
    let env = common::TestEnv::new();
    let data_dir = env.root.join("data/withnav");
    let config_dir = env.root.join("config/withnav");
    let history_db = data_dir.join("withnav.db");
    let guard = common::owner::OwnerGuard::new(&data_dir);

    let wanted = Wanted {
        config_dir,
        data_dir,
        history_db: history_db.clone(),
        models: None,
        exe: PathBuf::from(env!("CARGO_BIN_EXE_nav")),
    };

    let w_probe = wanted.clone();
    let stop_probe = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_probe_clone = stop_probe.clone();

    let probe_task = tokio::spawn(async move {
        while !stop_probe_clone.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = find_server(&w_probe).await;
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    });

    let owner = ensure_server(&wanted).await.expect("ensure server");
    stop_probe.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = probe_task.await;

    assert!(OwnerLock::is_held(&OwnerPaths::resolve(&history_db).unwrap()).unwrap_or(false));
    assert_eq!(owner.record.build, env!("WITHNAV_VERSION"));
    drop(guard);
}

#[tokio::test]
async fn lock_held_by_test_no_record_find_server_errors_after_bound() {
    let env = common::TestEnv::new();
    let data_dir = env.root.join("data/withnav");
    let config_dir = env.root.join("config/withnav");
    let history_db = data_dir.join("withnav.db");

    let wanted = Wanted {
        config_dir,
        data_dir,
        history_db: history_db.clone(),
        models: None,
        exe: PathBuf::from(env!("CARGO_BIN_EXE_nav")),
    };

    let paths = OwnerPaths::resolve(&history_db).expect("resolve paths");
    let lock = OwnerLock::try_acquire(&paths)
        .expect("probe")
        .expect("acquire lock");

    let res = find_server(&wanted).await;
    assert!(res.is_err());
    let err = res.err().unwrap().to_string();
    assert!(err.contains("a server is starting or stuck; `nav serve status`"));

    assert!(!paths.log.exists());
    drop(lock);
}

#[tokio::test]
async fn lock_held_forged_record_wrong_proof_refused_no_auth_header() {
    let env = common::TestEnv::new();
    let data_dir = env.root.join("data/withnav");
    let config_dir = env.root.join("config/withnav");
    let history_db = data_dir.join("withnav.db");

    let wanted = Wanted {
        config_dir: config_dir.clone(),
        data_dir,
        history_db: history_db.clone(),
        models: None,
        exe: PathBuf::from(env!("CARGO_BIN_EXE_nav")),
    };

    let paths = OwnerPaths::resolve(&history_db).expect("resolve paths");
    let lock = OwnerLock::try_acquire(&paths)
        .expect("probe")
        .expect("acquire lock");

    let fake_server = wiremock::MockServer::start().await;
    let fake_port = fake_server.address().port();

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/v1/hello"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "protocol": 1,
                "build": "0.8.3",
                "instance": "fake_instance",
                "proof": "bad_hex_proof_1234"
            })),
        )
        .mount(&fake_server)
        .await;

    let forged_record = Record {
        protocol: 1,
        build: "0.8.3".into(),
        instance: "fake_instance".into(),
        pid: std::process::id(),
        port: fake_port,
        token: "forged_token".into(),
        config_dir,
        models_path: None,
        started_at: "2026-11-14T00:00:00Z".into(),
    };
    forged_record.write_to(&paths).expect("write forged");

    let res = find_server(&wanted).await;
    assert!(res.is_err());
    let err = res.err().unwrap().to_string();
    assert!(
        err.contains("proof verification failed") || err.contains("a server is starting or stuck")
    );

    let requests = fake_server.received_requests().await.expect("requests");
    assert!(!requests.is_empty());
    for req in requests {
        assert!(
            req.headers.get("authorization").is_none(),
            "fake server must never receive Authorization header"
        );
    }

    let _ = Record::remove(&paths);
    drop(lock);
}

#[tokio::test]
async fn protocol_mismatch_record_errors_and_serve_stop_stops_owner() {
    let env = common::TestEnv::new();
    let data_dir = env.root.join("data/withnav");
    let config_dir = env.root.join("config/withnav");
    let history_db = data_dir.join("withnav.db");
    let guard = common::owner::OwnerGuard::new(&data_dir);

    let wanted = Wanted {
        config_dir,
        data_dir,
        history_db: history_db.clone(),
        models: None,
        exe: PathBuf::from(env!("CARGO_BIN_EXE_nav")),
    };

    let owner = ensure_server(&wanted).await.expect("ensure server");
    let paths = OwnerPaths::resolve(&history_db).expect("resolve paths");

    let mut mismatched = owner.record.clone();
    mismatched.protocol = 999;
    mismatched.write_to(&paths).expect("write mismatched");

    let res = find_server(&wanted).await;
    assert!(res.is_err());
    let err = res.err().unwrap().to_string();
    assert!(err.contains("server protocol mismatch"));

    let status = std::process::Command::new(&wanted.exe)
        .args(["serve", "stop", "--force"])
        .env("WITHNAV_DATA_DIR", &wanted.data_dir)
        .env("WITHNAV_CONFIG_DIR", &wanted.config_dir)
        .status()
        .expect("stop command");
    assert!(status.success());

    assert!(!OwnerLock::is_held(&paths).unwrap_or(true));
    drop(guard);
}

#[tokio::test]
async fn config_dir_and_models_conflict_name_nav_serve_stop() {
    let env = common::TestEnv::new();
    let data_dir = env.root.join("data/withnav");
    let config_dir = env.root.join("config/withnav");
    let history_db = data_dir.join("withnav.db");
    let guard = common::owner::OwnerGuard::new(&data_dir);

    let wanted = Wanted {
        config_dir: config_dir.clone(),
        data_dir: data_dir.clone(),
        history_db: history_db.clone(),
        models: None,
        exe: PathBuf::from(env!("CARGO_BIN_EXE_nav")),
    };

    let _owner = ensure_server(&wanted).await.expect("ensure server");

    let other_cfg = env.root.join("other_config");
    std::fs::create_dir_all(&other_cfg).expect("create dir");
    let mut wanted_diff_cfg = wanted.clone();
    wanted_diff_cfg.config_dir = other_cfg;
    let err1 = find_server(&wanted_diff_cfg).await.unwrap_err().to_string();
    assert!(err1.contains("server config dir mismatch"));
    assert!(err1.contains("`nav serve stop`"));

    let other_models = env.root.join("custom_models.json");
    std::fs::write(&other_models, "{}").expect("write models");
    let mut wanted_diff_models = wanted.clone();
    wanted_diff_models.models = Some(other_models);
    let err2 = find_server(&wanted_diff_models)
        .await
        .unwrap_err()
        .to_string();
    assert!(err2.contains("server models path mismatch"));
    assert!(err2.contains("`nav serve stop`"));

    drop(guard);
}

#[tokio::test]
async fn data_dir_reached_through_symlink_alias_shares_owner() {
    let env = common::TestEnv::new();
    let real_data = env.root.join("real_data/withnav");
    let alias_data = env.root.join("alias_data/withnav");
    let config_dir = env.root.join("config/withnav");

    std::fs::create_dir_all(&real_data).expect("create real");
    std::fs::create_dir_all(alias_data.parent().unwrap()).expect("create alias parent");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real_data, &alias_data).expect("symlink");

    let guard = common::owner::OwnerGuard::new(&real_data);

    let wanted_real = Wanted {
        config_dir: config_dir.clone(),
        data_dir: real_data.clone(),
        history_db: real_data.join("withnav.db"),
        models: None,
        exe: PathBuf::from(env!("CARGO_BIN_EXE_nav")),
    };

    let wanted_alias = Wanted {
        config_dir,
        data_dir: alias_data.clone(),
        history_db: alias_data.join("withnav.db"),
        models: None,
        exe: PathBuf::from(env!("CARGO_BIN_EXE_nav")),
    };

    let owner_real = ensure_server(&wanted_real).await.expect("owner real");
    let owner_alias = ensure_server(&wanted_alias).await.expect("owner alias");

    assert_eq!(owner_real.instance, owner_alias.instance);
    assert_eq!(owner_real.record.pid, owner_alias.record.pid);
    assert_eq!(owner_real.url, owner_alias.url);

    drop(guard);
}

#[tokio::test]
async fn symlinked_database_file_is_refused() {
    let env = common::TestEnv::new();
    let data_dir = env.root.join("data/withnav");
    let config_dir = env.root.join("config/withnav");
    std::fs::create_dir_all(&data_dir).expect("create data");

    let real_db = env.root.join("real.db");
    std::fs::write(&real_db, []).expect("create real db");
    let symlink_db = data_dir.join("withnav.db");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real_db, &symlink_db).expect("create symlink");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_nav"))
        .arg("serve")
        .env("WITHNAV_DATA_DIR", &data_dir)
        .env("WITHNAV_CONFIG_DIR", &config_dir)
        .output()
        .expect("serve run");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("is a symlink; point the data directory instead"),
        "expected symlink refusal, got: {stderr}"
    );

    let _ = std::fs::remove_file(&symlink_db);
    let dangling_target = env.root.join("nonexistent.db");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&dangling_target, &symlink_db).expect("create dangling symlink");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_nav"))
        .arg("serve")
        .env("WITHNAV_DATA_DIR", &data_dir)
        .env("WITHNAV_CONFIG_DIR", &config_dir)
        .output()
        .expect("serve run");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("is a symlink; point the data directory instead"),
        "expected dangling symlink refusal, got: {stderr}"
    );
}

#[tokio::test]
async fn legacy_instances_lock_held_nav_serve_refuses() {
    let env = common::TestEnv::new();
    let data_dir = env.root.join("data/withnav");
    let config_dir = env.root.join("config/withnav");
    let history_db = data_dir.join("withnav.db");

    std::fs::create_dir_all(&data_dir).expect("create data");
    std::fs::write(&history_db, []).expect("create db");
    let instances_dir = data_dir.join("withnav.db.instances");
    std::fs::create_dir_all(&instances_dir).expect("create instances dir");
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(instances_dir.join("legacy.lock"))
        .expect("open lock");

    #[cfg(unix)]
    let _lock = nix::fcntl::Flock::lock(lock_file, nix::fcntl::FlockArg::LockExclusiveNonblock)
        .expect("flock");

    let status = std::process::Command::new(env!("CARGO_BIN_EXE_nav"))
        .arg("serve")
        .env("WITHNAV_DATA_DIR", &data_dir)
        .env("WITHNAV_CONFIG_DIR", &config_dir)
        .status()
        .expect("serve status");

    assert!(!status.success());
}

#[tokio::test]
async fn nav_serve_stop_stops_owner_and_releases_lock() {
    let env = common::TestEnv::new();
    let data_dir = env.root.join("data/withnav");
    let config_dir = env.root.join("config/withnav");
    let history_db = data_dir.join("withnav.db");
    let guard = common::owner::OwnerGuard::new(&data_dir);

    let wanted = Wanted {
        config_dir: config_dir.clone(),
        data_dir: data_dir.clone(),
        history_db: history_db.clone(),
        models: None,
        exe: PathBuf::from(env!("CARGO_BIN_EXE_nav")),
    };

    let _owner = ensure_server(&wanted).await.expect("ensure server");
    let paths = OwnerPaths::resolve(&history_db).expect("resolve paths");
    assert!(OwnerLock::is_held(&paths).unwrap_or(false));
    assert!(paths.record.exists());

    let status = std::process::Command::new(&wanted.exe)
        .args(["serve", "stop"])
        .env("WITHNAV_DATA_DIR", &data_dir)
        .env("WITHNAV_CONFIG_DIR", &config_dir)
        .status()
        .expect("serve stop");
    assert!(status.success());

    assert!(!OwnerLock::is_held(&paths).unwrap_or(true));
    assert!(!paths.record.exists());
    drop(guard);
}

#[tokio::test]
async fn recording_proxy_receives_nothing_on_loopback() {
    let env = common::TestEnv::new();
    let data_dir = env.root.join("data/withnav");
    let config_dir = env.root.join("config/withnav");
    let history_db = data_dir.join("withnav.db");
    let guard = common::owner::OwnerGuard::new(&data_dir);

    let proxy_server = wiremock::MockServer::start().await;
    let proxy_url = proxy_server.uri();

    let wanted = Wanted {
        config_dir,
        data_dir,
        history_db,
        models: None,
        exe: PathBuf::from(env!("CARGO_BIN_EXE_nav")),
    };

    unsafe {
        std::env::set_var("HTTP_PROXY", &proxy_url);
        std::env::set_var("ALL_PROXY", &proxy_url);
    }

    let owner = ensure_server(&wanted).await.expect("ensure server");

    let client = withnav_client::client::Client::new(&owner.url, Some(owner.token.clone()));
    let _health: withnav_client_api::protocol::HealthBody = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client")
        .get(format!("{}/v1/health", owner.url))
        .send()
        .await
        .expect("health send")
        .json()
        .await
        .expect("health json");

    let received = proxy_server.received_requests().await.expect("proxy reqs");
    assert_eq!(received.len(), 0, "proxy must not receive loopback traffic");

    unsafe {
        std::env::remove_var("HTTP_PROXY");
        std::env::remove_var("ALL_PROXY");
    }

    drop(client);
    drop(guard);
}
