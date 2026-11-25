use super::*;
use serde_json::json;

#[test]
fn errors_are_categories_not_provider_bodies() {
    for (status, category) in [(401, "http_rejected"), (503, "http_server"), (0, "timeout")] {
        let error = ProviderError::Api {
            status,
            message: "secret-key https://user:password@host/ response".into(),
            partial: None,
        };
        let (actual, value, hint) = error_category(&error);
        assert_eq!(actual, category);
        assert!(!format!("{value}{hint}").contains("secret"));
    }
    assert_eq!(
        error_category(&ProviderError::MalformedResponse("secret".into())).0,
        "invalid_answer"
    );
}

#[test]
fn answers_require_every_expected_type_range_and_choice() {
    let questions = editor_questions();
    let mut answers = BTreeMap::new();
    assert!(!valid_answers(&questions, &answers));
    answers.insert(
        "worth_attention".into(),
        serde_json::from_value(json!({"type":"noul","noul":0.5})).unwrap(),
    );
    answers.insert(
        "route".into(),
        serde_json::from_value(json!({"type":"choice","choice":"silent","probabilities":{"silent":1.0},"confidence":0.9}))
            .unwrap(),
    );
    assert!(valid_answers(&questions, &answers));
    answers.insert(
        "worth_attention".into(),
        serde_json::from_value(json!({"type":"noul","noul":2.0})).unwrap(),
    );
    assert!(!valid_answers(&questions, &answers));
}

#[test]
fn cli_flags_and_invalid_layer() {
    use clap::Parser;
    assert!(
        super::super::Cli::try_parse_from([
            "nav",
            "doctor",
            "--offline",
            "--json",
            "--layer",
            "instinct",
            "--workspace",
            "/workspace"
        ])
        .is_ok()
    );
    assert!(super::super::Cli::try_parse_from(["nav", "doctor", "--layer", "bogus"]).is_err());
}

#[test]
fn redacts_credentials_urls_and_controls() {
    assert_eq!(
        sanitize(
            "secret https://user:pass@host/key \u{1b}[31m",
            &["secret".into()]
        ),
        "[redacted] [url omitted] [31m"
    );
}

// Given a config.json that sets `goal_watch.notes` and a misspelled sibling
// When `nav doctor` inspects it offline
// Then the setting raises no unknown-key warning, while the sibling still does
#[tokio::test]
async fn goal_watch_notes_is_a_known_config_key() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("cfg");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.json"),
        json!({"goal_watch": {"notes": true, "notez": true}}).to_string(),
    )
    .unwrap();
    let args = super::super::DoctorArgs {
        offline: true,
        json: false,
        workspace: root.path().to_path_buf(),
        layer: None,
    };
    let report = inspect(
        &args,
        &config,
        Some(root.path()),
        Some(root.path().join("data")),
    )
    .await;
    let unknown: Vec<&str> = report
        .checks
        .iter()
        .map(|c| c.id.as_str())
        .filter(|id| id.starts_with("unknown.config"))
        .collect();
    assert_eq!(
        unknown,
        ["unknown.config.goal_watch.notez"],
        "{}",
        report.human()
    );
}
