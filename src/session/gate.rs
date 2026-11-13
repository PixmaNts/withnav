//! The lifecycle gates: one per conversation tree (a root session and its subagents), plus a
//! service-wide shutdown gate.
//!
//! Resume, stop, delete, archive and slash commands of one tree run one at a time, so a session
//! id never has two runtimes and is never resumed while it is being stopped or deleted. Two
//! different trees never wait for each other: a teardown can hold its gate for up to two
//! teardown caps (a wedged runtime, then a wedged sequencer), and a single service-wide gate made
//! every other conversation's `/model`, stop or resume wait that long.
//!
//! Every tree gate is taken under a shared hold of the shutdown gate; shutdown takes it
//! exclusively, so it still never interleaves with a stop, resume or delete of any tree.
//! Lock order: shutdown gate, tree gate, admission gate, then the registry. A task never takes a
//! second tree gate while it holds one.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedMutexGuard, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

use super::service::SessionId;

type TreeGates = Arc<Mutex<HashMap<SessionId, Arc<tokio::sync::Mutex<()>>>>>;

#[derive(Debug, Default)]
pub(super) struct LifecycleGates {
    shutdown: Arc<RwLock<()>>,
    trees: TreeGates,
}

/// A held tree gate. Owns its guards, so it can be dropped early (a command lets go before a
/// provider call) and does not borrow the service.
#[derive(Debug)]
pub(crate) struct LifecycleGuard {
    tree: Option<OwnedMutexGuard<()>>,
    _shutdown: OwnedRwLockReadGuard<()>,
    root: SessionId,
    trees: TreeGates,
}

impl LifecycleGates {
    /// Takes the gate of the tree whose root is `root`.
    pub(super) async fn tree(&self, root: SessionId) -> LifecycleGuard {
        let shutdown = self.shutdown.clone().read_owned().await;
        // Cloned under the map lock, so a guard being dropped sees this waiter and keeps the
        // entry: one tree never has two live mutexes.
        let gate = self
            .trees
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(root)
            .or_default()
            .clone();
        LifecycleGuard {
            tree: Some(gate.lock_owned().await),
            _shutdown: shutdown,
            root,
            trees: self.trees.clone(),
        }
    }

    /// Takes the shutdown gate: waits for every held tree gate and keeps new ones out.
    pub(super) async fn shutdown(&self) -> OwnedRwLockWriteGuard<()> {
        self.shutdown.clone().write_owned().await
    }

    #[cfg(test)]
    pub(super) fn tracked_trees(&self) -> usize {
        self.trees.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

impl Drop for LifecycleGuard {
    fn drop(&mut self) {
        // Release first, then forget the tree when nobody holds or awaits its gate: the map
        // keeps one entry per tree in use, not one per conversation ever touched.
        drop(self.tree.take());
        let mut trees = self.trees.lock().unwrap_or_else(|e| e.into_inner());
        if trees
            .get(&self.root)
            .is_some_and(|gate| Arc::strong_count(gate) == 1)
        {
            trees.remove(&self.root);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ulid::Ulid;

    use super::*;

    const SHORT: Duration = Duration::from_millis(50);

    #[tokio::test]
    async fn two_trees_do_not_wait_for_each_other_and_one_tree_does() {
        let gates = Arc::new(LifecycleGates::default());
        let (a, b) = (Ulid::generate(), Ulid::generate());
        let held = gates.tree(a).await;
        assert!(
            tokio::time::timeout(SHORT, gates.tree(b)).await.is_ok(),
            "another tree's gate is free"
        );
        assert!(
            tokio::time::timeout(SHORT, gates.tree(a)).await.is_err(),
            "the same tree waits"
        );
        drop(held);
        assert!(tokio::time::timeout(SHORT, gates.tree(a)).await.is_ok());
    }

    #[tokio::test]
    async fn shutdown_waits_for_held_tree_gates_and_keeps_new_ones_out() {
        let gates = Arc::new(LifecycleGates::default());
        let held = gates.tree(Ulid::generate()).await;
        assert!(tokio::time::timeout(SHORT, gates.shutdown()).await.is_err());
        drop(held);
        let shutdown = gates.shutdown().await;
        assert!(
            tokio::time::timeout(SHORT, gates.tree(Ulid::generate()))
                .await
                .is_err()
        );
        drop(shutdown);
    }

    #[tokio::test]
    async fn a_released_tree_is_forgotten_but_a_waited_one_is_kept() {
        let gates = Arc::new(LifecycleGates::default());
        let root = Ulid::generate();
        let held = gates.tree(root).await;
        let waiter = {
            let gates = gates.clone();
            tokio::spawn(async move { gates.tree(root).await })
        };
        // Let the waiter clone the tree's gate and park on it.
        while gates
            .trees
            .lock()
            .unwrap()
            .get(&root)
            .is_some_and(|gate| Arc::strong_count(gate) < 3)
        {
            tokio::task::yield_now().await;
        }
        drop(held);
        assert_eq!(gates.tracked_trees(), 1, "the waiter still needs the entry");
        let second = waiter.await.unwrap();
        drop(second);
        assert_eq!(gates.tracked_trees(), 0);
    }
}
