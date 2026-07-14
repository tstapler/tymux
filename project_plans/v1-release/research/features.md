# Research: Features — Prior Art, Edge Cases, Unstated Needs

**Project**: v1-release
**Phase**: 2 (Research)
**Scope**: For each in-scope v1 item (splits, persistence, scrollback/copy-mode, status
bar, config/key-bindings, cross-language client's implications), what does tmux
itself do, what do modern alternatives (zellij, wezterm) do differently, what
edge cases does `requirements.md` not already call out, and what do users
(including AI-agent consumers) need that isn't stated explicitly.

---

## 1. Splits — layout model, tree edge cases

### tmux's actual model: an N-ary tree of alternating-orientation splits, not a binary tree, not a grid

tmux does **not** arrange panes in a fixed grid. Internally, a window's
layout is a tree of cells: each cell is either a `left-right` container, a
`top-bottom` container, or a leaf holding one pane. Containers hold a *list*
of children, not strictly two — so it's more accurate to describe it as an
**N-ary tree with alternating split orientation per level**, not a strict
binary tree (splitting `-h`/`-v` repeatedly against different targets can
produce a 3+-child container at one level; tmux's own preset `tiled` layout
in particular produces flat multi-child rows/columns rather than deep binary
nesting).

- `split-window -h` — "horizontal split": divides the target pane
  side-by-side (left/right). `-v` (the default if neither is given) —
  "vertical split": divides it top/bottom (stacked). This naming (horizontal
  = side-by-side) trips people up; worth deciding tymux's own terminology
  explicitly rather than copying tmux's if it's judged confusing.
- A split always operates on **the currently-active pane** — it splits one
  leaf into two, not the whole window. New pane placement: `-b` puts the new
  pane before (left of / above) the target instead of after.
- Sizing: `-l <size>` sets the new pane's size in lines/columns, or as a
  percentage (`-l 30%`); `-p <percentage>` is shorthand for a percentage.
  Source: tmux(1) man page (`man7.org/linux/man-pages/man1/tmux.1.html`).

### The layout string — a real serialization format worth mirroring in the Rust type

tmux can serialize a whole window's arrangement to a compact string (shown by
`list-windows`, restorable via `select-layout`), e.g.:

```
2e3a,80x24,0,0{40x24,0,0,0,39x24,41,0,1}
```

Structure: `checksum,WIDTHxHEIGHT,X,Y` for the root cell, followed by a
bracketed list of children — `{...}` for a left-right container, `[...]` for
top-bottom, commas separating siblings, and a leaf written as
`WIDTHxHEIGHT,X,Y,PANE_ID`. The checksum is a hash of the rest of the string
used to validate `select-layout` input hasn't been corrupted. Containers
parse recursively, so arbitrarily deep nesting is representable.

This is a strong, already-proven precedent for tymux's own Rust layout type:
a recursive enum (`Leaf(PaneId, Rect)` vs `Split{ orientation, children: Vec<(Node, size)> }`)
is the natural analogue, and — like tmux — should probably support N children
per split level rather than forcing strict binary nesting, since forcing
binary-only nesting means "3-way even split" requires two artificial nested
2-way splits instead of one 3-child node. That said, a **strict binary tree**
(always exactly 2 children, recursively) is a legitimate, simpler
implementation choice too — it trades "an extra synthetic split node for
3-way-even layouts" for a much simpler recursive resize/collapse algorithm,
and tmux's own preset layouts (`tiled`, `even-horizontal`) exist precisely
because manually building flat N-way layouts via repeated binary splits is
tedious for humans. **This binary-vs-N-ary tree choice is a concrete decision
this project's planning phase should make explicitly**, not inherit from tmux
by default — tmux's N-ary model is more expressive but more complex to
implement correctly (resize/collapse math for "redistribute this pane's
freed space across N siblings" vs. "give it all to the one sibling").

tmux also ships **preset layouts** as one-shot rebalancing commands:
`even-horizontal`, `even-vertical`, `main-horizontal`, `main-vertical`,
`tiled`. These are worth having as a v1 nicety once any tree structure
exists, since manually splitting-and-resizing to an even N-way layout by
hand is real friction tmux users hit constantly.

### resize-pane and its edge cases

`resize-pane -U/-D/-L/-R <adjustment>` (lines/columns, default 1) resizes
relative to the current pane; `-x`/`-y <size>` (absolute, or `N%`) sets an
absolute size. Default bindings: `prefix + arrow keys` (held via repeat) for
coarse resize, fine-grained resize via other modifiers. A resize only
directly affects the boundary with the **immediate adjacent sibling(s) in
the same split container** — it does not renegotiate the whole tree, though
of course the affected container's own size is bounded by its parent.

Edge cases `requirements.md` doesn't call out, that the planning phase should
decide on explicitly:
- **Minimum pane size.** tmux enforces a practical minimum (a pane can't
  shrink to zero or negative content area); once a window is too small to
  fit the requested split, `split-window` fails outright with an error
  rather than silently producing a degenerate pane. tymux needs the same
  guard — decide the minimum (tmux's own floor is small, effectively "a few
  cells," but the real-world failure a user hits is "no space for new pane,"
  not literally 1x1).
- **Closing a pane collapses its container.** When one pane of a 2-child
  split closes, the sibling absorbs the freed space and the (now
  single-child) split container is removed from the tree entirely — replaced
  by its one surviving child. This can cascade: if that collapse leaves ITS
  parent also single-child, the parent collapses too, recursively up the
  tree. **This recursive-collapse behavior is not mentioned anywhere in
  requirements.md and is the single trickiest correctness case in the whole
  splits epic** — get this wrong and repeated split/close cycles leave
  degenerate single-child containers cluttering the tree.
- **Closing the last pane in a window closes the window; closing the last
  window closes the session.** Straightforward cascade, but worth stating as
  an explicit invariant/test case.
- **Closing a pane with nested children on the other branch** (e.g. a 3-way
  layout where one branch is itself split again) — the collapse logic above
  must be genuinely recursive/tree-shaped, not assume a flat 2-pane case;
  this is where a naive "just remove from a Vec" implementation (the kind of
  approach ADR 0001 explicitly warns against — "not just add more panes to a
  Vec") will silently misbehave.
- **Zoom** (`prefix z` / `resize-pane -Z`) temporarily makes one pane fill
  the whole window while preserving the underlying tree geometry; un-zooming
  restores it exactly. This is a good, low-cost v1 candidate since it doesn't
  require any new tree-mutation logic — just a per-window "zoomed pane id or
  none" flag that changes rendering/sizing without touching the tree.
- **Window resize (client terminal resize) rescales the whole tree
  proportionally.** The specific edge case the journey-map already flagged
  as a known gap (cross-cutting gap #6, "no concurrency arbitration") —
  **multiple attached clients with different terminal sizes** — has a real
  tmux answer worth adopting explicitly rather than reinventing: tmux's
  `window-size` option has four modes — `smallest` (default; window shrinks
  to the smallest attached client, larger clients see the unused margin
  filled with a `·` filler character), `largest`, `latest` (most-recently-active
  client's size wins), or `manual` (fixed, only changed via explicit
  `resize-window`). tymux's planning should pick one of these as the default
  policy for concurrent attachers rather than leaving it as "last resize
  message wins" (today's actual, unintentional behavior per the journey
  map).
- **Other tmux operations worth at least a deliberate in/out-of-scope call**:
  `synchronize-panes` (broadcast keystrokes to every pane in a window
  simultaneously — a real, well-loved power feature), `swap-pane` (swap two
  panes' positions without changing their sizes), `join-pane`/`break-pane`
  (move a pane between windows, or promote a pane to its own window). None of
  these are in `requirements.md`'s scope and none need to be for v1 — noting
  them here so they're a deliberate exclusion, not an oversight discovered
  later.

### zellij and wezterm confirm tree-of-splits is the converged answer

- **zellij**: layouts are declarative KDL files; a `pane` block takes a
  `split_direction` ("vertical" or "horizontal") and nests child `pane`
  blocks — structurally the same recursive-container idea as tmux's
  `{`/`[` format, just declared up front in a config file rather than built
  interactively split-by-split (though zellij also supports interactive
  splitting at runtime, which produces the same tree shape). Root-level
  default direction is horizontal.
  (`zellij.dev/documentation/creating-a-layout.html`)
- **wezterm**: its Lua `pane:split()` API and `SplitHorizontal`/`SplitVertical`
  key assignments operate the same way as tmux — split the *current* pane,
  choosing which side (`Top`/`Bottom`/`Left`/`Right`) the new pane appears
  on. wezterm's mux maintains this as a tree per tab, same shape again.
  (`wezterm.org/config/lua/keyassignment/SplitPane.html`)

Three independent implementations (tmux, zellij, wezterm) all converge on
"recursive splits of the currently-focused pane, tree-shaped layout state" —
this isn't tmux being an outlier; it's the natural shape a terminal-splitting
model takes. tymux should adopt the same shape with high confidence, and
spend its actual design effort on the binary-vs-N-ary tree tradeoff above and
on getting collapse-on-close right, not on inventing an alternative model.

---

## 2. Persistence — the realistic durability contract

### What tmux itself actually guarantees (and it's less than people assume)

tmux's server is a single background process; all session/window/pane state
lives **only** in that process's memory. Detach/reattach survives because
the *client* disconnecting doesn't touch the server — that's the entire
persistence story. If the tmux **server** process dies (crash, `kill -9`,
OOM-kill, host reboot), every session is gone with zero recovery — there is
no on-disk session state in stock tmux at all. tymux's current status
section already gestures at this ("no persistence... same as tmux's own
server model but without tmux's socket-survives-crash guarantee") — worth
tightening to be explicit that tmux's guarantee is **client-crash/disconnect
survival only**, not daemon-crash survival, so nobody scopes v1 against an
inflated idea of what tmux itself actually promises.

### tmux-resurrect: what it actually saves/restores, and how

(`github.com/tmux-plugins/tmux-resurrect`, README fetched directly.)

**Saves**: every session, window, and pane; each pane's exact layout
position within the window (so the split tree is preserved); each pane's
current working directory; the active/alternate session and
active/alternate window per session; window focus state; the active pane
per window; and grouped-session relationships (multi-monitor setups).

**Restores running programs, not just shells** — this is the key mechanism
that makes it feel like more than metadata: a conservative default allow-list
of programs (`vi vim nvim emacs man less more tail top htop irssi weechat
mutt`) get *relaunched inside the pane* rather than just landing you at a
bare shell prompt. For vim/neovim specifically there's a documented special
path that restores the actual **editor session** (open buffers, etc.) via a
vim-side plugin integration — the deepest "restore" story of anything in the
plugin.

**What it does NOT and cannot do**: it does not checkpoint/resume an
arbitrary process's live runtime state. It works by walking `list-panes`/
`list-windows` at save time, writing a plain-text snapshot, and at restore
time **recreating the panes and re-running the same command line** in a
fresh shell in the right pane/directory. A long-running build or a REPL
mid-computation is not preserved — only the fact that "this pane was running
`npm run build`" is remembered, and (for allow-listed programs) that command
gets re-run from scratch. It's explicitly idempotent (won't restore into
panes/windows that already exist).

**Mechanics**: plain-text save files at `~/.local/share/tmux/resurrect/`
(respecting `$XDG_DATA_HOME`, historically `~/.tmux/resurrect/`), timestamped
filenames (`tmux_resurrect_YYYY-MM-DDTHH:MM:SS.txt`), with a `last` symlink
pointing at the most recent save — restoring an older snapshot is just
repointing that symlink. Save/restore are triggered by two key bindings,
`prefix + Ctrl-s` / `prefix + Ctrl-r`.

An optional "restoring pane contents" feature exists (captures
`capture-pane` text) but is a secondary, opt-in feature layered on top of
the core metadata-and-relaunch mechanism, not the default behavior.

### tmux-continuum: the autosave/autorestore layer on top

(`github.com/tmux-plugins/tmux-continuum`, README fetched directly.) A
strict dependent of tmux-resurrect (both plugins must be installed;
continuum has no independent save format). It (a) runs resurrect's save
automatically on a background interval — **default every 15 minutes** — via
a loop that requires the status line to be enabled to drive it, and (b) can
auto-restore on tmux **server start** specifically (`@continuum-restore on`)
— explicitly documented as triggering *only* on a fresh server start, never
on a `.tmux.conf` reload. This start-vs-reload distinction matters for
tymux's daemon too: an equivalent "restore on daemon start" hook needs the
same care to not accidentally re-trigger on every config change.

### A directly-relevant emerging pattern: session-aware restoration of AI agent panes

Worth flagging as concrete evidence for tymux's own stated AI-agent
audience: `tmux-assistant-resurrect` (`github.com/timvw/tmux-assistant-resurrect`)
extends tmux-resurrect specifically to make AI coding-assistant panes (Claude
Code, OpenCode, Codex CLI) survive a restart *meaningfully*, not just as a
relaunched blank shell. It captures the assistant's **session ID** (via
hooks/plugins/process-arg parsing — extraction logic is tool-specific since
each stores session metadata differently), plus the original CLI flags
(`--model opus`, permission flags) and environment variables, and on restore
issues a resumption command (e.g. `claude --resume <session-id>`) instead of
just re-running `claude` from scratch. It explicitly does **not** restore
in-flight tool calls or pending operations — same fundamental ceiling as
resurrect's own vim-session restore: "resume conversation/context," not
"resume the literal live process."

This is a strong, concrete signal that "relaunch with the tool's own
resume/session-id mechanism" (where the tool has one) is a legitimate, proven
middle tier between bare metadata and true process checkpointing — and it's
directly applicable to tymux's stated agent audience: a pane running `claude`
or similar is exactly the case where "just re-run the same command line"
loses the most value, and where "re-run with a resume flag if we captured
one" is cheap and high-payoff.

### Recommended tiered contract for v1 (grounded in what's actually proven achievable)

- **Tier 0 — metadata (cheap, do this)**: session/window/pane ids, names,
  working directory, the layout tree/split geometry, and the command line
  originally run, survive a daemon restart. On restart, the OS processes are
  gone (they can't not be — this is the same ceiling tmux itself has), so
  each pane needs an explicit "dead, needs relaunch" state exposed to
  clients (ties directly into the cross-cutting "no liveness field" gap the
  journey-map already flagged) rather than silently vanishing or panicking.
- **Tier 1 — auto-relaunch + resume hints (moderate, resurrect-proven)**:
  optionally, on restart or on demand, re-spawn the pane's original command
  in a fresh pty in the right working directory — mirroring resurrect's core
  mechanism exactly. If the daemon captured a resume-capable identifier for
  known interactive tools (a stretch but cheap to design room for, given the
  `tmux-assistant-resurrect` precedent and tymux's own agent-facing
  audience), pass that through on relaunch.
  scrollback (last N lines) persisted alongside the metadata so a
  reattached pane isn't a totally blank slate — this is static, non-live
  text prepended to the fresh pane, not resumed interactivity, exactly as
  resurrect's own optional pane-contents feature works.
- **Tier 2 — true live-process resumption**: explicitly **not** realistic
  for v1, and arguably not realistic ever without OS-level checkpointing
  (CRIU on Linux — itself limited with certain fd/socket types — no real
  macOS equivalent). `requirements.md`'s own Feasibility Risks section
  already reaches this conclusion; this research confirms it by showing that
  even the most mature persistence tool in the tmux ecosystem (resurrect,
  in production use for over a decade) never attempted it either — it solves
  the same problem the same way (relaunch + metadata), which is strong
  validation that this is the right ceiling to design to, not a corner being
  cut.

**Recommendation**: commit to Tier 0 as the hard v1 requirement (this alone
closes the "killing tymuxd loses everything silently" gap), with Tier 1 as
the stretch goal `requirements.md` already frames it as ("full scrollback
replay is a stretch goal... not a hard requirement") — this research confirms
that framing is exactly right and gives it a name (Tiers 0/1/2) to write into
the plan.

### zellij's persistence story (secondary check)

zellij has session detach/reattach (same as tmux — surviving client
disconnect), but no built-in equivalent of resurrect/continuum for surviving
a full server crash/restart either — it has the same fundamental ceiling as
tmux. No new prior art here beyond confirming tmux isn't uniquely limited.

---

## 3. Scrollback / copy-mode — minimal vs. full

### How copy-mode works today in tmux

Entered via `prefix [`. Two key tables selected by the `mode-keys` option
(`vi` or `emacs`, default `emacs`, set via `setw -g mode-keys vi`):

| Action | vi | emacs |
|---|---|---|
| Move cursor | `h j k l` | arrow keys |
| Start selection | `v` (char), `V` (line) | `Ctrl-Space` |
| Rectangle/block selection | `Ctrl-v` (toggle) | (via `rectangle-toggle` binding) |
| Copy selection & exit | `y` / Enter | `Ctrl-w` / `M-w` |
| Clear selection | Escape | `Ctrl-g` |
| Search forward / backward | `/` / `?`, then `n`/`N` for next/prev | `Ctrl-s` / `Ctrl-r` |
| Exit copy-mode | `q` | Escape |

Copying by default writes into tmux's own internal **paste-buffer** system
(`set-buffer`/`show-buffer`/`paste-buffer`, `prefix ]` to paste the most
recent buffer; multiple named buffers exist and are choosable via an
interactive buffer picker). Getting text *out* to the real OS clipboard
(especially relevant for tymux, which — like tmux over SSH — is a
remote-attach model where the daemon has no direct access to the local
user's clipboard) is a separate, optional layer: the `set-clipboard` option
(`off`/`on`/`external`, default `external` since tmux 2.6) makes tmux emit an
**OSC 52** escape sequence that many terminal emulators intercept and use to
set the *real* system clipboard — this works even over SSH with no X11
forwarding, but requires the outer terminal emulator to support and enable
OSC 52 passthrough, and (per tmux's own docs) is a real security
consideration: any untrusted program inside the multiplexer can, in
principle, write to the user's real clipboard if this is left on
indiscriminately.

Scrollback is bounded by `history-limit` (default **2000 lines** per pane,
fixed at pane-creation time — cannot be changed for an already-running
pane, only for panes created after the option changes). Outside copy-mode,
mouse-wheel scroll auto-enters copy-mode when `mouse` support is on.

### Minimal v1 subset vs. tmux's full feature set

**Minimal, realistically useful v1 subset** (roughly what's needed for
"scrollback is capturable and a human can interactively navigate/copy from
it," `requirements.md`'s literal bar):
1. Enter/exit a scroll-mode (a single keybinding, ties into the
   config/key-binding epic below)
2. Line/page navigation (arrows or hjkl, PageUp/PageDown)
3. Visual (character-range) selection
4. Copy selection to *some* buffer, and paste it back
5. Basic incremental forward search (`/`-style), next-match only — even
   without backward search or a full match-count UI this is the single most
   requested copy-mode feature after "can I scroll at all"

**Deliberately deferrable to post-v1** (full tmux surface, explicitly out of
scope for the minimal bar): both vi *and* emacs key-table choice (pick one
convention for v1, likely vi given tmux's own default lean in most
configs people actually use, or make it a config option later, not a launch
requirement); rectangle/block selection; multiple *named* buffers and an
interactive buffer picker; `copy-pipe` (pipe selection directly to an
external shell command); word-selection via double-click; a `choose-tree`/
`choose-buffer` interactive picker UI; real OS-clipboard integration via OSC
52 (genuinely valuable, since tymux like tmux-over-SSH can't reach the local
clipboard directly any other way, but is a meaningfully separate, riskier
feature — the security caveat above is real — and cleanly separable from
"scrollback navigation and internal copy/paste works at all").

### Copy-mode is a *rendering/input-interception* feature, not a pure server-side one

This is already flagged in `requirements.md`'s Rabbit Holes ("requires the
CLI to intercept keystrokes locally... same underlying gap as no detach
key"), and this research confirms that's the right framing — tmux's
copy-mode is fundamentally the client reinterpreting keystrokes locally
(navigating a local scrollback view) rather than a special server RPC per
keystroke. tymux's design should treat "enter local key-interception mode"
(shared machinery for detach-key handling and copy-mode both) as the one
piece of new CLI architecture this epic and the key-binding epic both need,
not two separate efforts.

---

## 4. Status bar — signal vs. decoration

### tmux's actual out-of-the-box default

Left-to-right: session name in brackets, then the window list (format
`#I:#W` — window index and name — with the *current* window visually
distinguished via `window-status-current-format` styling), then the active
pane's title in quotes, then on the right side, date and time (`%H:%M`
clock format, date via strftime-style format specifiers). Window-list flags
(`#F`) mark state per window: `*` current, `-` last-active, `#` has
activity, `!` has a bell, `Z` zoomed.

Everything beyond this is opt-in via format-string variables the user must
explicitly add: `#H` (hostname), `#{pane_current_path}`,
`#{pane_current_command}`, or arbitrary shell-command interpolation
`#(some command)` re-evaluated on an interval controlled by
`status-interval`. Widely-used *plugins* (tmux-battery, tmux-cpu, and
git-branch snippets) all ride on this same shell-interpolation mechanism —
tmux ships none of this by default; the status bar is blank/minimal until a
user customizes it.

### Genuinely useful vs. decorative for tymux's first version

**Useful, and arguably load-bearing given tymux's current total lack of UI
chrome** (a raw pty passthrough with zero visual context today, per the
journey-map): session name; window list with the active window visually
distinguished; something conveying **pane count / current split
layout at a glance** (tmux doesn't need this as prominently since panes are
visually self-evident on screen, but it's a good added signal); a
**zoom indicator** (so a user doesn't get confused about why they can't see
other panes); an **attached-client count** (directly closes the
already-documented cross-cutting gap #6 — "no attach-count visibility" — a
genuinely new, tymux-specific need beyond tmux's own defaults, precisely
*because* tymux's multi-attach story is currently silent where tmux at least
has `list-clients`); and a **liveness/connection indicator** (ties to
cross-cutting gap #1 — knowing at a glance whether the daemon connection or
the pane itself is still alive is more valuable for a young, unproven tool
than for tmux, which users already trust not to silently hang).

**Decorative / defer past v1**: clock and date (nice, not load-bearing —
the user's own terminal emulator or shell prompt usually shows this anyway);
hostname (only useful multi-host, explicitly out of scope per
requirements.md's loopback-only trust model); battery/weather/custom
shell-command widgets (classic plugin territory, zero-value for proving the
core multiplexer works); git-branch (valuable to many but squarely a
"plugin ecosystem" feature and `requirements.md` explicitly puts a
plugin/extension system out of scope for v1).

### zellij and wezterm: "helpful defaults" vs. tmux's "blank by default"

zellij's bottom bar is **not** customizable-by-default the way tmux's is —
by design it shows tabs plus a live, **context-sensitive keybinding hint
bar**: pressing `Ctrl-p` to enter pane-mode changes the bar to show
pane-mode's own available keys, `Ctrl-t` for tab-mode shows tab-mode's keys,
etc. wezterm's tab bar defaults to showing open tabs with minimal
configuration needed. Both ship meaningfully more out-of-the-box than tmux's
nearly-blank bar.

Given tymux's explicit goal of being *easier to adopt* than tmux (not merely
equivalent), and that it's introducing a **brand-new, non-tmux-compatible
key-binding system** (see below) that users can't lean on existing muscle
memory for, **zellij's contextual keybinding-hint model is a strong
precedent worth leaning toward** rather than tmux's blank-by-default
approach: a first-run tymux user has no `.tmux.conf`-equivalent yet and no
inherited muscle memory, so a status bar that also teaches the active
key-bindings live is disproportionately more valuable here than it is for
tmux (whose users already largely know the bindings before they ever look at
the status bar).

---

## 5. Config / key-bindings — minimal set vs. tmux's full table

### tmux's actual default table is large; the useful subset is small

All default bindings live behind a single prefix key (`Ctrl-b`). tmux's own
man page / `list-keys -T prefix` output amounts to roughly **80+ distinct
default bindings** covering window management, pane management, resizing,
layout selection, buffers, and miscellaneous utility commands (clock,
message display, command prompt). Representative defaults: `d` detach, `c`
new-window, `%` split vertically (side-by-side), `"` split horizontally
(stacked), arrow keys / `o` pane navigation, `x` kill-pane (with a
confirmation prompt), `&` kill-window (confirmation), `[` enter copy-mode,
`z` toggle zoom, `M-1`..`M-5` preset layouts, `Space` cycle layouts.

### Key tables — the mechanism behind "one prefix key," worth copying even if the bindings differ

tmux's binding system is scoped through **key tables**, not a single flat
map: `root` (fires with no prefix at all — used sparingly, e.g. mouse
bindings or a user's own `-n` rebinds), `prefix` (everything behind
`Ctrl-b`), and `copy-mode`/`copy-mode-vi` (active only while inside
copy-mode, selected by the `mode-keys` option). `bind-key -T <table> <key>
<command>` scopes a binding to one of these; `bind-key -n <key> <command>` is
shorthand for `-T root` (fire immediately, no prefix). This table-scoped
design is the actual reason tmux can safely reuse the same physical key for
different meanings depending on mode (e.g. `h`/`j`/`k`/`l` mean nothing in
`root`/`prefix` but mean cursor movement in `copy-mode-vi`) without
collisions — tymux's own interceptor (however it implements the
prefix-vs-modal choice below) should adopt the same "one active table at a
time, keys resolved against whichever table is current" shape rather than a
single global keymap, since copy-mode and normal-mode genuinely need
disjoint bindings for the same keys.

### Minimal v1 subset (~8-10 bindings) that actually unlocks the requirements-doc floor

Given `requirements.md`'s explicit floor is just "at minimum a working
detach key sequence," plus the new splits epic needing its own minimal
bindings, a realistic **minimal useful set**, grouped by lifecycle:

- **Session lifecycle**: detach (1 binding) — the literal, named
  requirement
- **Window management**: new-window, next/prev-window (2-3 bindings)
- **Pane management**: split-horizontal, split-vertical, switch-pane
  (directional or cycle), kill-pane (3-4 bindings)
- **Copy-mode entry**: enter scroll/copy mode (1 binding — the other
  researcher's copy-mode findings cover what happens once inside)

That's **8-10 bindings total** against tmux's ~80+ — a deliberately tiny
fraction, and worth stating plainly in planning: v1 does not need to, and
should not try to, replicate tmux's full binding surface. It needs exactly
enough to make the newly-shipped features (splits, detach, copy-mode) usable
at all.

### The real architecture implication: this requires new local input-interception, not just a config value

Because today's CLI is pure byte passthrough (every keystroke, including
Ctrl-C/Ctrl-D, forwarded straight to the remote pty — this is the exact
mechanism behind the journey-map's "no detach" and Ctrl-D hang findings), any
key-binding system at all requires the CLI to start **locally intercepting
and parsing raw input** before deciding whether to forward it — genuine new
local-terminal-handling logic, not a server-side config value read once at
startup. This is the same underlying gap copy-mode's local-navigation needs,
so — as `requirements.md`'s Rabbit Holes section already suggests — these two
epics share one real piece of new machinery (a local input-mode
interceptor) and should be designed together, not sequenced as fully
independent epics.

Two realistic architectural choices for that interceptor:
- **Prefix-key model** (tmux's own approach): one leader key (default
  `Ctrl-b`) then a second key selects the action. Familiar to anyone with
  tmux muscle memory; simple state machine (armed/not-armed).
- **Modal/hybrid model** (zellij's approach): no prefix key by default —
  direct `Ctrl+<letter>` combinations switch into named modes (Pane mode,
  Tab mode, Scroll mode, Session mode), each of which has its own active key
  set (shown live in the status bar per the finding above); zellij also
  offers an explicit "Locked"/"Unlock First" variant requiring a `Ctrl-g`
  prefix specifically to avoid colliding with keys other terminal
  applications or terminal emulators already use.

Given tymux has zero existing muscle-memory obligations (it's a fresh binary,
not asking existing tmux users to relearn anything file-format-compatible —
`requirements.md` already rejects tmux.conf compatibility as unnecessary
scope), there's no forced default toward the prefix-key model out of
compatibility concerns — it's a genuinely open choice, and zellij's
"reduces collisions with the outer terminal/apps" rationale for offering a
lockable prefix mode is a real, concrete argument worth weighing against
prefix-key's lower implementation complexity and higher familiarity to the
likely early-adopter audience (people evaluating tymux as "a tmux
alternative," per `requirements.md`'s own Users/Consumers section).

### Config file format: no reason to invent a bespoke DSL

`requirements.md` already rules full tmux.conf-syntax compatibility out of
scope ("tymux's config format does not need to be tmux-compatible, just
functional"). Modern precedent backs a plain structured-format choice
instead of inventing a new bespoke DSL the way tmux's own `.tmux.conf`
(`bind-key`, `set-option`, ad hoc grammar) does: zellij uses **KDL** (a
purpose-built config language — see `kdl.org`) for both its config and its
layout files; wezterm uses **Lua** (`wezterm.lua`), trading a bigger
learning curve for full scripting power. For tymux's stated minimal
key-binding needs, a plain, boring, well-supported format (TOML, or KDL if a
Rust crate is readily available) is very likely sufficient and is
lower-risk than either inventing a new DSL or reaching for a full
scripting language — worth stating as a planning-phase leaning, not a final
decision.

---

## 6. Unstated needs — what an agent-driven client needs that a human-only design misses

tymux's stated audience explicitly includes AI coding agents and future web
frontends driving sessions over gRPC directly, not just an interactive human
at a terminal (README, Users/Consumers section). Several v1 items have a
real "human affordance" and a parallel, different "agent affordance" that
`requirements.md` doesn't fully spell out:

- **Structured layout introspection, once splits exist.** The current
  proto's `Pane` message (`proto/tymux/v1/tymux.proto`) has only
  `id`/`rows`/`cols` — no x/y offset, no parent/split-tree relationship.
  That's adequate for "one pane per session" but **will not be sufficient
  the moment splits land**: an agent needs to know "pane A is to the left of
  pane B, and they're both inside a horizontal split that's the right child
  of the window's root vertical split" via a typed RPC response — not by
  rendering a status bar or a visual layout and screenshotting/parsing it
  (which would be a bizarre regression from tymux's own stated
  differentiator, `CapturePane`'s whole point being "no re-parsing needed").
  Whatever tree representation gets chosen for splits (section 1 above)
  needs to be exposed on the wire, not just kept as internal engine state —
  this is a concrete new proto-design requirement the splits epic should
  explicitly account for, not an incidental detail.
- **Liveness/exit signaling** — already the journey-map's cross-cutting gap
  #1 (no `Session`/`Pane`/`PaneSnapshot`/`AttachEvent` field says "is this
  alive," which is precisely why a polling agent can't distinguish a dead
  pane's frozen last frame from an idle live one). This research doesn't
  change that finding, just reinforces it: it also directly determines
  the shape of the persistence Tier-0 contract above (a relaunched-but-not-
  yet-relaunched pane after a daemon restart needs to report as "dead" via
  this same field, not a bespoke persistence-only status).
- **Direct scrollback/search access for non-interactive callers.** Humans
  navigate scrollback via copy-mode's interactive keys (arrows, visual
  select). An agent needs the *same underlying data* (the pane's history
  buffer) exposed as a direct request/response RPC — e.g. "return the last N
  lines as structured cells" or "search history for a pattern, return
  matching line ranges" — rather than being forced to simulate keypresses
  through the copy-mode UI to extract text a human would just read off
  screen. This is a real, distinct design implication for the scrollback
  epic: **one shared history buffer, two access paths** (interactive
  keys for the CLI, a direct RPC for programmatic clients) — not one
  keystroke-mediated path serving both.
- **No agent-facing status bar concept — but the same underlying state,
  yes.** A rendered status bar is meaningless to an agent (it's pixels/cells
  for a human to read), but the *data* it shows a human (window list, active
  pane, attach count, liveness) is exactly the kind of thing a
  programmatic client also wants — ideally via the same typed RPCs
  informing the human-facing status bar, not a human-only side channel. This
  argues for designing "what data backs the status bar" as a first-class,
  RPC-exposed data model regardless of whether a human is watching, rather
  than treating the status bar as a CLI-rendering-only concern.
- **Key-bindings are explicitly a human/CLI-only concern — and that's fine,
  provided every action has a real RPC underneath.** An agent doesn't send
  raw keystrokes through a local prefix-key interceptor; it calls typed RPCs
  directly. The important design principle this implies: **every
  human key-binding action (split, kill-pane, switch-pane, detach) should
  correspond to a real, directly-callable RPC** — not a CLI-only keystroke
  shortcut with no RPC equivalent (the same pattern the journey-map already
  flagged as a live problem for `KillSession`, which is fully implemented
  server-side but has no CLI surface at all — the same gap in the opposite
  direction: don't let split/pane-management CLI conveniences exist without
  the agent-facing RPC underneath them).
- **Other unstated agent needs worth flagging for planning, not fully
  researched here**: batched/atomic multi-step operations (e.g.
  "create session, split N times, run N commands" as one call instead of N
  round trips — meaningfully reduces latency and partial-failure surface for
  a scripted agent setting up a complex layout) and structured error
  codes/idempotency for retries over a connection that may be flakier for a
  remote/scripted client than for a human typing at a local terminal (ties
  to the journey-map's already-flagged cross-cutting gap #2, the raw
  `anyhow` Debug-dump problem, which is a human-UX issue but has a
  parallel programmatic-client-UX cost too: an agent parsing errors also
  needs structured codes, not a Debug string).
- **A concrete, real-world precedent that reinforces this whole
  section**: `tmux-assistant-resurrect` (section 2 above) exists specifically
  because AI coding-assistant sessions have session-resumption needs a
  generic shell-relaunch doesn't satisfy — independent validation, from
  outside this project, that "agent-driven terminal sessions have real,
  distinct persistence/state needs beyond a human's shell history" is not a
  hypothetical concern unique to tymux's framing.

---

## Sources

- tmux(1) man page — `man7.org/linux/man-pages/man1/tmux.1.html`
- tmux Window Layouts — `mintlify.com/tmux/tmux/advanced/layouts`, `github.com/tmux/tmux/wiki/Getting-Started`
- tmux layout string / checksum breakdown — `sleepwalker-hnd.blogspot.com/2016/07/tmux-layout-checksum.html`, `tmux-users.narkive.com/tsSrK1JY/reverse-engineering-layout-format`
- tmux Formats wiki — `github.com/tmux/tmux/wiki/Formats`
- tmux window-size option — `tmuxai.dev/tmux-window-size/`
- tmux copy-mode guides — `terminal.guide/tools/multiplexer/tmux/copy-mode-guide/`, `dev.to/iggredible/the-easy-way-to-copy-text-in-tmux-319g`, `waylonwalker.com/tmux-copy-mode/`
- tmux clipboard / OSC 52 — `github.com/tmux/tmux/wiki/Clipboard`, `mil.ad/blog/2024/remote-clipboard.html`, `sunaku.github.io/tmux-yank-osc52.html`
- tmux history-limit — `tmuxai.dev/tmux-increase-scrollback/`
- tmux default key bindings — `cs.smu.ca/~porter/csc/341/notes/tmuxDefaultKeyBindings.html`, `baeldung.com/linux/tmux-keys`
- tmux-resurrect README/docs — `github.com/tmux-plugins/tmux-resurrect`
- tmux-continuum README — `github.com/tmux-plugins/tmux-continuum`
- tmux-assistant-resurrect README — `github.com/timvw/tmux-assistant-resurrect`
- zellij layouts / KDL — `zellij.dev/documentation/creating-a-layout.html`, `zellij.dev/documentation/layouts.html`
- zellij modes / keybindings philosophy — `zellij.dev/documentation/keybindings-modes.html`, `vadosware.io/post/from-zellij-to-tmux-back-to-zellij/`
- wezterm split API — `wezterm.org/config/lua/keyassignment/SplitPane.html`, `wezterm.org/config/lua/pane/split.html`
- Project files read directly: `project_plans/v1-release/requirements.md`, `README.md`, `docs/ux/journey-map.md`, `proto/tymux/v1/tymux.proto`, `docs/adr/0001-single-pane-per-session-for-now.md`
