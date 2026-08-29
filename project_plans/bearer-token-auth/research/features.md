# Research: bearer-token-auth — feature landscape

**Date**: 2026-08-27
**Agent**: 2 (Features)

## 1. Prior art already gathered elsewhere in this repo

`project_plans/roadmap/README.md:74-85` and `:263` already did a first pass
on this exact question while scoping the "Next" outcome this project comes
from. Its conclusion, not re-derived here:

- **tmux**: relies on OS file permissions on the Unix socket — not
  transferable to tymux's TCP/gRPC transport, which has no filesystem ACL
  equivalent.
- **mosh / Eternal Terminal (ET) / tmate**: all three "treat a successful
  handshake as full control with no per-resource ACL"
  ([tmate.io](https://tmate.io/), [linuxhandbook.com/tmate](https://linuxhandbook.com/tmate/)).
  tmate's actual mechanism is instructive for the *shape* of this
  requirement, not a counter-example: it hands out an unguessable
  SSH-relay URL per session (bearer-by-obscurity, not a verified secret),
  with a separate read-only vs. read-write link as its only scoping
  axis. The roadmap explicitly earmarks that scoping idea for a **later**
  scoped-token outcome (`README.md:82-84`), reusing it "as a verifiable
  token claim instead of an unguessable URL" — confirms this project's own
  Non-Scope call (single shared bearer secret, no scoping) is deliberate
  and sequenced, not an oversight.
- **wezterm SSH/TLS multiplexer domains**: cited in the roadmap's Sources
  list ([wezterm.org/multiplexing](https://wezterm.org/multiplexing.html))
  as the shape of the **mTLS** follow-up already parked in "Later" —
  `README.md:147-148` ("mTLS for daemon-to-daemon / multi-host scenarios,
  layered on the bearer-token work above once it exists"). No fuller
  auth-specific writeup of wezterm exists elsewhere in this repo's research
  docs to build on; treat that roadmap line as the extent of it.
- **zellij**: no auth-relevant findings in any research doc — its
  client-server model (surveyed in `stapler-squad-integration/research/features.md:172-181`
  and `attach-resume-protocol/research/features.md`) is entirely about
  attach/detach and disconnect-survival, not about authenticating a remote
  client. Its docs describe an intentionally single-user, local-socket
  model with no non-loopback exposure story at all.
- **Discord Gateway `Resume`** (`attach-resume-protocol/research/features.md:104-120`):
  relevant to *this* project only as a negative precedent — it's a
  resume-token design, not an auth design, and its identity binding
  (session_id + seq) is unrelated to bearer-credential validation. Not
  reusable here beyond noting it's already been mined for the adjacent
  resume feature, so this project shouldn't re-read it for auth ideas.

**Net finding**: none of the tools this repo has already researched (tmux,
mosh, ET, tmate, zellij, wezterm) implement what this requirement asks
for — a single shared bearer secret validated per-RPC via a gRPC
interceptor. The roadmap's own conclusion — "None of the prior-art tools
surveyed solve this well" (`README.md:77-81`, in the context of
*ownership*, but the same is true of *authentication* — none gate on a
verified credential the way this requirement specifies) — holds. This is
new design work, not adaptation of an existing pattern; the useful prior
art is negative (confirms the requirement's own "one shared bearer secret
is correct for this scope" Alternatives-Considered call, `requirements.md`)
plus tmate's scoping idea already correctly deferred to Next/Later.

## 2. Edge cases the design must explicitly handle

Grounded in reading `crates/tymuxd/src/main.rs` directly (not assumed):

### 2a. Already-connected streaming `Attach` client when the token becomes invalid mid-session

tonic interceptors run **once, at request setup** (they wrap the service
via `.add_service()`/`InterceptedService`, evaluated on the initial
`Attach` unary-open-then-bidi-stream call, not per-message). A daemon
restarted with a different token has **no mechanism today** to re-validate
a token against an already-established stream — the interceptor gate is a
front door, not a per-frame check. Two honest options, not a solved
question:
- Accept this as out-of-scope for v1 (the existing stream keeps running
  under the old token's authorization until it naturally ends — reconnect
  after a restart is where the new token gets enforced). This matches
  gRPC's normal trust model (a channel, once authenticated, stays
  authenticated for its lifetime) and needs no new mechanism.
  **Recommended reading of the requirement**: requirements.md's Success
  Metrics only test "a request... is rejected before reaching any RPC
  handler" (new calls), not existing-stream revocation — so this is
  consistent with in-scope as written, but should be stated explicitly in
  the plan so it isn't assumed away as covered.
- A true kill-existing-streams-on-token-change design (interceptor
  re-checks per-frame, or a version counter compared per-frame) is real
  extra complexity with no concrete driver in this requirement's Success
  Metrics — flag as a Rabbit Hole if raised in planning, since
  requirements.md's own Rabbit Holes section already names "tonic
  interceptor API specifics on the bidirectional `Attach` stream" as a
  named risk.

### 2b. `--token` as an empty string

Must be treated as **equivalent to no token configured** (fail the
non-loopback-bind startup check), not as "auth disabled" or "empty string
is the valid secret." An empty bearer credential that "validates" against
an empty configured token is a classic gotcha (`TYMUXD_TOKEN=""` in a
misconfigured systemd unit or CI env would otherwise silently open the
daemon). The startup check in scope ("refuses to start... if bound
non-loopback with no token configured") needs its emptiness check to
run *before* the loopback/non-loopback branch, not just an
`Option::is_some()` check on the flag/env var.

### 2c. Loopback → non-loopback without a restart

**Not possible given tymuxd's current startup model — confirmed by
reading the code, not inferred.** `crates/tymuxd/src/main.rs:1227-1228`
reads the bind address **once**, from `std::env::var("TYMUXD_ADDR")` (no
CLI flag exists for it today — see 3a below), and `main.rs:1286-1291` calls
`Server::builder()....serve_with_shutdown(socket_addr, shutdown_signal())`
exactly once, blocking until shutdown. There is no re-bind, no config
reload, no signal handler that re-reads `TYMUXD_ADDR` — the bind address
is fixed for the process's entire lifetime. Changing bind address is
inherently a restart today; this requirement doesn't need to solve
live-rebinding, and the plan should state that explicitly rather than
leave it implicit.

### 2d. Simultaneous loopback (no-auth) + non-loopback (auth-required) listeners in one process

**Not supported by the current one-listener-per-process model — confirmed
by reading the code.** `main.rs` has exactly one `socket_addr`, one
`Server::builder()`, one `.serve_with_shutdown()` call. Tonic's
`Server::builder()` binds and serves a single `SocketAddr`; running two
listeners in one process would require either two separate `tokio::spawn`
tasks each running their own `Server::builder()...serve()` (with the
interceptor conditionally applied to only the non-loopback one), or a
custom accept loop merging two `TcpListener`s. This is a real, nontrivial
architectural addition, not a config tweak — **flag as likely out of scope
by construction** unless the plan phase explicitly decides to build it;
requirements.md's Scope doesn't ask for it (it only asks for a single
daemon that either is or isn't non-loopback-bound), and Success Metrics
are phrased as "tymuxd bound to a non-loopback address" (singular),
consistent with single-listener being the intended shape.

## 3. Unstated needs

### 3a. `--token` CLI flag doesn't have existing infra to build on

Read directly: `tymuxd`'s `Cargo.toml` has **no `clap` dependency at all**
(`crates/tymuxd/Cargo.toml`), and `main.rs` has zero CLI argument parsing
today — `TYMUXD_ADDR` is the *only* way to configure the bind address, via
`std::env::var`, with no `--addr` flag either. `tymux-cli` (the client) does
depend on `clap` and does have a proper `Cli`/flag struct
(`crates/tymux-cli/src/main.rs:273` uses `cli.addr`), but that's a
different binary. **Gap worth flagging to the plan phase**: requirements.md
explicitly asks for "`--token` CLI flag or `TYMUXD_TOKEN` env var" for
`tymuxd`, which means either (a) adding `clap` as a new dependency to
`tymuxd` for the first time, just for this one flag (and probably `--addr`
too, while touching startup parsing), or (b) hand-rolling minimal
`std::env::args()` parsing to match the existing `TYMUXD_ADDR`-style
env-var-first pattern instead. This is a real design choice with cost on
both sides, not a given — surfaced here since requirements.md states the
CLI flag as already-decided scope without acknowledging tymuxd doesn't
have flag-parsing infrastructure yet.

### 3b. Operator ergonomics: `TYMUXD_TOKEN_FILE` vs. raw env var

The requirement's Alternatives-Considered rejects "auto-generated token +
local file (Jupyter-style)" — but that's a rejection of **auto-generation**,
specifically. It does not address a distinct, narrower idea: an
**operator-supplied** secret distributed via a file path in an env var
(`TYMUXD_TOKEN_FILE=/run/secrets/tymuxd-token`), which is a completely
different risk profile from auto-generation:
- A raw secret in `TYMUXD_TOKEN` is readable via `/proc/<pid>/environ` by
  any process with ptrace-equivalent access to the daemon's UID, and shows
  up in `ps eww`/systemd's `journalctl -u <unit> -o verbose` under some
  configs, and in crash dumps/core files that capture the environment
  block. This is a real, well-known leak vector for daemons — it's why
  Docker's own docs recommend Docker secrets (files) over env vars for
  credentials, and why systemd has `LoadCredential=`/`EnvironmentFile=`
  (file-based) as the recommended pattern over inlining secrets in `Environment=`.
  Not independently verified in this repo (no existing tymux doc says this);
  cited as general operational-security practice, not a project-specific
  finding.
- A `TYMUXD_TOKEN_FILE` pointing at a 0600 file (fed by 1Password, Vault,
  a k8s Secret volume mount, or a systemd `LoadCredential=`) avoids that
  leak vector entirely while still being **operator-supplied**, not
  auto-generated — the file's *content* is still the same one shared
  bearer secret the requirement specifies, just distributed differently.

**This looks like a real scope gap worth flagging, not solved here**:
requirements.md's In Scope list only names `--token` / `TYMUXD_TOKEN` as
the two supported mechanisms. If `stapler-squad` or any containerized/
systemd deployment is a near-term consumer (see §4), a raw env var is the
worse of the two realistic choices for that deployment shape. Recommend
the plan phase explicitly decide whether `TYMUXD_TOKEN_FILE` is in scope
for v1 or a fast-follow — the underlying interceptor/validation logic is
identical either way (just where the byte string comes from at startup),
so the marginal implementation cost of supporting both from day one is
low, but it wasn't asked for in requirements.md and shouldn't be added
without confirming that's wanted.

## 4. What the concrete near-term consumer (`stapler-squad`'s `BackendTymux`) actually needs

Read directly from `project_plans/stapler-squad-integration/requirements.md:121-125,164-165`:

- That project's own Security classification states: *"both tymuxd and
  stapler-squad's backend are expected to run on the same host; tymux's
  existing loopback-only trust model is not being changed by this
  project."* Its Out of Scope explicitly excludes "Auth/authorization
  changes to tymux — loopback-trust model stays as-is; remote/multi-host
  deployment is not a goal here."
- **There is no existing plan or expectation, anywhere in
  `project_plans/stapler-squad-integration/`, for how `BackendTymux` would
  supply a token.** `BackendTymux` connects to `tymuxd` over loopback today
  and is explicitly scoped to stay that way — confirmed by reading both
  `requirements.md` and grepping `implementation/plan.md` (no `token`/
  `auth`/`bearer` hits in that project's implementation plan).
- Practically, this means: this project's Success Metric #4 ("`clients/go`
  and `clients/ts` can both authenticate... in their own integration
  tests") is the *only* concrete near-term proof this feature gets
  exercised by a real consumer — `stapler-squad` itself won't touch the
  non-loopback/token path until (per the roadmap) it becomes a genuinely
  hosted/multi-tenant deployment, which is a **later**, not-yet-scoped
  outcome. The plan phase should not assume `stapler-squad` integration
  work is a forcing function for any particular token-distribution
  ergonomics (§3b) — there's no real requirement to reverse-engineer from
  its current code, only the roadmap's stated intent ("a shared `tymuxd`
  behind stapler-squad" — `roadmap/README.md:71`) that this is *why* the
  feature exists at all, not *how* `stapler-squad` will consume it yet.

## Sources

Internal:
- `project_plans/roadmap/README.md:74-85,147-148,263` — prior auth-adjacent
  research already done for this outcome
- `project_plans/attach-resume-protocol/research/features.md:96-120` —
  Discord Gateway resume precedent (ruled not reusable for auth, §1)
- `project_plans/stapler-squad-integration/research/features.md:172-181` —
  zellij client-server model (no auth findings)
- `project_plans/stapler-squad-integration/requirements.md:121-125,164-166`
  — confirms loopback-only, auth explicitly out of scope, no token-supply
  plan exists yet
- `crates/tymuxd/src/main.rs:1227-1247,1283-1294` — current single
  env-var-only bind-address parsing, single blocking `serve_with_shutdown`
  call, existing non-loopback warning text
- `crates/tymuxd/Cargo.toml` — confirms no `clap` dependency (no existing
  CLI flag infra for tymuxd)
- `crates/tymux-cli/src/main.rs:273` — contrast: `tymux-cli` already has
  `clap`-based flag parsing (`cli.addr`)
- `clients/go/examples/attach/main.go:32-45` — Connect-RPC (not raw
  grpc-go) transport construction; comment cites "loopback-trust security
  model" for its plain-h2c (no TLS) choice
- `clients/ts/examples/client.ts:5-11` — Connect-RPC transport factory,
  same loopback-trust framing
- `README.md:78-80,97-99` — "loopback-trust model" is the repo's standing
  term for today's no-auth posture
- `docs/reviews/is-it-ready-2026-07-13.md:44` — original blocking-severity
  finding that seeded this whole requirement

External:
- tmate relay/link-scoping model — [tmate.io](https://tmate.io/),
  [linuxhandbook.com/tmate](https://linuxhandbook.com/tmate/)
- wezterm SSH/TLS multiplexer domains —
  [wezterm.org/multiplexing](https://wezterm.org/multiplexing.html)
- systemd credential-passing patterns (`LoadCredential=` vs.
  `Environment=`) — general operational-security practice, not a
  repo-internal finding; cited in §3b as context for the
  `TYMUXD_TOKEN_FILE` gap, not independently verified against this repo
