# ADR-003: The grace period gates deferred viewport/geometry cleanup only, via an independent per-disconnect task

**Status**: Accepted
**Date**: 2026-08-24
**Context**: attach-resume-protocol, Phase 3 planning

## Context

requirements.md's Success Metrics call for "a configurable grace period... during which the pane keeps running and a reconnect can still resume, distinct from an explicit detach." Taken literally, that phrasing suggests the grace period gates pane survival. Reading the actual code rules that out:

- **Pane survival is already unconditional**, independent of any `Attach` stream's lifetime — `Engine.panes` ownership doesn't depend on `Attach` at all (`stapler-squad-integration` Epic 1.1's disconnect-survival fix, confirmed structurally: a pane is torn down only by explicit `ClosePane`/`KillSession`).
- **The replay buffer (Epic 2.1) is capacity-bounded, not time-bounded** — it evicts by byte budget, not by elapsed time since the last read. A time-based grace period on top of a byte-based eviction policy would double-bound the same resource for no added correctness benefit.

What *does* happen immediately on every `Attach` stream ending today, with no delay: `unregister_viewport` + `recompute_window_geometry` (`crates/tymuxd/src/main.rs:768-771`), which drops the disconnecting client's reported viewport from ADR-004's (stapler-squad-integration's, concurrent-attacher geometry policy) per-window minimum-size calculation right away. For a window with multiple attachers, an abrupt drop followed by a prompt reconnect can cause visible geometry thrash: the window's computed minimum shrinks to reflect the remaining attachers, then grows back once the reconnect reports its viewport again.

## Decision

The grace period gates exactly this — the deferred call to `unregister_viewport`/`recompute_window_geometry` — and nothing else. Default `grace_period_duration = 60s`, daemon-wide, overridable via `TYMUXD_GRACE_PERIOD_MS` (mirroring `DEFAULT_DISCONNECT_REGRESSION_WINDOW`'s existing env-var pattern).

The mechanism is an **independent per-disconnect deferred task**: on stream end, spawn a `tokio::time::sleep(grace_period_duration)` followed by the cleanup call, scoped to that specific `client_id` alone. It is never reset, extended, or cancelled by a subsequent reconnect or a subsequent disconnect. Each disconnect gets exactly one scheduled cleanup, firing exactly once, `grace_period_duration` after itself.

Two alternatives were rejected (see plan.md's Step 0.5 creative pass for full detail):

- **A single mutable per-pane/window deadline, reset on every new `Attach`** — the natural first design, but it's pitfalls.md §4's documented DoS vector: a client that repeatedly reconnects-and-immediately-drops holds cleanup off forever, since each reconnect resets the shared deadline.
- **A cancelable per-`client_id` timer tracked in a `HashMap<ClientId, JoinHandle>`**, aborted on reconnect — adds real complexity (a new tracker, a new lock, a cancellation path) for no behavioral gain, because `Engine::new_client_id()` mints a fresh id on every single `Attach` call; a "reconnect" never actually reuses the old id, so there is nothing to cancel that the old id's own eventual cleanup wouldn't already resolve correctly on its own.

## Rationale

- **Closes the DoS vector by construction, not by adding a cap.** Because no shared mutable deadline exists, there is nothing to reset — the leak/DoS pattern pitfalls.md §4 warns about (entries or timers that live forever because something keeps postponing them) cannot occur here regardless of how many times a client reconnects and drops.
- **No new lock, no new tracker.** Unlike `disconnect_tracker` (which needed an explicit purge discipline on every deliberate-removal path to avoid leaking `Uuid`-keyed entries forever), this design has no persistent map to leak from at all — each spawned task owns its own state and exits after firing once.
- **Matches ADR-002's overall shape**: this project deliberately avoids introducing new server-side per-client identity-correlation state where a simpler, self-contained mechanism suffices.

## Consequences

- A window's geometry can reflect a stale (disconnected-but-not-yet-cleaned-up) viewport for up to `grace_period_duration` after a genuine, permanent detach — a deliberate tradeoff, not a bug: the alternative (immediate cleanup) is exactly the thrash this ADR exists to avoid for the *common* case of a brief drop.
- `heartbeat_timeout` (client-side, 45s default) is deliberately kept `< grace_period_duration` (60s) — see ADR-004 — so a well-behaved client has already started reconnecting, by its own detection, before the server would even consider the disconnect "final" enough to clean up geometry for.
- If the disconnecting window closes entirely before its deferred cleanup fires, the deferred task must check the window still exists and no-op rather than erroring (plan.md Task 3.2.2b) — this is the one piece of defensive code this design still needs, since the task's own reference to `window_id` can outlive the window itself.
