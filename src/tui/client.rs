//! HTTP client for the session server; no UI state or rendering.
use super::protocol::*;
#[derive(Clone)]
pub struct Client {
    pub base_url: String,
    pub token: Option<String>,
}
impl Client {
    pub fn new(base_url: impl Into<String>, token: Option<String>) -> Self {
        Self {
            base_url: base_url.into(),
            token,
        }
    }
    pub async fn models(&self, _workspace: &str) -> Result<ModelsBody, String> {
        Err("not implemented".into())
    }
    pub async fn sessions(&self, _workspace: &str) -> Result<SessionListBody, String> {
        Err("not implemented".into())
    }
    pub async fn create(
        &self,
        _request: CreateSessionRequest,
    ) -> Result<CreateSessionResponse, String> {
        Err("not implemented".into())
    }
    pub async fn resume(
        &self,
        _workspace: &str,
        _id: &str,
    ) -> Result<CreateSessionResponse, String> {
        Err("not implemented".into())
    }
    pub async fn summary(&self, _id: &str) -> Result<SessionMetadata, String> {
        Err("not implemented".into())
    }
    pub async fn send(&self, _id: &str, _request: SubmitRequest) -> Result<SubmitResponse, String> {
        Err("not implemented".into())
    }
    pub async fn stop(&self, _id: &str) -> Result<StopResponse, String> {
        Err("not implemented".into())
    }
}
