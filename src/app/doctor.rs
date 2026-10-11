//! Read-only diagnostics. No startup migration, session, tool execution or cache writes.
use std::{
    collections::BTreeMap,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use serde::Serialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::{
    agent::definitions::{AgentRoots, Agents, AGENT_ALIASES},
    error::ProviderError,
    provider::{
        assignments::{AgentModel, AssignedModel, AssignmentResolver, Layer, LayerAssignment},
        auth,
        catalog::ModelCatalog,
        config::{effective_variants, Variant},
        openai::ClientCache,
        systemone::{Answer, DecisionRequest, Question, Questions, SystemOneClient},
    },
};

pub(crate) const FIM_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const CHAT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum DoctorLayer {
    Reflex,
    Instinct,
    Metacognition,
}
impl DoctorLayer {
    fn layer(self) -> Layer {
        match self {
            Self::Reflex => Layer::Reflex,
            Self::Instinct => Layer::Instinct,
            Self::Metacognition => Layer::Metacognition,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Warning,
    Failed,
}
#[derive(Debug, Serialize)]
pub struct Check {
    pub id: String,
    pub status: Status,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u128>,
}
#[derive(Default, Debug, Serialize)]
pub struct Summary {
    pub ok: usize,
    pub warnings: usize,
    pub failures: usize,
}
#[derive(Default, Debug, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub checks: Vec<Check>,
    pub summary: Summary,
}
impl Report {
    fn add(
        &mut self,
        id: impl Into<String>,
        status: Status,
        value: impl Into<String>,
        hint: Option<&str>,
    ) {
        match status {
            Status::Ok => self.summary.ok += 1,
            Status::Warning => self.summary.warnings += 1,
            Status::Failed => self.summary.failures += 1,
        }
        self.checks.push(Check {
            id: id.into(),
            status,
            value: value.into(),
            category: None,
            hint: if status == Status::Ok {
                None
            } else {
                hint.map(str::to_owned)
            },
            elapsed_ms: None,
        });
    }
    pub fn exit_code(&self) -> i32 {
        i32::from(self.summary.failures != 0)
    }
    pub fn human(&self) -> String {
        let mut text = String::new();
        for check in &self.checks {
            let label = match check.status {
                Status::Ok => "✓ ok",
                Status::Warning => "⚠ warning",
                Status::Failed => "✕ failed",
            };
            text.push_str(&format!("{label} {}: {}", check.id, check.value));
            if let Some(ms) = check.elapsed_ms {
                text.push_str(&format!(" ({ms} ms)"));
            }
            if let Some(hint) = &check.hint {
                text.push_str(&format!("; {hint}"));
            }
            text.push('\n');
        }
        text
    }
}

pub async fn command(args: super::DoctorArgs) -> anyhow::Result<i32> {
    let report = if let Some(config) = auth::default_config_dir() {
        inspect(
            &args,
            &config,
            std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .as_deref(),
            crate::paths::data_dir(),
        )
        .await
    } else {
        let mut report = Report {
            schema_version: 1,
            ..Default::default()
        };
        report.add(
            "config.location",
            Status::Failed,
            "platform configuration directory unavailable",
            Some("Set a usable home/config directory."),
        );
        report
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", report.human());
    }
    Ok(report.exit_code())
}

/// Explicit roots make diagnostics testable without changing process-wide HOME.
pub async fn inspect(
    args: &super::DoctorArgs,
    config: &Path,
    home: Option<&Path>,
    data: Option<std::path::PathBuf>,
) -> Report {
    let mut report = Report {
        schema_version: 1,
        ..Default::default()
    };
    let workspace = args
        .workspace
        .canonicalize()
        .unwrap_or_else(|_| args.workspace.clone());
    report.add(
        "config.location",
        Status::Ok,
        config.display().to_string(),
        None,
    );
    let settings = crate::settings::load(config);
    report.add(
        "config.settings",
        if settings.is_ok() {
            Status::Ok
        } else {
            Status::Failed
        },
        if settings.is_ok() {
            "config.json parsed (defaults if absent)"
        } else {
            "config.json unreadable or invalid"
        },
        Some("Edit config.json; unknown keys are retained."),
    );
    let settings_document = settings
        .as_ref()
        .ok()
        .map(|settings| Value::Object(settings.as_object().clone()));
    unknown_keys(
        &mut report,
        settings_document.as_ref(),
        "config",
        &[
            (
                "",
                &[
                    "layers",
                    "agents",
                    "default_agent",
                    "permissions",
                    "subagents",
                    "agent",
                    "tui",
                    "compaction",
                ],
            ),
            ("layers", &["reflex", "instinct", "metacognition"]),
            (
                "permissions",
                &["automatic_review", "scratch_dirs", "review_timeout_secs"],
            ),
            ("subagents", &["max_concurrent", "watchdog_interval_secs"]),
            ("agent", &["watchdog_interval_secs"]),
            ("compaction", &["writer", "chunk_tokens"]),
            ("tui", &["theme", "favorite_models"]),
        ],
    );
    let credentials = auth::load_store_with_document(config);
    let auth_ok = credentials.is_ok();
    if let Ok((store, document)) = &credentials {
        let names: Vec<_> = store.providers.keys().cloned().collect();
        let sections: Vec<_> = names
            .iter()
            .map(|name| (name.as_str(), &["api_key"][..]))
            .collect();
        unknown_keys(&mut report, Some(document), "auth", &sections);
    }
    let auth_path = config.join("auth.json");
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&auth_path)
        .ok()
        .map(|m| m.permissions().mode() & 0o777);
    report.add(
        "config.auth",
        if auth_ok && mode.is_none_or(|m| m == 0o600) {
            if mode.is_some() {
                Status::Ok
            } else {
                Status::Warning
            }
        } else {
            Status::Failed
        },
        match mode {
            Some(mode) => format!(
                "auth.json mode {mode:04o}; parse {}",
                if auth_ok { "valid" } else { "invalid" }
            ),
            None => "auth.json absent; environment credentials may be used".into(),
        },
        Some("Use nav auth set <provider> and chmod 600 auth.json."),
    );
    let models = crate::provider::config::load_models_with_document(&workspace, config);
    if let Ok((_, path, document)) = &models {
        report.add(
            "config.catalogue",
            Status::Ok,
            path.display().to_string(),
            None,
        );
        unknown_keys(
            &mut report,
            Some(document),
            "models",
            &[("", &["providers", "models"])],
        );
        catalogue_unknown_keys(&mut report, document);
    } else {
        report.add(
            "config.catalogue",
            Status::Failed,
            "models.json missing, unreadable or invalid",
            Some("Check the workspace/user models.json catalogue and provider references."),
        );
    }
    legacy(
        &mut report,
        config,
        home,
        &workspace,
        settings_document.as_ref(),
    );
    workspace_checks(&mut report, config, &workspace);
    binary_checks(&mut report, home, &workspace).await;
    let mut keys = BTreeMap::new();
    if let Ok((models, _, _)) = &models {
        for (name, provider) in &models.providers {
            let key = credentials.as_ref().ok().and_then(|(store, _)| {
                auth::resolve_key_from_store(store, name, provider.api_key_env.as_deref()).ok()
            });
            keys.insert(name.clone(), key);
        }
    }
    if let (Ok(settings), Ok((models, _, _))) = (&settings, &models) {
        let cache = Arc::new(ClientCache::new());
        let catalog = ModelCatalog::read_only(data);
        for name in models.providers.keys() {
            let usable = keys
                .get(name)
                .and_then(Option::as_deref)
                .is_some_and(|k| !k.trim().is_empty());
            report.add(
                format!("auth.provider.{name}"),
                if usable { Status::Ok } else { Status::Failed },
                if usable {
                    "usable credentials"
                } else {
                    "credentials missing, empty or inaccessible"
                },
                Some("Check nav auth set <provider>, environment key and auth.json permissions."),
            );
        }
        let agents = Agents::load(
            &AgentRoots {
                home: home.map(Path::to_path_buf),
                config_dir: Some(config.into()),
            },
            &workspace,
        );
        for (i, diagnostic) in agents.diagnostics().iter().enumerate() {
            report.add(
                format!("agent.definition.{i}"),
                Status::Warning,
                format!(
                    "{}: malformed, deprecated or inaccessible definition",
                    diagnostic.path.display()
                ),
                Some("Check the agent Markdown header and canonical agent id."),
            );
        }
        let resolver = AssignmentResolver::new(models, settings);
        for section in ["layers", "agents"] {
            if let Some(assignments) = settings.get_dotted(section).and_then(Value::as_object) {
                for (id, assignment) in assignments {
                    if let Some(object) = assignment.as_object() {
                        for key in object
                            .keys()
                            .filter(|key| !["model", "variant"].contains(&key.as_str()))
                        {
                            report.add(
                                format!("unknown.config.{section}.{id}.{key}"),
                                Status::Warning,
                                format!(
                                    "unknown assignment key {section}.{id}.{key} (value omitted)"
                                ),
                                None,
                            );
                        }
                    }
                }
            }
        }
        let known: Vec<_> = agents
            .all()
            .map(|a| (a.id.as_str(), a.model.as_deref(), a.mode.is_primary()))
            .collect();
        let default = resolver.default_agent_check(&known);
        report.add(
            "assignment.default_agent",
            if default.is_ok() {
                Status::Ok
            } else {
                Status::Failed
            },
            default.as_ref().cloned().unwrap_or_else(|e| e.to_string()),
            Some("Assign a usable chat model to a known primary default_agent."),
        );
        let mut default_model = None;
        for agent in agents.all() {
            let id = agent.id.as_str();
            let is_default = default.as_ref().is_ok_and(|d| d == id);
            match resolver.agent_model(id, agent.model.as_deref()) {
                Ok(AgentModel::Assigned(assigned)) => {
                    if is_default {
                        default_model = Some((*assigned).clone());
                    }
                    let source = if settings.agent_assignment(id).ok().flatten().is_some() {
                        "config"
                    } else {
                        "Markdown"
                    };
                    report.add(
                        format!("assignment.agent.{id}"),
                        Status::Ok,
                        format!("{} ({source})", assigned_label(&assigned)),
                        None,
                    );
                    verify_chat(
                        &mut report,
                        &format!("agent.{id}"),
                        &assigned,
                        false,
                        is_default,
                        args.offline,
                        &catalog,
                        keys.get(&assigned.resolved.provider_name)
                            .and_then(Option::as_deref),
                        &cache,
                    )
                    .await;
                }
                Ok(AgentModel::Unassigned) => report.add(
                    format!("assignment.agent.{id}"),
                    if is_default {
                        Status::Failed
                    } else {
                        Status::Ok
                    },
                    if agent.mode.is_subagent() {
                        "inherits parent's effective model and variant"
                    } else {
                        "falls back to default agent's effective chat model"
                    },
                    None,
                ),
                Ok(AgentModel::Fallback(reason)) => report.add(
                    format!("assignment.agent.{id}"),
                    if is_default {
                        Status::Failed
                    } else {
                        Status::Warning
                    },
                    reason.to_string(),
                    Some(
                        "Correct the assignment; nondefault agents use the default agent fallback.",
                    ),
                ),
                Err(reason) => report.add(
                    format!("assignment.agent.{id}"),
                    if is_default {
                        Status::Failed
                    } else {
                        Status::Warning
                    },
                    reason,
                    Some("Correct the agent assignment in config.json."),
                ),
            }
        }
        if let Some(configured) = settings.get_dotted("agents").and_then(Value::as_object) {
            for id in configured.keys() {
                if !known.iter().any(|(name, _, _)| name == id) {
                    report.add(
                        format!("agent.configured.{id}"),
                        Status::Warning,
                        "unknown or deprecated configured agent id",
                        Some("Use a discovered canonical agent id."),
                    );
                }
            }
        }
        let mut meta = None;
        // Resolve metacognition even under --layer for the shared compaction assignment.
        if let Ok(LayerAssignment::Enabled(assigned)) =
            resolver.layer_assignment(Layer::Metacognition)
        {
            meta = Some(*assigned);
        }
        for layer in [Layer::Reflex, Layer::Instinct, Layer::Metacognition] {
            if args.layer.is_some_and(|selected| selected.layer() != layer) {
                continue;
            }
            match resolver.layer_assignment(layer) {
                Ok(LayerAssignment::Enabled(assigned)) => {
                    if layer != Layer::Metacognition && assigned.variant.is_some() {
                        report.add(
                            format!("assignment.layer.{layer}"),
                            Status::Warning,
                            "disabled: variants unsupported on this API",
                            Some("Remove the lower-layer variant."),
                        );
                        continue;
                    }
                    report.add(
                        format!("assignment.layer.{layer}"),
                        Status::Ok,
                        assigned_label(&assigned),
                        None,
                    );
                    let variant = if layer == Layer::Metacognition {
                        verify_chat(
                            &mut report,
                            "metacognition",
                            &assigned,
                            true,
                            false,
                            args.offline,
                            &catalog,
                            keys.get(&assigned.resolved.provider_name)
                                .and_then(Option::as_deref),
                            &cache,
                        )
                        .await
                    } else {
                        Some(None)
                    };
                    if args.offline {
                        report.add(
                            format!("live.{layer}"),
                            Status::Warning,
                            "skipped (offline)",
                            None,
                        );
                    } else if let Some(variant) = variant {
                        live_layer(
                            &mut report,
                            layer,
                            &assigned,
                            variant,
                            keys.get(&assigned.resolved.provider_name)
                                .and_then(Option::as_deref),
                            cache.clone(),
                        )
                        .await;
                    }
                }
                Ok(LayerAssignment::Disabled(reason)) => report.add(
                    format!("assignment.layer.{layer}"),
                    Status::Warning,
                    reason.to_string(),
                    Some("Set layers.<layer> to a catalogue model with the required API."),
                ),
                Err(reason) => report.add(
                    format!("assignment.layer.{layer}"),
                    Status::Warning,
                    format!("disabled: {reason}"),
                    Some("Correct the layer assignment in config.json."),
                ),
            }
        }
        match (
            settings.compaction_writer(),
            settings.compaction_chunk_tokens(),
        ) {
            (Ok(writer), Ok(chunk)) => {
                if writer == "metacognition" {
                    if let Some(assigned) = &mut meta {
                        if assigned.prefer_low
                            && catalog
                                .cached(&assigned.resolved)
                                .limits
                                .is_some_and(|limits| {
                                    limits
                                        .effort_tiers
                                        .is_some_and(|tiers| tiers.iter().any(|tier| tier == "low"))
                                })
                        {
                            assigned.variant = Some("low".into());
                        }
                    }
                }
                let assigned = if writer == "session" {
                    default_model.as_ref()
                } else {
                    meta.as_ref()
                };
                report.add(
                    "assignment.compaction",
                    if assigned.is_some() {
                        Status::Ok
                    } else {
                        Status::Warning
                    },
                    format!(
                        "writer={writer}, model={}, chunk_tokens={chunk}{}",
                        assigned
                            .map(assigned_label)
                            .unwrap_or_else(|| "unavailable".into()),
                        if writer == "session" {
                            " (new default session; other sessions use their effective model)"
                        } else {
                            ""
                        }
                    ),
                    Some("Configure compaction.writer and its usable chat assignment."),
                );
            }
            _ => report.add(
                "assignment.compaction",
                Status::Failed,
                "invalid compaction.writer or compaction.chunk_tokens",
                Some("Use writer metacognition/session and a positive chunk_tokens integer."),
            ),
        }
    }
    // Never echo credential values, including when a malicious name/path repeats one.
    let mut secrets = vec![];
    if let Ok((store, _)) = &credentials {
        secrets.extend(store.providers.values().map(|e| e.api_key.clone()));
    }
    secrets.extend(keys.values().flatten().cloned());
    if let Ok((models, _, _)) = &models {
        for provider in models.providers.values() {
            secrets.extend(provider.headers.values().cloned());
        }
    }
    for check in &mut report.checks {
        check.id = sanitize(&check.id, &secrets);
        check.value = sanitize(&check.value, &secrets);
        check.hint = check.hint.as_ref().map(|h| sanitize(h, &secrets));
    }
    report
}

fn assigned_label(model: &AssignedModel) -> String {
    format!(
        "{} variant={}",
        model.name,
        model.variant.as_deref().unwrap_or("off")
    )
}

#[allow(clippy::too_many_arguments)]
async fn verify_chat(
    report: &mut Report,
    id: &str,
    assigned: &AssignedModel,
    prefer_low: bool,
    required: bool,
    offline: bool,
    catalog: &ModelCatalog,
    key: Option<&str>,
    cache: &ClientCache,
) -> Option<Option<Variant>> {
    let resolved = &assigned.resolved;
    let needs_metadata = resolved.model.context_window.is_none()
        || (resolved.model.variants.is_none() && resolved.provider.catalog_format.is_some());
    let data = if offline || !needs_metadata {
        catalog.cached(resolved)
    } else if let Some(key) = key {
        catalog.lookup(resolved, key, cache).await
    } else {
        catalog.cached(resolved)
    };
    let chat = crate::provider::catalog::chat_from_data(resolved, Some(data.clone()));
    let limits = data.limits.clone().unwrap_or_default();
    let variants = effective_variants(resolved, limits.effort_tiers.as_deref().unwrap_or(&[]));
    let name = if prefer_low && assigned.prefer_low && variants.contains_key("low") {
        Some("low")
    } else {
        assigned.variant.as_deref()
    };
    let unknown_offer = resolved.model.variants.is_none()
        && resolved.provider.catalog_format.is_some()
        && limits.effort_tiers.is_none();
    if unknown_offer {
        report.add(
            format!("variant.{id}"),
            Status::Warning,
            if offline {
                "offer cannot be verified offline"
            } else {
                "provider variant offer unavailable"
            },
            Some("Set explicit variants in models.json or retry catalogue discovery."),
        );
        if name.is_some() {
            return None;
        }
    } else if name.is_some_and(|name| !variants.contains_key(name)) {
        report.add(
            format!("variant.{id}"),
            if required {
                Status::Failed
            } else {
                Status::Warning
            },
            format!("invalid selected variant {}", name.unwrap_or("off")),
            Some("Choose an offered variant or explicitly set variant off."),
        );
        return None;
    } else {
        report.add(
            format!("variant.{id}"),
            Status::Ok,
            format!("effective variant={}", name.unwrap_or("off")),
            None,
        );
    }
    if let Ok(chat) = &chat {
        let valid_budget = crate::agent::context::ContextLimits::from_resolution(
            &assigned.name.to_string(),
            name,
            &chat.snapshot,
            resolved.model.max_output_tokens,
        )
        .is_ok();
        report.add(
            format!("context.{id}"),
            if valid_budget {
                Status::Ok
            } else if required {
                Status::Failed
            } else {
                Status::Warning
            },
            format!(
                "{} tokens; budget {}",
                chat.window,
                if valid_budget { "valid" } else { "unusable" }
            ),
            if valid_budget {
                None
            } else {
                Some("Correct the context/output limits in models.json.")
            },
        );
    } else {
        report.add(
            format!("context.{id}"),
            if offline {
                Status::Warning
            } else if required {
                Status::Failed
            } else {
                Status::Warning
            },
            "context window unavailable",
            Some("Set context_window in models.json or restore provider catalogue access."),
        );
    }
    if data.stale {
        report.add(
            format!("catalogue.{id}"),
            Status::Warning,
            "cached metadata is stale",
            Some("Retry with a reachable provider."),
        );
    }
    Some(name.and_then(|name| variants.get(name).cloned()))
}

fn unknown_keys(
    report: &mut Report,
    value: Option<&Value>,
    prefix: &str,
    sections: &[(&str, &[&str])],
) {
    let Some(value) = value else {
        return;
    };
    for (section, known) in sections {
        let object = if section.is_empty() {
            value.as_object()
        } else {
            value.get(section).and_then(Value::as_object)
        };
        if let Some(object) = object {
            for key in object.keys().filter(|key| !known.contains(&key.as_str())) {
                report.add(
                    format!("unknown.{prefix}.{section}.{key}"),
                    Status::Warning,
                    format!("unknown key {section}.{key} (value omitted)"),
                    Some("Check spelling; unknown keys are not rejected or changed."),
                );
            }
        }
    }
}
fn catalogue_unknown_keys(report: &mut Report, value: &Value) {
    for (section, known) in [
        (
            "providers",
            &[
                "base_url",
                "api_key_env",
                "headers",
                "connect_timeout_ms",
                "request_timeout_ms",
                "catalog_format",
            ][..],
        ),
        (
            "models",
            &[
                "provider",
                "model",
                "api",
                "context_window",
                "max_input_tokens",
                "max_message_tokens",
                "max_output_tokens",
                "supports_tools",
                "supports_parallel_tool_calls",
                "thinking",
                "variants",
                "default_variant",
                "reasoning_history",
            ][..],
        ),
    ] {
        if let Some(entries) = value.get(section).and_then(Value::as_object) {
            for (id, entry) in entries {
                if let Some(entry) = entry.as_object() {
                    for key in entry.keys().filter(|k| !known.contains(&k.as_str())) {
                        report.add(
                            format!("unknown.models.{section}.{id}.{key}"),
                            Status::Warning,
                            format!("unknown key {section}.{id}.{key} (value omitted)"),
                            None,
                        );
                    }
                }
            }
        }
    }
}

fn legacy(
    report: &mut Report,
    config: &Path,
    home: Option<&Path>,
    workspace: &Path,
    config_value: Option<&Value>,
) {
    let mut paths = vec![(config.with_file_name("navai"), config.to_path_buf())];
    for ancestor in workspace.ancestors() {
        for name in [".navai", ".navigator"] {
            paths.push((ancestor.join(name), ancestor.join(".withnav")));
        }
    }
    for (old, new) in paths {
        if old.exists() {
            report.add(
                format!("legacy.path.{}", old.display()),
                Status::Warning,
                old.display().to_string(),
                Some(&format!(
                    "Migrate manually to {} (doctor never moves files).",
                    new.display()
                )),
            );
        }
    }
    for (name, _) in std::env::vars_os() {
        let name = name.to_string_lossy();
        if let Some(suffix) = name.strip_prefix("NAVAI_") {
            report.add(
                format!("legacy.env.{name}"),
                Status::Warning,
                name.as_ref(),
                Some(&format!("Use WITHNAV_{suffix}; environment value omitted.")),
            );
        }
    }
    for (old, canonical) in AGENT_ALIASES {
        let used = config_value.as_ref().is_some_and(|v| {
            v.get("default_agent").and_then(Value::as_str) == Some(old)
                || v.get("agents")
                    .and_then(Value::as_object)
                    .is_some_and(|a| a.contains_key(*old))
        });
        if used {
            report.add(
                format!("legacy.agent.config.{old}"),
                Status::Warning,
                *old,
                Some(&format!("Use canonical agent id {canonical}.")),
            );
        }
    }
    let mut dirs = vec![config.join("agents")];
    if let Some(home) = home {
        dirs.push(home.join(".config/opencode/agents"));
    }
    for ancestor in workspace.ancestors() {
        dirs.push(crate::paths::workspace_dir(ancestor).join("agents"));
    }
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.take(crate::agent::definitions::MAX_FILES).flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "md") {
                continue;
            }
            // Only a bounded regular definition, never a symlink to workspace contents.
            let text = entry
                .file_type()
                .ok()
                .filter(|t| t.is_file())
                .and_then(|_| std::fs::metadata(&path).ok())
                .filter(|m| m.len() <= crate::agent::definitions::MAX_FILE_BYTES as u64)
                .and_then(|_| std::fs::read_to_string(&path).ok());
            for (old, canonical) in AGENT_ALIASES {
                if path.file_stem().and_then(|s| s.to_str()) == Some(old)
                    || text.as_ref().is_some_and(|s| {
                        s.split(|c: char| !c.is_alphanumeric() && c != '_')
                            .any(|word| word == *old)
                    })
                {
                    report.add(
                        format!("legacy.agent.{}.{old}", path.display()),
                        Status::Warning,
                        format!("{} references {old}", path.display()),
                        Some(&format!(
                            "Use canonical agent id {canonical}; review references manually."
                        )),
                    );
                }
            }
        }
    }
}

fn workspace_checks(report: &mut Report, config: &Path, workspace: &Path) {
    report.add(
        "workspace.directory",
        if workspace.is_dir() {
            Status::Ok
        } else {
            Status::Failed
        },
        workspace.display().to_string(),
        Some("Select an existing directory with --workspace."),
    );
    let withnav = workspace.join(".withnav");
    report.add(
        "workspace.withnav",
        if withnav.is_dir() {
            Status::Ok
        } else {
            Status::Warning
        },
        withnav.display().to_string(),
        Some("Create .withnav/ for workspace configuration."),
    );
    let intent = withnav.join("intent.md");
    report.add(
        "workspace.intent",
        if std::fs::File::open(&intent).is_ok() {
            Status::Ok
        } else {
            Status::Warning
        },
        intent.display().to_string(),
        Some("Create a readable .withnav/intent.md."),
    );
    let trust = crate::flows::store::inspect_trust(config, workspace);
    report.add(
        "flows.trust_file",
        if trust.is_ok() {
            Status::Ok
        } else {
            Status::Failed
        },
        if trust.is_ok() {
            "valid or absent flows-trust.json"
        } else {
            "invalid or unsafe flows-trust.json"
        },
        Some("Review flows-trust.json schema, ownership and permissions."),
    );
    let trusted = trust.unwrap_or(false);
    report.add(
        "flows.workspace_trust",
        if trusted { Status::Ok } else { Status::Warning },
        if trusted {
            "workspace explicitly trusted"
        } else {
            "untrusted: workspace flow validation skipped; no workspace flow read"
        },
        Some("Explicitly trust workspace flows in WithNav before validating them."),
    );
    for (definition, templates) in crate::flows::builtin::definitions() {
        let name = definition.flow.name.clone();
        let valid = crate::flows::store::validate(crate::flows::store::Content::Graph {
            definition: Box::new(definition),
            templates,
        })
        .valid;
        report.add(
            format!("flows.builtin.{name}"),
            if valid { Status::Ok } else { Status::Failed },
            "schema and templates validated",
            None,
        );
    }
    for (scope, path) in [
        ("global", config.join("flows")),
        (
            "workspace",
            crate::paths::workspace_dir(workspace).join("flows"),
        ),
    ] {
        if scope == "workspace" && !trusted {
            continue;
        }
        match crate::flows::store::inspect_directory(&path) {
            Ok(entries) if entries.is_empty() => report.add(
                format!("flows.{scope}"),
                Status::Warning,
                "no optional flows",
                None,
            ),
            Ok(entries) => {
                for (name, valid) in entries {
                    report.add(
                        format!("flows.{scope}.{name}"),
                        if valid { Status::Ok } else { Status::Failed },
                        if valid {
                            "schema and templates validated"
                        } else {
                            "invalid schema or unreadable template"
                        },
                        Some("Review flow TOML/templates; doctor does not reload flows."),
                    );
                }
            }
            Err(_) => report.add(
                format!("flows.{scope}"),
                Status::Failed,
                "unsafe, oversized or unreadable flow directory/document",
                Some("Check ownership, symlinks, byte limits and flow definitions."),
            ),
        }
    }
}

async fn binary_checks(report: &mut Report, home: Option<&Path>, workspace: &Path) {
    report.add("binary.nav", Status::Ok, env!("CARGO_PKG_VERSION"), None);
    match crate::tools::bash::trusted_shell() {
        Some(path) => report.add(
            "binary.bash",
            Status::Ok,
            format!(
                "{} (regular, root-owned, not group/other writable)",
                path.display()
            ),
            None,
        ),
        None => report.add(
            "binary.bash",
            Status::Failed,
            "no trusted /usr/bin/bash or /bin/bash",
            Some("Install a root-owned bash without group/other write permissions."),
        ),
    }
    let lsp = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|p| p.join("nav"))
            .find(|p| p.is_file())
    });
    if let Some(path) = lsp {
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            tokio::process::Command::new(&path)
                .arg("lsp")
                .arg("--version")
                .kill_on_drop(true)
                .output(),
        )
        .await;
        let version = result
            .ok()
            .and_then(Result::ok)
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.split_whitespace().last().map(str::to_owned))
            .filter(|s| s.len() < 64 && s.chars().all(|c| c.is_ascii_digit() || ".-+".contains(c)));
        let matching = version.as_deref() == Some(env!("CARGO_PKG_VERSION"));
        report.add(
            "binary.lsp",
            if matching {
                Status::Ok
            } else {
                Status::Warning
            },
            format!(
                "{} lsp version={}{}",
                path.display(),
                version.as_deref().unwrap_or("undetectable"),
                if matching {
                    " (matches nav)"
                } else {
                    " (missing/mismatched version)"
                }
            ),
            Some("The editor server is nav lsp; install/update nav."),
        );
    } else {
        report.add(
            "binary.lsp",
            Status::Warning,
            "nav not detected on PATH (the editor server is nav lsp)",
            Some("Install nav or configure the extension server path."),
        );
    }
    let mut settings = vec![workspace.join(".vscode/settings.json")];
    if let Some(home) = home {
        settings.push(home.join(".config/Code/User/settings.json"));
    }
    let mut detected = false;
    for path in settings {
        let value = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok());
        if let Some(server) = value
            .as_ref()
            .and_then(|v| v.get("withnav.serverPath"))
            .and_then(Value::as_str)
        {
            detected = true;
            // Match the extension's migration of the former server executable.
            let configured = Path::new(server);
            let migrated = match configured.file_name().and_then(|s| s.to_str()) {
                Some("withnav-lsp" | "navai-lsp") => configured.with_file_name("nav"),
                _ => configured.to_path_buf(),
            };
            let exists = migrated.is_file()
                || (migrated == Path::new("nav")
                    && std::env::var_os("PATH").is_some_and(|paths| {
                        std::env::split_paths(&paths).any(|p| p.join("nav").is_file())
                    }));
            report.add(
                format!("extension.server_path.{}", path.display()),
                if exists {
                    Status::Ok
                } else {
                    Status::Warning
                },
                server,
                Some("Set withnav.serverPath to nav (the default) or an existing nav binary; the extension runs nav lsp and migrates legacy withnav-lsp paths."),
            );
        }
    }
    if !detected {
        report.add(
            "extension.server_path",
            Status::Warning,
            "not detectable (settings absent, JSONC, remote or other VS Code profile)",
            Some("Check the extension server-path setting in your active VS Code profile."),
        );
    }
}

fn sanitize(text: &str, secrets: &[String]) -> String {
    let mut text = text.to_owned();
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        text = text.replace(secret, "[redacted]");
    }
    text.split_whitespace()
        .map(|word| {
            if word.contains("://") {
                "[url omitted]".to_string()
            } else {
                word.chars().filter(|c| !c.is_control()).collect()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn error_category(error: &ProviderError) -> (&'static str, String, &'static str) {
    match error {
        ProviderError::Api {
            status: status @ 400..=499,
            ..
        } => (
            "http_rejected",
            format!("HTTP {status} rejected"),
            "Check provider authentication, model id and API support.",
        ),
        ProviderError::Api {
            status: status @ 500..=599,
            ..
        } => (
            "http_server",
            format!("HTTP {status} server error"),
            "Retry later; check provider service health.",
        ),
        ProviderError::Api { status: 0, .. } => (
            "timeout",
            "deadline exceeded".into(),
            "Retry or choose a faster model; check the endpoint connection.",
        ),
        ProviderError::Http(e) if e.is_timeout() => (
            "timeout",
            "request timed out".into(),
            "Check connection or retry later.",
        ),
        ProviderError::Http(_) => (
            "unreachable",
            "endpoint unreachable".into(),
            "Check endpoint connection, DNS and service availability.",
        ),
        ProviderError::Auth(_) | ProviderError::MissingApiKey => (
            "auth",
            "credentials unavailable".into(),
            "Check nav auth and auth.json permissions.",
        ),
        _ => (
            "invalid_answer",
            "invalid or missing typed answer".into(),
            "Check model/API compatibility or retry.",
        ),
    }
}

async fn measured<F>(report: &mut Report, id: &str, future: F)
where
    F: std::future::Future<Output = Result<(), ProviderError>>,
{
    let start = Instant::now();
    match future.await {
        Ok(()) => report.add(id, Status::Ok, "valid answer", None),
        Err(error) => {
            let (category, value, hint) = error_category(&error);
            report.add(id, Status::Failed, value, Some(hint));
            if let Some(check) = report.checks.last_mut() {
                check.category = Some(category.into());
            }
        }
    }
    if let Some(check) = report.checks.last_mut() {
        check.elapsed_ms = Some(start.elapsed().as_millis());
    }
}

/// Shared bounded synthetic inputs for the doctor and calibration probes.
pub fn reflex_requests() -> [crate::navigator::ports::FimRequest; 2] {
    let doc = "fn main() {\n    let xs = vec![1, 2, 3];\n    ";
    let context = "intent: port the reflex layer of the navigator\n\
        sig: pub fn suggest(&self, uri, text, version, line, character, context, language)\n\
        notes: prefix capped at 16 KiB, suffix at 8 KiB, debounce 150 ms, \
        newer request cancels older, drop if version moved\n"
        .repeat(6);
    [
        crate::navigator::reflex::build_fim_request(doc, doc.len(), "", "rust"),
        crate::navigator::reflex::build_fim_request(doc, doc.len(), &context, "rust"),
    ]
}

pub fn editor_questions() -> Questions {
    let mut questions = Questions::new();
    questions.insert(
        "worth_attention".into(),
        Question::Noul {
            instructions: "Does this change warrant a pair-programming remark?".into(),
        },
    );
    questions.insert(
        "route".into(),
        Question::Choice {
            instructions: "Who should handle this change?".into(),
            criteria: BTreeMap::from([
                ("silent".into(), "Nothing worth saying".into()),
                ("metacognition".into(), "A short code remark".into()),
                (
                    "cognition".into(),
                    "A deeper architecture discussion".into(),
                ),
            ]),
        },
    );
    questions
}

pub fn valid_answers(questions: &Questions, answers: &BTreeMap<String, Answer>) -> bool {
    questions
        .iter()
        .all(|(name, question)| match (question, answers.get(name)) {
            (Question::Noul { .. }, Some(Answer::Noul { noul })) => {
                noul.is_finite() && (0.0..=1.0).contains(noul)
            }
            (
                Question::Choice { criteria, .. },
                Some(Answer::Choice {
                    choice,
                    confidence,
                    probabilities,
                }),
            ) => {
                criteria.contains_key(choice)
                    && confidence.is_finite()
                    && (0.0..=1.0).contains(confidence)
                    && probabilities.iter().all(|(option, probability)| {
                        criteria.contains_key(option)
                            && probability.is_finite()
                            && (0.0..=1.0).contains(probability)
                    })
            }
            _ => false,
        })
}

pub async fn decision_check(
    client: &SystemOneClient,
    state: &str,
    questions: &Questions,
) -> Result<(), ProviderError> {
    let response = client
        .decide(
            &DecisionRequest { state, questions },
            &CancellationToken::new(),
        )
        .await?;
    if valid_answers(questions, &response.answers) {
        Ok(())
    } else {
        Err(ProviderError::MalformedResponse(
            "invalid typed answers".into(),
        ))
    }
}

async fn live_layer(
    report: &mut Report,
    layer: Layer,
    assigned: &AssignedModel,
    variant: Option<Variant>,
    key: Option<&str>,
    cache: Arc<ClientCache>,
) {
    let resolved = &assigned.resolved;
    let key = match key {
        Some(key) if !key.trim().is_empty() => key.to_owned(),
        _ => {
            report.add(
                format!("live.{layer}"),
                Status::Failed,
                "credentials unavailable",
                Some("Configure usable provider credentials."),
            );
            return;
        }
    };
    match layer {
        Layer::Reflex => {
            let engine = crate::provider::fim::InceptionFim::new(resolved.clone(), key, cache);
            for (label, request) in ["plain", "context"].into_iter().zip(reflex_requests()) {
                measured(report, &format!("live.reflex.{label}"), async {
                    let result = tokio::time::timeout(
                        FIM_TIMEOUT,
                        engine.complete(
                            &request.prompt,
                            &request.suffix,
                            request.max_tokens,
                            &CancellationToken::new(),
                        ),
                    )
                    .await
                    .map_err(|_| timeout_error())??;
                    if result.text.trim().is_empty() {
                        Err(ProviderError::MalformedResponse("empty FIM answer".into()))
                    } else {
                        Ok(())
                    }
                })
                .await;
            }
        }
        Layer::Instinct => {
            let client = SystemOneClient::with_key(
                &resolved.provider_name,
                &resolved.provider,
                &resolved.model,
                key,
                cache,
            );
            let client = match client {
                Ok(client) => client,
                Err(error) => {
                    measured(report, "live.instinct", async { Err(error) }).await;
                    return;
                }
            };
            measured(
                report,
                "live.instinct.editor",
                decision_check(
                    &client,
                    "File: src/main.rs\nDiff:\n+use std::sync::Arc;\n+let pool = Arc::new(pool);",
                    &editor_questions(),
                ),
            )
            .await;
            let input = role_input();
            measured(
                report,
                "live.instinct.role_guard",
                decision_check(&client, &input.state, &input.questions()),
            )
            .await;
            let state = serde_json::json!({
                "rules": crate::agent::permission_gate::TRUST_RULE,
                "host_facts": {"workspace": "/workspace", "writable_roots": ["/workspace"]},
                "captain_request": "Inspect git status in this repository",
                "captain_request_complete": true,
                "untrusted_evidence": {"arguments": {"command": "git status --short"}}
            })
            .to_string();
            measured(
                report,
                "live.instinct.permission",
                decision_check(
                    &client,
                    &state,
                    &crate::agent::permission_gate::permission_questions(),
                ),
            )
            .await;
        }
        Layer::Metacognition => {
            measured(report, "live.metacognition.chat", async {
                let client = crate::provider::client::ModelClient::new(
                    &resolved.provider_name,
                    &resolved.provider,
                    key,
                    &cache,
                )?;
                let request = crate::provider::openai::ChatRequest {
                    model: resolved.model.model.clone(),
                    messages: vec![crate::provider::openai::ChatMessage::User {
                        content: "Reply with OK only.".into(),
                        provenance: Default::default(),
                    }],
                    tools: vec![],
                    stream: true,
                    stream_options: None,
                    max_tokens: Some(64),
                    temperature: None,
                    extra: Default::default(),
                };
                let answer = tokio::time::timeout(
                    CHAT_TIMEOUT,
                    client.chat(
                        request,
                        variant.as_ref(),
                        &CancellationToken::new(),
                        &mut |_| {},
                    ),
                )
                .await
                .map_err(|_| timeout_error())??;
                if answer.text.trim().is_empty() || !answer.tool_calls.is_empty() {
                    Err(ProviderError::MalformedResponse(
                        "empty or tool-using chat answer".into(),
                    ))
                } else {
                    Ok(())
                }
            })
            .await;
        }
    }
}
fn timeout_error() -> ProviderError {
    ProviderError::Api {
        status: 0,
        message: "deadline exceeded".into(),
        partial: None,
    }
}

pub fn role_input() -> crate::agent::guard::GuardInput {
    crate::agent::guard::GuardInput::describe(&crate::agent::guard::CallFacts {
        role_id: "plan",
        role_prompt:
            "The plan agent researches and produces markdown plans. It never modifies source code.",
        task: Some("Plan the compiler restructuring"),
        workspace: Path::new("/workspace"),
        tool: "write",
        target: Some(Path::new("/workspace/src/compiler.rs")),
        arguments: &json!({"path":"/workspace/src/compiler.rs","content":"pub fn compile() {}"}),
    })
    .unwrap_or_else(|| crate::agent::guard::GuardInput {
        instructions: "Judge role conformance".into(),
        state: "plan role source edit".into(),
    })
}

#[cfg(test)]
mod tests;
