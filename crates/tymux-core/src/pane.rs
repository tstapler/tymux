use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use tokio::sync::{broadcast, Notify};
use uuid::Uuid;

/// vt100's third `Parser::new` arg: how many scrolled-off lines it keeps
/// for scrollback. 0 for now — `CapturePane` only ever reads the current
/// on-screen grid, so there's nothing to gain from buffering history it
/// can't expose yet (see the "no scrollback capture" gap in
/// docs/ux/journey-map.md).
const SCROLLBACK_LINES: usize = 0;

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
    output_tx: broadcast::Sender<Vec<u8>>,
    exited: AtomicBool,
    exit_notify: Notify,
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

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, SCROLLBACK_LINES)));
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
            output_tx: output_tx.clone(),
            exited: AtomicBool::new(false),
            exit_notify: Notify::new(),
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
                        parser.lock().unwrap().process(&buf[..n]);
                        // Fails only when nobody is currently attached (no
                        // receivers) — expected and benign (e.g. shell
                        // startup output before the first Attach), not an
                        // error worth logging.
                        let _ = output_tx.send(buf[..n].to_vec());
                    }
                }
            }
            // Child exited (or the pty read failed, which for a live child
            // is effectively the same signal). Mark it so any current or
            // future `wait_exit()` caller — including one that started
            // waiting after this point — observes it.
            pane_for_reader.exited.store(true, Ordering::SeqCst);
            pane_for_reader.exit_notify.notify_waiters();
        });
        *pane._reader_handle.lock().unwrap() = Some(handle);

        Ok(pane)
    }

    pub fn is_exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
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
        self.parser.lock().unwrap().set_size(rows, cols);
        Ok(())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
        self.output_tx.subscribe()
    }

    /// The pane's current (rows, cols) — cheap, unlike [`Self::snapshot`],
    /// which also walks and copies the entire cell grid.
    pub fn size(&self) -> (u32, u32) {
        let (rows, cols) = self.parser.lock().unwrap().screen().size();
        (rows as u32, cols as u32)
    }

    pub fn snapshot(&self) -> PaneSnapshot {
        let parser = self.parser.lock().unwrap();
        let screen = parser.screen();
        let (rows, cols) = screen.size();

        let mut grid = Vec::with_capacity(rows as usize);
        for row in 0..rows {
            let mut cells = Vec::with_capacity(cols as usize);
            for col in 0..cols {
                let (text, fg, bg, attrs) = match screen.cell(row, col) {
                    Some(cell) => (
                        cell.contents(),
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
        PaneSnapshot {
            rows: rows as u32,
            cols: cols as u32,
            grid,
            cursor_row: cursor_row as u32,
            cursor_col: cursor_col as u32,
        }
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
}
