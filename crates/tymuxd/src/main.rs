use std::pin::Pin;
use std::sync::Arc;

use futures::{Stream, StreamExt};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{transport::Server, Request, Response, Status, Streaming};
use uuid::Uuid;

use tymux_core::{
    Engine, LayoutSnapshot as CoreLayout, Orientation as CoreOrientation, PaneLookup,
    PersistenceBackend, SessionSnapshot, WindowSnapshot,
};
use tymux_proto::v1::tymux_service_server::{TymuxService, TymuxServiceServer};
use tymux_proto::v1::{
    attach_event, attach_request, AttachEvent, AttachRequest, CapturePaneRequest,
    Cell as ProtoCell, ClosePaneRequest, ClosePaneResponse, CreateSessionRequest,
    CreateWindowRequest, KillSessionRequest, KillSessionResponse, Layout as ProtoLayout,
    LayoutChild as ProtoLayoutChild, ListSessionsRequest, ListSessionsResponse, Liveness,
    Orientation as ProtoOrientation, Pane as ProtoPane, PaneSnapshot as ProtoSnapshot,
    ReviveSessionRequest, ReviveSessionResponse, Row as ProtoRow, SearchScrollbackRequest,
    SearchScrollbackResponse, Session as ProtoSession, Split as ProtoSplit, SplitPaneRequest,
    WatchWindowRequest, Window as ProtoWindow, WindowLayoutEvent,
};

pub struct TymuxDaemon {
    engine: Arc<Engine>,
}

fn liveness_of(live: bool) -> Liveness {
    if live {
        Liveness::Live
    } else {
        Liveness::Dead
    }
}

fn orientation_to_proto(o: CoreOrientation) -> ProtoOrientation {
    match o {
        CoreOrientation::Horizontal => ProtoOrientation::Horizontal,
        CoreOrientation::Vertical => ProtoOrientation::Vertical,
    }
}

// tonic::Status is a fixed ~176 bytes we don't control; boxing it here
// would just push the cost onto every call site.
#[allow(clippy::result_large_err)]
fn orientation_from_proto(o: i32) -> Result<CoreOrientation, Status> {
    match ProtoOrientation::try_from(o) {
        Ok(ProtoOrientation::Horizontal) => Ok(CoreOrientation::Horizontal),
        Ok(ProtoOrientation::Vertical) => Ok(CoreOrientation::Vertical),
        _ => Err(Status::invalid_argument("orientation must be specified")),
    }
}

fn layout_snapshot_to_proto(layout: &CoreLayout) -> ProtoLayout {
    use tymux_proto::v1::layout::Node;
    let node = match layout {
        CoreLayout::Leaf(info) => Node::Pane(ProtoPane {
            id: info.id.to_string(),
            rows: info.rows,
            cols: info.cols,
            liveness: liveness_of(info.live) as i32,
        }),
        CoreLayout::Split {
            orientation,
            children,
        } => Node::Split(ProtoSplit {
            orientation: orientation_to_proto(*orientation) as i32,
            children: children
                .iter()
                .map(|(child, ratio)| ProtoLayoutChild {
                    layout: Some(layout_snapshot_to_proto(child)),
                    ratio: *ratio,
                })
                .collect(),
        }),
    };
    ProtoLayout { node: Some(node) }
}

fn window_to_proto(window: &WindowSnapshot) -> ProtoWindow {
    ProtoWindow {
        id: window.id.to_string(),
        name: window.name.clone(),
        layout: Some(layout_snapshot_to_proto(&window.layout)),
    }
}

fn session_to_proto(session: &SessionSnapshot) -> ProtoSession {
    ProtoSession {
        id: session.id.to_string(),
        name: session.name.clone(),
        windows: session.windows.iter().map(window_to_proto).collect(),
        liveness: liveness_of(session.live) as i32,
    }
}

fn snapshot_to_proto(pane_id: &str, snap: tymux_core::PaneSnapshot, live: bool) -> ProtoSnapshot {
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
        liveness: liveness_of(live) as i32,
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

fn engine_error_to_status(e: tymux_core::EngineError) -> Status {
    match e {
        tymux_core::EngineError::PaneNotFound(id) => {
            Status::not_found(format!("no such pane: {id}"))
        }
        tymux_core::EngineError::SessionNotFound(id) => {
            Status::not_found(format!("no such session: {id}"))
        }
        tymux_core::EngineError::BelowMinimumSize { rows, cols } => {
            Status::failed_precondition(format!(
                "split would produce a pane of {rows} rows x {cols} cols, below the minimum size"
            ))
        }
    }
}

// tonic::Status is a fixed ~176 bytes we don't control; boxing it here
// would just push the cost onto every call site.
#[allow(clippy::result_large_err)]
fn resolve_live_pane(engine: &Engine, pane_id: Uuid) -> Result<Arc<tymux_core::Pane>, Status> {
    match engine.pane_lookup(pane_id) {
        PaneLookup::Live(pane) => Ok(pane),
        PaneLookup::Dead => Err(Status::failed_precondition(format!(
            "pane exited — run 'tymux revive <session_id>' to respawn it (pane_id={pane_id})"
        ))),
        PaneLookup::Unknown => Err(Status::not_found("no such pane")),
    }
}

/// Maps one `output_rx.recv()` result from the attach forwarding loop to
/// the `AttachEvent` (if any) it produces — pulled out of the loop so the
/// Lagged-becomes-`output_gap` transformation is unit-testable without a
/// live pty/broadcast channel. `None` means the stream should end (the
/// channel was permanently closed).
fn attach_event_for_output_result(
    result: Result<Vec<u8>, tokio::sync::broadcast::error::RecvError>,
    pane_id: Uuid,
) -> Option<AttachEvent> {
    use tokio::sync::broadcast::error::RecvError;
    match result {
        Ok(bytes) => Some(AttachEvent {
            payload: Some(attach_event::Payload::Output(bytes)),
        }),
        Err(RecvError::Lagged(n)) => {
            tracing::warn!(pane_id = %pane_id, skipped = n, "attach consumer lagged, output_gap signaled");
            Some(AttachEvent {
                payload: Some(attach_event::Payload::OutputGap(true)),
            })
        }
        Err(RecvError::Closed) => None,
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
        tracing::info!(session_id = %info.id, name = %info.name, "session created");
        Ok(Response::new(session_to_proto(&info)))
    }

    async fn list_sessions(
        &self,
        _request: Request<ListSessionsRequest>,
    ) -> Result<Response<ListSessionsResponse>, Status> {
        let sessions = self
            .engine
            .list_sessions()
            .iter()
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

    async fn revive_session(
        &self,
        request: Request<ReviveSessionRequest>,
    ) -> Result<Response<ReviveSessionResponse>, Status> {
        let session_id = parse_uuid(&request.into_inner().session_id)?;
        let outcome = self
            .engine
            .revive_session(session_id)
            .map_err(engine_error_to_status)?;
        let session = self
            .engine
            .list_sessions()
            .into_iter()
            .find(|s| s.id == session_id);
        let (already_live, pane_count) = match outcome {
            tymux_core::ReviveOutcome::AlreadyLive => (true, 0),
            tymux_core::ReviveOutcome::Revived { pane_count } => (false, pane_count as u32),
        };
        tracing::info!(session_id = %session_id, already_live, pane_count, "revive_session");
        Ok(Response::new(ReviveSessionResponse {
            already_live,
            pane_count,
            session: session.as_ref().map(session_to_proto),
        }))
    }

    async fn capture_pane(
        &self,
        request: Request<CapturePaneRequest>,
    ) -> Result<Response<ProtoSnapshot>, Status> {
        let req = request.into_inner();
        let pane_id = parse_uuid(&req.pane_id)?;
        let pane = resolve_live_pane(&self.engine, pane_id).inspect_err(|status| {
            tracing::warn!(pane_id = %pane_id, code = ?status.code(), "capture_pane: pane unavailable");
        })?;
        Ok(Response::new(snapshot_to_proto(
            &req.pane_id,
            pane.snapshot_at_offset(req.scrollback_offset as usize),
            true,
        )))
    }

    async fn search_scrollback(
        &self,
        request: Request<SearchScrollbackRequest>,
    ) -> Result<Response<SearchScrollbackResponse>, Status> {
        let req = request.into_inner();
        let pane_id = parse_uuid(&req.pane_id)?;
        let pane = resolve_live_pane(&self.engine, pane_id).inspect_err(|status| {
            tracing::warn!(pane_id = %pane_id, code = ?status.code(), "search_scrollback: pane unavailable");
        })?;
        match pane.search_scrollback(&req.pattern, req.start_offset as usize) {
            Some((offset, line)) => Ok(Response::new(SearchScrollbackResponse {
                found: true,
                offset: offset as u32,
                line,
            })),
            None => Ok(Response::new(SearchScrollbackResponse {
                found: false,
                offset: 0,
                line: String::new(),
            })),
        }
    }

    async fn split_pane(
        &self,
        request: Request<SplitPaneRequest>,
    ) -> Result<Response<ProtoSession>, Status> {
        let req = request.into_inner();
        let pane_id = parse_uuid(&req.pane_id)?;
        let orientation = orientation_from_proto(req.orientation)?;
        let command = if req.command.is_empty() {
            None
        } else {
            Some(req.command)
        };
        let session = self
            .engine
            .split_pane(pane_id, orientation, command)
            .map_err(engine_error_to_status)?;
        tracing::info!(pane_id = %pane_id, session_id = %session.id, "pane split");
        Ok(Response::new(session_to_proto(&session)))
    }

    async fn close_pane(
        &self,
        request: Request<ClosePaneRequest>,
    ) -> Result<Response<ClosePaneResponse>, Status> {
        let pane_id = parse_uuid(&request.into_inner().pane_id)?;
        let outcome = self
            .engine
            .close_pane(pane_id)
            .map_err(engine_error_to_status)?;
        tracing::info!(pane_id = %pane_id, window_closed = outcome.window_closed.is_some(), session_closed = outcome.session_closed.is_some(), "pane closed");
        Ok(Response::new(ClosePaneResponse {
            window_closed_id: outcome
                .window_closed
                .as_ref()
                .map(|(id, _)| id.to_string())
                .unwrap_or_default(),
            window_closed_name: outcome
                .window_closed
                .map(|(_, name)| name)
                .unwrap_or_default(),
            session_closed_id: outcome
                .session_closed
                .as_ref()
                .map(|(id, _)| id.to_string())
                .unwrap_or_default(),
            session_closed_name: outcome
                .session_closed
                .map(|(_, name)| name)
                .unwrap_or_default(),
            session: outcome.session.as_ref().map(session_to_proto),
        }))
    }

    async fn create_window(
        &self,
        request: Request<CreateWindowRequest>,
    ) -> Result<Response<ProtoSession>, Status> {
        let req = request.into_inner();
        let session_id = parse_uuid(&req.session_id)?;
        let command = if req.command.is_empty() {
            None
        } else {
            Some(req.command)
        };
        let session = self
            .engine
            .create_window(session_id, command)
            .map_err(engine_error_to_status)?;
        tracing::info!(session_id = %session_id, "window created");
        Ok(Response::new(session_to_proto(&session)))
    }

    type WatchWindowStream = Pin<Box<dyn Stream<Item = Result<WindowLayoutEvent, Status>> + Send>>;

    async fn watch_window(
        &self,
        request: Request<WatchWindowRequest>,
    ) -> Result<Response<Self::WatchWindowStream>, Status> {
        let window_id = parse_uuid(&request.into_inner().window_id)?;
        let mut changes = self.engine.watch_window(window_id);
        let engine = self.engine.clone();

        let (tx, rx) = tokio::sync::mpsc::channel(16);
        // Emit the current snapshot immediately, so a subscriber doesn't
        // have to wait for the *next* change to learn the current shape.
        if let Some(window) = engine.window_snapshot(window_id) {
            let _ = tx
                .send(Ok(WindowLayoutEvent {
                    layout: Some(layout_snapshot_to_proto(&window.layout)),
                }))
                .await;
        }

        tokio::spawn(async move {
            loop {
                match changes.recv().await {
                    Ok(()) => {
                        let Some(window) = engine.window_snapshot(window_id) else {
                            return; // window closed — end the stream
                        };
                        let event = WindowLayoutEvent {
                            layout: Some(layout_snapshot_to_proto(&window.layout)),
                        };
                        if tx.send(Ok(event)).await.is_err() {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });

        Ok(Response::new(
            Box::pin(ReceiverStream::new(rx)) as Self::WatchWindowStream
        ))
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
        let pane = resolve_live_pane(&self.engine, pane_id).inspect_err(|status| {
            tracing::warn!(pane_id = %pane_id, code = ?status.code(), "attach: pane unavailable");
        })?;
        tracing::info!(pane_id = %pane_id, "attach started");

        // Resize is window-scoped (ADR-004): track this client's reported
        // viewport against the pane's window and apply the dimension-wise
        // minimum across every attached client, rather than sizing this
        // one pane to this one client's report 1:1.
        let window_id = self.engine.window_id_for_pane(pane_id);
        let client_id = self.engine.new_client_id();

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
                        if let Some(event) = attach_event_for_output_result(result, pane_for_exit.id) {
                            if forward_tx.send(Ok(event)).await.is_err() {
                                return;
                            }
                        } else {
                            return;
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
        let engine_for_input = self.engine.clone();
        let input_handle = tokio::spawn(async move {
            while let Some(Ok(msg)) = inbound.next().await {
                match msg.payload {
                    Some(attach_request::Payload::Input(bytes)) => {
                        if let Err(e) = pane_for_input.write_input(&bytes) {
                            tracing::warn!(pane_id = %pane_for_input.id, error = %e, "write_input failed");
                        }
                    }
                    Some(attach_request::Payload::Resize(r)) => {
                        if let Some(window_id) = window_id {
                            engine_for_input.report_viewport_and_recompute(
                                window_id,
                                client_id,
                                r.rows as u16,
                                r.cols as u16,
                            );
                        } else {
                            tracing::warn!(pane_id = %pane_for_input.id, "resize: pane's window not found, ignoring");
                        }
                    }
                    _ => {}
                }
            }
            if let Some(window_id) = window_id {
                engine_for_input.unregister_viewport(window_id, client_id);
                engine_for_input.recompute_window_geometry(window_id);
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

    // Story 4.3: reconcile persisted session records before serving any
    // RPC — every session loads dead-flagged (ADR-002: never auto-revived
    // on daemon start); a file that fails to parse or fails structural
    // validation is logged and skipped, never fatal to daemon boot.
    let sessions_dir = tymux_core::default_sessions_dir();
    let backend = tymux_core::FsPersistenceBackend::new(sessions_dir.clone()).map_err(|e| {
        format!(
            "failed to prepare sessions directory {}: {e}",
            sessions_dir.display()
        )
    })?;
    let records = backend.load_all();
    let restored_count = records.len();
    let engine = Arc::new(Engine::with_persistence(Box::new(backend)));
    engine.load_persisted(records);
    if restored_count > 0 {
        tracing::info!(count = restored_count, dir = %sessions_dir.display(), "restored dead-flagged sessions from disk");
    }

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
/// with no log at all. Story 4.5: there is deliberately no separate
/// "flush persisted state" step here — every mutation (`create_session`,
/// `split_pane`, `close_pane`, `kill_session`, `create_window`, window
/// resize, `revive_session`) already writes its session's record
/// synchronously (atomic temp-file-then-rename) before the RPC handler
/// returns, so by the time any of those calls has completed, the on-disk
/// state is already current — there is nothing left to drain at shutdown
/// that isn't already durable.
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

    /// Extracts the pane from a freshly created single-pane window's
    /// `Layout` — the common case throughout these tests, which mostly
    /// predate splits.
    fn sole_pane(window: &ProtoWindow) -> &ProtoPane {
        use tymux_proto::v1::layout::Node;
        match window.layout.as_ref().unwrap().node.as_ref().unwrap() {
            Node::Pane(p) => p,
            Node::Split(_) => panic!("expected a single-leaf window"),
        }
    }

    // /bin/sh explicitly so these don't depend on $SHELL/bash being present.
    fn create_req(name: &str) -> CreateSessionRequest {
        CreateSessionRequest {
            name: name.to_string(),
            command: "/bin/sh".to_string(),
        }
    }

    /// Spins up a real server on an ephemeral port and returns a connected
    /// client — the shared setup every real-network (as opposed to
    /// direct-method-call) integration test in this module needs.
    async fn spawn_test_server(
        daemon: TymuxDaemon,
    ) -> TymuxServiceClient<tonic::transport::Channel> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            Server::builder()
                .add_service(TymuxServiceServer::new(daemon))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        TymuxServiceClient::connect(format!("http://{addr}"))
            .await
            .expect("client should connect to the just-bound listener")
    }

    async fn wait_for_pane_exit(pane: &Arc<tymux_core::Pane>) {
        tokio::time::timeout(Duration::from_secs(5), pane.wait_exit())
            .await
            .expect("pane should exit within 5s");
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
        let pane_id = sole_pane(&resp.windows[0]).id.clone();
        // Reflects the pane's real size (not a stale hardcoded literal).
        assert_eq!(sole_pane(&resp.windows[0]).rows, 24);
        assert_eq!(sole_pane(&resp.windows[0]).cols, 80);

        let list = daemon
            .list_sessions(Request::new(ListSessionsRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(list.sessions.len(), 1);
        assert_eq!(sole_pane(&list.sessions[0].windows[0]).id, pane_id);
        assert_eq!(sole_pane(&list.sessions[0].windows[0]).rows, 24);
        assert_eq!(sole_pane(&list.sessions[0].windows[0]).cols, 80);
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

    fn capture_req(pane_id: String) -> CapturePaneRequest {
        CapturePaneRequest {
            pane_id,
            scrollback_offset: 0,
        }
    }

    #[tokio::test]
    async fn capture_pane_unknown_id_is_not_found() {
        let daemon = test_daemon();
        let err = daemon
            .capture_pane(Request::new(capture_req(Uuid::new_v4().to_string())))
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
        let pane_id = sole_pane(&session.windows[0]).id.clone();

        let snapshot = daemon
            .capture_pane(Request::new(capture_req(pane_id)))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(snapshot.rows, 24);
        assert_eq!(snapshot.cols, 80);
        assert_eq!(snapshot.grid.len(), 24);
    }

    #[tokio::test]
    async fn create_session_should_report_liveness_live_when_pane_freshly_spawned() {
        let daemon = test_daemon();
        let session = daemon
            .create_session(Request::new(create_req("test")))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(session.liveness, Liveness::Live as i32);
        assert_eq!(
            sole_pane(&session.windows[0]).liveness,
            Liveness::Live as i32
        );
    }

    #[tokio::test]
    async fn list_sessions_should_report_liveness_dead_when_pane_child_process_exited() {
        let daemon = test_daemon();
        let session = daemon
            .create_session(Request::new(create_req("test")))
            .await
            .unwrap()
            .into_inner();
        let pane_id = parse_uuid(&sole_pane(&session.windows[0]).id).unwrap();

        let pane = match daemon.engine.pane_lookup(pane_id) {
            PaneLookup::Live(pane) => pane,
            _ => panic!("expected freshly created pane to be Live"),
        };
        pane.write_input(b"exit\n").unwrap();
        wait_for_pane_exit(&pane).await;

        let list = daemon
            .list_sessions(Request::new(ListSessionsRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(list.sessions[0].liveness, Liveness::Dead as i32);
        assert_eq!(
            sole_pane(&list.sessions[0].windows[0]).liveness,
            Liveness::Dead as i32
        );
    }

    /// Integration counterpart to the two liveness unit tests above: proves
    /// the LIVENESS_DEAD signal survives a real wire round trip, not just a
    /// direct in-process method call.
    #[tokio::test]
    async fn session_to_proto_should_map_exited_pane_to_liveness_dead_field() {
        let daemon = test_daemon();
        let engine = daemon.engine.clone();
        let mut client = spawn_test_server(daemon).await;

        let session = client
            .create_session(create_req("test"))
            .await
            .unwrap()
            .into_inner();
        let pane_id = parse_uuid(&sole_pane(&session.windows[0]).id).unwrap();
        let pane = match engine.pane_lookup(pane_id) {
            PaneLookup::Live(pane) => pane,
            _ => panic!("expected freshly created pane to be Live"),
        };
        pane.write_input(b"exit\n").unwrap();
        wait_for_pane_exit(&pane).await;

        let list = client
            .list_sessions(ListSessionsRequest {})
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            sole_pane(&list.sessions[0].windows[0]).liveness,
            Liveness::Dead as i32
        );
    }

    #[tokio::test]
    async fn capture_pane_should_return_failed_precondition_when_pane_lookup_is_dead_vs_not_found_when_unknown(
    ) {
        let daemon = test_daemon();
        let session = daemon
            .create_session(Request::new(create_req("test")))
            .await
            .unwrap()
            .into_inner();
        let pane_id_str = sole_pane(&session.windows[0]).id.clone();
        let pane_id = parse_uuid(&pane_id_str).unwrap();

        let pane = match daemon.engine.pane_lookup(pane_id) {
            PaneLookup::Live(pane) => pane,
            _ => panic!("expected freshly created pane to be Live"),
        };
        pane.write_input(b"exit\n").unwrap();
        wait_for_pane_exit(&pane).await;

        let dead_err = daemon
            .capture_pane(Request::new(capture_req(pane_id_str)))
            .await
            .unwrap_err();
        assert_eq!(dead_err.code(), tonic::Code::FailedPrecondition);

        let unknown_err = daemon
            .capture_pane(Request::new(capture_req(Uuid::new_v4().to_string())))
            .await
            .unwrap_err();
        assert_eq!(unknown_err.code(), tonic::Code::NotFound);
        assert_ne!(dead_err.code(), unknown_err.code());
    }

    #[test]
    fn attach_should_not_emit_output_gap_event_when_consumer_keeps_pace() {
        let pane_id = Uuid::new_v4();
        let event = attach_event_for_output_result(Ok(b"hello".to_vec()), pane_id).unwrap();
        assert!(matches!(
            event.payload,
            Some(attach_event::Payload::Output(_))
        ));
    }

    #[test]
    fn attach_should_emit_output_gap_event_when_consumer_lags_behind_broadcast_channel() {
        let pane_id = Uuid::new_v4();
        let event = attach_event_for_output_result(
            Err(tokio::sync::broadcast::error::RecvError::Lagged(5)),
            pane_id,
        )
        .unwrap();
        assert!(matches!(
            event.payload,
            Some(attach_event::Payload::OutputGap(true))
        ));
    }

    #[test]
    fn attach_event_for_output_result_ends_stream_on_closed_channel() {
        let pane_id = Uuid::new_v4();
        assert!(attach_event_for_output_result(
            Err(tokio::sync::broadcast::error::RecvError::Closed),
            pane_id
        )
        .is_none());
    }

    /// Integration-style proof (real `tokio::sync::broadcast` channel, tiny
    /// capacity, burst sender) that a lagged consumer observes an
    /// `OutputGap` event before normal `Output` events resume — exercising
    /// `attach_event_for_output_result` against tokio's actual `Lagged`
    /// semantics rather than a hand-constructed `RecvError`.
    #[tokio::test]
    async fn attach_stream_should_observe_output_gap_before_output_resumes_when_consumer_lags() {
        let (tx, mut rx) = tokio::sync::broadcast::channel::<Vec<u8>>(2);
        let pane_id = Uuid::new_v4();

        // Burst past the channel's capacity before the consumer ever reads,
        // guaranteeing the next recv() observes Lagged.
        for i in 0..5u8 {
            let _ = tx.send(vec![i]);
        }

        let first = attach_event_for_output_result(rx.recv().await, pane_id).unwrap();
        assert!(
            matches!(first.payload, Some(attach_event::Payload::OutputGap(true))),
            "first observed event after a burst past capacity must be OutputGap"
        );

        // Normal output resumes immediately after: the channel still holds
        // its last `capacity` (2) buffered items (3, 4) — the next recv()
        // must yield one of them as an ordinary Output event, not another
        // Lagged/OutputGap.
        let second = attach_event_for_output_result(rx.recv().await, pane_id).unwrap();
        assert!(matches!(
            second.payload,
            Some(attach_event::Payload::Output(_))
        ));
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
        let pane_id = sole_pane(&session.windows[0]).id.clone();

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

    /// Story 2.3 AC2/task 5: KillSession from a second simulated client must
    /// signal the first client's attach stream with a clean terminal event
    /// (reusing the existing pane-exit path) before the stream closes —
    /// never a bare stream error or silent hang. This is the direct
    /// counterpart to the already-fixed Ctrl-D hang regression test above.
    #[tokio::test]
    async fn kill_session_should_close_attached_stream_cleanly_when_second_client_kills_session() {
        let daemon = test_daemon();
        let mut client_a = spawn_test_server(daemon).await;
        let mut client_b = client_a.clone();

        let session = client_a
            .create_session(create_req("test"))
            .await
            .unwrap()
            .into_inner();
        let session_id = session.id.clone();
        let pane_id = sole_pane(&session.windows[0]).id.clone();

        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tx.send(AttachRequest {
            payload: Some(attach_request::Payload::PaneId(pane_id)),
        })
        .await
        .unwrap();

        let mut inbound = client_a
            .attach(Request::new(ReceiverStream::new(rx)))
            .await
            .unwrap()
            .into_inner();

        client_b
            .kill_session(KillSessionRequest { session_id })
            .await
            .expect(
                "kill_session should not produce a raw stream error while a client is attached",
            );

        let saw_clean_exit = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(msg) = inbound.message().await.transpose() {
                match msg {
                    Ok(event)
                        if matches!(event.payload, Some(attach_event::Payload::Exited(_))) =>
                    {
                        return true;
                    }
                    Ok(_) => continue,
                    Err(_) => return false, // raw stream error — the exact failure class this guards against
                }
            }
            false
        })
        .await
        .expect("attach stream must close within 5s, not hang");

        assert!(
            saw_clean_exit,
            "expected a clean Exited event before the stream closed, not a raw error or silent hang"
        );
    }

    #[tokio::test]
    async fn split_pane_rpc_should_produce_two_leaf_layout_visible_in_list_sessions() {
        let daemon = test_daemon();
        let session = daemon
            .create_session(Request::new(create_req("test")))
            .await
            .unwrap()
            .into_inner();
        let pane_id = sole_pane(&session.windows[0]).id.clone();

        daemon
            .split_pane(Request::new(SplitPaneRequest {
                pane_id,
                orientation: ProtoOrientation::Vertical as i32,
                command: "/bin/sh".to_string(),
            }))
            .await
            .unwrap();

        let list = daemon
            .list_sessions(Request::new(ListSessionsRequest {}))
            .await
            .unwrap()
            .into_inner();
        let layout = list.sessions[0].windows[0].layout.as_ref().unwrap();
        use tymux_proto::v1::layout::Node;
        match layout.node.as_ref().unwrap() {
            Node::Split(split) => assert_eq!(split.children.len(), 2),
            Node::Pane(_) => panic!("expected the window's layout to be a Split after SplitPane"),
        }
    }

    #[tokio::test]
    async fn split_pane_rpc_should_return_not_found_when_pane_id_unknown() {
        let daemon = test_daemon();
        let err = daemon
            .split_pane(Request::new(SplitPaneRequest {
                pane_id: Uuid::new_v4().to_string(),
                orientation: ProtoOrientation::Horizontal as i32,
                command: String::new(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn close_pane_should_collapse_and_report_no_window_closed_when_sibling_survives() {
        let daemon = test_daemon();
        let session = daemon
            .create_session(Request::new(create_req("test")))
            .await
            .unwrap()
            .into_inner();
        let pane_id = sole_pane(&session.windows[0]).id.clone();
        let split = daemon
            .split_pane(Request::new(SplitPaneRequest {
                pane_id,
                orientation: ProtoOrientation::Horizontal as i32,
                command: "/bin/sh".to_string(),
            }))
            .await
            .unwrap()
            .into_inner();

        use tymux_proto::v1::layout::Node;
        let second_pane_id = match split.windows[0]
            .layout
            .as_ref()
            .unwrap()
            .node
            .as_ref()
            .unwrap()
        {
            Node::Split(s) => match s.children[1]
                .layout
                .as_ref()
                .unwrap()
                .node
                .as_ref()
                .unwrap()
            {
                Node::Pane(p) => p.id.clone(),
                _ => panic!("expected a leaf"),
            },
            _ => panic!("expected a split"),
        };

        let resp = daemon
            .close_pane(Request::new(ClosePaneRequest {
                pane_id: second_pane_id,
            }))
            .await
            .unwrap()
            .into_inner();
        assert!(resp.window_closed_id.is_empty());
        assert!(resp.session_closed_id.is_empty());
        assert!(resp.session.is_some());
    }

    #[tokio::test]
    async fn create_window_rpc_should_add_a_second_window() {
        let daemon = test_daemon();
        let session = daemon
            .create_session(Request::new(create_req("test")))
            .await
            .unwrap()
            .into_inner();

        let updated = daemon
            .create_window(Request::new(CreateWindowRequest {
                session_id: session.id,
                command: "/bin/sh".to_string(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(updated.windows.len(), 2);
    }

    /// Story 3.3 AC2: a `WatchWindow` subscriber observes a `WindowLayoutEvent`
    /// reflecting the new tree shape when another client calls `SplitPane`,
    /// without polling `ListSessions`.
    #[tokio::test]
    async fn watch_window_should_emit_layout_event_when_another_client_calls_split_pane() {
        let daemon = test_daemon();
        let session = daemon
            .create_session(Request::new(create_req("test")))
            .await
            .unwrap()
            .into_inner();
        let window_id = session.windows[0].id.clone();
        let pane_id = sole_pane(&session.windows[0]).id.clone();

        let mut watch_stream = daemon
            .watch_window(Request::new(WatchWindowRequest {
                window_id: window_id.clone(),
            }))
            .await
            .unwrap()
            .into_inner();

        // First event: the current (single-leaf) shape, sent immediately.
        let first = tokio::time::timeout(Duration::from_secs(5), watch_stream.next())
            .await
            .expect("watch stream should emit promptly")
            .unwrap()
            .unwrap();
        use tymux_proto::v1::layout::Node;
        assert!(matches!(first.layout.unwrap().node.unwrap(), Node::Pane(_)));

        daemon
            .split_pane(Request::new(SplitPaneRequest {
                pane_id,
                orientation: ProtoOrientation::Vertical as i32,
                command: "/bin/sh".to_string(),
            }))
            .await
            .unwrap();

        let second = tokio::time::timeout(Duration::from_secs(5), watch_stream.next())
            .await
            .expect("watch stream should emit after SplitPane, not require polling ListSessions")
            .unwrap()
            .unwrap();
        assert!(matches!(
            second.layout.unwrap().node.unwrap(),
            Node::Split(_)
        ));
    }

    /// Story 4.6: the daemon-side rejection is the authoritative guard for
    /// any client (Rust or not) — a dead pane must never let `attach` open
    /// a stream, independent of the CLI's own pre-check.
    #[tokio::test]
    async fn attach_rpc_should_reject_with_failed_precondition_when_pane_lookup_is_dead() {
        let daemon = test_daemon();
        let engine = daemon.engine.clone();
        let mut client = spawn_test_server(daemon).await;

        let session = client
            .create_session(create_req("test"))
            .await
            .unwrap()
            .into_inner();
        let pane_id = sole_pane(&session.windows[0]).id.clone();
        let pane_uuid = parse_uuid(&pane_id).unwrap();
        let pane = match engine.pane_lookup(pane_uuid) {
            PaneLookup::Live(pane) => pane,
            _ => panic!("expected freshly created pane to be Live"),
        };
        pane.write_input(b"exit\n").unwrap();
        wait_for_pane_exit(&pane).await;

        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tx.send(AttachRequest {
            payload: Some(attach_request::Payload::PaneId(pane_id)),
        })
        .await
        .unwrap();
        let err = client
            .attach(Request::new(ReceiverStream::new(rx)))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    }
}
