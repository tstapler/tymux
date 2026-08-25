# UX Design: attach-resume-protocol

**Date**: 2026-08-24
**Role of this doc**: `research/ux.md` already resolved the design question ("invisible
replay, minimal fallback signal, no spinner/banner") — that's not re-litigated here. This
doc validates that `implementation/plan.md`'s actual surfaces implement that resolution
with no dead ends, and gives the one surface the plan explicitly left open (Story 6.1.1a's
give-up error) a concrete, testable design.

**Surface count**: 5 (4 interactive/terminal-stream surfaces + 1 non-interactive), plus one
cross-cutting input-handling note spanning Surfaces 1 and 3 ("Input during a drop or retry
window") — not counted as a separate surface since it has no chrome or rendering of its own.
**UX acceptance criteria written**: 24 (this repair pass added AC3.7, closing the
`pending_input`-on-give-up gap; AC3.6 was extended in place with a discard-on-Detach
clause, not renumbered)

---

## Surface inventory

| # | Surface | Type | Plan reference |
|---|---|---|---|
| 1 | Successful resume (happy path) | Interactive (terminal stream) | Story 6.1.1 AC1, Story 6.3.1 |
| 2 | Gap-exceeded fallback | Interactive (terminal stream) | Story 6.1.1 AC3, Task 6.1.1c |
| 3 | Reconnect exhaustion (give-up) | Interactive (terminal stream + process exit) | Story 6.1.1 AC2, Task 6.1.1a — **left open by the plan, designed here** |
| 4 | Manual reattach after being gone a while | Interactive (terminal stream), Phase 6 cut-candidate | Story 6.2.1 |
| 5 | Daemon structured logs + `tymux_attach_resume_outcome_total` | Non-interactive (operator-facing) | ADR-004 Observability Plan, Epic 4.1 |

All four interactive surfaces render through the same terminal — there is no separate
screen, modal, or window per surface. "Wireframe" below means literal terminal-buffer
content, since that's what the user actually looks at.

---

## Surface 1: Successful resume (happy path)

### Validation against research

`research/ux.md` §2 already ruled out a visible indicator here, citing mosh/ET's
"reconnection feels like the network was never gone" precedent. Confirmed against the
plan's actual mechanics — this holds:

- Story 6.3.1 AC1 requires buffered `OutputChunk` replay to go through the exact same
  `stdout.write_all(&bytes)` call as live output (`crates/tymux-cli/src/main.rs:552`) — no
  new code path, no inserted bytes.
- `chrome_message_for_event` (`main.rs:812-818`) returns `None` for
  `attach_event::Payload::Output(_)` — confirmed by the existing test
  `chrome_message_for_event_is_none_for_output_bytes` (`main.rs:972-974`). Replayed chunks
  arrive as `Output` payloads, so they never trigger chrome. This is the concrete
  mechanism that keeps the happy path silent — not just a design intent, but a testable
  fact about the match arms.

No design work needed on the interaction pattern itself. What follows documents the flow
precisely enough to test it.

### Flow diagram

```
 t0: user is attached, terminal showing live pane output
     ┌────────────────────────────────┐
     │ $ npm run build                 │
     │ Compiling...                    │
     │ ▊                                │  ← cursor, live
     └────────────────────────────────┘

 t1: connection drops (network blip, daemon restart, etc.)
     — terminal FREEZES at last-rendered frame. No chrome, no marker.
     — user may or may not notice yet (this is the only "signal": stale screen).

 t2: CLI's reconnect loop (Epic 6.1) detects the drop, redials with
     resume_from_seq = last seq processed, inside the backoff schedule
     (200ms → 400ms → 800ms → ... capped 8s). Entirely off-screen.

 t3: daemon accepts, replays buffered OutputChunks (seq gap only) through
     the SAME stdout.write_all() path as live output, then live output
     continues.
     ┌────────────────────────────────┐
     │ $ npm run build                 │
     │ Compiling...                    │
     │ Compiling module foo... [buffered, now visible]
     │ Compiling module bar... [buffered, now visible]
     │ Build succeeded.        [live, arrived after reconnect]
     │ ▊                                │
     └────────────────────────────────┘
```

The user-visible transition from t1 to t3 is: frozen screen → catches up → continues.
No banner ever appears between them.

### Acceptance criteria

- **AC1.1**: Given an active `tymux attach` session and a drop that resolves within the
  backoff schedule, when the connection is restored, then the terminal shows the buffered
  output followed by live output with no inserted bytes, no full-screen redraw, and no
  `[tymux: ...]` chrome line — byte-for-byte, this must be indistinguishable from having
  stayed connected (Story 6.3.1 AC1, `research/ux.md` §2/§5 JTBD).
- **AC1.2**: No dead end applies — this is the non-error path; the exit path is the same
  as any normal attach session (Ctrl-\ detach, pane exit, etc.), unchanged by this feature.
- **AC1.3**: Accessibility — replayed bytes reach the terminal emulator/screen reader via
  the identical code path live output already uses, so no new accessibility gap is
  introduced (`research/ux.md` §3). **Known, accepted limitation, not to be solved here**:
  a resume replay after a long gap can deliver a large burst in one write rather than a
  gradual trickle, which reads worse to a screen reader than live pacing would. This is
  pre-existing terminal behavior (same as `cat largefile`), not new to this feature.
  Retention is capacity-bounded, not time-bounded — the actual knob is a byte ceiling
  (`DEFAULT_REPLAY_BUFFER_BYTES = 256 * 1024`, plan.md Task 2.1.1d), not a duration, so
  there is no "minutes" setting to watch for. The *effective* duration a given byte
  budget covers is emergent, not configured: it depends on a pane's own output
  throughput — a busy pane's 256KiB might span only seconds, while a mostly-idle pane's
  same 256KiB could span minutes. Flag in release notes if production telemetry
  (Surface 5's `tymux_attach_resume_outcome_total`, or a future histogram of replayed
  chunk age) shows effective retention commonly running in the minutes range in
  practice — not something a static config check can catch, per `research/ux.md` §3's
  explicit call not to require pacing/chunking for this appetite.
- **AC1.4**: Input typed during a drop — the ACs above cover only the output side of a
  resume. See "Input during a drop or retry window" immediately below (cross-cutting
  with Surface 3) for the input side, which this review found undocumented.

### Input during a drop or retry window (cross-cutting with Surface 3)

Everything above documents the output path; the plan is silent on input. Two facts,
confirmed by reading `crates/tymux-cli/src/main.rs` directly, bound the answer:

1. **Input and output cannot fail/reopen independently.** `attach()`'s `tx`/`rx` pair
   (`main.rs:492`, forwarding keystrokes) is the request-stream half of the *same*
   bidirectional `client.attach(Request::new(outbound))` call (`main.rs:522`) that
   `inbound.message()` reads output from. When that RPC dies, both directions die
   together; Task 6.1.1a's "reopens `Attach`" necessarily creates a fresh `tx`/`rx` pair
   alongside the fresh inbound stream. There is no scenario where output resumes while
   input is still bound to the dead stream — asymmetry only in where bytes originate,
   never in stream lifecycle.
2. **The raw-keystroke-reading OS thread is independent of the RPC's health.**
   `main.rs:505-518` blocks on real stdin and pushes into a bounded (capacity 64)
   `stdin_tx`/`stdin_rx` channel regardless of whether the gRPC call is alive. A
   keystroke typed during a drop is never lost at the point of being read from the
   terminal.

**Resolved by this repair pass** — both questions this section originally raised are now
decided in `implementation/plan.md` (Task 6.1.1a), not left open. The retry loop runs
inline inside `attach()`'s own scope, and its `select!` keeps polling `stdin_rx`
throughout backoff, racing it against each reconnect cycle. This settles the first
question directly: `stdin_rx` *is* polled during backoff, specifically so the Detach
binding stays reachable (Surface 3 AC3.6).

That leaves what happens to a `Forward`ed byte with no live `tx` to send it on — the
second question this section raised. This repair pass adopts the recommendation
previously proposed here rather than leaving it open: ordinary `Forward` output from the
reassembler is appended to a `pending_input: Vec<u8>` buffer during backoff and flushed as
the first `AttachRequest::Input` on the freshly reopened stream once a reconnect attempt
succeeds — a user who typed during a blip gets it applied once the connection is back,
not silently dropped. (This queueing is also load-bearing, not just a nicety: before this
task, an *unpolled* `stdin_rx` at least left keystrokes sitting in the bounded channel,
delayed but not lost; polling it now — required for Detach — and then discarding
non-Detach output would have made that strictly worse.)

**One narrower residual gap, deliberately not resolved here**: a non-Detach `Action`
fired during backoff (e.g. the copy-mode or split-pane prefix sequences) is still
discarded, not queued — those need a live `client`/RPC call to do anything useful, unlike
a plain forwarded keystroke, so there's no well-defined "apply on reconnect" semantics the
way byte-forwarding has. This is a narrower, deliberate simplification, not the question
the triad review flagged as a blocker — that blocker was specifically about Detach being
unreachable, which is now fixed (Story 6.1.1's new AC and Task 6.1.1g, plan.md).

**`pending_input`'s fate when backoff ends in something other than a successful
reconnect**: the flush described above only covers the happy path (a reconnect attempt
succeeds). The two other ways backoff can end — the user pressing Detach, or all 14
attempts being exhausted (give-up) — both discard `pending_input` rather than flushing or
persisting it. See Surface 3 AC3.6 (Detach) and AC3.7 (give-up) below for the full
reasoning; in short, both are deliberate exits from the retry loop, not temporary gaps, so
there is no live pane left to deliver queued keystrokes to and no contradiction with this
buffer's purpose.

**Known, accepted limitation — input-side burst risk, mirroring AC1.3's output-side
note**: AC1.3 above already flags and accepts that a long *output* gap replays as one
large burst write rather than a paced trickle. `pending_input`'s flush on reconnect has
the same shape on the *input* side and is not currently called out anywhere: if a user
keeps typing through a 20+ second backoff, the daemon receives the entire backoff
window's worth of queued keystrokes in a single `AttachRequest::Input` send the instant
the connection comes back, rather than the same bytes trickling in one at a time the way
live typing would deliver them. This can behave differently from real-time input — for
example a shell or REPL processing a large burst non-interactively versus
character-by-character, or a TUI application receiving what looks like a large paste
rather than individual keystrokes. As with AC1.3, this is a known, accepted limitation
given this project's appetite, not something to solve now — no chunked or paced flush of
`pending_input` is in scope; `research/ux.md` §3's explicit call not to require
pacing/chunking for this appetite applies equally to the input side, not just the output
side it was written about.

**Does the user know their typing is being captured, not lost? Resolution, grounded in
the actual code**: this review checked whether `tymux-cli` does any local echo of typed
characters itself, since a user who can't tell "queued" from "lost" during a frozen
screen risks retyping and causing duplicate input once both `pending_input` and any
freshly typed live input land after reconnect. Reading `crates/tymux-cli/src/main.rs`
directly: `RawGuard::enable()` (`main.rs:229-233`) calls
`crossterm::terminal::enable_raw_mode()`, which — per POSIX raw-mode semantics
crossterm implements — disables the local terminal driver's own `ECHO`, not just
canonical-mode line buffering; the stdin-reading OS thread (`main.rs:505-518`) only reads
bytes and pushes them into `stdin_tx`, and never itself writes anything to `stdout`. The
*only* place `attach()` writes to `stdout` at all is the inbound-event loop's `Output`/
`OutputChunk` arm (`main.rs:551-554`), which only fires for bytes that actually arrive
over the (during backoff, dead) `Attach` stream. So today, with zero code from this
feature, a keystroke typed while the connection is down produces no visible character at
all — not because tymux suppresses it, but because all visible feedback of typed input is
remote-echo-driven, over the same stream that's currently down (fact 1 above: input and
output share one stream's lifecycle). This is exactly the same mechanism Surface 1's
AC1.1 already relies on as the implicit "something's wrong" signal for the *output* side
(a stale, unchanging screen) — it extends to the *input* side for free, by the same code
path, for the same reason: **conclusion: no new explicit "typing captured" indicator is
warranted.** Adding one would directly contradict `research/ux.md`'s already-settled
no-spinner/no-banner "invisible reconnect" philosophy, and for a case where the terminal
already gives an honest, if implicit, signal — a user who types during a frozen screen and
sees nothing echo back already has the same "is this dead?" cue this feature's happy path
elsewhere treats as sufficient (AC1.1, AC3.4). The residual duplicate-input risk (user
retypes, then both the retype and the queued `pending_input` land) is not a new gap this
feature introduces — it is the identical risk a raw-mode terminal already carries on any
drop, resume feature or not, since local echo has never been available as a distinguishing
signal in this codebase.

---

## Surface 2: Gap-exceeded fallback

### Validation against research

`research/ux.md` §4 calls for "one bracketed inline line, in the same voice as the
existing `[tymux: output dropped]` marker." Checking the plan's actual proposed string
against the existing precedent in `main.rs`:

| | String | Write call | Bracket/prefix | Case | Terminal punctuation |
|---|---|---|---|---|---|
| Existing (`OutputGap`, `main.rs:815`) | `"\r\n[tymux: output dropped]\r\n"` | `write!` (`main.rs:562`) | `[tymux: ...]` | lowercase | none |
| Existing (`Exited`, `main.rs:814`) | `"\r\n[tymux: pane exited]\n"` | `writeln!` (`main.rs:557`, adds its own `\n`) | `[tymux: ...]` | lowercase | none |
| New (`GapExceeded`, plan Task 6.1.1c) | `"\r\n[tymux: reconnect gap too large, resyncing]\r\n"` | `write!` (mirroring `OutputGap`'s arm per Task 6.1.1c) | `[tymux: ...]` | lowercase | none |

**Consistent.** The new string matches bracket/prefix format, lowercase voice, and no
terminal punctuation. It correctly mirrors `OutputGap`'s `write!`-not-`writeln!` call
(not `Exited`'s), so it gets exactly one `\r\n` on each side with no doubled newline —
right choice, since `OutputGap`, not `Exited`, is the precedent for "printed inline
alongside output that continues," while `Exited` is a terminal state after which the loop
breaks. One structural difference from both existing arms — `GapExceeded` joins two
independent clauses ("gap too large" / "resyncing") with a comma, where `OutputGap` is a
single 2-word clause and `Exited` is a single 2-word clause — but that's a message-content
difference forced by the new event needing to convey two facts (why, and what happens
next), not a format inconsistency; the bracket/prefix/case/punctuation contract still
holds. The existing test's assertion shape (`assert_ne!` on textual distinctness between
arms) extends cleanly to a third arm — Task 6.1.1b's planned test does this.

### Flow diagram

```
 t0: user is attached, drop happened long enough that the daemon's replay
     buffer no longer retains the gap (retention exceeded)

 t1: reconnect succeeds, but the resume request is now outside the
     retained window. Daemon sends GapExceeded (carries oldest_available_seq).

 t2: CLI prints the fallback line, THEN the next Snapshot event renders as
     a normal full-screen redraw (this is the plan's committed ordering —
     Story 6.1.1 AC3: "prints the above line and then renders the
     following Snapshot event").
     ┌────────────────────────────────┐
     │ $ npm run build                 │
     │ Compiling...                    │
     │                                  │
     │ [tymux: reconnect gap too large, resyncing]
     └────────────────────────────────┘
                    ↓ immediately followed by
     ┌────────────────────────────────┐  ← full CapturePane redraw,
     │ (full current pane contents,    │    same as any fresh attach
     │  scrollback lost for the gap    │
     │  period — by design, not a bug) │
     │ ▊                                │
     └────────────────────────────────┘
```

### Acceptance criteria

- **AC2.1**: Given a resume request outside the replay buffer's retention, when the CLI
  receives `GapExceeded`, then it prints exactly
  `"\r\n[tymux: reconnect gap too large, resyncing]\r\n"` via `write!` (not `writeln!`),
  immediately before the next `Snapshot` renders as a full-screen redraw — no other visible
  state (Story 6.1.1 AC3).
- **AC2.2**: The message is textually distinct from `OutputGap`'s `"output dropped"` and
  `Exited`'s `"pane exited"` strings (extend `chrome_message_for_event_is_none_for_output_bytes`'s
  sibling test, `main.rs:958-969`, with the same `assert_ne!` pattern across all three
  arms — Task 6.1.1b covers the two-way case only today; the design here explicitly asks
  for one `assert_ne!` per pair, or a loop over all pairs, so a future fourth arm can't
  collide silently).
- **AC2.3**: No dead end — the fallback line is followed by a working, fully-redrawn
  attached session; the user's next action (typing, detaching) works exactly as it would
  after any ordinary attach. There is no state where the CLI is left showing only the
  bracketed line with no live session behind it.
- **AC2.4**: This case must be distinguishable from a first-ever attach (no resume
  attempted at all) — per `research/ux.md` §4's explicit rationale, a silent fallback would
  make "nothing to resume" and "tried and failed to resume" indistinguishable. The
  bracketed line is the mechanism that keeps these distinguishable; a first attach (no
  stored `ResumeState`, Surface 4) must NOT print this line, since no resume was attempted.

---

## Surface 3: Reconnect exhaustion (give-up) — designed here, not just validated

The plan (Story 6.1.1 AC2, Task 6.1.1a) explicitly leaves the exact wording/format as an
implementer's call, requiring only "a clear, distinguishable error message (not a silent
hang)." This is the one surface this review designs concretely.

### Existing conventions this must match

Read directly from `crates/tymux-cli/src/main.rs`:

- **Top-level error handling** (`main.rs:242-251`): every fatal error funnels through
  `main()`'s `match run().await { Err(e) => eprintln!("tymux: {}", friendly_message(&e)) }`,
  then exits with `std::process::ExitCode::FAILURE`. There is exactly one exit-message
  mechanism in this codebase — no separate "fatal" vs "warning" chrome, no ANSI coloring.
- **`friendly_message`** (`main.rs:258-268`) has two precedents worth matching:
  - Connection-refused-at-start: `"couldn't connect to tymuxd — is the daemon running? \
    (start it with \`cargo run -p tymuxd\`)"` — lowercase, em-dash aside, a question
    posed to the user, then a parenthetical concrete remedy in backticks.
  - Server-side `tonic::Status`: passed through verbatim, e.g. `"no such session: abc"` —
    short, lowercase, no trailing period.
- **Terminal state on exit**: `RawGuard` (`main.rs:227-240`) restores cooked mode in its
  `Drop` impl. Since `_raw` is a local in `attach()`, it drops during unwind whether
  `attach()` returns via an explicit `break` or via a propagated `?` — so **any** error
  path out of `attach()`, including a give-up error, already restores the terminal before
  `main()`'s `eprintln!` runs. Confirmed by reading the code, not assumed: `_raw` (`main.rs:520`)
  is declared before the `'attach_loop`, has no early `drop(_raw)` on the error path (only
  the `Exited` arm calls `drop(_raw)` explicitly, `main.rs:556`, because that arm still
  needs to print a message *through the stdout handle*, not because raw-mode restoral
  depends on it), and Rust's scope-exit-drop ordering guarantees it runs before the
  caller regains control. **This means no explicit fix is needed for terminal-mode
  restoration on the give-up path** — it's a property of the existing `RawGuard` pattern,
  not something Task 6.1.1a needs to add.

### Proposed message

Total elapsed backoff time before the give-up point, computed from ADR-004's numbers
(200ms start, ×2, capped 8s, **14 attempts** — revised up from an original 8 during the
`pm:triad-review` repair loop specifically so this schedule's total exceeds ADR-003's 60s
`grace_period_duration`; delay before attempts 2 through 14, 13 delays total): the first 7
delays sum to 200+400+800+1600+3200+6400+8000 = 20,600ms, then 6 more delays at the 8s cap
add 48,000ms more — 68,600ms ≈ **68.6s** nominal, before jitter (±20% per interval). This is
a derived arithmetic fact from ADR-004's committed constants, not a new measurement.

```
tymux: lost connection to tymuxd and couldn't reconnect after 14 attempts (~69s) — \
is the daemon still running? (check with `tymux ls`, or restart it with `cargo run -p tymuxd`)
```

Rationale for this exact text, against the two existing precedents above:

- Opens with what happened ("lost connection... couldn't reconnect"), not just a bare
  status code — matches the descriptive-clause style of both existing messages.
- `"after 14 attempts (~69s)"` gives the user a concrete sense of how long the CLI already
  tried, addressing Story 6.1.1 AC2's "not a silent hang" requirement — the user isn't
  left wondering whether it gave up after 1 try or 50.
- `"is the daemon still running?"` deliberately echoes the connect-time message's
  `"is the daemon running?"` phrasing (same question, "still" added since this happens
  mid-session, not at initial connect) — same voice, and the word "still" is the one
  textual signal that distinguishes "never could connect" from "was connected, then
  wasn't," which matters for whoever reads this later in a support/debug context.
- `"check with \`tymux ls\`, or restart it with \`cargo run -p tymuxd\`"` gives two
  concrete next actions rather than one, because this failure mode has two plausible
  causes the connect-time message doesn't need to distinguish (daemon crashed vs. daemon
  is fine but this pane/session is gone) — `tymux ls` lets the user check which before
  restarting anything.
- This is deliberately **not** a literal copy of stapler-squad's "Backend unavailable"
  banner text — that's a persistent GUI state for an unattended multi-viewer dashboard
  (`project_plans/stapler-squad-integration/design/ux.md` Surface 2/4); a CLI process exit
  is a one-shot event to a single synchronous user. Per Story 6.1.1 AC2's actual
  requirement, what must match is the *precedent* ("surfacing a distinguishable
  state after give-up," not a silent hang) — satisfied by using this codebase's own
  existing fatal-error convention, not by importing foreign UI text.

### Flow diagram

```
 t0: connected, attached, working normally
 t1: daemon becomes unreachable
 t2: reconnect attempt 1 fails, wait ~200ms (jittered)
     reconnect attempt 2 fails, wait ~400ms
     ...
     reconnect attempt 14 fails
     ┌────────────────────────────────────────────────────┐
     │ $ npm run build                                      │
     │ Compiling...                                          │
     │                          ← screen frozen here,        │
     │                            no in-band chrome during    │
     │                            the retry window itself     │
     │                            (see AC3.3 below)            │
     └────────────────────────────────────────────────────┘
 t3: RawGuard drops (terminal restored to cooked mode), process exits
     $ tymux: lost connection to tymuxd and couldn't reconnect after 14
       attempts (~69s) — is the daemon still running? (check with
       `tymux ls`, or restart it with `cargo run -p tymuxd`)
     $ ▊                            ← back at the user's normal shell prompt
```

### Acceptance criteria

- **AC3.1**: Given the daemon is unreachable for the full backoff schedule (14 failed
  attempts), when the final attempt fails, then the CLI process exits with
  `ExitCode::FAILURE`, printing the exact message above via `eprintln!` (routed through
  `friendly_message`, matching every other fatal error in this CLI) — not a silent hang,
  not a raw `Debug`-formatted anyhow chain (Story 6.1.1 AC2).
- **AC3.2**: The terminal is left in cooked (non-raw) mode when the message prints and
  when the process exits — verified by `RawGuard`'s existing `Drop`-on-scope-exit
  property (no new code required, but a regression test is worth adding: assert
  `crossterm::terminal::is_raw_mode_enabled()` is `false` after a simulated give-up path,
  since a raw-mode leak on this specific exit path would render the error message with
  literal `\r`-less line wrapping — genuinely hard to read).
- **AC3.3**: **No dead end** — after the message prints, the user is returned to a normal,
  working shell prompt (this is a property of process exit, not something the CLI needs to
  construct) and can immediately retry (`tymux attach <pane_id>` again once the daemon is
  back), which — per Surface 4 below — will pick up from `ResumeState` if Epic 6.2 shipped,
  or fall through to a fresh attach otherwise. No terminal state, lock file, or raw-mode
  flag is left behind that would make a subsequent attach behave differently.
- **AC3.4**: During the 14-attempt retry window itself (t2 above), the screen is frozen
  with no chrome. The primary justification stands on its own, independent of external
  precedent: a mid-flight indicator has nothing truthful to say here — at any point
  during the backoff, the CLI cannot distinguish "will succeed on the next attempt" from
  "will give up in a minute more," so a "reconnecting…" indicator could only ever
  assert "still trying," which the frozen screen already implies by not having changed.
  `research/ux.md`'s mosh/ET research (§1/§2) supports the *general* posture of favoring
  minimal visible interruption during reconnect — worth citing precisely, though: mosh's
  and ET's documented silent-resume behavior is established for the common brief-blip
  case, not specifically evaluated there against a worst-case ~69s give-up tail, so treat
  it as directional support for the posture, not as proof this exact duration was
  validated elsewhere. This is a deliberate consistency choice, not an oversight —
  flagging explicitly since a reviewer unfamiliar with `research/ux.md` might otherwise
  expect a "reconnecting…" indicator here; that pattern was evaluated and rejected for
  the general posture this surface inherits.
- **AC3.5**: Message text uses the existing `friendly_message`/`eprintln!("tymux: {}", …)`
  convention exactly — lowercase body, `tymux: ` prefix (added once by the call site, not
  baked into the message string itself), no trailing period, backtick-quoted commands —
  so a human scanning CLI output sees one consistent error voice across every fatal path,
  not a one-off for this feature.
- **AC3.6**: **Confirmed escape hatch during the retry window — resolved by this repair
  pass.** The existing Detach binding (`C-b d` by default, `config.rs:55`) interrupts the
  retry window: `implementation/plan.md`'s Task 6.1.1a now races `stdin_rx.recv()` against
  each reconnect cycle (the backoff sleep plus the redial attempt itself) inside the retry
  loop's own `select!`, reusing the exact `stdin_rx`/`reassembler`/`RawGuard` instances the
  live session already owns — the retry loop runs inline inside `attach()`, never
  returning to `attach_and_follow`, specifically so that reuse (and `RawGuard`'s
  scope-exit `Drop` guarantee) both hold. Pressing Detach during backoff fires the
  identical exit path as a live-session Detach (`main.rs:652-657`): `AttachOutcome::Done`,
  the same `"\r\n[tymux: detached]"` message, and the terminal restored to cooked mode by
  the same `RawGuard::Drop` property AC3.2 already relies on for the give-up path —
  verified by the same `crossterm::terminal::is_raw_mode_enabled() == false` assertion
  (plan.md Task 6.1.1g). **The commonly assumed Ctrl-C fallback still does not hold**, and
  that's worth stating precisely rather than leaving it implicit: `RawGuard::enable()`
  (`main.rs:520`) puts the terminal into crossterm raw mode, and crossterm's own
  documentation for `enable_raw_mode` states that raw mode means "special keys like
  backspace and CTRL+C will not be processed by [the] terminal driver" — so Ctrl-C during
  an active `tymux attach` session, including during backoff, is still delivered as a
  literal `0x03` byte, never translated into `SIGINT`. That limitation is no longer
  load-bearing, though: Detach, not Ctrl-C, is the guaranteed in-band escape hatch here,
  exactly mirroring how a live attach session already expects Detach (not Ctrl-C) as its
  own exit gesture. External `SIGKILL` remains genuinely uninterruptible by any
  process-internal mechanism (true of any process, not specific to this feature), and
  `SIGTERM`/`SIGHUP` from another terminal still bypass `RawGuard::Drop` exactly as
  before — those residual gaps are real, but no longer the *only* way out, which is what
  made this a blocker in the first place. **`pending_input`'s fate on Detach, made
  explicit**: any bytes queued in `pending_input` (see "Input during a drop or retry
  window" above, under Surface 1) at the moment Detach fires are discarded, not delivered anywhere — this
  is correct, not a contradiction of that buffer's stated purpose. `pending_input` exists
  to bridge a *temporary* drop long enough for the connection to come back on its own; it
  was never meant to survive a *deliberate* exit. Once the user chooses Detach there is no
  live pane left for those keystrokes to reach, so silent discard here is the same
  behavior a live-session Detach already has for anything in flight, not a new gap
  (plan.md Task 6.1.1a/6.1.1g).
- **AC3.7**: **`pending_input`'s fate on give-up/exhaustion — also resolved by this repair
  pass.** Symmetric with AC3.6's Detach resolution above: when the backoff schedule is
  exhausted (all 14 attempts failed, t3 in the flow diagram above) and the CLI process
  exits, any bytes queued in `pending_input` at that point are discarded — not flushed to
  a stream (there is none left to flush to), not written to `resume_state.json` or
  anywhere else. This is the same reasoning as AC3.6, applied to the terminal exit instead
  of the deliberate one: `pending_input` bridges a *temporary* drop long enough to
  reconnect; give-up is the CLI concluding the drop is *not* temporary, at which point
  there is no live pane left and the process is about to end. Given the daemon stays
  unreachable through the full backoff schedule and the user typed during that window,
  when the final attempt fails, then those queued keystrokes are silently dropped as part
  of process exit, with no attempt to persist or resend them (plan.md Task
  6.1.1a/6.1.1g).

---

## Surface 4: Manual reattach after being gone a while (Phase 6 cut-candidate)

### Validation against research

This isn't a new interaction pattern — it's Surfaces 1/2 reached via a different
trigger (a brand-new process invocation instead of an in-process reconnect). The decision
tree the plan requires (Story 6.2.1 AC3) collapses onto the same two chrome outcomes
already designed above, gated by whether `ResumeState` has a usable entry.

### Flow diagram (decision tree)

```
 user runs: tymux attach <pane_id>
        │
        ▼
 does resume_state.json have a stored seq for <pane_id>?
        │
   ┌────┴─────┐
   NO          YES
   │            │
   ▼            ▼
 fresh attach   send resume_from_seq = stored_seq
 (today's       │
  behavior,      ▼
  unchanged)    is stored_seq still within the daemon's
   │             replay-buffer retention window?
   │                  │
   │            ┌─────┴──────┐
   │            YES           NO
   │             │             │
   │             ▼             ▼
   │        Surface 1      Surface 2
   │        (silent        (GapExceeded
   │        resume)        fallback line
   │                       + full redraw)
   │             │             │
   └─────────────┴─────────────┘
                 │
                 ▼
     CLI writes updated resume_state.json
     on exit (any AttachOutcome::Done after
     real output was received)
```

### Acceptance criteria

- **AC4.1**: Given no stored `ResumeState` entry for `<pane_id>` (first-ever attach),
  when the user runs `tymux attach <pane_id>`, then behavior is unchanged from today —
  a plain `CapturePane` full-screen redraw, no `resume_from_seq` sent, no `GapExceeded`
  chrome (nothing was attempted to resume, so nothing can have exceeded a gap) — this is
  the concrete mechanism behind AC2.4's "must be distinguishable from a first attach"
  requirement above.
- **AC4.2**: Given a stored, in-window seq, when the user reattaches, then the experience
  matches Surface 1 exactly (silent resume, buffered-then-live output through the same
  render path) — no separate "welcome back" chrome distinguishing a cross-process resume
  from an in-process one.
- **AC4.3**: Given a stored, out-of-window seq (gap exceeded retention while the terminal
  was closed), when the user reattaches, then the experience matches Surface 2 exactly
  (one bracketed fallback line, then full redraw).
- **AC4.4**: No dead end on a corrupt or unreadable `resume_state.json` — per Task 6.2.1b,
  `load()` returns empty on missing/corrupt file, never a hard error; a corrupt state file
  degrades to AC4.1's fresh-attach behavior, not a crash. Worth an explicit test: a
  hand-corrupted `resume_state.json` still allows `tymux attach` to succeed.
- **AC4.5**: State-file writes are atomic (temp-file-then-rename, Task 6.2.1b) — a `tymux
  attach` process killed mid-write (e.g. `kill -9` right at exit) must not leave a
  half-written `resume_state.json` that then fails AC4.4's corrupt-file fallback in a way
  that silently loses a *previously good* stored seq. Testable: kill the process during
  the save and confirm the prior file (or a clean absence) survives, never a truncated one.

---

## Surface 5: Daemon structured logs + `tymux_attach_resume_outcome_total` (non-interactive)

Operator-facing, not end-user-facing — condensed treatment per the Step 1 instruction.

### Representative sample

```
# Successful resume:
INFO attach: gauge incremented pane_id=abc123 tymux_attached_sessions_gauge=1 resume_requested=true
INFO tymux_attach_resume_outcome_total{outcome="resumed_from_buffer"}=42

# Gap exceeded:
WARN resume request outside replay buffer retention pane_id=abc123 resume_from_seq=100 oldest_available_seq=150
INFO tymux_attach_resume_outcome_total{outcome="gap_exceeded_fallback"}=3

# Grace period fires (no reconnect happened in time):
INFO grace period expired, deferred viewport cleanup fired pane_id=abc123 window_id=w1 client_id=c9 elapsed_ms=60000
```

### Acceptance criteria

- **AC5.1**: Every field named in ADR-004's Observability Plan (`pane_id`,
  `resume_from_seq`, `oldest_available_seq` on the `GapExceeded` warn line; `pane_id`,
  `window_id`, `client_id`, `elapsed_ms` on the grace-period info line;
  `resume_requested: bool` on the existing gauge-increment line) is present and
  correctly named — not a superset, not renamed — matching Task 4.1.1c's explicit
  "confirm field names match the Observability Plan exactly" instruction.
- **AC5.2**: `tymux_attach_resume_outcome_total` increments exactly once per attach
  outcome, tagged with exactly one of `resumed_from_buffer` / `gap_exceeded_fallback` /
  `no_resume_token_full_attach` — never zero or multiple tags for one attach.
- **AC5.3**: Logging style matches the existing `AttachedGaugeGuard`/gauge-line convention
  (`crates/tymuxd/src/main.rs:86-91`, `main.rs:642`) — lowercase message, structured
  fields via `tracing`'s `key = value` syntax, no ad hoc string formatting — so these new
  lines are greppable/parseable the same way existing daemon logs already are.
- **AC5.4**: No PII or terminal content in any of these lines — only IDs, counts, and
  durations, consistent with requirements.md's "internal/local, no on-call rotation"
  security classification (no justification needed for a heavier metrics pipeline).

---

## Cross-surface summary

### No-dead-end audit

| Surface | Exit path when things go wrong | Verified against |
|---|---|---|
| 1. Happy resume | N/A (not an error state) | — |
| 2. Gap exceeded | Full redraw restores a working session | AC2.3 |
| 3. Give-up | Process exits to a normal, working shell prompt | AC3.3 |
| 4. Manual reattach, corrupt state | Falls back to fresh attach, never a crash | AC4.4 |
| 5. Logs/metrics | N/A (no user-facing exit path; operator reads logs) | — |

Every interactive surface either has no error state (1) or resolves to a usable session
or a clear, actionable exit (2, 3, 4) — no surface leaves the user staring at a frozen
screen or a crash with no next action.

### Accessibility

- Keyboard: unaffected by this feature — attach was already 100% keyboard-driven (a raw
  pty stream), and nothing here adds a new input surface (no dialog, no button, no focus
  trap to manage). The existing Detach binding now interrupts Surface 3's retry window
  (AC3.6, resolved by this repair pass) — user control and freedom during that wait is
  guaranteed, not just hoped for.
- Screen reader: no new code path (Surfaces 1/2/4 all reuse the existing
  `stdout.write_all` render path); the one flagged risk (large replay bursts) is inherited
  terminal behavior, not new, and explicitly out of this project's appetite to solve
  (`research/ux.md` §3, AC1.3 above).
- Color contrast / visual design: N/A — no new colored UI; the `[tymux: ...]` chrome lines
  render in the terminal's own foreground color, same as `OutputGap`/`Exited` today.
- The one pre-existing, documented accessibility gap (no screen-reader navigation for
  multi-pane windows, README.md:104-107/115-116) is unchanged by this feature and correctly
  out of scope here.

### What this review explicitly did NOT add

Per the task framing, no new interaction pattern is proposed beyond Surface 3's message
text. Specifically not recommended, matching `research/ux.md`'s explicit rejections:

- No spinner, progress bar, or percentage-complete indicator for the reconnect/replay
  window.
- No "reconnecting…" banner during the Surface 3 retry window (AC3.4) — that pattern
  belongs to stapler-squad's standing-dashboard use case (Surface 2 of that project's own
  UX doc), not this synchronous single-user CLI.
- No "you're all caught up" chat-style divider marking where buffered output ends and live
  output begins — rejected in `research/ux.md` §1 as borrowing UI from a discrete-message
  paradigm that doesn't fit a continuous positional stream.
