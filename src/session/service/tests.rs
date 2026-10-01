use super::*;

pub(crate) fn test_config() -> SessionConfig {
    SessionConfig {
        models_path: None,
        config_dir: PathBuf::from("/tmp/rust-agent-test"),
        history_db: None,
    }
}
