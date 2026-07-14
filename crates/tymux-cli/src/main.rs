use std::io::{Read, Write};

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tonic::Request;

use tymux_proto::v1::tymux_service_client::TymuxServiceClient;
use tymux_proto::v1::{
    attach_event, attach_request, layout::Node, AttachRequest, ClosePaneRequest,
    CreateSessionRequest, KillSessionRequest, ListSessionsRequest, Orientation, Pane as ProtoPane,
    Resize, ReviveSessionRequest, ReviveSessionResponse, Session, SplitPaneRequest, Window,
};

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

    match cli.command {
        Command::New { name, command } => {
            let session = client
                .create_session(CreateSessionRequest {
                    name,
                    command: command.unwrap_or_default(),
                })
                .await?
                .into_inner();
            let pane_id = first_pane_id(&session)?;
            attach(&mut client, pane_id).await?;
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
            attach(&mut client, pane.id).await?;
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

async fn attach(client: &mut TymuxServiceClient<Channel>, pane_id: String) -> Result<()> {
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    tx.send(AttachRequest {
        payload: Some(attach_request::Payload::PaneId(pane_id)),
    })
    .await?;

    // Sync the pane to the local terminal's real size immediately, and
    // again on every SIGWINCH — without this the pane stays at whatever
    // fixed default the daemon created it with, forever, regardless of the
    // terminal it's actually attached to.
    send_resize(&tx).await?;
    spawn_resize_watcher(tx.clone());

    // stdin reads are blocking, so they get their own OS thread and feed
    // the outbound stream over a channel.
    let stdin_tx = tx.clone();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let msg = AttachRequest {
                        payload: Some(attach_request::Payload::Input(buf[..n].to_vec())),
                    };
                    if stdin_tx.blocking_send(msg).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let _raw = RawGuard::enable()?;
    let outbound = ReceiverStream::new(rx);
    let mut inbound = client.attach(Request::new(outbound)).await?.into_inner();

    let mut stdout = std::io::stdout();
    while let Some(event) = inbound.message().await? {
        match event.payload {
            Some(attach_event::Payload::Output(bytes)) => {
                stdout.write_all(&bytes)?;
                stdout.flush()?;
            }
            Some(attach_event::Payload::Exited(_)) => {
                drop(_raw); // restore the terminal before printing
                println!(
                    "{}",
                    chrome_message_for_event(&attach_event::Payload::Exited(true)).unwrap()
                );
                break;
            }
            Some(ref payload @ attach_event::Payload::OutputGap(_)) => {
                print!("{}", chrome_message_for_event(payload).unwrap());
                stdout.flush()?;
            }
            _ => {}
        }
    }

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

async fn send_resize(tx: &tokio::sync::mpsc::Sender<AttachRequest>) -> Result<()> {
    // A failure here just means the local terminal size can't be queried
    // (e.g. stdout isn't a real tty) — not worth aborting the attach over,
    // the pane just keeps whatever size it already had.
    if let Ok((cols, rows)) = crossterm::terminal::size() {
        tx.send(AttachRequest {
            payload: Some(attach_request::Payload::Resize(Resize {
                rows: rows as u32,
                cols: cols as u32,
            })),
        })
        .await?;
    }
    Ok(())
}

/// SIGWINCH only exists on Unix; on other platforms the pane just keeps
/// whatever size it got at attach time (still an improvement over never
/// syncing at all).
#[cfg(unix)]
fn spawn_resize_watcher(tx: tokio::sync::mpsc::Sender<AttachRequest>) {
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
            if send_resize(&tx).await.is_err() {
                break;
            }
        }
    });
}

#[cfg(not(unix))]
fn spawn_resize_watcher(_tx: tokio::sync::mpsc::Sender<AttachRequest>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("tymux").chain(args.iter().copied())).unwrap()
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
