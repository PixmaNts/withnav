//! What the server and its clients know about a workspace directory on disk, independent of any
//! session: today its git state and the worktrees of its repository.
pub mod git;
pub mod worktrees;
