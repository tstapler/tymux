# ADR-003: Cross-language client proof scoped to Node.js — browser `Attach` support explicitly deferred

## Status
Accepted

## Context
`requirements.md` names both "a non-Rust client... proving the
cross-language claim" and "future web frontends (e.g. stapler-squad)
embedding tymux sessions" as in-scope consumers/success metrics. Phase 2
research (`stack.md` §6, `architecture.md` §3, `build-vs-buy.md` §6) found a
hard, load-bearing technical constraint that makes these two asks partially
in tension for the `Attach` RPC specifically:

`Attach` is a true bidirectional-streaming RPC
(`proto/tymux/v1/tymux.proto`'s `rpc Attach(stream AttachRequest) returns
(stream AttachEvent)`). Bidirectional streaming requires end-to-end HTTP/2.
Browser `fetch`/XHR cannot send a streaming request body today — the
`duplex: 'full'` fetch option that would allow this is experimental,
behind flags in some Chromium builds only, and unsupported in stable
Chrome, Safari, and Firefox as of this research. This is true regardless of
which client library/codegen toolchain is chosen (Connect-Web, gRPC-Web,
hand-written) — it's a browser platform limitation, not a library gap.
`@connectrpc/connect-node` over real gRPC (HTTP/2, via `tonic`) has no such
limitation and supports full bidirectional streaming today, with no caveats,
for a Node.js process.

## Decision

The v1.0 "at least one working non-Rust client" success metric is satisfied
by a **Node.js TypeScript client** (`clients/ts/`), generated via
`@bufbuild/protoc-gen-es` (Connect-ES 2.0) + `@connectrpc/connect-node`
targeting the gRPC transport, demonstrated end-to-end against a running
`tymuxd` for `CreateSession`, `Attach` (input, output, and a clean detach),
and `CapturePane`. This is **explicitly sufficient** to satisfy
`requirements.md`'s stated bar.

**Browser support for `Attach` is explicitly out of scope for v1.0.** This
must be stated in `clients/ts/README.md` and the main project `README.md`,
not left implicit: a browser-based TS client (e.g. a future stapler-squad
web frontend) can use `CreateSession`, `ListSessions`, and `CapturePane`
(all unary, work fine over gRPC-Web/Connect from a browser) but **cannot**
use `Attach` directly from browser JavaScript. Closing this gap, if ever
needed, requires either (a) a Node-side proxy/gateway process the browser
talks to over WebSockets, or (b) a protocol-level change (e.g. a WebSocket-
based alternative to `Attach` specifically) — both are explicitly deferred,
not designed here.

The epic is sequenced to validate the toolchain in two stages, isolating
two distinct discovery risks (`architecture.md` §3, `pitfalls.md` §6):
first a trivial **unary** RPC (`ListSessions`) end-to-end against a live
daemon, proving the `buf generate` + Connect-ES toolchain setup itself
works; only then `Attach`'s bidi stream, so a failure there is legible as
"bidi-streaming has rough edges" rather than conflated with "buf/connect
setup is broken."

## Consequences
- `proto/buf.gen.yaml`'s currently-empty `plugins: []` gets its first real
  entry (`protoc-gen-es`, `out: ../clients/ts/gen`), targeting
  `clients/ts/gen/`.
- The `Attach` RPC's proto doc comment must be strengthened to state, at
  the RPC level (not buried in a field comment): the first `AttachRequest`
  must set `pane_id`; detach means full call cancellation, not half-close
  (`architecture.md` §3 point 2); and the `output_gap` signal's meaning
  (Epic 2). Connect-ES surfaces RPC-level doc comments most prominently to
  TS client authors, so this is where the contract needs to live.
- `tonic-web` (which would add real gRPC-Web serving to `tymuxd`) is **not**
  added in v1.0 — it would not by itself unlock browser `Attach` anyway
  (the browser-body-streaming limitation is independent of server-side
  support), so it provides no v1.0 value without also building a
  proxy/gateway, which is out of scope.
- README's cross-language claim must be phrased to match this scope exactly
  ("a Node.js TypeScript client... browser support for live attach sessions
  is a known, documented limitation, not yet solved") rather than a blanket
  "cross-language" claim that overpromises browser support.

## Alternatives considered
- **Add `tonic-web` and claim full cross-language + browser support**:
  rejected — would not actually deliver working browser `Attach` (the
  fetch/XHR limitation is unrelated to server-side gRPC-Web support), so it
  adds a dependency and false confidence without closing the real gap.
- **Defer the whole TS client epic post-1.0**: rejected — `requirements.md`
  names it as a hard v1.0 success metric ("proving the cross-language claim
  with working code, not just documentation"); Node-only is a legitimate,
  achievable scope cut, not a reason to cut the whole epic.
- **Pick a different first non-Rust language** (Python, Go): rejected per
  `requirements.md`'s own framing — TypeScript is the most likely
  stapler-squad integration path, and no research finding surfaced a reason
  to prefer a different language for the first proof.
