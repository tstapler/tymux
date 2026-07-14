use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::layout::{LayoutNode, Orientation, RemoveOutcome, MIN_PANE_COLS, MIN_PANE_ROWS};
use crate::pane::Pane;

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

/// `Engine.panes`' value type. `Dead` carries nothing yet — once Epic 4's
/// persistence lands it will carry a `PersistedPaneRecord` payload (see
/// `plan.md`'s `PaneEntry` glossary entry); for now "dead" just means the
/// pane's process is known to have exited (whether by natural exit or by
/// `kill_session`/`close_pane` explicitly killing it before removal).
pub enum PaneEntry {
    Live(Arc<Pane>),
    Dead,
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
    BelowMinimumSize { rows: u16, cols: u16 },
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
}

impl Default for Engine {
    fn default() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            panes: Mutex::new(HashMap::new()),
            viewports: Mutex::new(HashMap::new()),
            next_client_id: AtomicU64::new(1),
            window_watchers: Mutex::new(HashMap::new()),
        }
    }
}

impl Engine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create_session(&self, name: String, command: Option<String>) -> Result<Uuid> {
        let shell = command.unwrap_or_else(default_shell);
        let pane = Pane::spawn(&shell, DEFAULT_ROWS, DEFAULT_COLS)?;
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
        Ok(id)
    }

    pub fn list_sessions(&self) -> Vec<SessionSnapshot> {
        let sessions = self.sessions.lock().unwrap();
        let panes = self.panes.lock().unwrap();
        sessions
            .values()
            .map(|s| session_to_snapshot(s, &panes))
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
        Ok(())
    }

    /// Finds a pane by id across every session — the pane namespace is
    /// flat (a `pane_id` already uniquely identifies its pane regardless
    /// of which session/window it lives in).
    pub fn pane_lookup(&self, pane_id: Uuid) -> PaneLookup {
        match self.panes.lock().unwrap().get(&pane_id) {
            None => PaneLookup::Unknown,
            Some(PaneEntry::Dead) => PaneLookup::Dead,
            Some(PaneEntry::Live(pane)) if pane.is_exited() => PaneLookup::Dead,
            Some(PaneEntry::Live(pane)) => PaneLookup::Live(pane.clone()),
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

        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions
            .values_mut()
            .find(|s| s.windows.iter().any(|w| w.layout.contains(pane_id)));
        let Some(session) = session else {
            return Err(EngineError::PaneNotFound(pane_id));
        };
        let window = session
            .windows
            .iter_mut()
            .find(|w| w.layout.contains(pane_id))
            .expect("session was found by this exact predicate");

        match window
            .layout
            .split(pane_id, orientation, new_pane_id, window.rows, window.cols)
        {
            Ok(()) => {}
            Err(crate::layout::LayoutError::BelowMinimumSize { rows, cols }) => {
                return Err(EngineError::BelowMinimumSize { rows, cols });
            }
            Err(crate::layout::LayoutError::PaneNotFound { pane_id }) => {
                return Err(EngineError::PaneNotFound(pane_id));
            }
        }

        let session_id = session.id;
        let window_id = window.id;
        drop(sessions);

        self.panes
            .lock()
            .unwrap()
            .insert(new_pane_id, PaneEntry::Live(new_pane));

        let sessions = self.sessions.lock().unwrap();
        let panes = self.panes.lock().unwrap();
        let snapshot = session_to_snapshot(&sessions[&session_id], &panes);
        drop(sessions);
        drop(panes);
        self.notify_window_changed(window_id);
        Ok(snapshot)
    }

    /// Closes one pane. If it was its window's last pane, the window
    /// itself closes; if that was also the session's last window, the
    /// whole session closes (a semantic `KillSession`) — see
    /// `ClosePaneOutcome`.
    pub fn close_pane(&self, pane_id: Uuid) -> Result<ClosePaneOutcome, EngineError> {
        let mut sessions = self.sessions.lock().unwrap();
        let session_id = sessions
            .values()
            .find(|s| s.windows.iter().any(|w| w.layout.contains(pane_id)))
            .map(|s| s.id)
            .ok_or(EngineError::PaneNotFound(pane_id))?;

        let session = sessions.get_mut(&session_id).unwrap();
        let window_idx = session
            .windows
            .iter()
            .position(|w| w.layout.contains(pane_id))
            .unwrap();

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

        let (removed_session, final_snapshot) = if session_closed.is_some() {
            (sessions.remove(&session_id), None)
        } else {
            let snapshot = {
                let panes = self.panes.lock().unwrap();
                Some(session_to_snapshot(&sessions[&session_id], &panes))
            };
            (None, snapshot)
        };
        drop(sessions);

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

        let panes = self.panes.lock().unwrap();
        Ok(session_to_snapshot(&sessions[&session_id], &panes))
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

        // Compute the new geometry under `sessions`, then release before
        // calling into any blocking Pane::resize() syscalls.
        let rects = {
            let mut sessions = self.sessions.lock().unwrap();
            let window = sessions
                .values_mut()
                .flat_map(|s| s.windows.iter_mut())
                .find(|w| w.id == window_id)?;
            window.rows = rows;
            window.cols = cols;
            window.layout.compute_geometry(rows, cols)
        };

        let panes = self.panes.lock().unwrap();
        let live_panes: Vec<Arc<Pane>> = rects
            .iter()
            .filter_map(|(pane_id, _)| match panes.get(pane_id) {
                Some(PaneEntry::Live(pane)) => Some(pane.clone()),
                _ => None,
            })
            .collect();
        drop(panes);

        for (pane, (_, rect)) in live_panes.iter().zip(rects.iter()) {
            if let Err(e) = pane.resize(rect.rows, rect.cols) {
                tracing::warn!(pane_id = %pane.id, error = %e, "window geometry recompute: pane resize failed");
            }
        }
        tracing::debug!(window_id = %window_id, rows, cols, "window geometry recomputed");
        self.notify_window_changed(window_id);

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
        self.sessions
            .lock()
            .unwrap()
            .values()
            .flat_map(|s| s.windows.iter())
            .find(|w| w.layout.contains(pane_id))
            .map(|w| w.id)
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
            let (rows, cols, live) = match panes.get(pane_id) {
                Some(PaneEntry::Live(pane)) => {
                    let (rows, cols) = pane.size();
                    (rows, cols, !pane.is_exited())
                }
                Some(PaneEntry::Dead) | None => (0, 0, false),
            };
            LayoutSnapshot::Leaf(PaneInfo {
                id: *pane_id,
                rows,
                cols,
                live,
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

    fn sole_pane_id(snapshot: &SessionSnapshot) -> Uuid {
        match &snapshot.windows[0].layout {
            LayoutSnapshot::Leaf(info) => info.id,
            _ => panic!("expected a single-leaf window"),
        }
    }

    #[test]
    fn create_and_list_session() {
        let engine = Engine::new();
        let id = engine.create_session("test".to_string(), sh()).unwrap();

        let sessions = engine.list_sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, id);
        assert_eq!(sessions[0].name, "test");
        assert!(sessions[0].live);
    }

    #[test]
    fn multiple_sessions_are_independent() {
        let engine = Engine::new();
        let id1 = engine.create_session("one".to_string(), sh()).unwrap();
        let id2 = engine.create_session("two".to_string(), sh()).unwrap();

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
    fn kill_session_removes_it() {
        let engine = Engine::new();
        let id = engine.create_session("test".to_string(), sh()).unwrap();
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
        let id = engine.create_session("test".to_string(), sh()).unwrap();
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
        let id = engine.create_session("test".to_string(), sh()).unwrap();
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
        let id = engine.create_session("test".to_string(), sh()).unwrap();
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
        let id = engine.create_session("test".to_string(), sh()).unwrap();
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
        let id = engine.create_session("test".to_string(), sh()).unwrap();
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
        let id = engine.create_session("test".to_string(), sh()).unwrap();
        let snapshot = engine.create_window(id, sh()).unwrap();
        assert_eq!(snapshot.windows.len(), 2);
        assert_eq!(snapshot.windows[1].name, "1");
    }

    #[test]
    fn report_viewport_should_apply_dimension_wise_minimum_across_two_clients() {
        let engine = Engine::new();
        let id = engine.create_session("test".to_string(), sh()).unwrap();
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
}
