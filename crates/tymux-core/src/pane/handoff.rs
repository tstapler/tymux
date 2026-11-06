//! Pane state transfer across an in-place `exec` of the daemon (ADR 0008).
//!
//! The old image freezes every pane's reader ([`Pane::freeze_reads`]), exports each pane
//! ([`Pane::export_handoff`], which also clears `FD_CLOEXEC` on the pty master), serializes the
//! [`PaneHandoff`]s, and `execve`s. The new image rebuilds each pane with
//! [`Pane::from_adopted`] from the inherited master fd, the surviving child pid (still our child,
//! so `waitpid` keeps working) and the exported screen and replay state. Bytes the child wrote
//! during the gap wait in the kernel pty buffer, so none are lost.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use anyhow::{bail, ensure, Context, Result};
use portable_pty::{Child, ChildKiller, ExitStatus, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{Pane, QueryResponder, ReplayBuffer, StartedProcess};
use crate::launch::Launch;

/// Version of the serialized [`PaneHandoff`]. Bump on any incompatible change; additive fields
/// use `serde(default)`.
pub const HANDOFF_SCHEMA: u32 = 1;

/// Visible screen and scrollback, as terminal byte streams that rebuild the emulator state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenState {
    /// Scrollback rows, oldest first, each rendered without a trailing newline.
    pub scrollback: Vec<Vec<u8>>,
    /// `Screen::state_formatted()` of the live screen.
    pub visible: Vec<u8>,
}

/// Everything the new image needs to adopt one live pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneHandoff {
    pub id: Uuid,
    pub command: String,
    pub launch: Launch,
    pub cwd: String,
    /// Inherited pty master; `FD_CLOEXEC` is clear in the old image until the exec.
    pub master_fd: RawFd,
    /// Inherited write end of a `stdin_stream` pipe.
    #[serde(default)]
    pub stdin_writer_fd: Option<RawFd>,
    pub child_pid: u32,
    pub generation: u64,
    pub rows: u16,
    pub cols: u16,
    pub output_seq: u64,
    pub replay: Vec<(u64, Vec<u8>)>,
    pub screen: ScreenState,
    /// Bytes of an unfinished escape sequence the query scanner was holding back. The emulator
    /// already consumed them, so the adopting image re-feeds both the emulator and the scanner.
    #[serde(default)]
    pub scanner_held: Vec<u8>,
}

/// State applied to a freshly built pane before its reader starts.
pub(super) struct Seed {
    pub(super) scanner_held: Vec<u8>,
    screen: ScreenState,
    output_seq: u64,
    replay: Vec<(u64, Vec<u8>)>,
    generation: u64,
}

impl Seed {
    /// Rebuilds the emulator and replay buffer; returns `(output_seq, generation)`.
    pub(super) fn apply(
        self,
        parser: &Mutex<vt100::Parser<QueryResponder>>,
        replay: &mut ReplayBuffer,
    ) -> (u64, u64) {
        let mut parser = parser.lock().unwrap();
        restore_screen(&mut parser, &self.screen);
        parser.process(&self.scanner_held);
        for (seq, chunk) in &self.replay {
            replay.push(*seq, chunk);
        }
        (self.output_seq, self.generation)
    }
}

/// `poll(2)` for readability; true on data, EOF or hangup (a read then reports which), false on
/// timeout or interruption.
pub(super) fn poll_readable(fd: RawFd, timeout_ms: i32) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd for the duration of the call.
    let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    rc > 0
}

/// Sets or clears `FD_CLOEXEC` on `fd`.
pub fn set_cloexec(fd: RawFd, cloexec: bool) -> std::io::Result<()> {
    // SAFETY: fcntl on a caller-owned descriptor; failure is reported through errno.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let wanted = if cloexec {
            flags | libc::FD_CLOEXEC
        } else {
            flags & !libc::FD_CLOEXEC
        };
        if wanted != flags && libc::fcntl(fd, libc::F_SETFD, wanted) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Clears `FD_CLOEXEC` so `fd` survives the next `exec`.
pub fn clear_cloexec(fd: RawFd) -> std::io::Result<()> {
    set_cloexec(fd, false)
}

fn restore_screen<CB: vt100::Callbacks>(parser: &mut vt100::Parser<CB>, state: &ScreenState) {
    // Each row followed by CRLF scrolls the earlier ones into scrollback; the rows still on the
    // screen are then pushed off with LFs at the bottom row before the live screen is painted.
    let (rows, _) = parser.screen().size();
    for line in &state.scrollback {
        parser.process(line);
        parser.process(b"\r\n");
    }
    let on_screen = state
        .scrollback
        .len()
        .min(usize::from(rows).saturating_sub(1));
    if on_screen > 0 {
        parser.process(format!("\x1b[{rows};1H").as_bytes());
        parser.process("\n".repeat(on_screen).as_bytes());
    }
    parser.process(&state.visible);
}

fn export_screen<CB: vt100::Callbacks>(parser: &mut vt100::Parser<CB>) -> ScreenState {
    let screen = parser.screen_mut();
    let (_, cols) = screen.size();
    screen.set_scrollback(usize::MAX);
    let len = screen.scrollback();
    let mut scrollback = Vec::with_capacity(len);
    for offset in (1..=len).rev() {
        screen.set_scrollback(offset);
        let top = screen.rows_formatted(0, cols).next().unwrap_or_default();
        scrollback.push(top);
    }
    screen.set_scrollback(0);
    ScreenState {
        scrollback,
        visible: screen.state_formatted(),
    }
}

/// A pty master adopted from an inherited descriptor; `portable-pty` cannot build one from a raw fd.
struct AdoptedMaster {
    fd: OwnedFd,
}

impl AdoptedMaster {
    fn dup_file(&self) -> Result<File, anyhow::Error> {
        Ok(File::from(self.fd.try_clone()?))
    }
}

impl MasterPty for AdoptedMaster {
    fn resize(&self, size: PtySize) -> Result<(), anyhow::Error> {
        let ws = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: size.pixel_width,
            ws_ypixel: size.pixel_height,
        };
        // SAFETY: TIOCSWINSZ reads one `winsize` from a valid pointer.
        let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) };
        ensure!(
            rc == 0,
            "TIOCSWINSZ failed: {}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }

    fn get_size(&self) -> Result<PtySize, anyhow::Error> {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: TIOCGWINSZ writes one `winsize` through a valid pointer.
        let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::TIOCGWINSZ as _, &mut ws) };
        ensure!(
            rc == 0,
            "TIOCGWINSZ failed: {}",
            std::io::Error::last_os_error()
        );
        Ok(PtySize {
            rows: ws.ws_row,
            cols: ws.ws_col,
            pixel_width: ws.ws_xpixel,
            pixel_height: ws.ws_ypixel,
        })
    }

    fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>, anyhow::Error> {
        Ok(Box::new(self.dup_file()?))
    }

    fn take_writer(&self) -> Result<Box<dyn Write + Send>, anyhow::Error> {
        Ok(Box::new(self.dup_file()?))
    }

    fn process_group_leader(&self) -> Option<libc::pid_t> {
        // SAFETY: tcgetpgrp on a valid descriptor.
        let pgrp = unsafe { libc::tcgetpgrp(self.fd.as_raw_fd()) };
        (pgrp > 0).then_some(pgrp)
    }

    fn as_raw_fd(&self) -> Option<RawFd> {
        Some(self.fd.as_raw_fd())
    }
}

/// A child of this process that the previous image spawned; same pid, same parent.
#[derive(Debug)]
struct AdoptedChild {
    pid: libc::pid_t,
}

fn waitpid(pid: libc::pid_t, flags: libc::c_int) -> std::io::Result<Option<ExitStatus>> {
    use std::os::unix::process::ExitStatusExt;
    let mut status = 0;
    loop {
        // SAFETY: waitpid with a valid out-pointer.
        let rc = unsafe { libc::waitpid(pid, &mut status, flags) };
        if rc == pid {
            return Ok(Some(std::process::ExitStatus::from_raw(status).into()));
        }
        if rc == 0 {
            return Ok(None);
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

impl Child for AdoptedChild {
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        waitpid(self.pid, libc::WNOHANG)
    }

    fn wait(&mut self) -> std::io::Result<ExitStatus> {
        waitpid(self.pid, 0)?.ok_or_else(|| std::io::Error::other("blocking waitpid had no status"))
    }

    fn process_id(&self) -> Option<u32> {
        Some(self.pid as u32)
    }
}

#[derive(Debug)]
struct AdoptedKiller(libc::pid_t);

impl ChildKiller for AdoptedKiller {
    fn kill(&mut self) -> std::io::Result<()> {
        // SAFETY: plain signal send.
        if unsafe { libc::kill(self.0, libc::SIGHUP) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(AdoptedKiller(self.0))
    }
}

impl ChildKiller for AdoptedChild {
    fn kill(&mut self) -> std::io::Result<()> {
        AdoptedKiller(self.pid).kill()
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(AdoptedKiller(self.pid))
    }
}

/// Takes ownership of an inherited descriptor after checking it is open, and re-arms
/// `FD_CLOEXEC` so later pane children do not inherit it.
fn take_inherited(fd: RawFd, what: &str) -> Result<OwnedFd> {
    // SAFETY: fcntl(F_GETFD) only inspects the descriptor.
    ensure!(
        unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0,
        "inherited {what} fd {fd} is not open"
    );
    set_cloexec(fd, true).with_context(|| format!("re-arming CLOEXEC on {what} fd {fd}"))?;
    // SAFETY: the descriptor is open and, by the handoff contract, owned by nobody else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

impl Pane {
    /// Stops the reader thread from consuming pty output and waits for a chunk it is in the
    /// middle of applying. Afterwards every byte the child has written is either in this pane's
    /// screen and replay buffer or still in the kernel's pty buffer. Undo with
    /// [`Self::unfreeze_reads`] or [`Self::abort_handoff`].
    pub fn freeze_reads(&self) {
        self.reads_frozen.store(true, Ordering::SeqCst);
        // The reader checks the flag under this lock, so once it is taken and released no
        // later chunk can be consumed.
        drop(self.read_gate.lock().unwrap());
    }

    pub fn unfreeze_reads(&self) {
        self.reads_frozen.store(false, Ordering::SeqCst);
    }

    /// Exports this frozen, live pane and clears `FD_CLOEXEC` on its master (and `stdin_stream`
    /// pipe) so they survive `exec`. Call [`Self::abort_handoff`] if the exec does not happen.
    ///
    /// The caller must stop spawning children (respawn, new panes, any `Command`) from the first
    /// export until the exec or abort, or a child inherits the exported masters. An error means
    /// this pane was left as it was, but earlier panes still need `abort_handoff`.
    pub fn export_handoff(&self) -> Result<PaneHandoff> {
        ensure!(
            self.reads_frozen.load(Ordering::SeqCst),
            "freeze_reads must precede export_handoff"
        );
        ensure!(
            !self.is_exited() && !self.restart_pending() && !self.eof_seen.load(Ordering::SeqCst),
            "pane {} has no live process to hand off",
            self.id
        );
        let child_pid = self.pid.load(Ordering::SeqCst);
        ensure!(child_pid != 0, "pane {} has no pid", self.id);
        let master_fd = self
            .master
            .lock()
            .unwrap()
            .as_raw_fd()
            .context("pty master has no raw fd")?;
        let stdin_writer_fd = self
            .stdin_writer
            .lock()
            .unwrap()
            .as_ref()
            .map(AsRawFd::as_raw_fd);
        let (rows, cols) = self.size();
        let (screen, output_seq, replay) = {
            let mut parser = self.parser.lock().unwrap();
            let replay = self.replay.lock().unwrap().entries();
            let seq = self.output_seq.load(Ordering::SeqCst);
            (export_screen(&mut parser), seq, replay)
        };
        let scanner_held = self.scanner_held.lock().unwrap().clone();
        clear_cloexec(master_fd)?;
        if let Some(fd) = stdin_writer_fd {
            if let Err(e) = clear_cloexec(fd) {
                let _ = set_cloexec(master_fd, true);
                return Err(e.into());
            }
        }
        Ok(PaneHandoff {
            id: self.id,
            command: self.command.clone(),
            launch: self.launch.clone(),
            cwd: self.cwd.clone(),
            master_fd,
            stdin_writer_fd,
            child_pid,
            generation: self.generation.load(Ordering::SeqCst),
            rows: rows as u16,
            cols: cols as u16,
            output_seq,
            replay,
            screen,
            scanner_held,
        })
    }

    /// Undoes [`Self::freeze_reads`] and [`Self::export_handoff`] after a failed or cancelled exec.
    pub fn abort_handoff(&self) {
        if let Some(fd) = self.master.lock().unwrap().as_raw_fd() {
            let _ = set_cloexec(fd, true);
        }
        if let Some(file) = self.stdin_writer.lock().unwrap().as_ref() {
            let _ = set_cloexec(file.as_raw_fd(), true);
        }
        self.unfreeze_reads();
    }

    /// Rebuilds a pane in the new daemon image from `handoff`, taking ownership of the inherited
    /// descriptors. The child must be a child of this process (true across `exec`).
    pub fn from_adopted(handoff: PaneHandoff) -> Result<Arc<Self>> {
        let master = AdoptedMaster {
            fd: take_inherited(handoff.master_fd, "pty master")?,
        };
        let stdin_writer = match handoff.stdin_writer_fd {
            Some(fd) => Some(File::from(take_inherited(fd, "stdin pipe")?)),
            None => None,
        };
        // SAFETY: the pid was recorded by the previous image and is verified below.
        let alive = unsafe { libc::kill(handoff.child_pid as libc::pid_t, 0) };
        if alive != 0 {
            bail!("adopted child {} is gone", handoff.child_pid);
        }
        let reader = master.try_clone_reader()?;
        let writer = master.take_writer()?;
        // SAFETY: the master fd is owned by `master` for this call.
        let reply_fd =
            unsafe { BorrowedFd::borrow_raw(master.fd.as_raw_fd()) }.try_clone_to_owned()?;
        let process = StartedProcess {
            child: Box::new(AdoptedChild {
                pid: handoff.child_pid as libc::pid_t,
            }),
            pid: Some(handoff.child_pid),
            master: Box::new(master),
            writer,
            reader,
            stdin_writer,
            reply_fd,
        };
        let seed = Seed {
            scanner_held: handoff.scanner_held,
            screen: handoff.screen,
            output_seq: handoff.output_seq,
            replay: handoff.replay,
            generation: handoff.generation,
        };
        Ok(Self::assemble(
            &handoff.command,
            &handoff.launch,
            Some(&handoff.cwd),
            Some(handoff.id),
            handoff.rows,
            handoff.cols,
            process,
            Some(seed),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROWS: u16 = 6;
    const COLS: u16 = 24;

    /// Every scrollback offset and the live screen must be cell-for-cell identical after a round trip.
    fn assert_round_trips(input: &[u8], scrollback_len: usize) {
        let mut original = vt100::Parser::new(ROWS, COLS, scrollback_len);
        original.process(input);
        let state = export_screen(&mut original);

        let mut restored = vt100::Parser::new(ROWS, COLS, scrollback_len);
        restore_screen(&mut restored, &state);

        original.screen_mut().set_scrollback(usize::MAX);
        let depth = original.screen().scrollback();
        assert_eq!(depth, state.scrollback.len());
        for offset in 0..=depth {
            original.screen_mut().set_scrollback(offset);
            restored.screen_mut().set_scrollback(offset);
            assert_eq!(
                fingerprint(original.screen()),
                fingerprint(restored.screen()),
                "screen differs at scrollback offset {offset}"
            );
        }
    }

    /// Formatted cells (attributes included), plain text and cursor of the view.
    fn fingerprint(screen: &vt100::Screen) -> (Vec<u8>, String, (u16, u16)) {
        (
            screen.state_formatted(),
            screen.contents(),
            screen.cursor_position(),
        )
    }

    fn numbered_lines(n: usize) -> Vec<u8> {
        (0..n)
            .flat_map(|i| format!("line {i}\r\n").into_bytes())
            .collect()
    }

    #[test]
    fn round_trips_a_fresh_screen() {
        assert_round_trips(b"", 100);
    }

    #[test]
    fn round_trips_content_that_fits_without_scrollback() {
        assert_round_trips(&numbered_lines(3), 100);
    }

    #[test]
    fn round_trips_scrollback_and_screen() {
        assert_round_trips(&numbered_lines(40), 100);
    }

    #[test]
    fn round_trips_attributes_cursor_and_wide_glyphs() {
        let mut input = numbered_lines(10);
        input.extend_from_slice(
            b"\x1b[1;31mbold red\x1b[0m \x1b[4mul\x1b[0m \x1b[38;2;10;20;30mrgb\x1b[0m\r\n\xe4\xb8\xad\xe6\x96\x87 wide\r\n\x1b[3;5H",
        );
        assert_round_trips(&input, 100);
    }

    #[test]
    fn round_trips_a_wrapped_long_line() {
        let mut input = numbered_lines(8);
        input.extend_from_slice("x".repeat(usize::from(COLS) * 2 + 5).as_bytes());
        assert_round_trips(&input, 100);
    }

    #[test]
    fn round_trips_when_scrollback_was_truncated() {
        assert_round_trips(&numbered_lines(60), 10);
    }
}
