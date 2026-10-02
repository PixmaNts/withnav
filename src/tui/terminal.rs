//! Owns terminal mode lifecycle and restoration; no application state transitions.
pub struct TerminalGuard;
impl TerminalGuard {
    pub fn enter() -> Result<Self, super::TuiError> {
        Ok(Self)
    }
}
