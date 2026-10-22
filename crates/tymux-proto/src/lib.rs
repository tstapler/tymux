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

/// Optional behaviors a client can feature-detect via `ServerInfo.capabilities`.
pub const CAPABILITIES: &[&str] = &[
    "create_session.args_env",
    "create_session.stderr_redirect",
    "create_session.file_args",
    "uds_auth",
];
