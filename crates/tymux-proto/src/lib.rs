pub mod v1 {
    #![allow(clippy::result_large_err, clippy::double_must_use)]
    tonic::include_proto!("tymux.v1");
}

/// Wire-protocol revision this build speaks. Bump on any change a client may
/// need to branch on (see docs/adr/0003-protocol-versioning.md); additive
/// changes older clients can ignore do not bump it.
pub const PROTOCOL_VERSION: u32 = 1;

/// Oldest client protocol revision this build still serves correctly.
pub const MIN_CLIENT_PROTOCOL_VERSION: u32 = 1;

/// `ErrorInfo.domain` on errors carrying a machine-readable reason.
pub const ERROR_DOMAIN: &str = "tymux.v1";
/// `FAILED_PRECONDITION` reason: the pane's child exited; its last screen is still readable.
pub const REASON_PANE_EXITED: &str = "PANE_EXITED";
/// `FAILED_PRECONDITION` reason: the pane was restored from disk and has no screen to read.
pub const REASON_PANE_RESTORED_WITHOUT_SCREEN: &str = "PANE_RESTORED_WITHOUT_SCREEN";

/// Optional behaviors a client can feature-detect via `ServerInfo.capabilities`.
pub const CAPABILITIES: &[&str] = &[
    "create_session.args_env",
    "create_session.stderr_redirect",
    "create_session.file_args",
    "create_session.stdin_file",
    "create_session.stdin_stream",
    "uds_auth",
    "pane.lifecycle",
    "server_info.fd_usage",
    "pane.activity_hint",
];
