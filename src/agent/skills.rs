//! Skills: `SKILL.md` files a session finds once, when it is created or
//! resumed, and offers to the model as a catalog plus a `skill` tool that
//! loads one by id.
//!
//! This lives in `agent/` and not in `tools/` for two reasons: `tools/` sits
//! below `agent/` and may not import it, and discovery reuses the worktree
//! logic and the injectable home of [`crate::agent::instructions`].
//!
//! Where it looks, nearest first:
//!
//! 1. for each directory from the workspace up to the worktree root:
//!    `.agents/skills`, `.claude/skills`, `.opencode/skills`;
//! 2. the user's `<home>/.agents/skills`, then `<home>/.claude/skills`.
//!
//! Each root is walked recursively (the real library is nested:
//! `~/.agents/skills/matt/ask-matt/SKILL.md`), down to [`MAX_DEPTH`] levels
//! and [`MAX_ENTRIES`] directory entries in all, without `.git`,
//! `node_modules` or `target`, and without going below a directory that
//! holds a `SKILL.md`. A link is followed only while it stays inside one of
//! the skill roots; a loop or a second path to the same directory counts once.
//!
//! Two skills with one id: the better [`Rank`] wins (nearer project scope,
//! then `.agents` before `.claude` before `.opencode`, then the user's
//! `.agents` before `.claude`). Equal ranks make the id ambiguous: neither is
//! offered, and the tool says so.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::agent::instructions::{directories_from_root, RulesRoots};

mod catalog;
pub(crate) mod frontmatter;
mod tool;

pub use catalog::{MAX_DESCRIPTION_CHARS, MAX_TOKENS};
pub use tool::{SkillTool, MAX_BODY_BYTES, MAX_RESOURCES};

/// The deepest level, below a skill root, a `SKILL.md` may sit at.
pub const MAX_DEPTH: usize = 8;
/// The most directory entries one discovery looks at.
pub const MAX_ENTRIES: usize = 20_000;

const SKIPPED_DIRS: [&str; 3] = [".git", "node_modules", "target"];
const PROJECT_SOURCES: [&str; 3] = [".agents", ".claude", ".opencode"];
const USER_SOURCES: [&str; 2] = [".agents", ".claude"];

/// How well a skill ranks against another with the same id; lower wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rank {
    /// `true` for the user's skills, which any project skill beats.
    user: bool,
    /// Directories between the workspace and the scope (0 = the workspace).
    distance: u16,
    /// Index in the order of the source directories.
    source: u8,
}

/// One valid `SKILL.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// The canonical path of the `SKILL.md`.
    pub path: PathBuf,
    /// Its canonical directory.
    pub base_dir: PathBuf,
    pub disabled: bool,
    rank: Rank,
}

/// What went wrong, or was set aside, for one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticKind {
    /// The header is missing, unclosed, not valid YAML or lacks a field.
    Invalid(String),
    /// Another skill with the same id ranks higher.
    Shadowed { by: PathBuf },
    /// Equal-rank skills share the id; none of them is offered.
    Ambiguous { with: Vec<PathBuf> },
    /// A link leaves the skill roots.
    Escapes { to: PathBuf },
    /// A link points back to a directory being walked.
    Cycle { to: PathBuf },
    /// A bound stopped the walk.
    LimitReached(&'static str),
    /// The file or directory could not be read.
    ReadError(String),
}

impl fmt::Display for DiagnosticKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(why) => write!(f, "skipped: {why}"),
            Self::Shadowed { by } => write!(f, "shadowed by {}", by.display()),
            Self::Ambiguous { with } => {
                let others: Vec<_> = with.iter().map(|p| p.display().to_string()).collect();
                write!(f, "ambiguous with {}", others.join(", "))
            }
            Self::Escapes { to } => write!(f, "link leaves the skill roots (to {})", to.display()),
            Self::Cycle { to } => write!(f, "link loops back to {}", to.display()),
            Self::LimitReached(what) => write!(f, "walk stopped: {what}"),
            Self::ReadError(why) => write!(f, "read error: {why}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub path: PathBuf,
    pub kind: DiagnosticKind,
}

impl Diagnostic {
    /// Whether the driver should be told; a shadowed skill is ordinary.
    pub fn is_problem(&self) -> bool {
        !matches!(self.kind, DiagnosticKind::Shadowed { .. })
    }
}

/// What the `skill` tool finds for an id.
pub(crate) enum Resolution<'a> {
    Found(&'a Skill),
    Disabled,
    Ambiguous(&'a [PathBuf]),
    Unknown,
}

/// The snapshot taken when a session is created or resumed.
#[derive(Debug, Clone, Default)]
pub struct Skills {
    /// The winning skill of each id.
    winners: BTreeMap<String, Skill>,
    ambiguous: BTreeMap<String, Vec<PathBuf>>,
    diagnostics: Vec<Diagnostic>,
    roots: Vec<PathBuf>,
    catalog: Option<String>,
}

impl Skills {
    /// Discovers the skills of `workspace`. Blocking file I/O: call it from a
    /// blocking context.
    pub fn load(roots: &RulesRoots, workspace: &Path) -> Self {
        let workspace =
            std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
        let mut walk = Walk::default();
        for (dir, root) in candidate_roots(roots, &workspace) {
            walk.add_root(dir, root);
        }
        walk.run();
        walk.finish()
    }

    /// The catalog message; `None` when no skill is offered.
    pub fn catalog(&self) -> Option<&str> {
        self.catalog.as_deref()
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// The valid skills, offered or not.
    pub fn len(&self) -> usize {
        self.winners.len() + self.ambiguous.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The canonical roots that hold a skill: what `read` and `grep` may
    /// additionally open. Never a place to write.
    pub fn readable_roots(&self) -> &[PathBuf] {
        &self.roots
    }

    pub(crate) fn resolve(&self, name: &str) -> Resolution<'_> {
        if let Some(paths) = self.ambiguous.get(name) {
            return Resolution::Ambiguous(paths);
        }
        match self.winners.get(name) {
            Some(skill) if skill.disabled => Resolution::Disabled,
            Some(skill) => Resolution::Found(skill),
            None => Resolution::Unknown,
        }
    }
}

/// The roots to look at, nearest first, with their rank.
fn candidate_roots(roots: &RulesRoots, workspace: &Path) -> Vec<(PathBuf, Rank)> {
    let mut found = Vec::new();
    for (distance, dir) in directories_from_root(workspace).iter().rev().enumerate() {
        for (source, name) in PROJECT_SOURCES.iter().enumerate() {
            found.push((
                dir.join(name).join("skills"),
                Rank {
                    user: false,
                    distance: distance as u16,
                    source: source as u8,
                },
            ));
        }
    }
    if let Some(home) = &roots.home {
        for (source, name) in USER_SOURCES.iter().enumerate() {
            found.push((
                home.join(name).join("skills"),
                Rank {
                    user: true,
                    distance: 0,
                    source: source as u8,
                },
            ));
        }
    }
    found
}

#[derive(Default)]
struct Walk {
    /// Canonical roots with their rank, in priority order.
    roots: Vec<(PathBuf, Rank)>,
    entries: usize,
    /// The entry budget is spent: nothing more is read, in any root.
    exhausted: bool,
    /// Canonical directories already walked.
    seen_dirs: HashSet<PathBuf>,
    found: Vec<Skill>,
    diagnostics: Vec<Diagnostic>,
}

impl Walk {
    fn add_root(&mut self, dir: PathBuf, rank: Rank) {
        // An absent root is normal; one reached twice (a link, a home that is
        // the worktree) keeps its best rank.
        if let Ok(canonical) = std::fs::canonicalize(&dir) {
            if canonical.is_dir() && !self.roots.iter().any(|(r, _)| *r == canonical) {
                self.roots.push((canonical, rank));
            }
        }
    }

    fn run(&mut self) {
        for (root, rank) in self.roots.clone() {
            if self.exhausted {
                break;
            }
            let mut stack = Vec::new();
            self.dir(&root, &root, rank, 0, &mut stack);
        }
    }

    fn inside_roots(&self, path: &Path) -> bool {
        self.roots.iter().any(|(root, _)| path.starts_with(root))
    }

    fn note(&mut self, path: &Path, kind: DiagnosticKind) {
        self.diagnostics.push(Diagnostic {
            path: path.to_path_buf(),
            kind,
        });
    }

    /// Walks `dir` (canonical): a `SKILL.md` ends the descent.
    fn dir(&mut self, dir: &Path, root: &Path, rank: Rank, depth: usize, stack: &mut Vec<PathBuf>) {
        if self.exhausted || !self.seen_dirs.insert(dir.to_path_buf()) {
            return;
        }
        let manifest = dir.join("SKILL.md");
        if std::fs::metadata(&manifest).is_ok_and(|m| m.is_file()) {
            self.manifest(&manifest, rank);
            return;
        }
        if depth >= MAX_DEPTH {
            return;
        }
        // Never take more entries from the iterator than the budget left
        // (plus one, to know the directory was cut): a huge directory is
        // neither collected nor sorted whole.
        let remaining = MAX_ENTRIES.saturating_sub(self.entries);
        let mut names = match std::fs::read_dir(dir) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .map(|e| e.file_name())
                .take(remaining + 1)
                .collect::<Vec<_>>(),
            Err(e) => return self.note(dir, DiagnosticKind::ReadError(e.to_string())),
        };
        if names.len() > remaining {
            names.truncate(remaining);
            self.exhausted = true;
            self.note(root, DiagnosticKind::LimitReached("too many entries"));
        }
        self.entries += names.len();
        names.sort();
        stack.push(dir.to_path_buf());
        for name in names {
            if name.to_str().is_some_and(|n| SKIPPED_DIRS.contains(&n)) {
                continue;
            }
            let path = dir.join(&name);
            // Follows a link: only directories matter here.
            if !std::fs::metadata(&path).is_ok_and(|m| m.is_dir()) {
                continue;
            }
            let Ok(canonical) = std::fs::canonicalize(&path) else {
                continue;
            };
            if !self.inside_roots(&canonical) {
                self.note(&path, DiagnosticKind::Escapes { to: canonical });
            } else if stack.contains(&canonical) {
                self.note(&path, DiagnosticKind::Cycle { to: canonical });
            } else {
                self.dir(&canonical, root, rank, depth + 1, stack);
            }
            if self.exhausted {
                break;
            }
        }
        stack.pop();
    }

    fn manifest(&mut self, manifest: &Path, rank: Rank) {
        let Ok(path) = std::fs::canonicalize(manifest) else {
            return;
        };
        if !self.inside_roots(&path) {
            return self.note(manifest, DiagnosticKind::Escapes { to: path });
        }
        let text = match read_header(&path) {
            Ok(text) => text,
            Err(e) => return self.note(&path, DiagnosticKind::ReadError(e.to_string())),
        };
        match frontmatter::parse(&text) {
            Ok((meta, _)) => self.found.push(Skill {
                name: meta.name,
                description: meta.description,
                base_dir: path.parent().map(Path::to_path_buf).unwrap_or_default(),
                path,
                disabled: meta.disabled,
                rank,
            }),
            Err(why) => self.note(&path, DiagnosticKind::Invalid(why)),
        }
    }

    /// Settles the ids and builds the snapshot.
    fn finish(self) -> Skills {
        let Walk {
            roots,
            found,
            mut diagnostics,
            ..
        } = self;
        // One manifest reached by several routes (a `SKILL.md` that is a link
        // to another one, a directory linked twice) is one skill, at the best
        // rank any route gave it: it must never make its own id ambiguous.
        let mut by_manifest: BTreeMap<PathBuf, Skill> = BTreeMap::new();
        for skill in found {
            match by_manifest.get(&skill.path) {
                Some(kept) if kept.rank <= skill.rank => {}
                _ => {
                    by_manifest.insert(skill.path.clone(), skill);
                }
            }
        }
        let mut by_name: BTreeMap<String, Vec<Skill>> = BTreeMap::new();
        for skill in by_manifest.into_values() {
            by_name.entry(skill.name.clone()).or_default().push(skill);
        }
        let mut winners = BTreeMap::new();
        let mut ambiguous = BTreeMap::new();
        for (name, mut group) in by_name {
            group.sort_by(|a, b| a.rank.cmp(&b.rank).then_with(|| a.path.cmp(&b.path)));
            let best = group.remove(0);
            let (tied, worse): (Vec<_>, Vec<_>) = group
                .into_iter()
                .partition(|s| s.rank.cmp(&best.rank) == Ordering::Equal);
            let lost_to = |diagnostics: &mut Vec<Diagnostic>, losers: Vec<Skill>| {
                for s in losers {
                    diagnostics.push(Diagnostic {
                        path: s.path,
                        kind: DiagnosticKind::Shadowed {
                            by: best.path.clone(),
                        },
                    });
                }
            };
            if tied.is_empty() {
                lost_to(&mut diagnostics, worse);
                winners.insert(name, best);
            } else {
                let mut paths: Vec<PathBuf> = vec![best.path.clone()];
                paths.extend(tied.iter().map(|s| s.path.clone()));
                for p in &paths {
                    diagnostics.push(Diagnostic {
                        path: p.clone(),
                        kind: DiagnosticKind::Ambiguous {
                            with: paths.iter().filter(|o| *o != p).cloned().collect(),
                        },
                    });
                }
                ambiguous.insert(name, paths);
            }
        }
        let roots = roots
            .into_iter()
            .map(|(root, _)| root)
            .filter(|root| {
                winners.values().any(|s| s.base_dir.starts_with(root))
                    || ambiguous.values().flatten().any(|p| p.starts_with(root))
            })
            .collect();
        let mut offered: Vec<&Skill> = winners.values().filter(|s| !s.disabled).collect();
        offered.sort_by(|a, b| a.rank.cmp(&b.rank).then_with(|| a.name.cmp(&b.name)));
        let catalog = catalog::render(&offered);
        Skills {
            winners,
            ambiguous,
            diagnostics,
            roots,
            catalog,
        }
    }
}

/// The start of a `SKILL.md`: enough to hold any header.
fn read_header(path: &Path) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(frontmatter::MAX_HEADER_BYTES as u64)
        .read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests;
