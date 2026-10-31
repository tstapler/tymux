# ADR 0007: Restart policy and lifecycle control for panes

**Status**: Accepted

## Context

An exited pane stayed `PaneEntry::Live` in the engine, flagged dead, until `KillSession` or `ClosePane`. Nothing
restarted it. `ReviveSession` only respawned panes restored from disk after a daemon restart; for a pane that exited
inside the running daemon it answered `AlreadyLive` and did nothing, because the guard counted any `Live` entry as
running whether or not its child had exited. A client that wanted a service pane to survive crashes had to poll
`ListSessions` and rebuild the session itself, with no way to bound a crash loop.

## Decision

Additive proto only (`buf breaking` clean, no protocol bump), advertised as capability **`pane.lifecycle`**.

### Policy

`RestartPolicy` is per pane, with a per-session default for panes created later (`CreateSessionRequest.restart_policy`
sets both the first pane and the session default; `SetRestartPolicy` with `session_id` changes the default and, with
`apply_to_existing_panes`, the existing panes).

| Field | Meaning | Default |
|---|---|---|
| `mode` | `NEVER`, `ON_FAILURE` (non-zero or unknown code), `ALWAYS`, `UNLESS_STOPPED` | `NEVER` |
| `max_restarts` | automatic restarts allowed inside `window_ms` | 5 |
| `window_ms` | sliding window; 0 = lifetime budget until reset | 60000 |
| `initial_delay_ms`, `multiplier`, `max_delay_ms` | exponential backoff | 1000, 2.0, 30000 |
| `jitter` | fraction randomized either way | 0.2 |
| `healthy_after_ms` | a run this long resets the backoff delay | 10000 |

A pane created with no policy is `NEVER`, so an exit looks exactly as it always did: no marker in the stream, no
restart, `exited` event and stream close. Two operator actions are deliberately visible even then: `StopPane` writes a
`stopped` marker, and `ReviveSession` now respawns a pane that exited in this daemon (see below).

`UNLESS_STOPPED` behaves exactly like `ALWAYS`. `StopPane` parks a pane in `STOPPED` in every mode, so the two cannot
differ; the name exists for callers that expect Docker's spelling. We chose an alias over omitting the value so such
callers do not have to translate.

Delay for restart *n* (counting consecutive failures since the last healthy run) is
`min(initial * multiplier^n, max_delay)` times `1 + jitter * (2u - 1)` for a uniform `u` in `[0, 1)`, clamped to
`max_delay`. The restart budget is counted over automatic restart attempts in the last `window_ms`; an attempt whose spawn fails is
charged like any other (so a launch that can never succeed ends in `FAILED`).
When a failure arrives and that count is already `max_restarts`, the pane goes to `FAILED`: no further automatic
restart until an operator acts. Manual restarts are never charged to the budget.

### State machine

```
            spawn ok                 exit, policy says no
  (new) ───────────────▶ LIVE ─────────────────────────────▶ EXITED
                          │  ▲                                 │ StartPane / RestartPane
        exit, will restart│  │ respawn ok                      ▼
                          ▼  │                               (LIVE)
                       BACKOFF ──(timer due)──▶ RESTARTING ──▶ LIVE
                          │                          │ spawn fails: counts as a failed run
   budget exhausted ──────┴──▶ FAILED                └──▶ BACKOFF or FAILED
   StopPane (any state) ─────▶ STOPPED   (StartPane / RestartPane leave it)
```

`RESTARTING` is also the state of a *running* pane after `RestartPane`, until the old process dies and the new one
starts. `STOPPED` is set when `StopPane` is called; for a running pane the process is still on its way out until
`StopPane` returns, which is why `liveness` (process running) and `state` (supervision) are separate fields. `liveness`
stays `LIVE` only while a process runs; `DEAD` covers every other state.

### Operator actions

| RPC | Effect |
|---|---|
| `SetRestartPolicy` | takes effect at the pane's next exit; a pending backoff the new policy would not have scheduled is cancelled (the stream gets a "restart cancelled" marker and a second `EXITED` event after the one sent when the process died), one it would have is left on its schedule |
| `GetPaneStatus` | state, resolved policy, `restart_count` (lifetime), `restarts_in_window`, `consecutive_failures`, last exit code and time, `next_restart_at`, `started_at`, `uptime`, `pid`, and the daemon's clock (for a countdown that does not trust the client's) |
| `RestartPane` | running: SIGTERM, SIGKILL after `grace_ms` (default 5000), respawn once it dies, no delay, not charged to the budget; dead (any state): respawn now. `reset_backoff` also clears the delay and the window. Returns after the new process started |
| `StopPane` | SIGTERM, SIGKILL after the grace period, park in `STOPPED`, cancel a pending backoff; repeating it on a pane already `STOPPED` is a silent no-op. Returns once the process is reaped (or grace plus a 2 s margin) |
| `StartPane` | from `STOPPED`, `EXITED`, `FAILED` or `BACKOFF`, with a fresh budget; `FAILED_PRECONDITION` if running. Also starts a single pane restored from disk |
| `ResetRestartBudget` | clears delay and window; `FAILED` becomes `EXITED` and nothing starts |

Last operator command wins: `StopPane` after `RestartPane` stops; `RestartPane` after `StopPane` restarts. A second
`RestartPane` while one is pending does not stack.

Pane status is also on `ListSessions` (`Pane.state`) and `CapturePane` (`PaneSnapshot.state`); `CapturePane` on a pane
in `BACKOFF` returns its last screen with `liveness = DEAD` and `state = BACKOFF`.

### Restart keeps the pane

A restart is `Pane::respawn`: a new pty and process in the *same* `Pane` object. Pane id, vt100 screen, scrollback,
replay buffer, output sequence counter, broadcast channel, size and launch spec survive; the old pty master, writer
and reader thread are replaced (the reader is joined) and the child is re-launched from the stored command, args,
env, cwd, `stderr_path` and `stdin_path`, re-reading argument files as a revive does. Because the broadcast channel
survives, attached clients stay attached with no resubscribe.

The output stream shows the boundary: at exit the daemon writes `[tymux: process exited with code N; restarting in
1.0s]` (or `gave up after ...`, `stopped`) into the pane, and at respawn a terminal reset (leave the alternate
screen, mouse and paste modes off, show the cursor) then `[tymux: restarted (pid P, restart #K)]` before the child's
first byte. A pane left on the default `NEVER` policy gets no marker.

### Attach

While a restart is pending the stream stays open: `wait_final_exit` (resolves only when the pane exited and nothing
will bring it back) replaces `wait_exit` in `Attach`, and no `exited` event is sent. `exited` follows the final
lifecycle event once the pane is `EXITED`, `FAILED` or `STOPPED`. `Attach` also accepts a pane in `BACKOFF`.
`AttachRequest.subscribe_lifecycle` (first message) opts in to `AttachEvent.lifecycle` events (`EXITED`,
`RESTART_SCHEDULED`, `RESTARTED`, `FAILED`, `STOPPED`); a client that does not set it never sees one, so old clients
are unaffected apart from staying attached across restarts.

### Persistence

Policy and counters are written into the session record (`PersistedPaneRecord.restart`,
`PersistedSessionRecord.default_restart_policy`, both `#[serde(default)]`, schema version unchanged) on every change.
After a daemon restart the process is gone and the v1 persistence ADR's rule
([ADR-002](../../project_plans/v1-release/decisions/ADR-002-persistence-durability-tiers.md)) that nothing is revived
automatically on startup still holds, so `LIVE`, `BACKOFF` and `RESTARTING` come back as `EXITED`: a pending
backoff timer is dropped, not resumed. `STOPPED` and `FAILED` stay as they were; `restart_count`, the window log and
`consecutive_failures` are kept. `StartPane` or `RestartPane` starts any of them, with a fresh budget (`ReviveSession` does too, except for `STOPPED` and `FAILED`, see below);
the policy applies from then on.

### ReviveSession

`ReviveSession` now respawns panes whose child exited in this daemon (in place, same pane and subscribers) as well as
panes restored from disk. A session with any running pane is still `already_live` and nothing is spawned.
A revive never overrides an operator decision: a pane in `STOPPED` (StopPane) or `FAILED` (restart budget used up)
is skipped and stays as it is while the session's other panes are revived; if every pane is `STOPPED` or `FAILED` the
call is `FAILED_PRECONDITION`. A mixed session (some panes held, others revivable) returns
`Revived { pane_count: n }` counting only the panes started; the held panes stay as they are. A session with no panes
revives to `pane_count: 0`. `StartPane` or `RestartPane` (which reset the budget) bring those back.

### Implementation

- `restart.rs`: policy, `PaneSupervisor` state machine, `Clock`/`Jitter` traits. Pure; every behavior above is unit
  tested with `ManualClock` and `FixedJitter`, no sleeps.
- `supervision.rs`: drives the state machine from real exits. The pane's reader thread calls the exit hook after the
  exit code is recorded and *before* `is_exited()` turns true, so a client that sees the exit also sees whether a
  restart is pending. The restart timer is not a task inside the core: `Engine::supervise_tick` runs what is due and
  reports the next deadline; `tymuxd`'s supervisor task calls it on every wake and at the deadline. Tests call it
  after advancing the manual clock.
- Locking: the supervision lock is a leaf. It is held across `Pane::respawn`, so a stop, close or policy change
  either lands before the spawn and prevents it, or after and sees the new process. `ClosePane`/`KillSession` forget
  the pane (which retires it, so a respawn that lost the race fails) before killing it, so a closed pane's exit is
  never mistaken for a crash.
- `Pane::kill`'s delayed SIGKILL checks a per-pane generation, so it cannot hit a successor process.

## Consequences

- Services can run under tymuxd and survive crashes without a client in the loop, with a bounded crash loop.
- The signal a process died of is not recoverable (portable-pty reports any signal death as code 1), so
  `last_exit_signal` is set only when the daemon sent the signal (`StopPane`, `RestartPane`).
- Time is wall-clock unix milliseconds so it can be persisted and shown. A backwards clock step delays a pending
  restart; it never fires one early.
- A session record now changes on every exit and restart, not just on structural edits; the supervisor persists the
  affected sessions after each change, off the reader thread.
- Typing into a pane between lives is dropped silently.

## Rejected

- **Respawn from the client** (stapler-squad polling `ListSessions`): no shared crash-loop budget, races with other
  clients, and a restarted pane would be a new pane.
- **A new `Pane` per restart.** Clients holding the old pane's output channel would have to reattach, and scrollback
  would need copying.
- **A timer task per pane in the core.** It would tie the core to a runtime and make time untestable; a polled tick
  with an injected clock is deterministic.
