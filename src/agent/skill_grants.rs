//! Deliberate machine-wide Captain grants, bound to one user skill and script digest.
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillScript {
    pub skill: String,
    pub base_dir: PathBuf,
    pub relative_path: PathBuf,
    pub sha256: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_grants_persist_digests_and_revoke_without_lost_updates() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = SkillGrants {
            file: dir.path().join("skill-grants.json"),
        };
        let workers: Vec<_> = (0..12)
            .map(|n| {
                let store = store.clone();
                std::thread::spawn(move || {
                    store
                        .grant(&SkillScript {
                            skill: "sync".into(),
                            base_dir: PathBuf::from("/home/user/.agents/skills/sync"),
                            relative_path: PathBuf::from(format!("scripts/{n}.py")),
                            sha256: format!("hash{n}"),
                        })
                        .unwrap()
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(store.read().unwrap()[0].scripts.len(), 12);
        assert_eq!(
            std::fs::metadata(&store.file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let mut script = SkillScript {
            skill: "sync".into(),
            base_dir: PathBuf::from("/home/user/.agents/skills/sync"),
            relative_path: PathBuf::from("scripts/0.py"),
            sha256: "hash0".into(),
        };
        assert!(store.covers(&script));
        script.sha256 = "changed".into();
        assert!(!store.covers(&script));
        assert!(store.changed(&script));
        store.revoke("sync").unwrap();
        assert!(!store.covers(&script));
        std::fs::write(&store.file, b"broken").unwrap();
        assert!(store.grant(&script).is_err());
        assert_eq!(std::fs::read(&store.file).unwrap(), b"broken");
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillGrant {
    pub skill: String,
    pub base_dir: PathBuf,
    pub scripts: BTreeMap<PathBuf, String>,
    pub granted_at: String,
}

#[derive(Debug, Clone)]
pub struct SkillGrants {
    pub file: PathBuf,
}

impl SkillGrants {
    pub fn read(&self) -> Result<Vec<SkillGrant>, String> {
        use std::io::Read;
        match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&self.file)
        {
            Ok(mut file) => {
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
                serde_json::from_slice(&bytes).map_err(|e| e.to_string())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn covers(&self, script: &SkillScript) -> bool {
        self.read().is_ok_and(|grants| {
            grants.iter().any(|grant| {
                grant.skill == script.skill
                    && grant.base_dir == script.base_dir
                    && grant.scripts.get(&script.relative_path) == Some(&script.sha256)
            })
        })
    }

    pub fn changed(&self, script: &SkillScript) -> bool {
        self.read().is_ok_and(|grants| {
            grants
                .iter()
                .any(|grant| grant.skill == script.skill && grant.base_dir == script.base_dir)
        }) && !self.covers(script)
    }

    pub fn grant(&self, script: &SkillScript) -> Result<(), String> {
        self.update(|grants| {
            let index = grants
                .iter()
                .position(|grant| grant.skill == script.skill && grant.base_dir == script.base_dir);
            let index = index.unwrap_or_else(|| {
                grants.push(SkillGrant {
                    skill: script.skill.clone(),
                    base_dir: script.base_dir.clone(),
                    scripts: BTreeMap::new(),
                    granted_at: ulid::Ulid::new().to_string(),
                });
                grants.len() - 1
            });
            grants[index]
                .scripts
                .insert(script.relative_path.clone(), script.sha256.clone());
            grants[index].granted_at = ulid::Ulid::new().to_string();
        })
    }

    pub fn revoke(&self, skill: &str) -> Result<(), String> {
        self.update(|grants| grants.retain(|grant| grant.skill != skill))
    }

    fn update(&self, change: impl FnOnce(&mut Vec<SkillGrant>)) -> Result<(), String> {
        let parent = self.file.parent().ok_or("grant file has no parent")?;
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(self.file.with_extension("json.lock"))
            .map_err(|e| e.to_string())?;
        let _lock = nix::fcntl::Flock::lock(lock, nix::fcntl::FlockArg::LockExclusive)
            .map_err(|(_, e)| e.to_string())?;
        let mut grants = self.read()?;
        change(&mut grants);
        let bytes = serde_json::to_vec_pretty(&grants).map_err(|e| e.to_string())?;
        let tmp = self
            .file
            .with_extension(format!("{}.tmp", ulid::Ulid::new()));
        let result = (|| {
            let mut out = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|e| e.to_string())?;
            out.write_all(&bytes)
                .and_then(|()| out.sync_all())
                .map_err(|e| e.to_string())?;
            std::fs::rename(&tmp, &self.file).map_err(|e| e.to_string())?;
            std::fs::File::open(parent)
                .and_then(|dir| dir.sync_all())
                .map_err(|e| e.to_string())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(tmp);
        }
        result
    }
}
