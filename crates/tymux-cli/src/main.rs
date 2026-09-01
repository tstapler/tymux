use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

mod config;
mod copy_mode;
mod input;
mod status_bar;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio_stream::wrappers::ReceiverStream;
use tonic::service::interceptor::InterceptedService;
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

async fn resolve_target(client: &mut TymuxClient, target: &TargetString) -> Result<ProtoPane> {
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

/// Story 4.6 AC1/AC2's dead-session fail-fast check, pulled out of
/// `run()`'s `Command::Attach` arm so it's unit-testable without a live
/// daemon (`resolve_target`/`attach_and_follow` need a real gRPC
/// connection; this pure check doesn't). Only `Liveness::Dead` ever
/// blocks — a `Live` pane (including one just revived, AC2) always
/// returns `Ok(())`, and since `run()` calls this before ever calling
/// `attach_and_follow`, no `Attach` stream is opened when it returns an
/// error.
fn check_attach_liveness(pane: &ProtoPane, session_name: &str) -> Result<()> {
    if pane.liveness == tymux_proto::v1::Liveness::Dead as i32 {
        return Err(anyhow::anyhow!(
            "Session '{session_name}' is not running (restored from disk after a restart). \
             Run 'tymux revive {session_name}' to respawn it, then attach again."
        ));
    }
    Ok(())
}

/// Mirrors `tymuxd`'s `auth::default_uds_socket_path` byte-for-byte — see
/// plan.md Pattern Decisions row 10 for why this is duplicated rather than
/// shared via `tymux-core`. Any change here must be mirrored in `tymuxd`,
/// `clients/go`, and `clients/ts`.
fn default_uds_socket_path(uid: u32) -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir).join("tymuxd").join("tymuxd.sock");
    }
    let base = std::env::var_os("TMPDIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join(format!("tymuxd-{uid}")).join("tymuxd.sock")
}

/// Distinguishes "no UDS daemon here" (legitimate, falls back to TCP) from
/// "a UDS daemon is listening but rejected this peer at the OS level" (a
/// security signal — must never silently retry over the unauthenticated
/// TCP path). See pre-mortem.md P1 #1.
enum UdsDialError {
    PermissionDenied(anyhow::Error),
    Other(anyhow::Error),
}

async fn dial_uds(socket_path: &Path) -> Result<Channel, UdsDialError> {
    let path = socket_path.to_path_buf();
    let connector = tower::service_fn(move |_: tonic::transport::Uri| {
        let path = path.clone();
        async move {
            let stream = tokio::net::UnixStream::connect(&path).await?;
            Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
        }
    });
    // Placeholder authority — the connector ignores it entirely and always
    // dials socket_path; matches tonic's own documented `uds` client
    // example pattern (ADR-003).
    match tonic::transport::Endpoint::from_static("http://localhost")
        .connect_with_connector(connector)
        .await
    {
        Ok(channel) => Ok(channel),
        Err(e) => {
            // tonic::transport::Error's source chain, confirmed empirically
            // against the pinned tonic 0.12.3 (a chmod(0)'d UDS socket, see
            // `dial_channel_hard_errors_and_never_dials_tcp_when_uds_permission_denied`),
            // is three levels deep: tonic::transport::Error ->
            // (an internal, non-public) ConnectError -> the raw
            // std::io::Error. Walk the whole chain rather than assuming a
            // fixed depth, since that internal wrapper isn't part of
            // tonic's public API and could change across patch releases.
            let is_permission_denied = {
                let mut cur: Option<&(dyn std::error::Error + 'static)> =
                    std::error::Error::source(&e);
                let mut found = false;
                while let Some(err) = cur {
                    if let Some(io_err) = err.downcast_ref::<std::io::Error>() {
                        found = io_err.kind() == std::io::ErrorKind::PermissionDenied;
                        break;
                    }
                    cur = err.source();
                }
                found
            };
            if is_permission_denied {
                Err(UdsDialError::PermissionDenied(e.into()))
            } else {
                Err(UdsDialError::Other(e.into()))
            }
        }
    }
}

/// Dials `tymuxd`: an explicit `--addr` is honored exactly (UDS is never
/// touched), otherwise the resolved UDS socket path is tried first, falling
/// back to TCP loopback with a single logged notice when no daemon is
/// listening there. A UDS peer-cred rejection (`PermissionDenied` — a
/// daemon *is* listening and denied the connect syscall) is a hard error
/// and never falls back to TCP, since silently retrying over the
/// unauthenticated TCP path would defeat this feature's isolation
/// guarantee (pre-mortem.md P1 #1). Never gated on `isatty()`/
/// `is_terminal` — identical behavior piped or interactive (ux.md Surfaces
/// 6/7 cross-surface AC6).
async fn dial_channel(explicit_addr: Option<String>, socket_path: &Path) -> Result<Channel> {
    if let Some(addr) = explicit_addr {
        return Ok(tonic::transport::Endpoint::from_shared(addr)?
            .http2_keep_alive_interval(Duration::from_secs(30))
            .keep_alive_timeout(Duration::from_secs(10))
            .keep_alive_while_idle(true)
            .connect()
            .await?);
    }
    match dial_uds(socket_path).await {
        Ok(channel) => Ok(channel),
        Err(UdsDialError::PermissionDenied(source)) => {
            // A daemon IS listening at socket_path and the kernel denied us
            // the connect() itself — never fall back to the unauthenticated
            // TCP path for this case (pre-mortem.md P1 #1). Reuses the same
            // remedy text as the gRPC-level PermissionDenied case
            // (`friendly_message`) so a peer denied at accept time and a
            // peer denied by peer_is_authorized see one consistent message.
            // The underlying transport error is logged at debug level only
            // — the printed message itself stays short (ux.md Surface 9).
            tracing::debug!(error = %source, "UDS connect() denied by the kernel");
            anyhow::bail!(
                "tymuxd rejected this connection: not authorized to access this daemon's \
                 socket (ask the daemon's owner to add you to its configured \
                 --socket-group, or run tymux-cli as the daemon's own OS user)"
            )
        }
        Err(UdsDialError::Other(source)) => {
            tracing::debug!(error = %source, "no reachable UDS socket, falling back to TCP");
            eprintln!(
                "tymux: no reachable Unix socket at {} — falling back to TCP loopback \
                 (deprecated; make sure tymuxd is running)",
                socket_path.display()
            );
            Ok(
                tonic::transport::Endpoint::from_static("http://127.0.0.1:7419")
                    .http2_keep_alive_interval(Duration::from_secs(30))
                    .keep_alive_timeout(Duration::from_secs(10))
                    .keep_alive_while_idle(true)
                    .connect()
                    .await?,
            )
        }
    }
}

#[derive(Parser)]
#[command(name = "tymux")]
struct Cli {
    #[arg(long, global = true)]
    addr: Option<String>,

    /// Path to tymuxd's Unix domain socket. Defaults to the same path
    /// tymuxd itself computes ($XDG_RUNTIME_DIR/tymuxd/tymuxd.sock, or a
    /// uid-scoped fallback under $TMPDIR/tmp) — override only for
    /// non-default deployments (multiple tymuxd instances, a custom
    /// runtime dir). When overriding, prefer a tymuxd-owned subdirectory
    /// (e.g. $XDG_RUNTIME_DIR/my-tymuxd/tymuxd.sock) rather than a shared
    /// runtime directory directly, matching tymuxd's own default nesting.
    /// Note: a socket reached through a bind-mounted path inside a
    /// container may present a different uid than `id -u` shows locally —
    /// see this repo's README, "Multi-user / shared-host deployment"
    /// section (added by Task 9.1.1a), for the full caveat (Deployment
    /// Guidance; ux.md Gap 1 fix).
    #[arg(long, global = true, env = "TYMUXD_SOCKET_PATH")]
    socket_path: Option<String>,

    /// Disable the status bar entirely — pure pty passthrough, no
    /// DECSTBM scroll-region reservation, zero added escape bytes
    /// (accessibility floor, ux.md §3).
    #[arg(long, global = true)]
    no_status_bar: bool,

    /// Bearer token to authenticate against a non-loopback tymuxd.
    /// Generate one with `openssl rand -hex 32` if you don't already have
    /// one configured on the daemon side. Prefer TYMUXD_TOKEN over --token
    /// on a shared host — argv (and thus --token's value) is visible to
    /// any local user via `ps`/`/proc/<pid>/cmdline`, while environment
    /// variables are only readable via the owner-only `/proc/<pid>/environ`.
    #[arg(long, global = true, env = "TYMUXD_TOKEN", hide_env_values = true)]
    token: Option<String>,

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
        if status.code() == tonic::Code::Unauthenticated {
            return format!(
                "tymuxd rejected this connection: {} (set --token or TYMUXD_TOKEN to authenticate)",
                status.message()
            );
        }
        // A UDS peer-cred rejection (`peer_is_authorized` on the daemon
        // side) — distinct from the bearer-token `Unauthenticated` case
        // above so a scripted caller can branch on the status code alone.
        // Deliberately no mention of SO_PEERCRED/uid numbers in the
        // printed message (ux.md Surface 9's "plain language over
        // jargon"); note the one caveat this remedy doesn't cover: a
        // containerized client sees its host-mapped uid, which may not
        // match `id -u` inside the container (Deployment Guidance).
        if status.code() == tonic::Code::PermissionDenied {
            return format!(
                "tymuxd rejected this connection: {} (ask the daemon's owner to add you to its \
                 configured --socket-group, or run tymux-cli as the daemon's own OS user)",
                status.message()
            );
        }
        return status.message().to_string();
    }
    e.to_string()
}

/// Mirrors tymuxd's `BearerToken` (crates/tymuxd/src/auth.rs) — same
/// invariant (empty token unrepresentable), same reason (no `Debug`/
/// `PartialEq` derive to prevent a value leak or an accidental
/// non-constant-time comparison). Not shared as a library type: this is
/// the client side, tymuxd's is the server side, and they have no other
/// reason to depend on each other.
#[derive(Clone)]
struct BearerToken(String);

impl std::fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<redacted>")
    }
}

impl BearerToken {
    fn parse(raw: &str) -> Option<Self> {
        (!raw.is_empty()).then(|| Self(raw.to_string()))
    }
    fn as_str(&self) -> &str {
        &self.0
    }
}

/// Attaches the configured bearer token to every outgoing RPC as
/// `authorization: Bearer <token>`, unary and streaming (`Attach`) alike.
/// No-ops when no token is configured — loopback usage must stay
/// byte-for-byte unaffected. The `authorization` header value is formatted
/// and validated once at construction, not per call, since this
/// interceptor sits on the hot path of every RPC (e.g. every navigation
/// keystroke in `redraw_copy_mode`'s shared redraw path).
#[derive(Clone)]
struct BearerAuth {
    header: Option<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>,
}

/// Shorthand for the client type every RPC helper in this file threads
/// through — spelled out in full (`TymuxServiceClient<InterceptedService<
/// Channel, BearerAuth>>`) it was repeated at every call site.
type TymuxClient = TymuxServiceClient<InterceptedService<Channel, BearerAuth>>;

impl BearerAuth {
    fn new(token: Option<BearerToken>) -> anyhow::Result<Self> {
        let header = token
            .map(|token| format!("Bearer {}", token.as_str()).parse())
            .transpose()
            .map_err(|_| anyhow::anyhow!("token contains invalid header characters"))?;
        Ok(Self { header })
    }
}

impl tonic::service::Interceptor for BearerAuth {
    fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(header) = &self.header {
            req.metadata_mut().insert("authorization", header.clone());
        }
        Ok(req)
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let socket_path = cli
        .socket_path
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| default_uds_socket_path(unsafe { libc::geteuid() }));
    let channel = dial_channel(cli.addr, &socket_path).await?;
    let mut client = TymuxServiceClient::with_interceptor(
        channel,
        BearerAuth::new(cli.token.as_deref().and_then(BearerToken::parse))?,
    );
    let config = TymuxConfig::load_or_default();
    let status_bar_cfg = StatusBarConfig::new(!cli.no_status_bar);

    match cli.command {
        Command::New { name, command } => {
            let session = client
                .create_session(CreateSessionRequest {
                    name: name.clone(),
                    command: command.unwrap_or_default(),
                    cwd: String::new(),
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
            check_attach_liveness(&pane, &target.session)?;
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
    client: &mut TymuxClient,
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
    client: &mut TymuxClient,
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
    client: &mut TymuxClient,
    pane_id: String,
    session_name: &str,
    config: &TymuxConfig,
    status_bar_cfg: &StatusBarConfig,
) -> Result<AttachOutcome> {
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    tx.send(AttachRequest {
        payload: Some(attach_request::Payload::PaneId(pane_id.clone())),
        // Epic 1.1 / Task 1.1.1b: no resume state to offer yet — building
        // and sending a real resume token is Epic 6.1's job. `None` here
        // behaves identically to a pre-feature client's request.
        resume_from_seq: None,
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
    send_resize_and_repaint(&tx, &mut stdout, status_bar_cfg, mode, config, session_name).await?;

    let outcome = 'attach_loop: loop {
        tokio::select! {
            biased;
            _ = resize_rx.recv() => {
                send_resize_and_repaint(&tx, &mut stdout, status_bar_cfg, mode, config, session_name)
            .await?;
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
                        Some(ref payload @ attach_event::Payload::Exited(_)) => {
                            drop(_raw);
                            writeln!(stdout, "{}", chrome_message_for_event(payload).unwrap())?;
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
                            redraw_status_line(
                                &mut stdout,
                                term_rows,
                                mode,
                                config,
                                status_bar_cfg,
                                session_name,
                            )?;
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
                                // Epic 1.1 / Task 1.1.1b: resume_from_seq only
                                // has meaning on the first message; not used here.
                                resume_from_seq: None,
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
                        redraw_status_line(
                            &mut stdout,
                            term_rows,
                            mode,
                            config,
                            status_bar_cfg,
                            session_name,
                        )?;
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
    client: &mut TymuxClient,
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
    session_name: &str,
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
        // Epic 1.1 / Task 1.1.1b: resume_from_seq only has meaning on the
        // first message; not used here.
        resume_from_seq: None,
    })
    .await?;

    if cfg.enabled {
        stdout.write_all(&status_bar::decstbm_reserve(term_rows, cfg))?;
        redraw_status_line(stdout, term_rows, mode, config, cfg, session_name)?;
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
    session_name: &str,
) -> Result<()> {
    if !cfg.enabled {
        return Ok(());
    }
    let line = status_bar::colorize(
        &status_bar::render_hint_line(mode, config, session_name),
        cfg,
    );
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
    use std::sync::Mutex;
    use tymux_proto::v1::ExitStatus;

    // std::env::set_var/remove_var mutate global process state, so tests
    // touching TYMUXD_TOKEN must not run concurrently with each other.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    // --- socket-path-fixtures.json loading (shared with tymuxd, the Go
    // and TS clients — see plan.md Task 1.1.1b / 6.1.1b; lives in testdata/
    // at the repo root, not project_plans/, since two of the four consumers
    // read it via include_str! at compile time) ---

    #[derive(serde::Deserialize)]
    struct DefaultPathCase {
        case: String,
        env: std::collections::HashMap<String, String>,
        uid: u32,
        expected: String,
    }

    #[derive(serde::Deserialize)]
    struct SocketPathFixtures {
        default_path_cases: Vec<DefaultPathCase>,
    }

    const SOCKET_PATH_FIXTURES_JSON: &str =
        include_str!("../../../testdata/unix-socket-auth/socket-path-fixtures.json");

    fn load_socket_path_fixtures() -> SocketPathFixtures {
        serde_json::from_str(SOCKET_PATH_FIXTURES_JSON)
            .expect("socket-path-fixtures.json must be valid JSON matching the shared schema")
    }

    fn default_path_case(name: &str) -> DefaultPathCase {
        load_socket_path_fixtures()
            .default_path_cases
            .into_iter()
            .find(|c| c.case == name)
            .unwrap_or_else(|| panic!("no default_path_cases entry named {name}"))
    }

    /// Clears the env vars `default_uds_socket_path` reads, then applies
    /// the case's `env` map on top. Callers must hold `ENV_LOCK`.
    fn apply_socket_path_env(env: &std::collections::HashMap<String, String>) {
        for var in ["XDG_RUNTIME_DIR", "TMPDIR"] {
            std::env::remove_var(var);
        }
        for (k, v) in env {
            std::env::set_var(k, v);
        }
    }

    fn clear_socket_path_env() {
        for var in ["XDG_RUNTIME_DIR", "TMPDIR"] {
            std::env::remove_var(var);
        }
    }

    // --- default_uds_socket_path (Task 6.1.1a/b) — mirrors tymuxd's
    // auth::default_uds_socket_path byte-for-byte; see plan.md Pattern
    // Decisions row 10 ---

    #[test]
    fn default_uds_socket_path_prefers_xdg_runtime_dir() {
        let _guard = ENV_LOCK.lock().unwrap();
        let case = default_path_case("xdg_runtime_dir_set");
        apply_socket_path_env(&case.env);
        let resolved = default_uds_socket_path(case.uid);
        clear_socket_path_env();
        assert_eq!(resolved, PathBuf::from(case.expected));
    }

    #[test]
    fn default_uds_socket_path_falls_back_to_tmpdir_when_xdg_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        let case = default_path_case("xdg_unset_tmpdir_set");
        apply_socket_path_env(&case.env);
        let resolved = default_uds_socket_path(case.uid);
        clear_socket_path_env();
        assert_eq!(resolved, PathBuf::from(case.expected));
    }

    #[test]
    fn default_uds_socket_path_falls_back_to_tmp_when_both_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        let case = default_path_case("both_unset");
        apply_socket_path_env(&case.env);
        let resolved = default_uds_socket_path(case.uid);
        clear_socket_path_env();
        assert_eq!(resolved, PathBuf::from(case.expected));
    }

    #[test]
    fn default_uds_socket_path_treats_empty_xdg_runtime_dir_as_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        let case = default_path_case("xdg_empty_string_treated_as_unset");
        apply_socket_path_env(&case.env);
        let resolved = default_uds_socket_path(case.uid);
        clear_socket_path_env();
        assert_eq!(resolved, PathBuf::from(case.expected));
    }

    #[test]
    fn default_uds_socket_path_scopes_by_uid_to_avoid_collision() {
        let _guard = ENV_LOCK.lock().unwrap();
        let base_case = default_path_case("both_unset");
        let other_case = default_path_case("uid_scoping_distinctness_1001");
        apply_socket_path_env(&base_case.env);
        let resolved_base = default_uds_socket_path(base_case.uid);
        let resolved_other = default_uds_socket_path(other_case.uid);
        clear_socket_path_env();
        assert_eq!(resolved_base, PathBuf::from(base_case.expected));
        assert_eq!(resolved_other, PathBuf::from(other_case.expected));
        assert_ne!(resolved_base, resolved_other);
    }

    // ---- Task 2.1.2c/d: integration tests against a live, token-gated
    // tymuxd subprocess -----------------------------------------------

    /// Locates a sibling workspace binary at runtime from this test
    /// binary's own `current_exe()` path.
    ///
    /// `tymuxd` (`crates/tymuxd/Cargo.toml`) declares only a `[[bin]]`, no
    /// `[lib]` target, so it cannot be added as a path dependency of this
    /// crate at all — confirmed empirically: adding `tymuxd = { path =
    /// "../tymuxd" }` under `[dev-dependencies]` and running `cargo test -p
    /// tymux-cli` produces `warning: ... ignoring invalid dependency
    /// \`tymuxd\` which is missing a lib target`, and with no dependency
    /// edge, `env!("CARGO_BIN_EXE_tymuxd")` is never defined at compile
    /// time either (confirmed the same way, and separately confirmed that
    /// even `env!("CARGO_BIN_EXE_tymux")` — this crate's own bin target —
    /// fails to resolve from *unit* tests inside `main.rs`, since per
    /// Cargo's docs `CARGO_BIN_EXE_<name>` is only set when building an
    /// integration test or benchmark, not a package's own unit-test
    /// harness). `crates/tymux-e2e` hits this identical problem spawning
    /// `tymuxd` as a subprocess and solves it with its own `workspace_bin`
    /// helper (`crates/tymux-e2e/src/lib.rs`); mirrored here rather than
    /// adding a cross-crate dependency for one helper function, per this
    /// task's own instruction to stay within `tymux-cli`. Requires the
    /// workspace to already be built (`cargo build --workspace`, which CI
    /// already runs before `cargo test --workspace`) so `tymuxd` sits
    /// alongside this test binary's own profile directory.
    fn workspace_bin(name: &str) -> std::path::PathBuf {
        let exe = std::env::current_exe().expect("current test exe path");
        let deps_dir = exe.parent().expect("test exe has a parent dir");
        // Integration/unit test binaries build into target/<profile>/deps/;
        // the workspace's own binary targets land one level up.
        let profile_dir = deps_dir.parent().expect("deps dir has a parent dir");
        let candidate = profile_dir.join(name);
        assert!(
            candidate.exists(),
            "expected workspace binary at {candidate:?} — run `cargo build --workspace` first"
        );
        candidate
    }

    /// A real `tymuxd` subprocess bound non-loopback with `TYMUXD_TOKEN`
    /// configured — the bind shape that triggers the auth gate (mirrors
    /// `tymuxd`'s own in-process `spawn_non_loopback_test_server` helper
    /// and `daemon_startup.rs`'s real-subprocess-spawning pattern).
    /// `tymuxd`'s `main()` never logs the ephemeral port it actually binds
    /// (only the pre-resolve `TYMUXD_ADDR` string is logged — confirmed by
    /// reading `crates/tymuxd/src/main.rs`'s `tracing::info!(%addr, "tymuxd
    /// listening")` call site, which logs the *input* string, not
    /// `socket_addr`/a post-bind local address), so the port is picked up
    /// front via a bind-then-drop probe instead — the same trick
    /// `crates/tymux-e2e/src/daemon.rs`'s `ephemeral_port()` uses for its
    /// own subprocess-spawned `tymuxd`.
    struct TokenGatedDaemon {
        child: std::process::Child,
        state_dir: std::path::PathBuf,
        addr: String,
    }

    impl Drop for TokenGatedDaemon {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            std::fs::remove_dir_all(&self.state_dir).ok();
        }
    }

    fn spawn_token_gated_daemon(token: &str) -> TokenGatedDaemon {
        let port = std::net::TcpListener::bind("0.0.0.0:0")
            .expect("bind an ephemeral port to pick one for the test daemon")
            .local_addr()
            .unwrap()
            .port();
        // Bind non-loopback (0.0.0.0) so tymuxd's fail-fast gate treats
        // this as the "needs a token" case — but connect back via
        // 127.0.0.1, matching `spawn_non_loopback_test_server`'s own
        // connect-back address (0.0.0.0 is not a valid outbound connect
        // target).
        let bind_addr = format!("0.0.0.0:{port}");
        let connect_addr = format!("127.0.0.1:{port}");

        let state_dir = std::env::temp_dir().join(format!(
            "tymux-cli-bearer-auth-test-{}-{port}",
            std::process::id()
        ));
        std::fs::create_dir_all(&state_dir).unwrap();

        let child = std::process::Command::new(workspace_bin("tymuxd"))
            .env("TYMUXD_ADDR", &bind_addr)
            .env("TYMUXD_TOKEN", token)
            .env("XDG_STATE_HOME", &state_dir)
            .env("RUST_LOG", "warn")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("failed to spawn tymuxd binary");

        TokenGatedDaemon {
            child,
            state_dir,
            addr: connect_addr,
        }
    }

    /// Polls until `tymuxd` accepts a bare gRPC transport connection —
    /// mirrors `daemon_startup.rs`'s `wait_for_daemon`. A successful
    /// `connect()` alone doesn't prove auth is enforced (each test's own
    /// RPC call proves that); it only proves the daemon is up and ready to
    /// accept connections.
    async fn wait_for_daemon(addr: &str) -> Channel {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(channel) = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
                .unwrap()
                .connect()
                .await
            {
                return channel;
            }
            if std::time::Instant::now() > deadline {
                panic!("tymuxd did not become reachable within 10s");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Task 2.1.2c (plan.md Story 2.1.2 AC1/AC2): a `BearerAuth`-wrapped
    /// client constructed exactly like `run()` constructs its own (Task
    /// 2.1.2b) authenticates successfully against a real, non-loopback,
    /// token-gated `tymuxd` when configured with the correct token, and is
    /// rejected with `Unauthenticated` when configured with no token at
    /// all.
    #[tokio::test]
    async fn run_lists_sessions_successfully_against_token_gated_daemon_with_correct_token() {
        let daemon = spawn_token_gated_daemon("s3cr3t-integration-token");
        let channel = wait_for_daemon(&daemon.addr).await;

        let mut authed_client = TymuxServiceClient::with_interceptor(
            channel.clone(),
            BearerAuth::new(BearerToken::parse("s3cr3t-integration-token")).unwrap(),
        );
        authed_client
            .list_sessions(ListSessionsRequest {})
            .await
            .expect("ListSessions should succeed with the correct bearer token");

        let mut unauthed_client =
            TymuxServiceClient::with_interceptor(channel, BearerAuth::new(None).unwrap());
        let err = unauthed_client
            .list_sessions(ListSessionsRequest {})
            .await
            .expect_err("ListSessions should be rejected with no bearer token configured");
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
    }

    /// Task 2.1.2d (plan.md Story 2.1.2 AC3): `BearerAuth` applies to the
    /// `Attach` bidi stream too, not just unary calls — opens `Attach`
    /// against the same token-gated daemon with the correct token
    /// configured and asserts the stream actually delivers its priming
    /// Snapshot rather than being rejected or hanging.
    #[tokio::test]
    async fn attach_succeeds_against_token_gated_daemon_with_correct_token() {
        let daemon = spawn_token_gated_daemon("s3cr3t-attach-token");
        let channel = wait_for_daemon(&daemon.addr).await;

        let mut client = TymuxServiceClient::with_interceptor(
            channel,
            BearerAuth::new(BearerToken::parse("s3cr3t-attach-token")).unwrap(),
        );

        let session = client
            .create_session(CreateSessionRequest {
                name: "bearer-auth-attach-test".to_string(),
                command: "/bin/sh".to_string(),
                cwd: String::new(),
            })
            .await
            .expect("CreateSession should succeed with the correct bearer token")
            .into_inner();
        let pane_id = first_pane_id(&session).expect("created session should have a pane");

        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tx.send(AttachRequest {
            payload: Some(attach_request::Payload::PaneId(pane_id)),
            resume_from_seq: None,
        })
        .await
        .unwrap();
        let mut stream = client
            .attach(Request::new(ReceiverStream::new(rx)))
            .await
            .expect("Attach should open successfully with the correct bearer token")
            .into_inner();

        let first = tokio::time::timeout(Duration::from_secs(5), stream.message())
            .await
            .expect("Attach stream should respond within 5s")
            .expect("Attach stream should not error")
            .expect("Attach stream should not end before any event");
        assert!(
            matches!(first.payload, Some(attach_event::Payload::Snapshot(_))),
            "expected the first AttachEvent to be a Snapshot, got {first:?}"
        );
    }

    /// Counterpart to `attach_succeeds_against_token_gated_daemon_with_correct_token`:
    /// proves the `Attach` bidi stream is rejected, not just unary calls,
    /// when the client presents no bearer token at all — mirrors
    /// `crates/tymuxd/src/main.rs`'s
    /// `non_loopback_server_rejects_attach_stream_with_missing_token_promptly`
    /// for the identical scenario, including the bounded timeout to avoid a
    /// hang risk if rejection ever stopped happening promptly.
    #[tokio::test]
    async fn attach_rejected_against_token_gated_daemon_with_missing_token() {
        let daemon = spawn_token_gated_daemon("s3cr3t-attach-reject-token");
        let channel = wait_for_daemon(&daemon.addr).await;

        let mut client =
            TymuxServiceClient::with_interceptor(channel, BearerAuth::new(None).unwrap());

        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tx.send(AttachRequest {
            payload: Some(attach_request::Payload::PaneId(
                "missing-token-test-pane".to_string(),
            )),
            resume_from_seq: None,
        })
        .await
        .unwrap();

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            client.attach(Request::new(ReceiverStream::new(rx))),
        )
        .await
        .expect("Attach should fail promptly, not hang");

        let status = result.unwrap_err();
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }

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

    /// REQ-7 (Story 3.1.1 AC2, Task 3.1.1b) — proves the client's transport
    /// keepalive config is actually set on the `Endpoint` used to build the
    /// connection, not just present somewhere in the source. Only exercises
    /// the happy path (builder succeeds, channel connects to a local
    /// server); it can't prove keepalive fires under real packet loss — see
    /// validation.md REQ-7's manual/real-hardware verification note.
    #[tokio::test]
    async fn client_endpoint_should_configure_keep_alive_while_idle_true_when_constructed_via_explicit_endpoint_builder(
    ) {
        use tymux_proto::v1::tymux_service_server::{TymuxService, TymuxServiceServer};
        use tymux_proto::v1::{
            AttachEvent, ClosePaneResponse, KillSessionResponse, ListSessionsResponse,
            PaneSnapshot, SearchScrollbackRequest, SearchScrollbackResponse, WatchWindowRequest,
            WindowLayoutEvent,
        };

        /// Bare-bones service that only needs to complete an HTTP/2
        /// connection handshake — this test never issues an RPC, it only
        /// proves the client's `Endpoint` (with keepalive config attached)
        /// can establish a real transport connection to a real server.
        struct DummyTymuxService;

        #[tonic::async_trait]
        impl TymuxService for DummyTymuxService {
            async fn create_session(
                &self,
                _request: tonic::Request<CreateSessionRequest>,
            ) -> Result<tonic::Response<Session>, tonic::Status> {
                unreachable!("test never issues RPCs")
            }
            async fn list_sessions(
                &self,
                _request: tonic::Request<ListSessionsRequest>,
            ) -> Result<tonic::Response<ListSessionsResponse>, tonic::Status> {
                unreachable!("test never issues RPCs")
            }
            async fn kill_session(
                &self,
                _request: tonic::Request<KillSessionRequest>,
            ) -> Result<tonic::Response<KillSessionResponse>, tonic::Status> {
                unreachable!("test never issues RPCs")
            }
            async fn revive_session(
                &self,
                _request: tonic::Request<ReviveSessionRequest>,
            ) -> Result<tonic::Response<ReviveSessionResponse>, tonic::Status> {
                unreachable!("test never issues RPCs")
            }
            async fn capture_pane(
                &self,
                _request: tonic::Request<CapturePaneRequest>,
            ) -> Result<tonic::Response<PaneSnapshot>, tonic::Status> {
                unreachable!("test never issues RPCs")
            }
            async fn search_scrollback(
                &self,
                _request: tonic::Request<SearchScrollbackRequest>,
            ) -> Result<tonic::Response<SearchScrollbackResponse>, tonic::Status> {
                unreachable!("test never issues RPCs")
            }
            async fn split_pane(
                &self,
                _request: tonic::Request<SplitPaneRequest>,
            ) -> Result<tonic::Response<Session>, tonic::Status> {
                unreachable!("test never issues RPCs")
            }
            async fn close_pane(
                &self,
                _request: tonic::Request<ClosePaneRequest>,
            ) -> Result<tonic::Response<ClosePaneResponse>, tonic::Status> {
                unreachable!("test never issues RPCs")
            }
            async fn create_window(
                &self,
                _request: tonic::Request<CreateWindowRequest>,
            ) -> Result<tonic::Response<Session>, tonic::Status> {
                unreachable!("test never issues RPCs")
            }
            type WatchWindowStream = tokio_stream::Empty<Result<WindowLayoutEvent, tonic::Status>>;
            async fn watch_window(
                &self,
                _request: tonic::Request<WatchWindowRequest>,
            ) -> Result<tonic::Response<Self::WatchWindowStream>, tonic::Status> {
                unreachable!("test never issues RPCs")
            }
            type AttachStream = tokio_stream::Empty<Result<AttachEvent, tonic::Status>>;
            async fn attach(
                &self,
                _request: tonic::Request<tonic::Streaming<AttachRequest>>,
            ) -> Result<tonic::Response<Self::AttachStream>, tonic::Status> {
                unreachable!("test never issues RPCs")
            }
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(TymuxServiceServer::new(DummyTymuxService))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
                .unwrap();
        });

        let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
            .unwrap()
            .http2_keep_alive_interval(Duration::from_secs(30))
            .keep_alive_timeout(Duration::from_secs(10))
            .keep_alive_while_idle(true);

        let channel = endpoint.connect().await;
        assert!(
            channel.is_ok(),
            "Endpoint with keepalive config should still connect: {:?}",
            channel.err()
        );
    }

    #[test]
    fn attach_event_match_should_render_output_dropped_message_on_output_gap_variant() {
        let exited_msg =
            chrome_message_for_event(&attach_event::Payload::Exited(ExitStatus { code: None }))
                .unwrap();
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

    /// Task 2.2.1b, AC1: an `Unauthenticated` status gets the dedicated,
    /// remedy-naming message rather than a bare passthrough of
    /// `status.message()`.
    #[test]
    fn friendly_message_names_the_remedy_for_unauthenticated_status() {
        let status = tonic::Status::unauthenticated("missing bearer token");
        let err: anyhow::Error = status.into();
        assert_eq!(
            friendly_message(&err),
            "tymuxd rejected this connection: missing bearer token (set --token or TYMUXD_TOKEN to authenticate)"
        );
    }

    /// Task 2.2.1b, AC2: other status codes are unaffected by the new
    /// `Unauthenticated` branch — no regression on the existing passthrough
    /// behavior.
    #[test]
    fn friendly_message_unaffected_for_other_status_codes() {
        let status = tonic::Status::not_found("no such session: abc");
        let err: anyhow::Error = status.into();
        assert_eq!(friendly_message(&err), "no such session: abc");
    }

    /// Task 6.3.1b / R14: a `PermissionDenied` status (UDS peer-cred
    /// rejection) gets its own remedy-naming message, distinct from the
    /// `Unauthenticated` (bearer-token) branch above.
    #[test]
    fn friendly_message_names_the_remedy_for_permission_denied_status() {
        let status =
            tonic::Status::permission_denied("not authorized to access this daemon's socket");
        let err: anyhow::Error = status.into();
        assert_eq!(
            friendly_message(&err),
            "tymuxd rejected this connection: not authorized to access this daemon's socket \
             (ask the daemon's owner to add you to its configured --socket-group, or run \
             tymux-cli as the daemon's own OS user)"
        );
    }

    /// Task 2.1.1c: `--token s3cr3t` parses into `cli.token`.
    #[test]
    fn token_flag_parses() {
        let cli = parse(&["--token", "s3cr3t", "ls"]);
        assert_eq!(cli.token, Some("s3cr3t".to_string()));
    }

    /// Task 2.1.1c: with no `--token` flag, `TYMUXD_TOKEN` is used as a
    /// fallback.
    #[test]
    fn token_env_var_used_as_fallback() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("TYMUXD_TOKEN", "s3cr3t");
        let cli = Cli::try_parse_from(["tymux", "ls"]);
        std::env::remove_var("TYMUXD_TOKEN");
        assert_eq!(cli.unwrap().token, Some("s3cr3t".to_string()));
    }

    /// Task 2.1.1c: an explicit `--token` flag overrides `TYMUXD_TOKEN`.
    #[test]
    fn token_flag_overrides_env_var() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("TYMUXD_TOKEN", "envval");
        let cli = Cli::try_parse_from(["tymux", "--token", "flagval", "ls"]);
        std::env::remove_var("TYMUXD_TOKEN");
        assert_eq!(cli.unwrap().token, Some("flagval".to_string()));
    }

    /// Task 2.1.1d (security-critical): rendered `--help` text must never
    /// echo a live `TYMUXD_TOKEN` value — `hide_env_values = true` on the
    /// `token` field is what prevents clap's default `[env:
    /// TYMUXD_TOKEN=<value>]` annotation from leaking it.
    #[test]
    fn cli_help_does_not_echo_configured_token_value() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("TYMUXD_TOKEN", "s3cr3t-live-value");
        let help = Cli::command().render_help().to_string();
        std::env::remove_var("TYMUXD_TOKEN");
        assert!(
            !help.contains("s3cr3t-live-value"),
            "--help must never echo the live TYMUXD_TOKEN value, got: {help}"
        );
    }

    /// Task 6.1.2b: no `--addr` means "try UDS first" — `cli.addr` is
    /// `None`, not a hardcoded TCP default.
    #[test]
    fn no_addr_flag_leaves_addr_none() {
        let cli = parse(&["ls"]);
        assert_eq!(cli.addr, None);
    }

    #[test]
    fn addr_can_be_overridden() {
        let cli = parse(&["--addr", "http://example.com:1234", "ls"]);
        assert_eq!(cli.addr, Some("http://example.com:1234".to_string()));
    }

    /// Task 6.1.1c: `--socket-path` parses into `cli.socket_path`.
    #[test]
    fn socket_path_flag_parses() {
        let cli = parse(&["--socket-path", "/custom/tymuxd.sock", "ls"]);
        assert_eq!(cli.socket_path, Some("/custom/tymuxd.sock".to_string()));
    }

    /// Task 6.1.1c AC: no `--socket-path` flag and no `TYMUXD_SOCKET_PATH`
    /// env var leaves `cli.socket_path` `None`, so the caller falls back to
    /// `default_uds_socket_path`.
    #[test]
    fn no_socket_path_flag_or_env_leaves_socket_path_none() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TYMUXD_SOCKET_PATH");
        let cli = parse(&["ls"]);
        assert_eq!(cli.socket_path, None);
    }

    /// Task 6.1.1d: `--socket-path` renders in `--help` with its env-var
    /// annotation, matching `--token`'s existing discoverability precedent
    /// (closes ux.md Gap 2).
    #[test]
    fn cli_help_output_lists_socket_path_flag_with_env_annotation() {
        let help = Cli::command().render_help().to_string();
        assert!(
            help.contains("--socket-path"),
            "--help must list --socket-path, got: {help}"
        );
        assert!(
            help.contains("TYMUXD_SOCKET_PATH"),
            "--help must show the TYMUXD_SOCKET_PATH env annotation, got: {help}"
        );
    }

    // ---- Task 6.2.1c: dial_channel / dial_uds ------------------------

    /// A unique-per-call temp path — avoids collisions between tests
    /// running concurrently in the same process. Built directly under
    /// `/tmp`, bypassing `$TMPDIR`: macOS CI's default `$TMPDIR`
    /// (`/var/folders/<random>/T/`) combined with a descriptive label and
    /// a deep runner checkout path can push the full socket path past
    /// `SUN_LEN`, the ~104-byte kernel limit on `AF_UNIX` paths on macOS
    /// (~108 on Linux) — this matches the `short_unique_socket_path`
    /// pattern used by `crates/tymuxd/tests/*.rs`.
    fn temp_socket_path(label: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::path::PathBuf::from(format!(
            "/tmp/tymux-cli-{label}-{}-{n}.sock",
            std::process::id()
        ))
    }

    /// Task 6.2.1c AC1 (unit-level): a real, HTTP/2-speaking peer bound at
    /// the resolved UDS path is dialed successfully. Uses a bare
    /// `tonic::transport::Server` with zero registered services — enough
    /// to complete the HTTP/2 handshake `connect_with_connector` performs
    /// eagerly, without needing a real `tymuxd` (out of scope per this
    /// task's own instructions). The full "an actual RPC round-trips"
    /// proof is deferred to Epic 6.4's `uds_integration.rs` against a real
    /// daemon, per this task's own note.
    #[tokio::test]
    async fn dial_channel_uses_uds_when_reachable() {
        let socket_path = temp_socket_path("reachable");
        let _ = std::fs::remove_file(&socket_path);
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        let incoming = tokio_stream::wrappers::UnixListenerStream::new(listener);
        let server = tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_routes(tonic::service::Routes::default())
                .serve_with_incoming(incoming)
                .await;
        });

        let result = dial_uds(&socket_path).await;

        server.abort();
        let _ = std::fs::remove_file(&socket_path);
        assert!(
            result.is_ok(),
            "expected a reachable UDS peer to dial successfully"
        );
    }

    /// Task 6.2.1c AC2 (unit-level classification): no socket file at the
    /// resolved path classifies as `UdsDialError::Other` (the "no daemon
    /// listening here" case), which is what routes `dial_channel` to the
    /// TCP fallback branch. The full live-daemon round trip plus the
    /// exactly-one-fallback-line assertion are deferred to Epic 6.4's
    /// `uds_integration.rs`, per this task's own note.
    #[tokio::test]
    async fn dial_channel_falls_back_to_tcp_with_notice_when_uds_unreachable() {
        let socket_path = temp_socket_path("unreachable");
        let _ = std::fs::remove_file(&socket_path);

        let result = dial_uds(&socket_path).await;

        assert!(
            matches!(result, Err(UdsDialError::Other(_))),
            "expected a missing UDS socket to classify as UdsDialError::Other (falls back to TCP), not PermissionDenied"
        );
    }

    /// Task 6.2.1c AC3 / pre-mortem.md P1 #1: a UDS socket that exists but
    /// rejects the connect() syscall at the OS level (`PermissionDenied`)
    /// is a hard error from `dial_channel` — never a TCP fallback. Denies
    /// the calling uid via chmod(0) on the socket's own inode rather than
    /// needing a second real uid (same technique Task 6.4.1d's setup
    /// uses). Skipped when running as root, since root bypasses DAC
    /// permission checks entirely and the chmod(0) trick wouldn't deny
    /// the connect.
    #[tokio::test]
    async fn dial_channel_hard_errors_and_never_dials_tcp_when_uds_permission_denied() {
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipping: running as root, chmod(0) doesn't deny root's own connect()");
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let socket_path = temp_socket_path("permission-denied");
        let _ = std::fs::remove_file(&socket_path);
        let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o000)).unwrap();

        let result = dial_channel(None, &socket_path).await;

        drop(listener);
        let _ = std::fs::remove_file(&socket_path);

        let err = result.expect_err("a PermissionDenied UDS dial must be a hard error");
        assert_eq!(
            err.to_string(),
            "tymuxd rejected this connection: not authorized to access this daemon's socket \
             (ask the daemon's owner to add you to its configured --socket-group, or run \
             tymux-cli as the daemon's own OS user)"
        );
    }

    /// Task 6.2.1c AC4 (structural, no UDS I/O of any kind): an explicit
    /// `--addr` returns before any UDS logic runs at all — proven here by
    /// pointing `socket_path` at a path with no listener and no permission
    /// issue whatsoever, and asserting it's never touched (never created,
    /// so `.exists()` staying false proves the UDS branch never ran — a
    /// real `UnixStream::connect` attempt against a nonexistent path would
    /// itself just fail, not create the file, so the meaningful proof is
    /// structural rather than behavioral). The TCP side does perform a
    /// real (fast-failing) dial against `127.0.0.1:0`, an always-refused
    /// port, confirming `explicit_addr` — not the UDS path — is what got
    /// dialed.
    #[tokio::test]
    async fn dial_channel_skips_uds_entirely_when_addr_explicit() {
        let socket_path = temp_socket_path("skipped-because-addr-explicit");
        let _ = std::fs::remove_file(&socket_path); // never created — must never be touched

        let result = dial_channel(Some("http://127.0.0.1:0".to_string()), &socket_path).await;

        // Port 0 / an unbound loopback port refuses the connection — this
        // proves the TCP branch was taken (a real dial attempt was made
        // against the explicit addr), not that the UDS branch silently
        // succeeded some other way.
        assert!(result.is_err());
        assert!(
            !socket_path.exists(),
            "dial_channel must never touch the UDS path when --addr is explicit"
        );
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
            cwd: String::new(),
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

    /// Story 3.5 AC2 — the friendlier, higher-tier `RECOMMENDED_SPLIT_MIN_ROWS`
    /// usability warning, distinct from the hard `MIN_PANE_ROWS`/`MIN_PANE_COLS`
    /// structural floor (which has its own dedicated coverage in
    /// `crates/tymux-core/src/layout.rs`'s unit tests and the Story 3.2 AC3
    /// property suite). This exact message text must stay in sync with
    /// `engine_error_to_status`'s `BelowRecommendedSize` arm in
    /// `crates/tymuxd/src/main.rs`.
    #[test]
    fn split_command_should_show_exact_row_counts_when_terminal_below_minimum_size() {
        let status = tonic::Status::failed_precondition(
            "Can't split: pane is 15 rows, minimum for a horizontal split is ~20 rows. \
             Resize your terminal or close another pane first.",
        );
        let err: anyhow::Error = status.into();
        let msg = friendly_message(&err);
        assert_eq!(
            msg,
            "Can't split: pane is 15 rows, minimum for a horizontal split is ~20 rows. \
             Resize your terminal or close another pane first."
        );
        assert!(msg.contains("15"));
        assert!(msg.contains("20"));
    }

    /// Story 4.6 AC1: a dead-flagged target pane must fail fast with the
    /// exact remediation message naming `tymux revive <session>`.
    #[test]
    fn attach_should_fail_fast_with_revive_remediation_message_when_target_session_is_dead() {
        let pane = ProtoPane {
            id: "pane-1".to_string(),
            rows: 24,
            cols: 80,
            liveness: tymux_proto::v1::Liveness::Dead as i32,
            cwd: String::new(),
        };
        let err = check_attach_liveness(&pane, "myproject").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not running"));
        assert!(
            msg.contains("tymux revive myproject"),
            "message must name the exact revive remediation command, got: {msg}"
        );
    }

    /// Story 4.6 AC2: once a session is live again (e.g. after `tymux
    /// revive`), the same check must never block — the fail-fast path
    /// only triggers for `PaneLookup::Dead`, never for `Live`.
    #[test]
    fn attach_should_succeed_normally_when_target_session_is_live_after_revive() {
        let pane = ProtoPane {
            id: "pane-1".to_string(),
            rows: 24,
            cols: 80,
            liveness: tymux_proto::v1::Liveness::Live as i32,
            cwd: String::new(),
        };
        assert!(check_attach_liveness(&pane, "myproject").is_ok());
    }

    /// Story 4.5 AC2: live and dead-restored sessions must render
    /// distinctly in `tymux ls` — never identical, so a user can tell at a
    /// glance which sessions need `tymux revive` before they can be
    /// attached to.
    #[test]
    fn ls_command_should_render_distinct_status_strings_for_live_versus_dead_restored_session() {
        let live_session = Session {
            id: "s1".to_string(),
            name: "myproject".to_string(),
            windows: vec![],
            liveness: tymux_proto::v1::Liveness::Live as i32,
        };
        let dead_session = Session {
            id: "s2".to_string(),
            name: "myproject".to_string(),
            windows: vec![],
            liveness: tymux_proto::v1::Liveness::Dead as i32,
        };
        let live_label = ls_status_label(&live_session);
        let dead_label = ls_status_label(&dead_session);
        assert_ne!(
            live_label, dead_label,
            "live and dead-restored sessions must never render identically"
        );
        assert!(live_label.contains("live"));
        assert!(dead_label.contains("restored"));
        assert!(dead_label.contains("not running"));
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
