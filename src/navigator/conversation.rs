//! The conversation feed: the in-process seam between the session server
//! (where the cognition's sessions live) and the language server (where the
//! fast layers watch the Captain). One hub shared by both — the whole
//! navigator runs in one process over two channels.
//!
//! It tracks the ACTIVE conversation — the latest editor session
//! that received a captain message — keeps a bounded digest of it, remembers
//! whether the cognition is waiting on an `ask`, and posts wakes: short
//! navigator-sourced messages routed in-process, never over HTTP, so the
//! durable `message.received.source` keeps telling the panel and this feed
//! who wrote what.
//!
//! The ask guard lives here, where the wake is posted: the first message
//! after a yield answers the pending `ask`, and a navigator message must
//! never become that answer. So while the active conversation waits on the
//! captain, wakes are queued — coalesced, latest wins — and delivered right
//! after the next captain message has been accepted. (An open permission
//! request is guarded by the session runtime, not here: it holds a
//! navigator-sourced message until nothing is pending.) A navigator-sourced
//! message is also never a conversation trigger: the digest marks it as the
//! navigator's own words, and the turn it provokes is reported as not
//! captain-caused, so a wake never becomes a conversation trigger (no
//! self-reinforcing loops).
//!
//! The language server subscribes ([`ConversationFeed::subscribe`]) to hear
//! when a turn ends and whether the Captain caused it. Replayed events
//! rebuild state but stay silent: a resumed session must not re-trigger
//! the turns it already lived.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

use crate::bus::event::EventEnvelope;
use crate::navigator::log::excerpt;

/// The actor id of the navigator itself: the sender of in-process wakes,
/// never choosable by an HTTP client (the server assigns `user:http`
/// there, so the two provenances cannot be confused on the wire).
pub const NAVIGATOR_ACTOR_ID: &str = "navigator";

/// How many exchanges of the active conversation the digest keeps.
const MAX_EXCHANGES: usize = 12;

/// Each exchange's text is cut around this many characters: enough to
/// recognize what was agreed, not enough to mirror the whole chat.
const MAX_EXCHANGE_CHARS: usize = 600;

/// Posts one navigator-sourced message into a live session. Built by the
/// session server around a session handle; the feed stays independent of
/// the server types.
pub type SessionPost =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> + Send + Sync>;

/// Asks the Captain's editor to reveal a file: installed by the language
/// server, called by the cognition's `reveal` tool.
pub type EditorReveal = Arc<
    dyn Fn(
            crate::navigator::lsp::protocol::RevealParams,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<crate::navigator::lsp::protocol::RevealResult, String>>
                    + Send,
            >,
        > + Send
        + Sync,
>;

/// Typed flow effects bypass the message inbox; notes need not start a turn.
pub(crate) type FlowPost =
    Arc<dyn Fn(String, String, String, bool) -> Result<(), String> + Send + Sync>;

/// Opaque identifier for a registered language server editor connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EditorId(Ulid);

impl EditorId {
    pub fn new() -> Self {
        Self(Ulid::generate())
    }

    pub fn from_ulid(id: Ulid) -> Self {
        Self(id)
    }

    pub fn ulid(self) -> Ulid {
        self.0
    }
}

impl Default for EditorId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for EditorId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone)]
struct RegisteredEditor {
    reveal: EditorReveal,
    cancel: CancellationToken,
}

/// Resolved editor target from the selection algorithm.
#[derive(Clone)]
pub struct SelectedEditor {
    pub id: EditorId,
    pub reveal: EditorReveal,
    pub cancel: CancellationToken,
}

impl std::fmt::Debug for SelectedEditor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SelectedEditor")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// Outcome when an editor target cannot be unambiguously selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorSelectionError {
    Unavailable,
    Ambiguous,
}

impl std::fmt::Display for EditorSelectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => write!(f, "no editor is connected"),
            Self::Ambiguous => write!(f, "multiple editors are connected, target is ambiguous"),
        }
    }
}

impl std::error::Error for EditorSelectionError {}

/// Error returned when binding an editor to a session fails validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingError {
    SessionUnavailable,
    EditorUnavailable,
    WorkspaceMismatch,
}

impl std::fmt::Display for BindingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionUnavailable => write!(f, "session is not available or registered"),
            Self::EditorUnavailable => write!(f, "editor is not registered"),
            Self::WorkspaceMismatch => write!(f, "workspace mismatch between target and feed"),
        }
    }
}

impl std::error::Error for BindingError {}

/// RAII registration guard that unregisters the editor and cancels pending work on drop or detach.
pub struct EditorRegistration {
    feed: ConversationFeed,
    editor_id: EditorId,
    detached: std::sync::atomic::AtomicBool,
}

impl EditorRegistration {
    pub fn new(feed: ConversationFeed, editor_id: EditorId) -> Self {
        Self {
            feed,
            editor_id,
            detached: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn editor_id(&self) -> EditorId {
        self.editor_id
    }

    pub fn detach(self) {
        if !self
            .detached
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.feed.unregister_editor(self.editor_id);
        }
    }
}

impl Drop for EditorRegistration {
    fn drop(&mut self) {
        if !self
            .detached
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.feed.unregister_editor(self.editor_id);
        }
    }
}

/// Pinned target identifying a specific workspace, session, and incarnation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConversationTarget {
    pub workspace: PathBuf,
    pub session_id: Ulid,
    pub incarnation: Ulid,
}

/// Atomic snapshot of active conversation digest, target, and availability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestSnapshot {
    pub target: Option<ConversationTarget>,
    pub digest: String,
    pub waiting_on_captain: bool,
    pub available: bool,
}

/// Typed workspace identity for memory routing.
/// Prevents relative paths from resolving against cwd and distinguishes
/// missing absolute paths from canonical existing paths.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WorkspaceKey {
    /// Unspecified workspace (legacy single-feed mode).
    Unspecified,
    /// Existing canonicalized absolute directory.
    Canonical(PathBuf),
    /// Missing or unresolvable absolute directory.
    MissingAbsolute(PathBuf),
    /// Unresolved relative path preserved as-is without resolving relative to current working directory.
    Relative(PathBuf),
}

impl WorkspaceKey {
    pub fn from_path(path: &Path) -> Self {
        if path.as_os_str().is_empty() {
            return Self::Unspecified;
        }
        if path.is_relative() {
            return Self::Relative(path.to_path_buf());
        }
        if path.exists()
            && let Ok(canonical) = std::fs::canonicalize(path)
        {
            return Self::Canonical(canonical);
        }
        Self::MissingAbsolute(path.to_path_buf())
    }

    pub fn is_unspecified(&self) -> bool {
        matches!(self, Self::Unspecified)
    }
}

/// Feed registry selecting conversation feeds by canonical workspace identity.
#[derive(Clone, Debug, Default)]
pub struct WorkspaceFeedRegistry {
    feeds: Arc<Mutex<HashMap<WorkspaceKey, ConversationFeed>>>,
}

impl WorkspaceFeedRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolves typed workspace identity without resolving relative paths against cwd.
    pub fn key(workspace: &Path) -> WorkspaceKey {
        WorkspaceKey::from_path(workspace)
    }

    /// Resolves canonical workspace identity without rewriting stored paths,
    /// preserving missing historical paths without resolving relative to current working directory.
    pub fn canonical_key(workspace: &Path) -> PathBuf {
        match Self::key(workspace) {
            WorkspaceKey::Canonical(p) => p,
            WorkspaceKey::MissingAbsolute(p) => p,
            WorkspaceKey::Relative(p) => p,
            WorkspaceKey::Unspecified => PathBuf::new(),
        }
    }

    /// Returns a conversation feed for the workspace if one exists.
    pub fn get(&self, workspace: &Path) -> Option<ConversationFeed> {
        let key = Self::key(workspace);
        self.feeds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned()
    }

    /// Returns an existing conversation feed or creates a new one bound to this workspace.
    pub fn get_or_create(&self, workspace: &Path) -> ConversationFeed {
        let key = Self::key(workspace);
        let mut guard = self.feeds.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .entry(key)
            .or_insert_with(|| ConversationFeed::with_workspace(workspace.to_path_buf()))
            .clone()
    }

    /// Registers a specific feed for a workspace.
    pub fn insert(&self, workspace: &Path, feed: ConversationFeed) {
        let key = Self::key(workspace);
        self.feeds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, feed);
    }
}

/// What came of posting a wake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeOutcome {
    /// Posted as a navigator-sourced message to this session.
    Posted(Ulid),
    /// The cognition is waiting on the Captain: queued (latest wins), to be
    /// delivered right after the next captain message.
    Queued(Ulid),
    /// No active conversation in this process.
    NoSession,
    /// The session exists but refused the post (stopping, full inbox).
    Failed(Ulid, String),
}

/// What the feed tells its subscribers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationEvent {
    /// A navigator session finished a turn. `caused_by_captain` is true only
    /// when the turn answered a captain message (`message.received` whose
    /// source starts with `user:`); a turn provoked by a navigator wake is
    /// false, which keeps wakes from triggering themselves.
    TurnEnded {
        session_id: Ulid,
        incarnation: Ulid,
        caused_by_captain: bool,
        target_editor: Option<EditorId>,
    },
}

/// Capacity of the subscription channel; a lagging subscriber skips ahead.
const EVENT_CAPACITY: usize = 32;

/// Who said it, in the digest's vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Author {
    Captain,
    Cognition,
    Navigator,
    Subagent,
}

impl Author {
    fn as_str(self) -> &'static str {
        match self {
            Author::Captain => "captain",
            Author::Cognition => "cognition",
            Author::Navigator => "navigator",
            Author::Subagent => "subagent (untrusted)",
        }
    }
}

#[derive(Debug, Clone)]
struct Exchange {
    author: Author,
    text: String,
}

/// One watched navigator session: its poster, its ask state, its queued
/// wake, its bounded digest, and its lifecycle incarnation.
struct SessionConversation {
    flow_post: Option<FlowPost>,
    post: SessionPost,
    waiting_on_captain: bool,
    queued_wake: Option<String>,
    exchanges: VecDeque<Exchange>,
    captain_pending: bool,
    incarnation: Ulid,
    available: bool,
    invalidated: bool,
    has_captain_turns: bool,
}

impl SessionConversation {
    fn push_exchange(&mut self, author: Author, text: &str) {
        // Only captain messages carry the editor-context block the editor
        // appends at send time; it describes the editor, not the Captain.
        let text = match author {
            Author::Captain => strip_editor_context(crate::mcp::captain_prose(text)),
            _ => text.to_string(),
        };
        let text = excerpt(text.trim(), MAX_EXCHANGE_CHARS);
        self.exchanges.push_back(Exchange { author, text });
        while self.exchanges.len() > MAX_EXCHANGES {
            self.exchanges.pop_front();
        }
    }
}

#[derive(Default)]
struct FeedState {
    sessions: HashMap<Ulid, SessionConversation>,
    active: Option<Ulid>,
    editors: HashMap<EditorId, RegisteredEditor>,
    /// Maps session id to (bound editor id, bound incarnation).
    session_editors: HashMap<Ulid, (EditorId, Ulid)>,
    legacy_editor: Option<EditorReveal>,
}

/// The shared hub. Cheap to clone (one `Arc`); every method is safe under
/// concurrency. Locks are standard mutexes held only across synchronous
/// steps — awaits happen after the guard is dropped.
#[derive(Clone)]
pub struct ConversationFeed {
    workspace: PathBuf,
    inner: Arc<Mutex<FeedState>>,
    events: broadcast::Sender<ConversationEvent>,
    ready_notify: Arc<tokio::sync::Notify>,
}

impl Default for ConversationFeed {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ConversationFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.lock();
        f.debug_struct("ConversationFeed")
            .field("workspace", &self.workspace)
            .field(
                "active",
                &state.active.map(|id| id.to_string()).unwrap_or_default(),
            )
            .field("sessions", &state.sessions.len())
            .field("editors", &state.editors.len())
            .finish_non_exhaustive()
    }
}

impl ConversationFeed {
    pub fn new() -> Self {
        Self::with_workspace(PathBuf::new())
    }

    pub fn with_workspace(workspace: PathBuf) -> Self {
        Self {
            workspace,
            inner: Arc::default(),
            events: broadcast::channel(EVENT_CAPACITY).0,
            ready_notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub fn ready_notify(&self) -> Arc<tokio::sync::Notify> {
        self.ready_notify.clone()
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn matches_workspace(&self, target_workspace: &Path) -> bool {
        let feed_key = WorkspaceKey::from_path(&self.workspace);
        if feed_key.is_unspecified() {
            return true;
        }
        feed_key == WorkspaceKey::from_path(target_workspace)
    }

    /// Installs the editor that `reveal` drives (the language server's client).
    pub fn set_editor(&self, editor: EditorReveal) {
        let mut state = self.lock();
        state.legacy_editor = Some(editor);
    }

    /// The connected editor, if any.
    pub fn editor(&self) -> Option<EditorReveal> {
        let state = self.lock();
        if let Ok(selected) = self.select_editor_with_lock(&state, None) {
            return Some(selected.reveal);
        }
        state.legacy_editor.clone()
    }

    /// Resolves an editor according to the unified selection algorithm:
    /// 1. Valid explicit binding for the session and matching incarnation.
    /// 2. Sole eligible registered editor.
    /// 3. Otherwise unavailable or ambiguous error.
    pub fn select_editor(
        &self,
        session_id: Option<Ulid>,
    ) -> Result<SelectedEditor, EditorSelectionError> {
        let state = self.lock();
        self.select_editor_with_lock(&state, session_id)
    }

    fn select_editor_with_lock(
        &self,
        state: &FeedState,
        session_id: Option<Ulid>,
    ) -> Result<SelectedEditor, EditorSelectionError> {
        if let Some(sid) = session_id
            && let Some(&(editor_id, bound_inc)) = state.session_editors.get(&sid)
            && let Some(registered) = state.editors.get(&editor_id)
            && state
                .sessions
                .get(&sid)
                .is_some_and(|s| s.incarnation == bound_inc && s.available && !s.invalidated)
        {
            return Ok(SelectedEditor {
                id: editor_id,
                reveal: registered.reveal.clone(),
                cancel: registered.cancel.clone(),
            });
        }
        if state.editors.len() == 1 {
            let (id, registered) = state.editors.iter().next().unwrap();
            return Ok(SelectedEditor {
                id: *id,
                reveal: registered.reveal.clone(),
                cancel: registered.cancel.clone(),
            });
        }
        if state.editors.is_empty() {
            if let Some(legacy) = &state.legacy_editor {
                return Ok(SelectedEditor {
                    id: EditorId::from_ulid(Ulid::nil()),
                    reveal: legacy.clone(),
                    cancel: CancellationToken::new(),
                });
            }
            return Err(EditorSelectionError::Unavailable);
        }
        Err(EditorSelectionError::Ambiguous)
    }

    /// Registers an editor lifecycle connection with a cancellation token.
    pub fn register_editor(
        &self,
        editor_id: EditorId,
        reveal: EditorReveal,
        cancel: CancellationToken,
    ) {
        let mut state = self.lock();
        state
            .editors
            .insert(editor_id, RegisteredEditor { reveal, cancel });
    }

    /// Registers an editor lifecycle connection and returns an RAII registration guard.
    pub fn register_editor_lifecycle(
        &self,
        editor_id: EditorId,
        reveal: EditorReveal,
        cancel: CancellationToken,
    ) -> EditorRegistration {
        self.register_editor(editor_id, reveal, cancel);
        EditorRegistration::new(self.clone(), editor_id)
    }

    /// Unregisters an editor connection, clearing associated session bindings and cancelling in-flight work.
    pub fn unregister_editor(&self, editor_id: EditorId) {
        let cancel = {
            let mut state = self.lock();
            let entry = state.editors.remove(&editor_id);
            state.session_editors.retain(|_, (id, _)| *id != editor_id);
            entry.map(|e| e.cancel)
        };
        if let Some(token) = cancel {
            token.cancel();
        }
    }

    /// Explicitly binds a session to an editor connection, validating that the editor is registered
    /// and the session is present, available, and not invalidated.
    pub fn bind_editor(&self, session_id: Ulid, editor_id: EditorId) -> Result<(), BindingError> {
        let mut state = self.lock();
        if !state.editors.contains_key(&editor_id) {
            return Err(BindingError::EditorUnavailable);
        }
        let Some(entry) = state.sessions.get(&session_id) else {
            return Err(BindingError::SessionUnavailable);
        };
        if !entry.available || entry.invalidated {
            return Err(BindingError::SessionUnavailable);
        }
        let incarnation = entry.incarnation;
        state.session_editors.retain(|_, (id, _)| *id != editor_id);
        state
            .session_editors
            .insert(session_id, (editor_id, incarnation));
        Ok(())
    }

    /// Explicitly binds a pinned conversation target to an editor connection, validating
    /// workspace match, target availability, matching incarnation, and editor registration.
    pub fn bind_editor_target(
        &self,
        target: &ConversationTarget,
        editor_id: EditorId,
    ) -> Result<(), BindingError> {
        if !self.matches_workspace(&target.workspace) {
            return Err(BindingError::WorkspaceMismatch);
        }
        let mut state = self.lock();
        if !state.editors.contains_key(&editor_id) {
            return Err(BindingError::EditorUnavailable);
        }
        let Some(entry) = state.sessions.get(&target.session_id) else {
            return Err(BindingError::SessionUnavailable);
        };
        if entry.incarnation != target.incarnation || !entry.available || entry.invalidated {
            return Err(BindingError::SessionUnavailable);
        }
        state.session_editors.retain(|_, (id, _)| *id != editor_id);
        state
            .session_editors
            .insert(target.session_id, (editor_id, target.incarnation));
        Ok(())
    }

    /// Clears an explicit session-to-editor binding unconditionally.
    pub fn unbind_editor(&self, session_id: Ulid) {
        let mut state = self.lock();
        state.session_editors.remove(&session_id);
    }

    /// Clears an explicit session-to-editor binding only if it matches the expected incarnation.
    /// Prevents a delayed unbind from an older watcher from removing a newer incarnation's binding.
    pub fn unbind_editor_incarnation(&self, session_id: Ulid, incarnation: Ulid) -> bool {
        let mut state = self.lock();
        if let Some(&(_, bound_inc)) = state.session_editors.get(&session_id)
            && bound_inc == incarnation
        {
            state.session_editors.remove(&session_id);
            return true;
        }
        false
    }

    /// Clears an explicit session-to-editor binding only if currently bound to the given editor.
    pub fn unbind_editor_connection(&self, session_id: Ulid, editor_id: EditorId) -> bool {
        let mut state = self.lock();
        if let Some(&(bound_editor, _)) = state.session_editors.get(&session_id)
            && bound_editor == editor_id
        {
            state.session_editors.remove(&session_id);
            return true;
        }
        false
    }

    /// Returns the explicit editor binding and incarnation for a session, if one exists.
    pub fn bound_editor(&self, session_id: Ulid) -> Option<(EditorId, Ulid)> {
        let state = self.lock();
        state.session_editors.get(&session_id).copied()
    }

    /// A new subscription to the feed's [`ConversationEvent`]s, starting
    /// now. Cheap; any number of subscribers.
    pub fn subscribe(&self) -> broadcast::Receiver<ConversationEvent> {
        self.events.subscribe()
    }

    /// Registers a navigator session with an explicit incarnation.
    /// Resumed or rebuilt sessions start with `available: false` until replay finishes.
    /// Any preexisting explicit editor binding for the session ID is cleared on replacement.
    pub fn register_session_incarnation(
        &self,
        session_id: Ulid,
        incarnation: Ulid,
        post: SessionPost,
        available: bool,
    ) {
        let mut state = self.lock();
        state.session_editors.remove(&session_id);
        state.sessions.insert(
            session_id,
            SessionConversation {
                flow_post: None,
                post,
                waiting_on_captain: false,
                queued_wake: None,
                exchanges: VecDeque::new(),
                captain_pending: false,
                incarnation,
                available,
                invalidated: false,
                has_captain_turns: false,
            },
        );
        if available {
            drop(state);
            self.ready_notify.notify_waiters();
        }
    }

    /// Registers a navigator session as immediately available (convenience helper).
    pub fn register_session(&self, session_id: Ulid, post: SessionPost) -> Ulid {
        let incarnation = Ulid::generate();
        self.register_session_incarnation(session_id, incarnation, post, true);
        incarnation
    }

    /// Marks a session incarnation ready/available once its replay and live receiver are established.
    /// If there is no live active session and this session has captain messages, it reaches active
    /// without stealing any newer live selection.
    pub fn mark_incarnation_ready(&self, session_id: Ulid, incarnation: Ulid) -> bool {
        let mut state = self.lock();
        if let Some(entry) = state.sessions.get_mut(&session_id)
            && entry.incarnation == incarnation
            && !entry.invalidated
        {
            entry.available = true;
            let has_turns = entry.has_captain_turns;
            if state.active.is_none() && has_turns {
                state.active = Some(session_id);
            }
            drop(state);
            self.ready_notify.notify_waiters();
            return true;
        }
        false
    }

    pub fn register_flow_poster_incarnation(
        &self,
        session_id: Ulid,
        incarnation: Ulid,
        post: FlowPost,
    ) {
        let mut state = self.lock();
        if let Some(entry) = state.sessions.get_mut(&session_id)
            && entry.incarnation == incarnation
        {
            entry.flow_post = Some(post);
        }
    }

    pub fn register_flow_poster(&self, session_id: Ulid, post: FlowPost) {
        let mut state = self.lock();
        if let Some(entry) = state.sessions.get_mut(&session_id) {
            entry.flow_post = Some(post);
        }
    }

    /// Drops a session if the current entry matches the expected incarnation.
    pub fn forget_session_incarnation(&self, session_id: Ulid, incarnation: Ulid) -> bool {
        let mut state = self.lock();
        if let Some(entry) = state.sessions.get(&session_id)
            && entry.incarnation == incarnation
        {
            state.sessions.remove(&session_id);
            if let Some(&(_, bound_inc)) = state.session_editors.get(&session_id)
                && bound_inc == incarnation
            {
                state.session_editors.remove(&session_id);
            }
            if state.active == Some(session_id) {
                state.active = None;
            }
            return true;
        }
        false
    }

    /// Drops a session unconditionally; its queued wake, if any, goes with it.
    pub fn forget_session(&self, session_id: Ulid) {
        let mut state = self.lock();
        state.sessions.remove(&session_id);
        state.session_editors.remove(&session_id);
        if state.active == Some(session_id) {
            state.active = None;
        }
    }

    /// Invalidates a session incarnation (e.g. after a lagged subscription, resync, or disconnect).
    /// Drops queued wakes and marks the entry unavailable without removing transcript history.
    pub fn invalidate_session_incarnation(&self, session_id: Ulid, incarnation: Ulid) -> bool {
        let mut state = self.lock();
        if let Some(entry) = state.sessions.get_mut(&session_id)
            && entry.incarnation == incarnation
        {
            entry.available = false;
            entry.invalidated = true;
            entry.queued_wake = None;
            if state.active == Some(session_id) {
                state.active = None;
            }
            if let Some(&(_, bound_inc)) = state.session_editors.get(&session_id)
                && bound_inc == incarnation
            {
                state.session_editors.remove(&session_id);
            }
            return true;
        }
        false
    }

    /// Checks if a session incarnation is currently registered, available, and not invalidated.
    pub fn is_incarnation_available(&self, session_id: Ulid, incarnation: Ulid) -> bool {
        let state = self.lock();
        state.sessions.get(&session_id).is_some_and(|entry| {
            entry.incarnation == incarnation && entry.available && !entry.invalidated
        })
    }

    /// Atomically captures active target, digest, and availability.
    pub fn snapshot(&self) -> DigestSnapshot {
        let state = self.lock();
        match state
            .active
            .and_then(|id| state.sessions.get(&id).map(|e| (id, e)))
        {
            Some((id, entry)) if entry.available && !entry.invalidated => {
                let target = ConversationTarget {
                    workspace: self.workspace.clone(),
                    session_id: id,
                    incarnation: entry.incarnation,
                };
                let mut out = String::new();
                for (index, exchange) in entry.exchanges.iter().enumerate() {
                    if index > 0 {
                        out.push('\n');
                    }
                    let _ = write!(out, "{}: {}", exchange.author.as_str(), exchange.text);
                }
                DigestSnapshot {
                    target: Some(target),
                    digest: out,
                    waiting_on_captain: entry.waiting_on_captain,
                    available: true,
                }
            }
            _ => DigestSnapshot {
                target: None,
                digest: String::new(),
                waiting_on_captain: false,
                available: false,
            },
        }
    }

    /// Atomically captures digest, target, and availability for a specific pinned target.
    /// Validates workspace match, matching incarnation, availability, and non-invalidated state.
    pub fn snapshot_target(&self, target: &ConversationTarget) -> DigestSnapshot {
        if !self.matches_workspace(&target.workspace) {
            return DigestSnapshot {
                target: None,
                digest: String::new(),
                waiting_on_captain: false,
                available: false,
            };
        }
        let state = self.lock();
        match state.sessions.get(&target.session_id) {
            Some(entry)
                if entry.incarnation == target.incarnation
                    && entry.available
                    && !entry.invalidated =>
            {
                let mut out = String::new();
                for (index, exchange) in entry.exchanges.iter().enumerate() {
                    if index > 0 {
                        out.push('\n');
                    }
                    let _ = write!(out, "{}: {}", exchange.author.as_str(), exchange.text);
                }
                DigestSnapshot {
                    target: Some(target.clone()),
                    digest: out,
                    waiting_on_captain: entry.waiting_on_captain,
                    available: true,
                }
            }
            _ => DigestSnapshot {
                target: None,
                digest: String::new(),
                waiting_on_captain: false,
                available: false,
            },
        }
    }

    /// Atomically captures digest, target, and availability scoped to a specific editor connection.
    /// Selection policy:
    /// 1. If sessions are explicitly bound to this editor:
    ///    - If exactly one session is bound (sole bound session), snapshots it.
    ///    - If multiple sessions are bound, snapshots the active session only if it is one of the
    ///      bound sessions; otherwise returns unavailable on ambiguity (never picks map iteration order).
    /// 2. If no session is bound to this editor, and this editor is the sole registered connection,
    ///    snapshots the global active conversation.
    /// 3. Returns unavailable on ambiguity or when unroutable.
    pub fn snapshot_for_editor(&self, editor_id: EditorId) -> DigestSnapshot {
        let state = self.lock();
        if !state.editors.contains_key(&editor_id) {
            return DigestSnapshot {
                target: None,
                digest: String::new(),
                waiting_on_captain: false,
                available: false,
            };
        }

        // Collect all sessions explicitly bound to this editor with matching and valid incarnations.
        let mut bound_sessions = Vec::new();
        for (sid, (bound_editor, bound_inc)) in &state.session_editors {
            if *bound_editor == editor_id
                && let Some(entry) = state.sessions.get(sid)
                && entry.incarnation == *bound_inc
                && entry.available
                && !entry.invalidated
            {
                bound_sessions.push((*sid, entry));
            }
        }

        let selected_entry = match bound_sessions.len() {
            0 => None,
            1 => Some(bound_sessions[0]),
            _ => {
                // Multiple sessions bound to this editor:
                // Choose the bound ACTIVE session only if it is associated with this editor;
                // never pick non-deterministically by map iteration order.
                if let Some(active_id) = state.active {
                    bound_sessions
                        .into_iter()
                        .find(|(sid, _)| *sid == active_id)
                } else {
                    None
                }
            }
        };

        if let Some((sid, entry)) = selected_entry {
            let target = ConversationTarget {
                workspace: self.workspace.clone(),
                session_id: sid,
                incarnation: entry.incarnation,
            };
            let mut out = String::new();
            for (index, exchange) in entry.exchanges.iter().enumerate() {
                if index > 0 {
                    out.push('\n');
                }
                let _ = write!(out, "{}: {}", exchange.author.as_str(), exchange.text);
            }
            return DigestSnapshot {
                target: Some(target),
                digest: out,
                waiting_on_captain: entry.waiting_on_captain,
                available: true,
            };
        }

        // Fallback: If no sessions are bound to this editor and this is the sole registered editor,
        // snapshot the global active conversation.
        if state.editors.len() == 1
            && state.session_editors.values().all(|(e, _)| *e != editor_id)
            && let Some((id, entry)) = state
                .active
                .and_then(|id| state.sessions.get(&id).map(|e| (id, e)))
            && entry.available
            && !entry.invalidated
        {
            let target = ConversationTarget {
                workspace: self.workspace.clone(),
                session_id: id,
                incarnation: entry.incarnation,
            };
            let mut out = String::new();
            for (index, exchange) in entry.exchanges.iter().enumerate() {
                if index > 0 {
                    out.push('\n');
                }
                let _ = write!(out, "{}: {}", exchange.author.as_str(), exchange.text);
            }
            return DigestSnapshot {
                target: Some(target),
                digest: out,
                waiting_on_captain: entry.waiting_on_captain,
                available: true,
            };
        }

        DigestSnapshot {
            target: None,
            digest: String::new(),
            waiting_on_captain: false,
            available: false,
        }
    }

    /// Posts a flow note pinned to a specific conversation target, enforcing workspace equality.
    pub fn post_targeted_flow_note(
        &self,
        target: &ConversationTarget,
        text: String,
        flow: String,
        run_id: String,
        wake: bool,
    ) -> Result<(), String> {
        if !self.matches_workspace(&target.workspace) {
            return Err(
                "workspace mismatch: target workspace does not match feed workspace".to_string(),
            );
        }
        let post = {
            let state = self.lock();
            let Some(entry) = state.sessions.get(&target.session_id) else {
                return Err("session not found".to_string());
            };
            if entry.incarnation != target.incarnation || !entry.available || entry.invalidated {
                return Err("session incarnation is unavailable or superseded".to_string());
            }
            entry.flow_post.clone()
        }
        .ok_or_else(|| "no flow poster for session".to_string())?;
        post(text, flow, run_id, wake)
    }

    #[allow(dead_code)]
    pub(crate) fn post_flow_note(
        &self,
        text: String,
        flow: String,
        run_id: String,
        wake: bool,
    ) -> Result<(), String> {
        let post = {
            let state = self.lock();
            state
                .active
                .and_then(|id| state.sessions.get(&id))
                .filter(|entry| entry.available && !entry.invalidated)
                .and_then(|entry| entry.flow_post.clone())
        }
        .ok_or_else(|| "no active flow session".to_string())?;
        post(text, flow, run_id, wake)
    }

    /// Feeds one durable session event into the hub.
    pub async fn observe(&self, envelope: &EventEnvelope) {
        self.apply(envelope, true, None).await;
    }

    /// Feeds one durable session event with an expected session incarnation.
    pub async fn observe_incarnation(&self, incarnation: Ulid, envelope: &EventEnvelope) {
        self.apply(envelope, true, Some(incarnation)).await;
    }

    /// Feeds one event read back from storage.
    pub async fn observe_replayed(&self, envelope: &EventEnvelope) {
        self.apply(envelope, false, None).await;
    }

    /// Feeds one event read back from storage with an expected session incarnation.
    pub async fn observe_replayed_incarnation(&self, incarnation: Ulid, envelope: &EventEnvelope) {
        self.apply(envelope, false, Some(incarnation)).await;
    }

    async fn apply(
        &self,
        envelope: &EventEnvelope,
        announce: bool,
        expected_incarnation: Option<Ulid>,
    ) {
        {
            let state = self.lock();
            let Some(entry) = state.sessions.get(&envelope.session_id) else {
                return;
            };
            if let Some(inc) = expected_incarnation
                && entry.incarnation != inc
            {
                return;
            }
            if entry.invalidated {
                return;
            }
            if announce && !entry.available {
                return;
            }
        }
        match envelope.kind.as_str() {
            "message.received" => {
                self.observe_message(envelope, announce, expected_incarnation)
                    .await
            }
            "turn.end" => {
                let (caused_by_captain, target_editor, entry_incarnation) = {
                    let mut state = self.lock();
                    let Some(entry) = state.sessions.get_mut(&envelope.session_id) else {
                        return;
                    };
                    if let Some(inc) = expected_incarnation
                        && entry.incarnation != inc
                    {
                        return;
                    }
                    if entry.invalidated || (announce && !entry.available) {
                        return;
                    }
                    let caused = std::mem::take(&mut entry.captain_pending);
                    let entry_incarnation = entry.incarnation;
                    let target_editor = self.resolve_target_editor(&state, envelope.session_id);
                    (caused, target_editor, entry_incarnation)
                };
                if announce {
                    let _ = self.events.send(ConversationEvent::TurnEnded {
                        session_id: envelope.session_id,
                        incarnation: entry_incarnation,
                        caused_by_captain,
                        target_editor,
                    });
                }
            }
            "llm.response" => {
                let text = envelope
                    .payload
                    .get("message")
                    .and_then(|m| m.get("text"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                if text.trim().is_empty() {
                    return;
                }
                let mut state = self.lock();
                if let Some(entry) = state.sessions.get_mut(&envelope.session_id) {
                    if let Some(inc) = expected_incarnation
                        && entry.incarnation != inc
                    {
                        return;
                    }
                    if entry.invalidated || (announce && !entry.available) {
                        return;
                    }
                    entry.push_exchange(Author::Cognition, text);
                }
            }
            "agent.yield" => {
                let mut state = self.lock();
                if let Some(entry) = state.sessions.get_mut(&envelope.session_id) {
                    if let Some(inc) = expected_incarnation
                        && entry.incarnation != inc
                    {
                        return;
                    }
                    if entry.invalidated || (announce && !entry.available) {
                        return;
                    }
                    entry.waiting_on_captain = true;
                }
            }
            "command.result"
                if envelope.payload["name"] == "watchdog"
                    && envelope.payload["data"]["resolution"].is_string() =>
            {
                let mut state = self.lock();
                if let Some(entry) = state.sessions.get_mut(&envelope.session_id) {
                    if let Some(inc) = expected_incarnation
                        && entry.incarnation != inc
                    {
                        return;
                    }
                    if entry.invalidated || (announce && !entry.available) {
                        return;
                    }
                    entry.waiting_on_captain = false;
                }
            }
            _ => {}
        }
    }

    fn resolve_target_editor(&self, state: &FeedState, session_id: Ulid) -> Option<EditorId> {
        if let Some(&(editor_id, bound_inc)) = state.session_editors.get(&session_id)
            && state.editors.contains_key(&editor_id)
            && state
                .sessions
                .get(&session_id)
                .is_some_and(|s| s.incarnation == bound_inc && s.available && !s.invalidated)
        {
            return Some(editor_id);
        }
        if state.editors.len() == 1 {
            return state.editors.keys().next().copied();
        }
        None
    }

    async fn observe_message(
        &self,
        envelope: &EventEnvelope,
        announce: bool,
        expected_incarnation: Option<Ulid>,
    ) {
        let source = envelope
            .payload
            .get("source")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let content = envelope
            .payload
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if content.trim().is_empty() {
            return;
        }
        let captain = crate::bus::command::is_captain_source(source);
        let flush = {
            let mut state = self.lock();
            let Some(entry) = state.sessions.get_mut(&envelope.session_id) else {
                return;
            };
            if let Some(inc) = expected_incarnation
                && entry.incarnation != inc
            {
                return;
            }
            if entry.invalidated {
                return;
            }
            if announce && !entry.available {
                return;
            }
            if captain {
                entry.waiting_on_captain = false;
                entry.captain_pending = true;
                entry.has_captain_turns = true;
            }
            entry.push_exchange(
                if captain {
                    Author::Captain
                } else if matches!(source, "subagent" | "user:subagent") {
                    Author::Subagent
                } else {
                    Author::Navigator
                },
                content,
            );
            // Replay events must not flush queued wakes:
            let queued = if captain && announce {
                entry.queued_wake.take()
            } else {
                None
            };
            let post = queued.map(|text| (entry.post.clone(), text));
            // Rebuilding replay must NOT choose active:
            if captain && announce {
                state.active = Some(envelope.session_id);
            }
            post
        };
        if let Some((post, text)) = flush
            && let Err(error) = post(text).await
        {
            tracing::warn!(
                session = %envelope.session_id,
                error = %error,
                "queued wake delivery failed"
            );
        }
    }

    /// The active conversation's session id, if any captain message has
    /// reached a navigator session in this process.
    pub fn active_session(&self) -> Option<Ulid> {
        let state = self.lock();
        state.active.filter(|id| {
            state
                .sessions
                .get(id)
                .is_some_and(|e| e.available && !e.invalidated)
        })
    }

    /// Whether the active conversation's cognition is waiting on an `ask`
    /// the Captain has not answered yet. False when there is no active
    /// conversation.
    pub fn is_waiting_on_captain(&self) -> bool {
        let state = self.lock();
        state
            .active
            .and_then(|id| state.sessions.get(&id))
            .is_some_and(|entry| entry.available && !entry.invalidated && entry.waiting_on_captain)
    }

    /// The active conversation's digest: plain text lines, newest last,
    /// `author: text`. Empty when there is no active conversation.
    pub fn digest(&self) -> String {
        let state = self.lock();
        match state.active.and_then(|id| state.sessions.get(&id)) {
            Some(entry) if entry.available && !entry.invalidated => {
                let mut out = String::new();
                for (index, exchange) in entry.exchanges.iter().enumerate() {
                    if index > 0 {
                        out.push('\n');
                    }
                    let _ = write!(out, "{}: {}", exchange.author.as_str(), exchange.text);
                }
                out
            }
            _ => String::new(),
        }
    }

    /// Posts a wake to a pinned conversation target, validating workspace equality.
    pub async fn post_targeted_wake(&self, target: &ConversationTarget, text: &str) -> WakeOutcome {
        if !self.matches_workspace(&target.workspace) {
            return WakeOutcome::Failed(
                target.session_id,
                "workspace mismatch: target workspace does not match feed workspace".to_string(),
            );
        }
        let (session_id, post) = {
            let mut state = self.lock();
            let Some(entry) = state.sessions.get_mut(&target.session_id) else {
                return WakeOutcome::NoSession;
            };
            if entry.incarnation != target.incarnation || !entry.available || entry.invalidated {
                return WakeOutcome::Failed(
                    target.session_id,
                    "session incarnation is unavailable or superseded".to_string(),
                );
            }
            if entry.waiting_on_captain {
                entry.queued_wake = Some(text.to_string());
                return WakeOutcome::Queued(target.session_id);
            }
            (target.session_id, entry.post.clone())
        };
        match post(text.to_string()).await {
            Ok(()) => WakeOutcome::Posted(session_id),
            Err(error) => WakeOutcome::Failed(session_id, error),
        }
    }

    /// Posts a wake to the active conversation as a navigator-sourced
    /// message. The ask guard: while the cognition waits on the Captain, the
    /// wake is queued instead (coalesced, latest wins) and delivered by the
    /// next captain message.
    pub async fn post_wake(&self, text: &str) -> WakeOutcome {
        let (session_id, post) = {
            let mut state = self.lock();
            let Some(session_id) = state.active else {
                return WakeOutcome::NoSession;
            };
            let Some(entry) = state.sessions.get_mut(&session_id) else {
                state.active = None;
                return WakeOutcome::NoSession;
            };
            if !entry.available || entry.invalidated {
                state.active = None;
                return WakeOutcome::NoSession;
            }
            if entry.waiting_on_captain {
                entry.queued_wake = Some(text.to_string());
                return WakeOutcome::Queued(session_id);
            }
            (session_id, entry.post.clone())
        };
        match post(text.to_string()).await {
            Ok(()) => WakeOutcome::Posted(session_id),
            Err(error) => WakeOutcome::Failed(session_id, error),
        }
    }

    /// Poison recovery via into_inner: a panicking holder must not wedge
    /// the feed (same policy as the session registry).
    fn lock(&self) -> MutexGuard<'_, FeedState> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// Removes the `<editor-context>` block the editor appends to captain
/// messages: it describes the editor state at send time, not what the
/// captain said.
fn strip_editor_context(text: &str) -> String {
    const OPEN: &str = "<editor-context>";
    const CLOSE: &str = "</editor-context>";
    let Some(open) = text.find(OPEN) else {
        return text.to_string();
    };
    let after = &text[open + OPEN.len()..];
    let cut = match after.find(CLOSE) {
        Some(end) => open + OPEN.len() + end + CLOSE.len(),
        None => text.len(),
    };
    let mut out = String::with_capacity(open + text.len() - cut);
    let mut newlines = 0;
    for c in text[..open].chars().chain(text[cut..].chars()) {
        if c == '\n' {
            if newlines == 2 {
                continue;
            }
            newlines += 1;
        } else {
            newlines = 0;
        }
        out.push(c);
    }
    out.truncate(out.trim_end().len());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::event::EventKind;
    use serde_json::json;

    fn message_envelope(session: Ulid, source: &str, content: &str) -> EventEnvelope {
        EventEnvelope::new(
            session,
            0,
            EventKind::MessageReceived,
            json!({
                "command_id": Ulid::generate().to_string(),
                "source": source,
                "target": "agent:main",
                "mode": "next-turn",
                "content": content,
            }),
        )
    }

    fn response_envelope(session: Ulid, text: &str) -> EventEnvelope {
        EventEnvelope::new(
            session,
            0,
            EventKind::LlmResponse,
            json!({ "turn_id": Ulid::generate().to_string(), "step_id": Ulid::generate().to_string(), "message": { "text": text } }),
        )
    }

    fn yield_envelope(session: Ulid) -> EventEnvelope {
        EventEnvelope::new(
            session,
            0,
            EventKind::AgentYield,
            json!({ "actor_id": "agent:main", "turn_id": Ulid::generate().to_string(), "question": "which one?" }),
        )
    }

    fn turn_end_envelope(session: Ulid) -> EventEnvelope {
        EventEnvelope::new(
            session,
            0,
            EventKind::TurnEnd,
            json!({ "turn_id": Ulid::generate().to_string(), "actor_id": "agent:main", "outcome": "done", "duration_ms": 1 }),
        )
    }

    fn recording_poster(sink: Arc<Mutex<Vec<String>>>) -> SessionPost {
        Arc::new(move |text| {
            sink.lock().unwrap().push(text);
            Box::pin(async { Ok::<(), String>(()) })
        })
    }

    #[tokio::test]
    async fn legacy_human_event_is_read_as_captain() {
        let feed = ConversationFeed::new();
        let id = Ulid::generate();
        feed.register_session(id, recording_poster(Arc::new(Mutex::new(Vec::new()))));
        feed.observe(&message_envelope(id, "driver", "legacy request"))
            .await;
        assert!(feed.digest().contains("captain: legacy request"));
    }

    #[test]
    fn editor_context_block_is_stripped_from_captain_text() {
        let raw = "add the command\n\n<editor-context>\nfocused: src/main.rs\nopen files (1): src/main.rs\n</editor-context>";
        assert_eq!(
            strip_editor_context(raw),
            "add the command",
            "the block and its padding disappear"
        );
        assert_eq!(strip_editor_context("no block here"), "no block here");
        assert_eq!(
            strip_editor_context("no block\n\n\nhere"),
            "no block\n\n\nhere"
        );
        for before in 0..8 {
            for after in 0..8 {
                let prefix = "\n".repeat(before);
                let suffix = "\n".repeat(after);
                let raw = format!("hé{prefix}<editor-context>x</editor-context>{suffix}世界  ");
                let mut expected = format!("hé{prefix}{suffix}世界  ");
                while expected.contains("\n\n\n") {
                    expected = expected.replace("\n\n\n", "\n\n");
                }
                assert_eq!(strip_editor_context(&raw), expected.trim_end());
            }
        }
        assert_eq!(
            strip_editor_context("cut <editor-context> unterminated"),
            "cut"
        );
    }

    #[tokio::test]
    async fn digest_records_authors_newest_last_and_strips_the_block() {
        let feed = ConversationFeed::new();
        let id = Ulid::generate();
        let posted = Arc::new(Mutex::new(Vec::new()));
        feed.register_session(id, recording_poster(posted.clone()));

        feed.observe(&message_envelope(
            id,
            "user:http",
            "add the command\n\n<editor-context>\nfocused: x\n</editor-context>",
        ))
        .await;
        feed.observe(&response_envelope(id, "start in src/cli.rs"))
            .await;
        feed.observe(&message_envelope(id, NAVIGATOR_ACTOR_ID, "wake: drift"))
            .await;

        assert_eq!(feed.active_session(), Some(id));
        let digest = feed.digest();
        assert_eq!(
            digest,
            "captain: add the command\ncognition: start in src/cli.rs\nnavigator: wake: drift",
            "plain lines, newest last, block stripped"
        );
    }

    #[tokio::test]
    async fn digest_is_bounded_to_the_last_twelve_exchanges() {
        let feed = ConversationFeed::new();
        let id = Ulid::generate();
        let posted = Arc::new(Mutex::new(Vec::new()));
        feed.register_session(id, recording_poster(posted));
        for i in 0..20 {
            feed.observe(&message_envelope(id, "user:http", &format!("m{i}")))
                .await;
        }
        let digest = feed.digest();
        let lines: Vec<&str> = digest.lines().collect();
        assert_eq!(lines.len(), MAX_EXCHANGES, "only the last twelve remain");
        assert_eq!(lines.first().unwrap(), &"captain: m8");
        assert_eq!(lines.last().unwrap(), &"captain: m19");
    }

    #[tokio::test]
    async fn long_texts_are_cut_near_six_hundred_characters() {
        let feed = ConversationFeed::new();
        let id = Ulid::generate();
        let posted = Arc::new(Mutex::new(Vec::new()));
        feed.register_session(id, recording_poster(posted));
        let long = "x".repeat(5_000);
        feed.observe(&message_envelope(id, "user:http", &long))
            .await;
        let line = feed.digest();
        assert!(line.chars().count() < MAX_EXCHANGE_CHARS + 16);
        assert!(line.ends_with('…'), "the cut is marked");
    }

    #[tokio::test]
    async fn active_conversation_switches_on_a_captain_message_to_another_session() {
        let feed = ConversationFeed::new();
        let a = Ulid::generate();
        let b = Ulid::generate();
        let posted = Arc::new(Mutex::new(Vec::new()));
        feed.register_session(a, recording_poster(posted.clone()));
        feed.register_session(b, recording_poster(posted));

        feed.observe(&message_envelope(a, "user:http", "first conversation"))
            .await;
        assert_eq!(feed.active_session(), Some(a));
        // A navigator-sourced message does not steal the active spot.
        feed.observe(&message_envelope(b, NAVIGATOR_ACTOR_ID, "wake"))
            .await;
        assert_eq!(feed.active_session(), Some(a));

        feed.observe(&message_envelope(b, "user:http", "second conversation"))
            .await;
        assert_eq!(feed.active_session(), Some(b));
        assert_eq!(
            feed.digest(),
            "navigator: wake\ncaptain: second conversation"
        );
    }

    #[tokio::test]
    async fn queued_wakes_coalesce_and_deliver_after_the_captain_answers() {
        let feed = ConversationFeed::new();
        let id = Ulid::generate();
        let posted = Arc::new(Mutex::new(Vec::new()));
        feed.register_session(id, recording_poster(posted.clone()));

        feed.observe(&message_envelope(id, "user:http", "add a command"))
            .await;
        assert!(!feed.is_waiting_on_captain());

        feed.observe(&yield_envelope(id)).await;
        assert!(feed.is_waiting_on_captain(), "agent.yield starts the wait");

        assert_eq!(
            feed.post_wake("first wake").await,
            WakeOutcome::Queued(id),
            "a wake while waiting is queued, not posted"
        );
        assert_eq!(feed.post_wake("latest wake").await, WakeOutcome::Queued(id));
        assert!(posted.lock().unwrap().is_empty(), "nothing posted yet");

        feed.observe(&message_envelope(id, "user:http", "the auth remove one"))
            .await;
        assert!(!feed.is_waiting_on_captain());
        assert_eq!(
            posted.lock().unwrap().clone(),
            vec!["latest wake".to_string()],
            "coalesced: latest wins, delivered once"
        );
        feed.observe(&message_envelope(id, NAVIGATOR_ACTOR_ID, "latest wake"))
            .await;
        let digest = feed.digest();
        assert!(digest.contains("navigator: latest wake"));
    }

    #[tokio::test]
    async fn watchdog_resolution_clears_waiting_on_captain() {
        let feed = ConversationFeed::new();
        let id = Ulid::generate();
        feed.register_session(id, recording_poster(Arc::new(Mutex::new(Vec::new()))));
        feed.observe(&message_envelope(id, "user:http", "work"))
            .await;
        feed.observe(&yield_envelope(id)).await;
        assert!(feed.is_waiting_on_captain());
        let mut resolved = yield_envelope(id);
        resolved.kind = "command.result".into();
        resolved.payload = serde_json::json!({"name":"watchdog","data":{"resolution":"continued"}});
        feed.observe(&resolved).await;
        assert!(!feed.is_waiting_on_captain());
    }

    #[tokio::test]
    async fn paused_turn_supersession_keeps_replacement_captain_causality() {
        let feed = ConversationFeed::new();
        let id = Ulid::generate();
        feed.register_session(id, recording_poster(Arc::new(Mutex::new(Vec::new()))));
        let mut events = feed.subscribe();
        feed.observe(&message_envelope(id, "user:http", "original"))
            .await;
        feed.observe(&yield_envelope(id)).await;
        feed.observe(&turn_end_envelope(id)).await;
        assert!(matches!(
            events.try_recv().unwrap(),
            ConversationEvent::TurnEnded {
                caused_by_captain: true,
                ..
            }
        ));
        feed.observe(&message_envelope(id, "user:subagent", "not captain"))
            .await;
        feed.observe(&message_envelope(id, "user:cli", "replacement"))
            .await;
        let mut resolution = yield_envelope(id);
        resolution.kind = "command.result".into();
        resolution.payload = json!({"name":"watchdog","data":{"resolution":"superseded","turn_id":Ulid::generate()}});
        feed.observe(&resolution).await;
        assert!(
            events.try_recv().is_err(),
            "resolution never ends the old turn again"
        );
        feed.observe(&turn_end_envelope(id)).await;
        assert!(matches!(
            events.try_recv().unwrap(),
            ConversationEvent::TurnEnded {
                caused_by_captain: true,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_wake_without_wait_or_session_posts_or_reports_none() {
        let feed = ConversationFeed::new();
        assert_eq!(
            feed.post_wake("anyone?").await,
            WakeOutcome::NoSession,
            "no conversation at all"
        );

        let id = Ulid::generate();
        let posted = Arc::new(Mutex::new(Vec::new()));
        feed.register_session(id, recording_poster(posted.clone()));
        feed.observe(&message_envelope(id, "user:http", "hello"))
            .await;
        assert_eq!(feed.post_wake("not waiting").await, WakeOutcome::Posted(id));
        assert_eq!(posted.lock().unwrap().len(), 1);

        feed.forget_session(id);
        assert_eq!(feed.active_session(), None);
        assert_eq!(feed.digest(), "");
        assert_eq!(feed.post_wake("gone").await, WakeOutcome::NoSession);
    }

    #[tokio::test]
    async fn turn_ended_tells_whether_the_captain_caused_it() {
        let feed = ConversationFeed::new();
        let id = Ulid::generate();
        let posted = Arc::new(Mutex::new(Vec::new()));
        let inc = feed.register_session(id, recording_poster(posted));
        let mut events = feed.subscribe();

        feed.observe(&message_envelope(id, "user:http", "add a command"))
            .await;
        feed.observe(&turn_end_envelope(id)).await;
        assert_eq!(
            events.try_recv().unwrap(),
            ConversationEvent::TurnEnded {
                session_id: id,
                incarnation: inc,
                caused_by_captain: true,
                target_editor: None,
            },
            "a captain message's turn"
        );

        feed.observe(&message_envelope(id, NAVIGATOR_ACTOR_ID, "wake: drift"))
            .await;
        feed.observe(&turn_end_envelope(id)).await;
        assert_eq!(
            events.try_recv().unwrap(),
            ConversationEvent::TurnEnded {
                session_id: id,
                incarnation: inc,
                caused_by_captain: false,
                target_editor: None,
            },
            "a navigator wake's turn"
        );
        assert!(events.try_recv().is_err(), "one event per turn end");
    }

    #[tokio::test]
    async fn replayed_turn_ends_and_unknown_sessions_announce_nothing() {
        let feed = ConversationFeed::new();
        let id = Ulid::generate();
        let posted = Arc::new(Mutex::new(Vec::new()));
        let inc = feed.register_session(id, recording_poster(posted));
        let mut events = feed.subscribe();

        feed.observe_replayed(&message_envelope(id, "user:http", "old chat"))
            .await;
        feed.observe_replayed(&turn_end_envelope(id)).await;
        feed.observe(&turn_end_envelope(Ulid::generate())).await;
        assert!(events.try_recv().is_err());
        feed.mark_incarnation_ready(id, inc);
        assert!(feed.digest().contains("captain: old chat"), "state rebuilt");
    }

    #[tokio::test]
    async fn unknown_sessions_and_empty_kinds_are_ignored() {
        let feed = ConversationFeed::new();
        feed.observe(&message_envelope(Ulid::generate(), "user:http", "hello"))
            .await;
        feed.observe(&yield_envelope(Ulid::generate())).await;
        assert_eq!(feed.active_session(), None);
        assert!(!feed.is_waiting_on_captain());
        assert_eq!(feed.digest(), "");
    }

    #[tokio::test]
    async fn child_events_do_not_replace_root_conversation_or_answer_its_ask() {
        let feed = ConversationFeed::new();
        let root = Ulid::generate();
        let child = Ulid::generate();
        feed.register_session(root, recording_poster(Arc::new(Mutex::new(Vec::new()))));
        feed.observe(&message_envelope(root, "user:http", "root request"))
            .await;
        feed.observe(&yield_envelope(root)).await;
        feed.observe(&message_envelope(child, "user:http", "child request"))
            .await;
        feed.observe(&response_envelope(child, "raw child transcript"))
            .await;
        feed.observe(&turn_end_envelope(child)).await;
        feed.observe(&message_envelope(root, "subagent", "bounded child result"))
            .await;
        assert_eq!(feed.active_session(), Some(root));
        assert!(feed.is_waiting_on_captain());
        assert_eq!(
            feed.digest(),
            "captain: root request\nsubagent (untrusted): bounded child result"
        );
    }

    #[test]
    fn workspace_key_isolates_relative_paths_from_cwd_and_distinguishes_missing() {
        let temp = tempfile::tempdir().unwrap();
        let canonical_dir = temp.path().join("existing");
        std::fs::create_dir_all(&canonical_dir).unwrap();
        let real_canonical = std::fs::canonicalize(&canonical_dir).unwrap();

        // Existing absolute path resolves to Canonical
        assert_eq!(
            WorkspaceKey::from_path(&canonical_dir),
            WorkspaceKey::Canonical(real_canonical)
        );

        // Missing absolute path resolves to MissingAbsolute without cwd resolution
        let missing = Path::new("/nonexistent/historical/project/path");
        assert_eq!(
            WorkspaceKey::from_path(missing),
            WorkspaceKey::MissingAbsolute(missing.to_path_buf())
        );

        // Relative path resolves to Relative without invoking canonicalize or resolving relative to cwd
        let relative = Path::new("Cargo.toml");
        assert_eq!(
            WorkspaceKey::from_path(relative),
            WorkspaceKey::Relative(relative.to_path_buf())
        );

        // Empty path resolves to Unspecified
        assert_eq!(
            WorkspaceKey::from_path(Path::new("")),
            WorkspaceKey::Unspecified
        );
    }

    #[tokio::test]
    async fn replay_does_not_flush_queued_wakes_or_steal_active_selection() {
        let feed = ConversationFeed::new();
        let live_session = Ulid::generate();
        let replay_session = Ulid::generate();
        let posted = Arc::new(Mutex::new(Vec::new()));
        feed.register_session(live_session, recording_poster(posted.clone()));
        feed.register_session(replay_session, recording_poster(posted.clone()));

        // Establish live active conversation and queue a wake behind an ask
        feed.observe(&message_envelope(live_session, "user:http", "live turn"))
            .await;
        feed.observe(&yield_envelope(live_session)).await;
        assert_eq!(
            feed.post_wake("queued wake").await,
            WakeOutcome::Queued(live_session)
        );
        assert_eq!(feed.active_session(), Some(live_session));

        // Replaying messages for another session must not steal the active session or flush wakes
        feed.observe_replayed(&message_envelope(replay_session, "user:http", "past turn"))
            .await;
        assert_eq!(feed.active_session(), Some(live_session));
        assert!(posted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn rebuilding_replay_does_not_choose_active_until_ready_and_preserves_live() {
        let feed = ConversationFeed::new();
        let live_id = Ulid::generate();
        let live_inc = Ulid::generate();
        let resumed_id = Ulid::generate();
        let resumed_inc = Ulid::generate();

        // 1. Live session receives a message and becomes active
        feed.register_session_incarnation(
            live_id,
            live_inc,
            recording_poster(Arc::new(Mutex::new(Vec::new()))),
            true,
        );
        feed.observe_incarnation(
            live_inc,
            &message_envelope(live_id, "user:http", "live captain"),
        )
        .await;
        assert_eq!(feed.active_session(), Some(live_id));

        // 2. Resumed session starts in rebuilding state (available = false)
        feed.register_session_incarnation(
            resumed_id,
            resumed_inc,
            recording_poster(Arc::new(Mutex::new(Vec::new()))),
            false,
        );

        // 3. Replay occurs for resumed session: rebuilding replay MUST NOT steal active
        feed.observe_replayed_incarnation(
            resumed_inc,
            &message_envelope(resumed_id, "user:http", "historic captain message"),
        )
        .await;
        assert_eq!(
            feed.active_session(),
            Some(live_id),
            "rebuilding replay must not steal active"
        );

        // 4. Marking resumed session ready must not steal active while a live selection exists
        feed.mark_incarnation_ready(resumed_id, resumed_inc);
        assert_eq!(
            feed.active_session(),
            Some(live_id),
            "marking ready must not steal existing live selection"
        );

        // 5. In an empty feed with no live active session, a restored session's digest reaches active on ready
        let solo_feed = ConversationFeed::new();
        let solo_id = Ulid::generate();
        let solo_inc = Ulid::generate();
        solo_feed.register_session_incarnation(
            solo_id,
            solo_inc,
            recording_poster(Arc::new(Mutex::new(Vec::new()))),
            false,
        );
        solo_feed
            .observe_replayed_incarnation(
                solo_inc,
                &message_envelope(solo_id, "user:http", "restored solo conversation"),
            )
            .await;
        assert_eq!(
            solo_feed.active_session(),
            None,
            "rebuilding replay does not choose active"
        );

        solo_feed.mark_incarnation_ready(solo_id, solo_inc);
        assert_eq!(
            solo_feed.active_session(),
            Some(solo_id),
            "digest reaches active upon ready when no competing live active exists"
        );
        assert!(solo_feed.digest().contains("restored solo conversation"));
    }

    #[tokio::test]
    async fn delayed_flow_note_stays_pinned_to_original_target_after_active_switches() {
        let feed = ConversationFeed::with_workspace(PathBuf::from("/workspace"));
        let session_a = Ulid::generate();
        let inc_a = Ulid::generate();
        let session_b = Ulid::generate();
        let inc_b = Ulid::generate();
        let sink_a = Arc::new(Mutex::new(Vec::new()));
        let sink_b = Arc::new(Mutex::new(Vec::new()));

        let post_a = {
            let sink = sink_a.clone();
            Arc::new(
                move |text: String, flow: String, run_id: String, wake: bool| {
                    sink.lock().unwrap().push((text, flow, run_id, wake));
                    Ok(())
                },
            )
        };
        let post_b = {
            let sink = sink_b.clone();
            Arc::new(
                move |text: String, flow: String, run_id: String, wake: bool| {
                    sink.lock().unwrap().push((text, flow, run_id, wake));
                    Ok(())
                },
            )
        };

        feed.register_session_incarnation(
            session_a,
            inc_a,
            recording_poster(Arc::new(Mutex::new(Vec::new()))),
            true,
        );
        feed.register_flow_poster_incarnation(session_a, inc_a, post_a);

        feed.register_session_incarnation(
            session_b,
            inc_b,
            recording_poster(Arc::new(Mutex::new(Vec::new()))),
            true,
        );
        feed.register_flow_poster_incarnation(session_b, inc_b, post_b);

        // Make session A active and capture snapshot
        feed.observe(&message_envelope(session_a, "user:http", "start in A"))
            .await;
        let snap_a = feed.snapshot();
        let target_a = snap_a.target.expect("target for A must be captured");
        assert_eq!(target_a.session_id, session_a);
        assert_eq!(target_a.incarnation, inc_a);

        // Switch active conversation to session B
        feed.observe(&message_envelope(session_b, "user:http", "switch to B"))
            .await;
        assert_eq!(feed.active_session(), Some(session_b));

        // Post delayed flow note using pinned target A: must land in sink A, not sink B
        feed.post_targeted_flow_note(
            &target_a,
            "delayed report for A".into(),
            "flow-test".into(),
            "run-1".into(),
            false,
        )
        .expect("posting to target A must succeed");

        assert_eq!(sink_a.lock().unwrap().len(), 1);
        assert_eq!(sink_a.lock().unwrap()[0].0, "delayed report for A");
        assert!(sink_b.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn resumed_session_rejects_old_incarnation_flow_note() {
        let feed = ConversationFeed::with_workspace(PathBuf::from("/workspace"));
        let session_id = Ulid::generate();
        let old_inc = Ulid::generate();
        let new_inc = Ulid::generate();

        feed.register_session_incarnation(
            session_id,
            old_inc,
            recording_poster(Arc::new(Mutex::new(Vec::new()))),
            true,
        );
        let target_old = ConversationTarget {
            workspace: PathBuf::from("/workspace"),
            session_id,
            incarnation: old_inc,
        };

        // Resume starts new incarnation
        feed.register_session_incarnation(
            session_id,
            new_inc,
            recording_poster(Arc::new(Mutex::new(Vec::new()))),
            true,
        );

        let result = feed.post_targeted_flow_note(
            &target_old,
            "late note".into(),
            "flow".into(),
            "run-2".into(),
            false,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unavailable or superseded"));
    }

    #[tokio::test]
    async fn closed_receiver_or_resync_invalidates_session_incarnation() {
        let feed = ConversationFeed::with_workspace(PathBuf::from("/workspace"));
        let session_id = Ulid::generate();
        let inc = Ulid::generate();
        feed.register_session_incarnation(
            session_id,
            inc,
            recording_poster(Arc::new(Mutex::new(Vec::new()))),
            true,
        );
        feed.observe(&message_envelope(session_id, "user:http", "hello"))
            .await;
        feed.observe(&yield_envelope(session_id)).await;
        feed.post_wake("wake").await;

        let target = ConversationTarget {
            workspace: PathBuf::from("/workspace"),
            session_id,
            incarnation: inc,
        };

        // Invalidation marks the incarnation unavailable and drops queued wake
        assert!(feed.invalidate_session_incarnation(session_id, inc));
        assert!(!feed.is_incarnation_available(session_id, inc));
        assert_eq!(feed.active_session(), None);
        assert_eq!(feed.digest(), "");

        // Late wake to invalidated target is rejected
        assert!(matches!(
            feed.post_targeted_wake(&target, "late wake").await,
            WakeOutcome::Failed(_, msg) if msg.contains("unavailable")
        ));
    }

    #[tokio::test]
    async fn rebuilding_session_is_not_available_until_marked_ready() {
        let feed = ConversationFeed::with_workspace(PathBuf::from("/workspace"));
        let session_id = Ulid::generate();
        let inc = Ulid::generate();

        // Starts rebuilding (available = false)
        feed.register_session_incarnation(
            session_id,
            inc,
            recording_poster(Arc::new(Mutex::new(Vec::new()))),
            false,
        );
        assert!(!feed.is_incarnation_available(session_id, inc));
        assert!(!feed.snapshot().available);

        // Replay events are observed into digest even while rebuilding
        feed.observe_replayed_incarnation(
            inc,
            &message_envelope(session_id, "user:http", "replayed message"),
        )
        .await;

        // Still not available for live wakes/effects
        let target = ConversationTarget {
            workspace: PathBuf::from("/workspace"),
            session_id,
            incarnation: inc,
        };
        assert!(matches!(
            feed.post_targeted_wake(&target, "wake").await,
            WakeOutcome::Failed(_, msg) if msg.contains("unavailable")
        ));

        // Mark ready: now available and digest is accessible
        assert!(feed.mark_incarnation_ready(session_id, inc));
        assert!(feed.is_incarnation_available(session_id, inc));
        assert!(feed.digest().contains("replayed message"));
        assert!(feed.snapshot().available);
    }

    #[tokio::test]
    async fn workspace_mismatch_rejects_targeted_wake_and_flow_note() {
        let feed = ConversationFeed::with_workspace(PathBuf::from("/workspace/a"));
        let session_id = Ulid::generate();
        let inc = Ulid::generate();
        feed.register_session_incarnation(
            session_id,
            inc,
            recording_poster(Arc::new(Mutex::new(Vec::new()))),
            true,
        );

        let foreign_target = ConversationTarget {
            workspace: PathBuf::from("/workspace/b"),
            session_id,
            incarnation: inc,
        };

        // Targeted wake with workspace mismatch is rejected
        assert!(matches!(
            feed.post_targeted_wake(&foreign_target, "cross-workspace wake")
                .await,
            WakeOutcome::Failed(_, err) if err.contains("workspace mismatch")
        ));

        // Targeted flow note with workspace mismatch is rejected
        let res = feed.post_targeted_flow_note(
            &foreign_target,
            "note".into(),
            "flow".into(),
            "run".into(),
            false,
        );
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("workspace mismatch"));
    }

    #[tokio::test]
    async fn explicit_binding_validates_editor_and_session_availability() {
        let feed = ConversationFeed::with_workspace(PathBuf::from("/workspace"));
        let session_id = Ulid::generate();
        let inc = Ulid::generate();
        let editor_id = EditorId::new();

        // Editor not registered yet -> EditorUnavailable
        assert_eq!(
            feed.bind_editor(session_id, editor_id),
            Err(BindingError::EditorUnavailable)
        );

        feed.register_editor(
            editor_id,
            Arc::new(|_| Box::pin(async { Ok(Default::default()) })),
            CancellationToken::new(),
        );

        // Session not registered yet -> SessionUnavailable
        assert_eq!(
            feed.bind_editor(session_id, editor_id),
            Err(BindingError::SessionUnavailable)
        );

        // Session registered with available=false -> SessionUnavailable
        feed.register_session_incarnation(
            session_id,
            inc,
            recording_poster(Arc::new(Mutex::new(Vec::new()))),
            false,
        );
        assert_eq!(
            feed.bind_editor(session_id, editor_id),
            Err(BindingError::SessionUnavailable)
        );

        // Mark ready -> Binding succeeds
        feed.mark_incarnation_ready(session_id, inc);
        assert_eq!(feed.bind_editor(session_id, editor_id), Ok(()));

        // Invalidation clears explicit binding
        feed.invalidate_session_incarnation(session_id, inc);
        let sel = feed.select_editor(Some(session_id)).unwrap();
        assert_eq!(sel.id, editor_id);
    }

    #[tokio::test]
    async fn replacement_clears_old_binding_and_stale_unbind_is_ignored() {
        let feed = ConversationFeed::with_workspace(PathBuf::from("/workspace"));
        let session_id = Ulid::generate();
        let i1 = Ulid::generate();
        let i2 = Ulid::generate();

        let e1 = EditorId::new();
        let e2 = EditorId::new();
        feed.register_editor(
            e1,
            Arc::new(|_| Box::pin(async { Ok(Default::default()) })),
            CancellationToken::new(),
        );
        feed.register_editor(
            e2,
            Arc::new(|_| Box::pin(async { Ok(Default::default()) })),
            CancellationToken::new(),
        );

        // 1. I1 registered and bound to E1
        feed.register_session_incarnation(
            session_id,
            i1,
            recording_poster(Arc::new(Mutex::new(Vec::new()))),
            true,
        );
        assert_eq!(feed.bind_editor(session_id, e1), Ok(()));
        let sel = feed.select_editor(Some(session_id)).unwrap();
        assert_eq!(sel.id, e1);

        // 2. Replaced by I2 (e.g. session resume) before old watcher cleanup runs
        feed.register_session_incarnation(
            session_id,
            i2,
            recording_poster(Arc::new(Mutex::new(Vec::new()))),
            false,
        );

        // 3. Mark I2 ready with 2 editors connected => ambiguous until new bind!
        feed.mark_incarnation_ready(session_id, i2);
        assert_eq!(
            feed.select_editor(Some(session_id)).unwrap_err(),
            EditorSelectionError::Ambiguous,
            "replacement must clear old incarnation binding"
        );

        // 4. Bind I2 to E2
        assert_eq!(feed.bind_editor(session_id, e2), Ok(()));
        let sel2 = feed.select_editor(Some(session_id)).unwrap();
        assert_eq!(sel2.id, e2);

        // 5. Stale delayed unbind for I1 runs from old watcher cleanup
        assert!(
            !feed.unbind_editor_incarnation(session_id, i1),
            "stale unbind for old incarnation is a no-op"
        );

        // 6. Binding to E2 remains intact
        let sel_after_stale = feed.select_editor(Some(session_id)).unwrap();
        assert_eq!(sel_after_stale.id, e2);

        // 7. Legitimate unbind for I2 succeeds
        assert!(feed.unbind_editor_incarnation(session_id, i2));
        assert_eq!(
            feed.select_editor(Some(session_id)).unwrap_err(),
            EditorSelectionError::Ambiguous
        );
    }

    #[tokio::test]
    async fn snapshot_target_pins_digest_even_when_active_switches() {
        let feed = ConversationFeed::with_workspace(PathBuf::from("/workspace"));
        let session_a = Ulid::generate();
        let inc_a = Ulid::generate();
        let session_b = Ulid::generate();
        let inc_b = Ulid::generate();

        feed.register_session_incarnation(
            session_a,
            inc_a,
            recording_poster(Arc::new(Mutex::new(Vec::new()))),
            true,
        );
        feed.observe_incarnation(
            inc_a,
            &message_envelope(session_a, "user:http", "conversation A"),
        )
        .await;

        let target_a = ConversationTarget {
            workspace: PathBuf::from("/workspace"),
            session_id: session_a,
            incarnation: inc_a,
        };

        // Snapshot target A captures A's digest
        let snap_a = feed.snapshot_target(&target_a);
        assert!(snap_a.available);
        assert!(snap_a.digest.contains("conversation A"));

        // Switch active conversation to B
        feed.register_session_incarnation(
            session_b,
            inc_b,
            recording_poster(Arc::new(Mutex::new(Vec::new()))),
            true,
        );
        feed.observe_incarnation(
            inc_b,
            &message_envelope(session_b, "user:http", "conversation B"),
        )
        .await;
        assert_eq!(feed.active_session(), Some(session_b));

        // Global snapshot is now B
        assert!(feed.snapshot().digest.contains("conversation B"));

        // Target A snapshot still returns A's digest pinned!
        let pinned_a = feed.snapshot_target(&target_a);
        assert!(pinned_a.available);
        assert!(pinned_a.digest.contains("conversation A"));

        // When A is invalidated, target A snapshot reports unavailable
        feed.invalidate_session_incarnation(session_a, inc_a);
        let invalid_a = feed.snapshot_target(&target_a);
        assert!(!invalid_a.available);
    }

    #[tokio::test]
    async fn snapshot_for_editor_routes_by_binding_or_sole_editor() {
        let feed = ConversationFeed::with_workspace(PathBuf::from("/workspace"));
        let session_a = Ulid::generate();
        let inc_a = Ulid::generate();

        feed.register_session_incarnation(
            session_a,
            inc_a,
            recording_poster(Arc::new(Mutex::new(Vec::new()))),
            true,
        );
        feed.observe_incarnation(
            inc_a,
            &message_envelope(session_a, "user:http", "chat for editor A"),
        )
        .await;

        let e1 = EditorId::new();
        let e2 = EditorId::new();
        feed.register_editor(
            e1,
            Arc::new(|_| Box::pin(async { Ok(Default::default()) })),
            CancellationToken::new(),
        );

        // When E1 is sole editor, snapshot_for_editor returns active conversation
        let snap_sole = feed.snapshot_for_editor(e1);
        assert!(snap_sole.available);
        assert!(snap_sole.digest.contains("chat for editor A"));

        // Register E2: now 2 editors connected, unbound is ambiguous
        feed.register_editor(
            e2,
            Arc::new(|_| Box::pin(async { Ok(Default::default()) })),
            CancellationToken::new(),
        );
        assert!(!feed.snapshot_for_editor(e1).available);

        // Bind session A to E1
        assert_eq!(feed.bind_editor(session_a, e1), Ok(()));
        let snap_bound = feed.snapshot_for_editor(e1);
        assert!(snap_bound.available);
        assert!(snap_bound.digest.contains("chat for editor A"));
        assert_eq!(snap_bound.target.unwrap().session_id, session_a);

        // E2 is still unbound
        assert!(!feed.snapshot_for_editor(e2).available);
    }

    #[tokio::test]
    async fn binding_session_clears_previous_editor_bindings_and_snapshots_focused_session() {
        let feed = ConversationFeed::with_workspace(PathBuf::from("/workspace"));
        let s1 = Ulid::generate();
        let inc1 = Ulid::generate();
        let s2 = Ulid::generate();
        let inc2 = Ulid::generate();
        let s3 = Ulid::generate();
        let inc3 = Ulid::generate();

        feed.register_session_incarnation(
            s1,
            inc1,
            recording_poster(Arc::new(Mutex::new(Vec::new()))),
            true,
        );
        feed.register_session_incarnation(
            s2,
            inc2,
            recording_poster(Arc::new(Mutex::new(Vec::new()))),
            true,
        );
        feed.register_session_incarnation(
            s3,
            inc3,
            recording_poster(Arc::new(Mutex::new(Vec::new()))),
            true,
        );

        let editor = EditorId::new();
        feed.register_editor(
            editor,
            Arc::new(|_| Box::pin(async { Ok(Default::default()) })),
            CancellationToken::new(),
        );

        // 1. Bind s1 to editor
        assert_eq!(feed.bind_editor(s1, editor), Ok(()));
        assert_eq!(feed.bound_editor(s1).map(|(e, _)| e), Some(editor));
        feed.observe_incarnation(inc1, &message_envelope(s1, "user:http", "chat in s1"))
            .await;
        let snap1 = feed.snapshot_for_editor(editor);
        assert!(snap1.available);
        assert_eq!(snap1.target.unwrap().session_id, s1);
        assert!(snap1.digest.contains("chat in s1"));

        // 2. Binding s2 to the same editor replaces s1 binding
        assert_eq!(feed.bind_editor(s2, editor), Ok(()));
        assert_eq!(feed.bound_editor(s1), None, "s1 binding must be cleared");
        assert_eq!(feed.bound_editor(s2).map(|(e, _)| e), Some(editor));

        feed.observe_incarnation(inc2, &message_envelope(s2, "user:http", "chat in s2"))
            .await;
        let snap2 = feed.snapshot_for_editor(editor);
        assert!(snap2.available);
        assert_eq!(snap2.target.unwrap().session_id, s2);
        assert!(snap2.digest.contains("chat in s2"));
        assert!(!snap2.digest.contains("chat in s1"));

        // 3. Unrelated session s3 becoming active does not steal the snapshot of bound session s2
        feed.observe_incarnation(inc3, &message_envelope(s3, "user:http", "chat in s3"))
            .await;
        assert_eq!(feed.active_session(), Some(s3));
        let snap_after_s3 = feed.snapshot_for_editor(editor);
        assert!(snap_after_s3.available);
        assert_eq!(snap_after_s3.target.unwrap().session_id, s2);
    }

    #[tokio::test]
    async fn editor_selection_sole_editor_explicit_binding_and_ambiguity() {
        let feed = ConversationFeed::new();
        let session_1 = Ulid::generate();
        let inc_1 = Ulid::generate();
        let session_2 = Ulid::generate();
        let inc_2 = Ulid::generate();

        feed.register_session_incarnation(
            session_1,
            inc_1,
            recording_poster(Arc::new(Mutex::new(Vec::new()))),
            true,
        );
        feed.register_session_incarnation(
            session_2,
            inc_2,
            recording_poster(Arc::new(Mutex::new(Vec::new()))),
            true,
        );

        // 0 editors connected: Unavailable
        assert_eq!(
            feed.select_editor(None).unwrap_err(),
            EditorSelectionError::Unavailable
        );

        let editor_a = EditorId::new();
        let cancel_a = CancellationToken::new();
        let reg_a = feed.register_editor_lifecycle(
            editor_a,
            Arc::new(|_| Box::pin(async { Ok(Default::default()) })),
            cancel_a.clone(),
        );

        // 1 editor connected: unambiguous selection
        let sel = feed.select_editor(None).unwrap();
        assert_eq!(sel.id, editor_a);

        let editor_b = EditorId::new();
        let cancel_b = CancellationToken::new();
        let reg_b = feed.register_editor_lifecycle(
            editor_b,
            Arc::new(|_| Box::pin(async { Ok(Default::default()) })),
            cancel_b.clone(),
        );

        // 2 editors connected without explicit binding: Ambiguous
        assert_eq!(
            feed.select_editor(None).unwrap_err(),
            EditorSelectionError::Ambiguous
        );

        // Explicit binding routes to bound editor
        assert_eq!(feed.bind_editor(session_2, editor_b), Ok(()));
        let sel_bound = feed.select_editor(Some(session_2)).unwrap();
        assert_eq!(sel_bound.id, editor_b);

        // Session 1 remains unbound and ambiguous
        assert_eq!(
            feed.select_editor(Some(session_1)).unwrap_err(),
            EditorSelectionError::Ambiguous
        );

        // Detach editor B cancels its token and removes binding
        reg_b.detach();
        assert!(cancel_b.is_cancelled());
        assert!(!cancel_a.is_cancelled());

        // Now only editor A remains: unambiguous selection
        let sel_after = feed.select_editor(Some(session_2)).unwrap();
        assert_eq!(sel_after.id, editor_a);

        drop(reg_a);
        assert!(cancel_a.is_cancelled());
    }

    #[tokio::test]
    async fn old_watcher_cannot_mutate_or_cleanup_new_incarnation() {
        let feed = ConversationFeed::new();
        let session_id = Ulid::generate();
        let old_inc = Ulid::generate();
        let new_inc = Ulid::generate();

        feed.register_session_incarnation(
            session_id,
            new_inc,
            recording_poster(Arc::new(Mutex::new(Vec::new()))),
            true,
        );
        feed.observe_incarnation(
            new_inc,
            &message_envelope(session_id, "user:http", "new incarnation message"),
        )
        .await;
        assert!(feed.digest().contains("new incarnation message"));

        // Stale watcher events with old incarnation are ignored
        feed.observe_incarnation(
            old_inc,
            &message_envelope(session_id, "user:http", "stale message"),
        )
        .await;
        assert!(!feed.digest().contains("stale message"));

        // Stale watcher invalidation or cleanup does not drop new incarnation
        assert!(!feed.invalidate_session_incarnation(session_id, old_inc));
        assert!(!feed.forget_session_incarnation(session_id, old_inc));
        assert!(feed.is_incarnation_available(session_id, new_inc));
        assert_eq!(feed.active_session(), Some(session_id));
    }
}
