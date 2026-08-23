# UX Design: stapler-squad-integration

**Date**: 2026-08-21
**Phase**: SDD Phase 3 (design)
**Reads**: `requirements.md`, `research/ux.md`, `implementation/plan.md`

## Scope note

tymux has no UI of its own here. Every surface below is stapler-squad's
existing xterm.js dashboard, reacting to tymux/`BackendTymux` behavior. This
doc designs the *new* states `BackendTymux` introduces, fit to stapler-squad's
existing pattern language — `ConnectionIndicator.tsx` (persistent connection-
status affordance, portal-free, inline `aria-live="polite"` region) and
`InputDropBadge.tsx` (portal-rendered, auto-dismissing, `LiveRegion`-backed,
`aria-live="assertive"`) — read directly from
`web-app/src/components/layout/ConnectionIndicator.tsx` and
`web-app/src/components/sessions/InputDropBadge.tsx`. No new pattern is
invented; every state below is classified against one of these two existing
shapes.

Implementation of these states is stapler-squad-side UI work, downstream of
this project's `plan.md` (which stops at the Go `BackendTymux` layer — no
`.tsx` files appear in Phase 2's task list). This doc designs what that
downstream work should build, scoped to the states tymux's new behavior
actually produces (ADR-001 exit status, ADR-003 priming snapshot, the
disconnect-survival fix, the three-way error split from `research/ux.md` §4).

## Surface inventory

| # | Surface | Type | Driven by |
|---|---------|------|-----------|
| 1 | Reattach rendering (priming snapshot) | Interactive | ADR-003, Epic 1.3 |
| 2 | Standing-stream reconnect indicator | Interactive | Epic 2.5 (`ReconnectLoop`) |
| 3 | "Session ended while you were away" (dead-on-reattach) | Interactive | Epic 1.1 fix + Epic 2.4.2 exit callback |
| 4 | Session-start failure (3-way split) | Interactive | Epic 2.1–2.2 (`Start`/`CreateSession`) |
| 5 | Backend identity indicator | **Not designed — no plan story** | `plan.md` Risk Control / Epic 2.1.3 |
| 6 | Exit-status CLI/gRPC output | Non-interactive | Epic 1.2 |
| 7 | Structured logs (disconnect/reconnect/exit lifecycle) | Non-interactive | Observability Plan |
| 8 | Load-test harness output | Non-interactive | Epic 1.7 |
| 9 | Backend selection (`ProcessManagerOptions`) | Non-interactive | Epic 2.1.3 |

Surface 5 is listed and explicitly scoped out: `plan.md` has no story adding
a tmux-vs-tymux badge anywhere in stapler-squad's UI — backend choice is a
per-session Go-layer parameter (`ProcessManagerOptions.defaultBackend`,
Epic 2.1.3), invisible to the end user by design (Risk Control: opt-in,
no default-flip in this project). Designing an indicator for a surface no
story produces would be scope creep on this pass. **Recommendation for a
follow-up project** once `BackendTymux` is default-on: a small non-actionable
label, styled like `SessionCard.tsx`'s existing `muxIndicator` checkmark
badge (`web-app/src/components/sessions/SessionCard.tsx:547`), is the
right-sized answer when that day comes — not designed further here.

---

## Surface 1: Reattach rendering (priming snapshot)

**Problem** (`research/ux.md` §1): today, a fresh `Attach` subscribes to
*future* output only. A user closing and reopening the dashboard tab sees a
**blank terminal** until the agent produces new output — even though the
agent kept running the whole time. ADR-003 fixes this server-side: the first
`AttachEvent` on every `Attach` call is now `Snapshot`, populated from
`pane.snapshot()`, sent before any live `Output` bytes.

This is a rendering-correctness fix, not a new UI affordance — there is
nothing for the user to click. It is here because it is the single highest-
value fix in the whole project's UX story (`research/ux.md` §5: "the user
needs to *know*... that walking away didn't cost them anything") and because
its correctness is externally verifiable through the UI, which is where a
human tester will check it.

### Flow

```
User closes tab / loses network       Agent keeps running server-side
        │                                        │
        ▼                                        ▼
 xterm.js instance destroyed          tymuxd pane continues producing
 (browser-side only; BackendTymux's   output into its live broadcast
 standing Attach stream in Go is      channel; new content overwrites
 UNAFFECTED — see Surface 2)          older on-screen state as normal

        │  user reopens dashboard tab
        ▼
 New xterm.js instance mounts
        │
        ▼
 BackendTymux's ALREADY-OPEN standing stream (Epic 2.3) is reused —
 no new Attach RPC on plain tab reopen. The dashboard reads current
 render state from ClientFanout's last-known buffer + a fresh
 CapturePane-equivalent seed if the tab was closed long enough that
 the Go-side session object itself was torn down and rebuilt.
        │
        ▼
 xterm.js renders: the CURRENT screen, immediately — cursor position,
 colors, alt-screen state all correct — not a replay, not a blank
 screen (matches tmux/mosh/VS Code precedent, research/ux.md §1)
```

### ASCII wireframe — before vs. after ADR-003

```
BEFORE (bug):                          AFTER (ADR-003 fix):
┌─────────────────────────────┐        ┌─────────────────────────────┐
│ my-agent-session       [x]   │        │ my-agent-session       [x]   │
├─────────────────────────────┤        ├─────────────────────────────┤
│                               │        │ $ claude "refactor auth.go" │
│                               │        │ ⠙ Editing auth.go...        │
│        (blank — waiting       │        │ ⠙ Running tests...          │
│         for next output)      │        │ ✓ 12 passed                 │
│                               │        │ $ _                          │
│                               │        │                              │
└─────────────────────────────┘        └─────────────────────────────┘
  User has no idea if the agent          User sees exactly where the
  is alive, stuck, or dead.              agent is, immediately.
```

### Acceptance criteria

- UX-1.1: On reopening a dashboard tab for a session that was running
  unattended, the terminal shows the agent's current on-screen state within
  one render frame of the xterm.js instance mounting — never a blank
  terminal followed by a delayed repaint.
- UX-1.2: Cursor position, active color/attribute state, and alt-screen
  mode (if a full-screen tool like `vim`/`htop` is running) are all correct
  on first paint — not just plain text.
- UX-1.3: A human tester can verify this in ≤ 2 steps: (1) start a session
  and let it produce visible output, (2) close and reopen the tab — no
  reload, no manual "resync" action required.
- UX-1.4: No dead end: even in the rare case the priming snapshot itself
  fails to arrive (e.g. `tymuxd` restarted between disconnect and reattach),
  Surface 3 or Surface 4's error state must be shown instead of a
  silently blank terminal — a blank screen with no explanation is never an
  acceptable end state.

---

## Surface 2: Standing-stream reconnect indicator

**Problem**: `BackendTymux` keeps one `Attach` stream open for a session's
whole lifetime (Epic 2.3), independent of any browser tab. That stream can
still drop — network blip between stapler-squad's Go backend and `tymuxd`,
or a `tymuxd` restart — distinct from a user closing their browser tab. When
it drops, `ReconnectLoop` (Epic 2.5) retries with backoff and resyncs via
`CapturePane` on success.

This is exactly the shape `ConnectionIndicator.tsx` already handles for the
browser↔stapler-squad-backend WebSocket connection. **Reuse it, don't
reinvent it**: extend the existing `ConnectionState` union
(`connected`/`stale`/`disconnected` — `sessionsSlice.ts`) with the
`BackendTymux`-specific state, or add a **second**, session-scoped instance
of the same component pattern if the existing one is scoped to the
process-wide WebSocket only (confirm scope during implementation — this is
a stapler-squad-side wiring decision, not a new design).

### Wireframe — session card, reconnecting state

```
┌───────────────────────────────────────────────────┐
│ ● my-agent-session                            [x]  │
│   ⟳ Backend reconnecting… (attempt 2)               │  ← reuses
├───────────────────────────────────────────────────┤     ConnectionIndicator's
│ $ claude "refactor auth.go"                         │     spinner + label +
│ ⠙ Editing auth.go...                                │     tooltip shape
│ (last known state — may be stale)                   │
└───────────────────────────────────────────────────┘
```

### Interaction flow

1. `StandingAttachStream` errors (not preceded by `Close()`/`DetachSafely()`
   — Story 2.5.1's detach-vs-drop distinction).
2. `ReconnectLoop` begins backoff retry. The Go layer surfaces this via
   whatever channel `BackendTmux`'s reconnect state already reaches the
   frontend through (mirrring `ConnectionIndicator`'s existing
   `reconnectAttemptCount`/`ConnectionState` plumbing) — same signal shape,
   new source.
3. UI shows the reconnecting state within one render cycle of the drop
   being detected — same debounce/threshold `ConnectionIndicator` already
   uses for its own `stale`→`disconnected` transition, so a sub-second blip
   does not flash the indicator (per that component's existing behavior).
4. On successful reconnect: `ReconnectLoop`'s `CapturePane`-based resync
   (Story 2.5.2b) reseeds render state — terminal repaints to current
   content, same guarantee as Surface 1. Indicator returns to `connected`/
   `Live`.
5. If `ReconnectLoop` exhausts its retry budget: transitions to a distinct
   **"backend unavailable"** state — this is Surface 4's territory, not a
   fourth reconnect attempt shown forever. Never leave the indicator
   spinning indefinitely with no exit.

### Error / edge cases

| Condition | User sees | Exit path |
|---|---|---|
| Stream drops, retry in progress | "Reconnecting… (attempt N)" spinner, last-known terminal content held static | Auto-resolves on reconnect, or falls through to next row |
| Retry budget exhausted | "Backend unavailable" banner (Surface 4 shape), terminal content frozen at last-known state, explicitly labeled stale | Manual "Retry" action + link/expandable detail |
| Reconnect succeeds but `CapturePane` resync itself fails | Same "backend unavailable" fallback — do not show `connected` on a resync that didn't actually complete | Manual retry |
| User closes tab during a reconnect | No UI to show (tab gone) — `BackendTymux`'s standing stream keeps retrying server-side regardless of tab state | N/A — this is exactly the disconnect-survival guarantee working correctly |

### Acceptance criteria

- UX-2.1: Reconnect state uses the same visual language as
  `ConnectionIndicator.tsx` (spinner, label, tooltip) — a user who already
  understands that indicator for WebSocket drops does not have to learn a
  new one for backend drops.
- UX-2.2: An `aria-live="polite"` announcement fires on entering and
  leaving the reconnecting state, mirroring `ConnectionIndicator`'s existing
  `STATE_ANNOUNCE` map — screen-reader users get the same signal sighted
  users get from the spinner.
- UX-2.3: Terminal content is never silently replaced with placeholder/
  loading UI during a reconnect — the last-known screen stays visible and
  is explicitly marked stale by the indicator, not by dimming or hiding the
  terminal itself (dimming/hiding would itself be a second, undocumented
  signal competing with the indicator).
- UX-2.4: No dead end: retry-budget-exhausted always offers a manual retry
  action, never terminates in a state with no visible next step.

---

## Surface 3: "Session ended while you were away"

**Problem** (`research/ux.md` §4): if a pane dies while nobody was
attached — the very failure mode Epic 1.1's `setsid()` fix targets, but
which can still legitimately happen for other reasons (agent process itself
exits, OOM-killed, etc.) — the current gRPC-level signal is just
`Liveness::DEAD` on the next call. There is no proactive push. Silently
showing the last static screen with a live-looking cursor would be actively
misleading: the user could believe the agent is still working.

Precedent: Zellij's session list labels a resurrectable-but-dead session
"Exited" explicitly, rather than rendering it identically to a live one.

### Wireframe — session card, dead-on-reattach state

```
┌───────────────────────────────────────────────────┐
│ ○ my-agent-session          Exited (code 0)   [x]  │  ← status dot goes
├───────────────────────────────────────────────────┤     from ● (live) to
│ $ claude "refactor auth.go"                         │     ○ (dead), label
│ ⠙ Editing auth.go...                                │     replaces "Live"/
│ ✓ 12 passed                                         │     "Reconnecting"
│ $ _                                                  │
│                                                       │
│  ⓘ This session ended while you were away.          │  ← new, non-dismissing
│    Last activity: 14 minutes ago.  [ Start new ]    │     banner, terminal
└───────────────────────────────────────────────────┘     content stays visible
```

Exit code (Epic 1.2's `ExitStatus`) is shown when `code` is present (proto3
field presence — Go: a non-nil `*int32`; Rust: `Some(_)`); when `code` is
absent (signal-killed, `wait()` failed), the banner reads "ended
unexpectedly" instead of "Exited (code N)" — surfacing the field-presence
sum-type distinction ADR-001 was designed to make explicit (`plan.md`'s
Domain Glossary: `ExitStatus { optional int32 code = 1; }`, no separate
boolean field), rather than collapsing it to a fake "code 0."

### Interaction flow

1. `SetOnExitCallback` fires (Epic 2.4.2) — either live, while attached
   (normal case, e.g. Surface 1's flow already covers the visible-exit
   case), or the fire-once-after-registration path: the pane had *already*
   exited before the dashboard reattached and registered its callback.
2. In the reattach-finds-it-already-dead case specifically: the priming
   snapshot (Surface 1) still renders the last on-screen content correctly
   — that mechanism doesn't change — but the liveness state that arrives
   alongside it (from the standing stream's cached `Liveness`, Epic 2.2.1c)
   is `DEAD`, not `LIVE`.
3. UI renders the terminal content (frozen, no cursor blink) **plus** the
   explicit banner — never one without the other. A dead pane with no
   banner is the exact "actively misleading" failure `research/ux.md` §4
   calls out; a banner with no terminal content loses the "what did it
   actually do" context the user needs to decide next steps.
4. "Start new" is the only forward action — there is no "resume this exact
   process" (the process is gone; `ReviveSession`, per Epic 2.2.1b, revives
   the *tymux pane* for a fresh command, not the dead process itself).

### Error / edge cases

| Condition | User sees | Exit path |
|---|---|---|
| Exit code known (`code` present) | "Exited (code N)" — code 0 styled neutrally, nonzero styled as a warning color | "Start new" |
| Exit code unknown (`code` absent) | "Ended unexpectedly" — no fabricated code | "Start new" + link to logs/details if available |
| Pane died mid-attach (user watching live) | Same banner appears in place, terminal stops updating at the last frame — this is not a reattach case, but the same banner component covers it (one mechanism, not two, per `plan.md`'s pitfalls-§5 principle) | "Start new" |
| `disconnect_survival_e2e` regression (pane dies from the bug this project fixes) | Indistinguishable from a legitimate exit at the UI layer — this is intentional: the UI's job is to report ground truth, not diagnose *why* tymux reports DEAD. If the bug regresses, tymux-side observability (Surface 7) is where that gets diagnosed, not the dashboard. | "Start new" |

### Acceptance criteria

- UX-3.1: `Liveness::DEAD` on reattach or mid-session is always paired with
  an explicit, non-dismissible-by-timeout banner — this state must never be
  presented identically to a live session with no new output yet (the
  specific failure mode `research/ux.md` §4 flags).
- UX-3.2: The banner distinguishes known exit code, unknown exit code, and
  (implicitly, via Surface 4) "never started at all" — three different
  underlying causes never collapse into one generic "error" message.
- UX-3.3: Terminal content freezes but remains fully readable/scrollable —
  the last-known output is not hidden behind the banner.
- UX-3.4: "Start new" is reachable in 1 click from the banner — no dead end.
- UX-3.5: Screen-reader users get the state change via the same
  `aria-live="polite"` region pattern as Surface 2 (status-change, not
  "assertive" — this is informational, not a data-loss warning like
  `InputDropBadge`'s dropped-keystrokes case).

---

## Surface 4: Session-start failure (3-way split)

**Problem** (`research/ux.md` §4): three categorically different failures
must not collapse into one generic error, because they imply different next
actions:

1. **`tymuxd` unreachable** — gRPC connection failure before a session ever
   existed (daemon not running / crashed / misconfigured socket path).
2. **`CreateSession` returns an error** — daemon reachable, but session
   creation itself failed (bad command, resource limits).
3. **Session created, agent process immediately exited** — this is actually
   Surface 3's dead-state banner, reached via the normal lifecycle rather
   than a start-time error; listed here only to make the boundary explicit
   (`BackendTymux` should not swallow the underlying connect-error string
   into a generic "failed to start" for any of the three).

### Wireframe — new-session failure states

```
Case 1: tymuxd unreachable                Case 2: CreateSession errored
┌─────────────────────────────┐          ┌─────────────────────────────┐
│  ✕ Can't reach tymux backend │          │  ✕ Session failed to start  │
│                                │          │                                │
│  connect: dial tcp             │          │  tymuxd rejected the request: │
│  127.0.0.1:7419: connection    │          │  "invalid command: ..."       │
│  refused                       │          │                                │
│                                │          │  [ Edit command ]  [ Retry ]  │
│  [ Retry ]  [ Use tmux backend]│          │                                │
└─────────────────────────────┘          └─────────────────────────────┘
  Distinguishable root cause              Distinguishable root cause
  (daemon down) + a real fallback         (bad input) + an action that
  action (switch backend), not            actually addresses it (fix the
  just "try again"                        command), not just retry
```

Case 3 (immediate agent exit) is **not** a start-time error dialog — it
resolves into a normally-created, now-dead session, i.e., Surface 3's card
renders immediately with the "Exited" banner already showing. This is a
deliberate design choice: conflating "creation failed" with "creation
succeeded, then the thing inside it died" is exactly the confusion
`research/ux.md` §4 warns against — keeping them as different UI shapes
(a blocking dialog vs. a card that exists and shows dead) is how the
distinction stays visible instead of being flattened by a shared component.

### Interaction flow

1. User initiates "New session" with `BackendTymux` selected (per-session
   opt-in, Epic 2.1.3 — no global toggle exists to fail differently for).
2. `BackendTymux.Start(dir)` attempts `CreateSession`.
3. **Connect-level failure** (gRPC channel never established): surfaced
   before any session object exists in the UI at all — this must not
   silently retry forever or hang; error state 1 above appears immediately,
   with the raw connect-error string included (per `research/ux.md` §4:
   "should not swallow the underlying connect error string") so a user (or
   whoever they escalate to) can tell daemon-down from misconfiguration.
4. **`CreateSession` RPC-level failure** (daemon reachable, request
   rejected): error state 2, includes the daemon's returned error message
   verbatim, plus a "Retry" that resubmits the exact same request (no
   silent field-stripping).
5. **Success**: session card created normally; if the agent process inside
   it exits before the user ever sees live output, this is Surface 3's flow
   from that point on, not a special "instant death" case.

### Error / edge cases

| Condition | User sees | Exit path |
|---|---|---|
| `tymuxd` not running | "Can't reach tymux backend" + raw connect error + fallback to start with `BackendTmux` instead | Retry, or fall back to tmux backend (both real actions — the per-session selector this project builds makes the fallback actually available, not just theoretical) |
| `tymuxd` crashed mid-session-creation | Same as above — connect failure and mid-call failure are not meaningfully different to the user | Retry, or fall back |
| Bad command / resource limit rejection | "Session failed to start" + daemon's literal error text | Edit and retry |
| Agent process exits immediately after a successful `CreateSession` | No start-time error at all — session card renders, then immediately shows Surface 3's "Exited" banner | Start new (Surface 3's own exit path) |

### Acceptance criteria

- UX-4.1: The three failure classes above are visually and textually
  distinguishable — a user (or a support screenshot) can tell "daemon
  down" from "bad command" from "agent crashed" without reading logs.
- UX-4.2: Every error state includes the underlying error string verbatim
  (connect error or `CreateSession` error), not a generic "something went
  wrong" — directly required by `research/ux.md` §4.
- UX-4.3: Every error state offers at least one concrete, working action
  (Retry, Edit command, or fall back to `BackendTmux`) — no dead ends,
  matching the project's Risk Control design (per-session selector makes
  "fall back to tmux" a real, always-available action, not aspirational).
- UX-4.4: User can attempt session creation, hit a `tymuxd`-unreachable
  failure, and successfully start the same session on `BackendTmux` instead
  in ≤ 2 clicks from the error state (click fallback action, done — the
  per-session backend param means no separate settings navigation).

---

## Surface 5: Backend identity indicator — not designed

Explicitly out of scope for this pass. See Surface inventory table above.
No `plan.md` story adds this — `plan.md`'s Risk Control section (per-session
opt-in via `ProcessManagerOptions.Backend`/Epic 2.1.3, `BackendTmux` staying
the default, and its own statement that "this plan does not include a 'flip
the default' story at all") is the actual grounding for this being
out-of-scope-for-now, not any lettered requirements.md item (requirements.md
has no lettered/numbered scope list, and no research doc in this project
contains a "conditional on the plan" framing either — the citation
previously here, "requirements.md 1(c)," did not correspond to any real
source and has been corrected). Do not build ahead of a real story.

---

## Surface 6: Exit-status CLI/gRPC output (non-interactive)

Epic 1.2 makes exit status queryable via the `ExitStatus { optional int32
code = 1; }` message (proto3 field presence, no separate boolean field) on
`AttachEvent` and persisted on dead `PaneEntry` records. The only
non-interactive surface here is `tymux-cli`'s existing status output and
`clients/go`/`clients/ts` example output — both already text/JSON, not a
UI screen.

**Representative sample** (`tymux-cli`-style status line, mirroring the
existing `Liveness` display convention):

```
$ tymux-cli status my-agent-session
SESSION       PANE      LIVENESS   EXIT
my-agent-...  pane-01   DEAD       code=42
my-agent-...  pane-02   LIVE       -
```

### Acceptance criteria

- UX-6.1: A dead pane with a known exit code always prints `code=N`, never
  a bare "DEAD" with no code when one is available (`code` present — Go: a
  non-nil `*int32`; Rust: `Some(_)`).
- UX-6.2: A dead pane whose `code` is absent (Go: nil `*int32`; Rust:
  `None`) prints something explicit like `code=unknown`, never `code=0`
  (0 is a real, different exit code — the whole reason ADR-001 chose a
  field-presence shape).
- UX-6.3: Output is stable/parseable — no change to existing column
  ordering or session/pane identifiers that would break scripts already
  parsing `tymux-cli status`.
- UX-6.4: A live pane's EXIT column reads `-` (not blank, not "N/A",
  matching whatever placeholder convention `tymux-cli` already uses
  elsewhere — confirm against existing output during implementation).

---

## Surface 7: Structured logs — disconnect/reconnect/exit lifecycle

Per the Observability Plan: tymux-side `tracing` spans
(`attach priming snapshot sent`, `attach stream ended (exited|error|
cancelled)`, `setsid` outcome) and stapler-squad-side structured logs at the
same granularity as `BackendTmux`'s existing logging.

**Representative sample**:

```
INFO tymuxd::attach{pane_id=8f2a...}: attach priming snapshot sent bytes=1842
INFO tymuxd::attach{pane_id=8f2a...}: attach stream ended cause=cancelled
INFO tymuxd::main: setsid ok sid=48213 pgid=48213
DEBUG tymuxd::main: setsid already session leader (EPERM, expected under systemd)
```

```
INFO backend_tymux session=sess_01 event=standing_stream_reconnect attempt=2 cause=transport_error
INFO backend_tymux session=sess_01 event=output_gap_resync dropped_frames=3
INFO backend_tymux session=sess_01 event=exit_callback_fired code=0 code_present=true
```

### Acceptance criteria

- UX-7.1: Every state transition a user could see in Surfaces 1-4 has a
  corresponding log line on at least one side (tymux or stapler-squad) —
  a support engineer debugging a user's screenshot can find the matching
  event without guessing.
- UX-7.2: `cause=` (or equivalent) on every stream-end/reconnect log line
  distinguishes deliberate detach from transport error from daemon
  restart — matching Story 2.5.1's detach-vs-drop code-level distinction,
  so the log-level signal and the UI-level signal never disagree.
- UX-7.3: `setsid` outcome logs at `info` (success) or `debug` (expected
  EPERM), never `warn`/`error` for the expected already-session-leader
  case — an unexpected `setsid` failure (any other errno) should log at
  `warn` so it's discoverable if the disconnect-survival fix ever
  regresses silently.

---

## Surface 8: Load-test harness output (non-interactive)

Epic 1.7's concurrent load-test script (`clients/ts/examples/
load-test-concurrent.ts`). Output is a developer-facing pass/fail report,
not a product surface, but included for completeness per the requirements'
"error states" ask extending to observability of the scale work.

**Representative sample**:

```
$ npx tsx examples/load-test-concurrent.ts
Pre-creating 900 sessions... done (42.1s)
Firing 200 concurrent CreateSession calls...
  p50: 8ms  p99: 61ms  max: 94ms
  errors: 0 / 200
  threads: 1927 (expected 1925)  fds: 5710 (expected 5710)  rss: 118MB
PASS: p99 61ms < 200ms threshold, 0 errors
```

### Acceptance criteria

- UX-8.1: Output states pass/fail explicitly against the numeric threshold
  (200ms p99 per Story 1.7.2), not just raw numbers a human has to compare
  manually.
- UX-8.2: A failed run's output makes clear *which* assertion failed
  (latency vs. error count vs. resource-count drift) — not a single
  opaque "FAIL."
- UX-8.3: Before/after comparison runs (Task 1.7.2b) are labeled which
  build each result came from — a raw pair of numbers with no build label
  is not a usable comparison artifact.

---

## Surface 9: Backend selection (non-interactive, config/API only)

Per-session `ProcessManagerOptions.defaultBackend` override (Epic 2.1.3) —
this is a Go API parameter, not end-user-facing UI (Surface 5 covers why
no indicator exists yet). Included for completeness of "non-interactive
surfaces."

**Representative sample**:

```go
pm, err := session.NewProcessManager(ctx, session.BackendTymux, opts)
```

### Acceptance criteria

- UX-9.1: Passing no override continues to select `BackendTmux` (the
  documented default) — an omitted parameter must never silently opt a
  session into the new, less-battle-tested backend.
- UX-9.2: An invalid/unrecognized backend constant fails at construction
  time with a clear error, not a runtime panic deep in a delegated call.
- UX-9.3: The override is genuinely per-call — two concurrent
  `NewProcessManager` calls with different `defaultBackend` values never
  interfere with each other (Story 2.1.3's own acceptance criterion,
  restated here as a UX guarantee: a bad `BackendTymux` session never
  silently downgrades a sibling `BackendTmux` session's behavior).

---

## Cross-cutting acceptance criteria

These apply across all interactive surfaces (1-4):

- **No dead ends**: every error/terminal state (Surfaces 2, 3, 4) has at
  least one visible, working forward action — retry, fall back to
  `BackendTmux`, or start a new session. Verified per-surface above.
- **Keyboard navigability**: all new actionable elements (Retry, Start
  new, Edit command, fallback-to-tmux) are real `<button>`/`<a>` elements
  reachable via Tab and activatable via Enter/Space, matching
  `ConnectionIndicator.tsx`'s existing `onKeyDown` handling for Enter/Space
  on its own button (`ConnectionIndicator.tsx:44-49`) — new components
  should copy this handler rather than relying on native button semantics
  alone if they use a non-`<button>` element for styling reasons.
- **Screen-reader labels present**: every new state has an accessible name
  distinct from its visual-only content — `aria-label` on indicator
  buttons (mirroring `ConnectionIndicator`'s `ariaLabel` construction),
  and a paired `aria-live` region for state-change announcements
  (mirroring both existing components' `LiveRegion`/inline live-region
  pattern). "Assertive" politeness is reserved for data-loss-adjacent
  events (matching `InputDropBadge`'s existing use for dropped
  keystrokes); "polite" is used for status transitions (matching
  `ConnectionIndicator`'s existing use) — Surfaces 2 and 3 both use
  "polite," consistent with the existing convention, since no user input
  is lost in either case.
- **Color contrast ≥ 4.5:1**: dead-state ("Exited") and error-state
  ("Can't reach backend") text/icon colors must meet WCAG AA against their
  background in both light and dark themes — verify against
  `SessionCard.css.ts`'s existing status-color tokens
  (`statusCrashed`/`statusUnknown` are the closest existing precedent for
  the new dead/error states — reuse those tokens rather than introducing
  new colors where the semantic meaning already matches).
- **Testability**: every criterion above (UX-1.x through UX-9.x) is
  phrased as a concrete, human-checkable step — no criterion requires
  inspecting source code to verify pass/fail.

## Summary of what downstream implementation must build

None of Surfaces 1-4's UI components are in `plan.md`'s task list — that
plan stops at the Go `BackendTymux` layer, which is the seam these UI
states plug into (`SetOnExitCallback`, cached `Liveness`, `ReconnectLoop`'s
state, `CreateSession`'s distinguishable error path). This design doc is
the spec a follow-up stapler-squad-side UI epic should implement against;
it is not itself an implementation task in this project's Phase 5.
