use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use tokio::sync::{broadcast, Notify};
use uuid::Uuid;

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

/// Backlog for the output broadcast channel. Sized to absorb a burst of
/// terminal output between two `Attach` clients' `recv()` polls without
/// needing precise tuning — a slow consumer just gets `Lagged` and moves
/// on, it isn't a correctness concern.
const OUTPUT_CHANNEL_CAPACITY: usize = 1024;

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
    pub cwd: String,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    parser: Arc<Mutex<vt100::Parser>>,
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
    output_tx: broadcast::Sender<(u64, Vec<u8>)>,
    /// Monotonic count of pty-read chunks fed into `parser` so far (Task
    /// 1.3.1a / ADR-003 Amendment). Incremented in the reader thread in
    /// the same critical section as `parser.process(..)`, so a value read
    /// under `parser`'s lock (see [`Self::snapshot_at_offset_with_seq`])
    /// can never disagree with the grid state it was read alongside —
    /// this is what lets `attach()` tell which broadcast chunks are
    /// already reflected in a given snapshot and must not be replayed.
    output_seq: AtomicU64,
    exited: AtomicBool,
    exit_notify: Notify,
    /// The child's numeric exit code, captured by the reader thread's
    /// `_child.wait()` call right after it observes EOF (Story 1.2.2).
    /// `None` until the child has exited, and — even once exited — if
    /// `wait()` itself failed. Set before `exited` flips to `true`, so any
    /// caller that observes `is_exited() == true` is guaranteed to see the
    /// final `exit_code()` value too (write-then-flag, no separate race).
    exit_code: Mutex<Option<i32>>,
    // Held only to keep the child alive; not otherwise touched.
    _child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    // Tracked so the reader thread's lifecycle is at least observable
    // (e.g. joinable during a future shutdown path) rather than fully
    // abandoned; the thread itself already signals exit via `exited` above.
    _reader_handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

pub struct PaneSnapshot {
    pub rows: u32,
    pub cols: u32,
    pub grid: Vec<Vec<CellSnapshot>>,
    pub cursor_row: u32,
    pub cursor_col: u32,
}

pub struct CellSnapshot {
    pub text: String,
    pub fg: u32,
    pub bg: u32,
    pub attrs: u32,
}

impl Pane {
    pub fn spawn(command: &str, rows: u16, cols: u16) -> Result<Arc<Self>> {
        Self::spawn_internal(command, None, None, rows, cols)
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
        Self::spawn_internal(command, cwd, None, rows, cols)
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
        Self::spawn_internal(command, cwd, Some(id), rows, cols)
    }

    fn spawn_internal(
        command: &str,
        cwd: Option<&str>,
        id: Option<Uuid>,
        rows: u16,
        cols: u16,
    ) -> Result<Arc<Self>> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(command);
        if let Some(cwd) = cwd {
            cmd.cwd(cwd);
        }
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let writer = pair.master.take_writer()?;
        let mut reader = pair.master.try_clone_reader()?;

        let scrollback_lines = allocate_scrollback_budget();
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, scrollback_lines)));
        let (output_tx, _) = broadcast::channel(OUTPUT_CHANNEL_CAPACITY);

        let effective_cwd = cwd.map(str::to_string).unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        });
        let pane = Arc::new(Pane {
            id: id.unwrap_or_else(Uuid::new_v4),
            command: command.to_string(),
            cwd: effective_cwd,
            writer: Mutex::new(writer),
            master: Mutex::new(pair.master),
            parser: parser.clone(),
            rows: AtomicU32::new(rows as u32),
            cols: AtomicU32::new(cols as u32),
            scrollback_lines,
            output_tx: output_tx.clone(),
            output_seq: AtomicU64::new(0),
            exited: AtomicBool::new(false),
            exit_notify: Notify::new(),
            exit_code: Mutex::new(None),
            _child: Mutex::new(child),
            _reader_handle: Mutex::new(None),
        });

        // portable_pty's reader is blocking std::io::Read, so it gets its
        // own OS thread rather than a tokio task.
        let pane_for_reader = pane.clone();
        let handle = std::thread::spawn(move || {
            let mut buf = [0u8; PTY_READ_BUF_SIZE];
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
                        let seq = {
                            let mut parser = parser.lock().unwrap();
                            parser.process(&buf[..n]);
                            pane_for_reader.output_seq.fetch_add(1, Ordering::SeqCst) + 1
                        };
                        // Fails only when nobody is currently attached (no
                        // receivers) — expected and benign (e.g. shell
                        // startup output before the first Attach), not an
                        // error worth logging.
                        let _ = output_tx.send((seq, buf[..n].to_vec()));
                    }
                }
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
            // Mark it so any current or future `wait_exit()` caller —
            // including one that started waiting after this point —
            // observes it. Stored after `exit_code` above, so any caller
            // that sees `is_exited() == true` is guaranteed to also see
            // the final `exit_code()`.
            pane_for_reader.exited.store(true, Ordering::SeqCst);
            pane_for_reader.exit_notify.notify_waiters();
        });
        *pane._reader_handle.lock().unwrap() = Some(handle);

        Ok(pane)
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
        self._child.lock().unwrap().kill()?;
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

    pub fn write_input(&self, data: &[u8]) -> Result<()> {
        self.writer.lock().unwrap().write_all(data)?;
        Ok(())
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
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
    pub fn subscribe(&self) -> broadcast::Receiver<(u64, Vec<u8>)> {
        self.output_tx.subscribe()
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
        let screen = parser.screen();
        let (rows, cols) = screen.size();

        let mut grid = Vec::with_capacity(rows as usize);
        for row in 0..rows {
            let mut cells = Vec::with_capacity(cols as usize);
            for col in 0..cols {
                let (text, fg, bg, attrs) = match screen.cell(row, col) {
                    Some(cell) => (
                        cell.contents().to_string(),
                        pack_color(cell.fgcolor()),
                        pack_color(cell.bgcolor()),
                        pack_attrs(cell),
                    ),
                    None => (String::new(), 0, 0, 0),
                };
                cells.push(CellSnapshot {
                    text,
                    fg,
                    bg,
                    attrs,
                });
            }
            grid.push(cells);
        }

        let (cursor_row, cursor_col) = screen.cursor_position();
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
        (
            PaneSnapshot {
                rows: rows as u32,
                cols: cols as u32,
                grid,
                cursor_row: cursor_row as u32,
                cursor_col: cursor_col as u32,
            },
            seq,
        )
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
            if let Err(panic) = handle.join() {
                tracing::error!(pane_id = %self.id, ?panic, "pane reader thread panicked");
            }
        }
        release_scrollback_budget(self.scrollback_lines);
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
    attrs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn is_exited_should_return_false_when_process_still_running() {
        let pane = Pane::spawn("/bin/sh", 24, 80).unwrap();
        assert!(!pane.is_exited());
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
}
