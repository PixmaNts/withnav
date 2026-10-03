//! Which primary agent a live session runs as, shared between the session
//! service (which switches it) and the runtime (which starts turns).
//!
//! A turn runs as the agent selected when it began: [`AgentSelection::begin_turn`]
//! copies the selection, so a switch never reaches a turn in flight. A switch
//! is refused while a turn runs, and the choice is stored (through the
//! `persist` future the caller supplies) before it takes effect. The two
//! operations exclude each other through one async gate, so a turn cannot
//! start between the store write and the change.

use std::sync::{Arc, Mutex, MutexGuard};

use super::definitions::{AgentDefinition, Agents};

#[derive(Debug)]
struct State {
    current: Arc<AgentDefinition>,
    running: bool,
}

/// Why a switch did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchError {
    /// A turn is in flight.
    Busy,
    /// The choice could not be stored.
    Persist(String),
}

/// The agents a session can use and the one it runs as.
#[derive(Debug)]
pub struct AgentSelection {
    agents: Arc<Agents>,
    gate: tokio::sync::Mutex<()>,
    state: Mutex<State>,
    /// Whether turns send the role prompt. Off for a runtime driven without
    /// a session service, whose requests carry no role.
    send_role: bool,
}

impl Default for AgentSelection {
    /// The built-ins, running as `build`, with turns that send no role.
    fn default() -> Self {
        let agents = Arc::new(Agents::builtin());
        let current = agents
            .get(super::definitions::BUILD)
            .cloned()
            .expect("build is a built-in");
        let mut selection = Self::new(agents, current);
        selection.send_role = false;
        selection
    }
}

impl AgentSelection {
    pub fn new(agents: Arc<Agents>, current: Arc<AgentDefinition>) -> Self {
        Self {
            agents,
            gate: tokio::sync::Mutex::new(()),
            state: Mutex::new(State {
                current,
                running: false,
            }),
            send_role: true,
        }
    }

    /// The agents this session was created or resumed with.
    pub fn agents(&self) -> &Agents {
        &self.agents
    }

    /// The agent the session runs as now.
    pub fn current(&self) -> Arc<AgentDefinition> {
        self.state().current.clone()
    }

    /// Marks a turn as started and returns the role it runs with: the agent
    /// selected now, or `None` when this selection sends no role.
    pub async fn begin_turn(&self) -> Option<Arc<AgentDefinition>> {
        let _gate = self.gate.lock().await;
        let mut state = self.state();
        state.running = true;
        self.send_role.then(|| state.current.clone())
    }

    /// Marks the turn as over.
    pub fn end_turn(&self) {
        self.state().running = false;
    }

    /// Switches to `target` once `persist` has stored it, and returns the
    /// agent it replaces. Refused while a turn runs.
    pub async fn switch(
        &self,
        target: Arc<AgentDefinition>,
        persist: impl std::future::Future<Output = Result<(), String>>,
    ) -> Result<Arc<AgentDefinition>, SwitchError> {
        let _gate = self.gate.lock().await;
        if self.state().running {
            return Err(SwitchError::Busy);
        }
        persist.await.map_err(SwitchError::Persist)?;
        let mut state = self.state();
        Ok(std::mem::replace(&mut state.current, target))
    }

    fn state(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}
