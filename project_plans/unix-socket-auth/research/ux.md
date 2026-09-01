# UX Research: unix-socket-auth

**Date**: 2026-08-29
**Scope**: CLI/operator UX for adding a Unix-domain-socket (UDS) listener,
peer-credential (`SO_PEERCRED`) enforcement, and a TCP-loopback deprecation
warning to `tymuxd`, plus the matching client-side connect/error UX in
`tymux-cli`, `clients/go`, `clients/ts`. No GUI surface — everything below is
terminal text: flags, env vars, log lines, and error messages. Builds on the
sibling `bearer-token-auth` project's own UX docs
(`project_plans/bearer-token-auth/research/ux.md`,
`project_plans/bearer-token-auth/design/ux.md`), which this feature must stay
terminologically and structurally consistent with — that project already
established the "distinct opening clause per failure class" error idiom and
the plain-text-only (no color) convention this project inherits.

## 1. Comparable UX patterns: "local socket, auto-discovered, no config"

- **Docker CLI / `dockerd`'s Unix socket** — the closest precedent for this
  feature's actual mechanism (owner/group-gated Unix socket, not a shared
  secret). `dockerd` listens on `/var/run/docker.sock`, created
  `root:docker`, mode `0660`; a user not in the `docker` group gets a
  specific, well-known message: `"Got permission denied while trying to
  connect to the Docker daemon socket at unix:///var/run/docker.sock"`
  (confirmed via search — this exact string is what most Docker
  permission-denied guides quote verbatim). Two things make this feel
  zero-config rather than manually set up: (1) the socket path is fixed and
  well-known — no env var to discover, no flag to pass, the client just
  tries the default path; (2) the *fix* for the rejected case is a one-time
  system-level action (`usermod -aG docker $USER`, then re-login), not a
  per-invocation credential. This maps directly onto this feature's
  `--socket-group`/group-membership design: the UX goal should be the same
  two properties — a fixed default path tried automatically, and a
  permission-denied message that names the actual remedy (join the
  configured group) rather than a generic "access denied."
- **`ssh-agent` + `SSH_AUTH_SOCK`** — a *cautionary* precedent, not one to
  copy wholesale. Its zero-config feel is weaker than Docker's: it requires
  an explicit `eval $(ssh-agent)` (or shell/desktop-session integration) to
  populate `SSH_AUTH_SOCK` in the first place — it is discovery-by-
  environment-variable, not discovery-by-fixed-path. Its failure mode is
  also worth avoiding: a stale `SSH_AUTH_SOCK` (agent process died, socket
  file orphaned) produces a raw, unfriendly `Connection refused` errno
  string with no indication *why* — confirmed by search (multiple guides
  exist purely to explain this one error). tymux-cli's clients already
  default to a hardcoded address (`http://127.0.0.1:7419` in
  [`crates/tymux-cli/src/main.rs:179`](../../../crates/tymux-cli/src/main.rs#L179),
  `http://127.0.0.1:7419` in
  [`clients/go/examples/list-sessions/main.go:64`](../../../clients/go/examples/list-sessions/main.go#L64),
  and the same default in
  [`clients/ts/examples/client.ts:20`](../../../clients/ts/examples/client.ts#L20))
  with no env var required for the common case — the UDS default should
  extend that same fixed-default idiom (a well-known default path tried
  automatically) rather than introduce `ssh-agent`'s weaker
  discover-via-env-var-someone-else-set model. An env var is still useful as
  an *override* (for a non-default socket location), just not as the
  primary discovery mechanism the way `SSH_AUTH_SOCK` is.
- **`systemctl --user`** — relevant for the *fallback-path selection*
  question specifically (an Open Question in requirements.md), and for
  §3 below. It depends on `$XDG_RUNTIME_DIR` being set, which in turn
  depends on a PAM-managed login session (`pam_systemd`) — confirmed via
  search: running under `cron`, `su`, or a non-login shell frequently leaves
  `XDG_RUNTIME_DIR` unset, producing `Failed to connect to bus:
  $XDG_RUNTIME_DIR not defined` (a well-documented systemd issue class, not
  a one-off report). This is the single most relevant external precedent for
  this feature's own `$XDG_RUNTIME_DIR/tymuxd/tymuxd.sock` default: any context missing
  a full login session — CI runners, cron-launched scripts, `su -c`,
  containers without a session manager — can hit exactly the same gap. The
  daemon and every client need a **documented, deterministic fallback**
  (e.g. a path under the sessions directory tymux already owns, see
  `tymux_core::default_sessions_dir()` referenced in
  [`crates/tymuxd/src/main.rs:1279`](../../../crates/tymuxd/src/main.rs#L1279))
  rather than silently failing or silently choosing something different
  between an interactive shell and a script.
- **`gh` CLI's local state** (config/keyring discovery) — less directly
  applicable since it's about credential storage, not a socket, but the
  transferable lesson is *fallback chain transparency*: `gh` documents its
  exact lookup order (env var → keyring → config file) so a user can reason
  about which one is winning. This feature's socket-path resolution should
  have the same property: one documented order (explicit flag/env override
  → default `$XDG_RUNTIME_DIR` path → documented fallback path), stated once
  in `--help`/docs, not left to be reverse-engineered from source.

**Takeaway**: the fixed-well-known-default-path model (Docker) is the right
one for this feature's "zero-config" success metric, not the
env-var-someone-else-populates model (`ssh-agent`). The fallback path itself
needs the same rigor `systemctl --user`'s ecosystem still lacks — CI and cron
are exactly the environments most likely to hit the gap, and this project's
own client stacks (`tymux-cli`, `clients/go`, `clients/ts`) all get exercised
in scripted/CI contexts per the requirements' "integration tests per client"
success metric, so this isn't a theoretical edge case for this repo.

## 2. User mental models

**Baseline expectation (unaffected case)**: per requirements' explicit
success metric ("Loopback-bound tymuxd on single-user machine continues to
work with zero required config change"), a single-user-machine operator
should see *no* behavior change — same commands, same output, same defaults.
This mirrors `bearer-token-auth`'s own "silent upgrade for the common case"
precedent (`project_plans/bearer-token-auth/design/ux.md` Surface 2, AC1).
Nothing in this feature's scope should require that user to learn a new
concept.

**The risk this research flags explicitly** (per the task prompt's own
framing) is the opposite failure mode, and it is real, not hypothetical,
given this feature's specific "both-by-default" design:

A shared-host admin sees a new "Unix socket auth" feature ship, reasonably
infers "my users are now isolated from each other," and stops there. But
per requirements' Scope and Risk Control sections, the existing TCP loopback
listener **stays active and stays fully unauthenticated** — this project
only adds a `tracing::warn!` about it, it does not gate it. So on that same
shared host, *any* local process — including one running as a different OS
user — that connects over TCP to `127.0.0.1:7419` instead of the new UDS
path still gets full, uncredentialed access: `CreateSession`,
`Attach`/`CapturePane` against any `pane_id`, `KillSession`. The UDS
peer-cred check protects nothing for a client that simply doesn't use the
UDS path. This is not a corner case requiring unusual effort — TCP loopback
is `tymuxd`'s *original* default transport, so any pre-existing script,
alias, or the daemon's own `--addr`-based fallback that still points at TCP
continues to work exactly as unsafely as before.

This means the feature's actual security posture, stated plainly, is: **"an
attacker/other local user is blocked only if nothing they control can reach
TCP loopback."** That is a materially weaker guarantee than "sessions are
isolated by default," and an admin evaluating tymux for a shared/multi-user
deployment (the social job in §5) needs to be told this in the same breath
as being told the feature exists — not left to infer it from the fact that
Scope says "not removed in this project."

**UX recommendation**: the startup log message (already required by
requirements' Observability Requirements as a `warn`-level, once-per-startup
line) should not just note that TCP loopback is deprecated — it should state
the *consequence* in the same register `bearer-token-auth`'s own non-loopback
warning uses
([`crates/tymuxd/src/main.rs:1262-1267`](../../../crates/tymuxd/src/main.rs#L1262-L1267)):
name what remains true, not just what changed. E.g. (illustrative wording,
not final copy — that belongs in `design/`):

```
WARN tymuxd: TCP loopback listener (127.0.0.1:7419) is deprecated and will be
removed in a future release; it grants unauthenticated access to any local
user, regardless of the new Unix-socket listener at {uds_path}. Other local
users are isolated only if nothing on this host still connects over TCP —
audit any existing --addr/TYMUXD_ADDR usage before relying on this host being
multi-user-safe.
```

The same caveat belongs in whatever operator-facing doc introduces this
feature (README/CHANGELOG), matching Surface 5's precedent in
`bearer-token-auth`'s own design doc (a doc-comment/README moment for a
concept the flag/env-var name alone doesn't convey).

## 3. Terminal-UX equivalents (not WCAG/ARIA)

**Color-only signaling**: not applicable today and should stay that way.
Confirmed no colored-terminal-output dependency exists anywhere this
feature's error paths would touch — `grep` for
`colored|owo_colors|termcolor|ansi_term` across `crates/tymux-cli`,
`crates/tymuxd`, `clients/go`, `clients/ts` (excluding `node_modules`)
returns nothing beyond `node_modules`' own internals. This repeats the exact
finding `bearer-token-auth`'s own research doc made
(`project_plans/bearer-token-auth/research/ux.md` §5) — no drift since then.
The new UDS-related messages (§4 below) should follow the same rule: the
distinguishing signal between failure classes must be message text and (for
the gRPC-reachable cases) the `tonic::Code`, never a color that a
log-aggregator, screen reader, or `NO_COLOR`-respecting terminal would drop.

**Plain language over jargon**: `SO_PEERCRED`, "peer credential", "uid/gid"
are precise but not something every `tymux` user needs handed to them raw.
Recommend the *default*-path success case never mentions these terms at all
(it should be silent, per §2's baseline), and the *rejection* case
(§4, case (c)) names the concrete, checkable fact — "connected as a
different user" — rather than the mechanism, similar to how Docker's message
names "permission denied" and a concrete remedy (join a group) rather than
citing socket permission bits.

**Scriptability / non-interactive usage**: this is the section with a real,
concrete risk specific to this feature (not present in `bearer-token-auth`,
which touched no path-discovery logic). Two distinct concerns:

1. **`$XDG_RUNTIME_DIR` availability differs between interactive and
   scripted contexts.** Per §1's `systemctl --user` precedent, CI runners,
   cron jobs, and `su -c` invocations frequently lack a PAM-managed session
   and thus lack `$XDG_RUNTIME_DIR` — exactly the contexts the
   requirements' own "integration tests per client" success metric will
   exercise (CI running `clients/go`/`clients/ts`/`tymux-cli` integration
   tests against a live daemon). If the daemon's default path
   (`$XDG_RUNTIME_DIR/tymuxd/tymuxd.sock` per requirements' Scope) silently falls
   back to something else when the var is unset, and a *client* independently
   computes its own default without the exact same fallback logic, an
   interactive shell and a CI job could resolve to two different socket
   paths for what's meant to be the same daemon — a connectivity bug that
   only reproduces in CI. **Recommendation**: the fallback rule must be a
   single, shared, exactly-specified algorithm (same fallback order,
   documented once) applied identically by `tymuxd` and all three clients,
   not independently reimplemented per language, and it must not itself
   depend on TTY presence.
2. **No TTY-conditional branching precedent exists in this codebase, and
   this feature should not introduce the first one.** Confirmed via `grep`
   for `is_terminal|isatty|IsTerminal` across `crates/` and `clients/`
   (excluding `node_modules`): zero hits outside vendored dependencies —
   `tymux-cli` today behaves identically whether its output is piped or
   attached to a terminal. Whatever the UDS-vs-TCP default-transport
   decision logic is, it must be based only on socket-path
   presence/reachability (and flags/env), never on `isatty()` — a scripted
   invocation (CI, systemd unit, cron) must resolve the same transport an
   interactive shell would, given the same environment. This is also the
   more defensible security property: an attacker shouldn't be able to
   induce a downgrade to the less-authenticated TCP path just by running a
   client in a non-interactive context.

## 4. Error states

Enumerating every distinct failure and how it should read, extending
`bearer-token-auth`'s existing `friendly_message`
([`crates/tymux-cli/src/main.rs:266-282`](../../../crates/tymux-cli/src/main.rs#L266-L282))
dispatch pattern rather than replacing it — that function already
special-cases `tonic::transport::Error` (unreachable) and
`tonic::Code::Unauthenticated` (bearer-token rejection); this feature adds
new cases in the *same transport-connect stage*, before any RPC is even
attempted, which is a different code path than `friendly_message`'s
post-connect dispatch and needs its own explicit handling in the UDS-dial
logic (`run()`'s `endpoint.connect().await?` at
[`crates/tymux-cli/src/main.rs:350`](../../../crates/tymux-cli/src/main.rs#L350)
becomes a UDS-dial-then-TCP-fallback sequence, not a single `connect()`
call).

| # | Failure | Distinguishing signal available | Recommended message shape | Distinct from... |
|---|---|---|---|---|
| 1 | Daemon not running at all (no socket file, no TCP listener) | `ENOENT`/connection-refused on both UDS and TCP attempts | `couldn't connect to tymuxd — is the daemon running? (start it with \`cargo run -p tymuxd\`)` — **this is the existing message** (`crates/tymux-cli/src/main.rs:268-270`); no new case needed, since "socket file absent" and "TCP refused" both collapse to "daemon isn't running" from the operator's perspective. | New cases 2-4 below, all of which imply the daemon *is* running |
| 2 | Stale socket file (daemon crashed, file never cleaned up) | UDS `connect()` returns `ECONNREFUSED`/`ENOTCONN` despite the path existing, while a fresh `tymuxd` process isn't found (no independent process-liveness signal is actually available to the client — it can only observe the connect failure) | Treat identically to case 1 for messaging purposes — the client cannot reliably distinguish "no daemon ever started" from "daemon crashed, left a stale file" from the outside, and inventing two near-identical messages for a distinction the user can't act on differently ("start it") would violate the task's own "not a wall of near-identical variants" guidance. `tymuxd` itself should still remove its own socket file on clean shutdown and, where feasible, on startup detect+replace a stale one it owns (implementation concern, not a new client-visible message) | — |
| 3 | UDS socket present and daemon reachable, but peer-cred rejected (wrong uid, not in configured group) | `tonic`/transport-layer error surfaced *after* a successful UDS connect but rejected by the daemon before any RPC — needs to arrive as a distinguishable status, not a bare transport-level connection-reset (requirements' "clear specific error on peer-cred rejection, not a raw transport-error dump" is explicit about this) | `tymuxd rejected this connection: not authorized to access this daemon's socket (connected as uid {your_uid}; ask the daemon's owner to add you to its configured access group, or use the daemon's own OS user account)` — mirrors the `bearer-token-auth` rejected-connection shape (states what happened + the remedy in one line, no color, no jargon beyond "uid" which is unavoidable here) | Case 1 (daemon unreachable) — this case *did* reach the daemon; case 4 (path unusable) — this case's path works fine, the daemon actively said no |
| 4 | UDS path unwritable/unreadable — `$XDG_RUNTIME_DIR` itself has bad permissions, doesn't exist, or the fallback directory isn't writable by the daemon at startup | Filesystem-level error (`EACCES`/`ENOENT` on the *directory*, not the socket) surfaced at `tymuxd` bind time, not at client connect time | `tymuxd`-side startup failure, same "fail fast, name the remedy" register as `bearer-token-auth`'s Surface 1 (`project_plans/bearer-token-auth/design/ux.md`): `failed to create Unix socket at {path}: {underlying io error}. Check that {parent_dir} exists and is writable, or override the path with --socket-path/TYMUXD_SOCKET_PATH.` This is a *daemon*-side error (the daemon fails to start), not a client-visible RPC error — distinguishable from case 3 by which side it occurs on and by never reaching the client at all (the client just sees case 1, "daemon not running", which is correct here: the daemon genuinely isn't running) | Case 3 (daemon started fine, rejected *this client*) |
| 5 | (Not in the task's list but adjacent, flagged for completeness) TCP loopback still answering while UDS is down/misconfigured — client silently falls back to the now-deprecated, unauthenticated transport | No error at all from the client's point of view — this is a *silent* degrade, which is its own UX problem | If the client-side default-transport logic includes a UDS→TCP fallback (likely needed for the "both-by-default" backward-compat metric), that fallback should be logged/surfaced at least once (e.g. a one-line stderr note on first use, not per-RPC) so an operator debugging "why did my supposedly-isolated session let another user in" has a trail — silent, permanent fallback to the weaker transport is the single worst outcome this feature could produce given §2's mental-model risk | All of the above — this is a *non-error* that needs a signal anyway |

**Net distinguishability**, extending `bearer-token-auth`'s existing table
(`project_plans/bearer-token-auth/design/ux.md` Surface 4 AC1) with this
feature's new cases:

| Case | Opening clause |
|---|---|
| Daemon unreachable (cases 1-2, existing) | `couldn't connect to tymuxd — is the daemon running?` |
| Bearer-token rejected (existing, non-loopback) | `tymuxd rejected this connection: missing\|invalid bearer token ...` |
| UDS peer-cred rejected (new, case 3) | `tymuxd rejected this connection: not authorized to access this daemon's socket ...` |
| Other RPC error (existing) | raw `status.message()`, e.g. `no such session: abc` |
| Daemon-side socket-path failure (new, case 4) | `failed to create Unix socket at {path}: ...` — never reaches a client, appears only in `tymuxd`'s own stderr |

Reusing the exact `tymuxd rejected this connection: ` prefix for both the
bearer-token and peer-cred rejection cases keeps them visually grouped as
"the daemon said no to *me specifically*" (as opposed to "I couldn't reach
it" or "that request itself was invalid") while the differing text after the
colon keeps them individually actionable — consistent with this task's
requirement to distinguish without multiplying near-identical variants.

## 5. Jobs-to-be-done

- **Functional job**: isolate my terminal sessions from other local users on
  a host I don't fully control (a shared dev box, a shared server, a CI
  runner with other tenants) — without having to configure per-user tokens,
  run a reverse proxy, or otherwise leave the "just run `tymuxd`" workflow
  `bearer-token-auth`'s own research already established as the baseline
  expectation (`project_plans/bearer-token-auth/research/ux.md` §4, "the
  common case must stay invisible").
- **Emotional job**: confidence that "my terminal sessions are mine" — but
  per §2, that confidence must be *earned correctly*, not just felt. A
  feature that makes an admin feel safe without them understanding that TCP
  loopback remains an open door produces false confidence, which is worse
  for this job than no feature at all (a user who correctly believes they
  have zero isolation makes different, safer decisions than one who
  incorrectly believes they have full isolation). The UX obligation this
  creates: the feature's own announcement of itself (log line, docs, `gh pr`
  description when this ships) has to carry the caveat in the same breath as
  the capability, not as a footnote.
- **Social job**: this feature's existence — specifically, kernel-verified
  peer identity via `SO_PEERCRED` rather than a bolt-on shared secret — is
  itself evidence to an admin evaluating tymux for a shared/multi-user
  deployment that the maintainer takes local-multi-tenant isolation
  seriously, distinct from and stronger than what `bearer-token-auth` alone
  signals (a shared secret is copyable between users trivially; a uid is
  not — this project's own requirements.md "Alternatives Considered" section
  makes this exact comparison). That signal is only honest if the
  accompanying documentation and warning text state the current boundary of
  that guarantee precisely (UDS is isolated; TCP loopback, still active, is
  not) — an admin who reads only a marketing-style "now with local user
  isolation!" claim and doesn't also see the TCP caveat is being sold a
  stronger guarantee than what ships in this project's scope.
