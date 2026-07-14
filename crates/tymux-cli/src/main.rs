use std::io::{Read, Write};

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tonic::Request;

use tymux_proto::v1::tymux_service_client::TymuxServiceClient;
use tymux_proto::v1::{
    attach_event, attach_request, AttachRequest, CreateSessionRequest, KillSessionRequest,
    ListSessionsRequest, Resize, Session,
};

/// Every session today has exactly one window with one pane (see
/// docs/adr/0001-single-pane-per-session-for-now.md), but the proto
/// itself allows `repeated` windows/panes — so this is a real bounds
/// check, not a formality, and fails with a clear message instead of
/// panicking the moment that assumption is ever violated.
fn first_pane_id(session: &Session) -> Result<String> {
    let window = session
        .windows
        .first()
        .ok_or_else(|| anyhow::anyhow!("session {} has no windows", session.id))?;
    let pane = window
        .panes
        .first()
        .ok_or_else(|| anyhow::anyhow!("window {} has no panes", window.id))?;
    Ok(pane.id.clone())
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
    /// Attach to an existing session by id.
    Attach { session_id: String },
    /// End a session and its pane's process entirely.
    Kill { session_id: String },
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
                println!("{}\t{}", s.id, s.name);
            }
        }
        Command::Attach { session_id } => {
            let resp = client
                .list_sessions(ListSessionsRequest {})
                .await?
                .into_inner();
            let session = resp
                .sessions
                .into_iter()
                .find(|s| s.id == session_id)
                .ok_or_else(|| anyhow::anyhow!("no such session: {session_id}"))?;
            let pane_id = first_pane_id(&session)?;
            attach(&mut client, pane_id).await?;
        }
        Command::Kill { session_id } => {
            client
                .kill_session(KillSessionRequest { session_id })
                .await?;
        }
    }

    Ok(())
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
    fn attach_requires_session_id() {
        match parse(&["attach", "some-uuid"]).command {
            Command::Attach { session_id } => assert_eq!(session_id, "some-uuid"),
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

    fn session_with(windows: Vec<tymux_proto::v1::Window>) -> Session {
        Session {
            id: "session-1".to_string(),
            name: "test".to_string(),
            windows,
            liveness: tymux_proto::v1::Liveness::Live as i32,
        }
    }

    fn window_with(panes: Vec<tymux_proto::v1::Pane>) -> tymux_proto::v1::Window {
        tymux_proto::v1::Window {
            id: "window-1".to_string(),
            name: "0".to_string(),
            panes,
        }
    }

    fn pane(id: &str) -> tymux_proto::v1::Pane {
        tymux_proto::v1::Pane {
            id: id.to_string(),
            rows: 24,
            cols: 80,
            liveness: tymux_proto::v1::Liveness::Live as i32,
        }
    }

    #[test]
    fn first_pane_id_returns_the_pane() {
        let session = session_with(vec![window_with(vec![pane("pane-1")])]);
        assert_eq!(first_pane_id(&session).unwrap(), "pane-1");
    }

    #[test]
    fn first_pane_id_errors_on_no_windows() {
        let session = session_with(vec![]);
        assert!(first_pane_id(&session).is_err());
    }

    #[test]
    fn first_pane_id_errors_on_no_panes() {
        let session = session_with(vec![window_with(vec![])]);
        assert!(first_pane_id(&session).is_err());
    }
}
