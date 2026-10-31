use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
#[cfg(not(unix))]
use portable_pty::CommandBuilder;
use portable_pty::{native_pty_system, MasterPty, PtySize};
use tokio::sync::{broadcast, Notify};

use crate::restart::LifecycleEvent;
use uuid::Uuid;

use crate::replay_buffer::{
    allocate_replay_budget, release_replay_budget, ReplayBuffer, ReplayOutcome,
};
use crate::terminal_query::{
    ClientRegistry, Colors, LiveClient, PtyReplyWriter, QueryReplies, QueryResponder, QueryScanner,
};

/// Epic 5 Story 5.4: vt100's third `Parser::new` arg (how many
/// scrolled-off lines it keeps) is now a real per-pane budget, not the
/// placeholder `0` from before copy-mode/scrollback existed. The exact
/// number is plan.md Unresolved Question #1's proposed default, pending a
/// real memory-cost measurement pass — not derived from measurement here.
const DEFAULT_SCROLLBACK_LINES: usize = 5_000;
/// Floor a pane's scrollback is never shrunk below, even when the global
/// budget is under pressure — a pane with *zero* history defeats the
/// point of copy-mode entirely.
const MIN_SCROLLBACK_LINES: usize = 100;
/// Global ceiling across every live pane's *configured* scrollback
/// (Unresolved Question #1's "cap total retained scrollback... evict
/// oldest-inactive-pane's scrollback first if exceeded" — see
/// `allocate_scrollback_budget`'s doc comment for how this is actually
/// implemented, which differs from that literal framing because vt100
/// has no API to shrink an already-constructed `Parser`'s retention).
/// Also a placeholder pending real measurement, not a measured value.
const GLOBAL_SCROLLBACK_BUDGET_LINES: usize = 50_000;

static GLOBAL_SCROLLBACK_USED_LINES: AtomicUsize = AtomicUsize::new(0);

/// Grants a scrollback-line budget to a newly spawned pane, enforcing
/// `GLOBAL_SCROLLBACK_BUDGET_LINES` as a real ceiling on total retained
/// scrollback across every live pane. `pitfalls.md` §3's originally-
/// sketched mechanism was "evict the oldest-inactive pane's scrollback
/// first" — not implementable as written, because `vt100::Parser` has no
/// API to shrink an already-constructed instance's retention (only
/// `Screen::set_scrollback`, which moves the *view* into existing
/// history, not the ring buffer's capacity). This grants the *new* pane
/// less scrollback once the budget is under pressure instead (down to
/// `MIN_SCROLLBACK_LINES`, never zero) — the same ceiling-enforcement
/// property (total retained scrollback is bounded, not unbounded growth),
/// achieved the way the underlying library actually allows. See plan.md's
/// Unresolved Question #13-adjacent Epic 5 note for the full rationale.
fn allocate_scrollback_budget() -> usize {
    let used = GLOBAL_SCROLLBACK_USED_LINES.load(Ordering::Relaxed);
    let remaining = GLOBAL_SCROLLBACK_BUDGET_LINES.saturating_sub(used);
    let granted = DEFAULT_SCROLLBACK_LINES.min(remaining.max(MIN_SCROLLBACK_LINES));
    GLOBAL_SCROLLBACK_USED_LINES.fetch_add(granted, Ordering::Relaxed);
    granted
}

fn release_scrollback_budget(lines: usize) {
    GLOBAL_SCROLLBACK_USED_LINES.fetch_sub(lines, Ordering::Relaxed);
}

/// Read chunk size for the pty output thread. Program output (e.g. `ls
/// -la`, `cat` on a big file) comes in much chunkier bursts than a human's
/// keystrokes, so this is deliberately larger than tymux-cli's 1024-byte
/// stdin-forwarding buffer.
const PTY_READ_BUF_SIZE: usize = 4096;

/// How long a SIGHUP'd child gets before `Pane::kill` escalates to SIGKILL.
#[cfg(unix)]
const KILL_GRACE: Duration = Duration::from_millis(250);

/// Backlog for the output broadcast channel. Sized to absorb a burst of
/// terminal output between two `Attach` clients' `recv()` polls without
/// needing precise tuning — a slow consumer just gets `Lagged` and moves
/// on, it isn't a correctness concern.
const OUTPUT_CHANNEL_CAPACITY: usize = 1024;

pub use crate::launch::Launch;
use crate::launch::PreparedLaunch;

#[cfg(test)]
type PushBumpHook = Box<dyn Fn(&Pane) + Send>;

/// Called on the reader thread when the child has exited, after the exit code is recorded and
/// before `is_exited()` turns true. Returns whether a restart is pending, so attached clients
/// stay attached (see [`Pane::wait_final_exit`]).
pub type ExitHook = Arc<dyn Fn(&Pane, Option<i32>) -> bool + Send + Sync>;

/// Lifecycle events a slow subscriber may lag behind before it starts losing the oldest.
const LIFECYCLE_CHANNEL_CAPACITY: usize = 64;

/// A single pty-backed terminal. Owns the child process, the pty master
/// (for resize + writing input), and a `vt100::Parser` that keeps a
/// structured screen model in sync with everything the child prints.
///
/// The structured model (see [`PaneSnapshot`]) is the whole point of this
/// project over shelling out to tmux: a caller gets cells+attributes
/// directly instead of re-parsing ANSI escapes out of captured text.
pub struct Pane {
    pub id: Uuid,
    /// The command this pane was spawned with, and the daemon's working
    /// directory at spawn time — persisted (Epic 4) so `tymux revive` can
    /// respawn an equivalent process later; not otherwise used at runtime.
    pub command: String,
    pub launch: Launch,
    pub cwd: String,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// Write end of the `stdin_stream` pipe; `None` once closed, or when the
    /// pane was not created with `stdin_stream`.
    stdin_writer: Mutex<Option<std::fs::File>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    parser: Arc<Mutex<vt100::Parser<QueryResponder>>>,
    /// Mirrors the `vt100::Parser` screen's current size, updated by
    /// [`Self::resize`] right after the screen itself is resized. Lets
    /// [`Self::size`] answer "what size is this pane right now" without
    /// locking `parser` — a caller that only wants the current dimensions
    /// (e.g. `Engine::list_sessions`'s snapshot walk, Story 3.4 AC3) would
    /// otherwise contend with `resize`'s `Screen::set_size` call, which
    /// holds `parser` for the real, occasionally non-trivial (for a very
    /// large resize) duration of a grid reallocation — genuinely slow
    /// pty/vt100 work, not a mock. Reading two atomics instead avoids that
    /// contention entirely, at the cost of a caller occasionally seeing
    /// the pre-resize size for the instant between the ioctl and this
    /// store — an already-accepted class of narrow race (see
    /// `recompute_window_geometry`'s doc comment).
    rows: AtomicU32,
    cols: AtomicU32,
    /// This pane's granted share of `GLOBAL_SCROLLBACK_BUDGET_LINES` —
    /// released back to the budget on `Drop`.
    scrollback_lines: usize,
    /// Epic 2.1: bounded, byte-budgeted log of recent output chunks,
    /// populated in the reader thread's read loop inside `parser`'s lock,
    /// before `output_seq` is bumped and before `output_tx.send`. Lock order
    /// is parser -> replay only; nothing takes `replay` then `parser`. Having
    /// the chunk in the buffer before its seq is visible means a resuming
    /// reader that sees `output_seq == q` always finds chunk `q`. Chunks are
    /// stored with terminal queries stripped.
    replay: Mutex<ReplayBuffer>,
    /// This pane's granted share of `GLOBAL_REPLAY_BUFFER_BUDGET_BYTES` —
    /// released back to the budget on `Drop`. Stored separately from
    /// `replay` (which only tracks its own ceiling internally) so `Drop`
    /// can read the granted amount back out without locking `replay`.
    replay_budget_bytes: usize,
    output_tx: broadcast::Sender<(u64, Vec<u8>)>,
    /// Attached clients whose emulators answer terminal queries (see `terminal_query`).
    clients: ClientRegistry,
    /// Answers terminal queries for the process; re-armed on a new pty by `respawn`.
    query_replies: Arc<QueryReplies>,
    /// Test hook run between the replay push and the seq bump, with the replay lock held.
    #[cfg(test)]
    push_bump_hook: Mutex<Option<PushBumpHook>>,
    /// Monotonic count of pty-read chunks fed into `parser` so far (Task
    /// 1.3.1a / ADR-003 Amendment). Incremented in the reader thread in
    /// the same critical section as `parser.process(..)`, so a value read
    /// under `parser`'s lock (see [`Self::snapshot_at_offset_with_seq`])
    /// can never disagree with the grid state it was read alongside —
    /// this is what lets `attach()` tell which broadcast chunks are
    /// already reflected in a given snapshot and must not be replayed.
    output_seq: AtomicU64,
    exited: Arc<AtomicBool>,
    /// Test hook: makes `resize` take at least this long, standing in for the slow reallocation
    /// huge sizes used to cause (the size clamp removed that).
    #[cfg(test)]
    pub(crate) resize_delay_ms: std::sync::atomic::AtomicU64,
    exit_notify: Notify,
    /// The child's numeric exit code, captured by the reader thread's
    /// `_child.wait()` call right after it observes EOF (Story 1.2.2).
    /// `None` until the child has exited, and — even once exited — if
    /// `wait()` itself failed. Set before `exited` flips to `true`, so any
    /// caller that observes `is_exited() == true` is guaranteed to see the
    /// final `exit_code()` value too (write-then-flag, no separate race).
    exit_code: Mutex<Option<i32>>,
    /// Captured at spawn so `kill` can signal without taking `_child`; 0 = none. Replaced by
    /// [`Self::respawn`].
    pid: Arc<AtomicU32>,
    /// Bumped by every respawn, so a delayed SIGKILL aimed at an old process never hits its
    /// successor.
    generation: Arc<AtomicU64>,
    /// The last signal this daemon sent to the child (0 = none); reported as the exit signal.
    last_signal: AtomicI32,
    /// A restart is scheduled or in flight, so this pane's exit is not final.
    restart_pending: AtomicBool,
    /// The pane was closed: `respawn` must refuse.
    retired: AtomicBool,
    /// Serializes `respawn` against `retire`.
    spawn_lock: Mutex<()>,
    exit_hook: Mutex<Option<ExitHook>>,
    /// Set by the reader thread once it has looked for the exit hook (see `set_exit_hook`).
    exit_checked: AtomicBool,
    lifecycle_tx: broadcast::Sender<LifecycleEvent>,
    /// Pulsed on every change `wait_final_exit` cares about: exit, respawn, pending cleared.
    state_notify: Notify,
    // Held only to keep the child alive; not otherwise touched.
    _child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    // Tracked so the reader thread's lifecycle is at least observable
    // (e.g. joinable during a future shutdown path) rather than fully
    // abandoned; the thread itself already signals exit via `exited` above.
    _reader_handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// Why [`Pane::write_stdin`] / [`Pane::close_stdin`] failed.
#[derive(Debug)]
pub enum StdinError {
    /// The pane was not created with `stdin_stream`.
    NotStreaming,
    /// stdin was already closed.
    Closed,
    /// The process closed its stdin or exited.
    BrokenPipe,
    Io(std::io::Error),
}

impl std::fmt::Display for StdinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotStreaming => write!(f, "pane was not created with stdin_stream"),
            Self::Closed => write!(f, "stdin is already closed"),
            Self::BrokenPipe => write!(f, "the process closed its stdin or exited"),
            Self::Io(e) => write!(f, "writing stdin failed: {e}"),
        }
    }
}

impl std::error::Error for StdinError {}

/// Largest pane size `Pane::resize` accepts; larger requests are clamped (the engine clamps
/// viewport reports to the same bound so the size it reports is the size the pane runs at).
pub const MAX_PANE_ROWS: u16 = 500;
pub const MAX_PANE_COLS: u16 = 1000;

pub struct PaneSnapshot {
    pub rows: u32,
    pub cols: u32,
    pub grid: Vec<Vec<CellSnapshot>>,
    pub cursor_row: u32,
    /// Clamped to `cols - 1`; `cursor_pending_wrap` carries the parked-past-the-edge state.
    pub cursor_col: u32,
    pub cursor_pending_wrap: bool,
    /// OSC 8 targets as `(uri, id)`; `CellSnapshot::hyperlink` is a 1-based index into this.
    pub hyperlinks: Vec<(String, String)>,
    pub title: String,
    pub modes: TerminalModes,
    pub cursor_hidden: bool,
    /// DECSCUSR 0-6.
    pub cursor_style: u32,
    /// The grid is the alternate screen.
    pub alt_screen: bool,
    /// DECSTBM 1-based; 0/0 = whole screen.
    pub scroll_top: u32,
    pub scroll_bottom: u32,
    /// 0-based columns with a tab stop.
    pub tab_stops: Vec<u32>,
    pub charsets: Charsets,
}

/// Mirrors proto `TerminalModes`; mouse values use the proto's numbering.
#[derive(Default)]
pub struct TerminalModes {
    pub app_cursor_keys: bool,
    pub app_keypad: bool,
    pub bracketed_paste: bool,
    pub focus_events: bool,
    /// 0 off, 1 X10, 2 normal, 3 button, 4 any.
    pub mouse_mode: u32,
    /// 0 default, 1 utf8, 2 sgr, 3 urxvt.
    pub mouse_encoding: u32,
    pub reverse_video: bool,
    pub origin: bool,
    pub no_autowrap: bool,
    pub insert: bool,
    pub newline: bool,
}

/// Mirrors proto `Charsets`: 0 ASCII, 1 DEC special graphics, 2 UK.
#[derive(Default)]
pub struct Charsets {
    pub g: [u32; 4],
    pub gl: u32,
}

fn terminal_modes(screen: &vt100::Screen) -> TerminalModes {
    TerminalModes {
        app_cursor_keys: screen.application_cursor(),
        app_keypad: screen.application_keypad(),
        bracketed_paste: screen.bracketed_paste(),
        focus_events: screen.focus_events(),
        mouse_mode: match screen.mouse_protocol_mode() {
            vt100::MouseProtocolMode::None => 0,
            vt100::MouseProtocolMode::Press => 1,
            vt100::MouseProtocolMode::PressRelease => 2,
            vt100::MouseProtocolMode::ButtonMotion => 3,
            vt100::MouseProtocolMode::AnyMotion => 4,
        },
        mouse_encoding: match screen.mouse_protocol_encoding() {
            vt100::MouseProtocolEncoding::Default => 0,
            vt100::MouseProtocolEncoding::Utf8 => 1,
            vt100::MouseProtocolEncoding::Sgr => 2,
            vt100::MouseProtocolEncoding::Urxvt => 3,
        },
        reverse_video: screen.reverse_video(),
        origin: screen.origin_mode(),
        no_autowrap: !screen.autowrap(),
        insert: screen.insert_mode(),
        newline: screen.newline_mode(),
    }
}

pub struct CellSnapshot {
    pub text: String,
    pub fg: u32,
    pub bg: u32,
    pub attrs: u32,
    /// 0 single, 1 double, 2 curly, 3 dotted, 4 dashed; meaningful when the underline bit is set.
    pub underline_style: u32,
    /// Packed like `fg`; 0 = same as the text color.
    pub underline_color: u32,
    pub hyperlink: u32,
}

impl Pane {
    pub fn spawn(command: &str, rows: u16, cols: u16) -> Result<Arc<Self>> {
        Self::spawn_internal(command, &Launch::default(), None, None, rows, cols)
    }

    /// Like [`Self::spawn`], but with an explicit working directory — used
    /// by `tymux revive` (Epic 4) to respawn a pane in its persisted `cwd`
    /// rather than the daemon's own. `cwd: None` behaves exactly like
    /// [`Self::spawn`] (inherits the daemon's current directory).
    pub fn spawn_with_cwd(
        command: &str,
        cwd: Option<&str>,
        rows: u16,
        cols: u16,
    ) -> Result<Arc<Self>> {
        Self::spawn_internal(command, &Launch::default(), cwd, None, rows, cols)
    }

    /// Like [`Self::spawn_with_cwd`], but assigns a specific `id` rather
    /// than generating one — `tymux revive` (Epic 4) must respawn a pane
    /// at the *same* id as the dead `PaneEntry` it replaces, so every
    /// existing reference to that pane_id (the window's `LayoutNode` leaf,
    /// a client that already resolved a `TargetString` to it) keeps
    /// pointing at the right pane.
    pub fn spawn_with_id(
        id: Uuid,
        command: &str,
        cwd: Option<&str>,
        rows: u16,
        cols: u16,
    ) -> Result<Arc<Self>> {
        Self::spawn_internal(command, &Launch::default(), cwd, Some(id), rows, cols)
    }

    /// The general spawn: like [`Self::spawn_with_id`] (`id: None` generates
    /// one) but with explicit arguments and environment.
    pub fn spawn_launch(
        command: &str,
        launch: &Launch,
        cwd: Option<&str>,
        id: Option<Uuid>,
        rows: u16,
        cols: u16,
    ) -> Result<Arc<Self>> {
        Self::spawn_internal(command, launch, cwd, id, rows, cols)
    }

    fn spawn_internal(
        command: &str,
        launch: &Launch,
        cwd: Option<&str>,
        id: Option<Uuid>,
        rows: u16,
        cols: u16,
    ) -> Result<Arc<Self>> {
        let process = start_process(command, launch, cwd, rows, cols)?;

        let scrollback_lines = allocate_scrollback_budget();
        let (output_tx, _) = broadcast::channel(OUTPUT_CHANNEL_CAPACITY);
        let writer = Arc::new(Mutex::new(process.writer));
        let replies = Arc::new(QueryReplies::new(
            #[cfg(unix)]
            Box::new(PtyReplyWriter::new(process.reply_fd)),
            #[cfg(not(unix))]
            Box::new(PtyReplyWriter {
                writer: writer.clone(),
            }),
            Colors::from_env(),
        ));
        let parser = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(
            rows,
            cols,
            scrollback_lines,
            QueryResponder::new(replies.clone()),
        )));
        let (lifecycle_tx, _) = broadcast::channel(LIFECYCLE_CHANNEL_CAPACITY);
        let replay_budget_bytes = allocate_replay_budget();

        let effective_cwd = cwd.map(str::to_string).unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        });
        let pane = Arc::new(Pane {
            id: id.unwrap_or_else(Uuid::new_v4),
            command: command.to_string(),
            launch: launch.clone(),
            cwd: effective_cwd,
            writer,
            stdin_writer: Mutex::new(process.stdin_writer),
            master: Mutex::new(process.master),
            parser: parser.clone(),
            rows: AtomicU32::new(rows as u32),
            cols: AtomicU32::new(cols as u32),
            scrollback_lines,
            replay: Mutex::new(ReplayBuffer::new(replay_budget_bytes)),
            replay_budget_bytes,
            output_tx: output_tx.clone(),
            clients: ClientRegistry::default(),
            query_replies: replies.clone(),
            #[cfg(test)]
            push_bump_hook: Mutex::new(None),
            output_seq: AtomicU64::new(0),
            exited: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            resize_delay_ms: std::sync::atomic::AtomicU64::new(0),
            exit_notify: Notify::new(),
            exit_code: Mutex::new(None),
            pid: Arc::new(AtomicU32::new(process.pid.unwrap_or(0))),
            generation: Arc::new(AtomicU64::new(0)),
            last_signal: AtomicI32::new(0),
            restart_pending: AtomicBool::new(false),
            retired: AtomicBool::new(false),
            spawn_lock: Mutex::new(()),
            exit_hook: Mutex::new(None),
            exit_checked: AtomicBool::new(false),
            lifecycle_tx,
            state_notify: Notify::new(),
            _child: Mutex::new(process.child),
            _reader_handle: Mutex::new(None),
        });
        pane.start_reader(process.reader);
        Ok(pane)
    }

    /// Starts the thread that feeds the child's output into the screen model, the replay buffer
    /// and subscribers, and records the exit when the pty closes. portable_pty's reader is
    /// blocking `std::io::Read`, so it gets an OS thread rather than a tokio task. One runs per
    /// process generation; `respawn` starts the next only after the previous one has finished.
    fn start_reader(self: &Arc<Self>, mut reader: Box<dyn Read + Send>) {
        let parser = self.parser.clone();
        let output_tx = self.output_tx.clone();
        let replies = self.query_replies.clone();
        let pane_for_reader = self.clone();
        let handle = std::thread::spawn(move || {
            let mut buf = [0u8; PTY_READ_BUF_SIZE];
            let mut scanner = QueryScanner::default();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        // Task 1.3.1a: the sequence bump happens inside the
                        // same `parser` lock guard as `process(..)`, so the
                        // counter and the grid state it now reflects can
                        // never be observed out of sync by a concurrent
                        // `snapshot_at_offset_with_seq` reader (ADR-003
                        // Amendment).
                        //
                        // Terminal queries: decided once per chunk, before parsing, whether a
                        // registered client will see it live; if not the daemon answers (from
                        // the parser callbacks) and clients get the chunk with queries
                        // stripped. See `terminal_query`.
                        //
                        // Epic 2.1: the replay push and the seq bump happen under one `replay`
                        // lock (order parser -> replay, never the reverse). `replay_since`
                        // reads `output_seq` under that same lock, so it never returns a chunk
                        // newer than its `tail_seq`, nor misses a chunk at or below it.
                        let (seq, daemon_answered, scanned) = {
                            let mut parser = parser.lock().unwrap();
                            let next = pane_for_reader.output_seq.load(Ordering::SeqCst) + 1;
                            let daemon_answers = !pane_for_reader.clients.has_answerer(next);
                            replies.set_daemon_answers(daemon_answers);
                            parser.process(&buf[..n]);
                            let scanned = scanner.feed(&buf[..n]);
                            {
                                let mut replay = pane_for_reader.replay.lock().unwrap();
                                replay.push(next, scanned.stripped());
                                #[cfg(test)]
                                pane_for_reader.run_push_bump_hook();
                                pane_for_reader.output_seq.fetch_add(1, Ordering::SeqCst);
                            }
                            (next, daemon_answers, scanned)
                        };
                        let payload = if daemon_answered {
                            scanned.stripped()
                        } else {
                            &scanned.raw
                        };
                        // Fails only when nobody is currently attached (no
                        // receivers) — expected and benign (e.g. shell
                        // startup output before the first Attach), not an
                        // error worth logging. An empty payload (everything
                        // held back as a possible partial query) is not sent.
                        if !payload.is_empty() {
                            let _ = output_tx.send((seq, payload.to_vec()));
                        }
                    }
                }
            }
            replies.close();
            // A sequence still open at EOF was held back from every consumer; release it.
            let rest = scanner.take_held();
            if !rest.is_empty() {
                let next = pane_for_reader.output_seq.load(Ordering::SeqCst) + 1;
                let mut replay = pane_for_reader.replay.lock().unwrap();
                replay.push(next, &rest);
                pane_for_reader.output_seq.fetch_add(1, Ordering::SeqCst);
                drop(replay);
                let _ = output_tx.send((next, rest));
            }
            // Child exited (or the pty read failed, which for a live child
            // is effectively the same signal). Reap it and capture its
            // exit code (ADR-001/Story 1.2.2) before flipping `exited` —
            // this is the same single path that already owns exit
            // detection (pitfalls.md §5 principle 4: no second detection
            // path), just extended to also record *how* the child exited.
            let code = match pane_for_reader._child.lock().unwrap().wait() {
                Ok(status) => Some(status.exit_code() as i32),
                Err(_) => None,
            };
            *pane_for_reader.exit_code.lock().unwrap() = code;
            // Reaped: from here no signal may be aimed at this pid (it can be recycled), not even
            // by a SIGKILL escalation already scheduled or a Stop that raced the hook below.
            pane_for_reader.pid.store(0, Ordering::SeqCst);
            // The supervisor decides here, before the exit becomes visible, whether a restart
            // follows, so a client that sees `is_exited()` also sees `restart_pending()`.
            let hook = {
                let guard = pane_for_reader.exit_hook.lock().unwrap();
                pane_for_reader.exit_checked.store(true, Ordering::SeqCst);
                guard.clone()
            };
            // The hook sets `restart_pending` itself under the supervision lock; storing its
            // return value here would overwrite a concurrent Stop or policy change.
            if let Some(hook) = hook {
                hook(&pane_for_reader, code);
            }
            // Mark it so any current or future `wait_exit()` caller —
            // including one that started waiting after this point —
            // observes it. Stored after `exit_code` above, so any caller
            // that sees `is_exited() == true` is guaranteed to also see
            // the final `exit_code()`.
            pane_for_reader.exited.store(true, Ordering::SeqCst);
            pane_for_reader.exit_notify.notify_waiters();
            pane_for_reader.state_notify.notify_waiters();
        });
        *self._reader_handle.lock().unwrap() = Some(handle);
    }

    /// Replaces the exited child with a fresh process on a new pty, keeping this pane's id,
    /// screen, scrollback, replay buffer, size, subscribers and launch spec. Re-reads argument
    /// files and re-opens the stdin file, as a revive does. Returns the new pid.
    ///
    /// Errors if the pane is still running or was closed (`retire`); on a spawn failure nothing
    /// changes and the pane stays exited.
    pub fn respawn(self: &Arc<Self>) -> Result<Option<u32>> {
        self.respawn_with(&|_| Vec::new())
    }

    /// [`Self::respawn`], plus `banner(pid)` written into the output stream after the new child
    /// has started but before anything it prints, so the stream shows the boundary in order.
    pub fn respawn_with(
        self: &Arc<Self>,
        banner: &dyn Fn(Option<u32>) -> Vec<u8>,
    ) -> Result<Option<u32>> {
        let _guard = self.spawn_lock.lock().unwrap();
        anyhow::ensure!(!self.retired.load(Ordering::SeqCst), "pane was closed");
        anyhow::ensure!(self.is_exited(), "pane process is still running");
        let (rows, cols) = self.size();
        let process = start_process(
            &self.command,
            &self.launch,
            Some(&self.cwd),
            rows as u16,
            cols as u16,
        )?;
        // `exited` is the old reader's last write, so it is on its way out and the join is
        // immediate; doing it here (not in `Drop`) keeps at most one reader thread per pane.
        if let Some(old) = self._reader_handle.lock().unwrap().take() {
            let _ = old.join();
        }
        *self.writer.lock().unwrap() = process.writer;
        // A fresh reply writer for the new pty, so the old fd closes and a closed outbox reopens.
        #[cfg(unix)]
        self.query_replies
            .rearm(Box::new(PtyReplyWriter::new(process.reply_fd)));
        #[cfg(not(unix))]
        self.query_replies.rearm(Box::new(PtyReplyWriter {
            writer: self.writer.clone(),
        }));
        *self.master.lock().unwrap() = process.master;
        *self.stdin_writer.lock().unwrap() = process.stdin_writer;
        *self._child.lock().unwrap() = process.child;
        let pid = process.pid;
        self.pid.store(pid.unwrap_or(0), Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.last_signal.store(0, Ordering::SeqCst);
        *self.exit_code.lock().unwrap() = None;
        self.exit_checked.store(false, Ordering::SeqCst);
        // `exited` first: `restart_pending` stays set until the process is running, so a client
        // never sees "exited and nothing pending" in between. The supervisor clears it after.
        self.exited.store(false, Ordering::SeqCst);
        // No reader thread exists yet, so this cannot interleave with the child's output.
        let banner = banner(pid);
        if !banner.is_empty() {
            self.inject_output(&banner);
        }
        self.start_reader(process.reader);
        self.state_notify.notify_waiters();
        Ok(pid)
    }

    /// Refuses all future `respawn`s. Called when the pane is closed or its session killed;
    /// serialized with `respawn` so a restart that is mid-flight finishes first and the caller
    /// then kills what it started.
    pub fn retire(&self) {
        let _guard = self.spawn_lock.lock().unwrap();
        self.retired.store(true, Ordering::SeqCst);
        self.restart_pending.store(false, Ordering::SeqCst);
        self.state_notify.notify_waiters();
    }

    /// Installs the exit hook. Returns true if the child already exited without seeing it, in
    /// which case the caller must account for that exit itself.
    pub fn set_exit_hook(&self, hook: ExitHook) -> bool {
        let mut guard = self.exit_hook.lock().unwrap();
        *guard = Some(hook);
        self.exit_checked.load(Ordering::SeqCst)
    }

    pub fn restart_pending(&self) -> bool {
        self.restart_pending.load(Ordering::SeqCst)
    }

    /// How many times the process has been replaced (0 for the original).
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Resolves on the next exit, respawn or pending-restart change. Create it before checking
    /// the condition being waited on, then await it, so no change is missed.
    pub fn state_changed(&self) -> tokio::sync::futures::Notified<'_> {
        self.state_notify.notified()
    }

    pub fn is_retired(&self) -> bool {
        self.retired.load(Ordering::SeqCst)
    }

    /// The running child's pid, if any.
    pub fn pid(&self) -> Option<u32> {
        Some(self.pid.load(Ordering::SeqCst)).filter(|p| *p != 0)
    }

    pub fn set_restart_pending(&self, pending: bool) {
        let was = self.restart_pending.swap(pending, Ordering::SeqCst);
        if was != pending {
            self.state_notify.notify_waiters();
        }
    }

    /// The last signal this daemon sent the child, if any.
    pub fn signal_sent(&self) -> Option<i32> {
        Some(self.last_signal.load(Ordering::SeqCst)).filter(|s| *s != 0)
    }

    pub fn subscribe_lifecycle(&self) -> broadcast::Receiver<LifecycleEvent> {
        self.lifecycle_tx.subscribe()
    }

    pub fn emit_lifecycle(&self, event: LifecycleEvent) {
        // No receivers just means nobody is watching.
        let _ = self.lifecycle_tx.send(event);
    }

    /// Writes `data` into the output stream as if the child had printed it: it lands in the
    /// screen, the scrollback, the replay buffer and every subscriber. Used for the marker the
    /// supervisor leaves where a process died and its successor starts. Call only while no
    /// reader thread is running (between a child's exit and its respawn).
    pub fn inject_output(&self, data: &[u8]) {
        // Same ordering as the reader thread: parser, then replay, with the replay push and the
        // sequence bump under one replay lock so `replay_since` never sees one without the other.
        let seq = {
            let mut parser = self.parser.lock().unwrap();
            let next = self.output_seq.load(Ordering::SeqCst) + 1;
            parser.process(data);
            let mut replay = self.replay.lock().unwrap();
            replay.push(next, data);
            self.output_seq.fetch_add(1, Ordering::SeqCst);
            next
        };
        let _ = self.output_tx.send((seq, data.to_vec()));
    }

    pub fn is_exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }

    /// The child's numeric exit code, once known. `None` before the child
    /// has exited; also `None` after exit if the reaping `wait()` call
    /// itself failed (ADR-001: an absent code is a real, distinct state,
    /// never backfilled to a placeholder like `0`).
    pub fn exit_code(&self) -> Option<i32> {
        *self.exit_code.lock().unwrap()
    }

    /// Terminates the child process. Does not itself mark the pane exited
    /// or notify `wait_exit` waiters — that happens naturally once the
    /// reader thread observes EOF on the pty after the process dies, the
    /// same path an ordinary process exit takes (e.g. the shell running
    /// `exit`). Callers that need the exit to be observed (e.g.
    /// `KillSession` signaling attached clients) should await
    /// [`Self::wait_exit`] after calling this.
    pub fn kill(&self) -> Result<()> {
        #[cfg(unix)]
        {
            self.kill_unix()
        }
        #[cfg(not(unix))]
        {
            self._child.lock().unwrap().kill()?;
            Ok(())
        }
    }

    /// SIGHUP now, SIGKILL after `KILL_GRACE` if the child is still alive.
    /// Not `portable_pty::Child::kill`: it sleeps 50ms before its first
    /// liveness re-check even when the child dies instantly, which blocked
    /// `KillSession` (and the engine's `panes` lock) for ~50ms per pane.
    #[cfg(unix)]
    fn kill_unix(&self) -> Result<()> {
        self.signal_then_kill(libc::SIGHUP, KILL_GRACE)
    }

    /// Asks the child to stop: SIGTERM now, SIGKILL after `grace` if it is still alive. Like
    /// [`Self::kill`] it returns at once; await [`Self::wait_exit`] to see the process die.
    pub fn terminate(&self, grace: Duration) -> Result<()> {
        #[cfg(unix)]
        {
            self.signal_then_kill(libc::SIGTERM, grace)
        }
        #[cfg(not(unix))]
        {
            let _ = grace;
            self._child.lock().unwrap().kill()?;
            Ok(())
        }
    }

    #[cfg(unix)]
    fn signal_then_kill(&self, signal: i32, grace: Duration) -> Result<()> {
        let pid = self.pid.load(Ordering::SeqCst);
        if pid == 0 {
            return Ok(());
        }
        self.last_signal.store(signal, Ordering::SeqCst);
        // SAFETY: plain signal to a pid we spawned and have not yet reaped
        // (the reader thread reaps only after pty EOF).
        if unsafe { libc::kill(pid as i32, signal) } != 0 {
            let err = std::io::Error::last_os_error();
            // Already dead and reaped: the exit is on its way to being recorded.
            if err.raw_os_error() == Some(libc::ESRCH) {
                return Ok(());
            }
            return Err(err.into());
        }
        if self.is_exited() {
            return Ok(());
        }
        let exited = self.exited.clone();
        let generation = self.generation.clone();
        let live_pid = self.pid.clone();
        let this_generation = generation.load(Ordering::SeqCst);
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + grace;
            while std::time::Instant::now() < deadline {
                if exited.load(Ordering::SeqCst)
                    || generation.load(Ordering::SeqCst) != this_generation
                {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            // A respawn clears `exited`, so the generation check is what keeps this from
            // killing the successor process.
            if !exited.load(Ordering::SeqCst)
                && generation.load(Ordering::SeqCst) == this_generation
                && live_pid.load(Ordering::SeqCst) == pid
            {
                // SAFETY: as above; `exited` is set right after the reap.
                unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            }
        });
        Ok(())
    }

    /// Resolves once the pane's child process has exited. Safe to call
    /// after the exit already happened — checks the flag before *and*
    /// after registering for the notification, so a caller can't miss it
    /// by starting to wait in the gap between the check and the await.
    pub async fn wait_exit(&self) {
        loop {
            if self.is_exited() {
                return;
            }
            let notified = self.exit_notify.notified();
            if self.is_exited() {
                return;
            }
            notified.await;
        }
    }

    /// Like [`Self::wait_exit`], but keeps waiting while a restart is pending: it resolves only
    /// when the pane has exited and nothing will bring it back (no restart scheduled, or the
    /// pane was stopped or closed meanwhile). For a pane with no restart policy this is
    /// identical to `wait_exit`.
    pub async fn wait_final_exit(&self) {
        loop {
            if self.is_exited() && !self.restart_pending() {
                return;
            }
            let notified = self.state_notify.notified();
            if self.is_exited() && !self.restart_pending() {
                return;
            }
            notified.await;
        }
    }

    /// Writes `data` to the `stdin_stream` pipe, blocking while the process is
    /// not reading. Writers are serialized, so one call's bytes stay contiguous.
    pub fn write_stdin(&self, data: &[u8]) -> std::result::Result<(), StdinError> {
        let mut guard = self.stdin_writer.lock().unwrap();
        match guard.as_mut() {
            Some(pipe) => pipe.write_all(data).map_err(|e| match e.kind() {
                std::io::ErrorKind::BrokenPipe => StdinError::BrokenPipe,
                _ => StdinError::Io(e),
            }),
            None if self.launch.stdin_stream => Err(StdinError::Closed),
            None => Err(StdinError::NotStreaming),
        }
    }

    /// Sends EOF: closes the `stdin_stream` pipe's write end. Idempotent.
    pub fn close_stdin(&self) -> std::result::Result<(), StdinError> {
        if !self.launch.stdin_stream {
            return Err(StdinError::NotStreaming);
        }
        self.stdin_writer.lock().unwrap().take();
        Ok(())
    }

    pub fn write_input(&self, data: &[u8]) -> Result<()> {
        self.writer.lock().unwrap().write_all(data)?;
        Ok(())
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        // Every row of scrollback and screen costs cols x ~44 bytes, so an absurd client size
        // would pin gigabytes; clamp instead of trusting the wire value.
        let (rows, cols) = (rows.min(MAX_PANE_ROWS), cols.min(MAX_PANE_COLS));
        // A client re-sending its size must not reset the scroll region or pending wrap under a
        // running full-screen program, and there is nothing to tell the child.
        if self.size() == (u32::from(rows), u32::from(cols)) {
            return Ok(());
        }
        #[cfg(test)]
        std::thread::sleep(std::time::Duration::from_millis(
            self.resize_delay_ms.load(Ordering::SeqCst),
        ));
        self.master.lock().unwrap().resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        self.parser
            .lock()
            .unwrap()
            .screen_mut()
            .set_size(rows, cols);
        // Updated *after* the parser resize actually completes, so a
        // concurrent `size()` reader never observes the new dimensions
        // before the grid itself has been reallocated to match. Cheap
        // atomics, deliberately not gated on `self.parser`'s lock — see
        // `size()`'s doc comment for why.
        self.rows.store(rows as u32, Ordering::SeqCst);
        self.cols.store(cols as u32, Ordering::SeqCst);
        Ok(())
    }

    /// Each received item is `(seq, bytes)` — `seq` is this chunk's
    /// [`Self::output_seq`] value at the moment it was fed to the parser
    /// (Task 1.3.1a), letting a caller that also holds a snapshot's
    /// sequence number (from [`Self::snapshot_with_seq`]) tell which
    /// already-subscribed chunks predate that snapshot and must not be
    /// replayed on top of it (ADR-003 Amendment).
    /// Registers an attached client as the answerer of terminal queries. Until
    /// [`LiveClient::go_live_after`] is called the daemon keeps answering; call it with the seq
    /// the client's priming snapshot/replay covers. Hold the handle for the life of the attach.
    pub fn register_live_client(&self) -> LiveClient {
        self.clients.register()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<(u64, Vec<u8>)> {
        self.output_tx.subscribe()
    }

    #[cfg(test)]
    fn run_push_bump_hook(&self) {
        if let Some(hook) = self.push_bump_hook.lock().unwrap().as_ref() {
            hook(self);
        }
    }

    /// Epic 2.1 Story 2.1.2: returns the output chunks produced since
    /// `resume_from_seq`, if still retained, or `GapExceeded` if the
    /// requested seq has already been evicted (or is otherwise out of
    /// range). Reads `output_seq` directly rather than under `parser`'s
    /// lock — resume doesn't need the grid, consistent with `size()`'s
    /// existing precedent of reading atomics without `parser`.
    pub fn replay_since(&self, resume_from_seq: u64) -> ReplayOutcome {
        self.replay
            .lock()
            .unwrap()
            .replay_since(resume_from_seq, self.output_seq.load(Ordering::SeqCst))
    }

    /// The pane's current (rows, cols) — cheap, unlike [`Self::snapshot`],
    /// which also walks and copies the entire cell grid. Deliberately
    /// reads the cached atomics `resize()` maintains rather than locking
    /// `parser` — see the `rows`/`cols` fields' doc comment for why
    /// (Story 3.4 AC3).
    pub fn size(&self) -> (u32, u32) {
        (
            self.rows.load(Ordering::SeqCst),
            self.cols.load(Ordering::SeqCst),
        )
    }

    pub fn snapshot(&self) -> PaneSnapshot {
        self.snapshot_at_offset(0)
    }

    /// Like [`Self::snapshot`], but the grid reflects the screen as it
    /// appeared `offset` lines back in scrollback history (`0` = live,
    /// increasing values scroll further back) — Story 5.4's `CapturePane`
    /// `scrollback_offset` param, copy-mode's navigation primitive.
    pub fn snapshot_at_offset(&self, offset: usize) -> PaneSnapshot {
        self.snapshot_at_offset_with_seq(offset).0
    }

    /// Like [`Self::snapshot`], but also returns the [`Self::output_seq`]
    /// value the grid reflects, read under the same `parser` lock as the
    /// grid itself (Task 1.3.1a) — so the pair can never disagree.
    /// `attach()` uses this to send a priming `Snapshot` `AttachEvent`
    /// whose sequence number lets it drop any already-subscribed `Output`
    /// chunk that predates it, closing the double-render window ADR-003's
    /// Amendment describes.
    pub fn snapshot_with_seq(&self) -> (PaneSnapshot, u64) {
        self.snapshot_at_offset_with_seq(0)
    }

    fn snapshot_at_offset_with_seq(&self, offset: usize) -> (PaneSnapshot, u64) {
        let mut parser = self.parser.lock().unwrap();
        parser.screen_mut().set_scrollback(offset);
        let snapshot = snapshot_from_screen(parser.screen());
        // Reset the scroll position back to live — this method is a
        // point-in-time query, not a persistent view change; leaving the
        // parser scrolled would corrupt normal (offset-0) reads from any
        // other concurrent caller.
        parser.screen_mut().set_scrollback(0);
        // Read while still holding `parser`'s lock — the same lock the
        // reader thread holds while bumping `output_seq` alongside
        // `process(..)` (Task 1.3.1a) — so this value and the grid just
        // built above can never disagree about which bytes are reflected.
        let seq = self.output_seq.load(Ordering::SeqCst);
        (snapshot, seq)
    }

    /// Forward-only, next-match search through scrollback for `pattern`
    /// (plain substring, not regex — matching `features.md` §6's "one
    /// shared history buffer, two access paths" ask for a direct,
    /// non-interactive search entry point). Starts at `start_offset`
    /// (inclusive) and returns the first matching line's own offset and
    /// text, or `None` if no match exists between there and the oldest
    /// retained line.
    pub fn search_scrollback(&self, pattern: &str, start_offset: usize) -> Option<(usize, String)> {
        if pattern.is_empty() {
            return None;
        }
        let mut parser = self.parser.lock().unwrap();
        let cols = parser.screen().size().1;

        parser.screen_mut().set_scrollback(usize::MAX);
        let max_offset = parser.screen().scrollback();

        let result = (start_offset..=max_offset).find_map(|offset| {
            parser.screen_mut().set_scrollback(offset);
            let screen = parser.screen();
            let mut line = String::new();
            for col in 0..cols {
                if let Some(cell) = screen.cell(0, col) {
                    line.push_str(cell.contents());
                }
            }
            line.contains(pattern).then_some((offset, line))
        });
        parser.screen_mut().set_scrollback(0);
        result
    }
}

impl Drop for Pane {
    /// Joins the reader thread so a panic in it (as opposed to the normal
    /// EOF/error exit, which already signals via `exited`/`exit_notify`)
    /// gets surfaced instead of vanishing silently. In the common case the
    /// thread has already finished by the time the last `Arc<Pane>` drops,
    /// so this join returns immediately.
    fn drop(&mut self) {
        if let Some(handle) = self._reader_handle.lock().unwrap().take() {
            // The reader thread itself can drop the last Arc<Pane>; joining
            // itself fails with EDEADLK and panics.
            if handle.thread().id() == std::thread::current().id() {
                // fall through to budget release
            } else if let Err(panic) = handle.join() {
                tracing::error!(pane_id = %self.id, ?panic, "pane reader thread panicked");
            }
        }
        release_scrollback_budget(self.scrollback_lines);
        release_replay_budget(self.replay_budget_bytes);
    }
}

/// A child process on a fresh pty, before it is attached to a [`Pane`].
struct StartedProcess {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    pid: Option<u32>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    reader: Box<dyn Read + Send>,
    stdin_writer: Option<std::fs::File>,
    /// A duplicate of the pty master for the terminal-query reply writer.
    #[cfg(unix)]
    reply_fd: std::os::fd::OwnedFd,
}

/// Opens a pty of the given size and starts `command` on it. Shared by the first spawn and by
/// [`Pane::respawn`], so a restart launches exactly what the original did.
fn start_process(
    command: &str,
    launch: &Launch,
    cwd: Option<&str>,
    rows: u16,
    cols: u16,
) -> Result<StartedProcess> {
    // Before openpty so a bad argument file or stderr path fails with no
    // pty allocated.
    let prepared = launch.prepare()?;
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    #[cfg(unix)]
    let child: Box<dyn portable_pty::Child + Send + Sync> = {
        let master_fd = pair
            .master
            .as_raw_fd()
            .ok_or_else(|| anyhow::anyhow!("pty master has no raw fd"))?;
        // TYMUXD_TOKEN is stripped inside spawn_on_pty: it's the daemon's
        // own secret and must never reach a pane's environment
        // (research/pitfalls.md §2, HIGH PRIORITY).
        crate::spawn::spawn_on_pty(master_fd, command, launch, &prepared, cwd)?
    };
    #[cfg(not(unix))]
    let child = {
        if prepared.stderr.is_some() || prepared.stdin.is_some() {
            anyhow::bail!("stderr_path and stdin_path are only supported on unix");
        }
        let mut cmd = CommandBuilder::new(command);
        cmd.args(&prepared.args);
        for (key, value) in &launch.env {
            cmd.env(key, value);
        }
        if let Some(cwd) = cwd {
            cmd.cwd(cwd);
        }
        cmd.env_remove("TYMUXD_TOKEN");
        pair.slave.spawn_command(cmd)?
    };
    let pid = child.process_id();
    drop(pair.slave);
    // The pane must not hold the pipe's read end: only then does the
    // write end see EPIPE once the child exits.
    let PreparedLaunch {
        stdin_writer,
        stdin,
        ..
    } = prepared;
    drop(stdin);

    let writer = pair.master.take_writer()?;
    let reader = pair.master.try_clone_reader()?;
    #[cfg(unix)]
    let reply_fd = {
        use std::os::fd::{BorrowedFd, RawFd};
        let raw: RawFd = pair
            .master
            .as_raw_fd()
            .ok_or_else(|| anyhow::anyhow!("pty master has no raw fd"))?;
        // SAFETY: the master (and so `raw`) outlives this call; the duplicate is independent.
        unsafe { BorrowedFd::borrow_raw(raw) }.try_clone_to_owned()?
    };
    Ok(StartedProcess {
        child,
        pid,
        master: pair.master,
        writer,
        reader,
        stdin_writer,
        #[cfg(unix)]
        reply_fd,
    })
}

/// Everything a snapshot holds that is computed from the emulator's screen alone (the caller
/// owns locking and the output sequence). Pure, so it can be tested by feeding bytes to a
/// `vt100::Parser` with no pty or shell involved.
fn snapshot_from_screen(screen: &vt100::Screen) -> PaneSnapshot {
    let (rows, cols) = screen.size();

    let mut grid = Vec::with_capacity(rows as usize);
    // screen-wide link index -> 1-based index into the snapshot's own table
    let mut hyperlinks: Vec<(String, String)> = Vec::new();
    let mut link_index = std::collections::HashMap::new();
    for row in 0..rows {
        let mut cells = Vec::with_capacity(cols as usize);
        for col in 0..cols {
            let snap = match screen.cell(row, col) {
                Some(cell) => {
                    let hyperlink = match cell.hyperlink() {
                        0 => 0,
                        screen_idx => *link_index.entry(screen_idx).or_insert_with(|| match screen
                            .hyperlink(screen_idx)
                        {
                            Some((uri, id)) => {
                                hyperlinks.push((uri.to_string(), id.to_string()));
                                hyperlinks.len() as u32
                            }
                            None => 0,
                        }),
                    };
                    CellSnapshot {
                        text: cell.contents().to_string(),
                        fg: pack_color(cell.fgcolor()),
                        bg: pack_color(cell.bgcolor()),
                        attrs: pack_attrs(cell),
                        underline_style: pack_underline_style(cell.underline_style()),
                        underline_color: pack_color(cell.ulcolor()),
                        hyperlink,
                    }
                }
                None => CellSnapshot {
                    text: String::new(),
                    fg: 0,
                    bg: 0,
                    attrs: 0,
                    underline_style: 0,
                    underline_color: 0,
                    hyperlink: 0,
                },
            };
            cells.push(snap);
        }
        grid.push(cells);
    }

    let (cursor_row, cursor_col) = screen.cursor_position();
    let cursor_pending_wrap = screen.cursor_pending_wrap();
    let cursor_col = cursor_col.min(cols.saturating_sub(1));
    let title = screen.title().to_string();
    let modes = terminal_modes(screen);
    let cursor_hidden = screen.hide_cursor();
    let cursor_style = u32::from(screen.cursor_style());
    let alt_screen = screen.alternate_screen();
    let (scroll_top, scroll_bottom) = screen.scroll_region();
    let tab_stops = screen.tab_stops().into_iter().map(u32::from).collect();
    let (g, gl) = screen.charsets();
    let charsets = Charsets {
        g: g.map(u32::from),
        gl: u32::from(gl),
    };
    PaneSnapshot {
        rows: u32::from(rows),
        cols: u32::from(cols),
        grid,
        cursor_row: u32::from(cursor_row),
        cursor_col: u32::from(cursor_col),
        cursor_pending_wrap,
        hyperlinks,
        title,
        modes,
        cursor_hidden,
        cursor_style,
        alt_screen,
        scroll_top: u32::from(scroll_top),
        scroll_bottom: u32::from(scroll_bottom),
        tab_stops,
        charsets,
    }
}

/// Packs a vt100 color into one u32: top byte tags the variant so
/// `Default`/`Idx`/`Rgb` round-trip without a separate enum crossing the
/// gRPC boundary. 0x00 = default, 0x01 = indexed (low byte = index),
/// 0x02 = rgb (next three bytes = r,g,b).
fn pack_color(color: vt100::Color) -> u32 {
    match color {
        vt100::Color::Default => 0,
        vt100::Color::Idx(i) => 0x0100_0000 | i as u32,
        vt100::Color::Rgb(r, g, b) => {
            0x0200_0000 | ((r as u32) << 16) | ((g as u32) << 8) | b as u32
        }
    }
}

// Mirrors proto/tymux/v1/tymux.proto's Cell.attrs doc comment exactly —
// keep the two in sync if either changes.
const ATTR_BOLD: u32 = 1;
const ATTR_UNDERLINE: u32 = 2;
const ATTR_REVERSE: u32 = 4;
const ATTR_ITALIC: u32 = 8;
const ATTR_FAINT: u32 = 16;
const ATTR_STRIKE: u32 = 32;
const ATTR_BLINK: u32 = 64;
const ATTR_CONCEAL: u32 = 128;
const ATTR_OVERLINE: u32 = 256;

fn pack_underline_style(style: vt100::UnderlineStyle) -> u32 {
    match style {
        vt100::UnderlineStyle::Single => 0,
        vt100::UnderlineStyle::Double => 1,
        vt100::UnderlineStyle::Curly => 2,
        vt100::UnderlineStyle::Dotted => 3,
        vt100::UnderlineStyle::Dashed => 4,
    }
}

fn pack_attrs(cell: &vt100::Cell) -> u32 {
    let mut attrs = 0;
    if cell.bold() {
        attrs |= ATTR_BOLD;
    }
    if cell.underline() {
        attrs |= ATTR_UNDERLINE;
    }
    if cell.inverse() {
        attrs |= ATTR_REVERSE;
    }
    if cell.italic() {
        attrs |= ATTR_ITALIC;
    }
    if cell.dim() {
        attrs |= ATTR_FAINT;
    }
    if cell.strikethrough() {
        attrs |= ATTR_STRIKE;
    }
    if cell.blink() {
        attrs |= ATTR_BLINK;
    }
    if cell.conceal() {
        attrs |= ATTR_CONCEAL;
    }
    if cell.overline() {
        attrs |= ATTR_OVERLINE;
    }
    attrs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn terminal_modes_maps_vt100_state_onto_the_proto_numbering() {
        let mut parser = vt100::Parser::new(5, 20, 0);
        parser.process(b"\x1b=\x1b[?1h\x1b[?2004h\x1b[?1004h\x1b[?1003h\x1b[?1015h\x1b[?5h\x1b[?6h\x1b[?7l\x1b[4h\x1b[20h");
        let m = terminal_modes(parser.screen());
        assert!(m.app_cursor_keys && m.app_keypad && m.bracketed_paste && m.focus_events);
        assert!(m.reverse_video && m.origin && m.no_autowrap && m.insert && m.newline);
        assert_eq!((m.mouse_mode, m.mouse_encoding), (4, 3));
        parser.process(b"\x1b[?1003l\x1b[?1015l\x1b[?1000h\x1b[?1006h");
        let m = terminal_modes(parser.screen());
        assert_eq!((m.mouse_mode, m.mouse_encoding), (2, 2));
    }

    /// Feeds `bytes` to a fresh emulator and snapshots it: no pty, shell or timing involved.
    fn snapshot_of(rows: u16, cols: u16, bytes: &[u8]) -> PaneSnapshot {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(bytes);
        snapshot_from_screen(parser.screen())
    }

    #[test]
    fn snapshot_should_carry_title_cursor_scroll_region_tab_stops_and_charsets() {
        let snap = snapshot_of(
            10,
            40,
            b"\x1b]2;my title\x07\x1b[5 q\x1b[?25l\x1b[3;7r\x1b[1;4H\x1bH\x1b)0\x0edone",
        );
        assert_eq!(snap.title, "my title");
        assert_eq!(snap.charsets.gl, 1);
        assert_eq!(snap.cursor_style, 5);
        assert!(snap.cursor_hidden);
        assert_eq!((snap.scroll_top, snap.scroll_bottom), (3, 7));
        assert!(snap.tab_stops.contains(&3));
        assert_eq!(snap.charsets.g[1], 1);
        assert!(!snap.alt_screen);
    }

    #[test]
    fn snapshot_should_clamp_cursor_col_and_flag_a_pending_wrap() {
        let mut parser = vt100::Parser::new(10, 40, 0);
        parser.process("x".repeat(40).as_bytes());
        let snap = snapshot_from_screen(parser.screen());
        assert_eq!(snap.cursor_col, 39, "cursor_col must stay inside the grid");
        assert!(snap.cursor_col < snap.cols);
        assert!(snap.cursor_pending_wrap);

        // any cursor movement clears the pending wrap
        for mv in [&b"\x1b[C"[..], b"\x1b[D", b"\x1b[1;1H", b"\r"] {
            let mut p = vt100::Parser::new(10, 40, 0);
            p.process("x".repeat(40).as_bytes());
            p.process(mv);
            let snap = snapshot_from_screen(p.screen());
            assert!(!snap.cursor_pending_wrap, "{mv:?}");
            assert!(snap.cursor_col < snap.cols, "{mv:?}");
        }
    }

    #[test]
    fn snapshot_should_clamp_the_cursor_after_a_resize_and_across_the_alternate_screen() {
        let mut parser = vt100::Parser::new(10, 40, 0);
        parser.process("x".repeat(40).as_bytes());
        parser.screen_mut().set_size(10, 20);
        let snap = snapshot_from_screen(parser.screen());
        assert_eq!(snap.cols, 20);
        assert!(snap.cursor_col < snap.cols);
        assert!(!snap.cursor_pending_wrap);

        let mut parser = vt100::Parser::new(10, 40, 0);
        parser.process("x".repeat(40).as_bytes());
        parser.process(b"\x1b[?1049h");
        let snap = snapshot_from_screen(parser.screen());
        assert!(snap.alt_screen);
        assert_eq!(snap.cursor_col, 39, "the carried cursor is still clamped");
    }

    #[test]
    fn snapshot_should_reflow_wrapped_lines_when_the_pane_is_resized() {
        let mut parser = vt100::Parser::new(5, 20, 100);
        parser.process(b"0123456789abcdefghijKLMNOPQRST\r\nnext\r\n");
        parser.screen_mut().set_size(5, 10);
        let text = |snap: &PaneSnapshot, row: usize| -> String {
            snap.grid[row]
                .iter()
                .map(|c| c.text.as_str())
                .collect::<String>()
        };
        let snap = snapshot_from_screen(parser.screen());
        assert_eq!(text(&snap, 0).trim_end(), "0123456789");
        assert_eq!(text(&snap, 1).trim_end(), "abcdefghij");
        assert_eq!(text(&snap, 2).trim_end(), "KLMNOPQRST");
        assert_eq!(text(&snap, 3).trim_end(), "next");

        parser.screen_mut().set_size(5, 30);
        let snap = snapshot_from_screen(parser.screen());
        assert_eq!(text(&snap, 0).trim_end(), "0123456789abcdefghijKLMNOPQRST");
        assert_eq!(text(&snap, 1).trim_end(), "next");
    }

    #[test]
    fn pack_attrs_should_set_faint_bit_when_cell_is_dim() {
        let mut parser = vt100::Parser::new(2, 20, 0);
        parser.process(b"\x1b[2mdim\x1b[0m plain \x1b[1mbold\x1b[0m");
        let screen = parser.screen();

        assert_eq!(pack_attrs(screen.cell(0, 0).unwrap()), ATTR_FAINT);
        assert_eq!(pack_attrs(screen.cell(0, 4).unwrap()), 0);
        assert_eq!(pack_attrs(screen.cell(0, 10).unwrap()), ATTR_BOLD);
    }

    #[test]
    fn pack_attrs_should_set_every_new_flag_bit() {
        // ECMA-48 SGR 9 / 5 / 8 / 53; bit values are the wire contract in tymux.proto
        let mut parser = vt100::Parser::new(2, 20, 0);
        parser.process(b"\x1b[9ma\x1b[0;5mb\x1b[0;8mc\x1b[0;53md\x1b[0;1;2me");
        let screen = parser.screen();

        assert_eq!(pack_attrs(screen.cell(0, 0).unwrap()), ATTR_STRIKE);
        assert_eq!(pack_attrs(screen.cell(0, 1).unwrap()), ATTR_BLINK);
        assert_eq!(pack_attrs(screen.cell(0, 2).unwrap()), ATTR_CONCEAL);
        assert_eq!(pack_attrs(screen.cell(0, 3).unwrap()), ATTR_OVERLINE);
        assert_eq!(
            pack_attrs(screen.cell(0, 4).unwrap()),
            ATTR_BOLD | ATTR_FAINT
        );
    }

    #[test]
    fn pack_underline_style_should_match_proto_numbering() {
        use vt100::UnderlineStyle::*;
        let packed: Vec<u32> = [Single, Double, Curly, Dotted, Dashed]
            .into_iter()
            .map(pack_underline_style)
            .collect();
        assert_eq!(packed, [0, 1, 2, 3, 4]);
    }

    #[test]
    fn snapshot_should_carry_underline_style_color_and_deduped_hyperlinks() {
        let snap = snapshot_of(
            24,
            80,
            b"\x1b[4:3;58:2::9:8:7mUL\x1b[0m \x1b]8;;https://a.test\x1b\\LI\x1b]8;;\x1b\\ \x1b]8;;https://a.test\x1b\\NK\x1b]8;;\x1b\\ DONE\r\n",
        );

        let cells: Vec<&CellSnapshot> = snap
            .grid
            .iter()
            .flatten()
            .filter(|c| c.text == "U" || c.text == "L" || c.text == "N")
            .collect();
        let ul = cells
            .iter()
            .find(|c| c.text == "U" && c.attrs & 2 != 0)
            .expect("underlined U cell");
        assert_eq!(ul.attrs & 2, 2, "underline bit");
        assert_eq!(ul.underline_style, 2, "curly");
        assert_eq!(ul.underline_color, 0x0200_0000 | (9 << 16) | (8 << 8) | 7);
        assert_eq!(
            snap.hyperlinks,
            vec![("https://a.test".to_string(), String::new())]
        );
        let linked: Vec<u32> = snap
            .grid
            .iter()
            .flatten()
            .filter(|c| (c.text == "L" || c.text == "N") && c.hyperlink != 0)
            .map(|c| c.hyperlink)
            .collect();
        assert_eq!(linked, vec![1, 1], "both links dedupe to table entry 1");
    }

    #[test]
    fn is_exited_should_return_false_when_process_still_running() {
        let pane = Pane::spawn("/bin/sh", 24, 80).unwrap();
        assert!(!pane.is_exited());
    }

    fn screen_rows(pane: &Pane) -> Vec<String> {
        pane.snapshot()
            .grid
            .iter()
            .map(|row| row.iter().map(|c| c.text.as_str()).collect::<String>())
            .collect()
    }

    fn wait_for_row(pane: &Pane, want: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(row) = screen_rows(pane).into_iter().find(|r| want(r.trim_end())) {
                return row.trim_end().to_string();
            }
            assert!(
                Instant::now() < deadline,
                "row never appeared: {:?}",
                screen_rows(pane)
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Puts the tty in raw no-echo mode (so the script itself never echoes onto the screen),
    /// clears the screen, prints `abc\r\n  xy`, sends `query`, reads up to `reply_len` reply bytes (1s cap via VTIME)
    /// and returns the `GOT:<od output>:END` row.
    fn run_query_script(pane: &Pane, query: &str, reply_len: usize) -> String {
        pane.write_input(b"stty raw -echo min 0 time 10; echo READY\n")
            .unwrap();
        // bash/dash prefix the line with a prompt; sh on macOS does not
        wait_for_row(pane, |r| r.trim_start_matches("$ ") == "READY");
        let script = format!(
            "printf '\\033[2J\\033[Habc\\r\\n  xy'; printf '{query}'; \
             r=$(dd bs=1 count={reply_len} 2>/dev/null | od -An -c | tr -s ' '); echo \"GOT:$r:END\"\n"
        );
        pane.write_input(script.as_bytes()).unwrap();
        wait_for_row(pane, |r| r.contains("GOT:") && r.ends_with(":END"))
    }

    #[test]
    fn terminal_query_should_be_answered_by_the_daemon_when_no_client_is_attached() {
        let pane = Pane::spawn("/bin/sh", 24, 80).unwrap();
        // CPR after "abc\r\n  xy": real cursor is row 2, col 5 => ESC [ 2 ; 5 R
        let text = run_query_script(&pane, "\\033[6n", 6);
        // od's column padding differs between platforms; compare without whitespace
        assert!(
            text.split_whitespace()
                .collect::<String>()
                .contains("GOT:033[2;5R:END"),
            "expected the cursor-position reply, got: {text}"
        );
    }

    #[test]
    fn terminal_query_should_not_be_answered_by_the_daemon_when_a_client_is_attached() {
        let pane = Pane::spawn("/bin/sh", 24, 80).unwrap();
        let _rx = pane.subscribe();
        let client = pane.register_live_client();
        client.go_live_after(0);
        // The marker is printed after the query, so once it is on screen the query was parsed:
        // an event, not a timeout.
        pane.write_input(b"printf '\\033[6n\\033[c'; echo MA\"\"RK\n")
            .unwrap();
        wait_for_row(&pane, |r| r.ends_with("MARK") && !r.contains("echo"));
        assert_eq!(pane.query_replies.accepted(), 0, "daemon must stay silent");
    }

    #[test]
    fn terminal_query_should_be_counted_as_answered_when_no_client_is_attached() {
        let pane = Pane::spawn("/bin/sh", 24, 80).unwrap();
        pane.write_input(b"printf '\\033[6n\\033[c'; echo MA\"\"RK\n")
            .unwrap();
        // the tty may echo a reply just before the marker on the same row
        wait_for_row(&pane, |r| r.ends_with("MARK") && !r.contains("echo"));
        assert_eq!(pane.query_replies.accepted(), 2);
    }

    fn squash(text: &str) -> String {
        text.split_whitespace().collect()
    }

    #[test]
    fn terminal_query_should_be_answered_when_a_client_subscribed_but_is_not_live_yet() {
        // An attach subscribes before it has its snapshot threshold: nobody shows it these chunks.
        let pane = Pane::spawn("/bin/sh", 24, 80).unwrap();
        let _rx = pane.subscribe();
        let _unfinished_attach = pane.register_live_client();
        let text = run_query_script(&pane, "\\033[6n", 6);
        assert!(squash(&text).contains("GOT:033[2;5R"), "{text}");
    }

    #[test]
    fn terminal_query_should_be_answered_when_the_only_client_is_lagged() {
        let pane = Pane::spawn("/bin/sh", 24, 80).unwrap();
        let _rx = pane.subscribe();
        let client = pane.register_live_client();
        client.go_live_after(0);
        client.set_lagged(true);
        let text = run_query_script(&pane, "\\033[6n", 6);
        assert!(squash(&text).contains("GOT:033[2;5R"), "{text}");
    }

    /// Everything a subscriber receives while `pane` runs `script`, until `marker` shows up.
    fn collect_broadcast(
        pane: &Pane,
        rx: &mut broadcast::Receiver<(u64, Vec<u8>)>,
        marker: &str,
    ) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut bytes = Vec::new();
        while !String::from_utf8_lossy(&bytes).contains(marker) {
            assert!(
                Instant::now() < deadline,
                "no {marker}: {:?}",
                String::from_utf8_lossy(&bytes)
            );
            while let Ok((_, chunk)) = rx.try_recv() {
                bytes.extend_from_slice(&chunk);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = pane;
        bytes
    }

    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn broadcast_should_carry_the_query_only_when_a_client_answers_it() {
        // answered by the client: raw
        let pane = Pane::spawn("/bin/sh", 24, 80).unwrap();
        let mut rx = pane.subscribe();
        let client = pane.register_live_client();
        client.go_live_after(0);
        pane.write_input(b"printf 'q\\033[6nZ'; echo DO\"\"NE1\n")
            .unwrap();
        let bytes = collect_broadcast(&pane, &mut rx, "DONE1\r");
        assert!(
            contains_bytes(&bytes, b"q\x1b[6nZ"),
            "{:?}",
            String::from_utf8_lossy(&bytes)
        );

        // answered by the daemon: stripped for every subscriber, even a registered-but-lagged one
        let pane = Pane::spawn("/bin/sh", 24, 80).unwrap();
        let mut rx = pane.subscribe();
        let client = pane.register_live_client();
        client.go_live_after(0);
        client.set_lagged(true);
        pane.write_input(b"printf 'q\\033[6nZ'; echo DO\"\"NE2\n")
            .unwrap();
        let bytes = collect_broadcast(&pane, &mut rx, "DONE2\r");
        assert!(
            !contains_bytes(&bytes, b"\x1b[6n"),
            "{:?}",
            String::from_utf8_lossy(&bytes)
        );
        assert!(
            contains_bytes(&bytes, b"qZ"),
            "{:?}",
            String::from_utf8_lossy(&bytes)
        );
    }

    /// A restarted process gets its own terminal-query replies: the reply writer is re-armed on
    /// the new pty, and a fresh scanner runs in the new reader thread.
    #[cfg(unix)]
    #[test]
    fn a_respawned_pane_should_answer_terminal_queries_on_its_new_pty() {
        let launch = Launch {
            args: vec![
                "-c".into(),
                "stty raw -echo; printf '\\033[c'; dd bs=1 count=4 2>/dev/null | od -An -c; exit 0"
                    .into(),
            ],
            ..Launch::default()
        };
        let pane = Pane::spawn_launch("/bin/sh", &launch, None, None, 24, 200).unwrap();
        assert!(wait_for_grid_text(&pane, "033"), "first run was answered");
        wait_for_exit(&pane);
        let first = pane.query_replies.accepted();
        assert_eq!(first, 1);

        pane.respawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while pane.query_replies.accepted() < 2 {
            assert!(
                Instant::now() < deadline,
                "the new process was never answered"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        wait_for_exit(&pane);
        let text: String = pane
            .snapshot()
            .grid
            .iter()
            .flatten()
            .map(|c| c.text.as_str())
            .collect();
        assert_eq!(
            text.matches("033").count(),
            2,
            "the reply reached the second process too: {text}"
        );
    }

    #[test]
    fn a_child_that_never_reads_stdin_and_floods_queries_should_not_stall_output() {
        let pane = Pane::spawn("/bin/sh", 24, 80).unwrap();
        // `yes` is an external process writing queries as fast as the pty drains; it never reads
        // stdin, so every reply it provokes piles up unread.
        pane.write_input(b"yes \"$(printf '\\033[c\\033[6nx')\"\n")
            .unwrap();
        let wait_until = |what: &str, done: &dyn Fn() -> bool| {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !done() {
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        wait_until("the flood to be answered", &|| {
            pane.query_replies.accepted() > 0
        });
        let before = pane.snapshot_with_seq().1;
        // the reader keeps draining output even though replies sit unread in the child's pty
        wait_until("output to keep flowing", &|| {
            pane.snapshot_with_seq().1 > before + 50
        });
        pane.kill().unwrap();
    }

    #[test]
    fn replay_buffer_should_not_contain_queries_so_a_resuming_client_cannot_answer_them_twice() {
        let pane = Pane::spawn("/bin/sh", 24, 80).unwrap();
        pane.write_input(b"printf 'before\\033[6nafter'; echo\n")
            .unwrap();
        wait_for_row(&pane, |r| {
            r.contains("before") && r.contains("after") && !r.contains("printf")
        });
        let ReplayOutcome::InWindow { chunks, .. } = pane.replay_since(1) else {
            panic!("expected the whole history to be retained");
        };
        let bytes: Vec<u8> = chunks.into_iter().flat_map(|(_, b)| b).collect();
        assert!(
            !bytes.windows(4).any(|w| w == b"\x1b[6n"),
            "replay must not carry the query: {:?}",
            String::from_utf8_lossy(&bytes)
        );
        // the tty echoes the daemon's reply between the two words, so only check both survive
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains("before") && text.contains("after"),
            "{text:?}"
        );
    }

    #[test]
    fn replay_buffer_should_not_contain_a_query_split_across_reads() {
        let pane = Pane::spawn("/bin/sh", 24, 80).unwrap();
        // let the startup output land in chunk 1, which a resume can never replay
        pane.write_input(b"echo RE\"\"ADY\n").unwrap();
        wait_for_row(&pane, |r| r.trim_start_matches("$ ") == "READY");
        pane.write_input(b"printf 'a\\033['; i=0; while [ $i -lt 20000 ]; do i=$((i+1)); done; printf '6nb'; echo SPLIT\"\"DONE\n")
            .unwrap();
        // the tty may echo the daemon's reply between the marker and the text
        wait_for_row(&pane, |r| r.starts_with("ab") && r.contains("SPLITDONE"));
        let ReplayOutcome::InWindow { chunks, .. } = pane.replay_since(1) else {
            panic!("expected the whole history to be retained");
        };
        let bytes: Vec<u8> = chunks.into_iter().flat_map(|(_, b)| b).collect();
        let text = String::from_utf8_lossy(&bytes);
        // "a" and "b" adjacent proves both the ESC[ prefix and the 6n tail are gone
        assert!(!contains_bytes(&bytes, b"\x1b[6n"), "{text:?}");
        assert!(text.contains("\r\nab"), "{text:?}");
    }

    #[test]
    fn a_sequence_open_at_pty_eof_should_still_reach_subscribers_and_replay() {
        let pane = Pane::spawn("/bin/sh", 24, 80).unwrap();
        let mut rx = pane.subscribe();
        pane.write_input(b"echo RE\"\"ADY\n").unwrap();
        wait_for_row(&pane, |r| r.ends_with("READY") && !r.contains("echo"));
        // ESC [ 6 is a query prefix the scanner holds back; EOF must release it
        // exec: nothing else may follow the open sequence, or it would complete and not be held
        pane.write_input(b"exec printf 'x\\033[6'\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !pane.is_exited() {
            assert!(Instant::now() < deadline, "child never exited");
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut live = Vec::new();
        while let Ok((_, chunk)) = rx.try_recv() {
            live.extend_from_slice(&chunk);
        }
        assert!(
            contains_bytes(&live, b"x\x1b[6"),
            "subscriber: {:?}",
            String::from_utf8_lossy(&live)
        );
        let ReplayOutcome::InWindow { chunks, .. } = pane.replay_since(1) else {
            panic!("expected the history to be retained");
        };
        let replayed: Vec<u8> = chunks.into_iter().flat_map(|(_, b)| b).collect();
        assert!(
            contains_bytes(&replayed, b"x\x1b[6"),
            "replay: {:?}",
            String::from_utf8_lossy(&replayed)
        );
    }

    #[test]
    fn user_input_should_not_wait_behind_a_flood_of_unread_replies() {
        let pane = Arc::new(Pane::spawn("/bin/sh", 24, 80).unwrap());
        pane.write_input(b"yes \"$(printf '\\033[c')\"\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while pane.query_replies.accepted() == 0 {
            assert!(Instant::now() < deadline, "flood never answered");
            std::thread::sleep(Duration::from_millis(20));
        }
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let p = pane.clone();
        std::thread::spawn(move || {
            p.write_input(b"\x03").unwrap();
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("Ctrl-C write blocked behind reply writes");
        pane.kill().unwrap();
    }

    #[test]
    fn replay_push_and_seq_bump_should_happen_under_the_replay_lock() {
        // If the lock were not held between push and bump, `replay_since` could return a chunk
        // newer than its tail_seq (re-sent live: double render and double query).
        let pane = Pane::spawn("/bin/sh", 24, 80).unwrap();
        let held_every_time = Arc::new(AtomicBool::new(true));
        let calls = Arc::new(AtomicU64::new(0));
        let (flag, n) = (held_every_time.clone(), calls.clone());
        *pane.push_bump_hook.lock().unwrap() = Some(Box::new(move |p: &Pane| {
            n.fetch_add(1, Ordering::SeqCst);
            if p.replay.try_lock().is_ok() {
                flag.store(false, Ordering::SeqCst);
            }
        }));
        pane.write_input(b"echo HO\"\"OK\n").unwrap();
        wait_for_row(&pane, |r| r.ends_with("HOOK") && !r.contains("echo"));
        assert!(calls.load(Ordering::SeqCst) > 0);
        assert!(
            held_every_time.load(Ordering::SeqCst),
            "replay lock not held across push+bump"
        );
    }

    #[test]
    fn spawns_a_shell_and_captures_its_output() {
        let pane = Pane::spawn("/bin/sh", 24, 80).unwrap();
        pane.write_input(b"echo hello-from-pty\n").unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let found = loop {
            let text: String = pane
                .snapshot()
                .grid
                .iter()
                .flatten()
                .map(|c| c.text.as_str())
                .collect();
            if text.contains("hello-from-pty") {
                break true;
            }
            if Instant::now() > deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(found, "expected pane output to contain echoed text");
    }

    /// Spawns a shell, has it print `line-1`..`line-{count}` (via a single
    /// `awk` process — one exec, one output burst — rather than a shell
    /// `for` loop with many separate `echo` calls, which is more
    /// deterministic under process-scheduling variance on slower/loaded
    /// CI runners) followed by a marker, and blocks until that marker is
    /// visible on-screen plus a short settle delay for the reader thread
    /// to finish draining the burst.
    fn spawn_shell_with_numbered_lines(rows: u16, cols: u16, count: usize) -> Arc<Pane> {
        let pane = Pane::spawn("/bin/sh", rows, cols).unwrap();
        // The marker is emitted by a separate `echo ... | rev` after the
        // awk script (not printed by awk itself) so the terminal's echo of
        // this typed command — which contains the literal text
        // "DONE-MARKER" — can never satisfy the poll below before the awk
        // script has actually run (a real race observed in CI).
        let cmd = format!(
            "awk 'BEGIN{{for(i=1;i<={count};i++) print \"line-\" i}}'; echo DONE-MARKER | rev\n"
        );
        pane.write_input(cmd.as_bytes()).unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let text: String = pane
                .snapshot()
                .grid
                .iter()
                .flatten()
                .map(|c| c.text.as_str())
                .collect();
            if text.contains("REKRAM-ENOD") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "shell output did not complete in time"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // Settle delay: the marker being on-screen only guarantees the
        // bytes up to and including it were processed, but a slow CI
        // runner's scheduler could still be mid-flush on the reader
        // thread's last read() — give it a moment to fully quiesce.
        std::thread::sleep(Duration::from_millis(100));
        pane
    }

    #[test]
    fn capture_pane_should_return_historical_grid_when_scrollback_offset_specified() {
        let pane = spawn_shell_with_numbered_lines(5, 40, 50);
        let live = pane.snapshot();
        let historical = pane.snapshot_at_offset(10);
        let live_text: String = live
            .grid
            .iter()
            .flatten()
            .map(|c| c.text.as_str())
            .collect();
        let historical_text: String = historical
            .grid
            .iter()
            .flatten()
            .map(|c| c.text.as_str())
            .collect();
        assert_ne!(
            live_text, historical_text,
            "a nonzero scrollback offset must show different content than the live screen"
        );

        // Reading at offset must not leave the pane permanently scrolled —
        // a subsequent offset-0 read must be live again.
        let live_again: String = pane
            .snapshot()
            .grid
            .iter()
            .flatten()
            .map(|c| c.text.as_str())
            .collect();
        assert_eq!(live_text, live_again);
    }

    /// Task 1.3.1a: `snapshot_with_seq`'s returned sequence must exactly
    /// match the last broadcast chunk observed once the broadcast channel
    /// has fully settled — proving the counter and the grid state it
    /// reflects are read together under one lock, never torn relative to
    /// each other (ADR-003 Amendment's whole premise for how `attach()`
    /// can safely drop already-reflected `Output` chunks).
    #[test]
    fn snapshot_with_seq_should_return_grid_and_sequence_matching_the_last_broadcast_chunk_under_the_same_lock(
    ) {
        let pane = Pane::spawn("/bin/sh", 5, 40).unwrap();
        let mut rx = pane.subscribe();
        pane.write_input(b"echo marker-text\n").unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        // Poll until the broadcast channel has fully drained (Empty) at a
        // moment where its accumulated bytes contain the marker, recording
        // the sequence number of the chunk that completed it. Re-checked
        // (rather than asserted once) because more bytes — e.g. the
        // shell's next prompt — can keep trickling in for a few
        // iterations after the marker itself appears.
        let mut buffered = Vec::new();
        let mut last_chunk_seq = 0u64;
        let (snapshot, seq) = loop {
            match rx.try_recv() {
                Ok((chunk_seq, bytes)) => {
                    last_chunk_seq = chunk_seq;
                    buffered.extend_from_slice(&bytes);
                }
                Err(broadcast::error::TryRecvError::Empty) => {
                    if String::from_utf8_lossy(&buffered).contains("marker-text") {
                        let result = pane.snapshot_with_seq();
                        // Re-check the channel is *still* empty after
                        // taking the snapshot — broadcast channels queue
                        // messages until received, so if anything arrived
                        // during the snapshot call this will observe it
                        // (Ok, not Empty) and we retry rather than compare
                        // against a now-stale pairing.
                        if matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)) {
                            break result;
                        }
                    }
                    assert!(
                        Instant::now() < deadline,
                        "pane output around the marker never settled"
                    );
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("unexpected broadcast recv error: {e:?}"),
            }
        };

        assert_eq!(
            seq, last_chunk_seq,
            "snapshot_with_seq's sequence must exactly match the last broadcast chunk \
             observed once the channel has settled"
        );
        let text: String = snapshot
            .grid
            .iter()
            .flatten()
            .map(|c| c.text.as_str())
            .collect();
        assert!(
            text.contains("marker-text"),
            "snapshot grid must reflect the same bytes as the last broadcast chunk (seq {seq})"
        );
    }

    /// Story 2.1.2 AC2 / Task 2.1.2d: `pane.replay_since(resume_from_seq)`'s
    /// returned chunks must byte-concatenate to exactly what a live
    /// `subscribe()` delivered over the same window — proving the replay
    /// buffer, pushed in the reader thread's own critical section, can
    /// never disagree with what a concurrent live subscriber already
    /// received. `resume_from_seq` is the real seq of the last chunk from
    /// an initial settle-and-drain (not the `0` sentinel: a pane's very
    /// first-ever chunk is always `seq == 1`, so `replay_since(0)` on a
    /// pane that has already produced output legitimately falls into the
    /// documented "resume_from_seq one less than oldest retained ->
    /// GapExceeded" conservative boundary — see
    /// `replay_buffer_replay_since_should_return_gap_exceeded_when_resume_from_seq_is_one_less_than_oldest_retained`;
    /// that boundary is exercised there, not here). Mirrors
    /// `snapshot_with_seq_should_return_grid_and_sequence_matching_the_last_broadcast_chunk...`'s
    /// settle-and-compare style.
    #[test]
    fn pane_replay_since_should_match_bytes_delivered_by_live_subscribe_when_queried_concurrently_with_new_output(
    ) {
        let pane = Pane::spawn("/bin/sh", 5, 40).unwrap();
        let mut rx = pane.subscribe();
        let deadline = Instant::now() + Duration::from_secs(5);

        // Drain and settle on a marker, returning the seq of the last
        // chunk observed once the given marker text has fully arrived and
        // the channel is (re-checked) empty — same settle-and-recheck
        // pattern as the sibling snapshot_with_seq test.
        let settle_on = |rx: &mut broadcast::Receiver<(u64, Vec<u8>)>,
                         buffered: &mut Vec<u8>,
                         marker: &str| {
            let mut last_seq = 0u64;
            loop {
                match rx.try_recv() {
                    Ok((seq, bytes)) => {
                        last_seq = seq;
                        buffered.extend_from_slice(&bytes);
                    }
                    Err(broadcast::error::TryRecvError::Empty) => {
                        if String::from_utf8_lossy(buffered).contains(marker)
                            && matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty))
                        {
                            return last_seq;
                        }
                        assert!(
                            Instant::now() < deadline,
                            "pane output around marker {marker:?} never settled"
                        );
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(e) => panic!("unexpected broadcast recv error: {e:?}"),
                }
            }
        };

        // Settle past a first marker to establish a real, already-in-the-
        // buffer resume point (never the ambiguous `0` sentinel).
        let mut prefix_bytes = Vec::new();
        pane.write_input(b"echo marker-one\n").unwrap();
        let resume_from_seq = settle_on(&mut rx, &mut prefix_bytes, "marker-one");

        // Produce more output (the "new output" this test's name refers
        // to), collecting exactly what live subscribe() delivers after
        // resume_from_seq.
        let mut live_suffix_bytes = Vec::new();
        pane.write_input(b"echo marker-two\n").unwrap();
        settle_on(&mut rx, &mut live_suffix_bytes, "marker-two");

        let replayed_bytes: Vec<u8> = match pane.replay_since(resume_from_seq) {
            ReplayOutcome::InWindow { chunks, .. } => {
                assert!(
                    chunks.iter().all(|(seq, _)| *seq > resume_from_seq),
                    "no chunk in the replay may be at or before resume_from_seq"
                );
                chunks.into_iter().flat_map(|(_, data)| data).collect()
            }
            other => panic!("expected InWindow, got {other:?}"),
        };

        assert_eq!(
            replayed_bytes, live_suffix_bytes,
            "replay_since(resume_from_seq)'s bytes must exactly match what live subscribe() \
             delivered over the same window"
        );
    }

    #[test]
    fn search_scrollback_rpc_should_return_matching_line_range_when_pattern_present() {
        let pane = spawn_shell_with_numbered_lines(5, 40, 50);
        let found = pane.search_scrollback("line-3", 0);
        assert!(
            found.is_some(),
            "expected to find a historical line matching 'line-3'"
        );
    }

    #[test]
    fn search_scrollback_rpc_should_return_no_matches_when_pattern_absent() {
        let pane = Pane::spawn("/bin/sh", 5, 40).unwrap();
        pane.write_input(b"echo hi\n").unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert!(pane
            .search_scrollback("this-pattern-never-appears-anywhere", 0)
            .is_none());
    }

    #[test]
    fn scrollback_ceiling_should_shrink_new_pane_allocation_when_global_budget_exceeded() {
        // Spawn enough panes that the global budget (50_000 lines /
        // 5_000 default per pane = 10 panes) is exhausted regardless of
        // modest concurrent usage from other tests sharing the same
        // process-wide budget counter.
        let panes: Vec<_> = (0..15)
            .map(|_| Pane::spawn("/bin/sh", 24, 80).unwrap())
            .collect();
        assert!(
            panes.iter().any(|p| p.scrollback_lines < DEFAULT_SCROLLBACK_LINES),
            "at least one pane should have received a reduced scrollback budget once the global ceiling was under pressure"
        );
        assert!(
            panes
                .iter()
                .all(|p| p.scrollback_lines >= MIN_SCROLLBACK_LINES),
            "no pane should ever be reduced below the floor"
        );
    }

    #[tokio::test]
    async fn wait_exit_resolves_after_child_exits() {
        let pane = Pane::spawn("/bin/sh", 24, 80).unwrap();
        assert!(!pane.is_exited());
        pane.write_input(b"exit\n").unwrap();

        tokio::time::timeout(Duration::from_secs(5), pane.wait_exit())
            .await
            .expect("wait_exit should resolve once the child process exits");
        assert!(pane.is_exited());

        // Already-exited: must resolve immediately, not hang waiting for a
        // notification that already fired before this call started.
        tokio::time::timeout(Duration::from_secs(1), pane.wait_exit())
            .await
            .expect("wait_exit must resolve immediately for an already-exited pane");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kill_should_return_without_waiting_for_portable_pty_grace_sleep() {
        let pane = Pane::spawn("/bin/cat", 24, 80).unwrap();
        let started = std::time::Instant::now();
        pane.kill().unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(20),
            "kill() blocked for {:?}",
            started.elapsed()
        );
        tokio::time::timeout(Duration::from_secs(2), pane.wait_exit())
            .await
            .expect("SIGHUP should end a plain child promptly");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kill_should_escalate_to_sigkill_when_child_ignores_sighup() {
        let pane = Pane::spawn_launch(
            "/bin/sh",
            &Launch {
                args: vec!["-c".into(), "trap '' HUP; while :; do sleep 1; done".into()],
                ..Launch::default()
            },
            None,
            None,
            24,
            80,
        )
        .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        pane.kill().unwrap();
        tokio::time::timeout(Duration::from_secs(3), pane.wait_exit())
            .await
            .expect("a SIGHUP-ignoring child must still die via SIGKILL escalation");
    }

    #[tokio::test]
    async fn exit_code_should_return_none_when_queried_before_child_has_exited() {
        let pane = Pane::spawn("/bin/sh", 24, 80).unwrap();
        assert!(!pane.is_exited());
        assert_eq!(
            pane.exit_code(),
            None,
            "exit_code() must not fabricate a value while the pane is still live"
        );
    }

    #[tokio::test]
    async fn exit_code_should_return_some_code_when_reader_thread_observes_eof_and_waits_nonzero() {
        let pane = Pane::spawn("/bin/sh", 24, 80).unwrap();
        pane.write_input(b"exit 42\n").unwrap();

        tokio::time::timeout(Duration::from_secs(5), pane.wait_exit())
            .await
            .expect("wait_exit should resolve once the child process exits");
        assert!(pane.is_exited());
        assert_eq!(pane.exit_code(), Some(42));
    }

    #[tokio::test]
    async fn exit_code_should_return_some_zero_for_a_plain_exit() {
        let pane = Pane::spawn("/bin/sh", 24, 80).unwrap();
        pane.write_input(b"exit\n").unwrap();

        tokio::time::timeout(Duration::from_secs(5), pane.wait_exit())
            .await
            .expect("wait_exit should resolve once the child process exits");
        assert_eq!(pane.exit_code(), Some(0));
    }

    fn wait_for_grid_text(pane: &Pane, needle: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let text: String = pane
                .snapshot()
                .grid
                .iter()
                .flatten()
                .map(|c| c.text.as_str())
                .collect();
            if text.contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[cfg(unix)]
    #[test]
    fn spawn_should_give_child_a_controlling_terminal() {
        // Field 7 of /proc/self/stat is tty_nr: nonzero only with a
        // controlling terminal. Linux-only probe; the print is arithmetic so
        // the typed-command echo can't satisfy the match.
        if !std::path::Path::new("/proc/self/stat").exists() {
            return;
        }
        let script = "read -r l < /proc/self/stat; set -- $l; \
                      if [ \"$7\" != 0 ]; then echo CTTY-$((20+22)); else echo NOCTTY-$((20+22)); fi";
        let launch = Launch {
            args: vec!["-c".into(), script.into()],
            ..Launch::default()
        };
        let pane = Pane::spawn_launch("/bin/sh", &launch, None, None, 24, 80).unwrap();
        assert!(
            wait_for_grid_text(&pane, "CTTY-42"),
            "child had no controlling tty"
        );
    }

    fn tmp_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tymux-pane-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    /// No shell between the request and `exec`: `printf` (a real binary on
    /// PATH, not a builtin here) prints each argv entry on its own line, so
    /// anything a shell would have split, expanded or unquoted shows up raw.
    #[cfg(unix)]
    #[test]
    fn spawn_should_deliver_awkward_args_unmangled_without_a_shell() {
        let from_file = tmp_path("prompt");
        std::fs::write(&from_file, "file \"arg\" $HOME `id` 'x' ; & *").unwrap();
        let launch = Launch {
            args: vec![
                "<%s>\n".into(),
                "a b".into(),
                "it's \"q\"".into(),
                "$HOME".into(),
                "`id`;&|*".into(),
            ],
            arg_specs: vec![crate::launch::ArgSpec::FileContents(
                from_file.display().to_string(),
            )],
            ..Launch::default()
        };
        let pane = Pane::spawn_launch("printf", &launch, None, None, 24, 200).unwrap();
        for expected in [
            "<a b>",
            "<it's \"q\">",
            "<$HOME>",
            "<`id`;&|*>",
            "<file \"arg\" $HOME `id` 'x' ; & *>",
        ] {
            assert!(
                wait_for_grid_text(&pane, expected),
                "argv entry {expected:?} did not arrive unmangled"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn spawn_should_send_only_stderr_to_the_file_and_keep_stdout_on_the_pty() {
        let log = tmp_path("stderr.log");
        let launch = Launch {
            args: vec![
                "-c".into(),
                "echo OUT-$((40+2)); echo ERR-$((40+2)) >&2; \
                 if [ -t 2 ]; then echo STDERR-TTY; else echo STDERR-NOTTY; fi; \
                 if [ -t 1 ]; then echo STDOUT-TTY; fi"
                    .into(),
            ],
            stderr_path: Some(log.display().to_string()),
            ..Launch::default()
        };
        let pane = Pane::spawn_launch("/bin/sh", &launch, None, None, 24, 200).unwrap();
        assert!(wait_for_grid_text(&pane, "OUT-42"));
        assert!(wait_for_grid_text(&pane, "STDOUT-TTY"));
        assert!(wait_for_grid_text(&pane, "STDERR-NOTTY"));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !std::fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("ERR-42")
        {
            assert!(Instant::now() < deadline, "stderr never reached {log:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
        let screen: String = pane
            .snapshot()
            .grid
            .iter()
            .flatten()
            .map(|c| c.text.as_str())
            .collect();
        assert!(
            !screen.contains("ERR-42"),
            "stderr leaked onto the pty: {screen}"
        );
    }

    fn stdin_launch(script: &str, stdin: &std::path::Path) -> Launch {
        Launch {
            args: vec!["-c".into(), script.into(), "sh".into()],
            stdin_path: Some(stdin.display().to_string()),
            ..Launch::default()
        }
    }

    fn wait_for_exit(pane: &Pane) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !pane.is_exited() {
            assert!(Instant::now() < deadline, "child never saw EOF and exited");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// 3 MiB covering every byte value (NUL, ^C, ^D, 0xff, no trailing newline,
    /// lines far past the 4096-byte canonical-mode limit): what a pty write
    /// would mangle must arrive intact, and `cat` only exits on a real EOF.
    #[cfg(unix)]
    #[test]
    fn spawn_should_deliver_a_stdin_file_over_one_mib_byte_identical_then_eof() {
        let input = tmp_path("in.bin");
        let output = tmp_path("out.bin");
        let bytes: Vec<u8> = (0..3 * 1024 * 1024u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .chain(0..=255u8)
            .collect();
        std::fs::write(&input, &bytes).unwrap();
        let launch = Launch {
            args: vec!["-c".into(), "exec cat > \"$OUT\"".into()],
            env: [("OUT".to_string(), output.display().to_string())].into(),
            stdin_path: Some(input.display().to_string()),
            ..Launch::default()
        };
        let pane = Pane::spawn_launch("/bin/sh", &launch, None, None, 24, 80).unwrap();
        wait_for_exit(&pane);
        assert_eq!(pane.exit_code(), Some(0));
        let got = std::fs::read(&output).unwrap();
        assert_eq!(got.len(), bytes.len());
        assert!(got == bytes, "stdin bytes differ from the file");
    }

    #[cfg(unix)]
    #[test]
    fn spawn_with_stdin_file_should_keep_the_tty_on_stdout_but_not_stdin() {
        let input = tmp_path("in.txt");
        std::fs::write(&input, "hello\n").unwrap();
        let script = "if [ -t 0 ]; then echo STDIN-TTY; else echo STDIN-NOTTY; fi; \
                      if [ -t 1 ]; then echo STDOUT-TTY; fi; \
                      if [ -t 2 ]; then echo STDERR-TTY; fi; \
                      read -r line; echo GOT-$line; \
                      if read -r more; then echo MORE; else echo EOF-SEEN; fi";
        let pane = Pane::spawn_launch("/bin/sh", &stdin_launch(script, &input), None, None, 24, 80)
            .unwrap();
        for expected in [
            "STDIN-NOTTY",
            "STDOUT-TTY",
            "STDERR-TTY",
            "GOT-hello",
            "EOF-SEEN",
        ] {
            assert!(wait_for_grid_text(&pane, expected), "missing {expected}");
        }
        // /proc/self/stat field 7 (tty_nr) is nonzero only with a controlling
        // terminal; SIGWINCH reaches the child through it.
        if std::path::Path::new("/proc/self/stat").exists() {
            let ctty = "read -r l < /proc/self/stat; set -- $l; \
                        if [ \"$7\" != 0 ]; then echo CTTY-$((20+22)); else echo NOCTTY-$((20+22)); fi";
            let pane =
                Pane::spawn_launch("/bin/sh", &stdin_launch(ctty, &input), None, None, 24, 80)
                    .unwrap();
            assert!(
                wait_for_grid_text(&pane, "CTTY-42"),
                "lost the controlling tty"
            );
        }
    }

    fn stream_launch(script: &str, env_out: &std::path::Path) -> Launch {
        Launch {
            args: vec!["-c".into(), script.into()],
            env: [("OUT".to_string(), env_out.display().to_string())].into(),
            stdin_stream: true,
            ..Launch::default()
        }
    }

    /// Chunks written after the process started arrive in order and
    /// byte-identical; `cat` exits only on the EOF that `close_stdin` sends.
    #[cfg(unix)]
    #[test]
    fn write_stdin_should_stream_over_one_mib_in_chunks_then_eof_on_close() {
        let output = tmp_path("stream-out.bin");
        let bytes: Vec<u8> = (0..3 * 1024 * 1024u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 11) as u8)
            .collect();
        let script =
            "if [ -t 0 ]; then echo TTY > \"$OUT.tty\"; else echo NOTTY > \"$OUT.tty\"; fi; \
                      exec cat > \"$OUT\"";
        let pane = Pane::spawn_launch(
            "/bin/sh",
            &stream_launch(script, &output),
            None,
            None,
            24,
            80,
        )
        .unwrap();
        for chunk in bytes.chunks(100_003) {
            pane.write_stdin(chunk).unwrap();
        }
        assert!(!pane.is_exited(), "cat must wait for EOF");
        pane.close_stdin().unwrap();
        wait_for_exit(&pane);
        assert_eq!(pane.exit_code(), Some(0));
        assert!(std::fs::read(&output).unwrap() == bytes);
        assert_eq!(
            std::fs::read_to_string(format!("{}.tty", output.display())).unwrap(),
            "NOTTY\n"
        );
        assert!(matches!(
            pane.write_stdin(b"x"),
            Err(crate::StdinError::Closed)
        ));
        pane.close_stdin().unwrap(); // idempotent
    }

    #[cfg(unix)]
    #[test]
    fn write_stdin_should_report_not_streaming_and_broken_pipe() {
        let plain = Pane::spawn_launch("/bin/sh", &Launch::default(), None, None, 24, 80).unwrap();
        assert!(matches!(
            plain.write_stdin(b"x"),
            Err(crate::StdinError::NotStreaming)
        ));
        assert!(matches!(
            plain.close_stdin(),
            Err(crate::StdinError::NotStreaming)
        ));

        let out = tmp_path("unused");
        let gone = Pane::spawn_launch(
            "/bin/sh",
            &stream_launch("exit 0", &out),
            None,
            None,
            24,
            80,
        )
        .unwrap();
        wait_for_exit(&gone);
        assert!(matches!(
            gone.write_stdin(&[0u8; 1 << 20]),
            Err(crate::StdinError::BrokenPipe)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn spawn_should_fail_before_starting_anything_when_the_stdin_file_is_missing() {
        let err = Pane::spawn_launch(
            "/bin/sh",
            &stdin_launch("true", &tmp_path("absent")),
            None,
            None,
            24,
            80,
        )
        .err()
        .expect("spawn must fail");
        let launch_err = err.downcast_ref::<crate::launch::LaunchError>().unwrap();
        assert_eq!(launch_err.kind(), crate::launch::LaunchErrorKind::NotFound);
    }

    #[cfg(unix)]
    #[test]
    fn spawn_should_fail_before_starting_anything_when_an_arg_file_is_missing() {
        let launch = Launch {
            arg_specs: vec![crate::launch::ArgSpec::FileContents(
                tmp_path("missing").display().to_string(),
            )],
            ..Launch::default()
        };
        let err = Pane::spawn_launch("/bin/sh", &launch, None, None, 24, 80)
            .err()
            .expect("spawn must fail");
        assert!(
            err.downcast_ref::<crate::launch::LaunchError>().is_some(),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn spawn_should_fail_when_program_does_not_exist() {
        assert!(Pane::spawn("/nonexistent/tymux-test-prog", 24, 80).is_err());
        assert!(Pane::spawn("tymux-test-prog-not-on-path", 24, 80).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn spawn_should_start_in_home_when_requested_cwd_is_not_a_directory() {
        let home = std::env::var("HOME").expect("HOME set");
        let launch = Launch {
            args: vec!["-c".into(), "echo CWD-$(pwd)-END".into()],
            ..Launch::default()
        };
        let pane = Pane::spawn_launch(
            "/bin/sh",
            &launch,
            Some("/definitely/not/a/dir"),
            None,
            24,
            200,
        )
        .unwrap();
        let real_home = std::fs::canonicalize(&home).unwrap();
        assert!(
            wait_for_grid_text(&pane, &format!("CWD-{}-END", real_home.display()))
                || wait_for_grid_text(&pane, &format!("CWD-{home}-END")),
            "child did not fall back to HOME"
        );
    }

    /// Task 1.3.1b (bearer-token-auth Epic 1.3): `TYMUXD_TOKEN` — the
    /// daemon's own bearer secret, if set in `tymuxd`'s process
    /// environment — must never reach a spawned pane's process
    /// environment, since `portable_pty::CommandBuilder` inherits the full
    /// parent environment by default (research/pitfalls.md §2, HIGH
    /// PRIORITY). The fix (the `TYMUXD_TOKEN` filter in
    /// `spawn::spawn_on_pty`) must be targeted: every other variable still
    /// reaches the pane.
    #[test]
    fn spawn_should_not_leak_tymuxd_token_into_pane_environment() {
        std::env::set_var("TYMUXD_TOKEN", "leaked-secret");
        std::env::set_var("TYMUX_TEST_CONTROL_VAR", "visible");

        // Generous size so `env`'s full output (which can run to several
        // dozen lines) stays on the live grid rather than scrolling into
        // history, keeping the snapshot-based capture below simple.
        let pane = Pane::spawn("/bin/sh", 300, 200).unwrap();
        // The marker is reversed and emitted by a separate `echo ... | rev`
        // (not printed by `env` itself) so the terminal's echo of this
        // typed command line — which doesn't contain the marker text
        // itself — can never satisfy the completion poll before `env` has
        // actually run (same trick used by `spawn_shell_with_numbered_lines`
        // above).
        pane.write_input(b"env; echo DONE-MARKER | rev\n").unwrap();

        let deadline = Instant::now() + Duration::from_secs(30);
        let mut text = String::new();
        let mut completed = false;
        while Instant::now() < deadline {
            text = pane
                .snapshot()
                .grid
                .iter()
                .flatten()
                .map(|c| c.text.as_str())
                .collect();
            if text.contains("REKRAM-ENOD") {
                completed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        // Clean up process-global env vars before any assertion below can
        // panic, so this test never bleeds state into others in the same
        // process.
        std::env::remove_var("TYMUXD_TOKEN");
        std::env::remove_var("TYMUX_TEST_CONTROL_VAR");

        assert!(completed, "shell output did not complete in time");
        assert!(
            !text.contains("TYMUXD_TOKEN"),
            "spawned pane's environment must not contain TYMUXD_TOKEN"
        );
        assert!(
            text.contains("TYMUX_TEST_CONTROL_VAR=visible"),
            "spawned pane's environment must still inherit other variables"
        );
    }

    /// A restart spawns through the same path, but is a separate call site worth pinning: the
    /// respawned process must not inherit the daemon's token either.
    #[test]
    fn respawn_should_not_leak_tymuxd_token_into_pane_environment() {
        let pane = Pane::spawn("/bin/sh", 300, 200).unwrap();
        pane.write_input(b"exit\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !pane.is_exited() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(pane.is_exited(), "the first shell did not exit");

        std::env::set_var("TYMUXD_TOKEN", "respawn-leaked-secret");
        std::env::set_var("TYMUX_TEST_RESPAWN_VAR", "visible");
        let respawned = pane.respawn_with(&|_| Vec::new());
        let mut text = String::new();
        let mut completed = false;
        if respawned.is_ok() {
            pane.write_input(b"env; echo DONE-MARKER | rev\n").unwrap();
            while Instant::now() < deadline {
                text = pane
                    .snapshot()
                    .grid
                    .iter()
                    .flatten()
                    .map(|c| c.text.as_str())
                    .collect();
                if text.contains("REKRAM-ENOD") {
                    completed = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        std::env::remove_var("TYMUXD_TOKEN");
        std::env::remove_var("TYMUX_TEST_RESPAWN_VAR");

        respawned.unwrap();
        assert!(completed, "the respawned shell's output did not complete");
        assert!(
            !text.contains("TYMUXD_TOKEN") && !text.contains("respawn-leaked-secret"),
            "a respawned pane's environment must not contain TYMUXD_TOKEN"
        );
        assert!(
            text.contains("TYMUX_TEST_RESPAWN_VAR=visible"),
            "a respawned pane must still inherit other variables"
        );
    }
}
