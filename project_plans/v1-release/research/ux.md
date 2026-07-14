# UX Research — v1.0 New-Surface Features (splits, persistence, copy-mode, status bar, config/keybindings)

**Date**: 2026-07-13
**Scope**: the NEW v1.0 interaction surfaces that did not exist when `docs/ux/journey-map.md` was written
(10 journeys mapped against the one-pane MVP). This document does not repeat that map's findings — it
assumes the Ctrl-D hang is fixed, `kill` exists, and the non-loopback warning landed (all true per recent
commits). It focuses on what's *net-new* in `requirements.md`'s scope: splits, persistence,
scrollback/copy-mode, status bar, config/key-bindings, and how those interact with tymux's client/daemon
architecture and its AI-agent audience.

Feeds Phase 2 (research) of the SDD workflow for `project_plans/v1-release/`.

---

## 1. Comparable UX Patterns — tmux, zellij, wezterm

### Splits: creation and navigation

| Tool | Model | Creation | Navigation | Visual feedback |
|---|---|---|---|---|
| **tmux** | Arbitrary binary-tree layout per window | `prefix %` (vertical), `prefix "` (horizontal) — mnemonic only if you already know `%`/`"` mean "split here" | `prefix` + arrow, or `prefix o` to cycle | A thin border line; active pane usually distinguished only by cursor being live there (no default highlight unless configured) |
| **zellij** | Same tree model, but exposed through *modes* | `Ctrl p` enters Pane mode, then `d`/`r` for down/right split | Arrow keys while in Pane mode, or `Alt` + arrow outside any mode | **The status bar itself is the split UI** — entering Pane mode rewrites the bottom bar to `n new · d down · r right · x close · f fullscreen`, so the keybinding is discoverable at the moment of use, not memorized in advance |
| **wezterm** | Tree model, but splits are a *terminal emulator* feature, not a multiplexer-over-daemon feature | `LEADER` (commonly remapped to `Ctrl a`) + a symbol key (`\|` or `%`), fully user-configured in Lua | `LEADER` + `hjkl`/arrows, often vim-navigator-style | None by default; relies on the user's own Lua config for borders/titles |

**What works and why:**
- **Zellij's discoverability-by-doing** is the single most transferable insight here: the status bar doubles as a live keybinding cheat sheet scoped to the current mode, which eliminates the tmux "I forgot the split key" failure mode without requiring a manual. This is a mental-model win specifically because it defers memorization until the moment of need.
- **tmux's mnemonic symbols** (`%` looks like two panes side by side, `"` looks like a horizontal split) are a nice touch but only "click" retroactively, after someone already knows them — they are not self-discoverable on first use, which is why every tmux cheat sheet exists.
- **wezterm's config-first approach** (nothing is a keybinding until you write Lua for it) optimizes for power users who will invest in dotfiles, at the cost of an unopinionated out-of-box experience — not a great model for a tool trying to prove itself to a new evaluator.

### Copy-mode entry/exit and navigation

- **tmux**: `prefix [` enters copy-mode; `mode-keys vi` (or `emacs`) governs whether movement is `hjkl`/`v`/`y` (vi) or `Ctrl` combinations (emacs). Exit is `q` or `Escape` (in vi mode) or `Ctrl-c` (emacs mode). This dual-mode-keys config is *the* single highest-leverage "meet the user's muscle memory" lever tmux has — most technical users self-select into vi or emacs and expect their editor's navigation vocabulary to carry over.
- **zellij**: has scroll/search built in without a fully separate copy-mode; visual selection exists but multiple reviewers report friction — a pane border has to be toggled off to cleanly select text, which is a real regression versus tmux's "just drag or `v`" simplicity. This is a case where zellij's modernized model actually loses to tmux's older one.
- **wezterm**: `LEADER [` enters a vim-flavored copy mode; notably (per WezTerm's own issue tracker) copy-mode and search are *window-scoped, not pane-scoped*, and there's no per-pane visual indicator that a given pane is in copy-mode — a real gap for exactly the kind of "which pane am I actually navigating right now" confusion tymux should avoid.

**What works and why:** the near-universal expectation among the target audience is **vi-style navigation** (`hjkl`, `v` to select, `y`/`Enter` to yank, `/` to search, `Escape`/`q` to exit) because it's a superset of the muscle memory this audience already has from their editor, not just from tmux specifically. A dedicated, clearly-signaled mode (not an overlay that silently changes what keystrokes mean) avoids wezterm's "which pane is even in copy-mode" ambiguity.

### Status bar content

- **tmux**: fully user-configured (`status-left`/`status-right`/`status-format`), default shows session name, window list, hostname, time — informational but static; nothing changes based on interaction state.
- **zellij**: the differentiator is that it's **mode-reactive** — content changes to show exactly the keybindings relevant to the current mode, functioning simultaneously as status display and inline help.
- **wezterm**: tab bar by default (it's a terminal emulator, not primarily a multiplexer), workspace/pane info available but not a "mode cheat sheet" the way zellij's is.

**Takeaway for tymux**: a status bar that is *only* decorative (session/window name, clock) captures roughly the tmux-parity bar but wastes the highest-leverage opportunity in this whole feature set — using it as live, mode-aware key-binding help solves the exact "no detach key, no discoverability" gap the journey map already flagged as the project's worst usability floor. Given tymux is greenfield on config/keybindings anyway (per `requirements.md`'s Rabbit Holes), building the status bar and the key-binding system with this coupling in mind from day one is far cheaper than retrofitting it.

---

## 2. User Mental Models — what a tmux user expects vs. where tymux can reasonably diverge

tymux's own `requirements.md` explicitly names "interactive terminal users evaluating/adopting tymux as a
tmux alternative" as a primary user, and the journey map already established this audience is "porting
tmux muscle memory." That framing should govern every keybinding default below.

**Expect by muscle memory (match or pay a real cost for diverging):**
- A **prefix key** (`Ctrl-b` is tmux's default, `Ctrl-a` is screen's/commonly remapped) before multiplexer commands, not raw unprefixed hotkeys — this is the single most load-bearing convention, since it's what makes raw passthrough to the remote pty coexist with local multiplexer commands at all. tymux currently has *zero* local keystroke interception (pure passthrough per the journey map) — introducing a prefix key is not optional, it is the mechanism that makes detach/copy-mode/split-navigation possible in the first place.
- `prefix d` = detach. This is named explicitly in `requirements.md`'s Success Metrics as the concrete bar — "at minimum a working detach keybinding." Do not pick a different letter without a strong reason; this is the one binding a tmux user will reach for reflexively on day one.
- `prefix [` = enter copy-mode/scrollback, `prefix %`/`prefix "` (or a zellij-style mode entry) for splits, `prefix c` = new window — these are tmux's actual defaults and the path of least surprise.
- vi-style movement inside copy-mode (`hjkl`, `v`, `y`, `/`) as at least an option, ideally the default, given the audience overlap with vim/neovim users is very high in this niche.

**Reasonable to diverge, given tymux's different architecture:**
- **Session persistence semantics.** tmux's mental model is "the server process IS the sessions" — a tmux user has no concept of "restart the server, keep the sessions" because for tmux those are the same process. tymux's daemon/client split over gRPC makes "restart tymuxd without losing session *metadata*" a coherent, novel capability tmux literally cannot offer. This is a place to *lean into* the divergence and market it as a feature, not apologize for behaving differently — but the UX must be extremely explicit about what "restored" means (see §4) precisely because a tmux user's prior model (full live-process resume) will over-promise what's actually possible.
- **Structured/programmatic control surfaces** (`CapturePane` returning a typed grid, not ANSI bytes) have no tmux equivalent at all — tmux users have no prior mental model here, so there's no muscle memory to violate. This is pure greenfield and the journey map already found it's tymux's best-designed flow.
- **Config file format.** tmux users expect `tmux.conf` *syntax* but `requirements.md` has already explicitly scoped out syntax compatibility (Alternatives Considered: "unnecessary scope"). This is a safe divergence *semantically* (a modern format like TOML/KDL is fine) but the *binding names/verbs* should still map onto tmux's vocabulary (`bind-key`, `split-window`-equivalent concepts) so a tmux user's conceptual model transfers even though the syntax doesn't. Divergence in syntax + convergence in vocabulary is the sweet spot; divergence in both is what causes total reevaluation cost.
- **Multi-client attach arbitration.** The journey map already found tymux allows concurrent attach with no arbitration (Flow 6). tmux's own behavior here is also fairly loose, so there isn't a strong "expected" model to violate — this is a place tymux could actually differentiate positively (e.g., a read-only attach mode, or attach-count visibility) without breaking anyone's mental model, since tmux users don't have a strong prior here either.

**Where divergence is risky, not reasonable**: renaming or reordering the *verbs* users already know (e.g. using "detach" to mean something other than "leave the pane running, return to my shell," which requirements.md's own baseline names as the core missing gap) — semantic drift on already-overloaded terms erodes trust faster than syntax differences do.

---

## 3. Accessibility

This is a genuinely narrow area for a keyboard-only CLI tool, and honest scoping matters more than
aspirational claims here — most of what applies to GUI accessibility literally doesn't apply.

**Realistic and worth doing for v1.0:**
- **Keyboard-only navigation is already the entire input surface** — there's no incremental "make it keyboard accessible" work the way there would be for a GUI. The relevant accessibility work is really about *predictability and escapability*: every mode tymux introduces (copy-mode, any future command mode) must have an unambiguous, always-working exit key (`Escape` and/or `q`), because a mode with no visible exit is a trap for *any* user, not just ones using assistive tech — this is the same root problem as the journey map's "no detach primitive" finding, just generalized to every future mode.
- **Terminal color contrast for the status bar**: this is real and controllable. Recommendations:
  - Don't hardcode 256-color/truecolor-only palettes — degrade to ANSI 16-color reasonably, since users on low-color terminals or high-contrast/reduced-color accessibility themes exist.
  - Respect `NO_COLOR` (an already-established informal standard) to disable status-bar coloring entirely — cheap to implement, meaningfully helps both accessibility and screen-reader-adjacent tooling that chokes on ANSI escapes.
  - Don't encode information (e.g. "pane is dead" / "copy-mode active") in color alone — pair color with a text/symbol change, since color-only signaling fails for colorblind users and for any screen reader or terminal-scraping tool that strips ANSI.
- **Screen-reader considerations (narrow but real, per the research above)**: full-screen TUI chrome — status bars, redrawn/partial-screen updates, border-drawing — is exactly the pattern that breaks screen readers, because screen readers work by reading linear text output, not composited terminal regions. tmux/screen have documented, real accessibility gaps here (particularly around split panes), and there's no full fix available to a CLI-only tool. This is legitimately **out of scope** to solve fully for v1.0, but two cheap mitigations are worth doing:
  - A "plain mode" / `--no-status-bar` flag that suppresses the status bar chrome entirely and falls back to pure linear pty passthrough — the closest thing to an accessible mode achievable without real investment, and it's nearly free since it's just "don't render the chrome," not a parallel accessible UI.
  - Make sure any *new* redraw logic (status bar, mode indicators) never rewrites/repaints regions the pty itself already emitted — beyond the "don't corrupt output" correctness requirement already flagged in `requirements.md`'s Rabbit Holes, this also matters for accessibility because screen readers and terminal-recording/logging tools depend on output being append-only, not selectively overwritten.
- **Out of scope, and reasonable to say so explicitly in docs**: full screen-reader compatibility for split-pane layouts, braille-display region tracking, or anything requiring semantic (not just visual) pane boundaries — this is a known-hard, largely-unsolved problem across the entire terminal-multiplexer category (tmux/screen included), not a tymux-specific gap to apologize for. A one-line "Accessibility" note in the docs (what's supported: keyboard-only operation, `NO_COLOR`, a no-chrome mode; what isn't: screen-reader-aware split navigation) is more honest and more useful than silence.

---

## 4. Error States and Edge Cases — concrete UX recommendations

The journey map already established the pattern to follow here: **every current CLI failure path funnels
into `anyhow`'s raw Debug dump** (its #2 cross-cutting gap). The new v1.0 surfaces multiply the number of
distinct failure states, so this is the moment to design a friendly-error convention once, not patch it
per-feature. Concrete cases:

### A persisted session's pane can't actually be restored after a daemon restart
This is explicitly flagged as a feasibility risk in `requirements.md` — full live-process resume isn't
achievable without CRIU-class OS support, so "the process is gone" is not an edge case, it's the *expected*
outcome for every restart under any realistic durability contract.
- **Don't** silently show the session in `tymux ls` as if nothing happened — that recreates exactly the
  "dead pane looks identical to a live one" ambiguity the journey map flagged for `CapturePane` (Flow 5),
  now at the session level.
- **Do** give the restored session an explicit, visible status distinct from "live": e.g. `tymux ls` shows
  `myproject  [restored — not running]` vs `myproject  [attached]` vs `myproject  [detached, live]`. This
  needs a status/liveness field in the protocol regardless (the journey map already named this the
  single highest-leverage protocol fix) — persistence just makes the need non-optional rather than nice-to-have.
- **Do** make `tymux attach` on a restored-but-dead session either (a) offer to respawn the original command
  in a fresh pane with a clear "this is a NEW process, scrollback before restart is what's shown, nothing
  is live-resumed" message, or (b) fail fast with that same explanation, rather than attaching into a pty
  that silently does nothing. Silence here is the worst outcome — it looks exactly like the Ctrl-D hang bug
  that was just fixed, from the user's point of view.
- **Do** be explicit in the CLI's language about the durability contract at the moment it matters (on
  restore), not just in docs — e.g. "Session metadata restored. Scrollback captured before the last daemon
  restart is available; the process itself is not resumed." Users trust a tool more when it tells them
  the truth about a hard limitation than when it either overclaims or stays silent.

### A split can't be created because the terminal is too small
- **Don't** silently no-op or create a degenerate 1-row/1-column pane — this produces a corrupted-looking
  render that's indistinguishable from a bug.
- **Do** reject the split with a specific, actionable message before touching layout state:
  `"Can't split: pane is 12 rows, minimum for a horizontal split is ~20 rows. Resize your terminal or close another pane first."`
  Include the actual numbers, not just "too small" — actionable errors state the constraint, not just
  that one was violated.
- **Do** this check client-side (CLI) where possible for instant feedback, but also enforce it
  daemon-side (since programmatic/AI-agent clients bypass the CLI entirely) — this is a case where the
  two audiences (interactive human, AI agent) need the same guarantee expressed twice: a friendly string
  for the human path, a structured gRPC error code/detail for the programmatic path (see below).

### Copy-mode is entered on a pane whose process has already exited
- **Don't** treat this as an error — tmux's own behavior (and the reasonable one) is that copy-mode on a
  dead pane still works over whatever scrollback exists; the pane being dead and the scrollback being
  navigable are orthogonal.
- **Do** surface the pane's dead state *within* copy-mode's own status-bar segment (e.g. a `[exited: code 0]`
  or `[exited]` marker), since a user entering copy-mode on a pane they didn't realize had already exited
  is a real, mildly confusing moment worth one line of chrome to resolve — again downstream of the
  liveness-field gap the journey map already flagged as the top cross-cutting fix.
- **Do** make the exit path from copy-mode identical regardless of pane liveness (`q`/`Escape`) — don't let
  the dead-pane case have a different, subtly-different exit key, which is exactly the kind of inconsistency
  that turns one bug report into a systemic trust problem.

### General convention: human vs. programmatic error surfaces
Every new failure mode above has two audiences with different needs (per `requirements.md`'s own Users
list: interactive humans *and* AI agents driving sessions over gRPC directly). The design should produce,
for every new error class introduced by v1.0 scope:
1. A stable gRPC status code + structured error detail (not just a status message string) — this is what
   makes tymux's "structured programmatic control" pitch (see §5) actually true for error handling, not
   just the happy path.
2. A CLI-side friendly-message layer that translates that structured detail into human language — this is
   the fix for the journey map's #2 cross-cutting gap, and every new v1.0 error should be designed to flow
   through this layer from day one rather than accreting more raw `anyhow` dumps.

---

## 5. Jobs-to-be-Done — does the v1.0 config/keybinding/status-bar design support the jobs?

**Functional job**: *structured, programmatic control of terminal sessions* — an AI agent or script needs
to create sessions, read state, and drive input/output predictably, without ANSI-scraping. This is already
tymux's best-proven capability (`CapturePane`, per the journey map, is "the best-designed flow in the
codebase"). Does v1.0's new UX scope support or undercut this job?
- **Supports it**, *if* splits/windows get a real addressing scheme (window/pane IDs, not positional
  `windows[0].panes[0]` indexing the journey map already flagged as broken the moment splits exist) and if
  the status bar's state is also queryable structurally (not just rendered as bytes) — an agent should be
  able to ask "what does the status bar currently show" as structured data, the same way it can ask for
  pane content structurally today. This is worth stating as an explicit design constraint for Phase 3: **the
  status bar's content model and the config/keybinding system should be introspectable over gRPC, not just
  rendered client-side**, or the tool re-creates the ANSI-scraping problem it was built to avoid, just one
  layer up (scraping status-bar text instead of pane text).
- **Undercut risk**: if key-binding behavior is entirely client-local (CLI-side keystroke interception with
  no server-visible concept of "user pressed detach"), an AI agent driving a session has no way to trigger
  the equivalent action programmatically except by knowing the CLI's private keymap — the RPC surface should
  expose the *actions* (Detach, EnterCopyMode, SplitPane) as first-class calls independent of however a
  human triggers them via keystroke, so the functional job holds for both audiences symmetrically.

**Emotional job**: *confidence that AI-agent-driven sessions behave predictably.* This is really a trust
job — the emotional payoff of choosing tymux over tmux for agent-driven work is "I won't get surprised."
- **Supports it**: explicit, truthful status/liveness signaling (§4's recommendations), structured errors
  instead of raw dumps, and a documented, honest persistence contract ("metadata survives, live process
  does not, here's exactly what that means") are all trust-building moves — an agent operator's worst
  outcome is silent, ambiguous failure (exactly the old Ctrl-D hang), not a loud, well-typed one.
  Every recommendation in §4 is in service of this job specifically.
- **Undercut risk**: if the persistence UX (or docs) imply more continuity than the daemon can actually
  deliver — e.g. a `tymux ls` that shows a restored session identically to a live one — the very first
  time an agent operator discovers the gap (probably by an agent silently failing against a dead pane) is
  the moment they stop trusting the tool for anything unattended, which is a much worse trust failure than
  never having claimed the capability. Given `requirements.md` already flags this contract as
  Phase-3-TBD, this is the single highest-leverage place to over-invest in honesty over polish.

**Social job**: *using/recommending a novel tool to peers.* For a solo/side-project-scale open-source tool
being pitched partly on "here's a real cross-language client, not just an aspiration," the social payoff is
being able to say "I evaluated this and it holds up," not just "it exists."
- **Supports it**: a status bar and keybinding system that visibly *out-perform* tmux's own defaults in
  discoverability (the zellij-style "show me what I can press right now" pattern from §1) gives an
  evaluator something concretely better to point to, not just architectural novelty they'd have to explain
  to someone else. "It has a self-documenting status bar" is a much easier thing to recommend than "it has
  a gRPC API," which undersells to a non-technical or time-constrained peer.
- **Undercut risk**: recommending a tool whose config format doesn't map onto tmux vocabulary at all (full
  divergence, not just syntax divergence — see §2) raises the switching cost the recommender has to justify
  to their peer, which works against the social job even if the underlying engineering is sound. Keeping
  binding *verbs* tmux-recognizable while modernizing the *format* (per §2's recommendation) directly serves
  this job by keeping the "why should I switch" pitch short.

**Net assessment**: the current requirements scope (structured error handling implied but not yet
mandated, persistence contract explicitly TBD, cross-language client explicitly required and validated
against a real RPC) is well-aligned with the functional and emotional jobs *if* Phase 3 planning makes two
things explicit design requirements rather than implementation details: (1) status-bar/keybinding actions
must be introspectable/triggerable over gRPC, not just rendered client-side, and (2) the persistence
UX must actively communicate the durability contract at the moment of restore, not just in README prose.
Both are cheap to decide now and expensive to retrofit after the wire protocol and CLI rendering model are
built around an implicit assumption.

---

## Summary of Concrete Recommendations for Phase 3 Planning

1. Introduce a **prefix-key model** (tmux-vocabulary-compatible verbs, format free to diverge) as the
   mechanism underlying detach, copy-mode entry, and split creation — this is a prerequisite, not a
   parallel feature, since tymux currently has zero local keystroke interception.
2. Default `prefix d` = detach, `prefix [` = copy-mode, vi-style movement inside copy-mode as default or
   at minimum a first-class option.
3. Build the status bar as **mode-reactive, not static** — it should double as inline keybinding help,
   directly resolving the journey map's "no detach primitive"/discoverability gap rather than just adding
   decoration.
4. Add a **liveness/status field** to the protocol (already the journey map's top cross-cutting
   recommendation) — this document independently arrives at the same requirement from three separate
   angles: persistence restore state, dead-pane copy-mode, and status-bar truthfulness.
5. Design **structured gRPC errors + a CLI friendly-message translation layer** as a cross-cutting v1.0
   requirement, not a per-feature afterthought — every new error case in this document (undersized split,
   dead-pane restore, copy-mode-on-exited-pane) should flow through it from day one.
6. Make the **persistence restore UX explicit and honest** at the moment of restore (not just in docs)
   about what did and didn't survive — the single highest-leverage trust-preserving move given the
   feasibility risk already flagged in requirements.md.
7. Expose **status-bar content and key-binding actions over gRPC**, not just as client-rendered/
   client-local behavior, so the AI-agent audience isn't forced back into scraping — keeping the
   `CapturePane` model's "structured, not ANSI-scraped" promise consistent across all new v1.0 surfaces.
8. Ship a **`NO_COLOR`-respecting, `--no-status-bar`-capable "plain mode"** as the realistic, low-cost
   accessibility floor; document explicitly (one paragraph) what's out of scope (screen-reader-aware split
   navigation) rather than staying silent on it.

## Sources

- [Zellij: The Impressions of a Casual tmux User — Keyhole Software](https://keyholesoftware.com/zellij-the-impressions-of-a-casual-tmux-user/)
- [Zellij vs tmux: The Modern Terminal Multiplexer (2026) — Petronella](https://petronellatech.com/blog/zellij-terminal-multiplexer-guide-2026/)
- [tmux vs zellij: Which Terminal Multiplexer Wins in 2026 — Command in Line](https://www.commandinline.com/tmux-vs-zellij-comparison/)
- [tmux vs Termdock vs Zellij for AI Agents — Termdock](https://www.termdock.com/en/blog/terminal-multiplexing-tmux-termdock-zellij)
- [Key Binding — Wez's Terminal Emulator](https://wezterm.org/config/keys.html)
- [Make Wezterm Mimic Tmux — DEV Community](https://dev.to/lovelindhoni/make-wezterm-mimic-tmux-5893)
- [Request Copy Mode and status bar for each pane · Issue #6241 · wezterm/wezterm](https://github.com/wezterm/wezterm/issues/6241)
- [The State of Linux Command Line Accessibility — Blind Computing](https://blindcomputing.org/linux/state-of-cli-accessibility/)
- [Ask HN: How do blind people code and work with terminals?](https://news.ycombinator.com/item?id=5352608)
- [How to configure tmux, from scratch — Ian Henry](https://ianthehenry.com/posts/how-to-configure-tmux/)
- [tmux copy-mode — Waylon Walker](https://waylonwalker.com/tmux-copy-mode/)
