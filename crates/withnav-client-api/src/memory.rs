//! Memory register wire types. Scope references are empty for global items.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Global,
    Group,
    Project,
    Session,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryVersion {
    pub version: u32,
    pub change: String,
    pub title: String,
    pub body: String,
    pub why: Option<String>,
    pub quote: Option<String>,
    pub scope: Scope,
    pub scope_ref: String,
    pub source_actor: String,
    pub drafted_by: String,
    pub confirmed_by: Option<String>,
    pub source_session_id: Option<String>,
    pub source_event_seq: Option<u64>,
    pub reason: Option<String>,
    pub at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryItem {
    pub id: String,
    pub kind: String,
    pub scope: Scope,
    pub scope_ref: String,
    pub status: String,
    pub current_version: u32,
    pub pinned: bool,
    pub created_at: String,
    pub updated_at: String,
    pub current: MemoryVersion,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub group_answered: bool,
    pub groups: Vec<String>,
    pub aliases: Vec<String>,
    pub sessions: Vec<SessionLink>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionLink {
    pub session_id: String,
    pub project_id: String,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub members: Vec<String>,
    pub items: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MembershipChange {
    pub id: i64,
    #[serde(flatten)]
    pub relation: Relation,
    pub change: String,
    pub status: String,
    pub actor: String,
    pub session_id: Option<String>,
    pub at: String,
    pub decided_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "relation", rename_all = "snake_case")]
pub enum Relation {
    ProjectGroup {
        project_id: String,
        group_id: String,
    },
    SessionProject {
        linked_session_id: String,
        project_id: String,
    },
}
