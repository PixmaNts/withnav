//! `nav permissions`: rules, validations and decision history via the background owner.
use anyhow::Context as _;

use super::*;
use crate::app::connect::{Wanted, ensure_server};
use crate::permission::decisions::Decision;
use crate::permission::rules::{Rule, RuleScope};
use crate::permission::validation::Validation;

fn resolve_project_path(path: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    let resolved = std::fs::canonicalize(path)
        .with_context(|| format!("project {} is not accessible", path.display()))?;
    let resolved = crate::tools::canonical_path(&resolved)?;
    Ok(resolved)
}

pub(super) async fn permissions_command(command: PermissionCommand) -> anyhow::Result<i32> {
    let wanted = Wanted::from_env(None)?;
    let owner = ensure_server(&wanted).await?;

    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let auth_header = format!("Bearer {}", owner.token);

    match command {
        PermissionCommand::Validate {
            kind,
            name,
            location,
        } => {
            if kind != crate::permission::validation::ValidationKind::Flow {
                anyhow::bail!("skill admission is not available yet");
            }
            let url = format!("{}/v1/permissions/validate", owner.url);
            let workspace = resolve_project_path(&std::env::current_dir()?)?;
            let response = client
                .post(&url)
                .header(reqwest::header::AUTHORIZATION, &auth_header)
                .json(&serde_json::json!({
                    "kind": kind,
                    "name": name,
                    "location": location,
                    "workspace": workspace,
                }))
                .send()
                .await?;

            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            if !status.is_success() {
                anyhow::bail!("{}", extract_error(status, &body));
            }
            let approval: Validation = serde_json::from_str(&body)?;
            println!(
                "validated flow {} at {} ({}) as {}",
                name, approval.location, approval.content_sha256, approval.id
            );
        }
        PermissionCommand::Validations { json } => {
            let url = format!("{}/v1/permissions/validations", owner.url);
            let response = client
                .get(&url)
                .header(reqwest::header::AUTHORIZATION, &auth_header)
                .send()
                .await?;

            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            if !status.is_success() {
                anyhow::bail!("{}", extract_error(status, &body));
            }
            let validations: Vec<Validation> = serde_json::from_str(&body)?;
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
            let workspace = resolve_project_path(&std::env::current_dir()?)?;
            let url = format!("{}/v1/permissions/validations/{}", owner.url, id);
            let response = client
                .delete(&url)
                .header(reqwest::header::AUTHORIZATION, &auth_header)
                .query(&[("workspace", workspace.to_str().unwrap_or_default())])
                .send()
                .await?;

            let status = response.status();
            if status == reqwest::StatusCode::NOT_FOUND {
                anyhow::bail!("validation {id} not found or already revoked");
            }
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                anyhow::bail!("{}", extract_error(status, &body));
            }
            println!("revoked validation {id}; reload flows to apply");
        }
        PermissionCommand::List { all, scope, json } => {
            let url = format!("{}/v1/permissions/rules", owner.url);
            let mut req = client
                .get(&url)
                .header(reqwest::header::AUTHORIZATION, &auth_header);
            if all {
                req = req.query(&[("all", "true")]);
            }
            match scope.scope() {
                Some(RuleScope::Global) => {
                    req = req.query(&[("global", "true")]);
                }
                Some(RuleScope::Project(p)) => {
                    let canonical = resolve_project_path(&p)?;
                    req = req.query(&[("project", canonical.to_str().unwrap_or_default())]);
                }
                Some(RuleScope::Agent(a)) => {
                    req = req.query(&[("agent", a.as_str())]);
                }
                None => {}
            }
            let response = req.send().await?;
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            if !status.is_success() {
                anyhow::bail!("{}", extract_error(status, &body));
            }
            let rules: Vec<Rule> = serde_json::from_str(&body)?;
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
                Some(RuleScope::Project(p)) => {
                    RuleScope::Project(resolve_project_path(&p)?)
                }
                Some(scope) => scope,
                None => RuleScope::Project(resolve_project_path(&std::env::current_dir()?)?),
            };
            let url = format!("{}/v1/permissions/rules", owner.url);
            let response = client
                .post(&url)
                .header(reqwest::header::AUTHORIZATION, &auth_header)
                .json(&serde_json::json!({
                    "action": action,
                    "resource": resource,
                    "effect": effect,
                    "scope": scope,
                }))
                .send()
                .await?;

            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            if !status.is_success() {
                anyhow::bail!("{}", extract_error(status, &body));
            }
            let rule: Rule = serde_json::from_str(&body)?;
            println!("added permission rule {}", rule.id);
        }
        PermissionCommand::Revoke { id } => {
            let url = format!("{}/v1/permissions/rules/{}", owner.url, id);
            let response = client
                .delete(&url)
                .header(reqwest::header::AUTHORIZATION, &auth_header)
                .send()
                .await?;

            let status = response.status();
            if status == reqwest::StatusCode::NOT_FOUND {
                anyhow::bail!("permission rule {id} not found or already revoked");
            }
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                anyhow::bail!("{}", extract_error(status, &body));
            }
            println!("revoked permission rule {id}");
        }
        PermissionCommand::History { limit, json } => {
            let url = format!("{}/v1/permissions/history", owner.url);
            let req = client
                .get(&url)
                .header(reqwest::header::AUTHORIZATION, &auth_header)
                .query(&[("limit", limit.to_string())]);
            let response = req.send().await?;
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            if !status.is_success() {
                anyhow::bail!("{}", extract_error(status, &body));
            }
            let decisions: Vec<Decision> = serde_json::from_str(&body)?;
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

fn extract_error(status: reqwest::StatusCode, body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body)
        && let Some(err) = value.get("error").and_then(|e| e.as_str())
    {
        return err.to_string();
    }
    format!("HTTP {status}: {body}")
}
