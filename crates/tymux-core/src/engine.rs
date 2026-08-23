use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::layout::{
    LayoutNode, Orientation, RemoveOutcome, MIN_PANE_COLS, MIN_PANE_ROWS,
    RECOMMENDED_SPLIT_MIN_ROWS,
};
use crate::pane::Pane;
use crate::persistence::{
    persisted_layout_to_live, NullPersistenceBackend, PersistedPaneRecord, PersistedSessionRecord,
    PersistedWindowRecord, PersistenceBackend,
};

const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;

/// Per-window attached-client viewport tracker: `window_id -> (client_id ->
/// (rows, cols))`.
type ViewportsByWindow = HashMap<Uuid, HashMap<u64, (u16, u16)>>;

pub struct WindowState {
    pub id: Uuid,
    pub name: String,
    pub layout: LayoutNode,
    /// The window's current effective size — the dimension-wise minimum of
    /// every attached client's last-reported viewport for this window
    /// (ADR-004), or `DEFAULT_ROWS`/`DEFAULT_COLS` if nobody's attached.
    pub rows: u16,
    pub cols: u16,
}

/// `SessionState.windows`/`active_window_id` replaces the old
/// one-window-one-pane shape (Migration Plan §3.3). Real pane handles now
/// live in `Engine.panes`, keyed by id — `WindowState.layout`'s leaves
/// reference into that map, never holding an `Arc<Pane>` directly
/// (ADR-001 §2).
pub struct SessionState {
    pub id: Uuid,
    pub name: String,
    pub windows: Vec<WindowState>,
    pub active_window_id: Uuid,
}

/// `Engine.panes`' value type. `Dead(record)` means a session was loaded
/// from a persisted record at daemon startup and hasn't been revived yet
/// — there is no live process at all, only the metadata `tymux revive`
/// needs to respawn one. A pane whose process merely *exited* while still
/// tracked stays `Live(Arc<Pane>)`; `Pane::is_exited()` already answers
/// that (see `pane_lookup`), no separate entry state is needed for it.
pub enum PaneEntry {
    Live(Arc<Pane>),
    Dead(PersistedPaneRecord),
}

/// The three-way outcome of looking up a pane by id, replacing the old
/// `Option<Arc<Pane>>` (which collapsed "exited" and "never existed" into
/// the same `None`).
pub enum PaneLookup {
    Live(Arc<Pane>),
    Dead,
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EngineError {
    PaneNotFound(Uuid),
    SessionNotFound(Uuid),
    BelowMinimumSize {
        rows: u16,
        cols: u16,
    },
    /// Epic 3 Story 3.5 AC2's friendlier, higher-tier rejection — see
    /// [`crate::layout::LayoutError::BelowRecommendedSize`].
    BelowRecommendedSize {
        rows: u16,
    },
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::PaneNotFound(id) => write!(f, "no such pane: {id}"),
            EngineError::SessionNotFound(id) => write!(f, "no such session: {id}"),
            EngineError::BelowMinimumSize { rows, cols } => write!(
                f,
                "split would produce a pane of {rows} rows x {cols} cols, \
                 below the minimum of {MIN_PANE_ROWS} rows x {MIN_PANE_COLS} cols"
            ),
            EngineError::BelowRecommendedSize { rows } => write!(
                f,
                "Can't split: pane is {rows} rows, minimum for a horizontal split is \
                 ~{RECOMMENDED_SPLIT_MIN_ROWS} rows. Resize your terminal or close another \
                 pane first."
            ),
        }
    }
}

impl std::error::Error for EngineError {}

#[derive(Debug)]
pub struct PaneInfo {
    pub id: Uuid,
    pub rows: u32,
    pub cols: u32,
    pub live: bool,
    /// The pane's working directory at spawn time (Epic 1.5) — empty only
    /// when the pane itself couldn't be resolved (`PaneEntry`/`PaneHandle`
    /// `None`/`Missing`, which shouldn't happen for a leaf with a live
    /// `pane_id`).
    pub cwd: String,
}

/// A read-only snapshot of one window's layout tree, with each leaf's
/// `pane_id` resolved to full `PaneInfo` (rows/cols/liveness) — the shape
/// `tymuxd`'s `session_to_proto` walks directly into the wire `Layout`
/// message, decoupled from `Engine`'s internal lock-guarded state.
#[derive(Debug)]
pub enum LayoutSnapshot {
    Leaf(PaneInfo),
    Split {
        orientation: Orientation,
        children: Vec<(LayoutSnapshot, f32)>,
    },
}

#[derive(Debug)]
pub struct WindowSnapshot {
    pub id: Uuid,
    pub name: String,
    pub layout: LayoutSnapshot,
}

#[derive(Debug)]
pub struct SessionSnapshot {
    pub id: Uuid,
    pub name: String,
    pub windows: Vec<WindowSnapshot>,
    /// True if at least one pane anywhere in the session is still live.
    pub live: bool,
}

/// What closing a pane did to its window/session, beyond just removing the
/// pane — the CLI needs this to print "window closed"/"session closed"
/// instead of a silent disappearance (Story 3.5 AC3).
pub struct ClosePaneOutcome {
    pub window_closed: Option<(Uuid, String)>,
    pub session_closed: Option<(Uuid, String)>,
    /// The session's snapshot after the close, if it still exists.
    pub session: Option<SessionSnapshot>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReviveOutcome {
    Revived {
        pane_count: usize,
    },
    /// The session wasn't dead-flagged to begin with — a friendly no-op,
    /// never a second spawn (Story 4.4 AC3).
    AlreadyLive,
}

/// Cross-cutting locking discipline (`architecture.md` §4, `pitfalls.md`'s
/// closing observation): every method that touches both `sessions` and
/// `panes` acquires `sessions` first, then `panes`, for the duration of a
/// single mutation, and releases both together — never holding one across
/// an `.await`/blocking call into the other. This gives any concurrent
/// reader the invariant that a window's `LayoutNode` and the `panes` map
/// are always mutually consistent whenever neither lock is held: a reader
/// never observes a `Leaf.pane_id` with no corresponding `panes` entry, or
/// vice versa.
///
/// The one exception is `Pane::resize()`'s blocking OS syscall (window
/// geometry recompute): that is deliberately done *outside* both locks —
/// see `recompute_window_geometry` — to avoid reintroducing the
/// lock-held-across-a-blocking-call hang class already fixed once for
/// Ctrl-D.
pub struct Engine {
    sessions: Mutex<HashMap<Uuid, SessionState>>,
    panes: Mutex<HashMap<Uuid, PaneEntry>>,
    /// Per-window attached-client viewport tracker (ADR-004): every
    /// currently-attached client's last-reported `(rows, cols)` for the
    /// window it's attached into. The window's effective size is the
    /// dimension-wise minimum across all of them.
    viewports: Mutex<ViewportsByWindow>,
    next_client_id: AtomicU64,
    /// One broadcast channel per window with at least one `WatchWindow`
    /// subscriber — a `()` tick means "re-fetch this window's snapshot,
    /// something about its structure or geometry changed."
    window_watchers: Mutex<HashMap<Uuid, broadcast::Sender<()>>>,
    /// O(1) reverse lookups (pane -> window -> session) — see
    /// [`WindowIndex`]'s own doc comment for what it's for and why it's one
    /// lock, not two.
    window_index: Mutex<WindowIndex>,
    /// Storage seam (Story 4.1 Task 5, architecture-review.md Blocker #2):
    /// `Engine::new()` uses `NullPersistenceBackend` (tests never touch
    /// disk unless they opt in via `Engine::with_persistence`); `tymuxd`'s
    /// `main()` supplies a real `FsPersistenceBackend`.
    persistence: Box<dyn PersistenceBackend>,
}

/// Reverse indices (pane_id -> window_id -> session_id) for O(1) pane/window
/// lookups (plan.md's Epic 1.4 follow-up). One `Mutex` for both maps, not
/// two: two call sites once acquired separate locks in opposite order — a
/// real deadlock under concurrent, `Arc`-shared access. One lock makes that
/// bug class structurally impossible.
#[derive(Default)]
struct WindowIndex {
    pane_window: HashMap<Uuid, Uuid>,
    window_session: HashMap<Uuid, Uuid>,
}

impl WindowIndex {
    fn window_for_pane(&self, pane_id: Uuid) -> Option<Uuid> {
        self.pane_window.get(&pane_id).copied()
    }

    fn session_for_window(&self, window_id: Uuid) -> Option<Uuid> {
        self.window_session.get(&window_id).copied()
    }

    fn insert_pane(&mut self, pane_id: Uuid, window_id: Uuid) {
        self.pane_window.insert(pane_id, window_id);
    }

    fn insert_window(&mut self, window_id: Uuid, session_id: Uuid) {
        self.window_session.insert(window_id, session_id);
    }

    fn remove_pane(&mut self, pane_id: Uuid) {
        self.pane_window.remove(&pane_id);
    }

    fn remove_window(&mut self, window_id: Uuid) {
        self.window_session.remove(&window_id);
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            panes: Mutex::new(HashMap::new()),
            viewports: Mutex::new(HashMap::new()),
            next_client_id: AtomicU64::new(1),
            window_watchers: Mutex::new(HashMap::new()),
            window_index: Mutex::new(WindowIndex::default()),
            persistence: Box::new(NullPersistenceBackend),
        }
    }
}

impl Engine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_persistence(persistence: Box<dyn PersistenceBackend>) -> Self {
        Self {
            persistence,
            ..Self::default()
        }
    }

    /// Populates the engine with dead-flagged sessions reconstructed from
    /// already-validated persisted records (Story 4.3) — only called at
    /// daemon startup, before serving any RPC. Every leaf's `pane_id`
    /// becomes a `PaneEntry::Dead(record)`; `tymux revive` is the only
    /// path that ever turns one back into `Live` (ADR-002's "never
    /// triggered automatically on daemon start" invariant).
    pub fn load_persisted(&self, records: Vec<PersistedSessionRecord>) {
        let mut sessions = self.sessions.lock().unwrap();
        let mut panes = self.panes.lock().unwrap();
        let mut window_index = self.window_index.lock().unwrap();
        for record in records {
            let session_id = record.session_id;
            let windows: Vec<WindowState> = record
                .windows
                .into_iter()
                .map(|w: PersistedWindowRecord| {
                    let window = WindowState {
                        id: w.id,
                        name: w.name,
                        layout: persisted_layout_to_live(&w.layout, &mut panes),
                        rows: DEFAULT_ROWS,
                        cols: DEFAULT_COLS,
                    };
                    window_index.insert_window(window.id, session_id);
                    for pane_id in window.layout.leaves() {
                        window_index.insert_pane(pane_id, window.id);
                    }
                    window
                })
                .collect();
            sessions.insert(
                session_id,
                SessionState {
                    id: session_id,
                    name: record.name,
                    windows,
                    active_window_id: record.active_window_id,
                },
            );
        }
    }

    /// `(window_id, session_id)` for a pane, via the O(1) reverse index —
    /// shared by `split_pane`/`close_pane`, which both need to resolve a
    /// pane to its owning window *and* session before mutating either.
    fn window_and_session_for_pane(&self, pane_id: Uuid) -> Result<(Uuid, Uuid), EngineError> {
        let window_index = self.window_index.lock().unwrap();
        let window_id = window_index
            .window_for_pane(pane_id)
            .ok_or(EngineError::PaneNotFound(pane_id))?;
        let session_id = window_index
            .session_for_window(window_id)
            .ok_or(EngineError::PaneNotFound(pane_id))?;
        Ok((window_id, session_id))
    }

    /// Snapshots the record to persist — cheap, in-memory, done *while*
    /// holding `sessions`/`panes` (the same shape as computing new window
    /// geometry under lock). Deliberately does NOT call into
    /// `self.persistence.save()` here: callers must drop both locks first
    /// and call `Self::save_persisted` afterward, so a slow backend's I/O
    /// never blocks a concurrent `list_sessions`/etc. on an unrelated
    /// session (Story 4.2 AC2) — the same single-owner-writer shape
    /// already established for `Pane::resize()`.
    fn snapshot_persisted_record(
        sessions: &HashMap<Uuid, SessionState>,
        panes: &HashMap<Uuid, PaneEntry>,
        session_id: Uuid,
    ) -> Option<PersistedSessionRecord> {
        sessions
            .get(&session_id)
            .map(|session| PersistedSessionRecord::from_session_state(session, panes))
    }

    fn save_persisted(&self, record: Option<PersistedSessionRecord>) {
        let Some(record) = record else { return };
        let session_id = record.session_id;
        if let Err(e) = self.persistence.save(&record) {
            tracing::warn!(session_id = %session_id, error = %e, "failed to persist session");
        }
    }

    pub fn create_session(
        &self,
        name: String,
        command: Option<String>,
        cwd: Option<String>,
    ) -> Result<Uuid> {
        let shell = command.unwrap_or_else(default_shell);
        let pane = Pane::spawn_with_cwd(&shell, cwd.as_deref(), DEFAULT_ROWS, DEFAULT_COLS)?;
        let pane_id = pane.id;
        let window_id = Uuid::new_v4();

        let window = WindowState {
            id: window_id,
            name: "0".to_string(),
            layout: LayoutNode::leaf(pane_id),
            rows: DEFAULT_ROWS,
            cols: DEFAULT_COLS,
        };
        let session = SessionState {
            id: Uuid::new_v4(),
            name,
            windows: vec![window],
            active_window_id: window_id,
        };
        let id = session.id;

        // sessions-then-panes lock ordering (see the Engine doc comment).
        self.sessions.lock().unwrap().insert(id, session);
        self.panes
            .lock()
            .unwrap()
            .insert(pane_id, PaneEntry::Live(pane));
        {
            let mut window_index = self.window_index.lock().unwrap();
            window_index.insert_window(window_id, id);
            window_index.insert_pane(pane_id, window_id);
        }

        let record = {
            let sessions = self.sessions.lock().unwrap();
            let panes = self.panes.lock().unwrap();
            Self::snapshot_persisted_record(&sessions, &panes, id)
        };
        self.save_persisted(record);
        Ok(id)
    }

    /// Story 3.4 AC3: this must not block for the duration of a *different*
    /// window's in-flight `Pane::resize()` syscalls. It's not enough for
    /// `recompute_window_geometry` to release `sessions`/`panes` around its
    /// own `Pane::resize()` calls (it already does) — `list_sessions`
    /// itself also reads every live pane's current size via `pane.size()`,
    /// which locks that pane's own internal mutex, the same one
    /// `Pane::resize()` briefly holds while resizing its `vt100` screen
    /// buffer. So this resolves the structural shape (and each leaf's
    /// pane handle) under `sessions`/`panes` first, drops both locks, then
    /// only *after* that reads each live pane's size/exited status — see
    /// `handle_to_layout_snapshot`'s doc comment for what would go wrong
    /// if this were done in one locked pass instead.
    pub fn list_sessions(&self) -> Vec<SessionSnapshot> {
        let handles: Vec<SessionHandle> = {
            let sessions = self.sessions.lock().unwrap();
            let panes = self.panes.lock().unwrap();
            sessions
                .values()
                .map(|s| SessionHandle {
                    id: s.id,
                    name: s.name.clone(),
                    windows: s
                        .windows
                        .iter()
                        .map(|w| WindowHandle {
                            id: w.id,
                            name: w.name.clone(),
                            layout: layout_to_handle(&w.layout, &panes),
                        })
                        .collect(),
                })
                .collect()
        };

        handles
            .into_iter()
            .map(|session| {
                let windows: Vec<WindowSnapshot> = session
                    .windows
                    .into_iter()
                    .map(|w| WindowSnapshot {
                        id: w.id,
                        name: w.name,
                        layout: handle_to_layout_snapshot(&w.layout),
                    })
                    .collect();
                let live = windows.iter().any(|w| window_has_live_pane(&w.layout));
                SessionSnapshot {
                    id: session.id,
                    name: session.name,
                    windows,
                    live,
                }
            })
            .collect()
    }

    /// Kills every live pane in the session before removing it, so any
    /// client currently attached to one of its panes observes a normal
    /// pane-exit event through the existing `wait_exit` path rather than a
    /// bare stream error or silent hang (mirrors the single-pane-era
    /// behavior, extended to every pane in every window).
    pub fn kill_session(&self, id: Uuid) -> Result<()> {
        let session = self
            .sessions
            .lock()
            .unwrap()
            .remove(&id)
            .ok_or_else(|| anyhow!("no such session: {id}"))?;

        let pane_ids: Vec<Uuid> = session
            .windows
            .iter()
            .flat_map(|w| w.layout.leaves())
            .collect();

        {
            let mut window_index = self.window_index.lock().unwrap();
            for window in &session.windows {
                window_index.remove_window(window.id);
            }
            for pane_id in &pane_ids {
                window_index.remove_pane(*pane_id);
            }
        }

        let mut panes = self.panes.lock().unwrap();
        for pane_id in pane_ids {
            if let Some(PaneEntry::Live(pane)) = panes.remove(&pane_id) {
                if !pane.is_exited() {
                    if let Err(e) = pane.kill() {
                        tracing::warn!(session_id = %id, pane_id = %pane_id, error = %e, "kill_session: failed to kill pane process");
                    }
                }
            }
        }
        drop(panes);
        self.persistence.delete(id);
        Ok(())
    }

    /// Finds a pane by id across every session — the pane namespace is
    /// flat (a `pane_id` already uniquely identifies its pane regardless
    /// of which session/window it lives in).
    pub fn pane_lookup(&self, pane_id: Uuid) -> PaneLookup {
        match self.panes.lock().unwrap().get(&pane_id) {
            None => PaneLookup::Unknown,
            Some(PaneEntry::Dead(_)) => PaneLookup::Dead,
            Some(PaneEntry::Live(pane)) if pane.is_exited() => PaneLookup::Dead,
            Some(PaneEntry::Live(pane)) => PaneLookup::Live(pane.clone()),
        }
    }

    /// The last-known exit code for a pane that has already exited (Story
    /// 1.2.4) — the same code an open `Attach` stream would have
    /// delivered, but readable with no `Attach` stream open at all. Covers
    /// both dead shapes `pane_lookup` collapses into `PaneLookup::Dead`: a
    /// still-`PaneEntry::Live` pane whose child has exited, and a
    /// `PaneEntry::Dead` record reloaded from disk after a restart.
    ///
    /// Returns `None` if the pane is still running or unknown; `Some(None)`
    /// if it exited but no code was ever captured (ADR-001: a real,
    /// distinct state, not backfilled to a placeholder).
    pub fn dead_pane_exit_code(&self, pane_id: Uuid) -> Option<Option<i32>> {
        match self.panes.lock().unwrap().get(&pane_id) {
            Some(PaneEntry::Dead(record)) => Some(record.exit_code),
            Some(PaneEntry::Live(pane)) if pane.is_exited() => Some(pane.exit_code()),
            _ => None,
        }
    }

    /// Splits the leaf for `pane_id` into a new `Split` node (this pane
    /// plus a freshly spawned one), per Story 3.2's `LayoutNode::split`.
    /// The size floor is checked against the pane's window's current
    /// effective size (ADR-004's tracked viewport minimum, or the default
    /// if nobody's attached).
    pub fn split_pane(
        &self,
        pane_id: Uuid,
        orientation: Orientation,
        command: Option<String>,
    ) -> Result<SessionSnapshot, EngineError> {
        let shell = command.unwrap_or_else(default_shell);
        let new_pane = Pane::spawn(&shell, DEFAULT_ROWS, DEFAULT_COLS)
            .map_err(|_| EngineError::PaneNotFound(pane_id))?;
        let new_pane_id = new_pane.id;

        // O(1) via the reverse index instead of scanning every session's
        // every window's layout tree (plan.md's Epic 1.4 follow-up).
        let (window_id, session_id) = self.window_and_session_for_pane(pane_id)?;
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions
            .get_mut(&session_id)
            .ok_or(EngineError::PaneNotFound(pane_id))?;
        let window = session
            .windows
            .iter_mut()
            .find(|w| w.id == window_id)
            .ok_or(EngineError::PaneNotFound(pane_id))?;

        match window
            .layout
            .split(pane_id, orientation, new_pane_id, window.rows, window.cols)
        {
            Ok(()) => {}
            Err(crate::layout::LayoutError::BelowMinimumSize { rows, cols }) => {
                return Err(EngineError::BelowMinimumSize { rows, cols });
            }
            Err(crate::layout::LayoutError::BelowRecommendedSize { rows }) => {
                return Err(EngineError::BelowRecommendedSize { rows });
            }
            Err(crate::layout::LayoutError::PaneNotFound { pane_id }) => {
                return Err(EngineError::PaneNotFound(pane_id));
            }
        }

        drop(sessions);

        self.panes
            .lock()
            .unwrap()
            .insert(new_pane_id, PaneEntry::Live(new_pane));
        self.window_index
            .lock()
            .unwrap()
            .insert_pane(new_pane_id, window_id);

        let (snapshot, record) = {
            let sessions = self.sessions.lock().unwrap();
            let panes = self.panes.lock().unwrap();
            let snapshot = session_to_snapshot(&sessions[&session_id], &panes);
            let record = Self::snapshot_persisted_record(&sessions, &panes, session_id);
            (snapshot, record)
        };
        self.save_persisted(record);
        self.notify_window_changed(window_id);
        Ok(snapshot)
    }

    /// Closes one pane. If it was its window's last pane, the window
    /// itself closes; if that was also the session's last window, the
    /// whole session closes (a semantic `KillSession`) — see
    /// `ClosePaneOutcome`.
    pub fn close_pane(&self, pane_id: Uuid) -> Result<ClosePaneOutcome, EngineError> {
        // O(1) via the reverse index instead of scanning every session's
        // every window's layout tree (plan.md's Epic 1.4 follow-up).
        let (window_id, session_id) = self.window_and_session_for_pane(pane_id)?;

        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions
            .get_mut(&session_id)
            .ok_or(EngineError::PaneNotFound(pane_id))?;
        let window_idx = session
            .windows
            .iter()
            .position(|w| w.id == window_id)
            .ok_or(EngineError::PaneNotFound(pane_id))?;

        let window = session.windows.remove(window_idx);
        let (window_closed, remaining_layout) = match window.layout.remove(pane_id) {
            RemoveOutcome::Collapsed(layout) => (None, Some(layout)),
            RemoveOutcome::WindowEmpty => (Some((window.id, window.name.clone())), None),
        };

        if let Some(layout) = remaining_layout {
            session.windows.insert(
                window_idx,
                WindowState {
                    id: window.id,
                    name: window.name,
                    layout,
                    rows: window.rows,
                    cols: window.cols,
                },
            );
        }

        let session_closed = if session.windows.is_empty() {
            Some((session.id, session.name.clone()))
        } else {
            None
        };
        if let Some(new_active) = session.windows.first() {
            if window_closed.is_some() {
                session.active_window_id = new_active.id;
            }
        }

        let (removed_session, final_snapshot, record) = if session_closed.is_some() {
            (sessions.remove(&session_id), None, None)
        } else {
            let (snapshot, record) = {
                let panes = self.panes.lock().unwrap();
                let record = Self::snapshot_persisted_record(&sessions, &panes, session_id);
                (
                    Some(session_to_snapshot(&sessions[&session_id], &panes)),
                    record,
                )
            };
            (None, snapshot, record)
        };
        drop(sessions);
        if removed_session.is_some() {
            self.persistence.delete(session_id);
        }
        self.save_persisted(record);

        // Keep the O(1) reverse index in sync: the closed pane is always
        // gone; the window is gone too only when it had no panes left
        // (session_closed always implies exactly one window — the one just
        // removed above — so no separate per-window loop is needed here,
        // unlike kill_session which can remove a session with several).
        {
            let mut window_index = self.window_index.lock().unwrap();
            window_index.remove_pane(pane_id);
            if let Some((closed_window_id, _)) = &window_closed {
                window_index.remove_window(*closed_window_id);
            }
        }

        // Kill the closed pane's process, and (if the session closed too)
        // every other pane that went down with it.
        let mut panes = self.panes.lock().unwrap();
        if let Some(PaneEntry::Live(pane)) = panes.remove(&pane_id) {
            if !pane.is_exited() {
                let _ = pane.kill();
            }
        }
        if let Some(session) = &removed_session {
            for other_pane_id in session.windows.iter().flat_map(|w| w.layout.leaves()) {
                if let Some(PaneEntry::Live(pane)) = panes.remove(&other_pane_id) {
                    if !pane.is_exited() {
                        let _ = pane.kill();
                    }
                }
            }
        }

        if window_closed.is_none() {
            self.notify_window_changed(window.id);
        }

        Ok(ClosePaneOutcome {
            window_closed,
            session_closed,
            session: final_snapshot,
        })
    }

    /// Adds a new single-pane window to an existing session.
    pub fn create_window(
        &self,
        session_id: Uuid,
        command: Option<String>,
    ) -> Result<SessionSnapshot, EngineError> {
        let shell = command.unwrap_or_else(default_shell);
        let pane = Pane::spawn(&shell, DEFAULT_ROWS, DEFAULT_COLS)
            .map_err(|_| EngineError::SessionNotFound(session_id))?;
        let pane_id = pane.id;
        let window_id = Uuid::new_v4();

        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions
            .get_mut(&session_id)
            .ok_or(EngineError::SessionNotFound(session_id))?;
        let window_name = session.windows.len().to_string();
        session.windows.push(WindowState {
            id: window_id,
            name: window_name,
            layout: LayoutNode::leaf(pane_id),
            rows: DEFAULT_ROWS,
            cols: DEFAULT_COLS,
        });
        session.active_window_id = window_id;
        self.panes
            .lock()
            .unwrap()
            .insert(pane_id, PaneEntry::Live(pane));
        {
            let mut window_index = self.window_index.lock().unwrap();
            window_index.insert_window(window_id, session_id);
            window_index.insert_pane(pane_id, window_id);
        }

        let panes = self.panes.lock().unwrap();
        let record = Self::snapshot_persisted_record(&sessions, &panes, session_id);
        let snapshot = session_to_snapshot(&sessions[&session_id], &panes);
        drop(sessions);
        drop(panes);
        self.save_persisted(record);
        Ok(snapshot)
    }

    /// Respawns fresh ptys for every dead-flagged pane in a session,
    /// matching the persisted `LayoutNode` shape (same split tree, same
    /// ratios) — each pane's original command re-run in its persisted
    /// `cwd`. Never triggered automatically; only an explicit `tymux
    /// revive` call reaches this (ADR-002).
    ///
    /// **Orphan-on-restart trade-off (Story 1.1.4)**: this always spawns a
    /// *new* OS process. It never reattaches to whatever process a prior
    /// `tymuxd` instance's pane became — `tymuxd` persists no OS PID for a
    /// live pane (Story 2.5.3), so there is nothing to reattach to even in
    /// principle. If a pane was still alive when `tymuxd` restarted (crash,
    /// deploy, `setsid()`/session loss, etc.), that old process is
    /// orphaned: not reaped, not signaled, not tracked — it keeps running
    /// under its original parent (now gone) until it exits on its own or
    /// is found and killed manually. This is an accepted, deliberate
    /// trade-off, not an oversight: real reap-on-restart would require
    /// persisting actual OS PIDs and safely distinguishing "our old
    /// orphan" from "an unrelated process that reused this PID since
    /// restart," which is disproportionate to this project's current
    /// appetite. See Story 1.1.4 (`project_plans/stapler-squad-integration/
    /// implementation/plan.md`) for the full reasoning, `tymuxd`'s startup
    /// `tymux_orphaned_process_count` log line for the runtime visibility
    /// into this leak's size, and `docs/runbooks/orphaned-processes.md` for
    /// how to find and safely clean one up.
    pub fn revive_session(&self, session_id: Uuid) -> Result<ReviveOutcome, EngineError> {
        let sessions = self.sessions.lock().unwrap();
        let session = sessions
            .get(&session_id)
            .ok_or(EngineError::SessionNotFound(session_id))?;
        let pane_ids: Vec<Uuid> = session
            .windows
            .iter()
            .flat_map(|w| w.layout.leaves())
            .collect();
        drop(sessions);

        // Guard clause (Story 4.4 AC3): if any pane is already live, this
        // session isn't dead — never double-spawn. Checked before any
        // respawn work begins.
        let panes = self.panes.lock().unwrap();
        let already_live = pane_ids
            .iter()
            .any(|id| matches!(panes.get(id), Some(PaneEntry::Live(_))));
        drop(panes);
        if already_live {
            return Ok(ReviveOutcome::AlreadyLive);
        }

        let mut revived = 0usize;
        for pane_id in &pane_ids {
            let record = {
                let panes = self.panes.lock().unwrap();
                match panes.get(pane_id) {
                    Some(PaneEntry::Dead(record)) => record.clone(),
                    _ => continue,
                }
            };
            match Pane::spawn_with_id(
                *pane_id,
                &record.command,
                Some(&record.cwd),
                record.rows.max(MIN_PANE_ROWS),
                record.cols.max(MIN_PANE_COLS),
            ) {
                Ok(new_pane) => {
                    self.panes
                        .lock()
                        .unwrap()
                        .insert(*pane_id, PaneEntry::Live(new_pane));
                    revived += 1;
                }
                Err(e) => {
                    tracing::warn!(pane_id = %pane_id, error = %e, "revive_session: failed to respawn pane");
                }
            }
        }

        let record = {
            let sessions = self.sessions.lock().unwrap();
            let panes = self.panes.lock().unwrap();
            Self::snapshot_persisted_record(&sessions, &panes, session_id)
        };
        self.save_persisted(record);

        tracing::info!(session_id = %session_id, pane_count = revived, "session revived");
        Ok(ReviveOutcome::Revived {
            pane_count: revived,
        })
    }

    /// Registers (or updates) `client_id`'s reported viewport for
    /// `window_id`, recomputes the window's effective size as the
    /// dimension-wise minimum across every attached client, and applies it:
    /// the new per-leaf geometry is computed *under* the lock (pure,
    /// in-memory, fast), then each affected `Pane::resize()` syscall runs
    /// *outside* the lock (see the `Engine` doc comment's locking
    /// discipline). Returns the new window size, or `None` if the window
    /// doesn't exist.
    pub fn report_viewport_and_recompute(
        &self,
        window_id: Uuid,
        client_id: u64,
        rows: u16,
        cols: u16,
    ) -> Option<(u16, u16)> {
        // Clamped to the same anti-corruption floor `split` enforces
        // (`MIN_PANE_ROWS`/`MIN_PANE_COLS`) — a client reporting a
        // degenerate viewport (0x0, or anything below the floor, whether
        // from a buggy client or a real terminal transiently reporting a
        // bogus size mid-resize) must never reach `Pane::resize()`: vt100
        // 0.16.2 panics internally on a resize that leaves the tracked
        // cursor position out of bounds, and that panic poisons the
        // pane's parser mutex permanently (found via v1.0.0-alpha.7's
        // manual release verification — every subsequent operation on
        // that pane, and transitively on Engine-wide state depending on
        // the call path, then fails forever until the daemon restarts).
        let rows = rows.max(MIN_PANE_ROWS);
        let cols = cols.max(MIN_PANE_COLS);
        self.viewports
            .lock()
            .unwrap()
            .entry(window_id)
            .or_default()
            .insert(client_id, (rows, cols));
        self.recompute_window_geometry(window_id)
    }

    pub fn unregister_viewport(&self, window_id: Uuid, client_id: u64) {
        if let Some(clients) = self.viewports.lock().unwrap().get_mut(&window_id) {
            clients.remove(&client_id);
        }
    }

    pub fn new_client_id(&self) -> u64 {
        self.next_client_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Re-derives and applies a window's effective size from its
    /// currently-registered viewports (or the default, if none are
    /// registered) — used both after `report_viewport_and_recompute` and
    /// after `unregister_viewport` (a detaching client's departure can
    /// itself change the dimension-wise minimum).
    pub fn recompute_window_geometry(&self, window_id: Uuid) -> Option<(u16, u16)> {
        let (rows, cols) = {
            let viewports = self.viewports.lock().unwrap();
            match viewports.get(&window_id) {
                Some(clients) if !clients.is_empty() => {
                    let rows = clients.values().map(|(r, _)| *r).min().unwrap();
                    let cols = clients.values().map(|(_, c)| *c).min().unwrap();
                    (rows, cols)
                }
                _ => (DEFAULT_ROWS, DEFAULT_COLS),
            }
        };

        // O(1) via the reverse index instead of scanning every session
        // (plan.md's Epic 1.4 follow-up — this runs on every Resize, one of
        // the hottest paths in the engine).
        let session_id = self
            .window_index
            .lock()
            .unwrap()
            .session_for_window(window_id)?;

        // Compute the new geometry under `sessions`, then release before
        // calling into any blocking Pane::resize() syscalls.
        let rects = {
            let mut sessions = self.sessions.lock().unwrap();
            let window = sessions
                .get_mut(&session_id)?
                .windows
                .iter_mut()
                .find(|w| w.id == window_id)?;
            window.rows = rows;
            window.cols = cols;
            window.layout.compute_geometry(rows, cols)
        };

        // Pair each live pane with *its own* computed rect by pane id — not
        // by position. `rects` (one entry per leaf, in tree order) may
        // contain leaves whose `PaneEntry` is `Dead` (e.g. a window left in
        // a mixed live/dead state by a partially-failed `revive_session`,
        // see `revive_session` above), so filtering `rects` down to just
        // the live ones and then zipping that filtered list back against
        // the *unfiltered* `rects` by index would misalign every live pane
        // after the first dead one, handing it a neighbor's geometry
        // instead of its own.
        let panes = self.panes.lock().unwrap();
        let live_pane_rects: Vec<(Arc<Pane>, crate::layout::PtyRect)> = rects
            .iter()
            .filter_map(|(pane_id, rect)| match panes.get(pane_id) {
                Some(PaneEntry::Live(pane)) => Some((pane.clone(), *rect)),
                _ => None,
            })
            .collect();
        drop(panes);

        for (pane, rect) in &live_pane_rects {
            if let Err(e) = pane.resize(rect.rows, rect.cols) {
                tracing::warn!(pane_id = %pane.id, error = %e, "window geometry recompute: pane resize failed");
            }
        }
        tracing::debug!(window_id = %window_id, rows, cols, "window geometry recomputed");
        self.notify_window_changed(window_id);

        let record = {
            let sessions = self.sessions.lock().unwrap();
            let panes = self.panes.lock().unwrap();
            Self::snapshot_persisted_record(&sessions, &panes, session_id)
        };
        self.save_persisted(record);

        Some((rows, cols))
    }

    /// Subscribes to structural/geometry change notifications for one
    /// window — each tick means "call `window_snapshot` again, something
    /// changed" (kept as a plain change signal rather than broadcasting the
    /// snapshot itself, so a slow subscriber just re-fetches current state
    /// rather than needing to replay a backlog of intermediate ones).
    pub fn watch_window(&self, window_id: Uuid) -> broadcast::Receiver<()> {
        self.window_watchers
            .lock()
            .unwrap()
            .entry(window_id)
            .or_insert_with(|| broadcast::channel(16).0)
            .subscribe()
    }

    fn notify_window_changed(&self, window_id: Uuid) {
        if let Some(tx) = self.window_watchers.lock().unwrap().get(&window_id) {
            let _ = tx.send(());
        }
    }

    /// The window's id, if any, that currently contains `pane_id` — used
    /// by `Attach` to know which window's viewport tracker a `Resize`
    /// message should update.
    pub fn window_id_for_pane(&self, pane_id: Uuid) -> Option<Uuid> {
        self.window_index.lock().unwrap().window_for_pane(pane_id)
    }

    /// `(pane_window.len(), window_session.len())` — direct index-size
    /// inspection for tests. `window_id_for_pane`/`recompute_window_geometry`
    /// alone can't prove a stale entry was actually removed: both re-check
    /// real `sessions`/`panes` state after the index lookup and return
    /// `None` either way, so a test asserting only through them can pass
    /// even if the underlying removal never ran (confirmed empirically: a
    /// prior version of the regression test below still passed with
    /// `WindowIndex::remove_window` deleted from `close_pane`/`kill_session`).
    #[cfg(test)]
    fn window_index_len(&self) -> (usize, usize) {
        let idx = self.window_index.lock().unwrap();
        (idx.pane_window.len(), idx.window_session.len())
    }

    pub fn window_snapshot(&self, window_id: Uuid) -> Option<WindowSnapshot> {
        let sessions = self.sessions.lock().unwrap();
        let panes = self.panes.lock().unwrap();
        sessions
            .values()
            .flat_map(|s| s.windows.iter())
            .find(|w| w.id == window_id)
            .map(|w| WindowSnapshot {
                id: w.id,
                name: w.name.clone(),
                layout: layout_to_snapshot(&w.layout, &panes),
            })
    }

    /// True O(1) lookup for a single session: a direct `HashMap::get`,
    /// unlike `list_sessions().into_iter().find(...)`, which rebuilds a
    /// full snapshot of every session under both locks just to discard all
    /// but one. This was the confirmed scale-feasibility bottleneck behind
    /// `create_session`'s handler — measured `CreateSession` latency
    /// climbing 5ms→20ms as session count went 100→900 (see
    /// `project_plans/stapler-squad-integration/research/scale-feasibility.md`).
    pub fn session_snapshot(&self, session_id: Uuid) -> Option<SessionSnapshot> {
        let sessions = self.sessions.lock().unwrap();
        let panes = self.panes.lock().unwrap();
        sessions
            .get(&session_id)
            .map(|session| session_to_snapshot(session, &panes))
    }

    /// How many clients are currently attached to a pane within this
    /// window — Story 6.1's `StatusBarModel` field, already tracked by
    /// ADR-004's viewport tracker (one entry per attached client), so no
    /// new bookkeeping is needed.
    pub fn attached_client_count(&self, window_id: Uuid) -> u32 {
        self.viewports
            .lock()
            .unwrap()
            .get(&window_id)
            .map(|clients| clients.len() as u32)
            .unwrap_or(0)
    }
}

fn session_to_snapshot(
    session: &SessionState,
    panes: &HashMap<Uuid, PaneEntry>,
) -> SessionSnapshot {
    let windows: Vec<WindowSnapshot> = session
        .windows
        .iter()
        .map(|w| WindowSnapshot {
            id: w.id,
            name: w.name.clone(),
            layout: layout_to_snapshot(&w.layout, panes),
        })
        .collect();
    let live = windows.iter().any(|w| window_has_live_pane(&w.layout));
    SessionSnapshot {
        id: session.id,
        name: session.name.clone(),
        windows,
        live,
    }
}

fn window_has_live_pane(layout: &LayoutSnapshot) -> bool {
    match layout {
        LayoutSnapshot::Leaf(info) => info.live,
        LayoutSnapshot::Split { children, .. } => {
            children.iter().any(|(c, _)| window_has_live_pane(c))
        }
    }
}

fn layout_to_snapshot(node: &LayoutNode, panes: &HashMap<Uuid, PaneEntry>) -> LayoutSnapshot {
    match node {
        LayoutNode::Leaf { pane_id } => {
            let (rows, cols, live, cwd) = match panes.get(pane_id) {
                Some(PaneEntry::Live(pane)) => {
                    let (rows, cols) = pane.size();
                    (rows, cols, !pane.is_exited(), pane.cwd.clone())
                }
                Some(PaneEntry::Dead(record)) => (
                    record.rows as u32,
                    record.cols as u32,
                    false,
                    record.cwd.clone(),
                ),
                None => (0, 0, false, String::new()),
            };
            LayoutSnapshot::Leaf(PaneInfo {
                id: *pane_id,
                rows,
                cols,
                live,
                cwd,
            })
        }
        LayoutNode::Split {
            orientation,
            children,
        } => LayoutSnapshot::Split {
            orientation: *orientation,
            children: children
                .iter()
                .map(|(c, ratio)| (layout_to_snapshot(c, panes), *ratio))
                .collect(),
        },
    }
}

/// A leaf's pane reference resolved while `sessions`/`panes` are locked —
/// cheap (an `Arc` clone or a small record clone), no per-pane lock
/// touched yet. See [`layout_to_handle`]/[`handle_to_layout_snapshot`].
enum PaneHandle {
    Live(Arc<Pane>),
    Dead(PersistedPaneRecord),
    Missing,
}

/// Mirrors [`LayoutNode`]'s shape but with each leaf resolved to a
/// [`PaneHandle`] instead of a bare `pane_id` — the intermediate value
/// `list_sessions` builds *under* the `sessions`/`panes` locks (Story 3.4
/// AC3), so the locks can be released before the per-pane `size()`/
/// `is_exited()` calls in [`handle_to_layout_snapshot`] run.
enum LayoutHandle {
    Leaf {
        pane_id: Uuid,
        handle: PaneHandle,
    },
    Split {
        orientation: Orientation,
        children: Vec<(LayoutHandle, f32)>,
    },
}

/// Intermediate, lock-free (once built) mirror of [`WindowSnapshot`] —
/// `list_sessions`' phase-1 output; see [`LayoutHandle`].
struct WindowHandle {
    id: Uuid,
    name: String,
    layout: LayoutHandle,
}

/// Intermediate, lock-free (once built) mirror of [`SessionSnapshot`] —
/// `list_sessions`' phase-1 output; see [`LayoutHandle`].
struct SessionHandle {
    id: Uuid,
    name: String,
    windows: Vec<WindowHandle>,
}

fn layout_to_handle(node: &LayoutNode, panes: &HashMap<Uuid, PaneEntry>) -> LayoutHandle {
    match node {
        LayoutNode::Leaf { pane_id } => {
            let handle = match panes.get(pane_id) {
                Some(PaneEntry::Live(pane)) => PaneHandle::Live(pane.clone()),
                Some(PaneEntry::Dead(record)) => PaneHandle::Dead(record.clone()),
                None => PaneHandle::Missing,
            };
            LayoutHandle::Leaf {
                pane_id: *pane_id,
                handle,
            }
        }
        LayoutNode::Split {
            orientation,
            children,
        } => LayoutHandle::Split {
            orientation: *orientation,
            children: children
                .iter()
                .map(|(c, ratio)| (layout_to_handle(c, panes), *ratio))
                .collect(),
        },
    }
}

/// Resolves each leaf's live size/exited status into a [`LayoutSnapshot`]
/// — deliberately called *outside* the `sessions`/`panes` locks (see
/// `list_sessions`). A live pane's `size()`/`is_exited()` locks that
/// pane's own internal parser mutex, which a concurrent `Pane::resize()`
/// may hold for a real (if brief) amount of wall-clock time while
/// resizing its `vt100` screen buffer; holding the Engine-wide
/// `sessions`/`panes` locks while waiting on that per-pane lock would
/// reintroduce, on the read side, the exact lock-held-across-a-blocking-
/// call hang class the write side (window resize itself) was already
/// designed to avoid (Story 3.4 AC3).
fn handle_to_layout_snapshot(handle: &LayoutHandle) -> LayoutSnapshot {
    match handle {
        LayoutHandle::Leaf { pane_id, handle } => {
            let (rows, cols, live, cwd) = match handle {
                PaneHandle::Live(pane) => {
                    let (rows, cols) = pane.size();
                    (rows, cols, !pane.is_exited(), pane.cwd.clone())
                }
                PaneHandle::Dead(record) => (
                    record.rows as u32,
                    record.cols as u32,
                    false,
                    record.cwd.clone(),
                ),
                PaneHandle::Missing => (0, 0, false, String::new()),
            };
            LayoutSnapshot::Leaf(PaneInfo {
                id: *pane_id,
                rows,
                cols,
                live,
                cwd,
            })
        }
        LayoutHandle::Split {
            orientation,
            children,
        } => LayoutSnapshot::Split {
            orientation: *orientation,
            children: children
                .iter()
                .map(|(c, ratio)| (handle_to_layout_snapshot(c), *ratio))
                .collect(),
        },
    }
}

fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // /bin/sh explicitly (not the default_shell() fallback) so these tests
    // don't depend on $SHELL or bash being present, matching pane.rs's test.
    fn sh() -> Option<String> {
        Some("/bin/sh".to_string())
    }

    /// Recursively collects every leaf `pane_id` in a window's layout, in
    /// tree order — used by tests that need to address specific panes
    /// after multiple splits, where the top-level `Split`'s `children`
    /// only exposes the immediate two.
    fn leaf_ids(node: &LayoutSnapshot) -> Vec<Uuid> {
        match node {
            LayoutSnapshot::Leaf(info) => vec![info.id],
            LayoutSnapshot::Split { children, .. } => children
                .iter()
                .flat_map(|(child, _)| leaf_ids(child))
                .collect(),
        }
    }

    fn sole_pane_id(snapshot: &SessionSnapshot) -> Uuid {
        match &snapshot.windows[0].layout {
            LayoutSnapshot::Leaf(info) => info.id,
            _ => panic!("expected a single-leaf window"),
        }
    }

    /// Regression test for a real crash found via v1.0.0-alpha.7's manual
    /// release verification: a client reporting a degenerate 0x0 viewport
    /// (e.g. `pty.fork()` in a test harness never calling `TIOCSWINSZ`, or
    /// any other buggy/misbehaving client) reached `Pane::resize(0, 0)`
    /// unclamped, which panicked inside `vt100` 0.16.2's internal cursor-
    /// position invariant and permanently poisoned that pane's parser
    /// mutex. `report_viewport_and_recompute` must clamp to the same
    /// `MIN_PANE_ROWS`/`MIN_PANE_COLS` anti-corruption floor `split`
    /// already enforces, so a degenerate report can never reach
    /// `Pane::resize()` at all.
    #[test]
    fn report_viewport_and_recompute_should_clamp_degenerate_zero_size_to_minimum_floor() {
        let engine = Engine::new();
        let id = engine
            .create_session("test".to_string(), sh(), None)
            .unwrap();
        let sessions = engine.list_sessions();
        let window_id = sessions.iter().find(|s| s.id == id).unwrap().windows[0].id;
        let pane_id = sole_pane_id(sessions.iter().find(|s| s.id == id).unwrap());

        let client = engine.new_client_id();
        let (rows, cols) = engine
            .report_viewport_and_recompute(window_id, client, 0, 0)
            .expect("window exists");

        assert_eq!(
            (rows, cols),
            (MIN_PANE_ROWS, MIN_PANE_COLS),
            "a 0x0 viewport report must clamp to the anti-corruption floor, not pass through"
        );

        let pane = match engine.pane_lookup(pane_id) {
            PaneLookup::Live(pane) => pane,
            _ => panic!("pane must still be Live — resize must not have crashed it"),
        };
        assert_eq!(
            pane.size(),
            (MIN_PANE_ROWS as u32, MIN_PANE_COLS as u32),
            "the pane itself must have been resized to the clamped floor, not 0x0"
        );
    }

    #[test]
    fn create_and_list_session() {
        let engine = Engine::new();
        let id = engine
            .create_session("test".to_string(), sh(), None)
            .unwrap();

        let sessions = engine.list_sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, id);
        assert_eq!(sessions[0].name, "test");
        assert!(sessions[0].live);
    }

    #[test]
    fn multiple_sessions_are_independent() {
        let engine = Engine::new();
        let id1 = engine
            .create_session("one".to_string(), sh(), None)
            .unwrap();
        let id2 = engine
            .create_session("two".to_string(), sh(), None)
            .unwrap();

        let sessions = engine.list_sessions();
        assert_eq!(sessions.len(), 2);
        let ids: Vec<Uuid> = sessions.iter().map(|s| s.id).collect();
        assert!(ids.contains(&id1));
        assert!(ids.contains(&id2));
        assert_ne!(id1, id2);

        let pane_ids: Vec<Uuid> = sessions.iter().map(sole_pane_id).collect();
        assert_ne!(pane_ids[0], pane_ids[1], "each session gets its own pane");
    }

    #[test]
    fn session_snapshot_should_return_matching_session_when_id_exists_among_many_sessions() {
        let engine = Engine::new();
        let id1 = engine
            .create_session("one".to_string(), sh(), None)
            .unwrap();
        let id2 = engine
            .create_session("two".to_string(), sh(), None)
            .unwrap();
        let id3 = engine
            .create_session("three".to_string(), sh(), None)
            .unwrap();

        let snapshot = engine
            .session_snapshot(id2)
            .expect("id2 was just created and must be found");
        assert_eq!(snapshot.id, id2);
        assert_eq!(snapshot.name, "two");
        assert!(snapshot.live);

        // Sanity: the other two ids are still present via list_sessions,
        // confirming session_snapshot(id2) didn't accidentally return the
        // wrong session or wipe out the others.
        let all_ids: Vec<Uuid> = engine.list_sessions().iter().map(|s| s.id).collect();
        assert!(all_ids.contains(&id1));
        assert!(all_ids.contains(&id3));
    }

    #[test]
    fn session_snapshot_should_return_none_when_session_id_is_unknown() {
        let engine = Engine::new();
        engine
            .create_session("one".to_string(), sh(), None)
            .unwrap();
        engine
            .create_session("two".to_string(), sh(), None)
            .unwrap();
        engine
            .create_session("three".to_string(), sh(), None)
            .unwrap();

        assert!(engine.session_snapshot(Uuid::new_v4()).is_none());
    }

    #[test]
    fn kill_session_removes_it() {
        let engine = Engine::new();
        let id = engine
            .create_session("test".to_string(), sh(), None)
            .unwrap();
        assert_eq!(engine.list_sessions().len(), 1);

        engine.kill_session(id).unwrap();
        assert_eq!(engine.list_sessions().len(), 0);
    }

    #[test]
    fn kill_session_unknown_id_errors() {
        let engine = Engine::new();
        let result = engine.kill_session(Uuid::new_v4());
        assert!(result.is_err());
    }

    #[test]
    fn pane_lookup_should_return_live_when_pane_process_still_running() {
        let engine = Engine::new();
        let id = engine
            .create_session("test".to_string(), sh(), None)
            .unwrap();
        let pane_id = sole_pane_id(
            &engine
                .list_sessions()
                .into_iter()
                .find(|s| s.id == id)
                .unwrap(),
        );

        assert!(matches!(engine.pane_lookup(pane_id), PaneLookup::Live(_)));
    }

    #[test]
    fn pane_lookup_should_return_unknown_when_pane_id_never_created() {
        let engine = Engine::new();
        assert!(matches!(
            engine.pane_lookup(Uuid::new_v4()),
            PaneLookup::Unknown
        ));
    }

    #[test]
    fn pane_lookup_should_return_dead_when_pane_process_exited_but_record_exists() {
        let engine = Engine::new();
        let id = engine
            .create_session("test".to_string(), sh(), None)
            .unwrap();
        let pane_id = sole_pane_id(
            &engine
                .list_sessions()
                .into_iter()
                .find(|s| s.id == id)
                .unwrap(),
        );
        let pane = match engine.pane_lookup(pane_id) {
            PaneLookup::Live(pane) => pane,
            _ => panic!("expected a freshly created pane to be Live"),
        };
        pane.write_input(b"exit\n").unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !pane.is_exited() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(pane.is_exited(), "pane should have exited by now");

        assert!(matches!(engine.pane_lookup(pane.id), PaneLookup::Dead));
    }

    #[test]
    fn split_pane_should_produce_two_leaf_window() {
        let engine = Engine::new();
        let id = engine
            .create_session("test".to_string(), sh(), None)
            .unwrap();
        let pane_id = sole_pane_id(
            &engine
                .list_sessions()
                .into_iter()
                .find(|s| s.id == id)
                .unwrap(),
        );

        let snapshot = engine
            .split_pane(pane_id, Orientation::Horizontal, sh())
            .unwrap();
        match &snapshot.windows[0].layout {
            LayoutSnapshot::Split { children, .. } => assert_eq!(children.len(), 2),
            _ => panic!("expected split_pane to produce a Split window layout"),
        }
    }

    #[test]
    fn split_pane_unknown_pane_id_errors() {
        let engine = Engine::new();
        let err = engine
            .split_pane(Uuid::new_v4(), Orientation::Horizontal, sh())
            .unwrap_err();
        assert!(matches!(err, EngineError::PaneNotFound(_)));
    }

    #[test]
    fn close_pane_should_collapse_split_when_sibling_closes() {
        let engine = Engine::new();
        let id = engine
            .create_session("test".to_string(), sh(), None)
            .unwrap();
        let pane_a = sole_pane_id(
            &engine
                .list_sessions()
                .into_iter()
                .find(|s| s.id == id)
                .unwrap(),
        );
        let snapshot = engine
            .split_pane(pane_a, Orientation::Horizontal, sh())
            .unwrap();
        let pane_b = match &snapshot.windows[0].layout {
            LayoutSnapshot::Split { children, .. } => match &children[1].0 {
                LayoutSnapshot::Leaf(info) => info.id,
                _ => panic!("expected leaf"),
            },
            _ => panic!("expected split"),
        };

        let outcome = engine.close_pane(pane_b).unwrap();
        assert!(outcome.window_closed.is_none());
        assert!(outcome.session_closed.is_none());
        let session = outcome.session.unwrap();
        assert!(matches!(session.windows[0].layout, LayoutSnapshot::Leaf(_)));
    }

    #[test]
    fn close_pane_should_close_session_when_last_pane_in_last_window() {
        let engine = Engine::new();
        let id = engine
            .create_session("test".to_string(), sh(), None)
            .unwrap();
        let pane_id = sole_pane_id(
            &engine
                .list_sessions()
                .into_iter()
                .find(|s| s.id == id)
                .unwrap(),
        );

        let outcome = engine.close_pane(pane_id).unwrap();
        assert!(outcome.window_closed.is_some());
        assert!(outcome.session_closed.is_some());
        assert!(outcome.session.is_none());
        assert_eq!(engine.list_sessions().len(), 0);
    }

    #[test]
    fn create_window_adds_a_second_window() {
        let engine = Engine::new();
        let id = engine
            .create_session("test".to_string(), sh(), None)
            .unwrap();
        let snapshot = engine.create_window(id, sh()).unwrap();
        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(snapshot.windows[1].name, "1");
    }

    #[test]
    fn report_viewport_should_apply_dimension_wise_minimum_across_two_clients() {
        let engine = Engine::new();
        let id = engine
            .create_session("test".to_string(), sh(), None)
            .unwrap();
        let window_id = engine
            .list_sessions()
            .into_iter()
            .find(|s| s.id == id)
            .unwrap()
            .windows[0]
            .id;

        let c1 = engine.new_client_id();
        let c2 = engine.new_client_id();
        engine.report_viewport_and_recompute(window_id, c1, 40, 100);
        let (rows, cols) = engine
            .report_viewport_and_recompute(window_id, c2, 20, 200)
            .unwrap();
        assert_eq!(
            (rows, cols),
            (20, 100),
            "effective size must be the dimension-wise minimum"
        );
    }

    /// Regression test for the resize-geometry misassignment bug: a window
    /// with a mix of `PaneEntry::Live`/`PaneEntry::Dead` leaves (reachable
    /// via a partially-failed `revive_session`, see `revive_session` above
    /// at engine.rs:562-626, where some panes respawn successfully and
    /// others don't) must still resize each *live* pane to *its own*
    /// computed rect, never a neighbor's — `recompute_window_geometry`
    /// used to build the live-pane list by filtering `rects` down to
    /// `Live` entries and then zip that filtered list against the
    /// unfiltered `rects` by position, which misaligns as soon as a dead
    /// leaf precedes a live one in tree order.
    #[test]
    fn recompute_window_geometry_should_match_live_pane_by_id_not_position_when_window_has_dead_and_live_panes(
    ) {
        let engine = Engine::new();
        let id = engine
            .create_session("test".to_string(), sh(), None)
            .unwrap();
        let pane_a = sole_pane_id(
            &engine
                .list_sessions()
                .into_iter()
                .find(|s| s.id == id)
                .unwrap(),
        );
        let snapshot = engine
            .split_pane(pane_a, Orientation::Horizontal, sh())
            .unwrap();
        let window_id = snapshot.windows[0].id;
        let pane_b = match &snapshot.windows[0].layout {
            LayoutSnapshot::Split { children, .. } => {
                let leaf_id = |node: &LayoutSnapshot| match node {
                    LayoutSnapshot::Leaf(info) => info.id,
                    _ => panic!("expected a leaf child"),
                };
                assert_eq!(
                    leaf_id(&children[0].0),
                    pane_a,
                    "split target stays the first child"
                );
                leaf_id(&children[1].0)
            }
            _ => panic!("expected split_pane to produce a Split window layout"),
        };

        // Simulate the reachable mixed-state a partially-failed
        // `revive_session` leaves behind: pane_a's `PaneEntry` becomes
        // `Dead` while pane_b's stays `Live` — both are still leaves of
        // the same window's layout tree, so `compute_geometry` still
        // produces one rect per leaf (pane_a's and pane_b's), but only
        // pane_b's `PaneEntry` is `Live`.
        {
            let mut panes = engine.panes.lock().unwrap();
            panes.insert(
                pane_a,
                PaneEntry::Dead(PersistedPaneRecord {
                    pane_id: pane_a,
                    command: "/bin/sh".to_string(),
                    cwd: "/".to_string(),
                    rows: 24,
                    cols: 40,
                    exit_code: None,
                }),
            );
        }

        // An odd column count so the two leaves (equal 0.5 ratio) get
        // visibly different widths — pane_a (first child) computes to 40
        // cols, pane_b (second/remainder child) computes to 41 — making a
        // position-vs-id mismatch observable via pane_b's actual size.
        let client = engine.new_client_id();
        engine
            .report_viewport_and_recompute(window_id, client, 24, 81)
            .unwrap();

        let pane_b_live = match engine.pane_lookup(pane_b) {
            PaneLookup::Live(pane) => pane,
            _ => panic!("pane_b must still be Live — only pane_a was marked Dead"),
        };
        let (rows, cols) = pane_b_live.size();
        assert_eq!(
            (rows, cols),
            (24, 41),
            "pane_b (the live, second-child leaf) must be resized to its \
             own computed rect, not pane_a's (the dead, first-child leaf) \
             — a position-based zip would misassign pane_a's 40-col rect \
             to pane_b here"
        );
    }

    /// Story 4.2 AC2 — the concurrency regression: a slow persistence
    /// backend's `save()` must never block an unrelated `Engine` read
    /// (here, `list_sessions`), because the lock is dropped before
    /// `self.persistence.save()` is ever called (see
    /// `Engine::snapshot_persisted_record`/`save_persisted`).
    #[test]
    fn engine_list_sessions_should_not_block_when_slow_mock_persistence_backend_is_saving() {
        struct SlowMockPersistenceBackend {
            saving: Arc<std::sync::atomic::AtomicBool>,
        }
        impl PersistenceBackend for SlowMockPersistenceBackend {
            fn save(&self, _record: &PersistedSessionRecord) -> Result<()> {
                self.saving.store(true, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(300));
                self.saving
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
            fn load_all(&self) -> Vec<PersistedSessionRecord> {
                Vec::new()
            }
            fn delete(&self, _session_id: Uuid) {}
        }

        let saving = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let engine = Arc::new(Engine::with_persistence(Box::new(
            SlowMockPersistenceBackend {
                saving: saving.clone(),
            },
        )));

        // Trigger a save on a background thread (create_session calls
        // save_persisted internally) and wait until it's actually
        // in-flight before proceeding.
        let engine_for_save = engine.clone();
        let save_thread = std::thread::spawn(move || {
            engine_for_save
                .create_session("slow".to_string(), sh(), None)
                .unwrap();
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !saving.load(std::sync::atomic::Ordering::SeqCst)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            saving.load(std::sync::atomic::Ordering::SeqCst),
            "the slow save should be in flight by now"
        );

        // While the slow save is in flight, an unrelated operation must
        // complete quickly, not block on the save.
        let unrelated_id = engine
            .create_session("unrelated".to_string(), sh(), None)
            .unwrap();
        let start = std::time::Instant::now();
        let sessions = engine.list_sessions();
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(100),
            "list_sessions took {elapsed:?} — it must not block on the slow persistence save"
        );
        assert!(sessions.iter().any(|s| s.id == unrelated_id));

        save_thread.join().unwrap();
    }

    /// Story 3.4 AC3 — the resize-path sibling of the AC2 persistence
    /// concurrency test above. Proves `ListSessions` on an *unrelated*
    /// session is not blocked while a window's `Pane::resize()` syscalls
    /// are in flight for a *different* session, using real ptys and real
    /// timing rather than a mock (plan.md §6 Unresolved Question #11
    /// explicitly rejected building a `SlowMockPane`/`PtyBackend`-style
    /// test double solely for this one assertion — that would be a real
    /// abstraction cost with no other caller).
    ///
    /// To get a genuinely slow (not mocked) `Pane::resize()`, this test
    /// resizes a real 4-pane window to a very large size (1200 rows x
    /// 1200 cols per leaf): the underlying `portable-pty` ioctl is fast,
    /// but `vt100::Parser`'s screen reallocation at that cell count is
    /// not — calibrated on commodity hardware to take several hundred
    /// milliseconds across the 4 real panes, comfortably longer than any
    /// `list_sessions` call should ever take. The assertions are
    /// self-calibrating (comparing `list_sessions`'s latency against the
    /// resize's own measured duration, not a fixed wall-clock guess) so
    /// the test isn't tied to one machine's absolute speed.
    #[test]
    fn list_sessions_should_not_block_on_unrelated_session_while_window_resize_syscalls_in_flight()
    {
        let engine = Arc::new(Engine::new());

        // Build a 4-real-pane window on the session whose resize will be
        // slow.
        let resize_session_id = engine
            .create_session("resize-target".to_string(), sh(), None)
            .unwrap();
        let resize_session_snapshot = |engine: &Engine| {
            engine
                .list_sessions()
                .into_iter()
                .find(|s| s.id == resize_session_id)
                .unwrap()
        };
        let window_id = resize_session_snapshot(&engine).windows[0].id;
        let pane0 = sole_pane_id(&resize_session_snapshot(&engine));

        // Give the window room (2000 cols) to split three times without
        // hitting the structural minimum-width floor.
        let setup_client = engine.new_client_id();
        engine
            .report_viewport_and_recompute(window_id, setup_client, 24, 2000)
            .unwrap();
        engine
            .split_pane(pane0, Orientation::Horizontal, sh())
            .unwrap();
        let first_leaf = leaf_ids(&resize_session_snapshot(&engine).windows[0].layout)[0];
        engine
            .split_pane(first_leaf, Orientation::Horizontal, sh())
            .unwrap();
        let first_leaf = leaf_ids(&resize_session_snapshot(&engine).windows[0].layout)[0];
        engine
            .split_pane(first_leaf, Orientation::Horizontal, sh())
            .unwrap();
        assert_eq!(
            leaf_ids(&resize_session_snapshot(&engine).windows[0].layout).len(),
            4,
            "expected 4 real panes in the resize-target window"
        );

        // An unrelated session whose ListSessions visibility must not be
        // affected by the resize below.
        let other_session_id = engine
            .create_session("unrelated".to_string(), sh(), None)
            .unwrap();

        // Trigger the real, genuinely-slow window resize on a background
        // thread: 1200 rows x 4800 cols split across 4 leaves is ~1200 x
        // 1200 cells each. Effective window size is the dimension-wise
        // *minimum* across every registered viewport (ADR-004), so this
        // reuses `setup_client`'s id (rather than registering a second,
        // smaller-in-one-dimension client) to actually grow the window
        // instead of being clamped back down to the 24x2000 setup size.
        let engine_for_resize = engine.clone();
        let resize_start = std::time::Instant::now();
        let resize_thread = std::thread::spawn(move || {
            engine_for_resize
                .report_viewport_and_recompute(window_id, setup_client, 1200, 4800)
                .unwrap();
        });

        // While the resize is in flight, hammer `list_sessions` on the
        // *unrelated* session and record the slowest single call.
        let mut max_list_sessions_latency = std::time::Duration::ZERO;
        let mut list_sessions_calls = 0u32;
        while !resize_thread.is_finished() {
            let call_start = std::time::Instant::now();
            let sessions = engine.list_sessions();
            let elapsed = call_start.elapsed();
            max_list_sessions_latency = max_list_sessions_latency.max(elapsed);
            list_sessions_calls += 1;
            assert!(
                sessions.iter().any(|s| s.id == other_session_id),
                "the unrelated session must still be visible throughout"
            );
        }
        resize_thread.join().unwrap();
        let resize_duration = resize_start.elapsed();

        assert!(
            resize_duration >= std::time::Duration::from_millis(50),
            "the calibrated resize should take a real, measurable amount \
             of wall-clock time (got {resize_duration:?}) — if this is \
             failing, the resize dimensions in this test need to be made \
             larger for this hardware so the test can actually exercise \
             the concurrency property"
        );
        assert!(
            list_sessions_calls >= 2,
            "expected list_sessions to be called at least twice while the \
             resize was in flight (got {list_sessions_calls}) — the \
             resize finished too fast to meaningfully exercise concurrent \
             access"
        );
        assert!(
            max_list_sessions_latency < resize_duration / 2,
            "list_sessions on an unrelated session must not be blocked by \
             the window resize's syscalls: slowest observed list_sessions \
             call was {max_list_sessions_latency:?} while the resize \
             itself took {resize_duration:?} — list_sessions should \
             complete in a small fraction of that, not scale with it"
        );
    }

    #[test]
    fn revive_session_should_respawn_ptys_matching_persisted_layout_shape_and_mark_live() {
        let persist_dir =
            std::env::temp_dir().join(format!("tymux-revive-test-{}", Uuid::new_v4()));
        let backend = crate::persistence::FsPersistenceBackend::new(persist_dir.clone()).unwrap();
        let engine = Engine::with_persistence(Box::new(backend));
        let id = engine
            .create_session("test".to_string(), sh(), None)
            .unwrap();
        let pane_id = sole_pane_id(
            &engine
                .list_sessions()
                .into_iter()
                .find(|s| s.id == id)
                .unwrap(),
        );
        engine
            .split_pane(pane_id, Orientation::Horizontal, sh())
            .unwrap();

        // Simulate a daemon restart: reload from the persisted records
        // into a fresh Engine, rather than reusing the live one.
        let backend2 = crate::persistence::FsPersistenceBackend::new(persist_dir.clone()).unwrap();
        let records = backend2.load_all();
        assert_eq!(records.len(), 1);
        let fresh_engine = Engine::with_persistence(Box::new(backend2));
        fresh_engine.load_persisted(records);

        let dead_snapshot = fresh_engine
            .list_sessions()
            .into_iter()
            .find(|s| s.id == id)
            .unwrap();
        assert!(
            !dead_snapshot.live,
            "freshly loaded session must be dead-flagged"
        );
        assert!(
            matches!(
                dead_snapshot.windows[0].layout,
                LayoutSnapshot::Split { .. }
            ),
            "the split shape must survive the reload"
        );

        let outcome = fresh_engine.revive_session(id).unwrap();
        assert!(matches!(outcome, ReviveOutcome::Revived { pane_count: 2 }));

        let revived_snapshot = fresh_engine
            .list_sessions()
            .into_iter()
            .find(|s| s.id == id)
            .unwrap();
        assert!(revived_snapshot.live);
        assert!(
            matches!(
                revived_snapshot.windows[0].layout,
                LayoutSnapshot::Split { .. }
            ),
            "the split shape must be preserved through revival"
        );

        std::fs::remove_dir_all(&persist_dir).ok();
    }

    #[test]
    fn daemon_restart_should_leave_session_dead_flagged_when_revive_never_called() {
        let persist_dir =
            std::env::temp_dir().join(format!("tymux-revive-test-{}", Uuid::new_v4()));
        let backend = crate::persistence::FsPersistenceBackend::new(persist_dir.clone()).unwrap();
        let engine = Engine::with_persistence(Box::new(backend));
        let id = engine
            .create_session("test".to_string(), sh(), None)
            .unwrap();

        for _ in 0..2 {
            let backend =
                crate::persistence::FsPersistenceBackend::new(persist_dir.clone()).unwrap();
            let records = backend.load_all();
            let fresh = Engine::with_persistence(Box::new(backend));
            fresh.load_persisted(records);
            let snapshot = fresh
                .list_sessions()
                .into_iter()
                .find(|s| s.id == id)
                .unwrap();
            assert!(
                !snapshot.live,
                "a bare restart without revive must leave the session dead"
            );
        }

        std::fs::remove_dir_all(&persist_dir).ok();
    }

    #[test]
    fn revive_session_on_already_live_session_returns_already_live_outcome() {
        let engine = Engine::new();
        let id = engine
            .create_session("test".to_string(), sh(), None)
            .unwrap();
        let outcome = engine.revive_session(id).unwrap();
        assert_eq!(outcome, ReviveOutcome::AlreadyLive);
    }

    /// Regression coverage for `WindowIndex` (`pane_window`/`window_session`),
    /// added to make `window_id_for_pane`/`recompute_window_geometry` O(1)
    /// (plan.md's Epic 1.4 follow-up). The risk these indices add isn't
    /// speed — a throwaway diagnostic already confirmed `window_id_for_pane`
    /// stays flat at ~1µs from n=100 to n=900 sessions, unlike the ~1µs→7-13µs
    /// growth measured pre-fix — it's staying in sync with `sessions` across
    /// every mutation site: `create_session`, `create_window`, `split_pane`,
    /// `close_pane` (all three outcomes: collapse, window-closes,
    /// session-closes), and `kill_session`. `load_persisted` is covered
    /// separately below (it needs a persistence backend this test doesn't
    /// use).
    ///
    /// Three sessions are kept alive throughout specifically so a bug that
    /// only shows up with >1 session in play — a stale entry, or one
    /// session's mutation corrupting another's — can't hide: `session_b`'s
    /// entries are re-checked after *every* mutation on `session_a`, not
    /// just once at the end (an earlier version of this test only checked
    /// `session_b` at the very end, which a `kill_session`/`close_pane` bug
    /// affecting only the middle steps would have passed straight through).
    /// Removal is also asserted directly via `window_index_len()`, not just
    /// through `window_id_for_pane`/`recompute_window_geometry` — those two
    /// independently re-check real `sessions`/`panes` state and return
    /// `None` either way, so asserting only through them can't actually
    /// prove a removal ran (confirmed empirically: this test still passed
    /// with `WindowIndex::remove_window` deleted from `close_pane`).
    #[test]
    fn window_indices_should_stay_correct_across_every_mutation_site_with_multiple_sessions() {
        let engine = Engine::new();

        let session_a = engine.create_session("a".to_string(), sh(), None).unwrap();
        let session_b = engine.create_session("b".to_string(), sh(), None).unwrap();
        let session_c = engine.create_session("c".to_string(), sh(), None).unwrap();
        let pane_a1 = sole_pane_id(&engine.session_snapshot(session_a).unwrap());
        let pane_b1 = sole_pane_id(&engine.session_snapshot(session_b).unwrap());
        let pane_c1 = sole_pane_id(&engine.session_snapshot(session_c).unwrap());
        let window_a1 = engine.window_id_for_pane(pane_a1).unwrap();
        let window_b1 = engine.window_id_for_pane(pane_b1).unwrap();
        assert_ne!(window_a1, window_b1);
        assert!(engine.recompute_window_geometry(window_a1).is_some());
        assert!(engine.recompute_window_geometry(window_b1).is_some());
        assert_eq!(engine.window_index_len(), (3, 3));

        let assert_session_b_untouched = |engine: &Engine| {
            assert_eq!(
                engine.window_id_for_pane(pane_b1),
                Some(window_b1),
                "an unrelated session_a mutation must not disturb session_b"
            );
            assert!(engine.recompute_window_geometry(window_b1).is_some());
        };

        // create_window: a second window in session_a must resolve to
        // session_a's own geometry recompute, not session_b's, and must not
        // disturb session_b's own entries.
        let snapshot = engine.create_window(session_a, sh()).unwrap();
        let window_a2 = snapshot.windows[1].id;
        assert_ne!(window_a2, window_a1);
        assert!(engine.recompute_window_geometry(window_a2).is_some());
        assert_eq!(engine.window_index_len(), (4, 4));
        assert_session_b_untouched(&engine);

        // split_pane: the new pane must resolve to the *same* window as the
        // pane it split from, not get lost or attributed elsewhere.
        let snapshot = engine
            .split_pane(pane_a1, Orientation::Horizontal, sh())
            .unwrap();
        let pane_a1_leaves = leaf_ids(&snapshot.windows[0].layout);
        assert_eq!(pane_a1_leaves.len(), 2);
        for leaf in &pane_a1_leaves {
            assert_eq!(engine.window_id_for_pane(*leaf), Some(window_a1));
        }
        assert_eq!(engine.window_index_len(), (5, 4));
        assert_session_b_untouched(&engine);

        // close_pane on one half of the split: window survives (collapses),
        // so window_a1 must still resolve; the closed pane must not, and
        // the pane-side index must actually shrink (not just "look right"
        // through the redundant real-state check).
        let closed_pane = pane_a1_leaves[1];
        let outcome = engine.close_pane(closed_pane).unwrap();
        assert!(outcome.window_closed.is_none());
        assert_eq!(engine.window_id_for_pane(closed_pane), None);
        assert_eq!(engine.window_id_for_pane(pane_a1), Some(window_a1));
        assert!(engine.recompute_window_geometry(window_a1).is_some());
        assert_eq!(engine.window_index_len(), (4, 4));
        assert_session_b_untouched(&engine);

        // close_pane on window_a1's last pane: the window itself is now
        // gone, but session_a survives (window_a2 still there) — window_a1
        // must be purged from *both* maps (pane and window side), window_a2
        // must be untouched.
        let outcome = engine.close_pane(pane_a1).unwrap();
        assert!(outcome.window_closed.is_some());
        assert!(outcome.session_closed.is_none());
        assert_eq!(engine.window_id_for_pane(pane_a1), None);
        assert_eq!(engine.recompute_window_geometry(window_a1), None);
        assert!(engine.recompute_window_geometry(window_a2).is_some());
        assert_eq!(engine.window_index_len(), (3, 3));
        assert_session_b_untouched(&engine);

        // close_pane on session_c's only pane: session_closed must fire
        // (the case the other close_pane calls above don't reach), and
        // both maps must lose exactly session_c's one entry each — proof
        // that the session-closes branch's index cleanup, which reuses
        // window_closed's cleanup rather than a separate per-window loop
        // (see close_pane's own comment), actually runs.
        let outcome = engine.close_pane(pane_c1).unwrap();
        assert!(outcome.session_closed.is_some());
        assert_eq!(engine.window_id_for_pane(pane_c1), None);
        assert_eq!(engine.window_index_len(), (2, 2));
        assert_session_b_untouched(&engine);

        // kill_session on session_b: every window/pane belonging to it must
        // be purged from both indices; session_a's own entries (window_a2,
        // its pane) must be completely unaffected.
        engine.kill_session(session_b).unwrap();
        assert_eq!(engine.window_id_for_pane(pane_b1), None);
        assert_eq!(engine.recompute_window_geometry(window_b1), None);
        assert!(engine.recompute_window_geometry(window_a2).is_some());
        assert_eq!(engine.window_index_len(), (1, 1));
    }

    /// `load_persisted` is the one `WindowIndex` mutation site the test
    /// above can't reach (it needs a real persistence backend, not
    /// `Engine::new()`'s `NullPersistenceBackend`) — a gap the original
    /// version of that test's own doc comment claimed didn't exist.
    /// Mirrors `revive_session_should_respawn_ptys_matching_persisted_layout_shape_and_mark_live`'s
    /// restart-simulation shape, but asserts on the index immediately after
    /// `load_persisted`, before `revive_session` ever runs — `revive_session`
    /// never touches `WindowIndex` (it respawns panes under their existing
    /// ids), so this is the only path that actually exercises
    /// `load_persisted`'s own index-population code.
    #[test]
    fn load_persisted_should_populate_window_index_before_any_revive() {
        let persist_dir = std::env::temp_dir().join(format!(
            "tymux-load-persisted-index-test-{}",
            Uuid::new_v4()
        ));
        let backend = crate::persistence::FsPersistenceBackend::new(persist_dir.clone()).unwrap();
        let engine = Engine::with_persistence(Box::new(backend));
        let id = engine
            .create_session("test".to_string(), sh(), None)
            .unwrap();
        let pane_id = sole_pane_id(&engine.session_snapshot(id).unwrap());
        engine
            .split_pane(pane_id, Orientation::Horizontal, sh())
            .unwrap();

        let backend2 = crate::persistence::FsPersistenceBackend::new(persist_dir.clone()).unwrap();
        let records = backend2.load_all();
        let fresh_engine = Engine::with_persistence(Box::new(backend2));
        fresh_engine.load_persisted(records);

        // Not revived yet — panes are PaneEntry::Dead — but the index must
        // already be populated: window_id_for_pane needs no live pane.
        let dead_snapshot = fresh_engine.session_snapshot(id).unwrap();
        let leaves = leaf_ids(&dead_snapshot.windows[0].layout);
        assert_eq!(leaves.len(), 2, "the split shape must survive the reload");
        let window_id = fresh_engine.window_id_for_pane(leaves[0]).unwrap();
        assert_eq!(
            fresh_engine.window_id_for_pane(leaves[1]),
            Some(window_id),
            "both split leaves must resolve to the same window"
        );
        assert!(fresh_engine.recompute_window_geometry(window_id).is_some());
        assert_eq!(fresh_engine.window_index_len(), (2, 1));

        std::fs::remove_dir_all(&persist_dir).ok();
    }
}
