use super::*;

pub(crate) fn test_config(bind: &str, allow_non_loopback: bool) -> ServerConfig {
    ServerConfig {
        bind: bind.parse().expect("test bind addr"),
        models_path: None,
        config_dir: PathBuf::from("/tmp/rust-agent-test"),
        allow_non_loopback,
        bearer_token: None,
        history_db: None,
    }
}

// Given the default loopback bind and a 0.0.0.0 bind without the flag
// When ServerConfig::validate runs
// Then loopback is accepted and 0.0.0.0 is refused with NonLoopbackBind
#[test]
fn loopback_bind_is_accepted_and_wildcard_refused_without_flag() {
    assert!(test_config("127.0.0.1:7310", false).validate().is_ok());
    assert!(matches!(
        test_config("0.0.0.0:7310", false).validate(),
        Err(ServerError::NonLoopbackBind(_))
    ));
}

// Given a non-loopback bind WITH --allow-non-loopback
// When ServerConfig::validate runs
// Then it is accepted (documented at-your-own-risk; still no auth/TLS)
#[test]
fn non_loopback_bind_is_accepted_with_the_explicit_flag() {
    assert!(test_config("0.0.0.0:7310", true).validate().is_ok());
    assert!(test_config("192.168.1.10:7310", true).validate().is_ok());
}
