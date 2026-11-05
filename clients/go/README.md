# tymux Go client

Module `github.com/tstapler/tymux/clients/go`. The generated connect-go client lives in
`gen/tymux/v1`; the packages below are the hand-written transport layer.

| Package | Purpose |
|---|---|
| `udsdialer` | Socket-path resolution and h2c `*http.Client`s for the Unix socket and TCP loopback |
| `authinterceptor` | Bearer-token interceptor (unary and streaming) |
| `capabilities` | Feature-detect optional daemon behavior from `GetServerInfo` |
| `selecting` | Unix-first client with TCP fallback, stream-safe failover and re-probe |
| `liveness` | Classify an endpoint as healthy / busy / unhealthy / not-listening |
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
- `Quiet: true` for one-shot probes (no `OnTransition`, Debug-only logging); `defer c.CloseIdle()`.
- `WatchWindow` and `WriteStdin` are not wrapped; use the generated client for those.

## Consuming it

Pin a pseudo-version of the nested module, matching the `tymuxd` binary you ship:
`go get github.com/tstapler/tymux/clients/go@<commit-sha>`.
