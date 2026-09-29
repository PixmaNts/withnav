//! The navigator: a colleague over the driver's shoulder with four
//! cooperating layers — reflex (fill-in-the-middle suggestions), instinct
//! (typed decision questions), metacognition (fast remarks) and cognition
//! (deliberate chat). It never edits code.
//!
//! This module hosts the Language Server; the layer model seams live in
//! [`ports`], injected through [`NavigatorDeps`] so tests can script them.

pub mod commenter;
pub mod config;
pub mod document;
pub mod log;
pub mod policy;
pub mod ports;
pub mod protocol;
pub mod server;

use std::path::Path;

pub use commenter::{Commenter, LlmCommenter, RemarkContext};
pub use config::NavigatorConfig;
pub use log::new_ulid_string;
pub use policy::{TriggerEvent, TriggerInput, TriggerOutcome};
pub use ports::{
    Answer, Answers, DecisionModel, FimAnswer, FimModel, FimRequest, ModelError, Question,
    Questions,
};
pub use protocol::{
    ClearParams, FeedbackParams, InlineCompletionItem, InlineCompletionParams,
    InlineCompletionResult, IntentParams, Range, ReflexShownParams, Remark, RemarkParams,
    StatusParams,
};
use tower_lsp_server::ls_types::Uri;
use tower_lsp_server::{LspService, Server};

/// Everything the navigator needs besides the LSP channel itself. The model
/// seams (`commenter`, and `fim`/`decision` for the reflex and instinct
/// layers) and the JSONL log path are injectable so tests can script them.
/// `fim`/`decision` set to `None` keep today's behaviour (no suggestion, no
/// decision pipeline).
pub struct NavigatorDeps {
    pub commenter: std::sync::Arc<dyn Commenter>,
    pub fim: Option<std::sync::Arc<dyn FimModel>>,
    pub decision: Option<std::sync::Arc<dyn DecisionModel>>,
    pub log_path: std::path::PathBuf,
}

impl NavigatorDeps {
    pub fn new(
        commenter: std::sync::Arc<dyn Commenter>,
        log_path: impl Into<std::path::PathBuf>,
    ) -> Self {
        Self {
            commenter,
            fim: None,
            decision: None,
            log_path: log_path.into(),
        }
    }

    /// Builder-style: attaches the reflex layer's model seam.
    pub fn with_fim(mut self, fim: std::sync::Arc<dyn FimModel>) -> Self {
        self.fim = Some(fim);
        self
    }

    /// Builder-style: attaches the instinct layer's model seam.
    pub fn with_decision(mut self, decision: std::sync::Arc<dyn DecisionModel>) -> Self {
        self.decision = Some(decision);
        self
    }
}

/// Builds the [`LspService`] for a navigator backend. Split from [`serve`] so
/// the binary and the tests share the exact same wiring.
pub fn build_service(
    deps: NavigatorDeps,
) -> (
    LspService<server::NavigatorBackend>,
    tower_lsp_server::ClientSocket,
) {
    LspService::build(move |client| server::NavigatorBackend::new(client, deps))
        .custom_method("navigator/feedback", server::NavigatorBackend::feedback)
        .custom_method(
            "navigator/diagnostics",
            server::NavigatorBackend::diagnostics,
        )
        .custom_method("navigator/intent", server::NavigatorBackend::intent)
        .custom_method(
            "navigator/inlineCompletion",
            server::NavigatorBackend::inline_completion,
        )
        .custom_method(
            "navigator/reflexShown",
            server::NavigatorBackend::reflex_shown,
        )
        .finish()
}

/// Serves the navigator over the given in/out streams until the client exits.
/// Used by the binary (stdio) and by the integration tests (duplex streams).
pub async fn serve<I, O>(input: I, output: O, deps: NavigatorDeps)
where
    I: tokio::io::AsyncRead + Unpin + Send + 'static,
    O: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (service, socket) = build_service(deps);
    Server::new(input, output, socket).serve(service).await;
}

/// Default JSONL log path under the user data dir
/// (`~/.local/share/rust-agent/navigator.jsonl` on Linux).
pub fn default_log_path() -> Option<std::path::PathBuf> {
    directories::ProjectDirs::from("", "", "rust-agent")
        .map(|p| p.data_dir().join("navigator.jsonl"))
}

/// Normalizes a URI for use as a state key (documents are keyed by their
/// string form).
pub fn uri_key(uri: &Uri) -> String {
    uri.as_str().to_string()
}

/// Resolves the models config path the same way the main binary does.
pub fn resolve_models(explicit: Option<&Path>, workspace: &Path) -> Option<std::path::PathBuf> {
    let config_dir =
        crate::provider::auth::default_config_dir().unwrap_or_else(|| Path::new(".").to_path_buf());
    crate::provider::config::resolve_models_path(explicit, workspace, &config_dir)
}
