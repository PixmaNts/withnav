//! `nav serve`: the session server alone.
use super::*;

/// The shared conversation database, in WithNav's data dir.
pub(super) fn history_db_path() -> anyhow::Result<std::path::PathBuf> {
    crate::paths::history_db().ok_or_else(|| anyhow::anyhow!("cannot resolve the WithNav data dir"))
}

pub(super) async fn serve_command(args: ServeArgs) -> anyhow::Result<i32> {
    use crate::server::session::ServerConfig;

    let bind: std::net::SocketAddr = args
        .bind
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid --bind address {:?}: {e}", args.bind))?;
    let cfg_dir = config_dir()?;
    let rules_roots = RulesRoots::from_user_home();
    let client_cache = std::sync::Arc::new(crate::provider::openai::ClientCache::new());
    crate::session::preflight::check(
        &cfg_dir,
        args.models.as_deref(),
        &std::env::current_dir()?,
        &rules_roots,
        client_cache.clone(),
    )
    .await?;
    let config = ServerConfig {
        bind,
        models_path: args.models,
        config_dir: cfg_dir,
        allow_non_loopback: args.allow_non_loopback,
        bearer_token: None,
        history_db: history_db_path()?,
        data_dir: crate::paths::data_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve data dir"))?,
        rules_roots,
        web: None,
    };
    crate::server::routes::serve(
        config,
        std::sync::Arc::new(crate::navigator::NavigatorHooks::new()),
        client_cache,
    )
    .await?;
    Ok(EXIT_DONE)
}
