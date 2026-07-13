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

    pub fn list_sessions(&self) -> Vec<(Uuid, String, Uuid, Uuid)> {
        self.sessions
            .lock()
            .unwrap()
            .values()
            .map(|s| (s.id, s.name.clone(), s.window_id, s.pane.id))
            .collect()
    }

    pub fn kill_session(&self, id: Uuid) -> Result<()> {
        self.sessions
            .lock()
            .unwrap()
            .remove(&id)
            .map(|_| ())
            .ok_or_else(|| anyhow!("no such session: {id}"))
    }

    /// Finds the pane backing any session's window by pane id — the pane
    /// namespace is flat across sessions since each session has exactly one.
    pub fn pane(&self, pane_id: Uuid) -> Option<Arc<Pane>> {
        self.sessions
            .lock()
            .unwrap()
            .values()
            .find(|s| s.pane.id == pane_id)
            .map(|s| s.pane.clone())
    }
}

fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string())
}
