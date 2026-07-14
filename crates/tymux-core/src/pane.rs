use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use tokio::sync::{broadcast, Notify};
use uuid::Uuid;

/// A single pty-backed terminal. Owns the child process, the pty master
/// (for resize + writing input), and a `vt100::Parser` that keeps a
/// structured screen model in sync with everything the child prints.
///
/// The structured model (see [`PaneSnapshot`]) is the whole point of this
/// project over shelling out to tmux: a caller gets cells+attributes
/// directly instead of re-parsing ANSI escapes out of captured text.
pub struct Pane {
    pub id: Uuid,
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
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let cmd = CommandBuilder::new(command);
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let writer = pair.master.take_writer()?;
        let mut reader = pair.master.try_clone_reader()?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        let (output_tx, _) = broadcast::channel(1024);

        let pane = Arc::new(Pane {
            id: Uuid::new_v4(),
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
            let mut buf = [0u8; 4096];
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

fn pack_attrs(cell: &vt100::Cell) -> u32 {
    let mut attrs = 0;
    if cell.bold() {
        attrs |= 1;
    }
    if cell.underline() {
        attrs |= 2;
    }
    if cell.inverse() {
        attrs |= 4;
    }
    if cell.italic() {
        attrs |= 8;
    }
    attrs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

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
