use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use uuid::Uuid;

use crate::pane::Pane;

const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;

/// A session is one window with one pane for now.
///
/// ponytail: real tmux supports N windows of N split panes with layout
/// trees. Nothing here blocks adding that later (the proto already models
/// `repeated windows`/`repeated panes`) — it's just not built until a
/// caller actually needs a split.
pub struct SessionState {
    pub id: Uuid,
    pub name: String,
    pub window_id: Uuid,
    pub pane: Arc<Pane>,
}

/// A snapshot of one session's identity, decoupled from `SessionState`'s
/// internal field layout so callers (namely `tymuxd`) don't have to
/// destructure a positional tuple that would silently reorder/break on any
/// future field change.
pub struct SessionInfo {
    pub id: Uuid,
    pub name: String,
    pub window_id: Uuid,
    pub pane_id: Uuid,
    pub rows: u32,
    pub cols: u32,
    pub live: bool,
}

/// The three-way outcome of looking up a pane by id, replacing the old
/// `Option<Arc<Pane>>` (which collapsed "exited" and "never existed" into
/// the same `None`). A caller (e.g. `capture_pane`/`attach`) needs to tell
/// these apart to return `failed_precondition` (with a remediation) instead
/// of a bare `not_found` for a pane that used to exist.
pub enum PaneLookup {
    Live(Arc<Pane>),
    /// The pane is still tracked by the engine but its process has exited.
    /// (Once persistence lands in Epic 4, this will carry the persisted
    /// record instead — see `plan.md`'s `PaneLookup` glossary entry.)
    Dead,
    Unknown,
}

#[derive(Default)]
pub struct Engine {
    sessions: Mutex<HashMap<Uuid, SessionState>>,
}

impl Engine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create_session(&self, name: String, command: Option<String>) -> Result<Uuid> {
        let shell = command.unwrap_or_else(default_shell);
        let pane = Pane::spawn(&shell, DEFAULT_ROWS, DEFAULT_COLS)?;

        let session = SessionState {
            id: Uuid::new_v4(),
            name,
            window_id: Uuid::new_v4(),
            pane,
        };
        let id = session.id;
        self.sessions.lock().unwrap().insert(id, session);
        Ok(id)
    }

    pub fn list_sessions(&self) -> Vec<SessionInfo> {
        self.sessions
            .lock()
            .unwrap()
            .values()
            .map(|s| {
                let (rows, cols) = s.pane.size();
                SessionInfo {
                    id: s.id,
                    name: s.name.clone(),
                    window_id: s.window_id,
                    pane_id: s.pane.id,
                    rows,
                    cols,
                    live: !s.pane.is_exited(),
                }
            })
            .collect()
    }

    /// Kills the session's pane process (if still live) before removing the
    /// session, so any client currently attached to it observes a normal
    /// pane-exit event through the same path an ordinary process exit
    /// takes — rather than a bare stream error or silent hang. See
    /// `tymuxd`'s `attach` handler: its forwarding loop already reacts to
    /// `Pane::wait_exit`, so no separate out-of-band signal is needed here.
    pub fn kill_session(&self, id: Uuid) -> Result<()> {
        let session = self
            .sessions
            .lock()
            .unwrap()
            .remove(&id)
            .ok_or_else(|| anyhow!("no such session: {id}"))?;
        if !session.pane.is_exited() {
            if let Err(e) = session.pane.kill() {
                tracing::warn!(session_id = %id, pane_id = %session.pane.id, error = %e, "kill_session: failed to kill pane process");
            }
        }
        Ok(())
    }

    /// Finds the pane backing any session's window by pane id — the pane
    /// namespace is flat across sessions since each session currently has
    /// exactly one window and one pane (see
    /// docs/adr/0001-single-pane-per-session-for-now.md); a pane id is
    /// therefore already globally unique without needing to know which
    /// session it belongs to.
    pub fn pane_lookup(&self, pane_id: Uuid) -> PaneLookup {
        match self
            .sessions
            .lock()
            .unwrap()
            .values()
            .find(|s| s.pane.id == pane_id)
        {
            None => PaneLookup::Unknown,
            Some(s) if s.pane.is_exited() => PaneLookup::Dead,
            Some(s) => PaneLookup::Live(s.pane.clone()),
        }
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

    #[test]
    fn create_and_list_session() {
        let engine = Engine::new();
        let id = engine.create_session("test".to_string(), sh()).unwrap();

        let sessions = engine.list_sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, id);
        assert_eq!(sessions[0].name, "test");
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

        let pane_ids: Vec<Uuid> = sessions.iter().map(|s| s.pane_id).collect();
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
        let pane_id = engine
            .list_sessions()
            .into_iter()
            .find(|s| s.id == id)
            .unwrap()
            .pane_id;

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
        let pane = match engine.pane_lookup(
            engine
                .list_sessions()
                .into_iter()
                .find(|s| s.id == id)
                .unwrap()
                .pane_id,
        ) {
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
}
