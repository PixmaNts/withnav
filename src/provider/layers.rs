//! Shared layer assignments, without legacy role-alias fallback.
//! Constructed clients pin assignments for their lifetime. A coherent caller should
//! later pass a shared assignment snapshot; there is no global reload mechanism here.
use std::{path::Path, sync::Arc};

use super::{
    assignments::{AssignedModel, AssignmentResolver, Layer, LayerAssignment},
    config::ModelsConfig,
    openai::ClientCache,
    systemone::SystemOneClient,
};

/// Loads one layer from config.json and validates its catalog identity and API.
pub fn assigned(
    models: &ModelsConfig,
    config_dir: &Path,
    layer: Layer,
) -> anyhow::Result<AssignedModel> {
    let settings = crate::settings::load(config_dir)?;
    let result = AssignmentResolver::new(models, &settings)
        .layer_assignment(layer)
        .map_err(anyhow::Error::msg)?;
    let assigned = match result {
        LayerAssignment::Enabled(model) => *model,
        LayerAssignment::Disabled(reason) => anyhow::bail!("{reason}"),
    };
    if layer != Layer::Metacognition && assigned.variant.is_some() {
        anyhow::bail!(
            "layer '{layer}' disabled: model '{}' does not support variants on its API",
            assigned.name
        );
    }
    Ok(assigned)
}

/// Builds the configured System One client, retaining its fixed two-second deadline.
pub fn instinct(
    models: &ModelsConfig,
    config_dir: &Path,
    cache: Arc<ClientCache>,
) -> anyhow::Result<SystemOneClient> {
    let assigned = assigned(models, config_dir, Layer::Instinct)?;
    let resolved = assigned.resolved;
    Ok(SystemOneClient::new(
        &resolved.provider_name,
        &resolved.provider,
        &resolved.model,
        config_dir,
        cache,
    )?)
}

/// Disables unavailable instinct with a diagnostic, for fail-closed consumers.
pub fn available_instinct(
    models: &ModelsConfig,
    config_dir: &Path,
    cache: Arc<ClientCache>,
) -> Option<SystemOneClient> {
    match instinct(models, config_dir, cache) {
        Ok(client) => Some(client),
        Err(error) => {
            tracing::warn!(%error, "instinct layer unavailable");
            None
        }
    }
}

/// Resolves the assignment against the runtime offer, preserving explicit off.
pub fn chat_variant(
    assigned: &AssignedModel,
    chat: &super::catalog::ResolvedChat,
) -> anyhow::Result<Option<super::config::Variant>> {
    let name = if assigned.prefer_low && chat.variants.contains_key("low") {
        Some("low")
    } else {
        assigned.variant.as_deref()
    };
    name.map(|name| {
        chat.variants.get(name).cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "layer 'metacognition' disabled: model '{}' has invalid variant '{name}'",
                assigned.name
            )
        })
    })
    .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn catalog() -> ModelsConfig {
        ModelsConfig::parse(&json!({"providers":{"gateway":{"base_url":"http://127.0.0.1:1/v1"}},"models":{
            "gateway/edit":{"provider":"gateway","model":"edit","api":"fim"},
            "gateway/typesafe/jev":{"provider":"gateway","model":"typesafe/jev","api":"systemone"},
            "gateway/combo/chat":{"provider":"gateway","model":"combo/chat","context_window":128000,"default_variant":"high","variants":{"low":{"reasoning_effort":"low"},"high":{"reasoning_effort":"high"}}}
        }}).to_string()).unwrap()
    }

    fn assign(value: serde_json::Value, layer: Layer) -> anyhow::Result<AssignedModel> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            json!({"layers":{layer.config_key():value}}).to_string(),
        )
        .unwrap();
        assigned(&catalog(), dir.path(), layer)
    }

    #[test]
    fn each_layer_resolves_identity_not_catalog_key_and_enforces_api() {
        for (layer, name, id) in [
            (Layer::Reflex, "gateway/edit", "edit"),
            (Layer::Instinct, "gateway/typesafe/jev", "typesafe/jev"),
            (Layer::Metacognition, "gateway/combo/chat", "combo/chat"),
        ] {
            assert_eq!(assign(json!(name), layer).unwrap().resolved.model.model, id);
            assert!(assign(json!("gateway/missing"), layer)
                .unwrap_err()
                .to_string()
                .contains("unknown model"));
        }
        assert!(assign(json!("gateway/combo/chat"), Layer::Instinct)
            .unwrap_err()
            .to_string()
            .contains("must use"));
        let dir = tempfile::tempdir().unwrap();
        assert!(assigned(&catalog(), dir.path(), Layer::Reflex)
            .unwrap_err()
            .to_string()
            .contains("missing"));
    }

    #[test]
    fn implicit_low_wins_over_stale_catalog_default_but_explicit_invalid_fails() {
        let mut models = catalog();
        models
            .models
            .get_mut("gateway/combo/chat")
            .unwrap()
            .default_variant = Some("obsolete".into());
        let dir = tempfile::tempdir().unwrap();
        for (assignment, valid) in [
            (json!("gateway/combo/chat"), true),
            (
                json!({"model":"gateway/combo/chat","variant":"obsolete"}),
                false,
            ),
        ] {
            std::fs::write(
                dir.path().join("config.json"),
                json!({"layers":{"metacognition":assignment}}).to_string(),
            )
            .unwrap();
            let result = assigned(&models, dir.path(), Layer::Metacognition);
            if valid {
                assert_eq!(result.unwrap().variant.as_deref(), Some("low"));
            } else {
                assert!(result.is_err());
            }
        }
    }

    #[test]
    fn metacognition_low_preserves_explicit_off_and_rejects_unknown_variants() {
        for value in [
            json!("gateway/combo/chat"),
            json!({"model":"gateway/combo/chat","variant":"low"}),
        ] {
            assert_eq!(
                assign(value, Layer::Metacognition)
                    .unwrap()
                    .variant
                    .as_deref(),
                Some("low")
            );
        }
        for off in [json!(null), json!("off")] {
            let assigned = assign(
                json!({"model":"gateway/combo/chat","variant":off}),
                Layer::Metacognition,
            )
            .unwrap();
            assert!(assigned.variant.is_none());
            assert!(!assigned.prefer_low);
        }
        assert!(assign(
            json!({"model":"gateway/combo/chat","variant":"bogus"}),
            Layer::Metacognition
        )
        .is_err());
        assert!(assign(
            json!({"model":"gateway/typesafe/jev","variant":"low"}),
            Layer::Instinct
        )
        .is_err());
    }

    #[tokio::test]
    async fn discovered_low_is_selected_only_for_default_assignment() {
        let mut models = catalog();
        let model = models.models.get_mut("gateway/combo/chat").unwrap();
        model.variants = None;
        models.providers.get_mut("gateway").unwrap().catalog_format =
            Some(crate::provider::config::CatalogFormat::Omniroute);
        let dir = tempfile::tempdir().unwrap();
        let cache = ClientCache::new();
        let catalog = crate::provider::catalog::ModelCatalog::new(Some(dir.path().join("data")));
        for (choice, expected) in [(json!("off"), None), (json!(null), None)] {
            std::fs::write(
                dir.path().join("config.json"),
                json!({"layers":{"metacognition":{"model":"gateway/combo/chat","variant":choice}}})
                    .to_string(),
            )
            .unwrap();
            let assigned = assigned(&models, dir.path(), Layer::Metacognition).unwrap();
            let mut chat = crate::provider::catalog::resolve_chat(
                &catalog,
                &assigned.resolved,
                "test-key",
                &cache,
            )
            .await
            .unwrap();
            chat.variants.insert(
                "low".into(),
                serde_json::from_value(json!({"reasoning_effort":"low"})).unwrap(),
            );
            assert_eq!(chat_variant(&assigned, &chat).unwrap(), expected);
        }
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"layers":{"metacognition":"gateway/combo/chat"}}"#,
        )
        .unwrap();
        let assigned = assigned(&models, dir.path(), Layer::Metacognition).unwrap();
        let mut chat = crate::provider::catalog::resolve_chat(
            &catalog,
            &assigned.resolved,
            "test-key",
            &cache,
        )
        .await
        .unwrap();
        let low = serde_json::from_value(json!({"reasoning_effort":"low"})).unwrap();
        chat.variants.insert("low".into(), low);
        assert_eq!(
            chat_variant(&assigned, &chat).unwrap(),
            chat.variants.get("low").cloned()
        );
    }
}
