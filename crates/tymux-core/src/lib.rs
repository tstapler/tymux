mod engine;
mod layout;
mod pane;
mod persistence;

pub use engine::{
    ClosePaneOutcome, Engine, EngineError, LayoutSnapshot, PaneEntry, PaneInfo, PaneLookup,
    ReviveOutcome, SessionSnapshot, SessionState, WindowSnapshot, WindowState,
};
pub use layout::{
    LayoutError, LayoutNode, Orientation, PtyRect, RemoveOutcome, MIN_PANE_COLS, MIN_PANE_ROWS,
    RECOMMENDED_SPLIT_MIN_ROWS,
};
pub use pane::{CellSnapshot, Pane, PaneSnapshot};
pub use persistence::{
    default_sessions_dir, FsPersistenceBackend, NullPersistenceBackend, PersistedLayoutNode,
    PersistedPaneRecord, PersistedSessionRecord, PersistedWindowRecord, PersistenceBackend,
    CURRENT_SCHEMA_VERSION,
};
