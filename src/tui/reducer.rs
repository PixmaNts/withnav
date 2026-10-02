//! Pure state transitions; must not perform I/O, read a clock, or access the terminal.
use super::{
    protocol::{Action, ClientEvent, Effect, ProtocolError},
    state::{State, UiState},
};
pub fn update(state: &mut State, event: ClientEvent) -> Result<Vec<Effect>, ProtocolError> {
    if let ClientEvent::ProtocolError { message, .. } = event {
        state.last_protocol_error = Some(message);
    }
    Ok(Vec::new())
}
pub fn handle_action(_state: &mut State, _ui: &mut UiState, _action: Action) -> Vec<Effect> {
    Vec::new()
}
