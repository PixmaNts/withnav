//! Which primary agent a live session runs as, shared between the session
//! service (which switches it) and the runtime (which starts turns).
//!
//! A turn runs as the agent selected when it began: [`AgentSelection::begin_turn`]
//! copies the selection, so a switch never reaches a turn in flight. A switch
//! is refused while a turn runs and while the agent waits for the answer to
//! an `ask`: both facts live in the same state, so a turn that ends by
//! asking cannot slip past a switch that is being decided. The choice is stored (through the
//! `persist` future the caller supplies) before it takes effect. The two
//! operations exclude each other through one async gate, so a turn cannot
//! start between the store write and the change.

use std::sync::{Arc, Mutex, MutexGuard};

use super::definitions::{AgentDefinition, Agents};

#[derive(Debug)]
struct State {
    current: Arc<AgentDefinition>,
    running: bool,
    /// The last turn ended asking the human something that is not answered
    /// yet.
    ask_pending: bool,
    /// A compaction is preparing its summary: the agent and the variant it
    /// measures against must not change until it commits or gives up.
    compacting: bool,
    /// A model switch is preparing its target: nothing that changes the agent,
    /// the variant or the conversation may land until it commits or gives up.
    switching: bool,
}

/// Why a switch did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchError {
    /// A turn is in flight.
    Busy,
    /// The agent waits for the answer to an `ask`.
    AskPending,
    /// A compaction is preparing its summary.
    Compacting,
    /// A model switch is preparing its target.
    Switching,
    /// The choice could not be stored.
    Persist(String),
}

/// Clears the compacting mark when dropped.
#[derive(Debug)]
pub struct CompactionGuard<'a> {
    selection: &'a AgentSelection,
}

impl Drop for CompactionGuard<'_> {
    fn drop(&mut self) {
        self.selection.state().compacting = false;
    }
}

/// Clears the switching mark when dropped.
#[derive(Debug)]
pub struct SwitchGuard<'a> {
    selection: &'a AgentSelection,
}

impl Drop for SwitchGuard<'_> {
    fn drop(&mut self) {
        self.selection.state().switching = false;
    }
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
                ask_pending: false,
                compacting: false,
                switching: false,
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

    /// Whether the agent waits for the answer to an `ask`.
    pub fn ask_pending(&self) -> bool {
        self.state().ask_pending
    }

    pub fn is_running(&self) -> bool {
        self.state().running
    }

    /// Records whether an `ask` waits for its answer, for a session that
    /// starts with one (a resumed conversation).
    pub fn set_ask_pending(&self, pending: bool) {
        self.state().ask_pending = pending;
    }

    /// Marks a turn as started and returns the role it runs with: the agent
    /// selected now, or `None` when this selection sends no role. The turn
    /// carries the answer to a waiting `ask`, so none waits any more.
    pub async fn begin_turn(&self) -> Option<Arc<AgentDefinition>> {
        let _gate = self.gate.lock().await;
        let mut state = self.state();
        state.running = true;
        state.ask_pending = false;
        self.send_role.then(|| state.current.clone())
    }

    /// Marks the turn as over; `ask_pending` is whether it ended asking the
    /// human something. Both change in one step.
    pub fn end_turn(&self, ask_pending: bool) {
        let mut state = self.state();
        state.running = false;
        state.ask_pending = ask_pending;
    }

    /// Marks a compaction as preparing, so no agent or variant switch lands
    /// while its summary is in flight; the flag clears when the returned
    /// guard drops. The agent is read after this returns.
    pub async fn begin_compaction(&self) -> CompactionGuard<'_> {
        let _gate = self.gate.lock().await;
        self.state().compacting = true;
        CompactionGuard { selection: self }
    }

    /// Marks a model switch as preparing, so no agent or variant change and no
    /// compaction request lands while it measures and commits; the flag clears
    /// when the returned guard drops.
    pub async fn begin_switch(&self) -> SwitchGuard<'_> {
        let _gate = self.gate.lock().await;
        self.state().switching = true;
        SwitchGuard { selection: self }
    }

    /// Whether a compaction is preparing its summary.
    pub fn compacting(&self) -> bool {
        self.state().compacting
    }

    /// Whether a model switch is preparing its target.
    pub fn switching(&self) -> bool {
        self.state().switching
    }

    /// Holds the switch gate for a change that is not an agent switch (a
    /// variant), so no compaction starts while it applies. `Err` while one
    /// is preparing.
    pub async fn exclusive_unless_compacting(
        &self,
    ) -> Result<tokio::sync::MutexGuard<'_, ()>, SwitchError> {
        let gate = self.gate.lock().await;
        let state = self.state();
        if state.compacting {
            return Err(SwitchError::Compacting);
        }
        if state.switching {
            return Err(SwitchError::Switching);
        }
        drop(state);
        Ok(gate)
    }

    /// Switches to `target` once `persist` has stored it, and returns the
    /// agent it replaces. Refused while a turn runs or an `ask` waits.
    pub async fn switch(
        &self,
        target: Arc<AgentDefinition>,
        persist: impl std::future::Future<Output = Result<(), String>>,
    ) -> Result<Arc<AgentDefinition>, SwitchError> {
        let _gate = self.gate.lock().await;
        self.check_idle()?;
        persist.await.map_err(SwitchError::Persist)?;
        let mut state = self.state();
        Ok(std::mem::replace(&mut state.current, target))
    }

    fn check_idle(&self) -> Result<(), SwitchError> {
        let state = self.state();
        if state.compacting {
            Err(SwitchError::Compacting)
        } else if state.switching {
            Err(SwitchError::Switching)
        } else if state.running {
            Err(SwitchError::Busy)
        } else if state.ask_pending {
            Err(SwitchError::AskPending)
        } else {
            Ok(())
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> Arc<AgentDefinition> {
        AgentSelection::default()
            .agents()
            .get("plan")
            .cloned()
            .unwrap()
    }

    #[tokio::test]
    async fn a_switch_waits_for_no_turn_and_is_refused_while_one_runs() {
        let selection = AgentSelection::new(
            Arc::new(Agents::builtin()),
            Agents::builtin().get("build").cloned().unwrap(),
        );
        let role = selection.begin_turn().await.expect("a role");
        assert_eq!(role.id.as_str(), "build");
        let refused = selection.switch(plan(), async { Ok(()) }).await;
        assert_eq!(refused.unwrap_err(), SwitchError::Busy);
        assert_eq!(selection.current().id.as_str(), "build");

        selection.end_turn(false);
        let previous = selection.switch(plan(), async { Ok(()) }).await.unwrap();
        assert_eq!(previous.id.as_str(), "build");
        assert_eq!(selection.current().id.as_str(), "plan");
    }

    #[tokio::test]
    async fn a_choice_that_cannot_be_stored_does_not_take_effect() {
        let selection = AgentSelection::new(
            Arc::new(Agents::builtin()),
            Agents::builtin().get("build").cloned().unwrap(),
        );
        let err = selection
            .switch(plan(), async { Err("disk full".to_string()) })
            .await
            .unwrap_err();
        assert_eq!(err, SwitchError::Persist("disk full".to_string()));
        assert_eq!(selection.current().id.as_str(), "build");
    }

    #[tokio::test]
    async fn a_turn_keeps_its_role_when_the_selection_changes_after_it_began() {
        let selection = AgentSelection::new(
            Arc::new(Agents::builtin()),
            Agents::builtin().get("build").cloned().unwrap(),
        );
        let turn_role = selection.begin_turn().await.unwrap();
        selection.end_turn(false);
        selection.switch(plan(), async { Ok(()) }).await.unwrap();
        assert_eq!(turn_role.id.as_str(), "build");
        assert_eq!(selection.begin_turn().await.unwrap().id.as_str(), "plan");
    }

    #[tokio::test]
    async fn a_switch_is_refused_while_an_ask_waits_until_the_answer_starts_a_turn() {
        let selection = AgentSelection::new(
            Arc::new(Agents::builtin()),
            Agents::builtin().get("build").cloned().unwrap(),
        );
        selection.begin_turn().await;
        selection.end_turn(true);
        let refused = selection.switch(plan(), async { Ok(()) }).await;
        assert_eq!(refused.unwrap_err(), SwitchError::AskPending);
        assert_eq!(selection.current().id.as_str(), "build");

        selection.begin_turn().await;
        selection.end_turn(false);
        selection.switch(plan(), async { Ok(()) }).await.unwrap();
        assert_eq!(selection.current().id.as_str(), "plan");
    }

    // A turn that ends by asking races a switch: whichever the scheduler runs
    // first, the switch is refused (turn still running, or ask now pending)
    // and the agent does not change.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_turn_ending_in_an_ask_races_a_switch_and_the_switch_never_wins() {
        for _ in 0..50 {
            let selection = Arc::new(AgentSelection::new(
                Arc::new(Agents::builtin()),
                Agents::builtin().get("build").cloned().unwrap(),
            ));
            selection.begin_turn().await;
            let barrier = Arc::new(tokio::sync::Barrier::new(2));
            let ender = {
                let (selection, barrier) = (selection.clone(), barrier.clone());
                tokio::spawn(async move {
                    barrier.wait().await;
                    selection.end_turn(true);
                })
            };
            let switcher = {
                let (selection, barrier) = (selection.clone(), barrier.clone());
                tokio::spawn(async move {
                    barrier.wait().await;
                    selection.switch(plan(), async { Ok(()) }).await
                })
            };
            ender.await.unwrap();
            let result = switcher.await.unwrap();
            assert!(
                matches!(result, Err(SwitchError::Busy | SwitchError::AskPending)),
                "{result:?}"
            );
            assert_eq!(selection.current().id.as_str(), "build");
        }
    }

    #[tokio::test]
    async fn a_switch_and_a_variant_change_are_refused_while_a_compaction_prepares() {
        let selection = AgentSelection::new(
            Arc::new(Agents::builtin()),
            Agents::builtin().get("build").cloned().unwrap(),
        );
        let guard = selection.begin_compaction().await;
        let refused = selection.switch(plan(), async { Ok(()) }).await;
        assert_eq!(refused.unwrap_err(), SwitchError::Compacting);
        assert_eq!(
            selection.exclusive_unless_compacting().await.unwrap_err(),
            SwitchError::Compacting
        );
        assert_eq!(selection.current().id.as_str(), "build");

        drop(guard);
        assert!(selection.exclusive_unless_compacting().await.is_ok());
        selection.switch(plan(), async { Ok(()) }).await.unwrap();
        assert_eq!(selection.current().id.as_str(), "plan");
    }

    #[tokio::test]
    async fn an_agent_switch_and_a_variant_change_are_refused_while_a_model_switch_prepares() {
        let selection = AgentSelection::new(
            Arc::new(Agents::builtin()),
            Agents::builtin().get("build").cloned().unwrap(),
        );
        let guard = selection.begin_switch().await;
        assert!(selection.switching());
        let refused = selection.switch(plan(), async { Ok(()) }).await;
        assert_eq!(refused.unwrap_err(), SwitchError::Switching);
        assert_eq!(
            selection.exclusive_unless_compacting().await.unwrap_err(),
            SwitchError::Switching
        );

        drop(guard);
        assert!(!selection.switching());
        assert!(selection.exclusive_unless_compacting().await.is_ok());
    }

    #[tokio::test]
    async fn the_default_selection_sends_no_role() {
        let selection = AgentSelection::default();
        assert!(selection.begin_turn().await.is_none());
    }
}
