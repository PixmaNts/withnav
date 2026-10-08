use super::*;
use crate::error::ProviderError;
use crate::provider::model_name::ModelNameError;
use serde_json::{json, Value};

fn catalog() -> ModelsConfig {
    ModelsConfig::parse(&json!({
        "providers": {"omniroute": {"base_url":"http://localhost/v1", "catalog_format":"omniroute"}, "inception":{"base_url":"http://localhost/v1"}},
        "models": {
            "omniroute/cc/chat": {"provider":"omniroute", "model":"cc/chat", "default_variant":"high", "variants":{"low":{"reasoning_effort":"low"},"high":{"reasoning_effort":"high"}}},
            "inception/mercury": {"provider":"inception", "model":"mercury", "api":"fim"},
            "omniroute/typesafe/jev": {"provider":"omniroute", "model":"typesafe/jev", "api":"systemone"},
            "omniroute/cc/discovered": {"provider":"omniroute", "model":"cc/discovered", "default_variant":"high"},
            "omniroute/cc/empty": {"provider":"omniroute", "model":"cc/empty", "variants":{}},
            "omniroute/cc/no-low": {"provider":"omniroute", "model":"cc/no-low", "default_variant":"high", "variants":{"high":{}}}
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
fn resolve_name_uses_canonical_keys_and_rejects_unknown_models() {
    let catalog = catalog();
    let name = ModelName::parse("omniroute/cc/chat", &catalog).unwrap();
    let resolved = catalog.resolve_name(&name).unwrap();
    assert_eq!(resolved.alias, "omniroute/cc/chat");
    assert_eq!(resolved.model.model, "cc/chat");
    assert!(catalog.resolve("old-chat").is_err());
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
        let settings =
            self::settings(json!({"layers":{"metacognition":format!("omniroute/{model}")}}));
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
        json!({"agents":{"lux":{"model":"omniroute/cc/chat","variant":"high"},"bad":{"model":"omniroute/missing"}}}),
    );
    let resolver = AssignmentResolver::new(&catalog, &settings);
    let AgentModel::Assigned(model) = resolver
        .agent_model("lux", Some("omniroute/cc/no-low"))
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
    let known = [
        ("build", Some("omniroute/cc/chat"), true),
        ("lux", None, true),
    ];
    let empty = Settings::default();
    assert_eq!(
        AssignmentResolver::new(&catalog, &empty)
            .default_agent_check(&known)
            .unwrap(),
        "build"
    );
    let configured = settings(
        json!({"default_agent":"lux","agents":{"lux":{"model":"omniroute/cc/chat"}}}),
    );
    assert_eq!(
        AssignmentResolver::new(&catalog, &configured)
            .default_agent_check(&known)
            .unwrap(),
        "lux"
    );
    let unknown = settings(json!({"default_agent":"missing"}));
    let error = AssignmentResolver::new(&catalog, &unknown)
        .default_agent_check(&known)
        .unwrap_err();
    assert!(matches!(error, DefaultAgentError::Unknown { .. }));
    assert!(
        error.to_string().contains("default_agent") && error.to_string().contains("build, lux")
    );
    for value in [
        json!({"default_agent":"lux"}),
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
            json!({"agents":{"build":{"model":"omniroute/cc/chat","variant":false}}}),
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

#[test]
fn duplicate_identities_are_ambiguous_and_legacy_keys_are_not_resolvable() {
    for different_api in [false, true] {
        let mut catalog = catalog();
        let mut duplicate = catalog.models["omniroute/cc/chat"].clone();
        if different_api {
            duplicate.api = ApiKind::Fim;
        }
        catalog.models.insert("a-renamed".into(), duplicate);
        let name = ModelName::parse("omniroute/cc/chat", &catalog).unwrap();
        assert!(
            matches!(catalog.resolve_name(&name), Err(ProviderError::AmbiguousModel { model, keys })
            if model == "omniroute/cc/chat" && keys == ["a-renamed", "omniroute/cc/chat"])
        );
        assert!(catalog.resolve("omniroute/cc/chat").is_ok());
        catalog.models.remove("omniroute/cc/chat");
        assert!(catalog.resolve_name(&name).is_err());
        let mut other = catalog.models["a-renamed"].clone();
        other.provider = "inception".into();
        catalog.models.insert("other-provider".into(), other);
        assert!(catalog.resolve_name(&name).is_err());
        assert!(catalog
            .resolve_name(&ModelName::parse("inception/cc/chat", &catalog).unwrap())
            .is_err());
    }
}

#[test]
fn agent_apis_are_chat_only_in_config_and_headers_and_default() {
    let catalog = catalog();
    for model in ["inception/mercury", "omniroute/typesafe/jev"] {
        for from_config in [false, true] {
            let settings = self::settings(if from_config {
                json!({"agents":{"build":{"model":model}}})
            } else {
                json!({})
            });
            let resolver = AssignmentResolver::new(&catalog, &settings);
            let header = if from_config {
                "omniroute/cc/chat"
            } else {
                model
            };
            let AgentModel::Fallback(warning) =
                resolver.agent_model("build", Some(header)).unwrap()
            else {
                panic!()
            };
            assert_eq!(warning.model, model);
            assert!(warning.reason.contains("required API 'chat'"));
            assert!(warning.reason.contains(if model.starts_with("inception") {
                "fim"
            } else {
                "systemone"
            }));
            assert!(matches!(
                resolver.default_agent_check(&[("build", Some(header), true)]),
                Err(DefaultAgentError::NoUsableModel { .. })
            ));
        }
    }
}

#[test]
fn default_agent_requires_primary_and_lists_only_eligible_agents() {
    let catalog = catalog();
    let known = [
        ("build", Some("omniroute/cc/chat"), true),
        ("all", Some("omniroute/cc/chat"), true),
        ("sub", Some("omniroute/cc/chat"), false),
    ];
    for name in ["build", "all"] {
        let settings = self::settings(json!({"default_agent":name}));
        assert_eq!(
            AssignmentResolver::new(&catalog, &settings)
                .default_agent_check(&known)
                .unwrap(),
            name
        );
    }
    let settings = self::settings(json!({"default_agent":"sub"}));
    assert!(
        matches!(AssignmentResolver::new(&catalog, &settings).default_agent_check(&known), Err(DefaultAgentError::NotPrimary { valid_agents, .. }) if valid_agents == ["all", "build"])
    );
    let settings = self::settings(json!({"default_agent":"missing"}));
    assert!(
        matches!(AssignmentResolver::new(&catalog, &settings).default_agent_check(&known), Err(DefaultAgentError::Unknown { valid_agents, .. }) if valid_agents == ["all", "build"])
    );
    assert!(matches!(
        AssignmentResolver::new(&catalog, &Settings::default())
            .default_agent_check(&[("build", None, true)]),
        Err(DefaultAgentError::NoUsableModel { .. })
    ));
}

#[test]
fn assignment_variants_preserve_off_validate_static_and_defer_discovery() {
    let catalog = catalog();
    for value in [json!(null), json!("off")] {
        let settings = self::settings(
            json!({"layers":{"metacognition":{"model":"omniroute/cc/chat","variant":value}}}),
        );
        let assigned = enabled(
            AssignmentResolver::new(&catalog, &settings)
                .layer_assignment(Layer::Metacognition)
                .unwrap(),
        );
        assert_eq!(assigned.variant_choice, AssignmentVariant::Off);
        assert_eq!(assigned.variant, None);
        assert!(!assigned.prefer_low);
    }
    let settings = self::settings(json!({"layers":{"metacognition":"omniroute/cc/chat"}}));
    let assigned = enabled(
        AssignmentResolver::new(&catalog, &settings)
            .layer_assignment(Layer::Metacognition)
            .unwrap(),
    );
    assert_eq!(assigned.variant_choice, AssignmentVariant::Default);
    assert_eq!(assigned.variant.as_deref(), Some("low"));
    for model in [
        "omniroute/cc/chat",
        "omniroute/cc/empty",
        "inception/mercury",
    ] {
        let layer = if model.starts_with("inception") {
            Layer::Reflex
        } else {
            Layer::Metacognition
        };
        let settings = self::settings(
            json!({"layers":{layer.config_key():{"model":model,"variant":"bogus"}},"agents":{"build":{"model":model,"variant":"bogus"}}}),
        );
        let resolver = AssignmentResolver::new(&catalog, &settings);
        assert!(matches!(
            resolver.layer_assignment(layer).unwrap(),
            LayerAssignment::Disabled(LayerDisabled::InvalidVariant { .. })
        ));
        assert!(matches!(
            resolver.agent_model("build", None).unwrap(),
            AgentModel::Fallback(_)
        ));
        assert!(matches!(
            resolver.default_agent_check(&[("build", None, true)]),
            Err(DefaultAgentError::NoUsableModel { .. })
        ));
    }
    let settings = self::settings(
        json!({"agents":{"build":{"model":"omniroute/cc/discovered","variant":"future"}}}),
    );
    assert!(matches!(
        AssignmentResolver::new(&catalog, &settings)
            .agent_model("build", None)
            .unwrap(),
        AgentModel::Assigned(_)
    ));
}

#[test]
fn malformed_assignment_is_isolated_and_preserved_with_siblings() {
    let dir = tempfile::tempdir().unwrap();
    let raw = json!({"layers":{"reflex":{"model":false},"metacognition":"omniroute/cc/chat"},"agents":{"bad":{"model":3},"build":{"model":"omniroute/cc/chat"}},"editor":{"future":42}});
    std::fs::write(crate::settings::path(dir.path()), raw.to_string()).unwrap();
    let settings = crate::settings::load(dir.path()).unwrap();
    assert!(settings.layer_assignment(Layer::Reflex).is_err());
    assert!(settings.agent_assignment("bad").is_err());
    assert!(settings
        .layer_assignment(Layer::Metacognition)
        .unwrap()
        .is_some());
    assert!(settings.agent_assignment("build").unwrap().is_some());
    crate::settings::save(dir.path(), &settings).unwrap();
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(crate::settings::path(dir.path())).unwrap())
            .unwrap();
    assert_eq!(saved, raw);
}
