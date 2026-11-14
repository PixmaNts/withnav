//! `nav permissions`: rules, validations and decision history.
use super::*;

pub(super) fn permissions_command(command: PermissionCommand) -> anyhow::Result<i32> {
    use crate::permission::{
        decisions::DecisionFilter,
        rules::{RuleFilter, RuleOrigin, RuleScope},
        store::PermissionStore,
    };
    let data_dir = crate::paths::data_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve the WithNav data dir"))?;
    let store = PermissionStore::open(&data_dir)?;
    match command {
        PermissionCommand::Validate {
            kind,
            name,
            location,
        } => {
            if kind != crate::permission::validation::ValidationKind::Flow {
                anyhow::bail!("skill admission is not available yet");
            }
            let flows = crate::flows::store::FlowStore::with_data(
                std::env::current_dir()?,
                config_dir()?,
                data_dir,
            );
            let location = match location {
                Some(location) => location,
                None => {
                    let matches: Vec<_> = flows
                        .list()
                        .map_err(|e| anyhow::anyhow!("{}", e.reason))?
                        .into_iter()
                        .filter(|flow| {
                            flow.name == name
                                && flow.location != crate::flows::store::Location::Builtin
                        })
                        .collect();
                    if matches.len() != 1 {
                        anyhow::bail!(
                            "select --location global or workspace for an ambiguous or missing flow"
                        );
                    }
                    matches[0].location
                }
            };
            let document = flows
                .get(&name, location)
                .map_err(|e| anyhow::anyhow!("{}", e.reason))?;
            let approval = flows
                .validate_version(&name, location, &document.revision)
                .map_err(|e| anyhow::anyhow!("{}", e.reason))?;
            println!(
                "validated flow {} at {} ({}) as {}",
                name, approval.location, approval.content_sha256, approval.id
            );
        }
        PermissionCommand::Validations { json } => {
            let validations = store.list_validations()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&validations)?);
            } else {
                for validation in validations {
                    println!(
                        "{}\t{}\t{}\t{}\t{}{}",
                        validation.id,
                        validation.kind.as_str(),
                        validation.name,
                        validation.location,
                        validation.content_sha256,
                        if validation.revoked_at.is_some() {
                            "\trevoked"
                        } else {
                            ""
                        }
                    );
                }
            }
        }
        PermissionCommand::RevokeValidation { id } => {
            if !store.revoke_validation(id)? {
                anyhow::bail!("validation {id} not found or already revoked");
            }
            println!("revoked validation {id}; reload flows to apply");
        }
        PermissionCommand::List { all, scope, json } => {
            let rules = store.list(&RuleFilter {
                include_revoked: all,
                scope: scope.scope(),
            })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rules)?);
            } else if rules.is_empty() {
                println!("(no permission rules)");
            } else {
                for rule in rules {
                    println!(
                        "{}\t{}\t{}\t{}\t{:?}{}",
                        rule.id,
                        rule.action,
                        rule.resource,
                        rule.effect.as_str(),
                        rule.scope,
                        if rule.revoked_at.is_some() {
                            "\trevoked"
                        } else {
                            ""
                        }
                    );
                }
            }
        }
        PermissionCommand::Add {
            action,
            resource,
            effect,
            scope,
        } => {
            let scope = match scope.scope() {
                Some(scope) => scope,
                None => RuleScope::Project(std::env::current_dir()?),
            };
            let rule = store.add(&action, &resource, effect, scope, RuleOrigin::Manual)?;
            println!("added permission rule {}", rule.id);
        }
        PermissionCommand::Revoke { id } => {
            if !store.revoke(id)? {
                anyhow::bail!("permission rule {id} not found or already revoked");
            }
            println!("revoked permission rule {id}");
        }
        PermissionCommand::History { limit, json } => {
            let decisions = store.list_decisions(&DecisionFilter::default(), limit)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&decisions)?);
            } else if decisions.is_empty() {
                println!("(no permission decisions)");
            } else {
                for decision in decisions {
                    println!(
                        "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                        decision.id,
                        decision.time,
                        decision.session_id,
                        decision.agent,
                        decision.tool,
                        decision.outcome.as_str(),
                        crate::permission::decisions::scores_summary(&decision)
                    );
                }
            }
        }
    }
    Ok(EXIT_DONE)
}
