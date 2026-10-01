//! The pipeline snapshot: TOML plus every prompt it references, hashed. A
//! snapshot is validated whole before it replaces the active one, so a load
//! error keeps the previous complete snapshot running.
//!
//! Resolution: project `.navigator/pipeline.toml` > user
//! `~/.config/rust-agent/navigator/pipeline.toml` > built-in default.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::evaluator::{compile, CompiledSnapshot, SnapshotTrigger};
use super::{PipelineError, PipelineFile};

/// Where a snapshot was loaded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotSource {
    Project,
    User,
    Builtin,
}

impl SnapshotSource {
    pub fn as_str(self) -> &'static str {
        match self {
            SnapshotSource::Project => "project",
            SnapshotSource::User => "user",
            SnapshotSource::Builtin => "builtin",
        }
    }
}

/// A validated, ready-to-evaluate pipeline: the compiled triggers and the
/// hash of the TOML plus every referenced prompt.
#[derive(Clone)]
pub struct PipelineSnapshot {
    pub(crate) inner: CompiledSnapshot,
    /// The TOML text as loaded (part of the hash).
    pub toml_text: String,
    /// Prompt path (as referenced) to content.
    pub prompts: BTreeMap<String, String>,
    /// Hash over the TOML and every referenced prompt.
    pub hash: String,
    pub source: SnapshotSource,
}

impl std::fmt::Debug for PipelineSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipelineSnapshot")
            .field("hash", &self.hash)
            .field("source", &self.source)
            .field("triggers", &self.inner.triggers.len())
            .finish_non_exhaustive()
    }
}

impl PipelineSnapshot {
    /// The models.json alias of the instinct model.
    pub fn model_alias(&self) -> &str {
        &self.inner.model_alias
    }

    /// The snapshot hash (TOML + every referenced prompt).
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Which file the snapshot was loaded from.
    pub fn source(&self) -> SnapshotSource {
        self.source
    }

    /// The trigger table, for logging and the replay subcommand.
    pub fn triggers(&self) -> &[SnapshotTrigger] {
        &self.inner.triggers
    }

    /// Parses and validates TOML text (built-in default path). Prompts are
    /// taken from `prompts` (path -> content).
    pub fn from_parts(
        toml_text: &str,
        prompts: BTreeMap<String, String>,
        source: SnapshotSource,
    ) -> Result<Self, PipelineError> {
        let file: PipelineFile = toml::from_str(toml_text).map_err(|e| PipelineError::Parse {
            origin: source.as_str(),
            inner: e,
        })?;
        if file.instinct.model.is_empty() {
            return Err(PipelineError::invalid(
                source.as_str(),
                "[instinct] model must not be empty",
            ));
        }
        if file.triggers.is_empty() {
            return Err(PipelineError::invalid(
                source.as_str(),
                "the pipeline must declare at least one [[trigger]]",
            ));
        }
        let hash = hash_parts(toml_text, &prompts);
        let inner = compile(&file, &prompts, source.as_str())?;
        Ok(Self {
            inner,
            toml_text: toml_text.to_string(),
            prompts,
            hash,
            source,
        })
    }

    /// Loads a snapshot from a directory holding `pipeline.toml` and its
    /// `prompts/` files. `base_dir` is the `.navigator/` or user navigator
    /// dir.
    pub fn load_dir(base_dir: &Path, source: SnapshotSource) -> Result<Self, PipelineError> {
        let toml_path = base_dir.join("pipeline.toml");
        let toml_text = std::fs::read_to_string(&toml_path).map_err(|e| {
            PipelineError::io(source.as_str(), format!("{}: {e}", toml_path.display()))
        })?;
        let file: PipelineFile = toml::from_str(&toml_text).map_err(|e| PipelineError::Parse {
            origin: source.as_str(),
            inner: e,
        })?;
        let prompts = load_referenced_prompts(base_dir, &file, source.as_str())?;
        Self::from_parts_with_prompts_text(toml_text, prompts, source)
    }

    fn from_parts_with_prompts_text(
        toml_text: String,
        prompts: BTreeMap<String, String>,
        source: SnapshotSource,
    ) -> Result<Self, PipelineError> {
        Self::from_parts(&toml_text, prompts, source)
    }

    /// Resolution: project `.navigator/` > user dir > built-in.
    pub fn load(workspace: &Path, user_dir: &Path) -> Result<Self, PipelineError> {
        let project_dir = workspace.join(".navigator");
        if project_dir.join("pipeline.toml").is_file() {
            return Self::load_dir(&project_dir, SnapshotSource::Project);
        }
        if user_dir.join("pipeline.toml").is_file() {
            return Self::load_dir(user_dir, SnapshotSource::User);
        }
        Self::builtin()
    }

    /// The built-in default pipeline (spec §Instinct).
    pub fn builtin() -> Result<Self, PipelineError> {
        let prompts = BTreeMap::from([
            (
                "prompts/state.md".to_string(),
                crate::navigator::default_pipeline::STATE_PROMPT.to_string(),
            ),
            (
                "prompts/remark.md".to_string(),
                crate::navigator::default_pipeline::REMARK_PROMPT.to_string(),
            ),
            (
                "prompts/conversation_state.md".to_string(),
                crate::navigator::default_pipeline::CONVERSATION_STATE_PROMPT.to_string(),
            ),
            (
                "prompts/conversation_remark.md".to_string(),
                crate::navigator::default_pipeline::CONVERSATION_REMARK_PROMPT.to_string(),
            ),
        ]);
        Self::from_parts(
            crate::navigator::default_pipeline::PIPELINE_TOML,
            prompts,
            SnapshotSource::Builtin,
        )
    }
}

/// Reads every prompt referenced by the pipeline, relative to `base_dir`.
fn load_referenced_prompts(
    base_dir: &Path,
    file: &PipelineFile,
    source: &'static str,
) -> Result<BTreeMap<String, String>, PipelineError> {
    let mut prompts = BTreeMap::new();
    let mut load = |rel: &str| -> Result<(), PipelineError> {
        if prompts.contains_key(rel) {
            return Ok(());
        }
        let path = safe_join(base_dir, rel, source)?;
        let text = std::fs::read_to_string(&path)
            .map_err(|e| PipelineError::io(source, format!("{}: {e}", path.display())))?;
        prompts.insert(rel.to_string(), text);
        Ok(())
    };
    for trigger in &file.triggers {
        load(&trigger.state)?;
        for branch in &trigger.branch {
            if let Some(prompt) = &branch.prompt {
                load(prompt)?;
            }
        }
    }
    Ok(prompts)
}

/// Joins a relative prompt path, rejecting traversal outside `base_dir`.
fn safe_join(base_dir: &Path, rel: &str, source: &'static str) -> Result<PathBuf, PipelineError> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() || rel.contains("..") {
        return Err(PipelineError::invalid(
            source,
            format!("prompt path `{rel}` must be relative and stay inside the navigator dir"),
        ));
    }
    Ok(base_dir.join(rel_path))
}

/// Stable hash over the TOML text and every referenced prompt (sorted by
/// path). Content, not file metadata: an untouched reload yields the same
/// hash.
fn hash_parts(toml_text: &str, prompts: &BTreeMap<String, String>) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    toml_text.hash(&mut hasher);
    for (path, content) in prompts {
        path.hash(&mut hasher);
        content.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
[instinct]
model = "instinct"

[[trigger]]
on = ["save"]
state = "prompts/state.md"

[trigger.questions.go]
type = "noul"
instructions = "Worth it?"

[[trigger.branch]]
when = "go > 0.5"
action = "silent"
"#;

    fn dir_with(toml: &str, prompts: &[(&str, &str)]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("prompts")).unwrap();
        std::fs::write(tmp.path().join("pipeline.toml"), toml).unwrap();
        for (name, content) in prompts {
            std::fs::write(tmp.path().join(name), content).unwrap();
        }
        tmp
    }

    #[test]
    fn loads_from_dir_with_prompts() {
        let tmp = dir_with(
            MINIMAL,
            &[("prompts/state.md", "State: {{file}}\nDiff:\n{{diff}}")],
        );
        let snap = PipelineSnapshot::load_dir(tmp.path(), SnapshotSource::Project).unwrap();
        assert_eq!(snap.model_alias(), "instinct");
        assert_eq!(snap.source(), SnapshotSource::Project);
        assert_eq!(snap.prompts.len(), 1);
        assert_eq!(snap.hash().len(), 16);
        assert_eq!(snap.triggers().len(), 1);
    }

    #[test]
    fn hash_changes_when_a_referenced_prompt_changes() {
        let t1 = dir_with(MINIMAL, &[("prompts/state.md", "v1 {{file}}")]);
        let s1 = PipelineSnapshot::load_dir(t1.path(), SnapshotSource::Project).unwrap();
        std::fs::write(t1.path().join("prompts/state.md"), "v2 {{file}}").unwrap();
        let s2 = PipelineSnapshot::load_dir(t1.path(), SnapshotSource::Project).unwrap();
        assert_ne!(s1.hash(), s2.hash());
    }

    #[test]
    fn hash_stable_across_reloads() {
        let tmp = dir_with(MINIMAL, &[("prompts/state.md", "same")]);
        let a = PipelineSnapshot::load_dir(tmp.path(), SnapshotSource::Project).unwrap();
        let b = PipelineSnapshot::load_dir(tmp.path(), SnapshotSource::Project).unwrap();
        assert_eq!(a.hash(), b.hash());
    }

    #[test]
    fn resolution_project_beats_user_beats_builtin() {
        let ws = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();

        // Nothing anywhere: built-in.
        let snap = PipelineSnapshot::load(ws.path(), user.path()).unwrap();
        assert_eq!(snap.source(), SnapshotSource::Builtin);

        // User only (`user_dir` is the navigator dir itself, e.g.
        // ~/.config/rust-agent/navigator/).
        std::fs::create_dir_all(user.path().join("prompts")).unwrap();
        std::fs::write(user.path().join("pipeline.toml"), MINIMAL).unwrap();
        std::fs::write(user.path().join("prompts/state.md"), "{{diff}}").unwrap();
        let snap = PipelineSnapshot::load(ws.path(), user.path()).unwrap();
        assert_eq!(snap.source(), SnapshotSource::User);

        // Project wins.
        let proj = ws.path().join(".navigator");
        std::fs::create_dir_all(proj.join("prompts")).unwrap();
        std::fs::write(proj.join("pipeline.toml"), MINIMAL).unwrap();
        std::fs::write(proj.join("prompts/state.md"), "{{diff}}").unwrap();
        let snap = PipelineSnapshot::load(ws.path(), user.path()).unwrap();
        assert_eq!(snap.source(), SnapshotSource::Project);
    }

    #[test]
    fn prompt_traversal_is_rejected() {
        let evil = r#"
[instinct]
model = "instinct"

[[trigger]]
on = ["save"]
state = "../../etc/passwd"

[trigger.questions.go]
type = "noul"
instructions = "?"

[[trigger.branch]]
when = "go > 0.5"
action = "remark"
prompt = "../../secret.md"
"#;
        let tmp = dir_with(evil, &[]);
        let err = PipelineSnapshot::load_dir(tmp.path(), SnapshotSource::Project).unwrap_err();
        assert!(err.to_string().contains("must be relative"), "{err}");
    }

    #[test]
    fn missing_prompt_is_an_io_error() {
        let tmp = dir_with(MINIMAL, &[]);
        let err = PipelineSnapshot::load_dir(tmp.path(), SnapshotSource::Project).unwrap_err();
        assert!(matches!(err, PipelineError::Io { .. }), "{err}");
    }

    #[test]
    fn builtin_default_loads_and_validates() {
        let snap = PipelineSnapshot::builtin().unwrap();
        assert_eq!(snap.source(), SnapshotSource::Builtin);
        assert_eq!(snap.model_alias(), "instinct");
        assert!(!snap.hash.is_empty());
        assert!(snap.prompts.contains_key("prompts/state.md"));
        assert!(snap.prompts.contains_key("prompts/remark.md"));
    }
}
