//! Editor event names and the document cooldown sentinel.

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TriggerKind {
    Save,
    Idle,
    Diagnostics,
    Conversation,
}

impl TriggerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Save => "save",
            Self::Idle => "idle",
            Self::Diagnostics => "diagnostics",
            Self::Conversation => "conversation",
        }
    }
}

pub(crate) const NO_REMARK_MS: i64 = i64::MAX / 2;
