//! Read-once skill versions: hashing and admission use the same bytes.
use super::{MAX_BODY_BYTES, Skill, frontmatter};
use crate::{
    permission::{store::PermissionStore, validation::ValidationKind},
    tools::ToolResult,
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    sync::Mutex,
};

#[derive(Debug, Clone)]
pub struct SkillSnapshot {
    pub id: String,
    pub name: String,
    pub location: String,
    pub hash: String,
    pub instructions: String,
    pub output: ToolResult,
}

#[derive(Debug, Default)]
pub struct SkillValidation {
    pub data_dir: Option<PathBuf>,
    pub snapshots: Mutex<BTreeMap<String, SkillSnapshot>>,
    pub turn_snapshots: Mutex<std::collections::BTreeSet<String>>,
    pub approved: Mutex<BTreeMap<String, String>>,
    pub active: Mutex<Option<(String, String)>>,
}

impl SkillValidation {
    pub fn validated(&self, snapshot: &SkillSnapshot) -> Result<bool, String> {
        crate::permission::validation::inspect_valid(
            self.data_dir.as_deref(),
            ValidationKind::Skill,
            &snapshot.location,
            &snapshot.hash,
        )
        .map_err(|e| e.to_string())
    }
    pub fn approve(&self, snapshot: &SkillSnapshot, always: bool) -> Result<(), String> {
        if always {
            let data = self
                .data_dir
                .as_ref()
                .ok_or("skill validation storage unavailable")?;
            PermissionStore::open(data)
                .and_then(|store| {
                    store.validate(
                        ValidationKind::Skill,
                        &snapshot.name,
                        &snapshot.location,
                        &snapshot.hash,
                    )
                })
                .map_err(|e| e.to_string())?;
        }
        self.approved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(snapshot.name.clone(), snapshot.id.clone());
        *self.active.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((snapshot.name.clone(), snapshot.instructions.clone()));
        Ok(())
    }
    pub fn begin_turn(&self) {
        let mut snapshots = self.snapshots.lock().unwrap_or_else(|e| e.into_inner());
        for id in std::mem::take(
            &mut *self
                .turn_snapshots
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        ) {
            snapshots.remove(&id);
        }
        for id in self
            .approved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            snapshots.remove(id);
        }
        drop(snapshots);
        self.approved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        *self.active.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

pub fn snapshot(skill: &Skill) -> Result<SkillSnapshot, String> {
    if crate::tools::path_key(
        &crate::tools::canonical_path(&skill.base_dir).map_err(|e| e.to_string())?,
    ) != crate::tools::path_key(&skill.base_dir)
    {
        return Err("skill directory changed since discovery".into());
    }
    fn walk(
        root: &Path,
        dir: &Path,
        files: &mut BTreeMap<String, Vec<u8>>,
        total: &mut usize,
        entries: &mut usize,
    ) -> Result<(), String> {
        for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            *entries += 1;
            if *entries > 4096 {
                return Err("skill inventory exceeds 4096 entries; cannot validate".into());
            }
            let kind = entry.file_type().map_err(|e| e.to_string())?;
            if kind.is_symlink() {
                return Err(
                    "skill contains a symbolic link; cannot validate a complete version".into(),
                );
            }
            if kind.is_dir() {
                walk(root, &entry.path(), files, total, entries)?;
            } else if kind.is_file() {
                if files.len() >= 256 {
                    return Err("skill exceeds 256 files; cannot validate".into());
                }
                let mut bytes = vec![];
                std::fs::File::open(entry.path())
                    .map_err(|e| e.to_string())?
                    .take((8 * 1024 * 1024 - *total + 1) as u64)
                    .read_to_end(&mut bytes)
                    .map_err(|e| e.to_string())?;
                *total += bytes.len();
                if *total > 8 * 1024 * 1024 {
                    return Err("skill exceeds 8 MiB; cannot validate".into());
                }
                let path = entry.path();
                let relative = path
                    .strip_prefix(root)
                    .map_err(|e| e.to_string())?
                    .to_str()
                    .ok_or("skill paths must be UTF-8")?
                    .to_owned();
                #[cfg(windows)]
                let relative = relative.replace('\\', "/");
                files.insert(relative, bytes);
            } else {
                return Err("skill contains a non-regular file; cannot validate".into());
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    walk(&skill.base_dir, &skill.base_dir, &mut files, &mut 0, &mut 0)?;
    let manifest = files.get("SKILL.md").ok_or("skill manifest missing")?;
    let text = std::str::from_utf8(manifest).map_err(|e| e.to_string())?;
    let (_, body) = frontmatter::split_bounded(text)?;
    let body = body.trim_matches('\n');
    if body.len() > MAX_BODY_BYTES {
        return Err("skill body exceeds 32 KiB; cannot load".into());
    }
    let mut hasher = Sha256::new();
    for (path, bytes) in &files {
        hasher.update((path.len() as u64).to_be_bytes());
        hasher.update(path.as_bytes());
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    }
    let hash = format!("sha256:{}", hex::encode(hasher.finalize()));
    let resources: Vec<_> = files
        .keys()
        .filter(|p| p.as_str() != "SKILL.md")
        .cloned()
        .collect();
    let output = ToolResult::ok(format!("<skill name=\"{}\" path=\"{}\" base_dir=\"{}\">\n{body}\n</skill>\n\nFiles in the skill directory:\n{}", skill.name, skill.path.display(), skill.base_dir.display(), resources.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n"))).with_metadata(serde_json::json!({"name":skill.name,"path":skill.path,"base_dir":skill.base_dir,"resources":resources,"resources_omitted":0}));
    Ok(SkillSnapshot {
        id: ulid::Ulid::generate().to_string(),
        name: skill.name.clone(),
        location: skill.base_dir.to_string_lossy().into_owned(),
        hash,
        instructions: body.into(),
        output,
    })
}

/// Legacy Captain attachments and earlier-turn skill bodies are conversation
/// history, not live authority. Storage stays intact; only the model view changes.
pub fn project_skill_history(
    transcript: &[crate::provider::openai::ChatMessage],
    current_start: Option<usize>,
) -> Vec<crate::provider::openai::ChatMessage> {
    use crate::provider::openai::ChatMessage;
    transcript.iter().enumerate().map(|(index, message)| {
        let mut message = message.clone();
        let captain = message.is_captain_message();
        let ChatMessage::User { content, .. } = &mut message else { return message; };
        let current = current_start.is_some_and(|start| index >= start);
        for (open, close) in [("<captain_selected_skill encoding=\"json\">", "</captain_selected_skill>"), ("<driver_selected_skill encoding=\"json\">", "</driver_selected_skill>")] {
            let mut from = 0;
            while let Some(offset) = content[from..].find(open) {
                let start = from + offset;
                let Some(offset) = content[start + open.len()..].find(close) else { content.truncate(start); break; };
                let end = start + open.len() + offset;
                let value = serde_json::from_str::<serde_json::Value>(&content[start + open.len()..end]).ok();
                let historical = captain || !current;
                if historical {
                    let name = value.as_ref().and_then(|v| v["name"].as_str()).unwrap_or("unknown");
                    let note = format!("[Historical, non-authorising host context: skill {name} was used earlier]");
                    content.replace_range(start..end + close.len(), &note);
                    from = start + note.len();
                } else { from = end + close.len(); }
            }
        }
        message
    }).collect()
}

#[cfg(test)]
mod projection_tests {
    use super::*;
    use crate::provider::openai::{ChatMessage, UserProvenance};
    #[test]
    fn resumed_legacy_and_past_turn_blocks_cannot_authorize() {
        let block = "<captain_selected_skill encoding=\"json\">{\"name\":\"review\",\"selected_by\":\"captain\",\"content\":\"SECRET SKILL BODY\"}</captain_selected_skill>";
        let transcript = vec![
            ChatMessage::User {
                content: format!("old request\n{block}"),
                provenance: UserProvenance::Captain,
                images: Vec::new(),
            },
            ChatMessage::User {
                content: block.into(),
                provenance: UserProvenance::Navigator,
                images: Vec::new(),
            },
            ChatMessage::User {
                content: "new request".into(),
                provenance: UserProvenance::Captain,
                images: Vec::new(),
            },
            ChatMessage::User {
                content: block.into(),
                provenance: UserProvenance::Navigator,
                images: Vec::new(),
            },
        ];
        let resumed: Vec<ChatMessage> =
            serde_json::from_str(&serde_json::to_string(&transcript).unwrap()).unwrap();
        let projected = project_skill_history(&resumed, Some(2));
        assert!(
            !serde_json::to_string(&projected[..2])
                .unwrap()
                .contains("SECRET SKILL BODY")
        );
        assert!(
            serde_json::to_string(&projected[3])
                .unwrap()
                .contains("SECRET SKILL BODY")
        );
        assert!(
            !serde_json::to_string(&project_skill_history(&resumed, None))
                .unwrap()
                .contains("SECRET SKILL BODY")
        );
    }
}
