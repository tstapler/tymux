# UX Design: bearer-token-auth

**Date**: 2026-08-27 (Gap 1 status updated 2026-08-28 after Phase 4
validation fed back into `plan.md`; `plan.md` line-number anchors below
predate that revision and may be off by ~100-250 lines — locate by task
number, not line number, if a link is stale)
**Scope**: Condensed treatment per the ux-design-prompt's own guidance — every
surface in this feature is non-interactive CLI/daemon text output (nothing
clicked or typed into beyond ordinary flags), so this doc gives one
representative sample plus 3-5 acceptance-criteria bullets per surface
instead of wireframes or interaction-flow diagrams. Builds directly on
`research/ux.md` (comparable-pattern research, message wording, the
`friendly_message` precedent) and is checked against what
`implementation/plan.md` actually specifies, not a generic ideal.

---

## Surface 1 — `tymuxd` startup failure (non-loopback, no token)

**Story**: 1.1.3 (`check_non_loopback_requires_token`, `implementation/plan.md`
[Task 1.1.3a](../implementation/plan.md#L323-L345))

Representative output (`tymuxd` exits non-zero, prints via an explicit
`eprintln!` + `std::process::exit(1)`, **not** propagated through `main()`'s
`Result<(), Box<dyn Error>>` return — see the "Corrected 2026-08-28" note
below):

```
$ tymuxd --addr 0.0.0.0:7419
Error: failed to start: bound to non-loopback address with no token configured.
Set --token or TYMUXD_TOKEN before binding tymuxd to a non-loopback address — this port would otherwise let any network client run arbitrary commands.
(Loopback binds, e.g. 127.0.0.1, never require a token. Generate one with `openssl rand -hex 32` if you don't already have one.) (bind address: 0.0.0.0:7419)
```

**Corrected 2026-08-28 (triad review, Engineering/UX blocker)**: this
surface originally assumed the message would reach stderr via `main()`'s
`?`-propagated `Result<(), Box<dyn Error>>` return, matching the existing
`sessions_dir` precedent (`research/ux.md` §2). Empirically verified this
is wrong: Rust's default `Termination` impl reports a returned `Err` via
its `Debug` formatting, not `Display` — a `String` error propagated this
way prints as `Error: "the message\nwith literal backslash-n text and
quotes"`, exactly the Debug-dump AC4 below says must not happen (confirmed
by compiling and running the exact pattern standalone). Task 1.1.3b now
uses `eprintln!("Error: {e} ...")` + `std::process::exit(1)` directly
instead of `?`, which produces the clean, real-newline output shown above.
The pre-existing `sessions_dir` precedent (`main.rs:1254-1259`) has this
same latent bug — it's just far less visible there because that message
has no embedded newline (single line, only the surrounding quotes would
show). Not fixed here (out of this feature's scope), but not repeated
either.

Acceptance criteria:
- Fails fast, before any disk I/O: the process exits ahead of the
  `sessions_dir` prep step (plan's Task 1.1.3b ordering) — an operator never
  waits through session-loading only to be rejected at the end.
- Names both remedies (`--token` and `TYMUXD_TOKEN`), the concrete
  consequence of skipping them ("any network client run arbitrary
  commands"), and a concrete way to generate one (`openssl rand -hex 32`)
  — not just "config value missing" — matches the risk-forward register
  `research/ux.md` §4 calls for.
- States the exemption plainly ("Loopback binds ... never require a token")
  so an operator debugging this on a loopback deploy immediately knows the
  message doesn't apply to them.
- Prints as one clean block of real text via explicit `eprintln!` +
  `process::exit(1)` (Task 1.1.3b) — never a `Debug` dump of a boxed
  error (see the correction note above for why `main()`'s own `?`-return
  path was rejected for this specific message).
- Exit path: the message itself *is* the fix (two named flags plus a
  generation command) — no
  follow-up command needed, unlike Surface 4 below.

## Surface 2 — `tymuxd` startup success logging

**Story**: 1.1.3 (`implementation/plan.md` [Task 1.1.3b](../implementation/plan.md#L347-L370))

Representative output, two branches:

```
# non-loopback + valid token
WARN tymuxd: tymuxd is binding to a non-loopback address; bearer-token auth is enforced on every call socket_addr=0.0.0.0:7419

# loopback (today's default) — now also carries a reverse-proxy/tunnel
# caveat, added post-validation per pre-mortem.md P1 #1
INFO tymuxd: tymuxd binding to loopback; no auth required (if this daemon is reachable through a reverse proxy or tunnel, loopback auto-exemption does not protect you — bind non-loopback and set --token/TYMUXD_TOKEN instead) socket_addr=127.0.0.1:7419
```

Acceptance criteria:
- Loopback case logs at `info`, not `warn` — this is the normal, unchanged
  path and must not read as a problem (plan explicitly keeps this branch
  "exactly as it does today").
- Non-loopback case keeps `warn` level and states the *consequence*
  ("bearer-token auth is enforced on every call"), reusing the existing
  non-loopback warning's risk-forward register rather than a neutral
  "starting in non-loopback mode."
- Neither branch logs the token value or any derived material — confirmed
  against Task 1.2.1c's grep check and the Observability Requirements'
  "never a `tracing` field at any level."
- The two branches are the only startup-time signal; no separate "auth
  disabled"/"auth enabled" banner is introduced beyond this one line, per
  `research/ux.md` §4's "state the risk once, not repeatedly."

## Surface 3 — `tymux-cli --token` flag / `TYMUXD_TOKEN`

**Story**: 2.1.1 (`implementation/plan.md` [Task 2.1.1a](../implementation/plan.md#L701-L707))

Representative `--help` excerpt, **as currently planned** (no doc comment on
the `token` field in Task 2.1.1a — see Gap 1 below):

```
$ tymux --help
...
    --addr <ADDR>      [default: http://127.0.0.1:7419]
    --token <TOKEN>    [env: TYMUXD_TOKEN=]
    --no-status-bar     Disable the status bar entirely...
...
```

Acceptance criteria:
- `--help` shows the flag *and* its env fallback (clap's `env = "..."`
  auto-annotates `[env: TYMUXD_TOKEN=]` in the help output) — a user
  scanning `--help` can discover the env-var path without reading docs.
- Flag-beats-env precedence (Story 2.1.1 AC3) needs no extra UX surface —
  clap's built-in precedence matches ordinary CLI-flag-beats-environment
  expectations, nothing to document beyond the auto-generated `[env: ...]`
  annotation.
- **Resolved**: `--help` now shows a token-generation pointer
  (`openssl rand -hex 32`) via the `token` field's doc comment — see
  Gap 1 in "Gaps found" below (fixed 2026-08-28, after this surface was
  originally designed).
- Consistent with Surface 1/4's terminology: the flag is `--token`, the
  env var is `TYMUXD_TOKEN`, and both error surfaces (1 and 4) name these
  two exact identifiers — no alternate spelling (`--auth-token`,
  `TYMUX_TOKEN`) introduced anywhere in the plan.

## Surface 4 — `tymux-cli` rejected-connection error message

**Story**: 2.2.1 (`implementation/plan.md` [Task 2.2.1a](../implementation/plan.md#L819-L833))

Representative output:

```
$ tymux ls
Error: tymuxd rejected this connection: missing bearer token (set --token or TYMUXD_TOKEN to authenticate)
```

```
$ tymux ls --token wrongvalue
Error: tymuxd rejected this connection: invalid bearer token (set --token or TYMUXD_TOKEN to authenticate)
```

Acceptance criteria:
- Structurally distinct from the two adjacent failure modes at a glance:
  "couldn't connect to tymuxd — is the daemon running?" (unreachable,
  existing) vs. "tymuxd rejected this connection: ..." (this feature) vs.
  raw `status.message()` passthrough for other RPC errors (e.g.
  `no such session: abc`) — three different opening clauses, not three
  variations on one sentence (`research/ux.md` §3 table).
- Names the remedy inline every time: `(set --token or TYMUXD_TOKEN to
  authenticate)` is appended regardless of missing-vs-invalid, so the exit
  path is present even though the underlying reason (missing vs. wrong)
  varies.
- Other status codes are provably unaffected (Story 2.2.1 AC2: `not_found`
  still returns exactly `"no such session: abc"`, no wrapper text) — this
  feature's new branch doesn't leak into unrelated error paths.
- No dead end: the message both states what happened ("rejected this
  connection") and what to do next ("set --token or TYMUXD_TOKEN") in the
  same line — a user never has to guess or search docs for the fix.

## Surface 5 — Operator-facing token-generation doc moment

**Flagged by**: `research/build-vs-buy.md` §3 ("point operators at `openssl
rand -hex 32` ... in the `--token` flag's `--help` text and/or README").

Checked against `implementation/plan.md` directly (`grep -rn "openssl" plan.md
research/` finds it only in `research/build-vs-buy.md`, never in `plan.md`).
**Confirmed missing**: no story or task in `plan.md` adds this guidance
anywhere — not as `--help` text (Task 2.1.1a's `token` field has no `///`
doc comment, unlike the `no_status_bar` field two lines above it in the same
struct, which does), and not as a README task in any epic. See Gap 1.

Acceptance criteria (for what *should* exist, per the research
recommendation — not yet in plan.md):
- A first-time operator binding `tymuxd` non-loopback can find a concrete
  token-generation command (`openssl rand -hex 32` or equivalent) without
  leaving `--help` or the README — mirrors the Jupyter precedent
  `research/ux.md` §1 cites approvingly (never leaves the user guessing
  *that* a token is needed or *how* to make one).
- Placement matches this repo's existing doc-comment-as-help-text
  convention (a `///` line above `#[arg(long, ...)]`, as `no_status_bar`
  already demonstrates), not a new documentation mechanism.

---

## UX Acceptance Criteria (cross-surface)

1. **No dead ends** — every error state names the exit path in the same
   message as the problem:
   - Surface 1 (startup failure): pass — names `--token`/`TYMUXD_TOKEN`
     inline.
   - Surface 4 (rejected connection): pass — appends `(set --token or
     TYMUXD_TOKEN to authenticate)` on every `Unauthenticated` branch.
   - Surface 3 (`--help`): gap — no generation guidance (Gap 1).
2. **Terminology consistency between tymuxd-side and tymux-cli-side
   messages** — checked directly against plan.md's proposed strings:
   - tymuxd (Surface 1, 2): "token configured" / "bearer-token auth" /
     "missing bearer token" / "invalid bearer token".
   - tymux-cli (Surface 4): relays the server's own `status.message()`
     verbatim ("missing bearer token" / "invalid bearer token"), wrapped
     with "(set --token or TYMUXD_TOKEN to authenticate)".
   - **Consistent**: both sides say "bearer token" / "token" — never a mix
     of "bearer token" vs. "auth token" vs. "credential". Flag/env-var
     names (`--token`, `TYMUXD_TOKEN`) are identical strings on both sides
     (Story 1.1.2's `resolve_token` and Story 2.1.1's clap field use the
     same two identifiers).
3. **Status-code correctness as the primary distinguishing signal** —
   `tonic::Code::Unauthenticated` (not `PermissionDenied`) is used
   end-to-end (Domain Glossary), so a programmatic client (`clients/go`,
   `clients/ts`) can branch on the code without parsing prose — text is a
   human-readable second layer, not the only signal (mirrors
   `research/ux.md` §1's `redis-cli`/`curl` precedent).
4. **Loopback path is provably silent** — Surface 2's loopback branch is
   unchanged from today's log line, Surface 3's `--token`/`TYMUXD_TOKEN`
   are both optional with no default requiring them, and no new prompt,
   flag requirement, or message appears anywhere in a loopback session
   (Story 1.2.2 AC4, Story 2.1.2 AC2). This is the feature's primary
   backward-compatibility UX contract, not just a functional one.
5. **Risk stated once, not repeated** — the "arbitrary commands" /
   "run arbitrary commands" risk framing appears at the two failure points
   (Surface 1 at startup, implicitly reinforced by Surface 2's non-loopback
   warning) and is never re-stated on every successful authenticated call
   — a correctly-authenticated non-loopback session is indistinguishable in
   tone from a loopback one after the token is accepted (`research/ux.md`
   §4).
6. **Three-way error distinguishability holds under the plan's actual
   strings** — re-verified directly against Task 2.2.1a's code (not just
   research/ux.md's proposal): unreachable → "couldn't connect to tymuxd —
   is the daemon running?"; auth-rejected → "tymuxd rejected this
   connection: ..."; other RPC error → raw `status.message()` with no
   wrapper. Three distinct opening clauses, confirmed in the implementation
   task, not just the research doc.

## Accessibility

N/A — CLI/daemon, no GUI; confirmed no color-only-signal risk. Verified
directly against `implementation/plan.md`'s actual code for this feature
(not just re-asserting `research/ux.md` §5's prior grep): none of Tasks
1.1.3a, 1.2.1a, 2.1.1a, or 2.2.1a introduce `colored`/`owo_colors`/
`termcolor`/ANSI escapes or any color-coded distinction between the three
error classes in Surface 4 — the distinguishing signal is message text plus
the underlying `tonic::Code`, both of which are equally legible to a
plain-text terminal, a log aggregator, and a screen reader alike.

## Gaps found

**Gap 1 — RESOLVED 2026-08-28.** Originally: token-generation guidance
(`openssl rand -hex 32`, per `research/build-vs-buy.md` §3) was missing
from `plan.md` entirely. Closed in two steps: the Phase 3 repair pass
added it to `tymux-cli`'s `--token` field doc comment (now Task 2.1.1b)
and `tymuxd`'s `resolve_token` doc comment (Task 1.1.2c); Phase 4
validation then found the doc-comment-only fix still left `tymuxd`'s own
user-visible failure surface — its non-loopback-no-token startup error
(Task 1.1.3a) — without the hint, since `tymuxd` has no `--help` to
render a doc comment into. That residual half was closed by appending
the hint directly to Task 1.1.3a's error message text. Both the
`tymux-cli`-side and `tymuxd`-side surfaces now carry the guidance.

No other mismatches found between `research/ux.md`'s proposed message
wording and `plan.md`'s actual implementation: the startup-failure message
(Task 1.1.3a), the interceptor's rejection strings (Task 1.2.1a), and
`friendly_message`'s new branch (Task 2.2.1a) all match `research/ux.md`
§2/§3's proposed text verbatim or near-verbatim (the plan appends `(bind
address: {socket_addr})` to the startup message via Task 1.1.3b's
`.map_err`, which research/ux.md doesn't show but which is additive
context, not a contradiction).
