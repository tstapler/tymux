# Adversarial Review: unix-socket-auth

**Date**: 2026-08-29 (iteration 3 — final)
**Verdict**: CLEAN (0 blockers, 0 concerns, 3 minors — all optional/low-value-add)

## Prior concerns — verification status

All 6 concerns from iteration 1 were checked against the current
`plan.md`/`requirements.md`/`research/build-vs-buy.md` and are **fixed**:

1. **Scope drift (`--disable-tcp-loopback` vs. requirements.md)** — fixed.
   `requirements.md`'s Risk Control section now has an explicit
   "Added during planning, not in this section originally" paragraph
   authorizing the flag, citing architecture research's "cheap now"
   rationale and pointing at `plan.md`'s Epic 1.3/4.3. No longer a
   unilateral plan-side addition.
2. **CI cross-uid reject tests framed as still-open** — fixed.
   `plan.md`'s Unresolved Questions entry is now checked `[x]` and states
   the CI-does-not-run-as-root fact directly (verified against
   `.github/workflows/ci.yml`), rather than "confirm before merging."
   Task-ID cross-references (6.4.1c, 7.3.1b, 8.3.1c) were checked against
   the actual task headers in Phases 6/7/8 and all three match exactly —
   no ID drift.
3. **`bind_uds_listener` chmod'ing a directory it doesn't own** — fixed.
   `default_uds_socket_path` now nests both branches under a
   tymuxd-owned subdirectory (`tymuxd/` vs. `tymuxd-<uid>/`), and Story
   2.2.1 adds a dedicated AC + test
   (`bind_uds_listener_never_touches_permissions_of_a_pre_existing_grandparent_directory`)
   proving the outer (session-manager-owned) directory's mode is left
   untouched.
4. **Restart-with-open-connection untested** — fixed. New Story 5.2.2
   (`crates/tymuxd/tests/uds_socket_lifecycle.rs`) has two full
   Given-When-Then ACs — clean SIGTERM restart and unclean SIGKILL
   restart — both asserting the pre-existing resume-on-reconnect path
   completes over a freshly re-bound UDS socket, with concrete task
   breakdown (5.2.2a/5.2.2b).
5. **`--socket-group` EPERM path unhandled** — fixed. `bind_uds_listener`
   now has a dedicated `EPERM`-detecting branch with a distinct message
   ("is not a member of"), backed by a unit-level AC (Story 2.2.1) and an
   end-to-end wiring test (Task 4.2.1d,
   `main_exits_nonzero_with_clear_message_when_socket_group_membership_denied`)
   that proves the fatal-exit path through `main()`, not just the pure
   function.
6. **ADR-003 tower/hyper-util lacking a build-vs-buy section** — fixed.
   `research/build-vs-buy.md` §3b now covers this explicitly, citing
   `Cargo.lock`'s `tower 0.4.13`/`hyper-util 0.1.20` as already-transitive
   dependencies.

## Blockers

(none)

## Concerns

(none — see "Resolved in iteration 3" below)

### Resolved in iteration 3

- [x] **UDS accept-loop transient errors** — verified, not a real gap.
  The fix pass read tonic 0.12.3's actual source
  (`~/.cargo/registry/.../tonic-0.12.3/src/transport/server/mod.rs:626-628`,
  inside the same `Server::serve_with_shutdown` accept loop
  `serve_with_incoming_shutdown` calls): the `Some(Err(e))` arm is
  `trace!(...); continue;` — it does not `break` the accept loop, only
  `None` (stream end) does. A transient `accept()` error (e.g.
  `EMFILE`/`ENFILE`) is logged at `trace!` and the loop keeps accepting;
  it never silently kills the UDS listener or falls clients back to TCP.
  This session's independent architecture-review pass re-verified the
  same source lines directly rather than trusting the citation, and
  confirmed it's accurate and complete. `plan.md` now cites this in the
  Observability Plan and at Task 4.2.2a's `.map_ok` wiring, flagging the
  one residual (non-blocking) gap: tonic logs the dropped error at
  `trace!`, which is invisible under this project's default log-level
  filter — a documentation callout, not a design defect.

## Minors

- `resolve_gid_by_name`'s safety doc comment (Task 1.2.1b) is unchanged
  from iteration 1 — still justifies the `getgrnam` FFI call as safe
  because it runs "during single-threaded daemon startup," but `tymuxd`
  runs under `#[tokio::main]`'s default multi-threaded runtime, so worker
  OS threads already exist (idle, but present) at that point. Low
  practical risk (one-time startup call, no other identified concurrent
  NSS caller), but the comment still overclaims an invariant the code
  doesn't actually establish.
- Phase 7/8 (Go/TS) integration tests still cover only same-uid accept
  and mismatched-uid reject, not the group-based-access accept path
  (which `tymuxd`'s own Story 5.1.2 does cover directly, Linux-gated).
  The fix pass did not add the one-line "omission is deliberate" note
  iteration 1 suggested; still low value-add since the decision is
  server-side-only and already covered there, but the asymmetry itself
  is unaddressed.
- The "documented removal plan" success metric is still satisfied
  thinly — `requirements.md`'s Risk Control text still defers actual
  removal to an unscheduled "follow-up project," and `plan.md`'s Risk
  Control section doesn't add a concrete timeline/version target beyond
  restating that framing. Unchanged from iteration 1; acceptable given
  requirements.md itself is the source of the vagueness, but still reads
  as an open item rather than a plan.

## Fresh look at the Epic 3.2 redesign (`PreAuthorizedUnixStream`/`UdsAuthDecision`/split interceptor)

- **Panic/error surface at accept time**: `PreAuthorizedUnixStream::new`
  calls `inner.peer_cred().ok()`, swallowing any `io::Error` to `None`;
  the decision then evaluates to `authorized: false` via
  `cred.as_ref().is_some_and(...)` short-circuiting on `None`. This fails
  closed correctly — a `peer_cred()` failure produces a clean rejection,
  not a panic or a crash — and the function itself is infallible
  (`-> Self`, no `Result`). No gap found here; this is the one failure
  mode the task asked to check that the plan actually gets right.
- **Testability of `UdsPeerCredInterceptor` in isolation**: no gap. Task
  3.2.1b-test builds a bare `Request<()>` and inserts `UdsAuthDecision`
  directly via `req.extensions_mut().insert(...)` — the same pattern
  `BearerAuthInterceptor`'s existing tests already use — so the
  interceptor's "read the cached decision" behavior is fully unit
  -testable without ever constructing a real `UnixStream`/
  `PreAuthorizedUnixStream`. The split actually *improves* testability
  over the original (per-RPC-recomputing) design, which needed a real
  `UCred` to test at all.
- **Scope vs. the Performance SLO**: not over-engineered. The whole
  point of `PreAuthorizedUnixStream` is to satisfy requirements.md's
  literal "peer-credential check happens once per connection... no
  measurable per-call overhead" NFR, which the plan's own Pattern
  Decisions table shows the *original* design violated (re-deriving the
  decision, including the `/proc` read, on every RPC). The wrapper is
  minimal — delegates `AsyncRead`/`AsyncWrite` straight through, holds a
  `Copy` decision struct, one `Connected` impl — proportionate to the
  problem it fixes, not added machinery beyond it.
