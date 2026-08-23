# Research: UX

Scope: how tymux's disconnect-survival and rendering behavior shapes what a
stapler-squad user actually sees — comparable reattach UX in other tools, the
mental model users bring to "I closed the tab, did my agent keep working?",
concrete rendering-fidelity gaps in tymux's two output paths, error states
this integration needs to make debuggable, and the job-to-be-done that
"reliable disconnect survival" fulfills.

## 1. Comparable UX patterns: reattaching to a long-running remote session

The products in this space split into two families by what they show on
reattach: **replay** (dump the missed output/scrollback so the user catches
up) and **silent resume** (just keep going from the live screen, no replay).

- **tmux/screen (`attach`/`reattach-to-user-namespace`)**: silent resume.
  Reattaching redraws the *current* screen state (one full-screen repaint of
  the live buffer) — it does not replay everything that happened while
  detached line-by-line. Scrollback is available on demand (`Ctrl-b [`) but
  is not pushed automatically. This is the model tymux's own `CapturePane`
  RPC most directly mirrors: a structured snapshot of the *current* screen,
  not a byte-for-byte replay.
- **mosh**: silent resume, but goes further — it actively hides network
  disruption. Mosh's SSP (State Synchronization Protocol) diffs *predicted*
  local terminal state against the server's authoritative state and
  reconciles on reconnect, so a user roaming between WiFi/cellular sees no
  "disconnected" indicator at all in the common case; only a sustained
  outage triggers mosh's "Connecting..." overlay. The design goal is that
  disconnection is *invisible* unless it's prolonged.
- **GitHub Codespaces / VS Code Remote terminal persistence**: hybrid.
  The remote pty (via the VS Code Server / Codespaces port-forwarding proxy)
  keeps running after the local client disconnects, and reconnecting silently
  re-attaches to the live buffer — but the UI explicitly surfaces the gap: a
  toast/status-bar state ("Reconnecting...", then a brief "Terminal
  reconnected") tells the user a disruption happened, even though no content
  was lost. This is the "make the invisible visible, briefly" pattern.
- **Zellij session resurrection**: unlike the others, Zellij persists session
  *layout* (panes/tabs) to disk and can resurrect a session after the
  *daemon itself* died, not just after a client disconnected — closer to
  tymux's own `ReviveSession` RPC than to attach/reattach. Its UX explicitly
  labels resurrected sessions as such in the session list ("Exited") so the
  user knows they're looking at a dead-but-recoverable session, not a live
  one — directly relevant to how stapler-squad should render tymux's
  `Liveness::DEAD` state in a session list (see §4).
- **Warp cloud sessions / Blocks**: closest to a "the terminal is a document,
  not a stream" model — output persists as retrievable blocks independent of
  connection state, so reattach is really just "reload the document." Not
  directly comparable to tymux's pty-stream model, but useful as the extreme
  end of "the client should never have to wonder what it missed."

**What users expect on reattach, synthesized**: nobody expects a scrollback
replay animated back at them. The near-universal expectation is: (1) the
*current* screen state, correctly redrawn, immediately on reattach, and
(2) some out-of-band signal (status indicator, toast, or both) that a
disconnect happened, distinct from the terminal content itself. Silence on
the second point (tmux/mosh's default) is acceptable for expert users who
already trust the tool; a dashboard UI aimed at people multitasking across
several agent sessions should lean toward VS Code's explicit
"reconnected"/"was this running while you were away" signal instead, given
stapler-squad's own precedent (§4).

**tymux-specific gap this surfaces**: per `crates/tymuxd/src/main.rs`'s
`attach()` handler (read directly), a new `Attach` call subscribes to the
pane's **live** output broadcast channel (`pane.subscribe()` →
`tokio::sync::broadcast::Sender<Vec<u8>>`) and starts forwarding from that
point — it does not send a priming `AttachEvent::Snapshot` first. Contrast
with `WatchWindow`, which explicitly does emit the current layout
immediately on subscribe ("Emit the current snapshot immediately, so a
subscriber doesn't [block waiting for the next change]" — main.rs:405-407
comment). `Attach` has no equivalent comment or behavior. Practically: if
stapler-squad's `BackendTymux` opens a fresh xterm.js instance and pipes
raw `Attach` output bytes into it on reattach, the terminal will render
**blank** until the next byte of new output arrives — it will not show
whatever was on screen when the user left, even though the pane and its
process are still alive. This is a real fidelity gap against every
comparable product above, all of which redraw current state immediately.
The fix is mechanical on either side (tymux could emit a `Snapshot` as
the first `AttachEvent`, replacing `contents_formatted()`-style
priming bytes; or stapler-squad's client could call `CapturePane` once on
attach and feed its structured grid into xterm.js before wiring up the
live stream) but it is not automatic today and should be an explicit
decision in Phase 3 planning, not an assumption.

## 2. User mental models: what happens to my agent when I close the tab?

There is no single strong prior here — users of AI-coding-agent dashboards
specifically are working from an *unstable analogy*, not a clear model,
because the space blends two very different precedents:

- **Chat-app precedent** (ChatGPT, Claude.ai web): closing the tab does
  *not* stop generation server-side for a single response, but there is no
  concept of a long-running, multi-step *session* continuing to act
  autonomously — so this precedent under-predicts what an agent session
  actually does (keeps running, keeps making tool calls, keeps consuming
  resources, potentially waiting on an approval).
- **Cloud-IDE/CI precedent** (Codespaces, GitHub Actions, CI runners):
  closing the tab clearly does *not* stop a running job — the job is
  understood to be a server-side resource independent of the viewer. This
  precedent over-predicts confidence for users unfamiliar with it, and is
  the closer analogy to what stapler-squad + tymux actually deliver.
- **Local terminal precedent** (a plain terminal, not inside tmux): closing
  the window kills the process. Users who haven't internalized "this is
  tmux under the hood" may default to *this* model and wrongly assume their
  agent stopped — the opposite failure mode from the above.

Because stapler-squad specifically runs agents that take autonomous,
sometimes irreversible actions (the requirements doc's own "review-queue/
approval pipeline over agent actions" is direct evidence a wrong mental
model here has real stakes — a user who thinks the agent paused, but it
kept running and hit an approval gate, may come back to a stalled agent
they didn't know was waiting on them, or worse, one whose queued action
executed without them present to review it if approvals aren't strictly
gating), this ambiguity is not a nice-to-have to resolve. **Flagging for
stapler-squad's UI work (not this project's deliverable, but downstream of
it)**: the dashboard should state the mental model explicitly rather than
rely on any of the above analogies — e.g., a persistent, low-key label per
session ("Running in background" / "Waiting for your approval" / "Finished
while you were away") rather than leaving the user to infer continuity from
tmux/tymux's actual (correct, but invisible) behavior. This is exactly the
gap `InputDropBadge.tsx` and `ConnectionIndicator.tsx` already exist to fill
for the *connection* half of the story (see §4) — the *agent-execution*
half (did it keep going, did it hit an approval, is it done) needs the
equivalent treatment stapler-squad-side.

## 3. Rendering fidelity gaps a user would notice

tymux exposes two distinct output paths, with different fidelity
characteristics — this matters because which one stapler-squad wires into
xterm.js determines whether these gaps are real or moot.

**`Attach`'s raw byte stream — high fidelity, verified.** Per
`crates/tymux-core/src/pane.rs`, the pty reader thread broadcasts the *raw*
bytes read from the pty (`output_tx: broadcast::Sender<Vec<u8>>`) — these
bytes are never routed through tymux's `vt100::Parser` before reaching an
`Attach` subscriber; the parser is a *separate* consumer of the same raw
stream, used only to serve `CapturePane`/`SearchScrollback`. This means
`Attach`'s output is unprocessed terminal escape-sequence data, identical in
kind to what a real terminal emulator would receive directly from a pty —
truecolor/256-color SGR codes, alternate-screen-buffer sequences
(`\x1b[?1049h/l`), cursor visibility (`\x1b[?25h/l`), bracketed paste
(`\x1b[?2004h/l`), mouse reporting — all pass through untouched, because
xterm.js (or any terminal emulator) parses these directly and tymux never
strips or reinterprets them on this path. **This answers the requirements
doc's open question directly: `Attach`'s raw bytes should feed xterm.js
without a translation layer**, for the same reason tmux's own control-mode
`%output` pane data (which stapler-squad currently pipes into xterm.js
today, per the requirements baseline) works without one — same
raw-bytes-in, xterm.js-parses-it model.

**`CapturePane`'s structured `Cell` grid — real, verified gaps.** This RPC
is explicitly documented in the proto as "the AI-friendly alternative to
`tmux capture-pane`" for programmatic/agent callers, not a rendering path —
but if stapler-squad ever uses it for anything user-facing (e.g., a
priming snapshot on reattach, per §1's finding, or a lightweight preview
thumbnail without a live PTY stream), these gaps become visible defects:

- **Missing `dim` attribute.** `proto/tymux/v1/tymux.proto`'s `Cell.attrs`
  bitflags cover only `bold(1)/underline(2)/reverse(4)/italic(8)`
  (`tymux.proto:243-247`). `crates/tymux-core/src/pane.rs`'s `pack_attrs()`
  (pane.rs:442-457) mirrors exactly those four. The underlying
  `vt100::Cell` (vt100 0.16.2, `vt100-0.16.2/src/cell.rs`) additionally
  exposes `.dim()` — read directly from the vendored crate source — which is
  never packed. Any agent output using dim/faint text (common in CLI tools'
  secondary/muted text) would silently lose that styling through
  `CapturePane`. (vt100's `Cell` has no blink/strikethrough/hidden methods
  at all, so those aren't gaps tymux introduced — they don't exist upstream
  either.)
- **No alternate-screen-buffer signal.** `vt100::Screen::alternate_screen()`
  exists upstream (`vt100-0.16.2/src/screen.rs:548`, confirmed by reading the
  source) and is exactly what a full-screen TUI tool (vim, htop, an agent's
  own progress UI, `less`) flips on — but `PaneSnapshot` has no field
  carrying it. A client relying on `CapturePane` alone cannot tell whether
  it's looking at the primary or alternate screen, nor whether an app that
  uses the alt-screen just exited (which should trigger a full redraw back
  to the primary screen's prior content).
- **No cursor-visibility, bracketed-paste, or mouse-mode signal.**
  `vt100::Screen` tracks `hide_cursor()`, `bracketed_paste()`,
  `mouse_protocol_mode()`/`_encoding()` (all confirmed present in
  `screen.rs`), none of which surface on `PaneSnapshot`. `cursor_row`/
  `cursor_col` are present, but not whether the cursor should currently be
  rendered at all.

None of these `CapturePane` gaps affect `Attach`, since `Attach` never goes
through the `vt100::Parser` at all. The practical implication for planning:
**`CapturePane` is not a safe substitute for a real rendering path** — it's
fine for the AI-facing/debugging use it was designed for, and fine as a
coarse reattach-priming snapshot for un-styled or lightly-styled content,
but treating it as a full rendering source (e.g., building a
snapshot-and-diff UI instead of streaming `Attach`) would silently
reintroduce a version of the "fought xterm.js" problem the requirements doc
says was already tried and abandoned once, for a different reason (that one
was about diff/apply logic and scrollback breakage; this one would be about
lost styling and alt-screen state) — same failure mode, worth calling out
so nobody re-derives it the hard way. `Attach`'s raw stream is the
rendering path; `CapturePane` is the query/debug path.

## 4. Error states this integration needs to make debuggable/visible

stapler-squad already has UI precedent for transient connection-state
communication, which is direct evidence for how these new tymux-specific
error states should be handled rather than invented from scratch:

- **`web-app/src/components/layout/ConnectionIndicator.tsx`** — an existing
  persistent connection-status affordance.
- **`web-app/src/components/sessions/InputDropBadge.tsx`** — an existing,
  accessibility-conscious (portal-rendered + `LiveRegion` for screen
  readers, per its doc comment referencing `design/ux.md §2.4`), auto-
  dismissing badge specifically for "N keystrokes dropped — reconnecting"
  during today's tmux-backend WebSocket reconnects. This is the closest
  existing analog to what a `BackendTymux` disconnect/reconnect needs to
  surface, and should be the pattern extended rather than a new one
  invented.

New error states `BackendTymux` introduces, each needing a distinguishable
UI state (this project's job is making them *debuggable* at the tymux/
gRPC layer — surfacing them is stapler-squad's UI work, listed here for
completeness against the requirements' "Error states" ask):

- **`tymuxd` not running / unreachable.** A gRPC connection failure at
  session-start time is categorically different from a mid-session drop
  (§ above) — the user never got a session at all. This should not present
  as the same "reconnecting" transient state; it needs its own "backend
  unavailable" message, ideally with enough detail (surfaced from the
  connect-error) to distinguish "daemon not started" from "daemon crashed"
  from "network/socket path misconfigured" — otherwise this collapses into
  an unhelpful generic failure a user can't act on or report.
  `BackendTymux` should not swallow the underlying connect error string.
- **`BackendTymux` session fails to start.** `CreateSession` returning an
  error (bad command, resource limits, whatever tymuxd's failure modes turn
  out to be) needs to surface as a session-creation failure distinct from
  "session started but the agent process inside it immediately exited" —
  conflating these would make debugging a bad agent command indistinguishable
  from a broken backend integration.
- **Disconnect-survival guarantee actually failing (pane died while
  detached).** This is the one most directly downstream of the must-fix bug
  in `crates/tymux-e2e/tests/disconnect_survival_e2e.rs` (see below): if a
  pane dies while nobody was attached, the user's *first* signal today would
  be `Liveness::DEAD` on the next `ListSessions`/`CapturePane`/reattach
  call — there is no proactive notification, because nothing was attached to
  observe the death happen. The UI needs to treat `Liveness::DEAD` on
  reattach as a distinct, explained state ("this agent's session ended while
  you were away" — Zellij's "Exited" label in its session list, §1, is the
  closest existing UX precedent) rather than presenting a dead pane the same
  as a live one that simply has no new output yet. Silently showing a static
  last-known screen with no liveness cue would be actively misleading — the
  user could believe the agent is still working.

**This directly implicates the abrupt-disconnect bug.** Read directly from
`crates/tymux-e2e/tests/disconnect_survival_e2e.rs`
(`pane_survives_abrupt_disconnect`, `#[ignore]`d, lines 63-139): an abrupt
client disconnect (tab closed, network drop, laptop losing power — exactly
stapler-squad's stated baseline scenario) currently kills the pane's child
process, not just the attach stream, 100% reproducible when the client's
pty master closes while the CLI process is still alive. The commit history
(`ab88c81`) shows a follow-up investigation ruled out every code-level
cause in `tymux-core`/`tymuxd` (no `kill()` call site fires, not fd/device
aliasing, not timing-dependent) without finding the actual mechanism, and
explicitly recommends re-testing outside the current sandboxed dev
container. **Until this is fixed, "the pane died while detached" is not a
rare edge case for stapler-squad's users — it is closer to the default
outcome of the literal scenario this whole project's UX promise depends
on** ("close your laptop, agent keeps working"). The Liveness/DEAD-state UX
above (and any error-state work in general) is scaffolding around a bug
that, unfixed, makes the core promise false most of the time it's tested.

## 5. Job-to-be-done: what "reliable disconnect survival" is really for

**Functional job**: let a user start a long-running agent task, close the
laptop or lose network, and resume exactly where the agent left off later —
without having to keep a tab open, a laptop awake, or a network connection
alive for the duration of a task that may run much longer than the user
wants to actively babysit it.

**Emotional job**: trust that stepping away doesn't cost anything. The
specific fear this displaces is *loss* — "did closing my laptop just kill
twenty minutes of agent work" — which is a materially worse failure mode
for an *agentic* tool than for a passive one, because the user cannot
simply re-run a deterministic command to get back to where they were; agent
work involves real side effects (files changed, commands run, review-queue
items generated) that may not be cheaply reproducible. This is the same job
mosh fulfills for "my SSH session survives switching WiFi networks," and
tmux/screen fulfill for "my long-running process survives me logging out"
— but stapler-squad's stakes are higher than either, because the "process"
here is an autonomous agent making decisions and taking actions, not a
static compile job whose only cost of interruption is wasted time. A user
who has internalized the *wrong* mental model from §2 (fearing the agent
stopped when it didn't, or not realizing it's still burning tokens/making
changes when they assumed it paused) doesn't get this job fulfilled even if
the technical guarantee (§4) actually holds — which is why §2's "make the
mental model explicit in the UI" flag and §4's "error states must be
debuggable and distinguishable" finding both feed the same underlying job:
the user needs to *know*, not just have it technically be true, that
walking away didn't cost them anything.
