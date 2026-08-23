# Research: Stack (Go client for tymux's gRPC API)

Scope: how `BackendTymux` gets a Go client for tymux's Attach bidi stream, what
`buf.gen.yaml` needs, dependency/version fit with stapler-squad's existing
`go.mod`, and what stapler-squad already has for ANSI/terminal handling that
bears on the rendering-adapter question.

## 1. Bidirectional gRPC streaming from Go: Connect, not raw grpc-go

The current recommended way to do this against a buf-managed proto, mirroring
`clients/ts/`'s use of `@connectrpc/connect-node`, is
[`connectrpc.com/connect`](https://connectrpc.com/) (the successor to the
`bufbuild/connect-go` v1.x line, renamed/re-imported under the `connectrpc.com`
module path). It generates a plain Go client from the same proto, supports
gRPC, gRPC-Web, and Connect's own protocol over HTTP/2 (h2c for plaintext,
which is what a local daemon like tymuxd would use), and its generated
bidi-stream client type (`*connect.BidiStreamForClient[Req, Resp]`) is what
`Attach` maps to — `Send`/`Receive`/`CloseRequest`/`CloseResponse` on one
struct, no separate send/recv goroutine wiring required by hand the way raw
`google.golang.org/grpc` stream clients need.

**stapler-squad already runs this exact stack for its own server** —
`server/server.go:39` imports `connectrpc.com/connect`, and `go.mod` pins
`connectrpc.com/connect v1.19.0`. There is no new library to introduce; a
generated tymux Go client would use the same package the rest of the codebase
already depends on. (VERIFIED: `grep -n "connectrpc.com/connect" server/server.go`.)

There is also a legacy `github.com/bufbuild/connect-go v1.10.0` entry in
`go.mod`, but it is vestigial: it's referenced only from `tools.go` (a
`//go:build tools` file) to pin the `protoc-gen-connect-go` binary import, and
nothing else in the tree imports it (`grep -rln "bufbuild/connect-go" --include=*.go`
matches only `tools.go`). The actual codegen path uses the *remote* buf plugin
`buf.build/connectrpc/go` (see below), not this local binary — the tools.go
pin looks like dead weight from before the project moved to remote plugins.
Worth a note in the plan, not a blocker.

## 2. What `clients/ts/` does today, and how to add Go alongside it

`proto/buf.gen.yaml` (tymux repo) currently has one plugin block: a **local**
`protoc-gen-es` resolved from `clients/ts/node_modules/.bin`, generating into
`clients/ts/gen`. Its header comment explicitly says: *"This file is for
*other* language clients (TS, Python, Go, ...) as they show up"* — i.e. Go
codegen belongs in this same file, not a new one.

```yaml
# proto/buf.gen.yaml (tymux) — current
version: v2
inputs:
  - directory: .
plugins:
  - local: ../clients/ts/node_modules/.bin/protoc-gen-es
    out: ../clients/ts/gen
    opt: target=ts
```

stapler-squad's own root `buf.gen.yaml` is the concrete pattern to mirror for
adding Go — it already generates Go protobuf + Connect stubs via **remote**
plugins (no local binary/toolchain needed, `buf` calls out to the BSR):

```yaml
# stapler-squad/buf.gen.yaml (existing, for reference)
version: v2
managed:
  enabled: true
  override:
    - file_option: go_package_prefix
      value: github.com/tstapler/stapler-squad/gen/proto/go
plugins:
  - remote: buf.build/protocolbuffers/go
    out: gen/proto/go
    opt: [paths=source_relative]
  - remote: buf.build/connectrpc/go
    out: gen/proto/go
    opt: [paths=source_relative]
```

Adding Go codegen to tymux's `proto/buf.gen.yaml` means appending two more
plugin entries (protoc-gen-go + protoc-gen-connect-go) alongside the existing
TS one, e.g. into a new `clients/go/gen` directory to match the `clients/ts/`
layout, with `managed.override.file_option: go_package_prefix` set to
whatever Go module path the generated code should live under (either inside
the tymux repo as `github.com/<org>/tymux/clients/go/gen`, consumed by
stapler-squad as an external module dependency, or vendored/copied directly
into stapler-squad's own `gen/proto/go` tree the way its own protos are — this
is a real open decision, not just a config detail, since the two repos have
"no submodule/monorepo relationship" per the requirements' constraints).
`buf generate proto` from the tymux repo root would then produce both TS and
Go clients from one invocation, same as it does for TS today.

Either the `remote:` plugin form (network call to the BSR, what stapler-squad
uses) or a `local:` form pointing at `protoc-gen-go`/`protoc-gen-connect-go`
installed via `go install` works; tymux's TS plugin block already favors
`local:` specifically so generation works offline/in CI without a BSR call
(per the file's own comment) — the same rationale would argue for `local:` Go
plugins too, for consistency, rather than mixing remote (Go) and local (TS)
resolution strategies in one `buf.gen.yaml`.

## 3. Version/dependency fit for wiring a generated client into stapler-squad

- stapler-squad's `go.mod`: `go 1.26.3` (toolchain installed: `go1.26.4`),
  `connectrpc.com/connect v1.19.0`, `google.golang.org/protobuf v1.36.11`,
  `google.golang.org/grpc v1.81.1` (indirect, pulled in via OTel/other deps,
  not used directly for RPC — Connect is the actual RPC layer). A generated
  tymux Go client built against a compatible `google.golang.org/protobuf`
  major/minor and any recent `connectrpc.com/connect` v1.x should slot in as
  a plain `go get`/`go.sum` addition with no ecosystem clash — both repos are
  already on the same protobuf-Go/Connect generation lineage (`buf` +
  `protoc-gen-go` + `protoc-gen-connect-go`), so there's no parallel gRPC
  stack to reconcile.
- `github.com/bufbuild/buf v1.57.2` is already a direct dependency (used for
  stapler-squad's own proto generation), so the `buf` CLI itself doesn't need
  introducing — it's already available as a Go tool dependency in this module.
- No H2C/TLS mismatch expected: Connect's client transport handles h2c
  (cleartext HTTP/2) for local/plaintext daemon connections out of the box,
  which is the likely tymuxd deployment shape (local Unix socket or localhost
  TCP, no TLS) — same as how `clients/ts/` connects via `connect-node`.
- If tymux's generated Go package is published as its own external Go module
  (rather than copied into stapler-squad's tree), it needs a `go.mod` +
  tagged version in the tymux repo for stapler-squad to `go get` — currently
  tymux has no root `go.mod` at all (it's a Rust workspace); this is new
  infrastructure, not a version bump.

## 4. ANSI/terminal libraries already in stapler-squad — bears on the rendering rabbit hole

**No ANSI-parsing/cell-grid library is used in the current tmux capture/stream
path.** Searched `session/tmux/*.go` for `ansiterm`, `vt10x`, and similar —
no hits. `github.com/Azure/go-ansiterm` *is* present in `go.mod`, but only as
an **indirect** dependency (confirmed via `go.sum` provenance and
`grep -rln "go-ansiterm" --include=*.go` returning zero matches outside
`go.sum`) — it's pulled in transitively by the vendored Docker client
libraries (`docker/cli`, `docker/docker`, `moby/term` are all direct/indirect
deps too), not used by stapler-squad's own code.

Concretely, the current tmux control-mode output path
(`session/tmux/control_mode.go:648` `handleOutputBytes` →
`decodeControlModeOutput` → `broadcastControlModeUpdate`) does exactly one
transform: undo tmux control-mode's octal-escape encoding (`\ooo` → raw byte)
on the `%output` line. The resulting bytes — which already contain raw ANSI
escape sequences, since tmux panes are spawned with `TERM=xterm-256color`
(`tmux.go:999`) — are pushed unmodified onto a per-subscriber `chan []byte`
and from there straight to the browser's xterm.js over WebSocket. There is
**no cell-grid intermediate representation and no server-side ANSI
interpretation** in the live-streaming path today; `capture-pane -p -e -J`
(ANSI-preserving, joined-line snapshot) is used only for point-in-time
captures, not for the live stream.

This directly de-risks the "Cell-grid → xterm.js rendering" rabbit hole from
requirements.md: stapler-squad's *existing* live-output path is already raw
byte pass-through to xterm.js, not a cell-grid diff/apply scheme (that was
the abandoned ADR-002 approach per the requirements doc). tymux's `Attach`
RPC, which the requirements describe as delivering "raw PTY byte streaming,"
is architecturally the closer match to what stapler-squad does *today*, not
a mismatch requiring a new translation layer. The adapter work is more likely
to be a thin swap — replace `decodeControlModeOutput`'s octal-unescape with
whatever framing tymux's `Attach` stream uses (proto message unwrapping
instead of control-mode line parsing) — than a rendering-model change. This
should still be validated against tymux's actual `Attach` wire format
(need to confirm the bytes on that RPC are raw terminal bytes, not
pre-diffed/structured — the `CapturePane`/`SearchScrollback` RPCs sound more
likely to be structured snapshots, based on their names, and would need the
translation layer if used for the live path instead of `Attach`).

## Summary of concrete recommendations

1. Use `connectrpc.com/connect` (already a stapler-squad dependency) for the
   generated Go client — no new RPC library needed.
2. Extend tymux's existing `proto/buf.gen.yaml` (don't create a new file) with
   `protoc-gen-go` + `protoc-gen-connect-go` plugin entries alongside the
   current TS block, output to a new `clients/go/` directory mirroring
   `clients/ts/`'s layout; decide during planning whether the generated Go
   package ships as its own Go module (needs a new `go.mod` in tymux, which
   has none today) or gets vendored into stapler-squad's `gen/proto/go` tree.
3. No dependency conflicts expected wiring the generated client into
   stapler-squad's `go.mod` — same protobuf-Go/Connect/buf lineage on both
   sides already.
4. No ANSI/cell-grid library work is implied by stapler-squad's current
   architecture — it already streams raw bytes to xterm.js — but this needs
   one more confirmation: that tymux's `Attach` RPC carries raw terminal
   bytes (not a structured/diffed format), which determines whether the
   "rendering adapter" scope item in requirements.md is trivial or not.
