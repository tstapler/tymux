# UX Design: unix-socket-auth

**Date**: 2026-08-29
**Scope**: Condensed treatment, matching `project_plans/bearer-token-auth/design/ux.md`'s
own precedent — every surface in this feature is non-interactive CLI/daemon
text output (flags, env vars, log lines, error messages; nothing clicked or
typed into beyond ordinary flags), so this doc gives one representative
output sample plus 3-5 acceptance-criteria bullets per surface instead of
wireframes or interaction-flow diagrams. All wording below is quoted
verbatim from `implementation/plan.md`'s actual tasks (not re-derived from
`research/ux.md`'s illustrative drafts) — where the two differ, `plan.md`
wins, since it is what ships. Flag names, env vars, and status codes
(`--socket-path`/`TYMUXD_SOCKET_PATH`, `--socket-group`/
`TYMUXD_SOCKET_GROUP`, `--disable-tcp-loopback`/
`TYMUXD_DISABLE_TCP_LOOPBACK`, `tonic::Code::PermissionDenied`) match
`plan.md`'s Domain Glossary and Pattern Decisions exactly.

---

## Surface 1 — `tymuxd` startup: both listeners active (default, zero-config case)

**Story**: 4.2.2 ([Task 4.2.2a](../implementation/plan.md#L1121-L1201)), 4.3.1

Representative output:

```
$ tymuxd
INFO tymuxd: tymuxd listening socket_addr=127.0.0.1:7419 uds_path=/run/user/1000/tymuxd/tymuxd.sock
WARN tymuxd: tymuxd's TCP listener (127.0.0.1:7419) is deprecated and will be removed in a future release; it accepts connections from any local process with no credential check, regardless of the new Unix-socket listener at /run/user/1000/tymuxd/tymuxd.sock. Other local users are isolated only if nothing on this host still connects over TCP — set --disable-tcp-loopback/TYMUXD_DISABLE_TCP_LOOPBACK=1 once your clients have migrated to the Unix socket. socket_addr=127.0.0.1:7419 uds_path=/run/user/1000/tymuxd/tymuxd.sock
```

Acceptance criteria:
- The deprecation warning states the "additive, not a replacement" caveat
  in the *same sentence* as the deprecation notice, not a footnote —
  verified directly against Task 4.2.2a's literal string: "it accepts
  connections from any local process with no credential check, regardless
  of the new Unix-socket listener" appears in the same `tracing::warn!`
  call as "is deprecated and will be removed."
- Names the concrete off-switch (`--disable-tcp-loopback`/
  `TYMUXD_DISABLE_TCP_LOOPBACK=1`) inline — an operator reading this line
  never has to search docs for the remedy.
- Fires at `warn` level specifically (Story 4.3.1 AC1), surviving the
  default `EnvFilter::new("info")` — a single-user operator who never
  reads warnings still sees the daemon start silently otherwise (this line
  is the one exception to the "silent for the common case" rule, and it's
  deliberate: per `research/ux.md` §2/§5, staying silent about this
  specific gap would be the worse outcome).
- Fires once per startup, not per-connection (Observability Requirements)
  — an operator's log volume is unaffected by client traffic.
- A single-user-machine operator sees no *other* behavior change: same
  commands, same `ListSessions`/`Attach` output, no new required flag
  (requirements.md's "zero required config change" success metric).

## Surface 2 — `tymuxd` startup: `--disable-tcp-loopback` (UDS-only)

**Story**: 4.2.2, 4.3.1 ([Task 4.2.2a](../implementation/plan.md#L1129-L1130))

Representative output:

```
$ tymuxd --disable-tcp-loopback
INFO tymuxd: tymuxd listening socket_addr=127.0.0.1:7419 uds_path=/run/user/1000/tymuxd/tymuxd.sock
INFO tymuxd: TCP loopback listener disabled via --disable-tcp-loopback/TYMUXD_DISABLE_TCP_LOOPBACK
```

Acceptance criteria:
- Logs at `info`, not `warn` — this is an operator's deliberate,
  already-informed choice, not a problem to flag (mirrors
  `bearer-token-auth` Surface 2's loopback-vs-non-loopback level
  precedent: the safer configuration logs quieter, not louder).
- The warning from Surface 1 does not also fire — Story 4.3.1 AC2 requires
  these two log lines to be mutually exclusive, not additive, so an
  operator never sees a contradictory "deprecated but here's the flag to
  disable it" line right after they've already used that flag.
- Names the flag that produced this state literally
  (`--disable-tcp-loopback`/`TYMUXD_DISABLE_TCP_LOOPBACK`) so a log reader
  can confirm which mechanism (flag vs. env var) is in effect.

## Surface 3 — `tymuxd` startup failure: UDS socket path unwritable

**Story**: 4.2.1 ([Task 4.2.1a](../implementation/plan.md#L1068-L1075))

Representative output:

```
$ TYMUXD_SOCKET_PATH=/nonexistent-root-owned-path/tymuxd.sock tymuxd
Error: failed to create Unix socket at /nonexistent-root-owned-path/tymuxd.sock: Permission denied (os error 13). Check that the parent directory exists and is writable, or override the path with --socket-path/TYMUXD_SOCKET_PATH.
```
(exit code 1)

Acceptance criteria:
- Fails fast: this happens during startup's socket-bind sequence, before
  the TCP listener is ever spawned (Task 4.2.1a's ordering) — never a
  partially-up daemon serving TCP but silently missing UDS.
- Names the concrete remedy inline (`--socket-path`/`TYMUXD_SOCKET_PATH`)
  — matches Surface 1 of `bearer-token-auth`'s own "state both the flag
  and the env var, plus the fix" register.
- Prints as clean literal text via `eprintln!` + `process::exit(1)`
  (matching `bearer-token-auth`'s Surface 1 correction: never a `Debug`-
  dump of a boxed/propagated `Result` error).
- This is a **daemon-side, not client-visible** failure — a `tymux-cli`
  user pointed at this daemon sees Surface 8 ("couldn't connect... is the
  daemon running?"), which is the correct framing here: the daemon
  genuinely isn't running.

## Surface 4 — `tymuxd` startup failure: unknown `--socket-group`

**Story**: 4.2.1 ([Task 4.2.1a](../implementation/plan.md#L1050-L1056))

Representative output:

```
$ tymuxd --socket-group tyypo-group
Error: --socket-group/TYMUXD_SOCKET_GROUP names an unknown group: tyypo-group
```
(exit code 1)

Acceptance criteria:
- Fails loudly, never silently falls back to owner-only — Story 1.2.1's
  explicit design goal ("a typo doesn't silently leave the socket
  owner-only when I believed I'd granted team access"). An operator who
  typos a group name gets an error, not a quieter-than-expected socket.
- Names both spellings (`--socket-group` and `TYMUXD_SOCKET_GROUP`) so the
  operator can tell which source produced the bad value without having to
  check both.
- Echoes the exact string that failed to resolve (`tyypo-group`) — no
  generic "invalid group" message that would make a typo hard to spot.

## Surface 5 — `tymuxd` startup: concurrent-start / stale-socket races

**Story**: 2.1.1, 2.1.2 ([acquire_socket_lock](../implementation/plan.md#L553-L565), [reconcile_stale_socket](../implementation/plan.md#L612-L627))

Representative output, two distinct cases:

```
# Case A: a second tymuxd starts while the first is still mid-startup
Error: another tymuxd is already starting against /run/user/1000/tymuxd/tymuxd.sock (lock file: /run/user/1000/tymuxd/tymuxd.sock.lock)

# Case B: a second tymuxd starts while a first is already fully up and listening
Error: tymuxd is already running — a live listener answered at /run/user/1000/tymuxd/tymuxd.sock
```
(both exit code 1; a genuinely stale file from an unclean prior exit is
removed silently — no message, since it's not an error condition, matching
`reconcile_stale_socket`'s no-op-success AC)

Acceptance criteria:
- The two live-conflict cases (racing-startup vs. already-running) get
  textually distinct messages, so an operator debugging "why won't tymuxd
  start" can tell a genuine double-start from a stuck lock apart at a
  glance.
- A stale socket (crashed daemon, no live listener) is reconciled with
  **no** user-visible message at all — silently removed and replaced, per
  Story 2.1.2's AC — an operator restarting a crashed `tymuxd` sees a
  normal, unremarkable startup, not a scary-looking recovery notice for a
  routine, expected condition.
- Both live-conflict messages name the exact socket path involved, so an
  operator running multiple `tymuxd` instances (e.g. via distinct
  `--socket-path` values) can tell which instance is which.

## Surface 6 — client baseline: UDS-first connection, silent success

**Story**: 6.2.1 ([dial_channel](../implementation/plan.md#L1519-L1544)), 7.x, 8.x (parity across `tymux-cli`/`clients/go`/`clients/ts`)

Representative output:

```
$ tymux ls
default   1 window
```
(no socket-path, no auth mention, no warning — identical output to
pre-feature `tymux ls` on a loopback-only daemon)

Acceptance criteria:
- Zero new output on the success path — the UDS dial, its path resolution,
  and the peer-cred check it implicitly satisfies are all invisible unless
  something goes wrong (requirements.md's "zero required config change"
  success metric; `research/ux.md` §3's "the default-path success case
  never mentions these terms at all").
- Identical behavior whether invoked from an interactive shell or a script
  — `dial_channel`'s branch is selected purely by `--addr`
  presence/absence and socket reachability, never by `isatty()`
  (`research/ux.md` §3 point 2; confirmed no `is_terminal`/`isatty` call
  anywhere in `dial_channel`'s Task 6.2.1b implementation).
- `tymux-cli`, `clients/go`, `clients/ts` all resolve the identical default
  socket path given the identical environment (`XDG_RUNTIME_DIR`/`TMPDIR`)
  — the mirrored-algorithm requirement (Pattern Decisions row 10) means a
  CI job and an interactive shell on the same host never disagree about
  which socket to dial.

## Surface 7 — client: TCP fallback with logged notice

**Story**: 6.2.1 ([Task 6.2.1b](../implementation/plan.md#L1528-L1544))

Representative output:

```
$ tymux ls
tymux: no reachable Unix socket at /run/user/1000/tymuxd/tymuxd.sock — falling back to TCP loopback (deprecated; make sure tymuxd is running)
default   1 window
```

Acceptance criteria:
- Exactly one line, on every invocation that falls back — not silent
  (`research/ux.md` §2/§4 case 5's explicit flag: "silent, permanent
  fallback to the weaker transport is the single worst outcome this
  feature could produce"), and not repeated per-RPC within one invocation.
- Names the concrete path that was tried (`/run/user/1000/tymuxd/tymuxd.sock`),
  so an operator debugging "why is my client on the deprecated transport"
  has the exact value to check against the daemon's own logged `uds_path`.
- Uses the word "deprecated" — ties this client-side notice back to
  Surface 1's daemon-side deprecation framing, so the two surfaces read as
  one coherent story rather than two unrelated warnings.
- The command still succeeds (exit 0, normal output follows) — the
  fallback is a degrade, not a failure; this is a deliberate
  backward-compatibility choice, not an accidental one.
- Fires identically under a piped/non-interactive invocation as an
  interactive one (same `isatty()`-independence requirement as Surface 6).

## Surface 8 — client: daemon unreachable (existing message, unchanged, now also covers stale-socket)

**Story**: `research/ux.md` §4 case 1/2 (no new plan.md task — explicitly confirmed as "no new case needed")

Representative output:

```
$ tymux ls
tymux: couldn't connect to tymuxd — is the daemon running? (start it with `cargo run -p tymuxd`)
```

Acceptance criteria:
- Unchanged text, unchanged trigger condition scope: this single message
  now also covers "socket file present but stale" (case 2) in addition to
  "nothing running at all" (case 1) — deliberately *not* split into two
  near-identical variants, since the client cannot reliably distinguish
  them from the outside and both have the same remedy ("start it").
- Distinct opening clause from Surface 9 (peer-cred rejection) and the
  bearer-token-rejection message — "couldn't connect" (never reached the
  daemon) vs. "tymuxd rejected this connection" (reached the daemon, it
  said no) stay visually and semantically separate at a glance.
- Also covers Surface 3's daemon-side bind failure from the client's point
  of view: a `tymuxd` that failed to start (Surface 3) is, correctly,
  indistinguishable from "not running" here — the client never sees
  Surface 3's message directly.

## Surface 9 — client: UDS peer-cred rejection (`PermissionDenied`)

**Story**: 6.3.1 ([Task 6.3.1a](../implementation/plan.md#L1574-L1577))

Representative output:

```
$ tymux ls
tymux: tymuxd rejected this connection: not authorized to access this daemon's socket (ask the daemon's owner to add you to its configured --socket-group, or run tymux-cli as the daemon's own OS user)
```

Acceptance criteria:
- Exact text matches Task 6.3.1a's AC verbatim — this is a Gate-tested
  string (`friendly_message_names_the_remedy_for_permission_denied_status`),
  not an approximation.
- Shares the `tymuxd rejected this connection: ` prefix with the existing
  bearer-token `Unauthenticated` rejection message, grouping both as "the
  daemon actively said no to *me*" — but the text after the colon and the
  underlying `tonic::Code` differ (`PermissionDenied` vs.
  `Unauthenticated`), so a scripted caller can branch on the status code
  alone without parsing prose (`plan.md`'s own reasoning for choosing
  `PermissionDenied` over reusing `Unauthenticated`).
- Names the remedy without requiring jargon the user must first decode:
  "ask the daemon's owner to add you to its configured --socket-group, or
  run tymux-cli as the daemon's own OS user" — no mention of `SO_PEERCRED`,
  "peer credential", or raw uid/gid numbers in the printed message itself
  (`research/ux.md` §3's "plain language over jargon" — those numbers are
  logged server-side per Surface 11, not surfaced to the rejected client).
- The Deployment Guidance's containerized/namespaced-uid caveat
  ("a containerized client sees its host-mapped uid, which may not match
  `id -u` inside the container") is deliberately **not** in the printed
  message — it lives as a doc comment above the `friendly_message` branch
  (Task 6.3.1a) instead, per `research/ux.md`'s "not a wall of
  near-identical variants" guidance. See Gap 1 below: this means an
  affected container user gets the same generic remedy text as everyone
  else, with no in-message hint that their case is different.

## Surface 10 — New-flag discoverability: `--socket-path`, `--socket-group`, `--disable-tcp-loopback`

**Story**: Epics 1.1-1.3 (`tymuxd`, hand-rolled `std::env::args()` scan), Epic 6.1 (`tymux-cli`, ordinary `clap` field — [Task 6.1.1c](../implementation/plan.md#L2031), matching `--token`'s exact shape; see Pattern Decisions row "`tymux-cli` new-flag mechanism")

Representative output — what a user actually sees when trying to discover
these flags:

```
$ tymuxd --help
# (no output about --help at all: the hand-rolled std::env::args() scan
#  only ever looks for its own known flag strings — it has no unknown-
#  flag rejection path, so --help is silently ignored as noise and the
#  daemon just starts normally, listening on both transports. Matches
#  bearer-token-auth's own --token/--addr precedent exactly — no
#  regression here, but no improvement either.)

$ tymux --help
...
    --addr <ADDR>
    --token <TOKEN>              [env: TYMUXD_TOKEN=]
    --socket-path <SOCKET_PATH>  [env: TYMUXD_SOCKET_PATH=]
    --no-status-bar
...
# --socket-group, --disable-tcp-loopback do NOT appear here — they are
# tymuxd-only bind-time flags with no client-side clap field at all (no
# tymux-cli equivalent exists for either, so there's nothing for --help
# to render). --socket-path DOES now appear, closing what was this
# doc's own "Gap 2" — see below.
```

Acceptance criteria:
- On `tymuxd`: no regression from today — `tymuxd` has never had a
  `--help` (bearer-token-auth's own `--token`/`--addr` predate a
  `clap`/hand-rolled split, and `Pattern Decisions` row 8 deliberately
  keeps `tymuxd` dependency-light rather than adding `clap`). `--socket-group`
  and `--disable-tcp-loopback` are discoverable only via documentation
  (README/CHANGELOG), never via `--help`, matching the existing
  `--token`/`--addr` status quo exactly — not a new gap this feature
  introduces, but not closed by it either.
- On `tymux-cli`: **`--socket-path` is now `--help`-discoverable**,
  closing the regression an earlier draft of this plan would have
  introduced relative to this same feature's own `--token` precedent —
  `--socket-path` is an ordinary `clap` field (Task 6.1.1c) that renders
  via the identical `[env: TYMUXD_SOCKET_PATH=]` auto-annotation
  `--token` already uses (`bearer-token-auth` design doc, Surface 3), and
  Task 6.1.1d asserts this directly against captured `--help` output.
  `--socket-group`/`--disable-tcp-loopback` remain non-discoverable via
  `tymux --help`, but this is not an asymmetry this feature introduces —
  neither flag has a `tymux-cli`-side `clap` field to render in the first
  place, since both are `tymuxd`-only bind-time configuration with no
  client-side analogue (Pattern Decisions row scoping `--socket-path` as
  "only" flag with a client mirror).
- Whatever documentation *does* exist for `--socket-group`/
  `--disable-tcp-loopback` (README/CHANGELOG per Surface 11) uses the
  exact flag/env spellings this doc uses — no alternate spelling
  introduced anywhere.

## Surface 11 — Operator-facing doc/deployment caveats (non-interactive)

**Flagged by**: `implementation/plan.md`'s own Deployment Guidance section
(lines 156-186)

Four caveats `plan.md` requires to exist *somewhere* operator-visible, each
checked against where the plan actually places it:

| Caveat | Plan's placement | Verified present in a user-visible surface? |
|---|---|---|
| TCP loopback stays fully unauthenticated by design | `tracing::warn!` startup line (Surface 1) | Yes — literal text in Task 4.2.2a |
| `--socket-group` grants full daemon control, not a scoped subset | `tracing::info!` startup line, conditional on `--socket-group` being configured (Task 4.3.2a), *and* `resolve_socket_group_name`'s Rust doc comment (Task 1.2.1a) | **Yes** — closes Gap 1 for this caveat |
| Containerized/bind-mounted-socket clients see their host-mapped uid | `friendly_message`'s doc comment above the `PermissionDenied` branch (Task 6.3.1a), and now also README's "Multi-user / shared-host deployment" section (Task 9.1.1a) | **Yes** — closes Gap 1 for this caveat (was open, now tracked/closed by Task 9.1.1a) |
| macOS/BSD `--socket-group` is primary-gid-only, not full supplementary-group parity | Same conditional `tracing::info!` line as row 2 (Task 4.3.2a), *and* ADR-002 + the flag's own doc comment (Task 1.2.1a, `tymux-cli` mirror) | **Yes** — closes Gap 1 for this caveat |

Acceptance criteria:
- At minimum, the TCP-loopback caveat (row 1) is present in the one
  surface every operator sees unconditionally: the startup log (Surface
  1) — confirmed satisfied.
- The `--socket-group`-related caveats (rows 2 and 4) are now present in
  an operator-reachable surface: a startup log line, conditioned on
  `--socket-group`/`TYMUXD_SOCKET_GROUP` actually being set (Task
  4.3.2a) — this is Gap 1's own recommended fix, now picked up by a
  plan.md task and combined with pre-mortem.md's P2 #4 finding (the same
  gap, flagged independently), so an operator who configures group
  access sees both caveats the moment that configuration takes effect,
  not only in `--help` text they may never read.
- The containerized/bind-mounted-socket uid-mismatch caveat (row 3) is
  genuinely lower-frequency than the other two (it only matters to a
  containerized deployment, not every `--socket-group` user), so
  doc-comment-plus-README is the right placement per Gap 1's original
  recommendation — now tracked and closed by `implementation/plan.md`'s
  **Task 9.1.1a**, which adds the README section explicitly. This was
  this design's one remaining open finding; see Gaps found for the
  resolution.

---

## UX Acceptance Criteria (cross-surface)

1. **No dead ends** — every error state names its own exit path in the
   same message as the problem:
   - Surface 3 (UDS bind failure): pass — names `--socket-path`/
     `TYMUXD_SOCKET_PATH` inline.
   - Surface 4 (unknown group): pass — echoes the bad value and both flag
     spellings.
   - Surface 5 (lock/stale races): pass for the two live-conflict cases
     (each names the path in conflict); the stale-reconcile case correctly
     has *no* message because it isn't an error the user needs to act on.
   - Surface 8 (unreachable): pass — names the exact start command.
   - Surface 9 (peer-cred rejected): pass — names the two concrete
     remedies (`--socket-group` membership or run as the daemon's own
     user).
2. **Terminology consistency across daemon and client** — checked directly
   against `plan.md`'s proposed strings, not research's illustrative
   draft: `tymuxd`-side messages say "Unix socket"/"socket path"; the
   client-side rejection and fallback messages also say "Unix socket" (not
   a mix with "UDS socket"/"domain socket") — confirmed no alternate
   spelling appears in any of Tasks 1.1.1a-4.2.2a, 6.2.1b, or 6.3.1a's
   literal strings.
3. **Status-code correctness as the primary machine-readable signal** —
   `tonic::Code::PermissionDenied` (peer-cred reject) is provably distinct
   from `tonic::Code::Unauthenticated` (bearer-token reject) at the status
   level, so `clients/go`/`clients/ts` can branch without parsing prose —
   mirrors `bearer-token-auth`'s own AC3 and is the specific reason
   `plan.md`'s Pattern Decisions table rejected reusing `Unauthenticated`
   for this feature.
4. **Baseline path is provably silent** — Surface 6's zero-output success
   case, combined with requirements.md's "zero required config change"
   metric: a single-user-machine operator's `tymux ls` output, exit code,
   and timing are unaffected by this feature shipping. This is the
   feature's primary backward-compatibility contract.
5. **The "both-by-default reads as fully isolated" risk is addressed at
   the one surface every operator sees** — Surface 1's warning states the
   caveat in the same sentence as the capability announcement (not a
   footnote), satisfying `research/ux.md` §2/§5's explicitly-flagged
   mental-model risk. Verified against the literal warning string, not an
   intended paraphrase.
6. **Scriptability: no `isatty()`-conditional transport selection** —
   Surfaces 6 and 7's UDS-first/TCP-fallback branch is driven solely by
   `--addr` presence and socket reachability; confirmed no
   `is_terminal`/`isatty`/`IsTerminal` call anywhere in `dial_channel`
   (Task 6.2.1b) or its `tymuxd`-side counterpart. A cron job, CI runner,
   or `su -c` invocation resolves the identical transport an interactive
   shell would, given the identical environment — directly addressing
   `research/ux.md` §3's flagged `$XDG_RUNTIME_DIR`-under-cron risk.
7. **No color-only signaling** — every distinguishing signal across all 11
   surfaces is message text and/or `tonic::Code`, never color. Confirmed:
   none of the new code introduced by Tasks 1.1.1a through 6.3.1a touches
   `colored`/`owo_colors`/`termcolor`/raw ANSI escapes (repeats
   `research/ux.md` §3's grep finding — no drift). A warning piped through
   `| cat`, ingested by a log aggregator, or read by a screen reader is
   fully legible.
8. **User can diagnose a rejected connection in one command** — `tymux ls`
   (or any subcommand) against a UDS-rejecting daemon produces Surface 9's
   message directly on the first failed invocation; no `--verbose` flag or
   second command is needed to learn *why* the connection was rejected or
   *what* to do about it.
9. **Distinct opening clauses hold across all rejection/failure classes** —
   extending `bearer-token-auth`'s existing three-way table with this
   feature's two new rows:

   | Case | Opening clause |
   |---|---|
   | Daemon unreachable / stale socket (Surface 8) | `couldn't connect to tymuxd — is the daemon running?` |
   | Bearer-token rejected (existing) | `tymuxd rejected this connection: missing\|invalid bearer token ...` |
   | UDS peer-cred rejected (Surface 9, new) | `tymuxd rejected this connection: not authorized to access this daemon's socket ...` |
   | TCP fallback (Surface 7, new — not an error) | `no reachable Unix socket at {path} — falling back to TCP loopback ...` |
   | Other RPC error (existing) | raw `status.message()` |

   Five visually distinct openings, none a near-duplicate of another.

## Terminal-UX equivalents (in place of WCAG/ARIA)

- **No color-only signaling**: see cross-surface AC7 above.
- **Scriptability / non-interactive parity**: see cross-surface AC6 above
  — this is this feature's sharpest terminal-specific requirement, since
  (unlike `bearer-token-auth`) this feature adds real path-discovery logic
  that could plausibly (but must not) behave differently under a TTY.
- **Piped/redirected output stays parseable**: Surfaces 1, 3, 4, 5, 7, 9
  all emit exactly one line per event (no multi-line ASCII art, no
  progress spinners, no carriage-return-based redraws) — safe to `grep`,
  safe to pipe into a log aggregator, safe to capture in a CI log without
  control-character noise.
- **No mouse-only or interactive-prompt-only affordance**: every remedy
  named in every error message (Surfaces 3, 4, 8, 9) is itself a flag,
  env var, or shell command a script can act on programmatically — none
  requires an interactive follow-up prompt.

## Gaps found

**Gap 1 — two of Deployment Guidance's four operator-facing caveats
existed only as Rust doc comments, never rendered to an operator; now
fixed for those two, one remains open.** Per Surface 11's table: the
"full daemon control, not a scoped subset" `--socket-group` caveat and
the macOS/BSD primary-gid-only limitation are now both logged once at
`tymuxd` startup, conditional on `--socket-group`/`TYMUXD_SOCKET_GROUP`
actually being set (`implementation/plan.md`'s Story 4.3.2/Task 4.3.2a —
added specifically to close this gap, and to double as the fix for
`pre-mortem.md`'s independently-raised P2 finding #4, which flagged the
same "caveat needs to be logged at startup, not just documented" point).
This was this design's single concrete, actionable finding when first
written, and it's now addressed for the two caveats that are actually
`--socket-group`-scoped.

The third caveat — a containerized/bind-mounted-socket client seeing its
host-mapped uid, which may not match `id -u` inside the container — is
**not** `--socket-group`-specific (it applies to any UDS connection,
group-access or owner-only) and so isn't covered by Task 4.3.2a's
conditional log line; it previously remained doc-comment-only
(`friendly_message`'s doc comment above the `PermissionDenied` branch,
Task 6.3.1a, and the `--socket-path` flag's own doc comment, Task
6.1.1c). This caveat is genuinely lower-frequency than the other two —
it only matters to a containerized deployment — so doc-comment-plus-
README remains the right placement per this gap's original
recommendation.

**RESOLVED in this repair pass.** `implementation/plan.md`'s new Phase 9
(Epic 9.1, Story 9.1.1, **Task 9.1.1a**) adds a "Multi-user / shared-host
deployment" section to this repo's `README.md` documenting this caveat in
plain language, cross-referenced from both `--socket-path`'s (Task
6.1.1c) and `--socket-group`'s (Task 1.2.1a) doc-comment/help text (Task
9.1.1b). This closes the one item this design left open — see Surface
11's table, now updated to reflect it.

**Gap 2 — RESOLVED. `tymux-cli`'s `--socket-path` flag was invisible to
`tymux --help`, unlike this same feature's own `--token` precedent; it
now renders identically to `--token`.** This gap was originally raised
against this plan's first draft, which had `tymux-cli` resolve
`--socket-path`/`--socket-group`/`--disable-tcp-loopback` via the same
hand-rolled `std::env::args()` scan `tymuxd` uses — invisible to `--help`
for all three, an asymmetry with `--token` (which does render via clap's
`[env: TYMUXD_TOKEN=]` auto-annotation) introduced by this same feature.
`implementation/plan.md`'s Pattern Decisions ("`tymux-cli` new-flag
mechanism" row) picked up this gap's own recommendation: `--socket-path`
is now an ordinary `#[arg(long, global = true, env =
"TYMUXD_SOCKET_PATH")]` field on `tymux-cli`'s existing `Cli` struct
(Task 6.1.1c), mirroring `--token`'s exact shape, with Task 6.1.1d
asserting the resulting `--help` output directly. `--socket-group` and
`--disable-tcp-loopback` still have no `tymux --help` entry — but that's
not a residual instance of this gap, since neither ever had a
`tymux-cli`-side flag to begin with: both are `tymuxd`-only bind-time
configuration (Pattern Decisions scopes the clap-field fix to
`--socket-path` specifically, "the only [flag] with a client-side
analogue"), so there is nothing on the `tymux-cli` side for `--help` to
render for them. `tymuxd`'s own lack of any `--help` (for
`--socket-group`/`--disable-tcp-loopback` alike) is unchanged
pre-existing behavior, not something this feature was ever going to
close (see Surface 10's own AC on this point).

No other mismatches found between `research/ux.md`'s proposed message
wording and `plan.md`'s actual committed strings: the TCP-deprecation
warning (Task 4.2.2a), the UDS-bind-failure message (Task 4.2.1a), the
`PermissionDenied` remedy text (Task 6.3.1a), and the TCP-fallback notice
(Task 6.2.1b) all match `research/ux.md`'s illustrative drafts closely,
with `plan.md`'s versions treated as authoritative wherever the two
differ in exact phrasing.
