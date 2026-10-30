mod engine;
mod launch;
mod layout;
mod pane;
mod persistence;
mod replay_buffer;
#[cfg(unix)]
mod spawn;
mod terminal_query;

pub use engine::{
    ClosePaneOutcome, Engine, EngineError, LayoutSnapshot, PaneEntry, PaneInfo, PaneLookup,
    ReviveOutcome, SessionSnapshot, SessionState, WindowSnapshot, WindowState,
};
pub use launch::{ArgSpec, Launch, LaunchError, LaunchErrorKind, PreparedLaunch, MAX_ARG_STRLEN};
pub use layout::{
    LayoutError, LayoutNode, Orientation, PtyRect, RemoveOutcome, MIN_PANE_COLS, MIN_PANE_ROWS,
    RECOMMENDED_SPLIT_MIN_ROWS,
};
pub use pane::{CellSnapshot, Charsets, Pane, PaneSnapshot, StdinError, TerminalModes};
pub use persistence::{
    default_sessions_dir, FsPersistenceBackend, NullPersistenceBackend, PersistedLayoutNode,
    PersistedPaneRecord, PersistedSessionRecord, PersistedWindowRecord, PersistenceBackend,
    CURRENT_SCHEMA_VERSION, STALE_SESSION_MAX_AGE,
};
pub use replay_buffer::ReplayOutcome;
