# ADR-004: Concrete reconnect backoff/heartbeat/grace-period numbers (informed defaults, not measured)

**Status**: Accepted (revised)
**Date**: 2026-08-24 (original); revised 2026-08-24 during `pm:triad-review` — give-up threshold raised from 8 to 14 attempts
**Context**: attach-resume-protocol, Phase 3 planning

## Context

requirements.md's Scope commits to "one shared reconnect-loop specification (backoff policy, resume-token handling, `GapExceeded`-triggers-fallback), written once in the proto doc comments as the authoritative contract, implemented identically by `tymux-cli`, `clients/ts`, and `clients/go`." Concrete numbers are required — a specification that says "use reasonable exponential backoff" isn't implementable identically across three languages.

This repo has no production usage data to derive these numbers from (requirements.md's Feasibility Risks names this explicitly for replay-buffer sizing, and the same gap applies to timing constants). The nearest available reference point is `stapler-squad`'s own `ReconnectLoop` (`session/tmux/stream.go`), which the stapler-squad-integration plan (Task 2.5.2a) already judged reasonable for this class of drop: "bounded retry with jittered backoff... give up after a configurable max." That plan didn't pin exact numbers either, so this ADR picks concrete ones informed by that judgment, not measured against real traffic.

## Decision

| Constant | Value | Owner |
|---|---|---|
| Reconnect backoff, initial delay | 200ms | Client (shared spec) |
| Reconnect backoff, multiplier | x2 per attempt | Client (shared spec) |
| Reconnect backoff, cap | 8s | Client (shared spec) |
| Reconnect backoff, jitter | +/-20% | Client (shared spec) |
| Reconnect backoff, give-up threshold | 14 attempts (revised from 8 — see Revision below) | Client (shared spec) |
| App-level heartbeat interval | 15s | Server (`tokio::time::interval`) |
| Client heartbeat timeout | 45s (3x interval) | Client (shared spec) |
| Grace period duration | 60s | Server (`TYMUXD_GRACE_PERIOD_MS`, default) |
| HTTP/2 transport keepalive interval | 30s | Both (tonic builder config) |
| HTTP/2 transport keepalive timeout | 10s | Both (tonic builder config) |

The explicit ordering constraint `heartbeat_timeout (45s) < grace_period_duration (60s)` is deliberate (pitfalls.md's flagged need for this, not two independently-tuned constants — see ADR-003's Consequences): a well-behaved client should have already started reconnecting, by its own detection, before the server's grace period would even lapse.

These numbers are written into `Attach`'s RPC-level doc comment in `proto/tymux/v1/tymux.proto` (Task 1.1.1c) as the authoritative, language-agnostic contract every client implementation must match.

## Rationale

- **A specification needs numbers, not adjectives.** "Reasonable exponential backoff" is not independently implementable by three separate client codebases and expected to converge on matching behavior.
- **Informed, not measured.** 200ms-start/x2/8s-cap/jitter is a standard, widely-used shape (matches common retry-library defaults across ecosystems) rather than anything derived from this project's own traffic — there is none yet. This is stated explicitly here rather than presented as a tuned value, per this project's evidence discipline: a rationale ("why 200ms") is as falsifiable as a fact, and inventing a tighter justification than "reasonable default, informed by stapler-squad's own judgment call" would overclaim.
- **Grace period at 60s** is likewise a judgment call, not a measurement — chosen as comfortably longer than a typical brief network blip (the scenario Success Metrics describes) while still being short enough that a genuinely abandoned pane's window geometry doesn't stay visibly wrong for very long.

## Revision: give-up threshold raised from 8 to 14 attempts

The originally-accepted 8-attempt schedule (200ms, x2, capped 8s) sums to 20,600ms of nominal backoff delay across 7 inter-attempt waits (200+400+800+1600+3200+6400+8000) — about 28.6s once the pre-mortem's own estimate of ~1s per request-attempt duration is folded in. That total is **shorter** than ADR-003's `grace_period_duration` (60s) — the exact window the grace period exists to survive (a daemon restart/upgrade taking 30-60s). A `tymux-cli` client following the original 8-attempt schedule would exhaust its retries and exit *before* the server-side safety net it's supposed to rely on had even expired (pre-mortem.md finding #2, P2).

**Resolution: extend the give-up threshold, not accept the gap.** Raising the threshold to 14 attempts adds 6 more capped-at-8s delays on top of the original 7-delay, 20,600ms schedule: `20,600 + 6 x 8,000 = 68,600ms` (~68.6s) of nominal cumulative backoff — comfortably >= `grace_period_duration` (60s), with an ~8.6s margin. This was chosen over "document the mismatch as an accepted tradeoff" because the failure mode it would otherwise accept (users see `tymux attach` exit with an error during an entirely normal daemon upgrade) is a regression the grace period was explicitly built to prevent one layer up — extending one client-side constant is cheaper than accepting that regression. The invariant is enforced, not just asserted in this doc: `crates/tymux-cli` Task 6.1.1f adds a unit test asserting `sum(backoff_schedule_delays) >= grace_period_duration`, so a future change to either this ADR's or ADR-003's numbers can't silently reopen the gap.

This margin is computed on the *nominal* (pre-jitter) schedule; +/-20% jitter can shift any individual attempt's actual delay but is not accounted for as a worst-case reduction here — the same "informed default, not measured" standard the rest of this ADR uses, not a hardened SLA.

## Consequences

- These are the first numbers to revisit if real usage ever contradicts them — e.g. if 14 attempts/~68.6s total nominal backoff proves too short (or unnecessarily long) for a real flaky-network scenario, or if 60s grace period proves too long/short in practice. No telemetry currently exists to make that judgment; Epic 4.1's resume-outcome counter is a first step toward having some.
- Because these live in proto doc comments (not a runtime-negotiated config), changing them later is itself a coordinated change across `tymux-cli`, `clients/ts`, and `clients/go` — the same "shared spec" property that makes them useful now makes them slightly more expensive to revise later. Accepted as the right tradeoff: divergent per-client backoff behavior would be a worse outcome than an occasional coordinated tuning pass. Task 4.1.2a's per-client conformance test exists precisely to catch a client silently drifting from these numbers.
- The real-hardware/real-network verification gap noted in requirements.md's Feasibility Risks (tonic keepalive behavior under real network conditions is unverified in CI/local dev) applies most directly to the two HTTP/2 keepalive constants in this table — flagged in plan.md's Unresolved Questions and Risk Control sections, not resolved by this ADR.
- Raising the give-up threshold also raises the worst-case time a permanently-unreachable daemon leaves `tymux-cli` retrying before surfacing its give-up error (~68.6s nominal, up from ~20.6s) — accepted as the right tradeoff given the alternative (giving up too early during a legitimate restart) is the more common and more disruptive real-world case this ADR is tuned for.
