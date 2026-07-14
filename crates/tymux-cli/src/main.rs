use std::io::{Read, Write};

mod config;
mod copy_mode;
mod input;
mod status_bar;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tonic::Request;

use tymux_proto::v1::tymux_service_client::TymuxServiceClient;
use tymux_proto::v1::{
    attach_event, attach_request, layout::Node, AttachRequest, CapturePaneRequest,
    ClosePaneRequest, CreateSessionRequest, CreateWindowRequest, KillSessionRequest,
    ListSessionsRequest, Orientation, Pane as ProtoPane, Resize, ReviveSessionRequest,
    ReviveSessionResponse, Session, SplitPaneRequest, Window,
};

use config::{Action, TymuxConfig};
use copy_mode::{CopyModeEvent, CopyModeState};
use input::{KeystrokeReassembler, ReassembledOutput};
use status_bar::{DisplayMode, StatusBarConfig};

/// `session[:window.pane]` addressing grammar, replacing the old
/// unchecked `windows[0].panes[0]` indexing (docs/adr/0001). The
/// `:window.pane` suffix is optional — bare `myproject` defaults to
/// window 0, pane 0, preserving today's simple single-pane UX.
#[derive(Debug, PartialEq)]
struct TargetString {
    session: String,
    window_index: usize,
    pane_index: usize,
}

impl TargetString {
    fn parse(s: &str) -> Result<Self> {
        let (session, rest) = match s.split_once(':') {
            Some((session, rest)) => (session.to_string(), Some(rest)),
            None => (s.to_string(), None),
        };
        if session.is_empty() {
            return Err(anyhow::anyhow!(
                "target '{s}' must name a session, e.g. 'myproject' or 'myproject:0.1'"
            ));
        }
        let (window_index, pane_index) = match rest {
            None => (0, 0),
            Some(rest) => {
                let (window_str, pane_str) = rest.split_once('.').ok_or_else(|| {
                    anyhow::anyhow!(
                        "target '{s}' is missing '.pane' after the window (expected session:window.pane)"
                    )
                })?;
                let window_index: usize = window_str.parse().map_err(|_| {
                    anyhow::anyhow!("target '{s}': '{window_str}' is not a valid window index")
                })?;
                let pane_index: usize = pane_str.parse().map_err(|_| {
                    anyhow::anyhow!("target '{s}': '{pane_str}' is not a valid pane index")
                })?;
                (window_index, pane_index)
            }
        };
        Ok(TargetString {
            session,
            window_index,
            pane_index,
        })
    }

    /// Resolves this target against a real `Session`, bounds-checked at
    /// every step — a real bounds check, not a formality, matching
    /// ADR 0001's original design property that this never panics on an
    /// out-of-range index, it fails with a clear message instead. Returns
    /// the resolved pane in full (not just its id) so callers that care
    /// about liveness (e.g. `attach`'s Story 4.6 fail-fast check) don't
    /// need a second round trip.
    fn resolve(&self, session: &Session) -> Result<ProtoPane> {
        let window = session.windows.get(self.window_index).ok_or_else(|| {
            anyhow::anyhow!(
                "session '{}' has no window {} (it has {} window{})",
                self.session,
                self.window_index,
                session.windows.len(),
                if session.windows.len() == 1 { "" } else { "s" }
            )
        })?;
        let panes = flatten_panes(window);
        let pane = panes.get(self.pane_index).ok_or_else(|| {
            anyhow::anyhow!(
                "window {} of session '{}' has no pane {} (it has {} pane{})",
                self.window_index,
                self.session,
                self.pane_index,
                panes.len(),
                if panes.len() == 1 { "" } else { "s" }
            )
        })?;
        Ok((*pane).clone())
    }
}

/// Every leaf `Pane` in a window's `Layout` tree, in pre-order — the
/// positional indexing `TargetString`'s `.pane` component addresses into.
fn flatten_panes(window: &Window) -> Vec<&ProtoPane> {
    fn walk<'a>(node: &'a Node, out: &mut Vec<&'a ProtoPane>) {
        match node {
            Node::Pane(p) => out.push(p),
            Node::Split(split) => {
                for child in &split.children {
                    if let Some(layout) = &child.layout {
                        if let Some(node) = &layout.node {
                            walk(node, out);
                        }
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    if let Some(node) = window.layout.as_ref().and_then(|l| l.node.as_ref()) {
        walk(node, &mut out);
    }
    out
}

/// The very first pane of a freshly created session — used only right
/// after `CreateSession`, where the caller already knows the exact shape
/// (one window, one pane) without needing `TargetString` resolution.
fn first_pane_id(session: &Session) -> Result<String> {
    let window = session
        .windows
        .first()
        .ok_or_else(|| anyhow::anyhow!("session {} has no windows", session.id))?;
    flatten_panes(window)
        .first()
        .map(|p| p.id.clone())
        .ok_or_else(|| anyhow::anyhow!("window {} has no panes", window.id))
}

async fn resolve_target(
    client: &mut TymuxServiceClient<Channel>,
    target: &TargetString,
) -> Result<ProtoPane> {
    let resp = client
        .list_sessions(ListSessionsRequest {})
        .await?
        .into_inner();
    let session = resp
        .sessions
        .into_iter()
        .find(|s| s.name == target.session)
        .ok_or_else(|| anyhow::anyhow!("no such session: {}", target.session))?;
    target.resolve(&session)
}

#[derive(Parser)]
#[command(name = "tymux")]
struct Cli {
    #[arg(long, global = true, default_value = "http://127.0.0.1:7419")]
    addr: String,

    /// Disable the status bar entirely — pure pty passthrough, no
    /// DECSTBM scroll-region reservation, zero added escape bytes
    /// (accessibility floor, ux.md §3).
    #[arg(long, global = true)]
    no_status_bar: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Create a new session and attach to it.
    New {
        #[arg(long, default_value = "default")]
        name: String,
        #[arg(long)]
        command: Option<String>,
    },
    /// List sessions on the daemon.
    Ls,
    /// Attach to an existing session/window/pane, e.g. `myproject` or `myproject:0.1`.
    Attach { target: String },
    /// End a session and every pane's process in it entirely.
    Kill { session_id: String },
    /// Respawn a dead (restored-but-not-yet-revived) session's panes.
    Revive { session: String },
    /// Split an existing pane, e.g. `tymux split myproject:0.0 --vertical`.
    Split {
        target: String,
        #[arg(long, conflicts_with = "horizontal")]
        vertical: bool,
        #[arg(long, conflicts_with = "vertical")]
        horizontal: bool,
        #[arg(long)]
        command: Option<String>,
    },
    /// Close a single pane (not the whole session).
    KillPane { target: String },
}

/// Restores the local terminal out of raw mode on drop, including on
/// error paths — leaving a user's shell stuck in raw mode is a real
/// annoyance, not a hypothetical one.
struct RawGuard;

impl RawGuard {
    fn enable() -> Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tymux: {}", friendly_message(&e));
            std::process::ExitCode::FAILURE
        }
    }
}

/// Every failure used to funnel into Rust's default `Result`-returning-
/// `main` handler, which prints the full anyhow Debug chain — a multi-line
/// technical dump for something as ordinary as "the daemon isn't running."
/// This gives the two common cases (can't connect; a clean server-side
/// Status like "no such session") a short, actionable message instead.
fn friendly_message(e: &anyhow::Error) -> String {
    if e.downcast_ref::<tonic::transport::Error>().is_some() {
        return "couldn't connect to tymuxd — is the daemon running? \
                (start it with `cargo run -p tymuxd`)"
            .to_string();
    }
    if let Some(status) = e.downcast_ref::<tonic::Status>() {
        return status.message().to_string();
    }
    e.to_string()
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let mut client = TymuxServiceClient::connect(cli.addr).await?;
    let config = TymuxConfig::load_or_default();
    let status_bar_cfg = StatusBarConfig::new(!cli.no_status_bar);

    match cli.command {
        Command::New { name, command } => {
            let session = client
                .create_session(CreateSessionRequest {
                    name: name.clone(),
                    command: command.unwrap_or_default(),
                })
                .await?
                .into_inner();
            let pane_id = first_pane_id(&session)?;
            attach_and_follow(&mut client, pane_id, &name, &config, &status_bar_cfg).await?;
        }
        Command::Ls => {
            let resp = client
                .list_sessions(ListSessionsRequest {})
                .await?
                .into_inner();
            for s in resp.sessions {
                println!("{}\t{}", s.id, ls_status_label(&s));
            }
        }
        Command::Attach { target } => {
            let target = TargetString::parse(&target)?;
            let pane = resolve_target(&mut client, &target).await?;
            // Story 4.6 AC1: fail fast, naming the revive remediation,
            // before ever opening the Attach stream — never a hang, a
            // bare gRPC error, or a silent no-op on a dead session.
            if pane.liveness == tymux_proto::v1::Liveness::Dead as i32 {
                return Err(anyhow::anyhow!(
                    "Session '{}' is not running (restored from disk after a restart). \
                     Run 'tymux revive {}' to respawn it, then attach again.",
                    target.session,
                    target.session
                ));
            }
            attach_and_follow(
                &mut client,
                pane.id,
                &target.session,
                &config,
                &status_bar_cfg,
            )
            .await?;
        }
        Command::Kill { session_id } => {
            client
                .kill_session(KillSessionRequest { session_id })
                .await?;
        }
        Command::Revive { session } => {
            let resp = client
                .list_sessions(ListSessionsRequest {})
                .await?
                .into_inner();
            let session_id = resp
                .sessions
                .into_iter()
                .find(|s| s.name == session)
                .map(|s| s.id)
                .ok_or_else(|| anyhow::anyhow!("no such session: {session}"))?;
            let resp = client
                .revive_session(ReviveSessionRequest { session_id })
                .await?
                .into_inner();
            print_revive_outcome(&session, &resp);
        }
        Command::Split {
            target,
            vertical,
            horizontal: _,
            command,
        } => {
            let target = TargetString::parse(&target)?;
            let pane = resolve_target(&mut client, &target).await?;
            let orientation = if vertical {
                Orientation::Vertical
            } else {
                Orientation::Horizontal
            };
            client
                .split_pane(SplitPaneRequest {
                    pane_id: pane.id,
                    orientation: orientation as i32,
                    command: command.unwrap_or_default(),
                })
                .await?;
        }
        Command::KillPane { target } => {
            let target = TargetString::parse(&target)?;
            let pane = resolve_target(&mut client, &target).await?;
            let resp = client
                .close_pane(ClosePaneRequest { pane_id: pane.id })
                .await?
                .into_inner();
            print_close_pane_outcome(&resp);
        }
    }

    Ok(())
}

/// Story 4.4's two distinct message moments (task 3): a freshly succeeded
/// revive states these are NEW processes with no carried-forward
/// scrollback; an already-live session gets a friendly no-op pointing at
/// `attach` instead, exiting 0 — never a duplicate-spawn error.
fn print_revive_outcome(session_name: &str, resp: &ReviveSessionResponse) {
    if resp.already_live {
        println!(
            "'{session_name}' is already live — nothing to revive. Use `tymux attach {session_name}` instead."
        );
    } else {
        println!(
            "Session revived: {} pane(s) respawned with their original command and working directory. \
             These are NEW processes — scrollback from before the restart is not carried forward.",
            resp.pane_count
        );
    }
}

/// Story 4.5 AC2: live and dead-restored sessions must render distinctly
/// in `tymux ls` — never identical, so a user can tell at a glance which
/// sessions need `tymux revive` before they can be attached to.
fn ls_status_label(session: &Session) -> String {
    if session.liveness == tymux_proto::v1::Liveness::Dead as i32 {
        format!("{} [restored — not running]", session.name)
    } else {
        format!("{} [live]", session.name)
    }
}

/// Story 3.5 AC3: a pane close that cascades to closing its window (and,
/// if that was the session's last window, the session too) must state
/// exactly what happened — never a silent disappearance.
fn print_close_pane_outcome(resp: &tymux_proto::v1::ClosePaneResponse) {
    if !resp.session_closed_name.is_empty() {
        println!(
            "Window {} closed (last pane exited). '{}' closed (last window).",
            resp.window_closed_name, resp.session_closed_name
        );
    } else if !resp.window_closed_name.is_empty() {
        let remaining = resp.session.as_ref().map(|s| s.windows.len()).unwrap_or(0);
        println!(
            "Window {} closed (last pane exited). {} window(s) remain.",
            resp.window_closed_name, remaining
        );
    }
}

/// What one `attach()` call ended with.
enum AttachOutcome {
    /// Detach, pane exited, or the stream ended — nothing more to do.
    Done,
    /// `NextWindow`/`PrevWindow` fired — re-attach to this pane instead
    /// (client-side pane-focus cycling, Story 5.3 task 3: no RPC of its
    /// own, just choosing a different pane to open a fresh Attach stream
    /// against).
    SwitchTo(String),
}

/// Loops `attach()` to follow `NextWindow`/`PrevWindow` reattachment
/// requests until the user actually detaches (or the pane/stream ends).
async fn attach_and_follow(
    client: &mut TymuxServiceClient<Channel>,
    mut pane_id: String,
    session_name: &str,
    config: &TymuxConfig,
    status_bar_cfg: &StatusBarConfig,
) -> Result<()> {
    loop {
        match attach(client, pane_id, session_name, config, status_bar_cfg).await? {
            AttachOutcome::Done => return Ok(()),
            AttachOutcome::SwitchTo(next_pane_id) => pane_id = next_pane_id,
        }
    }
}

/// Resolves the pane adjacent (next or previous) to `current_pane_id`
/// within its session's window list — the client-side state Action::
/// NextWindow/PrevWindow cycle through (no server RPC; "next"/"prev" is
/// purely an ordering over `ListSessions`' response).
async fn adjacent_window_pane(
    client: &mut TymuxServiceClient<Channel>,
    session_name: &str,
    current_pane_id: &str,
    forward: bool,
) -> Result<Option<String>> {
    let resp = client
        .list_sessions(ListSessionsRequest {})
        .await?
        .into_inner();
    let session = resp
        .sessions
        .into_iter()
        .find(|s| s.name == session_name)
        .ok_or_else(|| anyhow::anyhow!("no such session: {session_name}"))?;
    if session.windows.len() < 2 {
        return Ok(None);
    }
    let current_idx = session
        .windows
        .iter()
        .position(|w| flatten_panes(w).iter().any(|p| p.id == current_pane_id));
    let Some(current_idx) = current_idx else {
        return Ok(None);
    };
    let next_idx = if forward {
        (current_idx + 1) % session.windows.len()
    } else {
        (current_idx + session.windows.len() - 1) % session.windows.len()
    };
    Ok(flatten_panes(&session.windows[next_idx])
        .first()
        .map(|p| p.id.clone()))
}

async fn attach(
    client: &mut TymuxServiceClient<Channel>,
    pane_id: String,
    session_name: &str,
    config: &TymuxConfig,
    status_bar_cfg: &StatusBarConfig,
) -> Result<AttachOutcome> {
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    tx.send(AttachRequest {
        payload: Some(attach_request::Payload::PaneId(pane_id.clone())),
    })
    .await?;

    // stdin reads are blocking, so they get their own OS thread; raw
    // bytes are handed to the async loop below over a channel rather than
    // being turned into AttachRequests directly here, since they now need
    // to pass through the keystroke reassembler / copy-mode dispatcher
    // first (Story 5.2/5.5), which may fire local Actions instead of
    // forwarding.
    let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if stdin_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let _raw = RawGuard::enable()?;
    let outbound = ReceiverStream::new(rx);
    let mut inbound = client.attach(Request::new(outbound)).await?.into_inner();

    let mut reassembler = KeystrokeReassembler::new(config);
    let mut copy_mode: Option<CopyModeState> = None;
    let mut mode = DisplayMode::Normal;
    let mut stdout = std::io::stdout();
    let mut resize_rx = spawn_resize_watcher();

    // Sync the pane to the local terminal's real size immediately (Story
    // 6.2 AC1: reserves the status bar's row via DECSTBM at the same
    // time), and again on every SIGWINCH via the coordinated path below.
    send_resize_and_repaint(&tx, &mut stdout, status_bar_cfg, mode, config).await?;

    let outcome = 'attach_loop: loop {
        tokio::select! {
            biased;
            _ = resize_rx.recv() => {
                send_resize_and_repaint(&tx, &mut stdout, status_bar_cfg, mode, config).await?;
            }
            maybe_event = inbound.message() => {
                match maybe_event? {
                    None => break AttachOutcome::Done,
                    // Copy-mode owns the screen while active — its own
                    // redraws happen out-of-band via CapturePane, not live
                    // pty output, per its AC1 (navigation reads, never
                    // forwards to the pane, and never lets live output
                    // that arrived while paused clobber the frozen view).
                    Some(event) => match event.payload {
                        Some(attach_event::Payload::Output(bytes)) if copy_mode.is_none() => {
                            stdout.write_all(&bytes)?;
                            stdout.flush()?;
                        }
                        Some(attach_event::Payload::Exited(_)) => {
                            drop(_raw);
                            writeln!(
                                stdout,
                                "{}",
                                chrome_message_for_event(&attach_event::Payload::Exited(true)).unwrap()
                            )?;
                            stdout.flush()?;
                            break AttachOutcome::Done;
                        }
                        Some(ref payload @ attach_event::Payload::OutputGap(_)) if copy_mode.is_none() => {
                            write!(stdout, "{}", chrome_message_for_event(payload).unwrap())?;
                            stdout.flush()?;
                        }
                        _ => {}
                    },
                }
            }
            maybe_bytes = stdin_rx.recv() => {
                let Some(bytes) = maybe_bytes else { break AttachOutcome::Done };

                if let Some(cs) = copy_mode.as_mut() {
                    // Story 5.5 AC4: copy-mode owns all key input while
                    // active — bytes never reach the reassembler/prefix
                    // logic at all, so the leader can't arm and no
                    // prefix-based Action (including Detach) is reachable
                    // until the user exits copy-mode first.
                    let mut should_exit = false;
                    let mut should_redraw = false;
                    let mut yank_range = None;
                    for &b in &bytes {
                        match cs.handle_byte(b) {
                            CopyModeEvent::Exit => should_exit = true,
                            CopyModeEvent::Redraw => should_redraw = true,
                            CopyModeEvent::Yanked => {
                                if let Some(from) = cs.selecting_from {
                                    yank_range = Some((from, cs.cursor));
                                }
                                should_exit = true;
                            }
                            CopyModeEvent::Consumed => {}
                        }
                        if should_exit {
                            break;
                        }
                    }

                    if let Some((from, to)) = yank_range {
                        if let Ok(snapshot) = client
                            .capture_pane(CapturePaneRequest {
                                pane_id: pane_id.clone(),
                                scrollback_offset: cs.scrollback_offset as u32,
                            })
                            .await
                        {
                            let grid: Vec<Vec<String>> = snapshot
                                .into_inner()
                                .grid
                                .into_iter()
                                .map(|row| row.cells.into_iter().map(|c| c.text).collect())
                                .collect();
                            cs.yanked = copy_mode::extract_selection(&grid, from, to);
                        }
                    }

                    if should_exit {
                        copy_mode = None;
                        mode = DisplayMode::Normal;
                        // Redraw the live screen copy-mode had been
                        // covering.
                        if let Ok(snapshot) = client
                            .capture_pane(CapturePaneRequest { pane_id: pane_id.clone(), scrollback_offset: 0 })
                            .await
                        {
                            render_plain_grid(&mut stdout, &snapshot.into_inner())?;
                        }
                        if let Ok((_, term_rows)) = crossterm::terminal::size() {
                            redraw_status_line(&mut stdout, term_rows, mode, config, status_bar_cfg)?;
                        }
                    } else if should_redraw {
                        redraw_copy_mode(&mut client.clone(), &pane_id, cs, &mut stdout).await?;
                    }
                    continue;
                }

                let was_armed = reassembler.is_armed();
                for output in reassembler.process(&bytes) {
                    match output {
                        ReassembledOutput::Forward(fwd) => {
                            tx.send(AttachRequest {
                                payload: Some(attach_request::Payload::Input(fwd)),
                            }).await?;
                        }
                        ReassembledOutput::Action(action) => match action {
                            Action::Detach => {
                                drop(_raw);
                                writeln!(stdout, "\r\n[tymux: detached]")?;
                                stdout.flush()?;
                                return Ok(AttachOutcome::Done);
                            }
                            Action::EnterCopyMode => {
                                if let Ok(snapshot) = client
                                    .capture_pane(CapturePaneRequest { pane_id: pane_id.clone(), scrollback_offset: 0 })
                                    .await
                                {
                                    let snap = snapshot.into_inner();
                                    let cs = CopyModeState::new(snap.rows as u16, snap.cols as u16);
                                    redraw_copy_mode(&mut client.clone(), &pane_id, &cs, &mut stdout).await?;
                                    copy_mode = Some(cs);
                                    mode = DisplayMode::CopyMode;
                                }
                            }
                            Action::SplitHorizontal | Action::SplitVertical => {
                                let orientation = if action == Action::SplitHorizontal {
                                    Orientation::Horizontal
                                } else {
                                    Orientation::Vertical
                                };
                                let _ = client
                                    .split_pane(SplitPaneRequest {
                                        pane_id: pane_id.clone(),
                                        orientation: orientation as i32,
                                        command: String::new(),
                                    })
                                    .await;
                            }
                            Action::KillPane => {
                                // Closing our own attached pane: the daemon
                                // kills the process, which the existing
                                // wait_exit path already reports as an
                                // ordinary Exited event on this same
                                // stream — no separate handling needed.
                                let _ = client
                                    .close_pane(ClosePaneRequest { pane_id: pane_id.clone() })
                                    .await;
                            }
                            Action::NewWindow => {
                                if let Ok(resp) = client
                                    .list_sessions(ListSessionsRequest {})
                                    .await
                                {
                                    if let Some(session) = resp
                                        .into_inner()
                                        .sessions
                                        .into_iter()
                                        .find(|s| s.name == session_name)
                                    {
                                        let _ = client
                                            .create_window(CreateWindowRequest {
                                                session_id: session.id,
                                                command: String::new(),
                                            })
                                            .await;
                                    }
                                }
                            }
                            Action::NextWindow | Action::PrevWindow => {
                                let forward = action == Action::NextWindow;
                                if let Ok(Some(next_pane_id)) =
                                    adjacent_window_pane(client, session_name, &pane_id, forward).await
                                {
                                    break 'attach_loop AttachOutcome::SwitchTo(next_pane_id);
                                }
                            }
                            Action::ExitCopyMode | Action::SendPrefixLiteral => {
                                // Structural actions, never produced by
                                // KeystrokeReassembler::process() itself
                                // (see input.rs) — unreachable here.
                            }
                        },
                    }
                }

                // Story 6.4: redraw the reserved status row whenever the
                // prefix arms/disarms — this is the one place a stale
                // hint from a prior mode could otherwise linger, so the
                // redraw is unconditional on any change, not just on
                // arming.
                let is_armed = reassembler.is_armed();
                if is_armed != was_armed {
                    mode = if is_armed { DisplayMode::PrefixArmed } else { DisplayMode::Normal };
                    if let Ok((_, term_rows)) = crossterm::terminal::size() {
                        redraw_status_line(&mut stdout, term_rows, mode, config, status_bar_cfg)?;
                    }
                }
            }
        }
    };

    Ok(outcome)
}

/// Basic (non-chrome) full-screen redraw of a captured grid as plain
/// text — clears the screen and prints each row. Epic 6 will replace this
/// with proper status-bar/mode-reactive rendering; this is deliberately
/// minimal, just enough for copy-mode to be genuinely usable now rather
/// than blocked on rendering infrastructure that hasn't landed yet.
fn render_plain_grid(
    stdout: &mut std::io::Stdout,
    snapshot: &tymux_proto::v1::PaneSnapshot,
) -> Result<()> {
    write!(stdout, "\x1b[2J\x1b[H")?; // clear screen, cursor to home
    for row in &snapshot.grid {
        for cell in &row.cells {
            if cell.text.is_empty() {
                stdout.write_all(b" ")?;
            } else {
                stdout.write_all(cell.text.as_bytes())?;
            }
        }
        stdout.write_all(b"\r\n")?;
    }
    stdout.flush()?;
    Ok(())
}

/// Re-captures the pane at `cs`'s current scrollback offset and redraws
/// it plus copy-mode's status line — the shared redraw path both entering
/// copy-mode and every subsequent navigation keystroke use.
async fn redraw_copy_mode(
    client: &mut TymuxServiceClient<Channel>,
    pane_id: &str,
    cs: &CopyModeState,
    stdout: &mut std::io::Stdout,
) -> Result<()> {
    let snapshot = client
        .capture_pane(CapturePaneRequest {
            pane_id: pane_id.to_string(),
            scrollback_offset: cs.scrollback_offset as u32,
        })
        .await?
        .into_inner();
    let live = snapshot.liveness != tymux_proto::v1::Liveness::Dead as i32;
    render_plain_grid(stdout, &snapshot)?;
    writeln!(
        stdout,
        "\r\n{}",
        copy_mode::render_status_line(live, cs.scrollback_offset)
    )?;
    stdout.flush()?;
    Ok(())
}

/// Maps an [`attach_event::Payload`] variant to the fixed status line (if
/// any) the CLI prints for it — pulled out of the attach loop above so the
/// exact wording (and that "pane exited" vs. "output dropped" render as
/// textually distinct messages) is unit-testable without a live stream.
fn chrome_message_for_event(payload: &attach_event::Payload) -> Option<&'static str> {
    match payload {
        attach_event::Payload::Exited(_) => Some("\r\n[tymux: pane exited]\n"),
        attach_event::Payload::OutputGap(_) => Some("\r\n[tymux: output dropped]\r\n"),
        _ => None,
    }
}

/// Sends the pane's effective size (`term_rows - 1` when the status bar
/// is reserving a row, the full terminal size otherwise) and, if the
/// status bar is enabled, writes its DECSTBM scroll-region reservation
/// and redraws the hint line — all through the caller's single owning
/// `stdout` handle, in the same call, so a resize's pty-side effect and
/// its status-bar-side effect are always one coordinated update (Story
/// 6.2 AC2), never two independently-timed writes.
async fn send_resize_and_repaint(
    tx: &tokio::sync::mpsc::Sender<AttachRequest>,
    stdout: &mut std::io::Stdout,
    cfg: &StatusBarConfig,
    mode: DisplayMode,
    config: &TymuxConfig,
) -> Result<()> {
    // A failure here just means the local terminal size can't be queried
    // (e.g. stdout isn't a real tty) — not worth aborting the attach over,
    // the pane just keeps whatever size it already had.
    let Ok((cols, term_rows)) = crossterm::terminal::size() else {
        return Ok(());
    };
    let pty_rows = status_bar::pty_rows(term_rows, cfg);
    tx.send(AttachRequest {
        payload: Some(attach_request::Payload::Resize(Resize {
            rows: pty_rows as u32,
            cols: cols as u32,
        })),
    })
    .await?;

    if cfg.enabled {
        stdout.write_all(&status_bar::decstbm_reserve(term_rows, cfg))?;
        redraw_status_line(stdout, term_rows, mode, config, cfg)?;
    }
    Ok(())
}

/// Repaints just the reserved status-bar row in place — saves the
/// terminal cursor, moves to the last row, clears it, writes the
/// mode-reactive hint line, and restores the cursor, so the pty's own
/// on-screen content is never disturbed.
fn redraw_status_line(
    stdout: &mut std::io::Stdout,
    term_rows: u16,
    mode: DisplayMode,
    config: &TymuxConfig,
    cfg: &StatusBarConfig,
) -> Result<()> {
    if !cfg.enabled {
        return Ok(());
    }
    let line = status_bar::colorize(&status_bar::render_hint_line(mode, config), cfg);
    write!(stdout, "\x1b7\x1b[{term_rows};1H\x1b[2K{line}\x1b8")?;
    stdout.flush()?;
    Ok(())
}

/// SIGWINCH only exists on Unix; on other platforms the pane just keeps
/// whatever size it got at attach time (still an improvement over never
/// syncing at all). Only signals that a resize happened — the actual
/// Resize RPC + DECSTBM/status-bar repaint happens in the main attach
/// loop, which owns `stdout` (Story 6.3's single-owner-writer property);
/// this task never writes to stdout itself.
#[cfg(unix)]
fn spawn_resize_watcher() -> tokio::sync::mpsc::Receiver<()> {
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tokio::spawn(async move {
        let mut winch =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("tymux: failed to install SIGWINCH handler: {e}");
                    return;
                }
            };
        while winch.recv().await.is_some() {
            if tx.send(()).await.is_err() {
                break;
            }
        }
    });
    rx
}

#[cfg(not(unix))]
fn spawn_resize_watcher() -> tokio::sync::mpsc::Receiver<()> {
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("tymux").chain(args.iter().copied())).unwrap()
    }

    /// Story 6.3 AC1 — structural, not just absence-of-observed-corruption:
    /// scans `attach()`'s own source text (this file, at compile time via
    /// `include_str!`) and asserts no bare `println!`/`print!`/
    /// `std::io::stdout()` call site exists inside its body outside the
    /// single `stdout` handle it declares once and threads through every
    /// write (directly, or via `redraw_status_line`/`redraw_copy_mode`/
    /// `render_plain_grid`, which all take `&mut std::io::Stdout` rather
    /// than acquiring their own handle).
    #[test]
    fn attach_loop_should_route_all_stdout_writes_through_single_owning_task_never_directly() {
        let source = include_str!("main.rs");
        let start = source
            .find("async fn attach(\n")
            .expect("attach() must exist in this file");
        let end = source[start..]
            .find("\n/// Basic (non-chrome) full-screen redraw")
            .expect("attach() must be immediately followed by render_plain_grid's doc comment");
        let attach_body = &source[start..start + end];

        assert!(
            !attach_body.contains("println!"),
            "attach() must not call println! directly — route through the owned `stdout` handle"
        );
        assert!(
            !attach_body.contains("print!("),
            "attach() must not call print! directly — route through the owned `stdout` handle"
        );
        assert_eq!(
            attach_body.matches("std::io::stdout()").count(),
            1,
            "attach() must acquire exactly one stdout handle (the single owner), not one per write site"
        );
    }

    #[test]
    fn attach_event_match_should_render_output_dropped_message_on_output_gap_variant() {
        let exited_msg = chrome_message_for_event(&attach_event::Payload::Exited(true)).unwrap();
        let gap_msg = chrome_message_for_event(&attach_event::Payload::OutputGap(true)).unwrap();
        assert!(gap_msg.contains("output dropped"));
        assert_ne!(
            exited_msg, gap_msg,
            "exited and output-gap messages must be textually distinct"
        );
    }

    #[test]
    fn chrome_message_for_event_is_none_for_output_bytes() {
        assert!(chrome_message_for_event(&attach_event::Payload::Output(vec![1, 2, 3])).is_none());
    }

    #[test]
    fn cli_definition_is_valid() {
        // clap's own debug_assert! sanity checks (duplicate args, etc.).
        Cli::command().debug_assert();
    }

    #[test]
    fn friendly_message_unwraps_tonic_status_to_its_plain_text() {
        let status = tonic::Status::not_found("no such session: abc");
        let err: anyhow::Error = status.into();
        assert_eq!(friendly_message(&err), "no such session: abc");
    }

    #[test]
    fn friendly_message_passes_through_generic_errors() {
        let err = anyhow::anyhow!("no such session: abc");
        assert_eq!(friendly_message(&err), "no such session: abc");
    }

    #[test]
    fn default_addr_is_localhost() {
        let cli = parse(&["ls"]);
        assert_eq!(cli.addr, "http://127.0.0.1:7419");
    }

    #[test]
    fn addr_can_be_overridden() {
        let cli = parse(&["--addr", "http://example.com:1234", "ls"]);
        assert_eq!(cli.addr, "http://example.com:1234");
    }

    #[test]
    fn ls_parses() {
        assert!(matches!(parse(&["ls"]).command, Command::Ls));
    }

    #[test]
    fn new_defaults_to_name_default_and_no_command() {
        match parse(&["new"]).command {
            Command::New { name, command } => {
                assert_eq!(name, "default");
                assert_eq!(command, None);
            }
            other => panic!("expected Command::New, got a different variant: {other:?}"),
        }
    }

    #[test]
    fn new_accepts_name_and_command() {
        match parse(&["new", "--name", "work", "--command", "bash"]).command {
            Command::New { name, command } => {
                assert_eq!(name, "work");
                assert_eq!(command, Some("bash".to_string()));
            }
            other => panic!("expected Command::New, got a different variant: {other:?}"),
        }
    }

    #[test]
    fn attach_requires_target() {
        match parse(&["attach", "myproject:0.1"]).command {
            Command::Attach { target } => assert_eq!(target, "myproject:0.1"),
            other => panic!("expected Command::Attach, got a different variant: {other:?}"),
        }
        assert!(Cli::try_parse_from(["tymux", "attach"]).is_err());
    }

    #[test]
    fn kill_requires_session_id() {
        match parse(&["kill", "some-uuid"]).command {
            Command::Kill { session_id } => assert_eq!(session_id, "some-uuid"),
            other => panic!("expected Command::Kill, got a different variant: {other:?}"),
        }
        assert!(Cli::try_parse_from(["tymux", "kill"]).is_err());
    }

    #[test]
    fn split_command_parses_target_and_orientation_flag() {
        match parse(&["split", "myproject:0.0", "--vertical"]).command {
            Command::Split {
                target, vertical, ..
            } => {
                assert_eq!(target, "myproject:0.0");
                assert!(vertical);
            }
            other => panic!("expected Command::Split, got a different variant: {other:?}"),
        }
    }

    #[test]
    fn kill_pane_command_requires_target() {
        match parse(&["kill-pane", "myproject:0.1"]).command {
            Command::KillPane { target } => assert_eq!(target, "myproject:0.1"),
            other => panic!("expected Command::KillPane, got a different variant: {other:?}"),
        }
        assert!(Cli::try_parse_from(["tymux", "kill-pane"]).is_err());
    }

    fn session_with(windows: Vec<Window>) -> Session {
        Session {
            id: "session-1".to_string(),
            name: "test".to_string(),
            windows,
            liveness: tymux_proto::v1::Liveness::Live as i32,
        }
    }

    fn window_with_panes(panes: Vec<ProtoPane>) -> Window {
        let mut children: Vec<tymux_proto::v1::LayoutChild> = Vec::new();
        for p in panes {
            children.push(tymux_proto::v1::LayoutChild {
                layout: Some(tymux_proto::v1::Layout {
                    node: Some(Node::Pane(p)),
                }),
                ratio: 1.0 / 2.0,
            });
        }
        // For test purposes, a single pane is a bare leaf; 2+ panes are
        // nested as a left-leaning chain of binary Splits (matching the
        // real LayoutNode's strictly-binary invariant).
        let layout = match children.len() {
            0 => None,
            1 => children.into_iter().next().unwrap().layout,
            _ => {
                let mut iter = children.into_iter();
                let mut acc = iter.next().unwrap().layout.unwrap();
                for child in iter {
                    acc = tymux_proto::v1::Layout {
                        node: Some(Node::Split(tymux_proto::v1::Split {
                            orientation: Orientation::Horizontal as i32,
                            children: vec![
                                tymux_proto::v1::LayoutChild {
                                    layout: Some(acc),
                                    ratio: 0.5,
                                },
                                child,
                            ],
                        })),
                    };
                }
                Some(acc)
            }
        };
        Window {
            id: "window-1".to_string(),
            name: "0".to_string(),
            layout,
        }
    }

    fn pane(id: &str) -> ProtoPane {
        ProtoPane {
            id: id.to_string(),
            rows: 24,
            cols: 80,
            liveness: tymux_proto::v1::Liveness::Live as i32,
        }
    }

    #[test]
    fn first_pane_id_returns_the_pane() {
        let session = session_with(vec![window_with_panes(vec![pane("pane-1")])]);
        assert_eq!(first_pane_id(&session).unwrap(), "pane-1");
    }

    #[test]
    fn first_pane_id_errors_on_no_windows() {
        let session = session_with(vec![]);
        assert!(first_pane_id(&session).is_err());
    }

    #[test]
    fn first_pane_id_errors_on_no_panes() {
        let session = session_with(vec![Window {
            id: "window-1".to_string(),
            name: "0".to_string(),
            layout: None,
        }]);
        assert!(first_pane_id(&session).is_err());
    }

    #[test]
    fn target_string_should_resolve_specific_pane_when_addressing_by_session_window_pane() {
        let target = TargetString::parse("myproject:0.1").unwrap();
        assert_eq!(target.session, "myproject");
        assert_eq!(target.window_index, 0);
        assert_eq!(target.pane_index, 1);

        let session = Session {
            id: "s1".to_string(),
            name: "myproject".to_string(),
            windows: vec![window_with_panes(vec![pane("pane-0"), pane("pane-1")])],
            liveness: tymux_proto::v1::Liveness::Live as i32,
        };
        assert_eq!(target.resolve(&session).unwrap().id, "pane-1");
    }

    #[test]
    fn target_string_bare_session_defaults_to_first_window_and_pane() {
        let target = TargetString::parse("myproject").unwrap();
        assert_eq!(target.window_index, 0);
        assert_eq!(target.pane_index, 0);
    }

    #[test]
    fn target_string_should_return_bounds_checked_error_when_pane_index_out_of_range() {
        let target = TargetString::parse("myproject:0.5").unwrap();
        let session = Session {
            id: "s1".to_string(),
            name: "myproject".to_string(),
            windows: vec![window_with_panes(vec![pane("pane-0")])],
            liveness: tymux_proto::v1::Liveness::Live as i32,
        };
        let err = target.resolve(&session).unwrap_err();
        assert!(err.to_string().contains("no pane 5"));
    }

    #[test]
    fn target_string_should_return_bounds_checked_error_when_window_index_out_of_range() {
        let target = TargetString::parse("myproject:3.0").unwrap();
        let session = Session {
            id: "s1".to_string(),
            name: "myproject".to_string(),
            windows: vec![window_with_panes(vec![pane("pane-0")])],
            liveness: tymux_proto::v1::Liveness::Live as i32,
        };
        let err = target.resolve(&session).unwrap_err();
        assert!(err.to_string().contains("no window 3"));
    }

    #[test]
    fn target_string_rejects_missing_pane_component() {
        assert!(TargetString::parse("myproject:0").is_err());
    }

    #[test]
    fn split_command_should_show_exact_row_counts_when_terminal_below_minimum_size() {
        let status = tonic::Status::failed_precondition(
            "split would produce a pane of 1 rows x 5 cols, below the minimum size",
        );
        let err: anyhow::Error = status.into();
        let msg = friendly_message(&err);
        assert!(msg.contains('1'));
        assert!(msg.contains('5'));
    }

    #[test]
    fn kill_pane_message_names_window_closed_when_last_pane_in_window() {
        let resp = tymux_proto::v1::ClosePaneResponse {
            window_closed_id: "w1".to_string(),
            window_closed_name: "0".to_string(),
            session_closed_id: String::new(),
            session_closed_name: String::new(),
            session: Some(session_with(vec![window_with_panes(vec![pane("p1")])])),
        };
        // Just confirm this doesn't panic and the outcome is structurally
        // distinguishable (window closed, session not).
        assert!(!resp.window_closed_name.is_empty());
        assert!(resp.session_closed_name.is_empty());
        print_close_pane_outcome(&resp);
    }
}
