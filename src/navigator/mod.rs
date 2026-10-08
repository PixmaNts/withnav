//! The navigator: a colleague over the Captain's shoulder with four
//! cooperating layers — reflex (fill-in-the-middle suggestions), instinct
//! (typed decision questions), metacognition (fast remarks) and cognition
//! (deliberate chat). It never edits code.
//!
//! This module only declares the submodules and re-exports their public
//! paths. The Language Server lives in [`lsp`], the shared state and trigger
//! scheduling in [`runtime`], the process wiring in [`bootstrap`]; the layer
//! model seams are in [`ports`], injected through [`NavigatorDeps`] so tests
//! can script them.

pub mod bootstrap;
pub mod config;
pub mod conversation;
pub mod document;
pub mod help_call;
pub mod intent;
pub mod log;
pub mod lsp;
pub mod metacognition;
pub mod models;
pub mod ports;
pub mod reflex;
pub mod replay;
pub mod runtime;
pub mod session_hooks;
mod trigger;

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
pub use lsp::{build_service, serve};
pub use metacognition::{LlmCommenter, RemarkContext};
pub use ports::{
    Answer, Answers, FimAnswer, FimModel, FimRequest, Metacognition, ModelError, Question,
    Questions, ReflexContextModel, ReflexContextRequest,
};
pub use reflex::{
    SuggestOutcome, Suggestion, DEBOUNCE_MS, MAX_CONTEXT_BYTES, MAX_PREFIX_BYTES, MAX_SUFFIX_BYTES,
};
pub use session_hooks::{NavigatorHooks, COGNITION_PROMPT};
pub use trigger::TriggerKind;

/// Injectable editor model seams, flow factory, conversation and decision log.
/// No flow factory or no eligible enabled flow means no editor evaluation.
pub struct NavigatorDeps {
    /// Creates the editor runtime with its document-scoped effect adapter.
    pub flows: Option<Arc<FlowFactory>>,
    pub commenter: Arc<dyn Metacognition>,
    pub fim: Option<Arc<dyn FimModel>>,
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
    /// The user-level navigator dir (`~/.config/withnav/navigator/`).
    pub user_dir: Option<PathBuf>,
    /// The cognition chat server's endpoint: when set, `initialized` sends
    /// `withnav/serverInfo` with the loopback port and bearer token.
    pub server_info: Option<ServerInfoParams>,
    /// Human-readable notes about layers that were disabled at startup;
    /// surfaced as one status message after initialize.
    pub disabled_layers: Vec<String>,
    /// Flow conversion failures, shown once after the editor initializes.
    pub startup_warnings: Vec<String>,
    pub log_path: PathBuf,
}

impl NavigatorDeps {
    pub fn new(commenter: Arc<dyn Metacognition>, log_path: impl Into<PathBuf>) -> Self {
        Self {
            flows: None,
            commenter,
            fim: None,
            reflex_context: None,
            reflex_model: None,
            instinct_model: None,
            metacognition_model: None,
            conversation: None,
            workspace: None,
            user_dir: None,
            server_info: None,
            disabled_layers: Vec::new(),
            startup_warnings: Vec::new(),
            log_path: log_path.into(),
        }
    }

    /// Builder-style: attaches the reflex layer's model seam.
    pub fn with_fim(mut self, fim: Arc<dyn FimModel>) -> Self {
        self.fim = Some(fim);
        self
    }

    pub fn with_flows(mut self, factory: Arc<FlowFactory>) -> Self {
        self.flows = Some(factory);
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
    /// (intent resolution and historical configuration migration).
    pub fn with_dirs(mut self, workspace: Option<PathBuf>, user_dir: Option<PathBuf>) -> Self {
        self.workspace = workspace;
        self.user_dir = user_dir;
        self
    }

    /// Builder-style: the cognition chat server's endpoint, announced as
    /// `withnav/serverInfo` after `initialized`.
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

pub type FlowFactory = dyn Fn(Arc<dyn crate::flows::runtime::FlowEffects>) -> Arc<crate::flows::runtime::FlowRuntime>
    + Send
    + Sync;

/// The default user-level navigator dir
/// (`~/.config/withnav/navigator/`), when it can be resolved.
pub fn default_user_dir() -> Option<PathBuf> {
    crate::provider::auth::default_config_dir().map(|d| d.join("navigator"))
}

/// Resolves the models config path the same way the main binary does.
pub fn resolve_models(explicit: Option<&Path>, workspace: &Path) -> Option<std::path::PathBuf> {
    let config_dir =
        crate::provider::auth::default_config_dir().unwrap_or_else(|| Path::new(".").to_path_buf());
    crate::provider::config::resolve_models_path(explicit, workspace, &config_dir)
}
