# Stack Research: unix-socket-auth

## Pinned versions (verified from this repo)

| Component | Pin | Source |
|---|---|---|
| `tokio` | **1.52.3** (`features = ["full"]`) | `Cargo.lock` (`name = "tokio"`); `Cargo.toml` workspace dep `tokio = { version = "1", features = ["full"] }` |
| `tonic` | **0.12.3** | `Cargo.lock`; workspace dep `tonic = "0.12"` |
| `tokio-stream` | `0.1` (`features = ["net"]`) | `Cargo.toml` workspace deps — already has the `net` feature needed for `UnixListenerStream` |
| `libc` | `0.2` | `Cargo.toml` workspace deps (available if a raw-syscall fallback is ever needed; not needed for this project's scope — see below) |
| `connectrpc.com/connect` (Go) | **v1.20.0** | `clients/go/go.mod` |
| `golang.org/x/net` (Go, for `http2`) | v0.58.0 | `clients/go/go.mod` |
| `@connectrpc/connect-node` (TS) | `^2.0.2` | `clients/ts/package.json` |
| `@connectrpc/connect` (TS) | `^2.0.2` | `clients/ts/package.json` |

## 1. `tokio::net::UnixStream::peer_cred()` — Linux-only, or cross-platform?

**Finding: the requirements doc's stated risk is out of date for the pinned tokio version. `peer_cred()` is NOT Linux-only in tokio 1.52.3 — it has a macOS implementation too.**

VERIFIED by reading the pinned version's actual source at tag `tokio-1.52.3`:
- `https://raw.githubusercontent.com/tokio-rs/tokio/tokio-1.52.3/tokio/src/net/unix/stream.rs` — `UnixStream::peer_cred()` has **no `#[cfg(target_os = ...)]` gate on the method itself**:
  ```rust
  pub fn peer_cred(&self) -> io::Result<UCred> {
      ucred::get_peer_cred(self)
  }
  ```
  It's available on every platform where `UnixStream` compiles at all (gated only by the crate's own `cfg_net_unix!`, i.e. any Unix target with the `net` feature).
- `https://raw.githubusercontent.com/tokio-rs/tokio/tokio-1.52.3/tokio/src/net/unix/ucred.rs` — the platform dispatch lives in `ucred::get_peer_cred`, implemented per-OS:
  - `impl_linux` (`getsockopt(SO_PEERCRED)`): Linux, Android, Redox, OpenBSD, Haiku, Cygwin — has pid.
  - `impl_macos` (`getpeereid`/`LOCAL_PEEREPID`): **macOS, iOS, tvOS, watchOS, visionOS** — has pid.
  - `impl_bsd`: DragonFly, FreeBSD — no pid.
  - `impl_netbsd`: NetBSD, QNX — has pid.
  - `impl_solaris`, `impl_aix`, `impl_noproc` (stub) for the rest.
  - `UCred` exposes `.uid()` (`unix::uid_t`), `.gid()` (`unix::gid_t`), and `.pid()` (`Option<unix::pid_t>`, platform-dependent).

**Implication for the requirements doc**: the single biggest Feasibility Risk ("macOS `peer_cred()` support ... if tokio doesn't support it there, either a `cfg`-gated raw `getsockopt(LOCAL_PEERCRED)` call is needed ... or macOS gets a documented reduced-security fallback") is resolved — tokio already wraps macOS's `getpeereid`/`LOCAL_PEEREPID` internally, transparently, behind the same `peer_cred()` call. No FFI/unsafe surface, no new dependency, no macOS-specific fallback needed at the tokio layer. This should be flagged back to planning as a risk downgrade, not treated as still-open.

## 2. tonic 0.12.3 + `UnixListener` — API and dual-listener pattern

**tonic serving over UDS is a first-class, documented pattern** — tonic ships its own example at `hyperium/tonic` `examples/src/uds/server.rs` (https://github.com/hyperium/tonic/blob/master/examples/src/uds/server.rs):
```rust
let path = "/tmp/tonic/helloworld";
std::fs::create_dir_all(Path::new(path).parent().unwrap())?;
let uds = UnixListener::bind(path)?;
let uds_stream = UnixListenerStream::new(uds);
Server::builder()
    .add_service(GreeterServer::new(greeter))
    .serve_with_incoming(uds_stream)
    .await?;
```
`UnixListenerStream` comes from `tokio-stream`'s `net` feature — already a pinned workspace dependency (`tokio-stream = { version = "0.1", features = ["net"] }`), and this repo's own test helpers already use the TCP sibling of this exact API: `crates/tymuxd/src/main.rs` test helpers (`spawn_test_server`, `spawn_non_loopback_test_server`) already call `Server::builder().add_service(...).serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))` — so `UnixListenerStream` is a drop-in structural sibling of a pattern already proven in this codebase.

**Serving TWO listeners (TCP + UDS) concurrently**: tonic has no built-in "merge two heterogeneous listener types into one incoming stream" feature (confirmed no evidence of this in tonic's docs/examples/`Server`/`Router` API surface, and the open community issue asking for exactly this — `hyperium/tonic#1080`, "one grpc service for both unix domain socket and tcp at the same time" — has no maintainer-endorsed built-in solution). The requirements doc's own guess is correct and is the way to do it: **run two independently-spawned `Server::builder()` instances**, each calling `.add_service(...)` with a `Clone`-able service (tonic's generated `*Server<T>` wrapper is `Clone` when `T: Clone`, or wrap the daemon in `Arc`), one via `.serve_with_shutdown(tcp_addr, ...)` (as `main.rs` does today) and the other via `.serve_with_incoming(UnixListenerStream::new(uds), ...)`, driven concurrently with `tokio::try_join!` or two `tokio::spawn`s under one shutdown signal. This is additive to the existing `main.rs:1311-1329` server-construction block, not a rewrite of it.

**Peer-cred timing vs. tonic's handshake**: `serve_with_incoming` accepts a `Stream<Item = Result<IO, E>>` of raw `AsyncRead + AsyncWrite (+ Connected)` values — the raw `UnixStream` is still directly available before tonic hands it into the HTTP/2 layer, since `UnixListenerStream::new(uds).next()` yields the raw `tokio::net::UnixStream` per item. Peer-cred can be checked (and the connection dropped, or wrapped/passed through) by mapping the stream (e.g. `.filter_map` or a custom `Stream` adapter calling `.peer_cred()` per accepted `UnixStream` before it's yielded downstream) — this resolves the rabbit hole "if tonic's incoming-stream abstraction hides the raw stream, extracting peer creds may need a wrapper type": it does not hide it: the raw `UnixStream` is exactly what `UnixListenerStream` yields, `peer_cred()` is a plain inherent method on it, callable synchronously at accept time before tonic ever sees the connection.

## 3. Socket file mode (0600) and gid — stdlib only, no new crate

Confirmed via `doc.rust-lang.org` (stable docs):
- **Mode bits**: `std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?` using `std::os::unix::fs::PermissionsExt` (`from_mode`/`set_mode` trait methods) — stdlib, no new crate. Note: `Permissions::from_mode` builds the value; `set_mode` on an existing `Permissions` mutates it in place — either then needs `fs::set_permissions` to actually apply to disk (setting the mode field alone doesn't touch the filesystem).
- **Group ownership**: `std::os::unix::fs::chown(path, uid: Option<u32>, gid: Option<u32>) -> io::Result<()>` — **stable since Rust 1.73.0**. `None` for either arg leaves that field unchanged, so `chown(sock_path, None, Some(configured_gid))` is the exact shape needed for "chgrp to a configured gid without touching owner." No `nix`/`users` crate needed — matches the same "stdlib is enough, no new dependency" posture the requirements doc already established for `peer_cred()`.
- `libc = "0.2"` is already a pinned workspace dependency (used elsewhere in the repo) and remains available as a fallback if a raw `fchmod`/`fchown` syscall on an already-open fd is ever preferred over the path-based stdlib calls (e.g. to avoid a TOCTOU window between `bind()` and `set_permissions()`/`chown()` — worth flagging to the planning phase as a hardening detail: `UnixListener::bind` creates the file with the process's current umask first, so there's a brief window before the mode/chown calls land; the standard mitigation is setting the process umask to `0o177` right before `bind()`, or binding into a directory that's already `0700`).

## 4. Go and TS: dialing HTTP/2-over-UDS

### Go — `connectrpc.com/connect` v1.20.0 + `golang.org/x/net/http2` v0.58.0

**This repo already has the exact scaffolding needed** — `clients/go/examples/list-sessions/main.go` and `clients/go/examples/attach/main.go` (https://github.com/tstapler/tymux — see `clients/go/examples/list-sessions/main.go:30-41`) construct the client with a custom `http2.Transport`:
```go
httpClient := &http.Client{
    Transport: &http2.Transport{
        AllowHTTP: true,
        DialTLSContext: func(ctx context.Context, network, addr string, _ *tls.Config) (net.Conn, error) {
            return net.Dial(network, addr)
        },
    },
}
return tymuxv1connect.NewTymuxServiceClient(httpClient, baseURL, connect.WithGRPC(), ...)
```
The UDS variant is a minimal, well-established modification of this exact pattern (same shape confirmed independently across multiple Go UDS-over-HTTP writeups, e.g. the `net/http` custom-`DialContext` idiom): replace the dial with one that ignores the `network`/`addr` the transport passes and always dials the fixed Unix socket path:
```go
DialTLSContext: func(ctx context.Context, _, _ string, _ *tls.Config) (net.Conn, error) {
    return (&net.Dialer{}).DialContext(ctx, "unix", socketPath)
},
```
`connect.NewClient`/`NewTymuxServiceClient` itself is transport-agnostic (`connect.HTTPClient` is just `{ Do(*http.Request) (*http.Response, error) }`), so no `connectrpc.com/connect`-level API changes are needed — the UDS switch is entirely in the `http.Client`/`http2.Transport` construction, identical in shape to the h2c work `bearer-token-auth` already did once.

### TypeScript — `@connectrpc/connect-node` v2.0.2

**No first-class UDS support is documented or shipped** — `connectrpc/connect-es` issue #756 ("Unix domain socket", opened 2023-08-14) is **still open with no linked PR** as of this research, confirming there's no built-in `unix://` scheme or `socketPath` shorthand on `createGrpcTransport`.

The workaround is real and mechanically sound, confirmed by reading the pinned major version's actual transport source (`connectrpc/connect-es`, package `connect-node`, `src/http2-session-manager.ts`): the session manager calls `http2.connect(authority, http2SessionOptions)` where `http2SessionOptions` is passed straight through from the transport's `nodeOptions` option. Node's `http2.connect(authority, options, listener)` accepts an `options.createConnection(authority, option)` callback (standard Node.js `http2` API) that can return an already-connected `net.Socket` from `net.connect({ path: socketPath })`, bypassing normal host/port dialing entirely while `authority` stays a syntactically valid placeholder URL (e.g. `"http://localhost"`) for the `:authority` pseudo-header. Concretely:
```ts
import { createGrpcTransport } from "@connectrpc/connect-node";
import * as net from "node:net";

const transport = createGrpcTransport({
  baseUrl: "http://localhost", // placeholder authority; actual socket comes from createConnection
  nodeOptions: {
    createConnection: () => net.connect({ path: "/run/user/1000/tymuxd.sock" }),
  },
});
```
This is the same technique documented generically for Node's `http2` module (createConnection callback returning a pre-connected socket) and is consistent with what `nodeOptions` is designed to forward. **Flag to planning**: this is real but unverified end-to-end against tonic's h2c server specifically — should be spike-tested early in implementation, matching the requirements doc's own framing of this as "custom transport/dialer wiring...real, but bounded."

## 5. Default UDS socket path — `$XDG_RUNTIME_DIR` reliability and platform conventions

- **Linux**: `$XDG_RUNTIME_DIR` is set by `pam_systemd` on any systemd-managed login session (typically `/run/user/<uid>`, mode `0700`, tmpfs-backed, cleared on logout) — this is the de facto standard on any modern systemd Linux distro (confirmed via Arch Wiki / systemd community docs). `ssh-agent` under a systemd user unit uses `%t/ssh-agent.socket` (`%t` = `$XDG_RUNTIME_DIR`) as its canonical convention. Not guaranteed on non-systemd init systems (musl/Alpine, some containers, non-systemd distros) — needs a fallback.
- **macOS**: no systemd equivalent, `$XDG_RUNTIME_DIR` is not set by the OS. Common daemon conventions on macOS:
  - Docker Desktop: fixed `/var/run/docker.sock` (root-owned system location, not per-user).
  - Colima (macOS Docker alternative): `$HOME/.colima/<profile>/docker.sock` — a per-user, `$HOME`-rooted path, not `/var/run` or `/tmp`.
  - General per-user runtime-dir convention on macOS in the absence of XDG: `$TMPDIR` (macOS sets this per-session to a private, user-owned, auto-cleaned directory, e.g. `/var/folders/.../T/`) is the closest functional analog to Linux's `$XDG_RUNTIME_DIR` and is what several cross-platform daemons fall back to.
- **Recommended default-path decision for planning**: Linux → `$XDG_RUNTIME_DIR/tymuxd/tymuxd.sock` (nested one level under a `tymuxd`-owned subdirectory, not directly in the shared runtime dir — see plan.md's `default_uds_socket_path`) when set, else a documented fallback (e.g. `$TMPDIR`/`/tmp` with a per-uid subpath, or `~/.local/state/tymux/`); macOS → `$TMPDIR/tymuxd.sock` or a `~/Library/Application Support/tymux/`-style path, since there's no systemd-equivalent runtime dir. This mirrors the requirements doc's own Rabbit Hole framing exactly and should be finalized by the planning phase — this research does not pick a single final path, it confirms the two platforms genuinely need different defaults and names concrete precedent for each.

## Summary of dependency additions needed

**None.** Every piece — `tokio::net::UnixStream::peer_cred()` (incl. macOS), `tonic::transport::Server::serve_with_incoming` over a `UnixListener`, `tokio_stream::wrappers::UnixListenerStream`, `std::os::unix::fs::{PermissionsExt, chown}`, Go's `net.Dialer` + `golang.org/x/net/http2.Transport`, and TS's `http2.connect` `createConnection` override — is covered by dependencies already pinned in this repo (`tokio` full, `tonic`, `tokio-stream[net]`, Go stdlib `net`/`golang.org/x/net/http2`, Node's built-in `http2`/`net` modules). This matches the requirements doc's own "no new crate needed" framing for `peer_cred()` and extends the same conclusion to the rest of the stack.
