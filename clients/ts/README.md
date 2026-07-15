# tymux TypeScript client

Proves tymux's cross-language claim (ADR-003): a Node.js client, generated
straight from `proto/tymux/v1/tymux.proto` via `buf`, driving a real
`tymuxd` daemon for `CreateSession`, `Attach` (bidirectional streaming
input/output), and `CapturePane` — no Rust code involved.

## Scope: Node.js only, no browser `Attach`

This client targets **Node.js** (`@connectrpc/connect-node`) over real
gRPC (HTTP/2). `Attach` is a true bidirectional-streaming RPC, and browser
`fetch`/XHR cannot send a streaming request body in any stable browser
today — that's a browser platform limitation, not a library gap (see
[ADR-003](../../project_plans/v1-release/decisions/ADR-003-typescript-client-node-only-scope.md)).

A browser client can still use `CreateSession`, `ListSessions`, and
`CapturePane` (all unary RPCs, which work fine over gRPC-Web/Connect from a
browser) — it just cannot open a live `Attach` session directly. This is a
known, documented v1.0 limitation, not yet solved.

## Setup

```sh
npm install
npm run generate   # regenerate gen/ from proto/tymux/v1/tymux.proto via buf
npm run build
```

`npm run generate` requires the `buf` CLI on `PATH` and resolves the
`protoc-gen-es` plugin from this package's own `node_modules/.bin` (a local
plugin, not a `buf.build` remote one, so it works offline/in CI). CI runs
this on every PR and fails on drift (`git diff --exit-code` against the
committed `gen/` output) — regenerate and commit if the proto changes.

## Running the examples

Start a `tymuxd` first — either from a checkout (from the repo root):

```sh
cargo run --bin tymuxd
```

or, with no Rust toolchain at all, a downloaded release binary:

```sh
./tymuxd
```

Then, from `clients/ts/`:

```sh
npm run list-sessions
npm run attach -- <pane_id>       # pane_id from list-sessions's own output (below)
npm run capture-pane -- <pane_id>
```

`list-sessions.ts` prints each session's id/name/liveness, then every
pane_id in its layout, one per line — that's where `<pane_id>` above
comes from. (`tymux ls`, the Rust CLI's own listing, does not print
pane IDs — only `list-sessions.ts` does.)

`examples/attach.ts` opens `Attach` as a bidi stream, sends the pane's id as
the required first message, forwards a keystroke, reads output until it
sees the command's real result, then **fully cancels the call** — per the
RPC's documented contract, detaching means cancelling the whole call, not
just closing the send side.

## Tests

```sh
npm test
```

Spawns a real `tymuxd` on an ephemeral loopback port (`test/daemon.ts`) and
runs the same `CreateSession` → `Attach` → `CapturePane` proof as an
assertion-backed integration test, wired into CI on every PR.
