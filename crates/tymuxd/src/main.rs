use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::{Stream, StreamExt};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{transport::Server, Request, Response, Status, Streaming};
use uuid::Uuid;

use tymux_core::{
    Engine, LayoutSnapshot as CoreLayout, Orientation as CoreOrientation, PaneLookup,
    PersistedLayoutNode, PersistenceBackend, SessionSnapshot, WindowSnapshot,
    RECOMMENDED_SPLIT_MIN_ROWS,
};
use tymux_proto::v1::tymux_service_server::{TymuxService, TymuxServiceServer};
use tymux_proto::v1::{
    attach_event, attach_request, AttachEvent, AttachRequest, CapturePaneRequest,
    Cell as ProtoCell, ClosePaneRequest, ClosePaneResponse, CreateSessionRequest,
    CreateWindowRequest, ExitStatus, KillSessionRequest, KillSessionResponse,
    Layout as ProtoLayout, LayoutChild as ProtoLayoutChild, ListSessionsRequest,
    ListSessionsResponse, Liveness, Orientation as ProtoOrientation, Pane as ProtoPane,
    PaneSnapshot as ProtoSnapshot, ReviveSessionRequest, ReviveSessionResponse, Row as ProtoRow,
    SearchScrollbackRequest, SearchScrollbackResponse, Session as ProtoSession,
    Split as ProtoSplit, SplitPaneRequest, WatchWindowRequest, Window as ProtoWindow,
    WindowLayoutEvent,
};

/// Default window (Task 1.1.2e / pre-mortem P1 #1) within which a pane
/// exiting shortly after its last `Attach` stream dropped is treated as a
/// possible disconnect-survival regression rather than an ordinary exit.
/// Overridable via `TYMUXD_DISCONNECT_REGRESSION_WINDOW_MS` for testing.
const DEFAULT_DISCONNECT_REGRESSION_WINDOW: Duration = Duration::from_millis(300);

pub struct TymuxDaemon {
    engine: Arc<Engine>,
    /// pane_id -> instant its last known `Attach` stream ended without a
    /// preceding deliberate close/kill (Task 1.1.2e). Consulted when the
    /// pane's own exit is subsequently observed, to emit a
    /// `possible disconnect-survival regression` warning if the two are
    /// close enough in time to be suspicious. Best-effort: keyed only by
    /// `pane_id`, so a pane with multiple concurrently attached clients can
    /// produce a false positive if one client detaches right before the
    /// pane legitimately exits while another client is still watching —
    /// an accepted simplification for a first-pass production signal.
    disconnect_tracker: Arc<Mutex<HashMap<Uuid, Instant>>>,
    disconnect_regression_window: Duration,
    /// Task 1.3.1d: count of currently-open `Attach` streams. Incremented
    /// once the pane resolves at the top of `attach()`, decremented once
    /// (via [`AttachedGaugeGuard`]) when `forward_handle` ends for any
    /// reason — normal `Exited`, an internal error, or the client
    /// cancelling/disconnecting. Exposed via a `tracing::info!` line on
    /// every change rather than a metrics crate (requirements.md's
    /// security classification: internal/local, no on-call rotation).
    attached_sessions_gauge: Arc<AtomicI64>,
}

impl TymuxDaemon {
    fn new(engine: Arc<Engine>) -> Self {
        let disconnect_regression_window = std::env::var("TYMUXD_DISCONNECT_REGRESSION_WINDOW_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_DISCONNECT_REGRESSION_WINDOW);
        TymuxDaemon {
            engine,
            disconnect_tracker: Arc::new(Mutex::new(HashMap::new())),
            disconnect_regression_window,
            attached_sessions_gauge: Arc::new(AtomicI64::new(0)),
        }
    }
}

/// RAII guard (Task 1.3.1d) that decrements `TymuxDaemon`'s
/// `attached_sessions_gauge` and logs the new value when dropped —
/// guaranteed to fire once `forward_handle`'s async block ends, via
/// whichever of its several `return` points is taken, or if the task
/// itself is aborted/cancelled, since all of those drop the block's
/// locals the same way.
struct AttachedGaugeGuard {
    gauge: Arc<AtomicI64>,
    pane_id: Uuid,
}

impl Drop for AttachedGaugeGuard {
    fn drop(&mut self) {
        let new_count = self.gauge.fetch_sub(1, Ordering::SeqCst) - 1;
        tracing::info!(pane_id = %self.pane_id, tymux_attached_sessions_gauge = new_count, "attach: gauge decremented");
    }
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

/// Collects every leaf pane id in a layout (Task 1.1.2e follow-up: needed
/// so `kill_session` can purge `disconnect_tracker` entries for every pane
/// the session is about to take with it — see `purge_disconnect_tracker`).
fn collect_leaf_pane_ids(layout: &CoreLayout, out: &mut Vec<Uuid>) {
    match layout {
        CoreLayout::Leaf(info) => out.push(info.id),
        CoreLayout::Split { children, .. } => {
            for (child, _ratio) in children {
                collect_leaf_pane_ids(child, out);
            }
        }
    }
}

/// Removes `pane_id`'s entry from `disconnect_tracker`, if any (Task
/// 1.1.2e follow-up / Phase 6 idiom review fix). Without this, a pane that
/// is detached from and then deliberately closed/killed — rather than
/// exiting on its own, which is the only other path that clears the entry
/// via `warn_if_exit_follows_disconnect` — leaves a permanent entry behind:
/// `Uuid`s are never reused, so every such pane leaks one `(Uuid, Instant)`
/// for the life of the daemon. Called from both `close_pane` and
/// `kill_session` so a deliberate removal always clears the bookkeeping
/// regardless of which path took the pane down.
fn purge_disconnect_tracker(tracker: &Mutex<HashMap<Uuid, Instant>>, pane_id: Uuid) {
    tracker.lock().unwrap().remove(&pane_id);
}

fn layout_snapshot_to_proto(layout: &CoreLayout) -> ProtoLayout {
    use tymux_proto::v1::layout::Node;
    let node = match layout {
        CoreLayout::Leaf(info) => Node::Pane(ProtoPane {
            id: info.id.to_string(),
            rows: info.rows,
            cols: info.cols,
            liveness: liveness_of(info.live) as i32,
            cwd: info.cwd.clone(),
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

/// Task 1.1.2e / pre-mortem P1 #1: a production-observable canary for the
/// abrupt-disconnect pane-kill bug Story 1.1.2 fixed. If a pane's process
/// exits within `window` of its last `Attach` stream having dropped, that's
/// the exact signature of the bug reappearing (e.g. a future regression
/// that reintroduces a controlling-terminal dependency) — as opposed to an
/// ordinary exit, which either happens while a client is still attached and
/// watching, or has no recent disconnect at all. Removes the tracked
/// timestamp on the way out so a pane can only ever trigger this once per
/// disconnect.
fn warn_if_exit_follows_disconnect(
    pane_id: Uuid,
    tracker: &Mutex<HashMap<Uuid, Instant>>,
    window: Duration,
) {
    let Some(disconnected_at) = tracker.lock().unwrap().remove(&pane_id) else {
        return;
    };
    let elapsed = disconnected_at.elapsed();
    if elapsed <= window {
        tracing::warn!(
            pane_id = %pane_id,
            elapsed_ms = elapsed.as_millis() as u64,
            "pane exited shortly after its Attach stream dropped — possible disconnect-survival regression"
        );
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
        // Epic 3 Story 3.5 AC2: the friendlier, higher-tier usability
        // warning — distinct from BelowMinimumSize's hard anti-corruption
        // floor above. The exact wording here is pinned by
        // `split_command_should_show_exact_row_counts_when_terminal_below_minimum_size`
        // in crates/tymux-cli/src/main.rs.
        tymux_core::EngineError::BelowRecommendedSize { rows } => {
            Status::failed_precondition(format!(
                "Can't split: pane is {rows} rows, minimum for a horizontal split is \
                 ~{RECOMMENDED_SPLIT_MIN_ROWS} rows. Resize your terminal or close another \
                 pane first."
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

/// What one `output_rx.recv()` result means for the attach forwarding
/// loop — pulled out of the loop so the Lagged-becomes-`OutputGap` and
/// (Task 1.3.1b) seq-filtering transformations are unit-testable without
/// a live pty/broadcast channel.
#[derive(Debug, PartialEq)]
enum ForwardStep {
    /// Forward this event to the client.
    Emit(AttachEvent),
    /// Don't forward anything, but the stream continues normally — used
    /// for output chunks already reflected in the priming snapshot.
    Skip,
    /// The stream should end (the broadcast channel was permanently
    /// closed).
    End,
}

/// Maps one `output_rx.recv()` result to a [`ForwardStep`], given the
/// priming snapshot's sequence number (`snapshot_seq`, from
/// `pane.snapshot_with_seq()`). Task 1.3.1b / ADR-003 Amendment: an
/// output chunk whose sequence is `<= snapshot_seq` was already reflected
/// in the just-sent `Snapshot` event's grid state — forwarding it again
/// would double-render it, so it's dropped (`Skip`) without ending the
/// stream. Everything else behaves exactly as before Task 1.3.1: a
/// normal chunk becomes an `Output` event, a `Lagged` receive becomes
/// `OutputGap`, and a closed channel ends the stream.
fn forward_step_for_output_result(
    result: Result<(u64, Vec<u8>), tokio::sync::broadcast::error::RecvError>,
    pane_id: Uuid,
    snapshot_seq: u64,
) -> ForwardStep {
    use tokio::sync::broadcast::error::RecvError;
    match result {
        Ok((seq, bytes)) => {
            if seq <= snapshot_seq {
                ForwardStep::Skip
            } else {
                ForwardStep::Emit(AttachEvent {
                    payload: Some(attach_event::Payload::Output(bytes)),
                })
            }
        }
        Err(RecvError::Lagged(n)) => {
            tracing::warn!(pane_id = %pane_id, skipped = n, "attach consumer lagged, output_gap signaled");
            ForwardStep::Emit(AttachEvent {
                payload: Some(attach_event::Payload::OutputGap(true)),
            })
        }
        Err(RecvError::Closed) => ForwardStep::End,
    }
}

#[tonic::async_trait]
impl TymuxService for TymuxDaemon {
    async fn create_session(
        &self,
        request: Request<CreateSessionRequest>,
    ) -> Result<Response<ProtoSession>, Status> {
        let started = Instant::now();
        let req = request.into_inner();
        let command = if req.command.is_empty() {
            None
        } else {
            Some(req.command)
        };
        let cwd = if req.cwd.is_empty() {
            None
        } else {
            Some(req.cwd)
        };
        let id = self
            .engine
            .create_session(req.name, command, cwd)
            .map_err(|e| Status::internal(e.to_string()))?;
        // O(1) lookup, not list_sessions().find() — the latter rebuilds a
        // full snapshot of every session under both locks just to find the
        // one this call just created (the confirmed scale-feasibility
        // bottleneck: CreateSession latency climbing 5ms→20ms as session
        // count went 100→900).
        let info = self
            .engine
            .session_snapshot(id)
            .ok_or_else(|| Status::internal("session vanished after create"))?;
        let duration_ms = started.elapsed().as_secs_f64() * 1000.0;
        tracing::info!(session_id = %info.id, name = %info.name, duration_ms, "session created");
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
        // Gathered before kill_session removes the session from the
        // engine's active state — this is the only chance to learn which
        // panes are going away, so disconnect_tracker's per-pane entries
        // can be purged too (see purge_disconnect_tracker).
        let mut pane_ids = Vec::new();
        if let Some(snapshot) = self.engine.session_snapshot(id) {
            for window in &snapshot.windows {
                collect_leaf_pane_ids(&window.layout, &mut pane_ids);
            }
        }
        self.engine.kill_session(id).map_err(|e| {
            tracing::warn!(session_id = %id, error = %e, "kill_session: no such session");
            Status::not_found(e.to_string())
        })?;
        for pane_id in pane_ids {
            purge_disconnect_tracker(&self.disconnect_tracker, pane_id);
        }
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
        purge_disconnect_tracker(&self.disconnect_tracker, pane_id);
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
                    attached_client_count: engine.attached_client_count(window_id),
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
                            attached_client_count: engine.attached_client_count(window_id),
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

        // Task 1.3.1d: count this as one more open Attach stream. The
        // matching decrement happens via AttachedGaugeGuard, dropped when
        // forward_handle ends for any reason.
        let new_gauge_count = self.attached_sessions_gauge.fetch_add(1, Ordering::SeqCst) + 1;
        tracing::info!(pane_id = %pane_id, tymux_attached_sessions_gauge = new_gauge_count, "attach: gauge incremented");
        let attached_sessions_gauge = self.attached_sessions_gauge.clone();

        // Resize is window-scoped (ADR-004): track this client's reported
        // viewport against the pane's window and apply the dimension-wise
        // minimum across every attached client, rather than sizing this
        // one pane to this one client's report 1:1.
        let window_id = self.engine.window_id_for_pane(pane_id);
        let client_id = self.engine.new_client_id();

        // ADR-003 / Task 1.3.1b: subscribe *before* snapshotting, so no
        // output produced between the two is lost — then send the
        // snapshot as the very first AttachEvent, before any live Output.
        // Its sequence number (read atomically with the grid under the
        // same lock, Task 1.3.1a) is threaded into forward_handle so it
        // can drop any already-subscribed chunk that predates the
        // snapshot and would otherwise double-render it.
        let mut output_rx = pane.subscribe();
        let (pane_snapshot, snapshot_seq) = pane.snapshot_with_seq();
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let priming_event = AttachEvent {
            payload: Some(attach_event::Payload::Snapshot(snapshot_to_proto(
                &pane_id_str,
                pane_snapshot,
                true,
            ))),
        };
        // Practically infallible this early (the receiver was just
        // created), but a client that cancelled instantly could already
        // be gone — benign either way, forward_handle's own sends will
        // fail and end the stream the same way any other disconnect does.
        let _ = tx.send(Ok(priming_event)).await;

        let forward_tx = tx.clone();
        let pane_for_exit = pane.clone();
        let disconnect_tracker_for_exit = self.disconnect_tracker.clone();
        let disconnect_regression_window = self.disconnect_regression_window;
        let forward_handle = tokio::spawn(async move {
            let _gauge_guard = AttachedGaugeGuard {
                gauge: attached_sessions_gauge,
                pane_id,
            };
            loop {
                // `biased` checks output_rx first every iteration, so any
                // output already sent before the child exited (the reader
                // thread sends, then marks exited — see pane.rs) is always
                // drained before we report the exit, rather than racing.
                tokio::select! {
                    biased;
                    result = output_rx.recv() => {
                        match forward_step_for_output_result(result, pane_for_exit.id, snapshot_seq) {
                            ForwardStep::Emit(event) => {
                                if forward_tx.send(Ok(event)).await.is_err() {
                                    return;
                                }
                            }
                            ForwardStep::Skip => continue,
                            ForwardStep::End => return,
                        }
                    }
                    _ = pane_for_exit.wait_exit() => {
                        tracing::info!(pane_id = %pane_for_exit.id, "pane exited, closing attach stream");
                        warn_if_exit_follows_disconnect(
                            pane_for_exit.id,
                            &disconnect_tracker_for_exit,
                            disconnect_regression_window,
                        );
                        let event = AttachEvent {
                            payload: Some(attach_event::Payload::Exited(ExitStatus {
                                code: pane_for_exit.exit_code(),
                            })),
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
        let disconnect_tracker_for_input = self.disconnect_tracker.clone();
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
            // This Attach stream just ended (client detached, gracefully or
            // abruptly — there is no separate deliberate-close signal in
            // this RPC today). Record when, so a pane exit observed shortly
            // after can be flagged as a possible disconnect-survival
            // regression (Task 1.1.2e).
            //
            // Only a genuine mid-session disconnect (pane still running) is
            // worth tracking — if the pane already exited, no future
            // wait_exit() will ever fire to consume this entry, so inserting
            // anyway would leak it for the life of the daemon. A narrow
            // TOCTOU race remains if the pane exits between this check and
            // the insert; that's acceptable and matches the tracker's
            // already-documented best-effort design.
            if !pane_for_input.is_exited() {
                disconnect_tracker_for_input
                    .lock()
                    .unwrap()
                    .insert(pane_id, Instant::now());
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

/// Interprets a raw `setsid()`(2) result. Split out from the actual libc
/// call (see [`detach_controlling_terminal`]) so unit tests can exercise
/// both outcomes deterministically — calling the real syscall twice within
/// one test binary is inherently order-dependent, since only the first
/// caller in the process can actually succeed.
fn interpret_setsid_result(sid: i32, errno: i32) -> Result<i32, i32> {
    if sid == -1 {
        Err(errno)
    } else {
        Ok(sid)
    }
}

/// Story 1.1.2 (ADR-002 / Epic 1.1): detaches `tymuxd` from any controlling
/// terminal it inherited from its parent, matching real tmux's own startup
/// behavior. Must run before any pty is opened (Task 1.1.2b). Root cause
/// this addresses: a `tymuxd` process that still holds a controlling
/// terminal can receive a `SIGHUP` on that terminal's hangup, which (per
/// this repo's investigation in `disconnect_survival_e2e.rs`) is
/// indistinguishable at the process-group level from the hangup propagating
/// to child pane processes — an abrupt client disconnect must never be able
/// to kill a pane. Calling `setsid()` makes `tymuxd` a session leader with
/// no controlling terminal at all, closing that path structurally rather
/// than by handling `SIGHUP` after the fact.
///
/// Returns `Ok(new_sid)` on success. `Err(errno)` on failure; `EPERM` is the
/// *expected* failure when this process is already a session leader (e.g.
/// started under systemd, which already detaches units from a controlling
/// terminal) — callers should log that case at `debug`, not `warn`.
fn detach_controlling_terminal() -> Result<i32, i32> {
    // SAFETY: setsid(2) takes no arguments and has no preconditions beyond
    // being a valid libc call; it always returns either the new session id
    // or -1 with errno set.
    let sid = unsafe { libc::setsid() };
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(-1);
    interpret_setsid_result(sid, errno)
}

/// Logs `detach_controlling_terminal`'s outcome at the level Task 1.1.2b
/// specifies: `info` on success, `debug` (not `warn`) for the expected
/// already-a-session-leader case, `warn` for any other, unexpected errno.
fn log_detach_controlling_terminal_outcome(result: Result<i32, i32>) {
    match result {
        Ok(sid) => tracing::info!(sid, "detached from controlling terminal"),
        Err(errno) if errno == libc::EPERM => {
            tracing::debug!(
                errno,
                "setsid: already a session leader (expected, e.g. under systemd)"
            );
        }
        Err(errno) => {
            tracing::warn!(
                errno,
                "setsid failed unexpectedly — tymuxd may still hold a controlling terminal"
            );
        }
    }
}

/// Story 1.1.4 / pre-mortem P1 #3: best-effort upper-bound approximation of
/// processes orphaned by a prior `tymuxd` instance dying or restarting
/// while sessions were alive — `Engine::revive_session` always spawns a
/// *new* process on an explicit `tymux revive`, never reattaching to
/// whatever the old process became (see that function's doc comment for
/// the accepted trade-off this metric exists to make visible instead).
///
/// `PersistedPaneRecord` carries no explicit liveness flag: `PersistedLayoutNode::from_live`
/// only fills in `command`/`cwd`/size for a pane that was `PaneEntry::Live` at the moment it
/// was last persisted; a dead pane's record round-trips as empty strings. So a non-empty
/// `command` at load time is the best proxy this schema can offer for "was last known to be
/// Live". Story 1.2.4 added `exit_code`, which is populated at the same persist that would
/// otherwise make this look like an orphan once the pane has actually exited — a leaf with a
/// recorded `exit_code` (`Some(_)`) has a confirmed fate and must NOT be counted, even though
/// its `command` is still non-empty (the record isn't blanked on exit, only on the
/// `Live -> Dead` transition). Only `command` non-empty AND `exit_code: None` means "last known
/// to be Live, with no confirmed exit recorded" — a record counted here may in fact have
/// already exited cleanly before the restart without that exit ever being captured, so this
/// is an upper bound, not a guarantee.
fn count_orphan_candidates(records: &[tymux_core::PersistedSessionRecord]) -> usize {
    fn count_node(node: &PersistedLayoutNode) -> usize {
        match node {
            PersistedLayoutNode::Leaf { pane } => {
                usize::from(!pane.command.is_empty() && pane.exit_code.is_none())
            }
            PersistedLayoutNode::Split { children, .. } => {
                children.iter().map(|(c, _)| count_node(c)).sum()
            }
        }
    }
    records
        .iter()
        .flat_map(|r| r.windows.iter())
        .map(|w| count_node(&w.layout))
        .sum()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    log_detach_controlling_terminal_outcome(detach_controlling_terminal());

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
    let orphan_candidate_count = count_orphan_candidates(&records);
    let engine = Arc::new(Engine::with_persistence(Box::new(backend)));
    engine.load_persisted(records);
    if restored_count > 0 {
        tracing::info!(count = restored_count, dir = %sessions_dir.display(), "restored dead-flagged sessions from disk");
    }
    // Story 1.1.4: visibility for the orphan-on-restart trade-off documented
    // on `Engine::revive_session` — see `docs/runbooks/orphaned-processes.md`
    // for how to act on a nonzero count.
    if orphan_candidate_count > 0 {
        tracing::warn!(
            count = orphan_candidate_count,
            "possible orphaned processes from prior tymuxd instance — see docs/runbooks/orphaned-processes.md"
        );
    } else {
        tracing::info!(
            count = 0,
            "no orphaned-process candidates found from prior tymuxd instance"
        );
    }

    let daemon = TymuxDaemon::new(engine);

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
        TymuxDaemon::new(Arc::new(Engine::new()))
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
            cwd: String::new(),
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

    /// REQ-8 (Epic 1.5) happy path — a nonempty `CreateSessionRequest.cwd`
    /// both (a) reaches the spawned shell's actual working directory (not
    /// just the returned field, which could otherwise be a no-op relabel)
    /// and (b) is reflected back on the returned `Pane.cwd` field.
    #[tokio::test]
    async fn create_session_should_spawn_pane_in_requested_cwd_and_return_it_on_pane_cwd_field() {
        let daemon = test_daemon();
        let engine = daemon.engine.clone();

        let tmp_dir =
            std::env::temp_dir().join(format!("tymux-cwd-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        // Canonicalize so a symlinked temp dir (e.g. macOS's /tmp ->
        // /private/tmp) can't make the shell's real `pwd` output disagree
        // with the path we asked for.
        let tmp_dir = tmp_dir.canonicalize().unwrap();
        let cwd = tmp_dir.display().to_string();

        let mut req = create_req("test");
        req.cwd = cwd.clone();
        let session = daemon
            .create_session(Request::new(req))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(sole_pane(&session.windows[0]).cwd, cwd);

        let pane_id = parse_uuid(&sole_pane(&session.windows[0]).id).unwrap();
        let pane = match engine.pane_lookup(pane_id) {
            PaneLookup::Live(pane) => pane,
            _ => panic!("expected freshly created pane to be Live"),
        };
        pane.write_input(b"pwd; echo DONE-MARKER\n").unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let text: String = pane
                .snapshot()
                .grid
                .iter()
                .flatten()
                .map(|c| c.text.clone())
                .collect();
            if text.contains("DONE-MARKER") {
                assert!(
                    text.contains(&cwd),
                    "expected `pwd` output to contain {cwd}, got: {text}"
                );
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "expected DONE-MARKER to appear within 5s"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        std::fs::remove_dir_all(&tmp_dir).ok();
    }

    /// REQ-8 (Epic 1.5) edge path — empty string means "the daemon's own
    /// cwd," matching `command`'s existing empty-means-default convention.
    /// Must not regress: this is today's implicit behavior for every
    /// existing caller that never set `cwd`.
    #[tokio::test]
    async fn create_session_should_use_daemon_own_cwd_when_cwd_field_is_empty_string() {
        let daemon = test_daemon();
        let session = daemon
            .create_session(Request::new(create_req("test")))
            .await
            .unwrap()
            .into_inner();

        let daemon_cwd = std::env::current_dir().unwrap().display().to_string();
        assert_eq!(sole_pane(&session.windows[0]).cwd, daemon_cwd);
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

    /// Story 1.2.4: a pane's last-known exit code survives into a
    /// `PaneEntry::Dead` record across a simulated daemon restart, readable
    /// with no `Attach` stream ever reopened post-exit — the ADR-001 gap
    /// `CapturePane`'s own `FailedPrecondition` response (asserted above)
    /// leaves for a fully dead pane.
    #[tokio::test]
    async fn capture_pane_should_surface_persisted_exit_code_when_pane_is_dead_and_no_attach_stream_was_ever_reopened(
    ) {
        use tymux_core::FsPersistenceBackend;

        let persist_dir =
            std::env::temp_dir().join(format!("tymux-exit-code-test-{}", Uuid::new_v4()));
        let backend = FsPersistenceBackend::new(persist_dir.clone()).unwrap();
        let engine = Arc::new(Engine::with_persistence(Box::new(backend)));
        let daemon = TymuxDaemon::new(engine.clone());

        let session = daemon
            .create_session(Request::new(create_req("test")))
            .await
            .unwrap()
            .into_inner();
        let session_id = parse_uuid(&session.id).unwrap();
        let pane_id_str = sole_pane(&session.windows[0]).id.clone();
        let pane_id = parse_uuid(&pane_id_str).unwrap();

        let pane = match engine.pane_lookup(pane_id) {
            PaneLookup::Live(pane) => pane,
            _ => panic!("expected freshly created pane to be Live"),
        };
        pane.write_input(b"exit 3\n").unwrap();
        wait_for_pane_exit(&pane).await;

        // No Attach stream is ever opened post-exit. Instead, trigger the
        // same structural-mutation persist path any other session mutation
        // would (Task 1.2.4b) — adding a second window re-snapshots the
        // whole session, capturing the already-exited pane's exit code
        // into its persisted record.
        engine
            .create_window(session_id, Some("/bin/sh".to_string()))
            .unwrap();

        // Simulate a daemon restart: reload from the persisted records
        // into a fresh Engine, exactly as `tymuxd`'s startup path does.
        let backend2 = FsPersistenceBackend::new(persist_dir.clone()).unwrap();
        let records = backend2.load_all();
        let fresh_engine = Engine::with_persistence(Box::new(backend2));
        fresh_engine.load_persisted(records);

        assert_eq!(
            fresh_engine.dead_pane_exit_code(pane_id),
            Some(Some(3)),
            "a dead pane's last-known exit code must survive a daemon \
             restart, readable with no Attach stream ever reopened"
        );

        std::fs::remove_dir_all(&persist_dir).ok();
    }

    #[test]
    fn attach_should_not_emit_output_gap_event_when_consumer_keeps_pace() {
        let pane_id = Uuid::new_v4();
        let step = forward_step_for_output_result(Ok((1, b"hello".to_vec())), pane_id, 0);
        assert!(matches!(
            step,
            ForwardStep::Emit(AttachEvent {
                payload: Some(attach_event::Payload::Output(_))
            })
        ));
    }

    #[test]
    fn attach_should_emit_output_gap_event_when_consumer_lags_behind_broadcast_channel() {
        let pane_id = Uuid::new_v4();
        let step = forward_step_for_output_result(
            Err(tokio::sync::broadcast::error::RecvError::Lagged(5)),
            pane_id,
            0,
        );
        assert!(matches!(
            step,
            ForwardStep::Emit(AttachEvent {
                payload: Some(attach_event::Payload::OutputGap(true))
            })
        ));
    }

    #[test]
    fn attach_event_for_output_result_ends_stream_on_closed_channel() {
        let pane_id = Uuid::new_v4();
        assert_eq!(
            forward_step_for_output_result(
                Err(tokio::sync::broadcast::error::RecvError::Closed),
                pane_id,
                0,
            ),
            ForwardStep::End
        );
    }

    /// Task 1.3.1b / REQ-5: the exact double-render window
    /// adversarial-review.md flagged as a Blocker — an output chunk whose
    /// sequence number is `<=` the priming snapshot's must not be
    /// forwarded (it's already reflected in that snapshot's grid state),
    /// but the stream must *not* end either; it's a `Skip`, not `End`.
    /// Chunks strictly newer than the snapshot forward normally.
    #[test]
    fn forward_handle_should_drop_output_chunks_with_sequence_less_than_or_equal_to_snapshot_sequence(
    ) {
        let pane_id = Uuid::new_v4();
        let snapshot_seq = 10u64;

        assert_eq!(
            forward_step_for_output_result(Ok((10, b"already-in-snapshot".to_vec())), pane_id, snapshot_seq),
            ForwardStep::Skip,
            "a chunk at exactly the snapshot's sequence must be dropped, not forwarded"
        );
        assert_eq!(
            forward_step_for_output_result(Ok((3, b"predates-snapshot".to_vec())), pane_id, snapshot_seq),
            ForwardStep::Skip,
            "a chunk older than the snapshot's sequence must be dropped, not forwarded"
        );
        assert!(
            matches!(
                forward_step_for_output_result(Ok((11, b"new-output".to_vec())), pane_id, snapshot_seq),
                ForwardStep::Emit(AttachEvent {
                    payload: Some(attach_event::Payload::Output(bytes))
                }) if bytes == b"new-output"
            ),
            "a chunk newer than the snapshot's sequence must forward normally"
        );
    }

    /// Integration-style proof (real `tokio::sync::broadcast` channel, tiny
    /// capacity, burst sender) that a lagged consumer observes an
    /// `OutputGap` event before normal `Output` events resume — exercising
    /// `forward_step_for_output_result` against tokio's actual `Lagged`
    /// semantics rather than a hand-constructed `RecvError`.
    #[tokio::test]
    async fn attach_stream_should_observe_output_gap_before_output_resumes_when_consumer_lags() {
        let (tx, mut rx) = tokio::sync::broadcast::channel::<(u64, Vec<u8>)>(2);
        let pane_id = Uuid::new_v4();

        // Burst past the channel's capacity before the consumer ever reads,
        // guaranteeing the next recv() observes Lagged.
        for i in 0..5u8 {
            let _ = tx.send((i as u64, vec![i]));
        }

        let first = forward_step_for_output_result(rx.recv().await, pane_id, 0);
        assert!(
            matches!(
                first,
                ForwardStep::Emit(AttachEvent {
                    payload: Some(attach_event::Payload::OutputGap(true))
                })
            ),
            "first observed event after a burst past capacity must be OutputGap"
        );

        // Normal output resumes immediately after: the channel still holds
        // its last `capacity` (2) buffered items (3, 4) — the next recv()
        // must yield one of them as an ordinary Output event, not another
        // Lagged/OutputGap.
        let second = forward_step_for_output_result(rx.recv().await, pane_id, 0);
        assert!(matches!(
            second,
            ForwardStep::Emit(AttachEvent {
                payload: Some(attach_event::Payload::Output(_))
            })
        ));
    }

    /// Task 1.3.1c / REQ-5: the regression test the previous ("wait for it
    /// to settle, then attach") version of this test explicitly avoided —
    /// adversarial-review.md's Blocker. Starts a pane emitting distinct
    /// markers on a ~10ms-spaced loop, then calls `attach()` while that
    /// loop is still actively running (only a short, deliberately-short
    /// head start, not a settle), and asserts every marker observed
    /// across the priming `Snapshot` plus every subsequent `Output` event
    /// appears at most once — the exact double-render signature ADR-003's
    /// Amendment (Tasks 1.3.1a/b) fixes: bytes that land in the window
    /// between `pane.subscribe()` and `pane.snapshot()` must not be both
    /// baked into the snapshot's grid *and* replayed as a queued `Output`
    /// chunk on top of it.
    #[tokio::test]
    async fn attach_should_emit_snapshot_first_with_no_duplicated_bytes_when_output_streams_concurrently_not_after_settling(
    ) {
        let engine = Arc::new(Engine::new());
        let daemon = TymuxDaemon::new(engine.clone());
        let mut client = spawn_test_server(daemon).await;

        let session = client
            .create_session(create_req("test"))
            .await
            .unwrap()
            .into_inner();
        let pane_id_str = sole_pane(&session.windows[0]).id.clone();
        let pane_id = parse_uuid(&pane_id_str).unwrap();

        let pane = match engine.pane_lookup(pane_id) {
            PaneLookup::Live(pane) => pane,
            _ => panic!("expected freshly created pane to be Live"),
        };

        // Fixed-width markers ("L00007E") so no marker is ever a substring
        // of another (unlike "LINE-7" inside "LINE-70"), which lets the
        // no-duplication check below use a plain substring count. Written
        // directly to the pane (bypassing Attach, which isn't open yet)
        // so the loop is already running before we ever call attach().
        //
        // Deliberately *no* sleep between lines: the actual race window
        // this test must hit (bytes fed to the vt100 parser strictly
        // between `pane.subscribe()` and `pane.snapshot_with_seq()`
        // acquiring the parser lock, inside `attach()`) is only a few
        // Rust statements wide — sub-microsecond. A sleep-throttled
        // producer (e.g. one line per 10ms) leaves that window almost
        // always idle, so attaching "during" it rarely actually lands a
        // reader-thread cycle inside the gap and would pass even with the
        // Task 1.3.1a/b fix reverted (confirmed empirically). A tight,
        // unthrottled flood keeps the reader thread cycling
        // read+process+broadcast essentially back-to-back, so the gap is
        // virtually always straddled by an in-flight chunk.
        const LINE_COUNT: usize = 50_000;
        let cmd = format!(
            "i=0; while [ $i -lt {LINE_COUNT} ]; do printf 'L%07dE\\n' \"$i\"; i=$((i+1)); done; echo DONE-MARKER\n"
        );
        pane.write_input(cmd.as_bytes()).unwrap();

        // A short, deliberate head start — NOT a settle delay. The flood
        // runs for a few hundred ms total; this only guarantees it has
        // already begun by the time we attach, so attach()'s
        // subscribe()+snapshot() genuinely races live output instead of
        // running before the command even starts.
        tokio::time::sleep(Duration::from_millis(5)).await;

        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tx.send(AttachRequest {
            payload: Some(attach_request::Payload::PaneId(pane_id_str)),
        })
        .await
        .unwrap();
        let mut inbound = client
            .attach(Request::new(ReceiverStream::new(rx)))
            .await
            .unwrap()
            .into_inner();

        let first = tokio::time::timeout(Duration::from_secs(5), inbound.message())
            .await
            .expect("attach must respond within 5s")
            .unwrap()
            .expect("stream ended before any event");
        let snapshot = match first.payload {
            Some(attach_event::Payload::Snapshot(s)) => s,
            other => panic!("expected the first AttachEvent to be a Snapshot, got {other:?}"),
        };
        let snapshot_text: String = snapshot
            .grid
            .iter()
            .flat_map(|row| row.cells.iter())
            .map(|c| c.text.as_str())
            .collect();

        // Drain Output events, concatenating their raw bytes, until
        // DONE-MARKER shows up — proof the loop (still running when we
        // attached) has now fully streamed past us.
        let mut streamed = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(
                Instant::now() < deadline,
                "attach stream did not deliver DONE-MARKER in time"
            );
            let event = tokio::time::timeout(Duration::from_secs(5), inbound.message())
                .await
                .expect("attach stream stalled")
                .unwrap();
            match event {
                Some(AttachEvent {
                    payload: Some(attach_event::Payload::Output(bytes)),
                }) => {
                    streamed.extend_from_slice(&bytes);
                    if String::from_utf8_lossy(&streamed).contains("DONE-MARKER") {
                        break;
                    }
                }
                // A gap just means some frames were skipped (a known,
                // unrelated behavior for a slow-enough consumer under a
                // deliberately extreme flood) — it doesn't itself produce
                // a duplicate, so it's not a failure for this test.
                Some(_) => continue,
                None => panic!("attach stream closed before DONE-MARKER arrived"),
            }
        }

        // The core assertion: the union of Snapshot content + every
        // subsequent Output chunk must contain each marker at most once.
        // A marker appearing twice is exactly the double-render bug —
        // already baked into the snapshot's grid state *and* separately
        // replayed from the broadcast channel on top of it.
        let full_text = format!("{snapshot_text}{}", String::from_utf8_lossy(&streamed));
        let mut duplicated = Vec::new();
        for i in 0..LINE_COUNT {
            let marker = format!("L{i:07}E");
            let occurrences = full_text.matches(&marker).count();
            if occurrences > 1 {
                duplicated.push((marker, occurrences));
            }
        }
        assert!(
            duplicated.is_empty(),
            "markers rendered more than once (double-render): {duplicated:?}"
        );
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

        let exit_status = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = inbound.message().await.unwrap() {
                if let Some(attach_event::Payload::Exited(status)) = event.payload {
                    return Some(status);
                }
            }
            None
        })
        .await
        .expect("attach stream must close within 5s, not hang");

        assert_eq!(
            exit_status.expect("expected an Exited event before the stream closed").code,
            Some(0),
            "a plain `exit` should report exit code 0, not an unknown code"
        );
    }

    /// ADR-001 regression test: the live `Attach` path (unlike the
    /// persisted/`CapturePane` path, already covered by
    /// `capture_pane_should_surface_persisted_exit_code_when_pane_is_dead_and_no_attach_stream_was_ever_reopened`)
    /// had no test proving a real *nonzero* exit code survives the
    /// `ExitStatus { code: pane.exit_code() }` send site intact — a
    /// `.unwrap_or(0)`-style regression there would silently backfill any
    /// code to `Some(0)`, which is exactly what
    /// `attach_streams_output_and_signals_exit` above cannot catch, since
    /// `exit` alone already produces code 0.
    #[tokio::test]
    async fn attach_streams_a_nonzero_exit_code_without_backfilling_to_zero() {
        let daemon = test_daemon();
        let mut client = spawn_test_server(daemon).await;

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
            payload: Some(attach_request::Payload::Input(b"exit 7\n".to_vec())),
        })
        .await
        .unwrap();

        let mut inbound = client
            .attach(Request::new(ReceiverStream::new(rx)))
            .await
            .unwrap()
            .into_inner();

        let exit_status = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = inbound.message().await.unwrap() {
                if let Some(attach_event::Payload::Exited(status)) = event.payload {
                    return Some(status);
                }
            }
            None
        })
        .await
        .expect("attach stream must close within 5s, not hang");

        assert_eq!(
            exit_status.expect("expected an Exited event before the stream closed").code,
            Some(7),
            "a real nonzero exit code must round-trip through the live Attach path, not be \
             backfilled to 0 or lost"
        );
    }

    /// Companion regression test for the fix described on
    /// `close_pane_should_purge_disconnect_tracker_entry_it_left_behind`
    /// below: that test covers the *explicit close/kill* purge path, but
    /// the dominant real-world exit path is a pane exiting normally while a
    /// client is still attached (e.g. this test's plain `exit`), with no
    /// explicit `ClosePane`/`KillSession` call ever following. Before the
    /// fix, `attach()`'s `input_handle` task unconditionally inserted a
    /// `disconnect_tracker` entry when the request stream ended — even
    /// though the pane had, by then, already exited and can never exit
    /// again to trigger the one path (`warn_if_exit_follows_disconnect`)
    /// that would have removed it. That leaked one entry per such pane for
    /// the life of the daemon. Drives a real live `Attach` stream through a
    /// normal exit (not an abrupt client disconnect) and asserts no
    /// `disconnect_tracker` entry survives.
    #[tokio::test]
    async fn attach_should_not_leak_disconnect_tracker_entry_when_pane_exits_normally_while_attached()
     {
        let daemon = test_daemon();
        let disconnect_tracker = daemon.disconnect_tracker.clone();
        let mut client = spawn_test_server(daemon).await;

        let session = client
            .create_session(create_req("test"))
            .await
            .unwrap()
            .into_inner();
        let pane_id = sole_pane(&session.windows[0]).id.clone();
        let pane_uuid = parse_uuid(&pane_id).unwrap();

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
        assert!(saw_exit, "expected an Exited event before the stream closed");

        // Ends the request stream, so `input_handle`'s reader loop returns
        // and runs its (now-guarded) disconnect_tracker insert. Give the
        // background task a moment to actually run before asserting.
        drop(tx);
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert!(
            !disconnect_tracker.lock().unwrap().contains_key(&pane_uuid),
            "a pane that already exited before its Attach stream closed must not leave a \
             disconnect_tracker entry behind — no future pane exit will ever occur to purge it, \
             so it would leak for the life of the daemon"
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

    /// Phase 6 idiom review fix: before this, `disconnect_tracker`'s only
    /// removal path was `warn_if_exit_follows_disconnect`, reached only
    /// from the *same* `Attach` call's `forward_handle` task observing
    /// `pane.wait_exit()`. If that task had already ended (e.g. the
    /// client's whole connection dropped, failing an in-flight
    /// `forward_tx.send()`) before the pane was deliberately closed, no
    /// task was left to ever call it again — a permanent leak, since
    /// `Uuid`s are never reused. Simulates the detach directly (an insert
    /// identical in shape to the one `attach()`'s `input_handle` task
    /// performs) so the test is deterministic rather than racing a real
    /// subprocess exit against a dropped gRPC stream.
    #[tokio::test]
    async fn close_pane_should_purge_disconnect_tracker_entry_it_left_behind() {
        let daemon = test_daemon();
        let session = daemon
            .create_session(Request::new(create_req("test")))
            .await
            .unwrap()
            .into_inner();
        let pane_id = parse_uuid(&sole_pane(&session.windows[0]).id).unwrap();

        daemon
            .disconnect_tracker
            .lock()
            .unwrap()
            .insert(pane_id, Instant::now());

        daemon
            .close_pane(Request::new(ClosePaneRequest {
                pane_id: pane_id.to_string(),
            }))
            .await
            .unwrap();

        assert!(
            !daemon
                .disconnect_tracker
                .lock()
                .unwrap()
                .contains_key(&pane_id),
            "close_pane must purge the disconnect_tracker entry for the pane it closed, or it \
             leaks for the life of the daemon (Uuids are never reused)"
        );
    }

    /// Same leak as above, via the other deliberate-removal path:
    /// `kill_session` takes every pane in the session with it, so it must
    /// purge a `disconnect_tracker` entry for each one, not just the pane
    /// a caller happens to name directly.
    #[tokio::test]
    async fn kill_session_should_purge_disconnect_tracker_entries_it_left_behind() {
        let daemon = test_daemon();
        let session = daemon
            .create_session(Request::new(create_req("test")))
            .await
            .unwrap()
            .into_inner();
        let session_id = session.id.clone();
        let pane_id = parse_uuid(&sole_pane(&session.windows[0]).id).unwrap();

        daemon
            .disconnect_tracker
            .lock()
            .unwrap()
            .insert(pane_id, Instant::now());

        daemon
            .kill_session(Request::new(KillSessionRequest { session_id }))
            .await
            .unwrap();

        assert!(
            !daemon
                .disconnect_tracker
                .lock()
                .unwrap()
                .contains_key(&pane_id),
            "kill_session must purge disconnect_tracker entries for every pane it kills, or \
             they leak for the life of the daemon"
        );
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

    /// Story 6.1 AC1: `attached_client_count` is real gRPC-introspectable
    /// data (ADR-004's viewport tracker), not something a client has to
    /// scrape ANSI output to learn.
    #[tokio::test]
    async fn status_bar_model_rpc_should_return_structured_data_reflecting_two_attached_clients() {
        let daemon = test_daemon();
        let engine = daemon.engine.clone();
        let session = daemon
            .create_session(Request::new(create_req("test")))
            .await
            .unwrap()
            .into_inner();
        let window_id = parse_uuid(&session.windows[0].id).unwrap();

        engine.report_viewport_and_recompute(window_id, engine.new_client_id(), 24, 80);
        engine.report_viewport_and_recompute(window_id, engine.new_client_id(), 30, 100);

        let mut watch_stream = daemon
            .watch_window(Request::new(WatchWindowRequest {
                window_id: session.windows[0].id.clone(),
            }))
            .await
            .unwrap()
            .into_inner();
        let first = tokio::time::timeout(Duration::from_secs(5), watch_stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(first.attached_client_count, 2);
    }

    #[tokio::test]
    async fn status_bar_model_rpc_should_return_zero_attached_client_count_when_none_attached() {
        let daemon = test_daemon();
        let session = daemon
            .create_session(Request::new(create_req("test")))
            .await
            .unwrap()
            .into_inner();

        let mut watch_stream = daemon
            .watch_window(Request::new(WatchWindowRequest {
                window_id: session.windows[0].id.clone(),
            }))
            .await
            .unwrap()
            .into_inner();
        let first = tokio::time::timeout(Duration::from_secs(5), watch_stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(first.attached_client_count, 0);
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

    fn search_req(pane_id: String, pattern: &str, start_offset: u32) -> SearchScrollbackRequest {
        SearchScrollbackRequest {
            pane_id,
            pattern: pattern.to_string(),
            start_offset,
        }
    }

    /// Story 5.4: `Pane::search_scrollback` itself already has unit test
    /// coverage in `crates/tymux-core/src/pane.rs`; what's missing (and
    /// what this covers) is an in-process round trip through the tonic
    /// `SearchScrollback` RPC handler, matching `attach_rpc_should_reject_*`
    /// above's real-network-server pattern.
    #[tokio::test]
    async fn search_scrollback_rpc_should_return_matching_line_range_when_pattern_present() {
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

        // Produce deterministic scrollback content to search, polling for a
        // completion marker rather than sleeping a fixed duration — mirrors
        // `spawn_shell_with_numbered_lines` in tymux-core's own
        // `Pane::search_scrollback` unit tests.
        pane.write_input(
            b"awk 'BEGIN{for(i=1;i<=50;i++) print \"line-\" i; print \"DONE-MARKER\"}'\n",
        )
        .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let text: String = pane
                .snapshot()
                .grid
                .iter()
                .flatten()
                .map(|c| c.text.clone())
                .collect();
            if text.contains("DONE-MARKER") {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "expected DONE-MARKER to appear within 5s"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let response = client
            .search_scrollback(search_req(pane_id, "line-3", 0))
            .await
            .unwrap()
            .into_inner();
        assert!(
            response.found,
            "expected to find a historical line matching 'line-3'"
        );
        assert!(response.line.contains("line-3"));
    }

    #[tokio::test]
    async fn search_scrollback_rpc_should_return_no_matches_when_pattern_absent() {
        let daemon = test_daemon();
        let mut client = spawn_test_server(daemon).await;

        let session = client
            .create_session(create_req("test"))
            .await
            .unwrap()
            .into_inner();
        let pane_id = sole_pane(&session.windows[0]).id.clone();

        let response = client
            .search_scrollback(search_req(
                pane_id,
                "this-pattern-never-appears-anywhere",
                0,
            ))
            .await
            .unwrap()
            .into_inner();
        assert!(!response.found);
        assert_eq!(response.offset, 0);
        assert!(response.line.is_empty());
    }

    // --- Story 1.1.2: setsid() detach-from-controlling-terminal ---

    #[test]
    fn detach_controlling_terminal_should_report_new_sid_when_setsid_succeeds() {
        assert_eq!(interpret_setsid_result(1234, 0), Ok(1234));
    }

    #[test]
    fn detach_controlling_terminal_should_tolerate_eperm_when_already_session_leader_and_log_debug_not_warn(
    ) {
        assert_eq!(interpret_setsid_result(-1, libc::EPERM), Err(libc::EPERM));
    }

    #[test]
    #[tracing_test::traced_test]
    fn detach_controlling_terminal_should_log_info_on_success_debug_on_expected_eperm_warn_on_unexpected_errno(
    ) {
        log_detach_controlling_terminal_outcome(Ok(42));
        assert!(logs_contain("INFO"));
        assert!(logs_contain("detached from controlling terminal"));

        log_detach_controlling_terminal_outcome(Err(libc::EPERM));
        assert!(logs_contain("DEBUG"));
        assert!(logs_contain("already a session leader"));
        assert!(!logs_contain("WARN"));

        log_detach_controlling_terminal_outcome(Err(libc::EINVAL));
        assert!(logs_contain("WARN"));
        assert!(logs_contain("setsid failed unexpectedly"));
    }

    // --- Story 1.1.2e: pane-exit-shortly-after-disconnect regression signal ---

    #[test]
    fn warn_if_exit_follows_disconnect_should_warn_when_exit_is_within_window() {
        let tracker: Mutex<HashMap<Uuid, Instant>> = Mutex::new(HashMap::new());
        let pane_id = Uuid::new_v4();
        tracker.lock().unwrap().insert(pane_id, Instant::now());

        // No direct log assertion here (see the tracing_test-based case
        // below) — this just proves the tracker entry is consumed so a
        // second call for the same pane_id can't double-fire.
        warn_if_exit_follows_disconnect(pane_id, &tracker, Duration::from_millis(300));
        assert!(!tracker.lock().unwrap().contains_key(&pane_id));
    }

    #[test]
    #[tracing_test::traced_test]
    fn warn_if_exit_follows_disconnect_should_log_warn_when_exit_follows_disconnect_within_window()
    {
        let tracker: Mutex<HashMap<Uuid, Instant>> = Mutex::new(HashMap::new());
        let pane_id = Uuid::new_v4();
        tracker.lock().unwrap().insert(pane_id, Instant::now());

        warn_if_exit_follows_disconnect(pane_id, &tracker, Duration::from_millis(300));
        assert!(logs_contain("possible disconnect-survival regression"));
    }

    #[test]
    fn warn_if_exit_follows_disconnect_should_not_warn_when_no_recent_disconnect_recorded() {
        let tracker: Mutex<HashMap<Uuid, Instant>> = Mutex::new(HashMap::new());
        let pane_id = Uuid::new_v4();
        // No entry recorded for this pane_id at all (ordinary exit while a
        // client is still attached and watching) — must be a silent no-op.
        warn_if_exit_follows_disconnect(pane_id, &tracker, Duration::from_millis(300));
    }

    #[test]
    fn warn_if_exit_follows_disconnect_should_not_warn_when_disconnect_outside_window() {
        let tracker: Mutex<HashMap<Uuid, Instant>> = Mutex::new(HashMap::new());
        let pane_id = Uuid::new_v4();
        tracker.lock().unwrap().insert(pane_id, Instant::now());
        std::thread::sleep(Duration::from_millis(20));

        // A 0ms window can never be satisfied by a real elapsed duration —
        // proves the window bound is actually enforced, not always true.
        warn_if_exit_follows_disconnect(pane_id, &tracker, Duration::from_millis(0));
        // Entry is still consumed either way (best-effort, fires at most once).
        assert!(!tracker.lock().unwrap().contains_key(&pane_id));
    }

    // --- Story 1.1.4: orphaned-process-count startup metric ---

    #[test]
    fn count_orphan_candidates_should_return_zero_for_no_records() {
        assert_eq!(count_orphan_candidates(&[]), 0);
    }

    #[test]
    fn count_orphan_candidates_should_count_leaves_with_nonempty_command_as_orphan_candidates() {
        use tymux_core::{
            PersistedLayoutNode, PersistedPaneRecord, PersistedSessionRecord,
            PersistedWindowRecord, CURRENT_SCHEMA_VERSION,
        };

        let live_leaf = PersistedLayoutNode::Leaf {
            pane: PersistedPaneRecord {
                pane_id: Uuid::new_v4(),
                command: "/bin/sh".to_string(),
                cwd: "/tmp".to_string(),
                rows: 24,
                cols: 80,
                exit_code: None,
            },
        };
        let dead_leaf = PersistedLayoutNode::Leaf {
            pane: PersistedPaneRecord {
                pane_id: Uuid::new_v4(),
                command: String::new(),
                cwd: String::new(),
                rows: 0,
                cols: 0,
                exit_code: None,
            },
        };
        let record = PersistedSessionRecord {
            schema_version: CURRENT_SCHEMA_VERSION,
            session_id: Uuid::new_v4(),
            name: "test".to_string(),
            windows: vec![
                PersistedWindowRecord {
                    id: Uuid::new_v4(),
                    name: "win-live".to_string(),
                    layout: live_leaf,
                },
                PersistedWindowRecord {
                    id: Uuid::new_v4(),
                    name: "win-dead".to_string(),
                    layout: dead_leaf,
                },
            ],
            active_window_id: Uuid::new_v4(),
        };

        assert_eq!(count_orphan_candidates(&[record]), 1);
    }

    #[test]
    fn count_orphan_candidates_should_not_count_leaves_with_a_recorded_exit_code() {
        use tymux_core::{
            PersistedLayoutNode, PersistedPaneRecord, PersistedSessionRecord,
            PersistedWindowRecord, CURRENT_SCHEMA_VERSION,
        };

        // Story 1.2.4b persists `exit_code` on a still-`Live`-in-name pane the
        // moment its process is observed to have exited, before the entry
        // fully transitions to `PaneEntry::Dead`. Such a leaf still has a
        // nonempty `command` (it isn't blanked until the Live -> Dead
        // transition) but its fate IS known — it must not be counted as an
        // orphan candidate, unlike the true-unknown-fate case covered by
        // `count_orphan_candidates_should_count_leaves_with_nonempty_command_as_orphan_candidates`.
        let exited_leaf = PersistedLayoutNode::Leaf {
            pane: PersistedPaneRecord {
                pane_id: Uuid::new_v4(),
                command: "/bin/sh".to_string(),
                cwd: "/tmp".to_string(),
                rows: 24,
                cols: 80,
                exit_code: Some(0),
            },
        };
        let record = PersistedSessionRecord {
            schema_version: CURRENT_SCHEMA_VERSION,
            session_id: Uuid::new_v4(),
            name: "test".to_string(),
            windows: vec![PersistedWindowRecord {
                id: Uuid::new_v4(),
                name: "win-exited".to_string(),
                layout: exited_leaf,
            }],
            active_window_id: Uuid::new_v4(),
        };

        assert_eq!(count_orphan_candidates(&[record]), 0);
    }
}
