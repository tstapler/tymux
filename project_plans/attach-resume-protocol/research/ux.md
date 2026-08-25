# Research: UX — attach-resume-protocol

**Date**: 2026-08-24
**Scope note**: This is a protocol/daemon-first feature. Its only current
human-facing surface is `tymux-cli`'s attach loop
(`crates/tymux-cli/src/main.rs`). stapler-squad's `BackendTymux`/xterm.js
adoption is explicitly a future follow-up (requirements.md, Out of Scope),
so this doc treats that surface as *context*, not something being designed
here — `project_plans/stapler-squad-integration/design/ux.md`'s Surface 2
already specs a visible "reconnecting…" indicator for that product, and
that decision is not revisited below.
**What's NOT re-covered here**: `project_plans/roadmap/README.md:55-71`
already researched mosh's State Synchronization Protocol and Eternal
Terminal's `BackedReader`/`BackedWriter` as the *transport-resume*
precedent for this project's per-subscriber cursor design, and zellij's
`Active → ActiveDetached → Killed` state machine for the grace-period
concept. This doc doesn't re-derive those; it looks specifically at the
angle the roadmap research didn't cover — the **user-visible experience of
an incremental catch-up replay**, not whether reconnection itself
succeeds.

## Baseline: what a `tymux-cli` user experiences today

Confirmed by reading `crates/tymux-cli/src/main.rs`:

- **The CLI has no client-side reconnect loop.** `attach_and_follow`'s loop
  (`main.rs:438-441`) only handles `AttachOutcome::SwitchTo` (window
  switching); a dropped `Attach` stream hits `None => break
  AttachOutcome::Done` (`main.rs:544`) and the process simply exits. A
  human notices the drop (terminal stops updating) and manually re-runs
  `tymux attach`. This is unlike stapler-squad's `ReconnectLoop`, which
  retries automatically inside a long-lived Go process.
- **Every reattach today — deliberate or after a drop — gets an identical,
  silent full-screen `CapturePane` redraw.** There is no signal
  distinguishing "first attach ever" from "reattach after a drop." The
  redraw itself is unremarkable: it's exactly what happens on every
  `tymux attach` invocation already.
- **The one existing "something went wrong" signal is a one-line inline
  chrome marker, not a banner or spinner.** `chrome_message_for_event`
  (`main.rs:812-818`) maps `OutputGap` to `"\r\n[tymux: output dropped]\r\n"`,
  written directly into the pty output stream alongside real output
  (`main.rs:561-564`). This is the CLI's only precedent for "tell the user
  we couldn't fully recover something," and it's minimal by design — no
  modal, no persistent status-bar state, just one bracketed line inline
  with the terminal content it's describing.
- **Live output and captured/replayed output already share one render
  path.** Live bytes go through `stdout.write_all(&bytes)` (`main.rs:552`);
  `CapturePane`-fetched snapshots go through `render_plain_grid`
  (`main.rs:625`, `798`). A resume-replay is a third source of bytes that
  would need to pick one of these paths (see Accessibility below).

## 1. Comparable UX patterns for incremental catch-up replay

The roadmap research covered *whether* mosh/ET/zellij reconnect
succeeds. Here's the gap it left: what does the user *see* while the
missed content is being filled in?

- **tmux**: no incremental-replay concept exists at all. Detach/reattach
  either shows the current live screen or nothing — tmux has no protocol
  awareness of "what happened while detached" beyond whatever the pane's
  own scrollback buffer happened to retain locally. There's no "catching
  up" UX to borrow because tmux was never designed to lose the connection
  in the first place (it assumes the client, not the session, disconnects
  and the terminal keeps drawing to the same in-process buffer).
- **mosh**: deliberately gives **no catch-up indicator**. State Sync
  Protocol diffs are applied to the local predicted screen state silently;
  the design goal (per Winstein's USENIX paper, already cited in the
  roadmap doc) is that reconnection *feels* like the network was never
  gone — the screen just becomes correct again. Mosh's authors treat any
  visible "syncing…" UI as a failure of the abstraction, not a feature.
- **Eternal Terminal**: same silent-resume posture — `BackedReader`/
  `BackedWriter` replay the missed byte range and the terminal simply
  catches up; ET doesn't show a distinct "replaying" state either.
- **zellij**: session resurrection reattach does a **full repaint**
  (closer to tymux's current `CapturePane` fallback than to an incremental
  replay) — zellij has no incremental catch-up UX to borrow from because
  it doesn't have an incremental catch-up *mechanism* at this layer.
- **Chat apps with offline sync (Slack/Discord)**: these DO show explicit
  "you're all caught up" banners and unread-message dividers — but the
  reason is structural, not aesthetic: chat messages are discrete,
  addressed units a user reviews at their own pace after the fact. A
  terminal stream is the opposite — one continuous, positionally-ordered
  stream where the "correct" experience is that the screen simply *is*
  up to date, not that the user reviews a list of what they missed. This
  is the key transferable insight: **the chat-app pattern doesn't map to
  a terminal**, and reaching for it here would be borrowing UI from a
  fundamentally different information shape.
- **SSH session managers (tmate, ttyd)**: no distinct catch-up UX beyond
  tmux's own reattach behavior — they're a transport wrapper around tmux,
  not an independent design point here.

**Conclusion**: every terminal-multiplexer-family precedent (mosh, ET,
tmux, zellij) converges on the same answer — replay/resume is invisible
by design. The chat-app "catching up" pattern is the one candidate model
that doesn't apply, because it solves a different problem (reviewing
discrete missed items) than this feature has (making a continuous stream
positionally correct again).

## 2. Mental model: should replay be visible or invisible?

**Invisible/instant**, for the CLI specifically — refining rather than
overturning the requirements doc's framing:

- The replay buffer is in-memory and local to the daemon; filling in a
  missed range is not a slow network fetch, it's a buffer read, so there's
  no meaningful "loading" duration to communicate.
- The CLI's own existing behavior already treats every reattach as
  "redraw and move on" with no state announcement (see Baseline). Adding
  a visible "replaying missed output…" indicator would be a *new*
  interruption for something that today produces zero signal — it would
  make the resumed case feel more ceremonious than the case it's
  replacing, which is backwards for a feature whose whole point is to
  make reconnection cheaper and less disruptive.
  Concretely: a human should see the missed lines appear (via the same
  `stdout.write_all` path as live output) and land back in "normal
  attached" state, indistinguishable from having stayed connected the
  whole time — via the same render path live output already uses,
  extended with an initial burst of the replayed bytes rather than a
  new/different code path with its own visible state.
- This is explicitly narrower than stapler-squad's Surface 2, which shows
  a persistent reconnecting indicator — and that's *correct*, not
  inconsistent, because the situations differ: `BackendTymux` holds one
  standing stream open across an unattended session that multiple humans
  view asynchronously through a persistent dashboard, so a drop needs a
  durable, glanceable signal. A CLI user is synchronously watching their
  own terminal and initiates reattachment themselves the moment they
  notice the drop (their own terminal freezing *is* the signal) — there's
  no separate audience that needs to be told asynchronously that a
  reconnect is in progress.

## 3. Accessibility

No new consideration beyond the CLI's already-documented gap (README.md:104-107,
115-116: no screen-reader navigation for multi-pane windows, out of scope
for v1.0). Confirmed by reading the render path: replayed bytes would
reasonably reuse the same `stdout.write_all` path live output already
uses (`main.rs:552`), so a screen reader attached to the terminal emulator
processes them exactly as it would live output — resume introduces no new
screen-reader-specific code path.

One genuinely new wrinkle, flagged but not a blocker: a replay can deliver
a **burst** — everything missed during the gap — in one write, rather than
the gradual trickle a live session produces. For a screen reader reading
new terminal content as it streams, a large single dump (e.g., a minute of
build-log output that accumulated while disconnected) reads worse than the
same content arriving gradually. This isn't a new failure mode — the same
thing already happens today with `cat largefile` or any fast-running
command — but this feature makes the burst case more *likely* to be
exercised, since a dedicated replay buffer sized for "a real reconnect
window" (requirements.md, Rabbit Holes) is specifically designed to make
longer gaps recoverable, where today they'd just fall through to a
single-screen `CapturePane` snapshot (no burst, because only the current
screen is shown, not the history). Pacing/chunking replayed output is not
required for this project's appetite — noting it as a candidate follow-up
if replay-buffer retention ends up sized in the minutes range rather than
seconds.

## 4. Error states: resume fails, falls back to `CapturePane`

Recommend a **visible, minimal signal on fallback only** — reusing the
CLI's existing `chrome_message_for_event` pattern
(`main.rs:808-818`) rather than inventing a new UI shape:

- **Resume succeeds**: no marker. Per the invisible-replay mental model
  above, a successful resume should read identically to output that
  happened to still be live — adding a "resumed!" marker for the happy
  path would be new noise for a case designed to be unremarkable, and
  breaks symmetry with `OutputGap`'s existing precedent of only signaling
  the *bad* case.
- **Resume falls back to full `CapturePane`** (gap exceeded retention):
  one bracketed inline line, in the same voice as the existing
  `"[tymux: output dropped]"` marker — e.g.
  `"[tymux: reconnect gap too large, resyncing]"` — immediately before the
  fresh snapshot redraw. This should extend `chrome_message_for_event`'s
  existing match arm set with the new fallback-signal variant (Scope
  already requires the wire-level "explicit fallback signal"; this is
  its one concrete client-visible surface) rather than being silently
  identical to a first attach.
- Rationale for *not* going fully silent on fallback: a silent fallback
  makes "the daemon successfully resumed nothing because there was
  nothing to resume (first attach)" indistinguishable from "the daemon
  tried to resume and failed because the gap was too large" — exactly the
  kind of collapsed-distinct-causes failure this codebase's own UX
  precedent (`stapler-squad-integration`'s Surface 3/4 design, and this
  repo's own `OutputGap` handling) consistently avoids. A one-line signal
  costs nothing and keeps the two cases distinguishable without demanding
  attention.

## 5. Job to be done — confirm/refine

The requirements/task framing — "I got disconnected, I don't want to lose
what I missed and I don't want a jarring full-screen redraw" — is
**directionally right but slightly mis-weighted** once the Baseline above
is accounted for:

- Today's reattach is not actually *jarring* — it's a normal, silent
  `CapturePane` redraw, same as any fresh attach. So "avoid a jarring
  redraw" isn't the real gap; the real gap is **information loss**: output
  that scrolled past while disconnected and never appears anywhere, not a
  UI-smoothness problem.
- Refined JTBD: *"When I reattach, give me back exactly what I would have
  seen if I'd stayed connected — not just wherever things ended up."*
  Continuity of the stream is the job, not smoothness of the redraw
  per se (the redraw was already smooth; it just skipped content).
- Secondary job, for the case that exceeds retention: *"If too much
  happened for you to give it all back, say so plainly rather than
  quietly showing me a fresh screen that looks complete but isn't."* This
  is exactly the fallback-signal case in §4.

**Scope gap worth flagging** (not a UX decision to make now, but a
finding this doc surfaces): the CLI has no reconnect loop and each
`tymux attach` invocation is a fresh process, so for a human to actually
benefit from a resume token on a *manual* reattach, something needs to
persist `pane_id → last-seen seq` across process exits — there's no such
state today. `crates/tymux-cli/src/config.rs:207-208` and
`crates/tymux-core/src/persistence.rs` already establish the
`$XDG_STATE_HOME`-style local-state precedent this could reuse, but
requirements.md's Scope only commits to updating `clients/ts`/`clients/go`
reference clients, not `tymux-cli` itself. Whether `tymux-cli` gets wired
to actually send a resume token (vs. only being able to benefit from
resume for a within-process detach/reattach, e.g. copy-mode's own
redraw path, which never left the process) is an open implementation
question this UX research surfaces but doesn't resolve.

## Summary of recommendations

1. Replay should be invisible/instant on the happy path — extend the
   live-output render path, no spinner/banner, no chat-app-style
   "catching up" UI (that pattern solves a different problem).
2. Only the degraded/fallback path (gap exceeded retention) gets a
   visible signal, following the CLI's existing one-line bracketed
   `chrome_message_for_event` convention — not a new UI element.
3. No new accessibility affordance is required; the one flagged risk
   (large replay bursts reading worse to a screen reader than gradual
   output) is a pre-existing terminal/screen-reader interaction, not a
   new one, and doesn't block this project's appetite.
4. For `tymux-cli` to actually exercise resume tokens across separate
   process invocations (the common manual-reattach case), it needs
   cross-invocation local state it doesn't have today — flagged as an
   open scope question, not decided here.
