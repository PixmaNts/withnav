//! The Language Server face of the navigator: the `LanguageServer`
//! implementation and the custom `navigator/*` methods, thin adapters over
//! the [`Runtime`](crate::navigator::runtime::Runtime), plus the service
//! registration shared by the binary and the tests. Also hosts the wire
//! types ([`protocol`]) and URI conversion ([`uri`]).

pub mod protocol;
pub mod uri;

use std::sync::Arc;

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    DidChangeConfigurationParams, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, InitializeParams, InitializeResult,
    InitializedParams, ServerCapabilities, ServerInfo, TextDocumentSyncCapability,
    TextDocumentSyncKind,
};
use tower_lsp_server::{Client, ClientSocket, LanguageServer, LspService, Server};

use crate::navigator::runtime::Runtime;
use crate::navigator::NavigatorDeps;
use protocol::{
    DiagnosticsParams, FeedbackParams, InlineCompletionParams, InlineCompletionResult,
    IntentParams, ReflexShownParams,
};
use uri::uri_to_path;

pub struct NavigatorBackend {
    runtime: Arc<Runtime>,
}

impl NavigatorBackend {
    pub fn new(client: Client, deps: NavigatorDeps) -> Self {
        Self {
            runtime: Runtime::new(client, deps),
        }
    }
}

impl LanguageServer for NavigatorBackend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        if let Some(options) = &params.initialization_options {
            self.runtime.apply_config(options.clone()).await;
        }
        #[allow(deprecated)]
        let from_root = params.root_uri.as_ref().and_then(uri_to_path);
        let from_folder = params
            .workspace_folders
            .as_ref()
            .and_then(|folders| folders.first().and_then(|f| uri_to_path(&f.uri)));
        self.runtime.set_workspace(from_root, from_folder).await;
        self.runtime.report_disabled_layers().await;
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "rust-agent-navigator".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            ..InitializeResult::default()
        })
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    /// `initialized`: announce the cognition chat server's endpoint when the
    /// binary started one, so the extension can bridge the chat panel.
    async fn initialized(&self, _params: InitializedParams) {
        self.runtime.announce_server_info().await;
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.runtime.on_did_open(params).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        self.runtime.on_did_change(params).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        self.runtime.on_did_save(params).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.runtime.on_did_close(params).await;
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        self.runtime.apply_config(params.settings).await;
    }
}

// Custom client->server notifications, wired as custom JSON-RPC methods in
// `build_service`. They must be inherent async methods on the backend to
// satisfy the custom_method signature.
impl NavigatorBackend {
    pub async fn feedback(&self, params: FeedbackParams) {
        self.runtime.on_feedback(params).await;
    }

    pub async fn diagnostics(&self, params: DiagnosticsParams) {
        self.runtime.on_diagnostics(params).await;
    }

    pub async fn intent(&self, params: IntentParams) {
        self.runtime.on_intent(params).await;
    }

    /// `navigator/inlineCompletion`: ghost-text request from the extension's
    /// inline-completion provider. The reflex layer answers or returns an
    /// empty item list.
    pub async fn inline_completion(
        &self,
        params: InlineCompletionParams,
    ) -> Result<InlineCompletionResult> {
        Ok(self.runtime.inline_completion(params).await)
    }

    pub async fn reflex_shown(&self, params: ReflexShownParams) {
        self.runtime.on_reflex_shown(params).await;
    }
}

/// Builds the [`LspService`] for a navigator backend. Split from [`serve`] so
/// the binary and the tests share the exact same wiring.
pub fn build_service(deps: NavigatorDeps) -> (LspService<NavigatorBackend>, ClientSocket) {
    LspService::build(move |client| NavigatorBackend::new(client, deps))
        .custom_method("navigator/feedback", NavigatorBackend::feedback)
        .custom_method("navigator/diagnostics", NavigatorBackend::diagnostics)
        .custom_method("navigator/intent", NavigatorBackend::intent)
        .custom_method(
            "navigator/inlineCompletion",
            NavigatorBackend::inline_completion,
        )
        .custom_method("navigator/reflexShown", NavigatorBackend::reflex_shown)
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
