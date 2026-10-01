//! Live latency probe for the reflex layer's engine: 10 calls against the
//! real Inception endpoint with and without a ~300-token reflex context
//! block, reporting p50/p95 for each. Not run by CI — invoke explicitly:
//!
//! ```text
//! cargo test --release --example reflex_latency_probe -- --ignored --nocapture
//! ```
//!
//! The API key resolves from the auth store (`inception` entry of
//! `~/.config/rust-agent/auth.json`); it is never printed.

use std::sync::Arc;
use std::time::Duration;

use rust_agent::navigator::reflex::{build_fim_request, MAX_CONTEXT_BYTES};
use rust_agent::provider::auth;
use rust_agent::provider::config::ModelsConfig;
use rust_agent::provider::fim::InceptionFim;
use rust_agent::provider::openai::ClientCache;

const CALLS: usize = 10;

fn percentile(mut samples: Vec<Duration>, p: f64) -> Duration {
    samples.sort();
    let idx = ((samples.len() as f64 - 1.0) * p).round() as usize;
    samples[idx.min(samples.len() - 1)]
}

fn report(label: &str, samples: &[Duration]) {
    let v: Vec<Duration> = samples.to_vec();
    println!(
        "{label}: p50 {:.0} ms, p95 {:.0} ms (min {:.0}, max {:.0}, n={})",
        percentile(v.clone(), 0.50).as_secs_f64() * 1000.0,
        percentile(v.clone(), 0.95).as_secs_f64() * 1000.0,
        v.iter()
            .map(|d| d.as_secs_f64() * 1000.0)
            .fold(f64::INFINITY, f64::min),
        v.iter()
            .map(|d| d.as_secs_f64() * 1000.0)
            .fold(f64::NEG_INFINITY, f64::max),
        v.len(),
    );
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Resolve models.json the way the navigator does: the workspace copy
    // first (it carries the navigator aliases), the user dir second.
    let workspace = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let config_dir = auth::default_config_dir().ok_or_else(|| anyhow::anyhow!("no config dir"))?;
    let models_path = rust_agent::navigator::resolve_models(None, &workspace)
        .unwrap_or_else(|| config_dir.join("models.json"));
    let models = ModelsConfig::load(&models_path)?;
    let resolved = models.resolve("reflex")?;
    let api_key = auth::resolve_key(
        &config_dir,
        &resolved.provider_name,
        resolved.provider.api_key_env.as_deref(),
    )?;
    let engine = InceptionFim::new(resolved, api_key, Arc::new(ClientCache::new()));
    let cancel = tokio_util::sync::CancellationToken::new();

    // A realistic middle-of-function cursor position.
    let doc = "fn main() {\n    let xs = vec![1, 2, 3];\n    ";
    let offset = doc.len();
    // ~300 tokens of reflex context (capped at MAX_CONTEXT_BYTES bytes).
    let reflex_context = "intent: port the reflex layer of the navigator\n\
        sig: pub fn suggest(&self, uri, text, version, line, character, context, language)\n\
        notes: prefix capped at 16 KiB, suffix at 8 KiB, debounce 150 ms, \
        newer request cancels older, drop if version moved\n"
        .repeat(6);

    let req_plain = build_fim_request(doc, offset, "", "rust");
    let req_ctx = build_fim_request(doc, offset, &reflex_context, "rust");
    println!(
        "context block: {} bytes (cap {MAX_CONTEXT_BYTES}), prompt plain {} B / with context {} B",
        req_ctx.prompt.len() - req_plain.prompt.len(),
        req_plain.prompt.len(),
        req_ctx.prompt.len()
    );

    let mut plain = Vec::new();
    let mut with_ctx = Vec::new();
    for i in 0..CALLS {
        let a = engine
            .complete(
                &req_plain.prompt,
                &req_plain.suffix,
                req_plain.max_tokens,
                &cancel,
            )
            .await?;
        plain.push(a.latency);
        let b = engine
            .complete(
                &req_ctx.prompt,
                &req_ctx.suffix,
                req_ctx.max_tokens,
                &cancel,
            )
            .await?;
        with_ctx.push(b.latency);
        println!(
            "call {i}: plain {:.0} ms ({}, {} in / {} out), with context {:.0} ms ({}, {} in / {} out)",
            a.latency.as_secs_f64() * 1000.0,
            a.text.len(),
            a.usage.prompt_tokens,
            a.usage.completion_tokens,
            b.latency.as_secs_f64() * 1000.0,
            b.text.len(),
            b.usage.prompt_tokens,
            b.usage.completion_tokens,
        );
    }
    report("plain        ", &plain);
    report("with context ", &with_ctx);
    Ok(())
}
