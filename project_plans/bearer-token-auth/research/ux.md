# UX Research: bearer-token-auth

**Date**: 2026-08-27
**Scope**: `tymuxd` startup gate + `tymux-cli`/client error UX for the new
bearer-token requirement on non-loopback binds. No prior UX research file
existed for this project; this is new research, not a synthesis of an
existing `research/ux.md`.

## 1. Comparable UX patterns

No prior tymux research doc covers shared-secret/token auth specifically —
`project_plans/stapler-squad-integration/research/features.md:146-181` and
`project_plans/attach-resume-protocol/research/features.md:95-110` cover
mosh/Eternal Terminal/Zellij/Discord Gateway, but all for *stream
continuity* (reconnect/resume), not authentication. The roadmap
(`project_plans/roadmap/README.md`, "Next" section) explicitly notes none
of tmux/mosh/ET/tmate solve per-resource authorization well, but doesn't
cover their *token UX* either. Filling that gap here from general knowledge
of the same tools plus this repo's own established error-message idiom:

- **Jupyter Notebook** — the closest "operator forgot to configure auth"
  precedent: an unauthenticated request gets a 403 with a page explaining a
  token or password is required, and the startup log prints the full
  `?token=...` URL. tymux's requirements explicitly reject the
  auto-generated-token-in-a-URL model (see requirements.md "Alternatives
  Considered") — but the lesson worth keeping is that Jupyter never leaves
  the user guessing *that* a token is needed; the very first thing printed
  makes the requirement and the remedy obvious. tymux's equivalent is
  "fail fast at startup with the flag name in the message," not "run
  degraded and hope the client figures out why every call 401s."
- **`docker` / `DOCKER_HOST` with TLS** — a `docker` CLI pointed at a
  TLS-secured remote daemon without certs fails with a specific, short
  message naming the missing credential material, distinguishable from
  "connection refused" (wrong host/port) and from a normal API error
  (image not found, etc.). Same three-way split this feature needs:
  unreachable vs. missing/bad credential vs. ordinary error.
- **`redis-cli` with `requirepass`** — `NOAUTH Authentication required.` is
  a single-line, distinct error code/prefix (`NOAUTH`) that is trivially
  greppable and never confusable with `ERR no such key` or a connection
  timeout. The pattern worth reusing: give the *auth-specific* failure a
  distinct, stable shape (here: gRPC `Unauthenticated` code + a message
  that names the remedy), not just distinct prose.
- **`curl -u`/`Authorization: Bearer` APIs generally** — convention is
  `401 Unauthorized` for "no/bad credential" vs. `403 Forbidden` for
  "credential fine, not allowed." gRPC's own status vocabulary mirrors
  this split (`UNAUTHENTICATED` vs. `PERMISSION_DENIED`); this feature is
  squarely the `UNAUTHENTICATED` case (no per-resource authz yet — that's
  the separate, later roadmap item), so `tonic::Status::unauthenticated`
  is the correct, idiomatic choice, not `permission_denied`.

**Takeaway**: every comparable tool keeps "you need a credential" and
"that's not a valid credential" recognizably different from *transport*
failures (host down/refused) and from *ordinary* API errors (not found).
None of them require the client to parse prose to tell those apart — they
rely on a distinct status code/prefix first, clear text second. tymux-cli
already has the scaffolding for exactly this pattern (see §3).

## 2. Operator mental model: first-run non-loopback without `--token`

Requirements mandate fail-fast at startup, not a warning. Today's
`crates/tymuxd/src/main.rs:1238-1247` non-loopback case only
`tracing::warn!`s and continues; the new behavior must refuse to bind at
all when non-loopback + no token.

**Existing tone/format precedent to match** — two established patterns in
`main()`:

1. The `sessions_dir` prep failure
   ([`crates/tymuxd/src/main.rs:1254-1259`](../../../crates/tymuxd/src/main.rs#L1254-L1259)):
   converts the error to a plain `String` via `.map_err(|e| format!(...))?`
   *specifically* so Rust's default `Result`-returning-`main` prints one
   clean line instead of a Debug dump —
   `"failed to prepare sessions directory {path}: {e}"`. This is the
   template: `failed to <action>: <context>: <underlying cause>`.
2. The existing non-loopback warning
   ([`crates/tymuxd/src/main.rs:1239-1246`](../../../crates/tymuxd/src/main.rs#L1239-L1246)):
   states the risk in plain language ("any client that can reach this port
   has full control"), not just "insecure configuration detected."

**Recommended startup-failure message**, following both precedents and
naming the exact remedy (mirrors `tymux-cli`'s own
`check_attach_liveness` idiom, §3):

```
failed to start: bound to non-loopback address {addr} with no token configured.
Set --token or TYMUXD_TOKEN before binding tymuxd to a non-loopback address —
this port would otherwise let any network client run arbitrary commands.
(Loopback binds, e.g. 127.0.0.1, never require a token.)
```

This should route through the same `.map_err(|e| format!(...))?` →
plain-`String`-error path as `sessions_dir`, not a `tracing::error!` +
`std::process::exit`, so it reads as one clean line consistent with the
one existing fatal-startup-error case, not a new format. It should fire
*before* the `sessions_dir`/persistence-loading work
(`crates/tymuxd/src/main.rs:1253` onward) — no reason to touch disk or
load sessions just to then refuse to serve.

## 3. Error states in `tymux-cli`

`crates/tymux-cli/src/main.rs:259-269` already has exactly the
three-way-dispatch structure this feature needs to extend —
`friendly_message(e: &anyhow::Error)`:

```rust
fn friendly_message(e: &anyhow::Error) -> String {
    if e.downcast_ref::<tonic::transport::Error>().is_some() {
        return "couldn't connect to tymuxd — is the daemon running? \
                (start it with `cargo run -p tymuxd`)"
            .to_string();
    }
    if let Some(status) = e.downcast_ref::<tonic::Status>() {
        return status.message().to_string();
    }
    e.to_string()
}
```

Case (a) "unreachable" is already handled and its wording
("X — is Y? (do Z)") is the tone to match. Case (c) "reachable, real RPC
error" is the generic `tonic::Status` fallthrough today — for example
`no such session: abc`
([`crates/tymux-cli/src/main.rs:1125`](../../../crates/tymux-cli/src/main.rs#L1125)
in tests) or the dead-session remediation message in
`check_attach_liveness`
([`crates/tymux-cli/src/main.rs:168-176`](../../../crates/tymux-cli/src/main.rs#L168-L176)):
`"Session '{name}' is not running (...). Run 'tymux revive {name}' to
respawn it, then attach again."` — that Run-X-to-Y-then-Z shape is this
repo's established remediation idiom and should be matched, not
reinvented.

Case (b) "reachable, no/wrong token" is new and, left unhandled, would
silently fall into the generic `tonic::Status` branch — printing whatever
raw message the server-side interceptor happens to set, with no
CLI-specific framing and no reminder of `--token`/`TYMUXD_TOKEN`. That's
exactly the "4th indistinguishable case" the requirements warn against:
today a `Status::not_found` and a hypothetical bare
`Status::unauthenticated("invalid token")` would look almost identical —
one line of prose, no visual or structural distinction. Recommend an
explicit branch, checked before the generic `Status` fallthrough:

```rust
if let Some(status) = e.downcast_ref::<tonic::Status>() {
    if status.code() == tonic::Code::Unauthenticated {
        return format!(
            "tymuxd rejected this connection: {} \
             (set --token or TYMUXD_TOKEN to authenticate)",
            status.message()
        );
    }
    return status.message().to_string();
}
```

Proposed server-side `status.message()` text (interceptor side, not
`tymux-cli`) — kept short since `friendly_message` wraps it:

- No token supplied at all: `"missing bearer token"`
- Token supplied but wrong: `"invalid bearer token"`

These two need not be distinguished any further in the CLI-visible text
per the requirements (only "distinguishable from daemon-unreachable and
session-not-found" is required) — but keeping "missing" vs. "invalid"
separate server-side costs nothing (checked as metadata-present-or-not,
*before* the constant-time comparison of a supplied token, so it adds no
timing side-channel) and helps an operator debugging a typo'd env var
without exposing any part of the token itself. Composed with the CLI
wrapper: `tymux: tymuxd rejected this connection: missing bearer token
(set --token or TYMUXD_TOKEN to authenticate)`.

This keeps all three cases visually and structurally distinct:

| Case | Message shape |
|---|---|
| (a) unreachable | `couldn't connect to tymuxd — is the daemon running? (start it with ...)` |
| (b) no/wrong token | `tymuxd rejected this connection: {missing\|invalid} bearer token (set --token or TYMUXD_TOKEN to authenticate)` |
| (c) other RPC error | raw `status.message()`, e.g. `no such session: abc` |

## 4. Job-to-be-done

The operator's actual job is "expose one `tymuxd` instance to a backend
service (stapler-squad's `BackendTymux`) or a future web frontend, safely"
— not "learn an auth system." Two sub-jobs the UX needs to serve without
conflating them:

- **The common case (loopback, solo dev) must stay invisible.** No flag,
  no env var, no message change, ever — confirmed nothing in scope touches
  the loopback path (requirements: "Loopback bind ... is unaffected").
- **The security-relevant case (non-loopback) must announce itself as a
  security decision, not a connectivity setting.** The existing
  non-loopback warning already does this well
  ("has full control... arbitrary commands... Do not do this on an
  untrusted network") — the new startup-fail message (§2) should carry the
  same register (states the *consequence* of skipping the token, not just
  "config value missing") so an operator who copies a `docker-compose.yml`
  from a stranger's gist without reading it still gets the risk stated
  plainly, not just a flag name.

Balance: state the risk once, clearly, at the point of failure (startup,
or the CLI's rejected-connection message) — not repeatedly, and not with
alarmist tone on every subsequent successful authenticated call. A
correctly-authenticated non-loopback session should behave identically to
a loopback one after the token is accepted; the security framing belongs
at the two failure points (daemon won't start; client got rejected), not
sprinkled through normal operation.

## 5. Accessibility

Not applicable in the WCAG/ARIA sense (CLI/daemon, no GUI), per the task
scope — skipped. Checked the narrower color-reliance question instead:
`crates/tymux-cli/src/main.rs` and the rest of `crates/tymux-cli/src/`
have **no colored-terminal-output dependency** (`grep` for
`colored`/`owo_colors`/`termcolor`/`ansi_term`/raw ANSI escapes in
`crates/tymux-cli` returns nothing; the one `Color`-named hit in the repo
is `crates/tymux-core/src/pane.rs`, unrelated pane-content color handling,
not CLI chrome). All existing `tymux-cli` error/status text — including
`friendly_message` and `check_attach_liveness` — is plain, uncolored
stdout/stderr text. The new `Unauthenticated` message should stay
consistent with that: no new color dependency, no reliance on color to
convey "this is the auth failure" — the distinguishing signal is the
message text and gRPC status code, which is also what makes it robust for
a programmatic client (`clients/go`/`clients/ts`) parsing the status code
rather than a human reading colored output.

## Flag/env-var precedence note (feeds Open Questions in requirements.md)

`crates/tymux-cli/src/main.rs:181-182` — the existing `--addr` flag has
**no** `env = "..."` clap attribute, even though `tymuxd` itself reads
`TYMUXD_ADDR` via `std::env::var`
([`crates/tymuxd/src/main.rs:1227`](../../../crates/tymuxd/src/main.rs#L1227)).
So there is no existing *clap-level* flag+env-var precedent in
`tymux-cli` to follow mechanically — `TYMUXD_ADDR` today only works
daemon-side, not as a `tymux-cli` client flag fallback. For the new
`--token`, using clap's built-in `#[arg(long, env = "TYMUXD_TOKEN")]` is
still the right idiomatic choice (clap is already a dependency; its
documented precedence is explicit-CLI-arg overrides env-var, which
matches ordinary CLI-flag-beats-environment expectations) — just note in
the plan that this is a *new* pattern for this CLI, not a reuse of an
existing one, so it's worth being deliberate about applying it
consistently to `addr` too rather than leaving `--token` as the only
env-aware flag.
