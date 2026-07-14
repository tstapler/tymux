use std::pin::Pin;
use std::sync::Arc;

use futures::{Stream, StreamExt};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{transport::Server, Request, Response, Status, Streaming};
use uuid::Uuid;

use tymux_core::Engine;
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

fn session_to_proto(id: Uuid, name: String, window_id: Uuid, pane_id: Uuid) -> ProtoSession {
    ProtoSession {
        id: id.to_string(),
        name,
        windows: vec![ProtoWindow {
            id: window_id.to_string(),
            name: "0".to_string(),
            panes: vec![ProtoPane {
                id: pane_id.to_string(),
                rows: 24,
                cols: 80,
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
        let (_, name, window_id, pane_id) = self
            .engine
            .list_sessions()
            .into_iter()
            .find(|(sid, ..)| *sid == id)
            .ok_or_else(|| Status::internal("session vanished after create"))?;
        Ok(Response::new(session_to_proto(
            id, name, window_id, pane_id,
        )))
    }

    async fn list_sessions(
        &self,
        _request: Request<ListSessionsRequest>,
    ) -> Result<Response<ListSessionsResponse>, Status> {
        let sessions = self
            .engine
            .list_sessions()
            .into_iter()
            .map(|(id, name, window_id, pane_id)| session_to_proto(id, name, window_id, pane_id))
            .collect();
        Ok(Response::new(ListSessionsResponse { sessions }))
    }

    async fn kill_session(
        &self,
        request: Request<KillSessionRequest>,
    ) -> Result<Response<KillSessionResponse>, Status> {
        let id = parse_uuid(&request.into_inner().session_id)?;
        self.engine
            .kill_session(id)
            .map_err(|e| Status::not_found(e.to_string()))?;
        Ok(Response::new(KillSessionResponse {}))
    }

    async fn capture_pane(
        &self,
        request: Request<CapturePaneRequest>,
    ) -> Result<Response<ProtoSnapshot>, Status> {
        let pane_id_str = request.into_inner().pane_id;
        let pane_id = parse_uuid(&pane_id_str)?;
        let pane = self
            .engine
            .pane(pane_id)
            .ok_or_else(|| Status::not_found("no such pane"))?;
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
        let pane = self
            .engine
            .pane(pane_id)
            .ok_or_else(|| Status::not_found("no such pane"))?;

        let mut output_rx = pane.subscribe();
        let (tx, rx) = tokio::sync::mpsc::channel(64);

        let forward_tx = tx.clone();
        let pane_for_exit = pane.clone();
        tokio::spawn(async move {
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
                        let event = AttachEvent {
                            payload: Some(attach_event::Payload::Exited(true)),
                        };
                        let _ = forward_tx.send(Ok(event)).await;
                        return;
                    }
                }
            }
        });

        let pane_for_input = pane.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = inbound.next().await {
                match msg.payload {
                    Some(attach_request::Payload::Input(bytes)) => {
                        let _ = pane_for_input.write_input(&bytes);
                    }
                    Some(attach_request::Payload::Resize(r)) => {
                        let _ = pane_for_input.resize(r.rows as u16, r.cols as u16);
                    }
                    _ => {}
                }
            }
        });

        Ok(Response::new(
            Box::pin(ReceiverStream::new(rx)) as Self::AttachStream
        ))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::var("TYMUXD_ADDR").unwrap_or_else(|_| "127.0.0.1:7419".to_string());
    let engine = Arc::new(Engine::new());
    let daemon = TymuxDaemon { engine };

    println!("tymuxd listening on {addr}");
    Server::builder()
        .add_service(TymuxServiceServer::new(daemon))
        .serve(addr.parse()?)
        .await?;
    Ok(())
}
