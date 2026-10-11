use std::{fs, path::PathBuf};

pub(crate) struct WorkspaceFixture {
    _tmp: tempfile::TempDir,
    pub(crate) home: PathBuf,
    /// A Git worktree root.
    pub(crate) repo: PathBuf,
    /// A directory below the root.
    pub(crate) ws: PathBuf,
}

impl WorkspaceFixture {
    pub(crate) fn new(subdir: &str) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = fs::canonicalize(tmp.path()).expect("canonical");
        let home = base.join("home");
        let repo = base.join("repo");
        let ws = repo.join(subdir);
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(&ws).unwrap();
        Self {
            _tmp: tmp,
            home,
            repo,
            ws,
        }
    }
}
