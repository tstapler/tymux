# Research: Build vs. Buy (supporting pieces for the tymux/stapler-squad integration)

Scope: four concrete tooling/library decisions the integration needs, evaluated
as build-vs-buy, not a re-litigation of tymux itself (tymux exists, works, and
is not up for replacement). Overlaps partially with `research/stack.md`
(written by a parallel research pass) on the Go client and ANSI questions;
where it does, this doc cross-references rather than repeats, and resolves one
question `stack.md` left open (see §2).

## 1. Go gRPC/Connect client generation for tymux's proto

**Verdict: Recommended (buf + `connectrpc.com/connect`) — not a real decision.**

`stack.md` §1–3 already did the deep dive: stapler-squad's `go.mod` pins
`connectrpc.com/connect v1.19.0` directly (used by `server/server.go`), plus
`github.com/bufbuild/buf v1.57.2` as a direct dependency and
`google.golang.org/protobuf v1.36.11`. stapler-squad's own `buf.gen.yaml`
already generates Go protobuf + Connect stubs via the `buf.build/protocolbuffers/go`
and `buf.build/connectrpc/go` remote plugins — the exact toolchain a Go client
for tymux's proto would use. tymux's `proto/buf.gen.yaml` already generates a
TS client this way (`protoc-gen-es` → `clients/ts/gen`), with a header comment
anticipating "other language clients ... as they show up."

There is no alternative Go gRPC codegen toolchain worth evaluating here: raw
`google.golang.org/grpc` codegen (`protoc-gen-go-grpc`) is the only other
mainstream option, and it would introduce a second RPC client shape
(context-based unary + `grpc.ClientStream`) alongside the Connect-based one
stapler-squad's own server already uses, for zero benefit — Connect's
generated bidi-stream client (`*connect.BidiStreamForClient[Req, Resp]`,
`Send`/`Receive`/`CloseRequest`/`CloseResponse`) maps directly onto `Attach`,
and `google.golang.org/grpc` is present in stapler-squad's `go.mod` only as an
*indirect* dependency pulled in via OTel — not something the codebase writes
RPC code against directly today.

| | |
|---|---|
| **Pros** | Zero new dependency (already in `go.mod`); mirrors the proven `clients/ts/` pattern in the same repo; one `buf generate` invocation can emit both TS and Go from `proto/buf.gen.yaml`; h2c/plaintext transport works out of the box for a local daemon. |
| **Cons** | tymux has no root `go.mod` today — publishing the generated Go client as an importable module (vs. vendoring it into stapler-squad's `gen/proto/go` tree) is new repo infrastructure, not a version bump (open question, not a blocker — see `stack.md` §3). |
| **Verdict** | **Recommended.** Use buf + `connectrpc.com/connect`, extending the existing `proto/buf.gen.yaml`. |

## 2. ANSI/terminal rendering translation layer

**Verdict: split by RPC — no library needed for the live path (`Attach`); a
small hand-rolled serializer for the snapshot path (`CapturePane`).**

`stack.md` §4 confirmed stapler-squad's *current* tmux live-output path
(`session/tmux/control_mode.go` → `decodeControlModeOutput`) is raw-byte
pass-through to xterm.js — no cell-grid intermediate representation exists in
production today — but left open "whether tymux's `Attach` RPC carries raw
terminal bytes or a structured/diffed format." Reading
`proto/tymux/v1/tymux.proto` directly resolves that:

```protobuf
message AttachEvent {
  oneof payload {
    bytes output = 1; // raw pty output bytes, as they arrive
    PaneSnapshot snapshot = 2;
    bool exited = 3;
    bool output_gap = 4;
  }
}
```

`Attach`'s `output` field is raw PTY bytes, byte-for-byte the same shape
stapler-squad's control-mode path already forwards to the browser — **the
live-streaming path needs no rendering adapter at all**, just a swap of framing
(proto message unwrapping instead of control-mode `%output` line parsing).
This directly de-risks the "Cell-grid → xterm.js" rabbit hole named in
requirements.md for the hot path.

The structured side is `CapturePaneRequest` → `PaneSnapshot { rows, cols,
repeated Row grid, cursor_row, cursor_col }`, where `Row` is `repeated Cell`
and `Cell { text, fg, bg, attrs }` carries color/attribute state as plain
integers rather than embedded ANSI codes (`proto/tymux/v1/tymux.proto:239-248`).
This RPC is what a reattach-with-history or scrollback-search flow would use
for an initial snapshot, analogous to how stapler-squad already massages
`tmux capture-pane -p -e -J` output before seeding a fresh xterm.js instance
(`server/services/connectrpc_websocket.go`, `withCursorSync` /
`normalizeCapturePane`-style helpers per its comments).

No existing Go library does *this specific direction* — structured
cell-attribute grid → ANSI/SGR byte stream. The well-known Go terminal
libraries all point the other way (ANSI text → cell grid) or own the whole
terminal loop:
- `github.com/hinshun/vt10x` / `gdamore/tcell` — vt100 emulation and
  cell-based terminal control, both built to *parse* ANSI or *drive a live
  tty*, not to serialize an externally-supplied `{text, fg, bg, attrs}` grid
  into a byte buffer for transport.
- `github.com/Azure/go-ansiterm` — already in stapler-squad's `go.sum`, but
  only as an indirect dependency of the vendored Docker client libraries; it's
  an ANSI *parser* (the opposite direction) and not imported by any
  stapler-squad code (`grep -rln "go-ansiterm" --include=*.go` → zero hits).
- `github.com/charmbracelet/x/ansi` — a maintained low-level SGR/escape-code
  *builder* (part of the Charm ecosystem, used by bubbletea/lipgloss). This is
  the closest fit as a primitive to build on: it handles correct SGR sequence
  construction (256-color/truecolor codes, attribute bits) so the integration
  doesn't hand-roll escape-code correctness, but the cell-walk/diff logic
  (tracking prior cell's fg/bg/attrs, emitting a reset+SGR only when they
  change, per-row cursor positioning) is integration-specific and has to be
  written regardless.

This is a bounded, well-specified problem — converting a fixed-shape 2D array
into SGR-prefixed text is meaningfully simpler than the reverse (parsing
arbitrary ANSI streams, which is where the real complexity of terminal
emulation lives, per `server/terminal/escape_scan.go`'s own scanner). Pulling
in a full terminal-emulation library to solve it would be over-buying.

| | |
|---|---|
| **Pros (buy `charmbracelet/x/ansi` for SGR primitives)** | Correct, tested escape-code construction; actively maintained; already the kind of dependency the Go terminal-tooling ecosystem converges on (bubbletea/lipgloss). |
| **Cons** | Doesn't solve the actual problem (cell-diff walk, cursor placement) — still custom code on top; one more direct dependency for a narrow use. |
| **Pros (build outright, no new dep)** | No new dependency; the serialization logic is small (row/cell loop + SGR diff) and easier to unit-test in isolation than to adapt a general-purpose library's API to. |
| **Verdict** | **Viable, lean toward build.** `Attach` (the hot path) needs nothing — pure pass-through. `CapturePane` (snapshot path) is small enough to hand-write; `charmbracelet/x/ansi` is worth reaching for only if the hand-rolled SGR encoding turns out fiddlier than expected (256-color/truecolor edge cases) — start without it. |

## 3. Bidirectional-stream reconnect/resume libraries

**Verdict: Build. Custom reconnect logic in `BackendTymux`, no off-the-shelf fit.**

gRPC's own ecosystem confirms this is not a solved-by-library problem for
bidirectional streams specifically. grpc-go's built-in retry (service-config
retry policy) explicitly does not cover streaming RPCs once they've started —
see [grpc/grpc-go#8328](https://github.com/grpc/grpc-go/issues/8328) ("How to
use retries for connections/rpc failures with a bidirectional stream") and
[grpc/grpc-go#3946](https://github.com/grpc/grpc-go/issues/3946)
("Bidirectional streaming RPC Retry policy"), both open issues where the
maintainer guidance is that streams must manage their own
reconnect/backoff — retry policies only retry the *initial* RPC attempt before
any message is sent. Connect (`connectrpc.com/connect`) doesn't add anything
on top of this for bidi streams either; it generates the same
`Send`/`Receive` shape and leaves reconnect to the caller.

More importantly, no generic reconnect wrapper could satisfy the actual
requirement here without integration-specific logic anyway: requirements.md
calls for distinguishing "client-initiated detach" from "connection dropped,
pane should keep running" — that's an application-level semantic (did
`BackendTymux` choose to close the stream, or did it die mid-flight?), not
something a transport-level retry/backoff library has visibility into. A
generic library would retry both cases identically or need the same
detach-vs-drop signal threaded through it that custom code would need anyway,
which erases most of the library's value.

| | |
|---|---|
| **Pros (buy a generic retry lib)** | Handles exponential backoff/jitter boilerplate. |
| **Cons** | No mainstream Go library specifically targets bidi-stream reconnect (confirmed via grpc-go's own issue tracker — it's treated as inherently app-specific); still requires custom code to inject the detach-vs-drop signal; adds a dependency for a thin slice of the actual problem. |
| **Pros (build)** | Full control over the detach-vs-drop distinction, which is the entire point; backoff/jitter itself is a handful of lines (or `golang.org/x/time/rate`, already in stapler-squad's `go.mod`, for pacing retries — not a new dependency). |
| **Verdict** | **Recommended: build.** Custom reconnect logic in `BackendTymux`, using stapler-squad's existing `golang.org/x/time` for backoff pacing if wanted — no new dependency needed. |

## 4. PTY/disconnect-survival fix approach — prior art worth studying

**Verdict: study prior art as reference, but tymux still has to find and fix
its own bug — no crate/library "buys" this away.**

`portable-pty` (the crate tymux already uses,
`crates/tymux-core/src/pane.rs:6`) is a cross-platform PTY *abstraction*
only — `openpty`/`spawn_command`/`kill()` — it does not itself provide
session persistence or detach/reattach semantics. That layer is built by
callers on top of it. Two concrete, relevant prior-art examples:

- **`wezterm-mux-server-impl`** (part of the WezTerm monorepo, same author/
  ecosystem as `portable-pty` itself) is exactly this: a headless daemon built
  on `portable-pty` that keeps PTYs alive independent of client connections,
  with client attach/detach handled in `sessionhandler.rs`. Because it's
  built on the identical crate tymux already depends on, it's the most
  directly applicable reference for "how does a `portable-pty`-based daemon
  keep a child alive when the attached client goes away" — worth reading its
  session/detach handling before re-deriving the fix from first principles.
- **Zellij** (a Rust terminal multiplexer, not built on `portable-pty` — its
  own `zellij-utils` crate wraps PTY handling directly) uses an explicit
  server-owns-everything model: session states include `Active` (clients
  attached), `ActiveDetached` (server alive, no clients, PTYs kept running),
  and `Killed`. The invariant is that a client socket closing is never itself
  a kill signal — only an explicit command tears down a pane. This confirms
  the *architectural* pattern tymux is already presumably aiming for (daemon
  owns the PTY master; client disconnect ≠ process kill) is well-established
  and correct in the abstract.

Neither is a library tymux can adopt directly — WezTerm's mux server isn't
packaged as a reusable crate, and Zellij's PTY layer is bespoke to Zellij and
not `portable-pty`-based, so there's no `Cargo.toml` line that "buys" this
fix. What both confirm is that the target architecture is right and this is
implementation-bug territory, not a design gap — which matches where tymux's
own investigation already landed:
[docs/reviews/](../../../docs) and the `disconnect_survival_e2e.rs` history
(commit `ab88c81`, "docs: record deeper findings on the abrupt-disconnect
pane-kill bug") show every code-level cause has been ruled out already (no
`kill()` call site fires — `crates/tymux-core/src/pane.rs:256`,
`crates/tymux-core/src/engine.rs:399,561,568` — the CLI exits with status 0
not a signal, not fd/device aliasing, not timing/input-dependent), with the
investigation itself producing ambiguous strace output attributed to a
possible ptrace/PID-reuse artifact specific to the sandboxed dev container.

| | |
|---|---|
| **Pros (study wezterm-mux-server-impl / Zellij)** | Free, concrete reference implementations of the exact "daemon owns PTY, client-gone ≠ kill" pattern, from the same crate family (`portable-pty`) and a comparable Rust multiplexer; cheap to read before the next debugging pass. |
| **Cons** | Neither is adoptable as a dependency; doesn't shortcut the actual bug-finding work, which is a container/environment-specific mechanism that source-reading elsewhere won't reveal — the next attempt needs a real terminal/machine (per the commit's own conclusion), not more comparative reading. |
| **Verdict** | **Viable as reference, not a substitute for the fix.** Read `wezterm-mux-server-impl`'s `sessionhandler.rs` for how a `portable-pty`-based daemon structures detach; then re-attempt the repro on real hardware per the existing investigation's own recommendation, rather than continuing to look for an adoptable crate. |

## Summary table

| # | Decision | Verdict | New dependency? |
|---|---|---|---|
| 1 | Go client codegen | **Recommended** — buf + `connectrpc.com/connect` | No (already in `go.mod`) |
| 2 | ANSI/rendering layer | **Viable, lean build** — no adapter for `Attach`; small hand-rolled `Cell`→SGR serializer for `CapturePane` | No (optionally `charmbracelet/x/ansi` if hand-rolled SGR proves fiddly) |
| 3 | Stream reconnect/resume | **Recommended: build** — custom logic in `BackendTymux` | No |
| 4 | Disconnect-survival fix | **Study prior art, then build the fix** — `wezterm-mux-server-impl` and Zellij as reference, no adoptable crate | No |

None of the four introduces a new runtime dependency into either repo. The
pattern across all four is the same: stapler-squad and tymux already sit on
proven, idiomatic toolchains for each piece (Connect/buf, raw-byte PTY
streaming, `portable-pty`), and the "buy" question in each case resolves to
either "already bought" (#1) or "nothing on the market actually does the
integration-specific part" (#2–#4).
