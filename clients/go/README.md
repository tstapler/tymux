# tymux Go client

Module `github.com/tstapler/tymux/clients/go`. The generated connect-go client lives in
`gen/tymux/v1`; the packages below are the hand-written transport layer.

| Package | Purpose |
|---|---|
| `udsdialer` | Socket-path resolution and h2c `*http.Client`s for the Unix socket and TCP loopback |
| `authinterceptor` | Bearer-token interceptor (unary and streaming) |
| `capabilities` | Feature-detect optional daemon behavior from `GetServerInfo` |
| `selecting` | Unix-first client with TCP fallback, stream-safe failover and re-probe |
| `liveness` | Classify an endpoint as healthy / busy / access-denied / unhealthy / not-listening; gate kills on `Result.Alive()` |
| `paneerr` | Classify `PANE_EXITED` / `PANE_RESTORED_WITHOUT_SCREEN` errors |

## Selecting transport

```go
c := selecting.New(selecting.Options{
    Token:        os.Getenv("TYMUXD_TOKEN"),
    OnTransition: func(from, to selecting.Kind, why string) { log.Printf("tymuxd %s -> %s: %s", from, to, why) },
})
defer c.CloseIdle()
resp, err := c.ListSessions(ctx, connect.NewRequest(&tymuxv1.ListSessionsRequest{}))
```

- Unix socket first (`udsdialer.ResolveSocketPath`), TCP loopback only when the socket is
  absent or refusing (`ENOENT`/`ECONNREFUSED`) **and** the TCP listener answers `GetServerInfo`.
- `EACCES` on the socket is a hard error (`selecting.ErrSocketAccessDenied`); it never falls
  back to the unauthenticated TCP listener.
- A call is retried at most once, and only after a refused dial, so a half-sent RPC is never replayed.
- After a fallback the choice sticks; one caller's next RPC re-probes the socket every
  `ReprobeInterval` (default 30s). Probes honour the caller's `ctx` plus `ProbeTimeout`.
- `Attach` streams: a refused dial surfaces from `Send` as the real error (connect-go
  reports only `io.EOF` there). The selector learns of it, the failed stream is not retried,
  and the next `Attach` uses the fallback.
- The bearer token goes to the Unix transport only; `Options.TokenOnTCP` opts in to sending it
  over the plaintext TCP fallback too.
- The socket is vetted before each dial (unix only; a no-op elsewhere), and on Linux the connected
  peer's uid is checked after it. Anything failing is a hard `ErrSocketUntrusted` (no TCP fallback):
  - every symlink on the path must be owned by root or the caller, and the socket itself must not be one;
  - the socket and its resolved parent directory must have the same owner, who must be the caller,
    root, or listed in `Options.TrustedSocketOwnerUIDs` (**group-mode clients, where tymuxd runs as
    another user, must opt in**);
  - in a world-writable directory the directory must be sticky and the socket owned by the caller or
    a trusted uid, so a root-owned socket in `/tmp` is refused for non-root callers.
  The path check is best effort (it races the dial); macOS has no peer-credential check in the
  standard library, so there it is the only defence. The path algorithm
  (`udsdialer.DefaultSocketPath`) is shared with tymuxd, tymux-cli and the TS client and is unchanged.
- Supervisors probing the socket should use `selecting.CheckedDial` with `liveness.Options.Dial` and a
  Prober from `selecting.NewUnixTransport`, so the probe cannot be pointed at a planted socket.
- `Quiet: true` for one-shot probes (no `OnTransition`, Debug-only logging); `defer c.CloseIdle()`.
- `WatchWindow` and `WriteStdin` are not wrapped (no failover or EACCES annotation), so `*Client` is not a full `TymuxServiceClient`; use the generated client for those two. `Transport` is not for external implementation and may gain methods in minor releases.

## Consuming it

Pin a pseudo-version of the nested module, matching the `tymuxd` binary you ship:
`go get github.com/tstapler/tymux/clients/go@<commit-sha>`.
