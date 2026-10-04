use super::*;
use crate::error::ProviderError;
use crate::provider::model_name::ModelNameError;
use serde_json::{json, Value};

fn catalog() -> ModelsConfig {
    ModelsConfig::parse(&json!({
        "providers": {"omniroute": {"base_url":"http://localhost/v1", "catalog_format":"omniroute"}, "inception":{"base_url":"http://localhost/v1"}},
        "models": {
            "old-chat": {"provider":"omniroute", "model":"cc/chat", "default_variant":"high", "variants":{"low":{"reasoning_effort":"low"},"high":{"reasoning_effort":"high"}}},
            "old-fim": {"provider":"inception", "model":"mercury", "api":"fim"},
            "old-instinct": {"provider":"omniroute", "model":"typesafe/jev", "api":"systemone"},
            "discovered": {"provider":"omniroute", "model":"cc/discovered", "default_variant":"high"},
            "empty": {"provider":"omniroute", "model":"cc/empty", "variants":{}},
            "no-low": {"provider":"omniroute", "model":"cc/no-low", "default_variant":"high", "variants":{"high":{}}}
        }
    }).to_string()).unwrap()
}

fn settings(value: Value) -> Settings {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(crate::settings::path(dir.path()), value.to_string()).unwrap();
    crate::settings::load(dir.path()).unwrap()
}

#[test]
fn model_name_preserves_nested_ids_and_groups_providers() {
    let catalog = catalog();
    let name = ModelName::parse("omniroute/cc/chat/nested", &catalog).unwrap();
    assert_eq!(name.gateway(), "omniroute");
    assert_eq!(name.id(), "cc/chat/nested");
    assert_eq!(name.provider_group(), "cc");
    assert_eq!(name.to_string(), "omniroute/cc/chat/nested");
    assert_eq!(
        ModelName::parse("inception/mercury", &catalog)
            .unwrap()
            .provider_group(),
        "inception"
    );
}

#[test]
fn model_name_rejects_malformed_names_and_unknown_gateways() {
    let catalog = catalog();
    for name in [
        "",
        "omniroute",
        "/chat",
        "omniroute/",
        "omniroute//chat",
        "omniroute/chat/",
        "omniroute/two words",
        "omniroute/a\u{0}",
    ] {
        assert!(
            matches!(
                ModelName::parse(name, &catalog),
                Err(ModelNameError::Invalid(_))
            ),
            "{name:?}"
        );
    }
    let error = ModelName::parse("unknown/chat", &catalog).unwrap_err();
    assert!(matches!(error, ModelNameError::UnknownGateway { .. }));
    assert!(error.to_string().contains("unknown"));
}

#[test]
fn resolve_name_ignores_alias_keys_and_names_unknown_models() {
    let catalog = catalog();
    let name = ModelName::parse("omniroute/cc/chat", &catalog).unwrap();
    let resolved = catalog.resolve_name(&name).unwrap();
    assert_eq!(resolved.alias, "old-chat");
    assert_eq!(resolved.model.model, "cc/chat");
    assert_eq!(catalog.resolve("old-chat").unwrap().model.model, "cc/chat");
    let missing = ModelName::parse("omniroute/cc/missing", &catalog).unwrap();
    assert!(
        matches!(catalog.resolve_name(&missing), Err(ProviderError::UnknownModel(name)) if name == missing.to_string())
    );
}

fn enabled(outcome: LayerAssignment) -> AssignedModel {
    match outcome {
        LayerAssignment::Enabled(model) => *model,
        other => panic!("{other:?}"),
    }
}

#[test]
fn layers_have_config_keys_required_apis_and_resolve_matching_models() {
    let catalog = catalog();
    let settings = settings(
        json!({"layers":{"reflex":"inception/mercury", "instinct":"omniroute/typesafe/jev", "metacognition":{"model":"omniroute/cc/chat", "variant":"high"}}}),
    );
    let resolver = AssignmentResolver::new(&catalog, &settings);
    for (layer, key, api) in [
        (Layer::Reflex, "reflex", ApiKind::Fim),
        (Layer::Instinct, "instinct", ApiKind::SystemOne),
        (Layer::Metacognition, "metacognition", ApiKind::Chat),
    ] {
        assert_eq!(layer.config_key(), key);
        assert_eq!(layer.required_api(), api);
        assert_eq!(
            enabled(resolver.layer_assignment(layer).unwrap())
                .resolved
                .model
                .api,
            api
        );
    }
    assert_eq!(
        enabled(resolver.layer_assignment(Layer::Metacognition).unwrap())
            .variant
            .as_deref(),
        Some("high")
    );
}

#[test]
fn layers_disable_missing_unknown_and_wrong_api_assignments() {
    let catalog = catalog();
    let empty = Settings::default();
    let resolver = AssignmentResolver::new(&catalog, &empty);
    for layer in [Layer::Reflex, Layer::Instinct, Layer::Metacognition] {
        assert!(
            matches!(resolver.layer_assignment(layer).unwrap(), LayerAssignment::Disabled(LayerDisabled::Missing { layer: found }) if found == layer)
        );
        for model in ["omniroute/missing", "missing/model", "not-a-name"] {
            let settings = settings(json!({"layers":{layer.config_key(): model}}));
            assert!(matches!(
                AssignmentResolver::new(&catalog, &settings)
                    .layer_assignment(layer)
                    .unwrap(),
                LayerAssignment::Disabled(LayerDisabled::UnknownModel { .. })
            ));
        }
        let wrong = if layer == Layer::Metacognition {
            "inception/mercury"
        } else {
            "omniroute/cc/chat"
        };
        let settings = settings(json!({"layers":{layer.config_key():wrong}}));
        let LayerAssignment::Disabled(reason @ LayerDisabled::WrongApi { .. }) =
            AssignmentResolver::new(&catalog, &settings)
                .layer_assignment(layer)
                .unwrap()
        else {
            panic!("wrong api was accepted")
        };
        let message = reason.to_string();
        assert!(
            message.contains(layer.config_key())
                && message.contains(wrong)
                && message.contains(api_name(layer.required_api()))
        );
    }
}

#[test]
fn metacognition_prefers_explicit_low_or_defers_to_runtime_offers() {
    let catalog = catalog();
    for (model, variant, prefer_low) in [
        ("cc/chat", Some("low"), false),
        ("cc/discovered", Some("high"), true),
        ("cc/empty", None, false),
        ("cc/no-low", Some("high"), false),
    ] {
        let settings = settings(json!({"layers":{"metacognition":format!("omniroute/{model}")}}));
        let assigned = enabled(
            AssignmentResolver::new(&catalog, &settings)
                .layer_assignment(Layer::Metacognition)
                .unwrap(),
        );
        assert_eq!(assigned.variant.as_deref(), variant);
        assert_eq!(assigned.prefer_low, prefer_low);
    }
}

#[test]
fn agents_prefer_config_over_headers_and_return_warning_fallback() {
    let catalog = catalog();
    let settings = settings(
        json!({"agents":{"oracle":{"model":"omniroute/cc/chat","variant":"high"},"bad":{"model":"omniroute/missing"}}}),
    );
    let resolver = AssignmentResolver::new(&catalog, &settings);
    let AgentModel::Assigned(model) = resolver
        .agent_model("oracle", Some("omniroute/cc/no-low"))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(model.name.id(), "cc/chat");
    assert_eq!(model.variant.as_deref(), Some("high"));
    let AgentModel::Assigned(model) = resolver
        .agent_model("header", Some("omniroute/cc/no-low"))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(model.name.id(), "cc/no-low");
    assert_eq!(model.variant.as_deref(), Some("high"));
    assert!(matches!(
        resolver.agent_model("none", None).unwrap(),
        AgentModel::Unassigned
    ));
    let AgentModel::Fallback(warning) = resolver
        .agent_model("bad", Some("omniroute/cc/chat"))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(warning.agent, "bad");
    assert_eq!(warning.model, "omniroute/missing");
    assert!(warning.to_string().contains("default agent"));
    assert!(matches!(
        resolver
            .agent_model("bad-header", Some("missing/chat"))
            .unwrap(),
        AgentModel::Fallback(_)
    ));
}

#[test]
fn default_agent_validates_name_and_requires_usable_model() {
    let catalog = catalog();
    let known = [("build", Some("omniroute/cc/chat")), ("oracle", None)];
    let empty = Settings::default();
    assert_eq!(
        AssignmentResolver::new(&catalog, &empty)
            .default_agent_check(&known)
            .unwrap(),
        "build"
    );
    let configured = settings(
        json!({"default_agent":"oracle","agents":{"oracle":{"model":"omniroute/cc/chat"}}}),
    );
    assert_eq!(
        AssignmentResolver::new(&catalog, &configured)
            .default_agent_check(&known)
            .unwrap(),
        "oracle"
    );
    let unknown = settings(json!({"default_agent":"missing"}));
    let error = AssignmentResolver::new(&catalog, &unknown)
        .default_agent_check(&known)
        .unwrap_err();
    assert!(matches!(error, DefaultAgentError::Unknown { .. }));
    assert!(
        error.to_string().contains("default_agent") && error.to_string().contains("build, oracle")
    );
    for value in [
        json!({"default_agent":"oracle"}),
        json!({"agents":{"build":{"model":"omniroute/missing"}}}),
    ] {
        let settings = settings(value);
        assert!(matches!(
            AssignmentResolver::new(&catalog, &settings).default_agent_check(&known),
            Err(DefaultAgentError::NoUsableModel { .. })
        ));
    }
    let malformed = settings(json!({"default_agent":false}));
    assert!(matches!(
        AssignmentResolver::new(&catalog, &malformed).default_agent_check(&known),
        Err(DefaultAgentError::InvalidSetting(_))
    ));
}

#[test]
fn malformed_assignment_settings_name_the_offending_key() {
    for (value, key) in [
        (json!({"layers":false}), "layers"),
        (json!({"layers":{"reflex":null}}), "layers.reflex"),
        (json!({"layers":{"reflex":{}}}), "layers.reflex.model"),
        (
            json!({"layers":{"reflex":{"model":1}}}),
            "layers.reflex.model",
        ),
        (
            json!({"layers":{"reflex":{"model":"inception/mercury","variant":false}}}),
            "layers.reflex.variant",
        ),
    ] {
        assert!(settings(value)
            .layer_assignment(Layer::Reflex)
            .unwrap_err()
            .contains(key));
    }
    for (value, key) in [
        (json!({"agents":[]}), "agents"),
        (
            json!({"agents":{"build":"omniroute/cc/chat"}}),
            "agents.build",
        ),
        (json!({"agents":{"build":{}}}), "agents.build.model"),
        (
            json!({"agents":{"build":{"model":""}}}),
            "agents.build.model",
        ),
        (
            json!({"agents":{"build":{"model":"omniroute/cc/chat","variant":null}}}),
            "agents.build.variant",
        ),
    ] {
        assert!(settings(value)
            .agent_assignment("build")
            .unwrap_err()
            .contains(key));
    }
    for value in [json!(null), json!(1), json!(""), json!("two words")] {
        assert!(settings(json!({"default_agent":value}))
            .default_agent()
            .unwrap_err()
            .contains("default_agent"));
    }
}

#[test]
fn assignment_getters_preserve_unknown_settings_on_save() {
    let dir = tempfile::tempdir().unwrap();
    let raw = json!({"layers":{"reflex":"inception/mercury","future":42},"agents":{"build":{"model":"omniroute/cc/chat","future":true}},"editor":{"future":3}});
    std::fs::write(crate::settings::path(dir.path()), raw.to_string()).unwrap();
    let settings = crate::settings::load(dir.path()).unwrap();
    settings.layer_assignment(Layer::Reflex).unwrap();
    settings.agent_assignment("build").unwrap();
    crate::settings::save(dir.path(), &settings).unwrap();
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(crate::settings::path(dir.path())).unwrap())
            .unwrap();
    assert_eq!(saved, raw);
}
