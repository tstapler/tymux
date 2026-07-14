use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::engine::{PaneEntry, SessionState};
use crate::layout::{LayoutNode, Orientation};

/// Tier 0 (v1.0's only implemented tier, per ADR-002): metadata + layout
/// shape survives a daemon restart, dead-flagged. The persisted process
/// itself is never resumed automatically — only `tymux revive` respawns
/// it, on explicit user request.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedPaneRecord {
    pub pane_id: Uuid,
    pub command: String,
    pub cwd: String,
    pub rows: u16,
    pub cols: u16,
}

/// Mirrors [`LayoutNode`], but leaves hold a full [`PersistedPaneRecord`]
/// (command/cwd/rows/cols) instead of a live `pane_id` reference into
/// nothing — a dead session's tree must carry everything `tymux revive`
/// needs to respawn it, since there's no live `Engine.panes` entry to fall
/// back on after a restart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum PersistedLayoutNode {
    Leaf {
        pane: PersistedPaneRecord,
    },
    Split {
        orientation: Orientation,
        /// Always exactly 2 entries in a valid record — see
        /// `validate_structure`, which is exactly as strict about this as
        /// live `LayoutNode` mutation is (Story 3.2's invariants).
        children: Vec<(PersistedLayoutNode, f32)>,
    },
}

impl PersistedLayoutNode {
    /// Checks the same structural invariants Story 3.2's proptest suite
    /// enforces on a live `LayoutNode` — corruption on disk is checked
    /// with the same rigor as live-tree mutation, not a weaker ad hoc
    /// check. Must be called (and must pass) before a record is ever fed
    /// to `compute_geometry`/`ReviveSession`'s respawn walk, neither of
    /// which were designed to accept invalid shapes.
    pub fn validate_structure(&self) -> Result<(), String> {
        match self {
            PersistedLayoutNode::Leaf { .. } => Ok(()),
            PersistedLayoutNode::Split { children, .. } => {
                if children.len() != 2 {
                    return Err(format!(
                        "Split node has {} children, expected exactly 2",
                        children.len()
                    ));
                }
                let ratio_sum: f32 = children.iter().map(|(_, r)| r).sum();
                if (ratio_sum - 1.0).abs() > 0.01 {
                    return Err(format!(
                        "Split node children ratios sum to {ratio_sum}, expected ~1.0"
                    ));
                }
                for (child, _) in children {
                    child.validate_structure()?;
                }
                Ok(())
            }
        }
    }

    fn from_live(node: &LayoutNode, panes: &HashMap<Uuid, PaneEntry>) -> Self {
        match node {
            LayoutNode::Leaf { pane_id } => {
                let record = match panes.get(pane_id) {
                    Some(PaneEntry::Live(pane)) => {
                        let (rows, cols) = pane.size();
                        PersistedPaneRecord {
                            pane_id: *pane_id,
                            command: pane.command.clone(),
                            cwd: pane.cwd.clone(),
                            rows: rows as u16,
                            cols: cols as u16,
                        }
                    }
                    _ => PersistedPaneRecord {
                        pane_id: *pane_id,
                        command: String::new(),
                        cwd: String::new(),
                        rows: 0,
                        cols: 0,
                    },
                };
                PersistedLayoutNode::Leaf { pane: record }
            }
            LayoutNode::Split {
                orientation,
                children,
            } => PersistedLayoutNode::Split {
                orientation: *orientation,
                children: children
                    .iter()
                    .map(|(c, ratio)| (PersistedLayoutNode::from_live(c, panes), *ratio))
                    .collect(),
            },
        }
    }
}

/// Rebuilds a live `LayoutNode` from a persisted (already-validated) tree,
/// inserting a `PaneEntry::Dead(record)` into `panes` for every leaf it
/// walks — the reverse of `PersistedLayoutNode::from_live`. Used at daemon
/// startup (`Engine::load_persisted`) to reconstruct dead-flagged
/// sessions.
pub(crate) fn persisted_layout_to_live(
    node: &PersistedLayoutNode,
    panes: &mut HashMap<Uuid, PaneEntry>,
) -> LayoutNode {
    match node {
        PersistedLayoutNode::Leaf { pane } => {
            panes.insert(pane.pane_id, PaneEntry::Dead(pane.clone()));
            LayoutNode::leaf(pane.pane_id)
        }
        PersistedLayoutNode::Split {
            orientation,
            children,
        } => LayoutNode::Split {
            orientation: *orientation,
            children: children
                .iter()
                .map(|(c, ratio)| (persisted_layout_to_live(c, panes), *ratio))
                .collect(),
        },
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedWindowRecord {
    pub id: Uuid,
    pub name: String,
    pub layout: PersistedLayoutNode,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedSessionRecord {
    pub schema_version: u32,
    pub session_id: Uuid,
    pub name: String,
    pub windows: Vec<PersistedWindowRecord>,
    pub active_window_id: Uuid,
}

impl PersistedSessionRecord {
    /// Snapshots a live `SessionState` (plus the pane metadata living
    /// separately in `Engine.panes`) into a persistable record. Not a
    /// `From` impl (as originally sketched in plan.md) because the pane
    /// metadata this needs — command/cwd/rows/cols — lives in a second
    /// map `SessionState` alone doesn't have access to.
    pub fn from_session_state(session: &SessionState, panes: &HashMap<Uuid, PaneEntry>) -> Self {
        PersistedSessionRecord {
            schema_version: CURRENT_SCHEMA_VERSION,
            session_id: session.id,
            name: session.name.clone(),
            windows: session
                .windows
                .iter()
                .map(|w| PersistedWindowRecord {
                    id: w.id,
                    name: w.name.clone(),
                    layout: PersistedLayoutNode::from_live(&w.layout, panes),
                })
                .collect(),
            active_window_id: session.active_window_id,
        }
    }

    /// Version and structural validation together — a record is only
    /// loadable if both pass, per Story 4.1 AC2/AC3.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != CURRENT_SCHEMA_VERSION {
            return Err(format!(
                "unknown schema_version {} (expected {CURRENT_SCHEMA_VERSION})",
                self.schema_version
            ));
        }
        for w in &self.windows {
            w.layout.validate_structure()?;
        }
        Ok(())
    }
}

/// Storage seam `Engine` depends on (Dependency Inversion — the concrete
/// filesystem implementation is a swappable detail, not baked into
/// `Engine` itself). `FsPersistenceBackend` is the production
/// implementation; `NullPersistenceBackend` is a real, valid
/// implementation used by `Engine::new()` so unit tests don't touch disk
/// unless they opt in.
pub trait PersistenceBackend: Send + Sync {
    fn save(&self, record: &PersistedSessionRecord) -> Result<()>;
    fn load_all(&self) -> Vec<PersistedSessionRecord>;
    /// Removes a session's persisted record entirely — called when a
    /// session is killed/closed, so it doesn't reappear dead-flagged on
    /// the next daemon restart.
    fn delete(&self, session_id: Uuid);
}

pub struct NullPersistenceBackend;

impl PersistenceBackend for NullPersistenceBackend {
    fn save(&self, _record: &PersistedSessionRecord) -> Result<()> {
        Ok(())
    }
    fn load_all(&self) -> Vec<PersistedSessionRecord> {
        Vec::new()
    }
    fn delete(&self, _session_id: Uuid) {}
}

pub struct FsPersistenceBackend {
    dir: PathBuf,
}

impl FsPersistenceBackend {
    pub fn new(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn final_path(&self, session_id: Uuid) -> PathBuf {
        self.dir.join(format!("{session_id}.json"))
    }

    fn tmp_path(&self, session_id: Uuid) -> PathBuf {
        self.dir.join(format!("{session_id}.json.tmp"))
    }
}

impl PersistenceBackend for FsPersistenceBackend {
    fn save(&self, record: &PersistedSessionRecord) -> Result<()> {
        let tmp = self.tmp_path(record.session_id);
        let json = serde_json::to_vec_pretty(record)?;
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, self.final_path(record.session_id))?;
        tracing::info!(session_id = %record.session_id, bytes = json.len(), "session persisted");
        Ok(())
    }

    fn load_all(&self) -> Vec<PersistedSessionRecord> {
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(dir = %self.dir.display(), error = %e, "could not read sessions directory, starting with no persisted sessions");
                return out;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!(path = %path.display(), error = %e, "failed to read persisted session file, skipping");
                    continue;
                }
            };
            let record: PersistedSessionRecord = match serde_json::from_slice(&bytes) {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(path = %path.display(), error = %e, "failed to parse persisted session file, skipping");
                    continue;
                }
            };
            if let Err(e) = record.validate() {
                tracing::error!(path = %path.display(), session_id = %record.session_id, error = %e, "persisted session file failed validation, skipping");
                continue;
            }
            out.push(record);
        }
        out
    }

    fn delete(&self, session_id: Uuid) {
        let _ = std::fs::remove_file(self.final_path(session_id));
    }
}

/// Resolves the directory persisted session records live in:
/// `$XDG_STATE_HOME/tymux/sessions` (or the platform-appropriate
/// equivalent `dirs::state_dir()` resolves — falling back to
/// `dirs::data_local_dir()` on platforms without a native state dir, e.g.
/// macOS, then finally the current directory if neither is available).
pub fn default_sessions_dir() -> PathBuf {
    let base = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("tymux").join("sessions")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Orientation as O;

    fn leaf(id: Uuid) -> PersistedLayoutNode {
        PersistedLayoutNode::Leaf {
            pane: PersistedPaneRecord {
                pane_id: id,
                command: "/bin/sh".to_string(),
                cwd: "/tmp".to_string(),
                rows: 24,
                cols: 80,
            },
        }
    }

    fn sample_record() -> PersistedSessionRecord {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let window_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        PersistedSessionRecord {
            schema_version: CURRENT_SCHEMA_VERSION,
            session_id,
            name: "test".to_string(),
            windows: vec![PersistedWindowRecord {
                id: window_id,
                name: "0".to_string(),
                layout: PersistedLayoutNode::Split {
                    orientation: O::Horizontal,
                    children: vec![(leaf(a), 0.5), (leaf(b), 0.5)],
                },
            }],
            active_window_id: window_id,
        }
    }

    #[test]
    fn persisted_session_record_should_round_trip_identical_layout_shape_when_serialized_and_deserialized(
    ) {
        let record = sample_record();
        let json = serde_json::to_string(&record).unwrap();
        let restored: PersistedSessionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(record, restored);
    }

    #[test]
    fn persisted_session_record_should_reject_load_when_schema_version_unknown() {
        let mut record = sample_record();
        record.schema_version = 999;
        assert!(record.validate().is_err());
    }

    #[test]
    fn persisted_layout_node_validate_structure_should_reject_when_split_has_three_children() {
        let node = PersistedLayoutNode::Split {
            orientation: O::Horizontal,
            children: vec![
                (leaf(Uuid::new_v4()), 0.34),
                (leaf(Uuid::new_v4()), 0.33),
                (leaf(Uuid::new_v4()), 0.33),
            ],
        };
        assert!(node.validate_structure().is_err());
    }

    #[test]
    fn persisted_layout_node_validate_structure_should_reject_when_ratios_do_not_sum_to_one() {
        let node = PersistedLayoutNode::Split {
            orientation: O::Horizontal,
            children: vec![(leaf(Uuid::new_v4()), 0.5), (leaf(Uuid::new_v4()), 0.2)],
        };
        assert!(node.validate_structure().is_err());
    }

    #[test]
    fn persisted_layout_node_validate_structure_should_reject_when_split_has_zero_children() {
        let node = PersistedLayoutNode::Split {
            orientation: O::Horizontal,
            children: vec![],
        };
        assert!(node.validate_structure().is_err());
    }

    #[test]
    fn engine_should_accept_any_persistence_backend_implementation_via_trait_object() {
        struct RecordingBackend {
            saved: std::sync::Mutex<Vec<PersistedSessionRecord>>,
        }
        impl PersistenceBackend for RecordingBackend {
            fn save(&self, record: &PersistedSessionRecord) -> Result<()> {
                self.saved.lock().unwrap().push(record.clone());
                Ok(())
            }
            fn load_all(&self) -> Vec<PersistedSessionRecord> {
                Vec::new()
            }
            fn delete(&self, _session_id: Uuid) {}
        }
        let backend: Box<dyn PersistenceBackend> = Box::new(RecordingBackend {
            saved: std::sync::Mutex::new(Vec::new()),
        });
        // Compiles and runs against the trait object — confirms the DI
        // seam exists (architecture-review.md Blocker #2 fix).
        backend.save(&sample_record()).unwrap();
        assert!(backend.load_all().is_empty());
    }

    #[test]
    fn fs_persistence_backend_save_should_write_temp_file_and_rename_never_truncate_in_place() {
        let tmp_dir = std::env::temp_dir().join(format!("tymux-test-{}", Uuid::new_v4()));
        let backend = FsPersistenceBackend::new(tmp_dir.clone()).unwrap();
        let record = sample_record();

        backend.save(&record).unwrap();
        let tmp_leftover = backend.tmp_path(record.session_id);
        assert!(
            !tmp_leftover.exists(),
            "the .tmp file must be renamed away, not left behind"
        );
        assert!(backend.final_path(record.session_id).exists());

        let loaded = backend.load_all();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0], record);

        std::fs::remove_dir_all(&tmp_dir).ok();
    }

    #[test]
    fn fs_persistence_backend_load_all_should_skip_corrupt_json_without_failing() {
        let tmp_dir = std::env::temp_dir().join(format!("tymux-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        std::fs::write(tmp_dir.join("bad.json"), b"not json").unwrap();
        let backend = FsPersistenceBackend::new(tmp_dir.clone()).unwrap();
        backend.save(&sample_record()).unwrap();

        let loaded = backend.load_all();
        assert_eq!(loaded.len(), 1, "the one valid record should still load");

        std::fs::remove_dir_all(&tmp_dir).ok();
    }

    #[test]
    fn fs_persistence_backend_delete_should_remove_the_file() {
        let tmp_dir = std::env::temp_dir().join(format!("tymux-test-{}", Uuid::new_v4()));
        let backend = FsPersistenceBackend::new(tmp_dir.clone()).unwrap();
        let record = sample_record();
        backend.save(&record).unwrap();
        assert_eq!(backend.load_all().len(), 1);

        backend.delete(record.session_id);
        assert_eq!(backend.load_all().len(), 0);

        std::fs::remove_dir_all(&tmp_dir).ok();
    }
}
