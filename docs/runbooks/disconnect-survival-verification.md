# Runbook: verifying the abrupt-disconnect pane-kill fix on real hardware

## Background

`tmux`'s whole value proposition is that a session survives the client
going away. `tymuxd` had (has, pending this verification) a bug where an
*abrupt* client disconnect — the client's controlling terminal hanging up,
e.g. a crashed terminal emulator, a dropped SSH connection, a laptop losing
power, as opposed to a clean `C-b d` or `SIGTERM` — killed the pane's own
child process, not just the attach stream. See ADR-002
(`project_plans/stapler-squad-integration/decisions/ADR-002-tymuxd-session-detachment.md`)
for the full root-cause investigation.

**The fix** (`libc::setsid()` called at the top of `tymuxd`'s `main()`,
`crates/tymuxd/src/main.rs`) is implemented, matches real tmux's own daemon
design, and is safe/low-risk — but it is **unverified against the actual
bug**. Every sandboxed dev container this project has been built in already
runs `tymuxd` as its own session leader with no controlling terminal
(`SID=PGID=PID`, `TT=?`) *before* the fix is even applied — the exact state
`setsid()` itself would produce. That makes the fix a structural no-op in
any such sandbox: it can neither be confirmed nor refuted there, because the
precondition it targets (`tymuxd` actually having a controlling terminal to
lose) never existed in the test environment to begin with. Confirmed by
directly re-running the repro with the fix compiled in, inside the sandbox:
it still failed, for exactly this reason (see
`crates/tymux-e2e/tests/disconnect_survival_e2e.rs`'s doc comment for the
full 2026-08-21 findings).

This runbook is Tasks 1.1.2c/1.1.2d/Story 1.1.3 from
`project_plans/stapler-squad-integration/implementation/plan.md` — the one
piece of Epic 1.1 that requires a human with real hardware access, which no
sandboxed agent session can substitute for.

## What "real hardware" means here

Two separate environments are required, not one — a single machine isn't
enough to trust the fix in production (pre-mortem P1 #1):

1. **A real, non-containerized machine** with an actual controlling
   terminal — your own laptop/desktop is fine, as long as you're running
   `tymuxd` from a normal interactive shell (not inside Docker, not inside
   this project's dev container, not inside `tmux`/`screen` itself, which
   would give `tymuxd` a different kind of terminal ancestry than the bug
   targets).
2. **A systemd-managed host** — a machine (can be a VM) where `tymuxd` runs
   as a systemd unit. This exercises the *other* branch of the fix: when
   `setsid()` returns `EPERM` because `tymuxd` is already a session leader
   (which systemd guarantees for services it starts), the fix's tolerance
   path (Task 1.1.2b) needs to actually run, not just be assumed correct.

If you only have access to one of these, run it anyway and record which
one — a single confirmed environment is still real signal, just not the
full pre-mortem P1 #1 bar. Note that clearly when you record findings (see
Recording Results below).

## Step 1: Build in release mode

```bash
cd tymux
cargo build --release --workspace
```

Use the release binaries (`target/release/tymuxd`, `target/release/tymux`)
for every step below — matching Task 1.1.1a's original repro.

## Step 2: Automated repro — `pane_survives_abrupt_disconnect`

The exact repro is already written as an ignored e2e test. Temporarily
un-ignore it to run it directly (do not commit this change — see Step 5):

```bash
cd tymux
# Comment out or delete the #[ignore = "..."] line above
# pane_survives_abrupt_disconnect in crates/tymux-e2e/tests/disconnect_survival_e2e.rs,
# then:
cargo test --release --package tymux-e2e --test disconnect_survival_e2e \
  pane_survives_abrupt_disconnect -- --exact --nocapture
```

**Expected if the fix works**: the test passes — `CapturePane` after the
abrupt disconnect returns `Liveness::Live`.
**Expected if the fix doesn't work**: the test fails with `pane should
still respond to CapturePane after an abrupt disconnect` or an assertion
that `liveness` isn't `Live`.

Run it **10 consecutive times** before trusting a pass (Story 1.1.3's own
acceptance criterion — a single pass isn't enough to rule out flakiness):

```bash
for i in $(seq 1 10); do
  cargo test --release --package tymux-e2e --test disconnect_survival_e2e \
    pane_survives_abrupt_disconnect -- --exact --nocapture || echo "FAILED on run $i"
done
```

## Step 3: Manual repro + `ps` capture (matches Task 1.1.1a/b exactly)

The automated test above is the same repro Task 1.1.1a used, but the manual
version lets you directly inspect `tymuxd`'s session/controlling-terminal
state at the moment of disconnect — the actual signal that confirms *why*
it passed or failed, not just that it did.

1. Start `tymuxd` from a real interactive shell:
   ```bash
   ./target/release/tymuxd &
   TYMUXD_PID=$!
   ```
2. From another terminal, create and attach a client in one step (default
   address is `http://127.0.0.1:7419`; only pass `--addr` if `tymuxd` is
   listening elsewhere):
   ```bash
   ./target/release/tymux new verify-disconnect
   ```
3. While attached, in a third terminal, find the pane's child shell PID
   (it's a direct child of `tymuxd`) and capture both processes' session
   state — this is the exact command from Task 1.1.1b:
   ```bash
   ps -o pid,ppid,pgid,sid,tty,cmd --ppid "$TYMUXD_PID"   # find the pane child's PID
   ps -o pid,ppid,pgid,sid,tty -p "$TYMUXD_PID"
   ps -o pid,ppid,pgid,sid,tty -p <pane-child-pid>        # from the command above
   ```
   Record the output now, before the disconnect, as a baseline.
4. Abruptly disconnect: close the terminal emulator window running the
   `tymux attach` client directly (not `exit`, not `C-b d`) — or, if
   scripted, kill the *terminal emulator's* process, not the `tymux`
   client process itself (killing the client directly doesn't reproduce
   the bug — see the e2e test's doc comment, isolating experiment #2).
5. Immediately re-run the same `ps` commands against `tymuxd` and the pane
   child PID.
6. **What to look for**:
   - `tymuxd`'s row: is `TT` a real device (`pts/N`) or `?`? Is
     `SID`/`PGID` equal to its own `PID` (session leader, no controlling
     terminal) or does it share a `SID` with something else?
   - The pane child's row: is it still present and alive, or gone /
     `<defunct>`?
   - **Confirms the fix worked**: the pane child process is still alive
     and its `ps` row is unchanged from the baseline.
   - **Confirms the fix didn't work**: the pane child is gone or
     `<defunct>` shortly after the disconnect.

## Step 4: Repeat on the second environment

Re-run Step 2 and Step 3 in full on whichever of the two environments
(container / systemd host) you didn't use first. Record both outcomes
separately — do not average or assume the second matches the first.

## Step 5: Recording results

Update `pane_survives_abrupt_disconnect`'s doc comment in
`crates/tymux-e2e/tests/disconnect_survival_e2e.rs`, **appending** a new
dated section after the existing 2026-08-21 sandbox findings (do not
delete or edit the sandbox investigation notes — they're the reason this
runbook exists). Include:

- Date, machine description (bare-metal / VM / container / systemd
  unit), and which of the two required environments it satisfies.
- The `ps` output captured in Step 3, before and after disconnect, for
  both `tymuxd` and the pane child.
- The automated test's result across the 10 consecutive runs (Step 2).
- If either environment's finding diverges from the other, or from the
  fix's expected behavior: **do not un-ignore the test**. Record the
  divergence and stop — Epic 1.1 needs a follow-up investigation pass
  before proceeding, per Task 1.1.2d's own acceptance criterion.

## Step 6: Un-ignore the test, for real

Only after **both** required environments (Step 4) confirm the fix, with
10/10 consecutive passes each:

1. Remove the `#[ignore = "..."]` attribute from `pane_survives_abrupt_disconnect`
   permanently (a real commit, not the temporary local edit from Step 2).
2. Commit alongside the doc comment update from Step 5, in
   `crates/tymux-e2e/tests/disconnect_survival_e2e.rs`.
3. This closes Story 1.1.3 and Epic 1.1 — update
   `project_plans/stapler-squad-integration/implementation/plan.md`'s
   Unresolved Questions entry (the first bullet under "## Unresolved
   Questions") to `[x] Resolved`, with a summary of what was confirmed.
4. Open a PR through the normal flow (draft first, review, CI, merge).
