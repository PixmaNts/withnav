//! `nav serve`: the session server alone.
use super::*;
use crate::server::owner::{OwnerLock, OwnerPaths, PROTOCOL, Record, generate_token};
use std::sync::Arc;

/// The shared conversation database, in WithNav's data dir.
pub(super) fn history_db_path() -> anyhow::Result<std::path::PathBuf> {
    crate::paths::history_db().ok_or_else(|| anyhow::anyhow!("cannot resolve the WithNav data dir"))
}

pub(super) async fn serve_command(args: ServeArgs) -> anyhow::Result<i32> {
    if let Some(cmd) = args.command {
        return match cmd {
            ServeSubcommand::Stop { force } => serve_stop(force).await,
            ServeSubcommand::Status => serve_status().await,
        };
    }

    let history_db = history_db_path()?;
    if let Ok(meta) = std::fs::symlink_metadata(&history_db) {
        if meta.file_type().is_symlink() {
            anyhow::bail!(
                "history database {} is a symlink; point the data directory instead",
                history_db.display()
            );
        }
    }
    let paths = OwnerPaths::resolve(&history_db)?;
    paths.ensure_dir()?;

    let lock = match acquire_patiently(&paths).await? {
        Some(l) => l,
        None => {
            if args.detach {
                return Ok(EXIT_DONE);
            }
            anyhow::bail!(
                "a WithNav server already owns {}; `nav serve status`",
                paths.database.display()
            );
        }
    };

    // Remove any stale owner record under the acquired lock
    let _ = Record::remove(&paths);

    crate::session::Service::check_legacy_instances(&paths.database)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let token = generate_token()?;
    let client_cache = std::sync::Arc::new(crate::provider::openai::ClientCache::new());
    let shared = crate::navigator::bootstrap::Shared::new(client_cache.clone());

    let raw_cfg = config_dir()?;
    if !raw_cfg.exists() {
        std::fs::create_dir_all(&raw_cfg)?;
    }
    let cfg_dir = std::fs::canonicalize(&raw_cfg).unwrap_or(raw_cfg);
    let canonical_models = match args.models {
        Some(ref p) => {
            let abs = if p.is_absolute() {
                p.clone()
            } else {
                std::env::current_dir()?.join(p)
            };
            Some(std::fs::canonicalize(&abs)?)
        }
        None => None,
    };
    let rules_roots = RulesRoots::from_user_home();
    let bind_addr = std::net::SocketAddr::from(([127, 0, 0, 1], args.port));

    let config = crate::server::session::ServerConfig {
        bind: bind_addr,
        models_path: canonical_models.clone(),
        config_dir: cfg_dir.clone(),
        allow_non_loopback: false,
        bearer_token: Some(token.clone()),
        history_db: paths.database.clone(),
        data_dir: paths
            .database
            .parent()
            .ok_or_else(|| anyhow::anyhow!("database has no parent directory"))?
            .to_path_buf(),
        rules_roots,
        web: Some(crate::server::config::WebConfig::default()),
    };

    config.validate()?;
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    let bound_addr = listener.local_addr()?;
    let bound_port = bound_addr.port();

    let mut state =
        crate::server::session::AppState::with_hooks(config, shared.hooks(), client_cache)?
            .with_navigator(shared.clone());
    Arc::make_mut(&mut state.http).bind = bound_addr;

    let record = Record {
        protocol: PROTOCOL,
        build: env!("WITHNAV_VERSION").to_string(),
        instance: state.instance().to_string(),
        pid: std::process::id(),
        port: bound_port,
        token,
        config_dir: cfg_dir,
        models_path: canonical_models,
        started_at: time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
    };
    record.write_to(&paths)?;

    let watcher = tokio::spawn(watch_lock_file(paths.lock.clone(), state.clone()));
    let serve_res = crate::server::routes::serve_state_on(state.clone(), listener).await;
    watcher.abort();

    shared.shutdown().await;
    state.close_all().await;
    let flows = state.flows.clone();
    let _ = tokio::task::spawn_blocking(move || flows.shutdown()).await;
    let _ = Record::remove(&paths);
    drop(lock);

    serve_res?;
    Ok(EXIT_DONE)
}

async fn serve_stop(force: bool) -> anyhow::Result<i32> {
    let history_db = history_db_path()?;
    let paths = OwnerPaths::resolve(&history_db)?;

    if !OwnerLock::is_held(&paths)? {
        println!(
            "no WithNav server is running for {}",
            paths.database.display()
        );
        return Ok(EXIT_DONE);
    }

    let record = Record::read_from(&paths)?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    let url = format!("http://127.0.0.1:{}/v1/shutdown", record.port);
    let req = crate::server::owner::ShutdownRequest { force };
    let resp = client
        .post(&url)
        .bearer_auth(&record.token)
        .json(&req)
        .send()
        .await?;

    if resp.status() == reqwest::StatusCode::CONFLICT {
        anyhow::bail!("server is busy with active sessions; use --force to stop anyway");
    }
    if resp.status() != reqwest::StatusCode::ACCEPTED {
        anyhow::bail!(
            "server rejected shutdown request with status {}",
            resp.status()
        );
    }

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if !OwnerLock::is_held(&paths)? {
            println!("stopped WithNav server (pid {})", record.pid);
            return Ok(EXIT_DONE);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    println!(
        "shutdown signal delivered to WithNav server (pid {})",
        record.pid
    );
    Ok(EXIT_DONE)
}

async fn serve_status() -> anyhow::Result<i32> {
    let history_db = history_db_path()?;
    let paths = OwnerPaths::resolve(&history_db)?;

    if !OwnerLock::is_held(&paths)? {
        println!(
            "no WithNav server is running for {}",
            paths.database.display()
        );
        return Ok(EXIT_DONE);
    }

    match Record::read_from(&paths) {
        Ok(rec) => {
            println!(
                "WithNav server running: pid {}, port {}, url http://127.0.0.1:{}, build {}, started at {}",
                rec.pid, rec.port, rec.port, rec.build, rec.started_at
            );
        }
        Err(e) => {
            println!(
                "WithNav server lock is held for {} but record cannot be read: {e}",
                paths.database.display()
            );
        }
    }
    Ok(EXIT_DONE)
}

/// The owner lock, retried for a short while: a client checking whether an owner runs takes the
/// same lock for an instant, and a starting owner that met that probe must not conclude another
/// owner exists and exit, leaving nobody to serve. A published record next to a taken lock means
/// a live owner already serves: stop at once rather than linger and take over later. A lock still
/// taken after the retries belongs to an owner that is starting.
async fn acquire_patiently(
    paths: &crate::server::owner::OwnerPaths,
) -> std::io::Result<Option<OwnerLock>> {
    for _ in 0..40 {
        if let Some(lock) = OwnerLock::try_acquire(paths)? {
            return Ok(Some(lock));
        }
        if is_live_owner(paths).await {
            return Ok(None);
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    Ok(None)
}

async fn is_live_owner(paths: &crate::server::owner::OwnerPaths) -> bool {
    let Ok(record) = Record::read_from(paths) else {
        return false;
    };
    let Ok(client) = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_millis(150))
        .timeout(std::time::Duration::from_millis(300))
        .build()
    else {
        return false;
    };
    let mut nonce_bytes = [0u8; 16];
    if getrandom::getrandom(&mut nonce_bytes).is_err() {
        return false;
    }
    let nonce = hex::encode(nonce_bytes);
    let url = format!("http://127.0.0.1:{}/v1/hello?nonce={nonce}", record.port);
    let Ok(resp) = client.get(&url).send().await else {
        return false;
    };
    if !resp.status().is_success() {
        return false;
    }
    let Ok(hello) = resp.json::<crate::server::owner::Hello>().await else {
        return false;
    };
    if hello.instance != record.instance {
        return false;
    }
    let expected = crate::server::owner::hello_proof(&record.token, &nonce, &record.instance);
    crate::server::owner::constant_time_eq(&hello.proof, &expected)
}

/// Shuts the owner down once its lock file is gone or replaced: the data directory was deleted
/// or moved, so no client can find this owner any more and its database is no longer the one
/// at that path. Without this a detached owner whose directory was removed (a temporary test
/// directory, a wiped data dir) would run forever, unreachable.
async fn watch_lock_file(lock: std::path::PathBuf, state: crate::server::session::AppState) {
    let identity = |path: &std::path::Path| {
        let metadata = std::fs::metadata(path).ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            Some((metadata.dev(), metadata.ino()))
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            Some((0, 0))
        }
    };
    let Some(original) = identity(&lock) else {
        return;
    };
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        interval.tick().await;
        if identity(&lock) != Some(original) {
            tracing::warn!("the owner lock file is gone or replaced; shutting down");
            state.trigger_shutdown();
            return;
        }
    }
}
