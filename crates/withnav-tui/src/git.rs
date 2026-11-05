//! What the header and the Changes panel show of the workspace's git state. The host reads it
//! ([`TuiHost::read_git`](crate::TuiHost::read_git)); this crate only displays it.

/// The changed files the panel is laid out for; a host keeps at most this many in
/// [`GitStatus::files`] and counts the rest in [`GitStatus::dirty`].
pub const MAX_FILES: usize = 10;

/// One changed file: `M`, `A`, `D`, `R` or `?` (untracked), and its path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub mark: char,
    pub path: String,
}

/// The workspace's branch and changes, as `git status` reported them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitStatus {
    /// The branch name; `None` on a detached head.
    pub branch: Option<String>,
    /// How many files differ from the last commit (untracked included).
    pub dirty: usize,
    /// The first [`MAX_FILES`] of them.
    pub files: Vec<Change>,
}
