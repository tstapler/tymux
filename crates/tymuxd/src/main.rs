use std::pin::Pin;
use std::sync::Arc;

use futures::{Stream, StreamExt};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{transport::Server, Request, Response, Status, Streaming};
use uuid::Uuid;

use tymux_core::{Engine, SessionInfo};
use tymux_proto::v1::tymux_service_server::{TymuxService, TymuxServiceServer};
use tymux_proto::v1::{
    attach_event, attach_request, AttachEvent, AttachRequest, CapturePaneRequest,
    Cell as ProtoCell, CreateSessionRequest, KillSessionRequest, KillSessionResponse,
    ListSessionsRequest, ListSessionsResponse, Pane as ProtoPane, PaneSnapshot as ProtoSnapshot,
    Row as ProtoRow, Session as ProtoSession, Window as ProtoWindow,
};

pub struct TymuxDaemon {
    engine: Arc<Engine>,
}

/// Every session has exactly one window today (see
/// docs/adr/0001-single-pane-per-session-for-now.md), so it's always
/// window index 0 — tmux itself names windows by their index by default,
/// so "0" here is that same convention, not an arbitrary placeholder.
const SOLE_WINDOW_NAME: &str = "0";

fn session_to_proto(info: SessionInfo) -> ProtoSession {
    ProtoSession {
        id: info.id.to_string(),
        name: info.name,
        windows: vec![ProtoWindow {
            id: info.window_id.to_string(),
            name: SOLE_WINDOW_NAME.to_string(),
            panes: vec![ProtoPane {
                id: info.pane_id.to_string(),
                rows: info.rows,
                cols: info.cols,
            }],
        }],
    }
}

fn snapshot_to_proto(pane_id: &str, snap: tymux_core::PaneSnapshot) -> ProtoSnapshot {
    ProtoSnapshot {
        pane_id: pane_id.to_string(),
        rows: snap.rows,
        cols: snap.cols,
        cursor_row: snap.cursor_row,
        cursor_col: snap.cursor_col,
        grid: snap
            .grid
            .into_iter()
            .map(|row| ProtoRow {
                cells: row
                    .into_iter()
                    .map(|c| ProtoCell {
                        text: c.text,
                        fg: c.fg,
                        bg: c.bg,
                        attrs: c.attrs,
                    })
                    .collect(),
            })
            .collect(),
    }
}

// tonic::Status is a fixed ~176 bytes we don't control; boxing it here
// would just push the cost onto every call site.
#[allow(clippy::result_large_err)]
fn parse_uuid(s: &str) -> Result<Uuid, Status> {
    Uuid::parse_str(s).map_err(|_| Status::invalid_argument("invalid id"))
}

/// Awaits a spawned task's handle purely to log if it panicked — a bare
/// `tokio::spawn` with nothing ever awaiting the handle means a panic
/// inside it disappears with no trace anywhere.
async fn supervise(pane_id: Uuid, task: &'static str, handle: tokio::task::JoinHandle<()>) {
    if let Err(e) = handle.await {
        tracing::error!(pane_id = %pane_id, task, error = %e, "attach task panicked");
    }
}

#[tonic::async_trait]
impl TymuxService for TymuxDaemon {
    async fn create_session(
        &self,
        request: Request<CreateSessionRequest>,
    ) -> Result<Response<ProtoSession>, Status> {
        let req = request.into_inner();
        let command = if req.command.is_empty() {
            None
        } else {
            Some(req.command)
        };
        let id = self
            .engine
            .create_session(req.name, command)
            .map_err(|e| Status::internal(e.to_string()))?;
        let info = self
            .engine
            .list_sessions()
            .into_iter()
            .find(|s| s.id == id)
            .ok_or_else(|| Status::internal("session vanished after create"))?;
        tracing::info!(session_id = %info.id, name = %info.name, pane_id = %info.pane_id, "session created");
        Ok(Response::new(session_to_proto(info)))
    }

    async fn list_sessions(
        &self,
        _request: Request<ListSessionsRequest>,
    ) -> Result<Response<ListSessionsResponse>, Status> {
        let sessions = self
            .engine
            .list_sessions()
            .into_iter()
            .map(session_to_proto)
            .collect();
        Ok(Response::new(ListSessionsResponse { sessions }))
    }

    async fn kill_session(
        &self,
        request: Request<KillSessionRequest>,
    ) -> Result<Response<KillSessionResponse>, Status> {
        let id = parse_uuid(&request.into_inner().session_id)?;
        self.engine.kill_session(id).map_err(|e| {
            tracing::warn!(session_id = %id, error = %e, "kill_session: no such session");
            Status::not_found(e.to_string())
        })?;
        tracing::info!(session_id = %id, "session killed");
        Ok(Response::new(KillSessionResponse {}))
    }

    async fn capture_pane(
        &self,
        request: Request<CapturePaneRequest>,
    ) -> Result<Response<ProtoSnapshot>, Status> {
        let pane_id_str = request.into_inner().pane_id;
        let pane_id = parse_uuid(&pane_id_str)?;
        let pane = self.engine.pane(pane_id).ok_or_else(|| {
            tracing::warn!(pane_id = %pane_id, "capture_pane: no such pane");
            Status::not_found("no such pane")
        })?;
        Ok(Response::new(snapshot_to_proto(
            &pane_id_str,
            pane.snapshot(),
        )))
    }

    type AttachStream = Pin<Box<dyn Stream<Item = Result<AttachEvent, Status>> + Send>>;

    async fn attach(
        &self,
        request: Request<Streaming<AttachRequest>>,
    ) -> Result<Response<Self::AttachStream>, Status> {
        let mut inbound = request.into_inner();

        let first = inbound
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("attach stream closed before pane id"))?;
        let pane_id_str = match first.payload {
            Some(attach_request::Payload::PaneId(id)) => id,
            _ => {
                return Err(Status::invalid_argument(
                    "first Attach message must set pane_id",
                ))
            }
        };
        let pane_id = parse_uuid(&pane_id_str)?;
        let pane = self.engine.pane(pane_id).ok_or_else(|| {
            tracing::warn!(pane_id = %pane_id, "attach: no such pane");
            Status::not_found("no such pane")
        })?;
        tracing::info!(pane_id = %pane_id, "attach started");

        let mut output_rx = pane.subscribe();
        let (tx, rx) = tokio::sync::mpsc::channel(64);

        let forward_tx = tx.clone();
        let pane_for_exit = pane.clone();
        let forward_handle = tokio::spawn(async move {
            loop {
                // `biased` checks output_rx first every iteration, so any
                // output already sent before the child exited (the reader
                // thread sends, then marks exited — see pane.rs) is always
                // drained before we report the exit, rather than racing.
                tokio::select! {
                    biased;
                    result = output_rx.recv() => {
                        match result {
                            Ok(bytes) => {
                                let event = AttachEvent {
                                    payload: Some(attach_event::Payload::Output(bytes)),
                                };
                                if forward_tx.send(Ok(event)).await.is_err() {
                                    return;
                                }
                            }
                            Err(_) => return,
                        }
                    }
                    _ = pane_for_exit.wait_exit() => {
                        tracing::info!(pane_id = %pane_for_exit.id, "pane exited, closing attach stream");
                        let event = AttachEvent {
                            payload: Some(attach_event::Payload::Exited(true)),
                        };
                        let _ = forward_tx.send(Ok(event)).await;
                        return;
                    }
                }
            }
        });
        // Spawned tasks that panic vanish silently by default — surface it.
        tokio::spawn(supervise(pane_id, "forward", forward_handle));

        let pane_for_input = pane.clone();
        let input_handle = tokio::spawn(async move {
            while let Some(Ok(msg)) = inbound.next().await {
                match msg.payload {
                    Some(attach_request::Payload::Input(bytes)) => {
                        if let Err(e) = pane_for_input.write_input(&bytes) {
                            tracing::warn!(pane_id = %pane_for_input.id, error = %e, "write_input failed");
                        }
                    }
                    Some(attach_request::Payload::Resize(r)) => {
                        if let Err(e) = pane_for_input.resize(r.rows as u16, r.cols as u16) {
                            tracing::warn!(pane_id = %pane_for_input.id, error = %e, "resize failed");
                        }
                    }
                    _ => {}
                }
            }
        });
        tokio::spawn(supervise(pane_id, "input", input_handle));

        Ok(Response::new(
            Box::pin(ReceiverStream::new(rx)) as Self::AttachStream
        ))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let addr = std::env::var("TYMUXD_ADDR").unwrap_or_else(|_| "127.0.0.1:7419".to_string());
    let socket_addr: std::net::SocketAddr = addr.parse()?;

    // There is no authentication anywhere in this daemon today: any client
    // that can reach this port can CreateSession (spawning an arbitrary
    // command) and Attach/CapturePane/KillSession against any pane_id with
    // no ownership check. That's an acceptable default on loopback, where
    // only local processes can reach it — it is unauthenticated remote
    // code execution if bound to a non-loopback address. This can't be
    // forbidden outright (a real multi-host deployment may need it and
    // that's a legitimate choice), but it must not be silent.
    if !socket_addr.ip().is_loopback() {
        tracing::warn!(
            %socket_addr,
            "tymuxd is binding to a non-loopback address with NO authentication of any kind. \
             Any client that can reach this port has full control: it can run arbitrary \
             commands via CreateSession and attach to any existing pane. Do not do this on an \
             untrusted network. Per-pane authorization is not implemented yet — see \
             docs/reviews/is-it-ready-2026-07-13.md."
        );
    }

    let engine = Arc::new(Engine::new());
    let daemon = TymuxDaemon { engine };

    tracing::info!(%addr, "tymuxd listening");
    Server::builder()
        .add_service(TymuxServiceServer::new(daemon))
        .serve_with_shutdown(socket_addr, shutdown_signal())
        .await?;
    tracing::info!("tymuxd shut down");
    Ok(())
}

/// Resolves on Ctrl-C or SIGTERM, whichever comes first — so tonic stops
/// accepting new connections and exits cleanly instead of dying mid-request
/// with no log at all. There's nothing to drain beyond that (no
/// persistence exists to flush — see the ADR/README), but a clean, logged
/// stop instead of a silent kill is still worth having.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received Ctrl-C, shutting down"),
        _ = terminate => tracing::info!("received SIGTERM, shutting down"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tymux_proto::v1::tymux_service_client::TymuxServiceClient;

    fn test_daemon() -> TymuxDaemon {
        TymuxDaemon {
            engine: Arc::new(Engine::new()),
        }
    }

    // /bin/sh explicitly so these don't depend on $SHELL/bash being present.
    fn create_req(name: &str) -> CreateSessionRequest {
        CreateSessionRequest {
            name: name.to_string(),
            command: "/bin/sh".to_string(),
        }
    }

    #[tokio::test]
    async fn create_session_appears_in_list() {
        let daemon = test_daemon();
        let resp = daemon
            .create_session(Request::new(create_req("test")))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(resp.name, "test");
        let pane_id = resp.windows[0].panes[0].id.clone();
        // Reflects the pane's real size (not a stale hardcoded literal).
        assert_eq!(resp.windows[0].panes[0].rows, 24);
        assert_eq!(resp.windows[0].panes[0].cols, 80);

        let list = daemon
            .list_sessions(Request::new(ListSessionsRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(list.sessions.len(), 1);
        assert_eq!(list.sessions[0].windows[0].panes[0].id, pane_id);
        assert_eq!(list.sessions[0].windows[0].panes[0].rows, 24);
        assert_eq!(list.sessions[0].windows[0].panes[0].cols, 80);
    }

    #[tokio::test]
    async fn kill_session_unknown_id_is_not_found() {
        let daemon = test_daemon();
        let err = daemon
            .kill_session(Request::new(KillSessionRequest {
                session_id: Uuid::new_v4().to_string(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn kill_session_invalid_uuid_is_invalid_argument() {
        let daemon = test_daemon();
        let err = daemon
            .kill_session(Request::new(KillSessionRequest {
                session_id: "not-a-uuid".to_string(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn capture_pane_unknown_id_is_not_found() {
        let daemon = test_daemon();
        let err = daemon
            .capture_pane(Request::new(CapturePaneRequest {
                pane_id: Uuid::new_v4().to_string(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn capture_pane_returns_structured_snapshot() {
        let daemon = test_daemon();
        let session = daemon
            .create_session(Request::new(create_req("test")))
            .await
            .unwrap()
            .into_inner();
        let pane_id = session.windows[0].panes[0].id.clone();

        let snapshot = daemon
            .capture_pane(Request::new(CapturePaneRequest { pane_id }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(snapshot.rows, 24);
        assert_eq!(snapshot.cols, 80);
        assert_eq!(snapshot.grid.len(), 24);
    }

    /// End-to-end regression test for the Ctrl-d hang bug fixed earlier:
    /// spins up a real server, attaches, tells the shell to exit, and
    /// asserts the stream reports Exited and closes — instead of hanging.
    #[tokio::test]
    async fn attach_streams_output_and_signals_exit() {
        let daemon = test_daemon();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            Server::builder()
                .add_service(TymuxServiceServer::new(daemon))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
                .unwrap();
        });

        let mut client = TymuxServiceClient::connect(format!("http://{addr}"))
            .await
            .expect("client should connect to the just-bound listener");

        let session = client
            .create_session(create_req("test"))
            .await
            .unwrap()
            .into_inner();
        let pane_id = session.windows[0].panes[0].id.clone();

        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tx.send(AttachRequest {
            payload: Some(attach_request::Payload::PaneId(pane_id)),
        })
        .await
        .unwrap();
        tx.send(AttachRequest {
            payload: Some(attach_request::Payload::Input(b"exit\n".to_vec())),
        })
        .await
        .unwrap();

        let mut inbound = client
            .attach(Request::new(ReceiverStream::new(rx)))
            .await
            .unwrap()
            .into_inner();

        let saw_exit = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = inbound.message().await.unwrap() {
                if matches!(event.payload, Some(attach_event::Payload::Exited(_))) {
                    return true;
                }
            }
            false
        })
        .await
        .expect("attach stream must close within 5s, not hang");

        assert!(
            saw_exit,
            "expected an Exited event before the stream closed"
        );
    }
}
