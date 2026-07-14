use std::io::{Read, Write};

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tonic::Request;

use tymux_proto::v1::tymux_service_client::TymuxServiceClient;
use tymux_proto::v1::{
    attach_event, attach_request, AttachRequest, CreateSessionRequest, ListSessionsRequest,
};

#[derive(Parser)]
#[command(name = "tymux")]
struct Cli {
    #[arg(long, global = true, default_value = "http://127.0.0.1:7419")]
    addr: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
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
async fn main() -> Result<()> {
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
            let pane_id = session.windows[0].panes[0].id.clone();
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
            let pane_id = session.windows[0].panes[0].id.clone();
            attach(&mut client, pane_id).await?;
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
                println!("\r\n[tymux: pane exited]");
                break;
            }
            _ => {}
        }
    }

    Ok(())
}
