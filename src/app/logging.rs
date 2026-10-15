//! Subscriber-level exclusion of MCP wire payloads, independent of user verbosity.
use tracing_subscriber::filter::FilterFn;

/// rmcp 3.5 logs peer instructions and notifications at INFO (2026-10-13).
/// WARN response errors also carry server-controlled messages. Exclude the whole dependency;
/// the host reports bounded, redacted failures. User directives cannot reopen payload logging.
pub fn mcp_safe_filter() -> FilterFn<impl Fn(&tracing::Metadata<'_>) -> bool> {
    tracing_subscriber::filter::filter_fn(|metadata| !metadata.target().starts_with("rmcp"))
}
