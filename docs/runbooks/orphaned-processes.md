# Runbook: cleaning up orphaned pane processes after a `tymuxd` restart

## Background

`tymuxd` never reattaches to a pane's OS process after a restart —
`Engine::revive_session` (`crates/tymux-core/src/engine.rs`) always spawns a
*fresh* process on an explicit `tymux revive`. If a pane was still alive
when the prior `tymuxd` instance died or was restarted (crash, deploy,
losing its controlling terminal, etc.), that old process is orphaned: it
keeps running, unowned, until it exits on its own or someone finds and
kills it. See Story 1.1.4 in
`project_plans/stapler-squad-integration/implementation/plan.md` for why
this is an accepted trade-off rather than a bug to fix here.

At startup, `tymuxd` logs a best-effort upper-bound estimate of how many
such orphans a restart may have left behind:

```
possible orphaned processes from prior tymuxd instance count=<N>
```

(`count=0` logs at `info`; `count>0` logs at `warn`.) This is an
*approximation*, not a guarantee — a counted record may already have
exited cleanly before the restart. Treat a nonzero count as "worth
checking," not as "N processes definitely leaked."

## Finding an actual orphan

1. Note the `count` from the startup log line, and the persisted sessions
   directory `tymuxd` logged (`dir=...`, typically
   `$XDG_STATE_HOME/tymux/sessions` or `~/.local/state/tymux/sessions`).
2. For each session file in that directory, read the persisted pane
   records' `command` and `cwd` — these are the process(es) that were
   flagged live before the restart.
3. List real, currently-running processes and cross-reference:

   ```
   ps -eo pid,ppid,pgid,lstart,cmd
   ```

   Look for entries whose `cmd` matches a persisted pane's `command`/`cwd`
   combination, whose `ppid` is **not** the current `tymuxd`'s PID (an
   orphan has no live parent tracking it — its original parent is gone),
   and whose `lstart` predates the current `tymuxd` instance's start time.

4. **Confirm via `lstart`, not command text alone, before touching
   anything.** Command-line text is not a unique identifier: PIDs and
   command lines get reused by unrelated processes over a long-lived
   host's uptime. A process that merely *looks* like the orphaned pane's
   command but started *after* the restart is not the same process — do
   not kill it. Only a candidate whose `lstart` is earlier than the
   restart, combined with the `ppid`/`pgid` mismatch above, is safe to
   treat as the orphan.

## Cleaning up

Once a candidate is confirmed by both the command/cwd match and the
`lstart` check:

```
kill <pid>          # SIGTERM first — give it a chance to exit cleanly
# if it doesn't exit:
kill -9 <pid>
```

Verify it's gone with another `ps -p <pid>` before considering it done.

## What not to do

- Do not script a blanket "kill anything whose `cmd` matches a persisted
  record" cleanup — the PID/command-line reuse hazard above makes that
  unsafe on any host that's been up a while.
- Do not treat the startup `count` as an exact number of processes to
  kill; it's an upper bound on candidates, not a target count.
