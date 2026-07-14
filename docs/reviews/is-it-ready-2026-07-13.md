# Is It Ready? — Shipping Gate Report

**Branch**: master **PR**: none (no remote configured) **CI**: N/A — no remote/PR, `.github/workflows/ci.yml` has never actually run **Date**: 2026-07-13 **Iteration**: 1 of 5 (auto-fix loop paused — see note below)

**Scope**: everything built so far (diff `e952971..HEAD`) — the Rust workspace (`tymux-core`, `tymux-proto`, `tymuxd`, `tymux-cli`), proto schema, CI config, brand/logo assets, and the UX journey map.

> **Note on process**: this run was invoked with "document findings for us to work on and fix," which I read as wanting a report to work through yourselves rather than this command's default behavior (auto-fix every blocking issue immediately without asking). **No fixes have been applied.** Say the word if you want me to run the fix loop instead.

| Dimension | Status | Blocking | Notes |
|---|---|---|---|
| CI / PR | ⚪ N/A | — | No remote configured, nothing to check |
| Plan | 🔴 | 2 | README's documented Ctrl-d behavior is false today |
| Architecture | 🟡 | 4 | Data-model mismatch + coupling, not systemic |
| Code Quality | 🟡 | 2 | Mostly swallowed errors + DRY on hardcoded size |
| Tests | 🔴 | 3 | One PTY-level test; the entire gRPC/CLI surface is untested |
| Security | 🟡 | 2 | Safe by default; unsafe if misconfigured or exposed |
| Product/UX | 🔴 | 4 | Same hang bug, confirmed independently against the code |
| Ops | 🔴 | 5 | Zero structured logging; failures are invisible |

## Verdict: ⚠️ FIX THEN SHIP

*(Per this command's own criteria, a hard 🛑 HOLD only triggers on Security 🔴 or systemic Architecture 🔴 — neither happened here. But 4 of 7 dimensions independently converged on the same root-cause bug, which is a stronger signal than the verdict label alone conveys — see below.)*

### The one issue 5 of 7 reviewers found independently
**Exiting the shell in an attached pane (Ctrl-d) hangs forever.** Plan Compliance, Architecture, Ops, and Product/UX all flagged this as their top blocking issue, and the earlier UX journey-mapping pass (`docs/ux/journey-map.md`) found it first. Root cause, confirmed by multiple reviewers reading the same three call sites:
- `crates/tymux-core/src/pane.rs:72-83` — the pty reader thread gets `Ok(0)` on child exit and just `break`s; nothing is signaled to anyone
- `crates/tymux-core/src/pane.rs:79` — the pane's output `broadcast::Sender` is never dropped, so it stays "alive" forever
- `crates/tymuxd/src/main.rs:~168` — the daemon's forwarding task blocks forever on `output_rx.recv().await`, awaiting a message that will never arrive
- `proto/tymux/v1/tymux.proto:95-100` — `AttachEvent` has no variant to express "pane exited" even if the above were fixed
- `crates/tymux-cli/src/main.rs:~134` — the CLI's `inbound.message().await` blocks forever in turn, leaving the terminal stuck in raw mode

This directly contradicts what `README.md` currently documents ("Ctrl-d ... ends the pane; the daemon keeps running for the next session") and is very likely the first thing any new user hits. **This should be fixed before anything else on this list.**

### Blocking Issues

1. **[Plan/Architecture/Ops/Product-UX]** The Ctrl-d hang above — `crates/tymux-core/src/pane.rs:72-83`, `crates/tymuxd/src/main.rs` (Attach forwarding task), `proto/tymux/v1/tymux.proto:95-100`. Needs: child-exit detection (`try_wait()`/SIGCHLD), a new `AttachEvent` variant for pane-exit, and the forwarding task treating that as a clean stream close instead of blocking.
2. **[Ops/Code Quality]** Errors are silently discarded throughout the attach path — `crates/tymuxd/src/main.rs:183,186` (`let _ = pane_for_input.write_input(...)`, `let _ = pane_for_input.resize(...)`) and `crates/tymux-core/src/pane.rs:79` (`let _ = output_tx.send(...)`). Failures are invisible to both the client and any operator.
3. **[Ops]** No structured logging anywhere — the daemon has exactly one `println!` (on startup). Combined with #2, failures in `tymuxd` are completely unobservable; there's no `log`/`tracing` crate in use at all.
4. **[Ops]** Spawned OS threads (`pane.rs` reader thread) and `tokio::spawn`ed tasks (`tymuxd/main.rs` forwarder/input tasks) are never joined or monitored — if one panics or gets stuck, nothing surfaces it.
5. **[Tests]** Test coverage is effectively zero outside one PTY-level test in `tymux-core`. `tymuxd`'s entire gRPC surface (`CreateSession`, `ListSessions`, `KillSession`, `CapturePane`, and especially the `Attach` bidi-stream handler's forwarding/input/resize logic) and `tymux-cli`'s command parsing/attach flow have no automated tests at all — error paths (invalid UUIDs, missing sessions) are unverified.
6. **[Architecture/Code Quality]** `engine.rs::list_sessions()` returns an untyped `Vec<(Uuid, String, Uuid, Uuid)>` tuple, coupling `tymuxd` to `SessionState`'s internal field order with no compiler-enforced safety net if a field is added or reordered.
7. **[Architecture]** The proto models `Session → repeated Windows → repeated Panes`, but the engine hardcodes exactly one pane per session (acknowledged in a code comment as "not built yet"). Adding splits/multiple windows later means touching all three layers (engine, proto usage, daemon) at once — worth deciding now whether that's near-term work or genuinely deferred.
8. **[Architecture/Tests]** `crates/tymux-cli/src/main.rs:71,93` hardcodes `session.windows[0].panes[0].id` array indexing with no bounds check — this will panic the moment the one-pane-per-session assumption above changes.
9. **[Security]** `TYMUXD_ADDR` is accepted with no validation (`crates/tymuxd/src/main.rs:~201`). The default (`127.0.0.1`) is safe, but nothing stops (or even warns on) binding to `0.0.0.0` or another non-loopback address — combined with zero authentication and `CreateSession`'s ability to spawn an arbitrary command, that misconfiguration is unauthenticated remote code execution. Separately, there's no per-pane authorization anywhere: any client that can reach the port can `Attach`/`CapturePane`/`KillSession` against any `pane_id`, with no ownership check — a real gap given the project's own stated AI-agent/web-frontend multi-client use case.
10. **[Plan/Product]** `KillSession` is fully implemented server-side but has no CLI subcommand (`tymux-cli`'s `Command` enum only has `New`/`Ls`/`Attach`) — a human user has no way to invoke a documented, working RPC.

### Non-Blocking

- No SIGWINCH handling and no initial `Resize` sent on attach — pane geometry is permanently stuck at the hardcoded 24×80 default regardless of the user's real terminal size (`crates/tymux-core/src/engine.rs:9-10`, `crates/tymuxd/src/main.rs:~31`, `crates/tymux-cli/src/main.rs`)
- No graceful shutdown on SIGTERM/SIGINT in `tymuxd` — active panes are dropped with no cleanup or client notification
- Sessions `HashMap` only shrinks on explicit `KillSession` — nothing garbage-collects a session whose client disconnected or whose pane died
- Broadcast channel (`pane.rs`, buffer 1024) has no backpressure visibility — a full buffer silently drops output frames
- Magic numbers/unexplained constants: buffer size mismatch (1024 in `tymux-cli` vs. 4096 in `pane.rs`), the `0` third argument to `vt100::Parser::new(rows, cols, 0)`, unexplained bitflag values (1/2/4/8) for cell attributes, and the hardcoded window name `"0"`
- All CLI error paths print `anyhow`'s raw `Debug` output rather than a friendly, actionable message (connection-refused, session-not-found, and pane-not-found all look the same)
- No liveness/exit-code field anywhere in the proto (`Session`/`Pane`/`PaneSnapshot`/`AttachEvent`) — the root protocol gap behind blocking issue #1, also independently limits `CapturePane` (a dead pane's last snapshot is indistinguishable from an idle one)

### Auto-Fixed This Iteration
None — auto-fix loop intentionally not run this pass (see process note above).

### Next Action
Fix blocking issue #1 (the Ctrl-d hang) first — it's a broken promise on the project's most basic interaction, and issues #2–4 (silent errors, no logging, unjoined tasks) are really the same underlying observability gap that made #1 possible to ship unnoticed in the first place, so fixing them together is the highest-leverage next step.
