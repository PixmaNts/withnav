//! The navigator: a colleague over the driver's shoulder with four
//! cooperating layers — reflex (fill-in-the-middle suggestions), instinct
//! (typed decision questions), metacognition (fast remarks) and cognition
//! (deliberate chat). It never edits code.
//!
//! This module hosts the Language Server; the layer model seams live in
//! [`ports`], injected through [`NavigatorDeps`] so tests can script them.

pub mod config;
pub mod conversation;
pub mod default_pipeline;
pub mod document;
pub mod intent;
pub mod log;
pub mod lsp;
pub mod metacognition;
pub mod pipeline;
pub mod ports;
pub mod reflex;
pub mod replay;
pub mod server;

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use config::NavigatorConfig;
pub use conversation::{
    ConversationEvent, ConversationFeed, SessionPost, WakeOutcome, NAVIGATOR_ACTOR_ID,
};
pub use intent::{read_intent, IntentRecord};
pub use log::new_ulid_string;
pub use lsp::protocol;
pub use lsp::protocol::{
    ClearParams, FeedbackParams, InlineCompletionItem, InlineCompletionParams,
    InlineCompletionResult, IntentParams, Range, ReflexShownParams, Remark, RemarkParams,
    ServerInfoParams, StatusParams,
};
pub use lsp::uri::{uri_key, uri_to_path, uri_to_path_str};
pub use metacognition::{LlmCommenter, RemarkContext};
pub use pipeline::{
    Action, Evaluation, Pipeline, PipelineError, PipelineSnapshot, PrefilterOutcome, TriggerInputs,
    TriggerKind,
};
pub use ports::{
    Answer, Answers, DecisionModel, FimAnswer, FimModel, FimRequest, Metacognition, ModelError,
    Question, Questions, ReflexContextModel, ReflexContextRequest,
};
pub use reflex::{
    SuggestOutcome, Suggestion, DEBOUNCE_MS, MAX_CONTEXT_BYTES, MAX_PREFIX_BYTES, MAX_SUFFIX_BYTES,
};
use tower_lsp_server::{LspService, Server};

/// Everything the navigator needs besides the LSP channel itself. The model
/// seams (metacognition, and `fim`/`decision` for the reflex and instinct
/// layers) and the JSONL log path are injectable so tests can script them.
///
/// `fim`/`decision` set to `None` keep the corresponding layer disabled (no
/// suggestion / triggers logged with `instinct_unavailable`); the other
/// layers keep working. `workspace`/`user_dir` drive pipeline resolution,
/// the intent record and hot reload.
pub struct NavigatorDeps {
    pub commenter: Arc<dyn Metacognition>,
    pub fim: Option<Arc<dyn FimModel>>,
    pub decision: Option<Arc<dyn DecisionModel>>,
    /// The metacognition's reflex-context seam; `None` keeps the intent
    /// summary as the reflex context.
    pub reflex_context: Option<Arc<dyn ReflexContextModel>>,
    /// Model ids for the per-layer call records.
    pub reflex_model: Option<String>,
    pub instinct_model: Option<String>,
    pub metacognition_model: Option<String>,
    /// The conversation feed shared with the session server: the seam a
    /// `wake_cognition` action posts through. `None` keeps wakes logged as
    /// `cognition_unavailable` (no active conversation to wake).
    pub conversation: Option<ConversationFeed>,
    /// The workspace root (upgraded from `rootUri` at initialize when the
    /// client sends one).
    pub workspace: Option<PathBuf>,
    /// The user-level navigator dir (`~/.config/rust-agent/navigator/`).
    pub user_dir: Option<PathBuf>,
    /// The cognition chat server's endpoint: when set, `initialized` sends
    /// `navigator/serverInfo` with the loopback port and bearer token.
    pub server_info: Option<ServerInfoParams>,
    /// Human-readable notes about layers that were disabled at startup;
    /// surfaced as one status message after initialize.
    pub disabled_layers: Vec<String>,
    pub log_path: PathBuf,
}

impl NavigatorDeps {
    pub fn new(commenter: Arc<dyn Metacognition>, log_path: impl Into<PathBuf>) -> Self {
        Self {
            commenter,
            fim: None,
            decision: None,
            reflex_context: None,
            reflex_model: None,
            instinct_model: None,
            metacognition_model: None,
            conversation: None,
            workspace: None,
            user_dir: None,
            server_info: None,
            disabled_layers: Vec::new(),
            log_path: log_path.into(),
        }
    }

    /// Builder-style: attaches the reflex layer's model seam.
    pub fn with_fim(mut self, fim: Arc<dyn FimModel>) -> Self {
        self.fim = Some(fim);
        self
    }

    /// Builder-style: attaches the instinct layer's model seam.
    pub fn with_decision(mut self, decision: Arc<dyn DecisionModel>) -> Self {
        self.decision = Some(decision);
        self
    }

    /// Builder-style: attaches the seam refreshing the reflex context.
    pub fn with_reflex_context(mut self, model: Arc<dyn ReflexContextModel>) -> Self {
        self.reflex_context = Some(model);
        self
    }

    /// Builder-style: model ids for the per-layer call records.
    pub fn with_model_ids(
        mut self,
        reflex: Option<String>,
        instinct: Option<String>,
        metacognition: Option<String>,
    ) -> Self {
        self.reflex_model = reflex;
        self.instinct_model = instinct;
        self.metacognition_model = metacognition;
        self
    }

    /// Builder-style: the workspace root and the user navigator dir
    /// (pipeline resolution, intent record, hot reload).
    pub fn with_dirs(mut self, workspace: Option<PathBuf>, user_dir: Option<PathBuf>) -> Self {
        self.workspace = workspace;
        self.user_dir = user_dir;
        self
    }

    /// Builder-style: the cognition chat server's endpoint, announced as
    /// `navigator/serverInfo` after `initialized`.
    pub fn with_server_info(mut self, info: ServerInfoParams) -> Self {
        self.server_info = Some(info);
        self
    }

    /// Builder-style: the conversation feed the `wake_cognition` action
    /// posts through (shared with the in-process session server).
    pub fn with_conversation(mut self, feed: ConversationFeed) -> Self {
        self.conversation = Some(feed);
        self
    }

    /// Builder-style: notes about layers disabled at startup.
    pub fn with_disabled_layers(mut self, notes: Vec<String>) -> Self {
        self.disabled_layers = notes;
        self
    }
}

/// The default user-level navigator dir
/// (`~/.config/rust-agent/navigator/`), when it can be resolved.
pub fn default_user_dir() -> Option<PathBuf> {
    crate::provider::auth::default_config_dir().map(|d| d.join("navigator"))
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

/// Resolves the models config path the same way the main binary does.
pub fn resolve_models(explicit: Option<&Path>, workspace: &Path) -> Option<std::path::PathBuf> {
    let config_dir =
        crate::provider::auth::default_config_dir().unwrap_or_else(|| Path::new(".").to_path_buf());
    crate::provider::config::resolve_models_path(explicit, workspace, &config_dir)
}
