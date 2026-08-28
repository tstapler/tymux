//! Bearer-token auth for a non-loopback-bound tymuxd: token resolution
//! (`resolve_token`), the fail-fast startup gate
//! (`check_non_loopback_requires_token`), and the gRPC request gate
//! (`BearerAuthInterceptor`). Extracted from `main.rs` during
//! architecture review to keep the god-file from absorbing another
//! concern (see plan.md's Pattern Decisions).

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use tonic::Status;

/// The one shared, operator-supplied bearer secret. `parse` is the
/// only constructor — an empty token is unrepresentable, closing the
/// gap where "empty string counts as absent" was previously enforced
/// by a single `.filter()` call a future second token source could
/// bypass (architecture-review.md, first Concern).
#[derive(Clone)]
pub struct BearerToken(String);

impl std::fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<redacted>")
    }
}

impl BearerToken {
    /// The ONLY way to produce a `BearerToken`. Deliberately no
    /// `PartialEq`/`Eq` derive on the type — a derived `==` would be a
    /// second, non-constant-time equality path sitting right next to
    /// the required `constant_time_eq` call (Story 1.2.1); see
    /// ADR-001 for why that risk is taken seriously here.
    pub fn parse(raw: &str) -> Option<Self> {
        (!raw.is_empty()).then(|| Self(raw.to_string()))
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

/// Resolves the configured bearer token for a non-loopback bind:
/// `--token <value>` or `--token=<value>` on argv, falling back to
/// `TYMUXD_TOKEN`. An explicit flag wins over the env var (ADR-002:
/// hand-rolled, no clap, but the same flag-beats-env precedence
/// tymux-cli gets from clap's `env=` attribute). An empty value from
/// either source is treated as absent, never as "auth disabled with
/// an empty secret" (research/pitfalls.md §5) — enforced by
/// `BearerToken::parse`, not a bare filter, so it can't be
/// accidentally bypassed if a third token source is ever added (see
/// Unresolved Questions' `TYMUXD_TOKEN_FILE` note).
///
/// Prefer TYMUXD_TOKEN over --token on a shared host — argv (and thus
/// --token's value) is visible to any local user via `ps`/
/// `/proc/<pid>/cmdline`, while environment variables are only
/// readable via the owner-only `/proc/<pid>/environ`.
///
/// Generate a token with `openssl rand -hex 32` if you don't already
/// have one to configure.
pub fn resolve_token(args: &[String]) -> Option<BearerToken> {
    let flag_value = args
        .iter()
        .position(|a| a == "--token")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .or_else(|| {
            args.iter()
                .find_map(|a| a.strip_prefix("--token=").map(|v| v.to_string()))
        });
    let env_value = std::env::var("TYMUXD_TOKEN").ok();
    flag_value
        .or(env_value)
        .and_then(|t| BearerToken::parse(&t))
}

/// The fail-fast invariant this feature exists to enforce: a
/// non-loopback bind must have a (non-empty, already-guaranteed by
/// `BearerToken::parse`) token. Extracted as a pure function so it's
/// testable without a real network bind.
pub fn check_non_loopback_requires_token(
    is_loopback: bool,
    token: Option<&BearerToken>,
) -> Result<(), String> {
    if !is_loopback && token.is_none() {
        return Err(
            "failed to start: bound to non-loopback address with no token configured.\n\
             Set --token or TYMUXD_TOKEN before binding tymuxd to a non-loopback address — \
             this port would otherwise let any network client run arbitrary commands.\n\
             (Loopback binds, e.g. 127.0.0.1, never require a token. Generate one with \
             `openssl rand -hex 32` if you don't already have one.)"
                .to_string(),
        );
    }
    Ok(())
}

/// Gates every `TymuxService` RPC behind the configured bearer token
/// when tymuxd is bound non-loopback. Owns its own rejection counter
/// rather than reaching into `TymuxDaemon`/`Engine` — auth is a pure
/// request-gate concern, never consulted by RPC handler bodies
/// (research/architecture.md §2).
#[derive(Clone)]
pub struct BearerAuthInterceptor {
    token: Arc<BearerToken>,
    rejection_count: Arc<AtomicI64>,
}

impl BearerAuthInterceptor {
    pub fn new(token: Arc<BearerToken>, rejection_count: Arc<AtomicI64>) -> Self {
        Self {
            token,
            rejection_count,
        }
    }
}

impl tonic::service::Interceptor for BearerAuthInterceptor {
    fn call(&mut self, req: tonic::Request<()>) -> Result<tonic::Request<()>, Status> {
        // `remote_addr()` itself is cheap (no allocation); only the
        // `.to_string()` in each rejection arm heap-allocates, so the
        // common accepted-call path doesn't pay for a peer string it
        // never uses.
        let remote_addr = req.remote_addr();

        let presented = req
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));

        match presented {
            None => {
                let peer = remote_addr
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                let count = self.rejection_count.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::warn!(
                    peer = %peer,
                    tymux_auth_rejection_total = count,
                    "rejected TymuxService call: missing bearer token"
                );
                Err(Status::unauthenticated("missing bearer token"))
            }
            Some(supplied)
                if constant_time_eq::constant_time_eq(
                    supplied.as_bytes(),
                    self.token.as_bytes(),
                ) =>
            {
                Ok(req)
            }
            Some(_) => {
                let peer = remote_addr
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                let count = self.rejection_count.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::warn!(
                    peer = %peer,
                    tymux_auth_rejection_total = count,
                    "rejected TymuxService call: invalid bearer token"
                );
                Err(Status::unauthenticated("invalid bearer token"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicI64;
    use std::sync::Mutex;
    use tonic::service::Interceptor;
    use tonic::transport::server::TcpConnectInfo;
    use tonic::Request;

    // std::env::set_var/remove_var mutate global process state, so tests
    // touching TYMUXD_TOKEN must not run concurrently with each other.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    // --- BearerToken ---

    #[test]
    fn bearer_token_parse_rejects_empty_string() {
        assert!(BearerToken::parse("").is_none());
    }

    #[test]
    fn bearer_token_parse_accepts_non_empty_string() {
        let token = BearerToken::parse("s3cr3t").unwrap();
        assert_eq!(token.as_bytes(), b"s3cr3t");
    }

    #[test]
    fn bearer_token_debug_always_prints_redacted() {
        let token = BearerToken::parse("s3cr3t").unwrap();
        let debug = format!("{token:?}");
        assert_eq!(debug, "<redacted>");
        assert!(!debug.contains("s3cr3t"));
    }

    // --- resolve_token ---

    #[test]
    fn resolve_token_prefers_explicit_flag_over_env_var() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("TYMUXD_TOKEN", "envval");
        let args: Vec<String> = vec!["tymuxd", "--token", "flagval"]
            .into_iter()
            .map(String::from)
            .collect();
        let resolved = resolve_token(&args);
        std::env::remove_var("TYMUXD_TOKEN");
        assert_eq!(resolved.unwrap().as_bytes(), b"flagval");
    }

    #[test]
    fn resolve_token_supports_equals_joined_flag_form() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TYMUXD_TOKEN");
        let args: Vec<String> = vec!["tymuxd", "--token=flagval"]
            .into_iter()
            .map(String::from)
            .collect();
        let resolved = resolve_token(&args);
        assert_eq!(resolved.unwrap().as_bytes(), b"flagval");
    }

    #[test]
    fn resolve_token_falls_back_to_env_var_when_no_flag() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("TYMUXD_TOKEN", "envval");
        let args: Vec<String> = vec!["tymuxd"].into_iter().map(String::from).collect();
        let resolved = resolve_token(&args);
        std::env::remove_var("TYMUXD_TOKEN");
        assert_eq!(resolved.unwrap().as_bytes(), b"envval");
    }

    #[test]
    fn resolve_token_treats_empty_flag_value_as_absent() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TYMUXD_TOKEN");
        let args: Vec<String> = vec!["tymuxd", "--token", ""]
            .into_iter()
            .map(String::from)
            .collect();
        let resolved = resolve_token(&args);
        assert!(resolved.is_none());
    }

    #[test]
    fn resolve_token_returns_none_when_neither_source_present() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TYMUXD_TOKEN");
        let args: Vec<String> = vec!["tymuxd"].into_iter().map(String::from).collect();
        let resolved = resolve_token(&args);
        assert!(resolved.is_none());
    }

    // --- check_non_loopback_requires_token ---

    #[test]
    fn check_non_loopback_requires_token_returns_ok_when_token_present_on_non_loopback_bind() {
        let token = BearerToken::parse("s3cr3t").unwrap();
        assert!(check_non_loopback_requires_token(false, Some(&token)).is_ok());
    }

    #[test]
    fn check_non_loopback_requires_token_returns_err_when_non_loopback_and_no_token() {
        let err = check_non_loopback_requires_token(false, None).unwrap_err();
        assert!(err.contains("--token"));
        assert!(err.contains("TYMUXD_TOKEN"));
    }

    #[test]
    fn check_non_loopback_requires_token_errs_on_empty_token_via_resolve_token_composition() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("TYMUXD_TOKEN");
        let args: Vec<String> = vec!["tymuxd", "--token", ""]
            .into_iter()
            .map(String::from)
            .collect();
        let resolved = resolve_token(&args);
        assert!(resolved.is_none());
        let err = check_non_loopback_requires_token(false, resolved.as_ref());
        assert!(err.is_err());
    }

    #[test]
    fn check_non_loopback_requires_token_returns_ok_when_loopback_and_no_token() {
        assert!(check_non_loopback_requires_token(true, None).is_ok());
    }

    // --- BearerAuthInterceptor ---

    fn metadata_request(auth_header: Option<&str>) -> Request<()> {
        let mut req = Request::new(());
        if let Some(value) = auth_header {
            req.metadata_mut()
                .insert("authorization", value.parse().unwrap());
        }
        req
    }

    #[test]
    fn bearer_auth_interceptor_accepts_matching_token() {
        let token = BearerToken::parse("s3cr3t").unwrap();
        let counter = Arc::new(AtomicI64::new(0));
        let mut interceptor = BearerAuthInterceptor::new(Arc::new(token), counter.clone());
        let req = metadata_request(Some("Bearer s3cr3t"));
        assert!(interceptor.call(req).is_ok());
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn bearer_auth_interceptor_rejects_missing_token() {
        let token = BearerToken::parse("s3cr3t").unwrap();
        let counter = Arc::new(AtomicI64::new(0));
        let mut interceptor = BearerAuthInterceptor::new(Arc::new(token), counter.clone());
        let req = metadata_request(None);
        let err = interceptor.call(req).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
        assert_eq!(err.message(), "missing bearer token");
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn bearer_auth_interceptor_rejects_wrong_token() {
        let token = BearerToken::parse("s3cr3t").unwrap();
        let counter = Arc::new(AtomicI64::new(0));
        let mut interceptor = BearerAuthInterceptor::new(Arc::new(token), counter.clone());
        let req = metadata_request(Some("Bearer wrongvalue"));
        let err = interceptor.call(req).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
        assert_eq!(err.message(), "invalid bearer token");
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn bearer_auth_interceptor_rejects_malformed_authorization_header() {
        let token = BearerToken::parse("s3cr3t").unwrap();
        let counter = Arc::new(AtomicI64::new(0));
        let mut interceptor = BearerAuthInterceptor::new(Arc::new(token), counter.clone());
        let req = metadata_request(Some("Bearer"));
        let err = interceptor.call(req).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
        assert_eq!(err.message(), "missing bearer token");
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn bearer_auth_interceptor_rejection_counter_counts_only_rejections() {
        let token = BearerToken::parse("s3cr3t").unwrap();
        let counter = Arc::new(AtomicI64::new(0));
        let mut interceptor = BearerAuthInterceptor::new(Arc::new(token), counter.clone());

        // 3 rejected, 2 accepted, interleaved.
        assert!(interceptor.call(metadata_request(None)).is_err());
        assert!(interceptor
            .call(metadata_request(Some("Bearer s3cr3t")))
            .is_ok());
        assert!(interceptor
            .call(metadata_request(Some("Bearer wrongvalue")))
            .is_err());
        assert!(interceptor
            .call(metadata_request(Some("Bearer s3cr3t")))
            .is_ok());
        assert!(interceptor.call(metadata_request(None)).is_err());

        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[test]
    #[tracing_test::traced_test]
    fn bearer_auth_interceptor_logs_real_peer_address_when_available() {
        let token = BearerToken::parse("s3cr3t").unwrap();
        let counter = Arc::new(AtomicI64::new(0));
        let mut interceptor = BearerAuthInterceptor::new(Arc::new(token), counter);

        let mut req = Request::new(());
        req.extensions_mut().insert(TcpConnectInfo {
            local_addr: None,
            remote_addr: Some("203.0.113.5:54321".parse().unwrap()),
        });

        let err = interceptor.call(req).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);

        assert!(logs_contain("203.0.113.5:54321"));
        assert!(!logs_contain("s3cr3t"));
    }
}
