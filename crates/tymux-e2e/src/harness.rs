use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

use crate::golden;

/// Drives a CLI binary under a real pseudo-terminal — the same pty
/// machinery `tymux-core::Pane` uses for its own shell panes — and keeps a
/// `vt100::Parser` fed with everything it prints, so tests assert on
/// rendered screen content instead of scraping raw ANSI bytes.
pub struct CliHarness {
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    parser: Arc<Mutex<vt100::Parser>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
}

impl CliHarness {
    /// Spawns `bin` (pass `crate::workspace_bin("tymux")`) with `args`
    /// under a `rows`x`cols` pty. `envs` are set on the child only — never
    /// mutates this process's own environment, so tests stay safe to run
    /// in parallel (cargo runs test functions within a binary concurrently
    /// by default).
    pub fn spawn(
        bin: &std::path::Path,
        args: &[&str],
        envs: &[(&str, &str)],
        rows: u16,
        cols: u16,
    ) -> anyhow::Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(bin);
        for arg in args {
            cmd.arg(arg);
        }
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let writer = pair.master.take_writer()?;
        let mut reader = pair.master.try_clone_reader()?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));

        let parser_for_reader = parser.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => parser_for_reader.lock().unwrap().process(&buf[..n]),
                }
            }
        });

        Ok(Self {
            writer: Mutex::new(writer),
            master: Mutex::new(pair.master),
            parser,
            child: Mutex::new(child),
        })
    }

    /// Sends SIGHUP directly to the CLI child process (`portable_pty::Child::kill`'s
    /// actual signal on Unix) *without* closing this harness's own pty
    /// master — simulates a client process dying (crash, `kill`) while its
    /// terminal is still technically open, as opposed to [`Self`] simply
    /// being dropped (which additionally closes the pty master, a genuine
    /// OS-level hangup — see `tests/disconnect_survival_e2e.rs`, where the
    /// two are shown to have different consequences daemon-side).
    pub fn kill_child(&self) {
        let _ = self.child.lock().unwrap().kill();
    }

    pub fn send(&self, bytes: &[u8]) {
        self.writer
            .lock()
            .unwrap()
            .write_all(bytes)
            .expect("write to pty");
    }

    pub fn send_str(&self, s: &str) {
        self.send(s.as_bytes());
    }

    /// Sends the default `C-b d` detach sequence and waits for the CLI's
    /// own "[tymux: detached]" confirmation — the standard, graceful way
    /// a test should end an attach session. Deliberately does **not**
    /// just `drop()` the harness for this: dropping closes the pty master
    /// out from under a still-live client process (a genuine OS-level
    /// hangup), which is a materially different — and, as of this
    /// writing, buggy — disconnect path; see
    /// `tests/disconnect_survival_e2e.rs`. Returns whether the
    /// confirmation was observed in time — callers typically `assert!` it.
    pub fn detach(&self, timeout: Duration) -> bool {
        self.send(&[0x02, b'd']); // C-b d, the default binding
        self.wait_for("detached", timeout)
    }

    /// Resizes both the OS-level pty (so the child's own size queries
    /// observe it) and the local tracking parser, mirroring
    /// `tymux_core::pane::Pane::resize`.
    pub fn resize(&self, rows: u16, cols: u16) {
        self.master
            .lock()
            .unwrap()
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize pty");
        self.parser
            .lock()
            .unwrap()
            .screen_mut()
            .set_size(rows, cols);
    }

    /// Runs `f` with the current parsed screen under lock — avoids needing
    /// `vt100::Screen` to be `Clone`.
    pub fn with_screen<T>(&self, f: impl FnOnce(&vt100::Screen) -> T) -> T {
        let parser = self.parser.lock().unwrap();
        f(parser.screen())
    }

    pub fn screen_text(&self) -> String {
        self.with_screen(golden::render_screen)
    }

    /// Polls the rendered screen until it contains `pattern` or `timeout`
    /// elapses. Returns whether it was found.
    pub fn wait_for(&self, pattern: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.screen_text().contains(pattern) {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }
}
