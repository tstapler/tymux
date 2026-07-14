# UX Design — tymux v1.0

**Phase**: Design pass, feeding Phase 4 (`validation.md`) and Phase 5 implementation
**Input**: `project_plans/v1-release/requirements.md`, `implementation/plan.md`, `research/ux.md`, `docs/ux/journey-map.md`, ADR-001/002/003/004/005/006
**Scope**: the new user-facing surfaces introduced by v1.0 — splits, copy-mode, status bar, config loading, session persistence restore, and the CLI error/friendly-message layer as it extends to all of the above. Does not re-litigate `research/ux.md`'s comparative analysis or `journey-map.md`'s MVP findings — it builds concrete wireframes, flows, and acceptance criteria on top of both.

This document assumes ADR-005's binding table, ADR-004's smallest-wins geometry policy, ADR-002's Tier-0-only persistence contract, and ADR-001's binary-split `LayoutNode`, and designs the user-visible surface consistent with all four.

---

## 0. Conventions carried forward (do not re-derive, just apply)

- **Prefix key**: default `Ctrl-b`, tmux-vocabulary verbs (ADR-005). All examples below use the default.
- **Friendly-message layer**: every new error funnels through `crates/tymux-cli/src/main.rs`'s `friendly_message()` (journey-map's fix for cross-cutting gap #2) — never a raw `anyhow`/`tonic::Status` Debug dump.
- **Liveness is always signaled with color + symbol + text**, never color alone (`research/ux.md` §3). Symbols used consistently across this document:
  - `●` live + attached (green)
  - `○` live + detached (green/dim)
  - `✖` dead / restored, not running (red)
- **Every mode has an unconditional, always-visible exit key** (`Escape` or `q`) — no mode-specific variant, no dead end (`research/ux.md` §3, journey-map Flow 3).
- **`NO_COLOR`** disables all status-bar/chrome coloring; **`--no-status-bar`** disables the chrome entirely and returns to pure passthrough (the accessibility floor named in `research/ux.md` §3).

---

## 1. Surface: Splits — creation, navigation, addressing

**New CLI surface**: `tymux split <target> [--vertical|--horizontal]`, `tymux kill-pane <target>`, `TargetString` addressing (`session:window.pane`, e.g. `myproject:0.1`).
**New keybindings** (prefix-armed, per ADR-005): `%` split horizontal (side-by-side), `"` split vertical (stacked), `o` cycle pane focus, `c` new window, `x` kill active pane, `n`/`p` next/prev window.

### 1.1 Wireframe — layout tree to rendered result

```
tymux split myproject:0.0 --horizontal
                                                 window 0 (myproject)
   before                                        LayoutNode::Leaf{0}
   ┌──────────────────────────────┐              becomes
   │                               │              LayoutNode::Split{H, [(Leaf 0,0.5),(Leaf 1,0.5)]}
   │   pane 0 (bash)               │
   │                               │
   └──────────────────────────────┘

   CLI's rendering model for v1: FOCUSED-PANE, full width/height.
   Only one pane's live output occupies the terminal at a time;
   `prefix o` cycles focus among the window's panes.

   ┌──────────────────────────────┐   prefix o    ┌──────────────────────────────┐
   │  ● pane 0.0 (focused)         │  ──────────>  │  ● pane 0.1 (focused)         │
   │  bash                         │               │  bash                         │
   │                               │               │                               │
   ├───────────────────────────────┤               ├───────────────────────────────┤
   │ myproject [0:0.0*|0.1] ●LIVE  │               │ myproject [0:0.0|0.1*] ●LIVE  │
   └──────────────────────────────┘               └──────────────────────────────┘
```

**Important design note**: v1.0's plan does not implement true tmux-style simultaneous composited rendering (multiple live pty regions visible at once with borders in a single terminal) — see Cross-Check finding #1. This document designs to the **focused-pane, cycle-to-switch** model, which is what the stored tasks (single-owner stdout writer forwarding one pty stream, client-side pane-focus cycling with no RPC) actually support. The status bar's window/pane summary is the substitute for a visual mini-map — see §4.

### 1.2 Interaction flow

**Path A — explicit CLI subcommand** (validated first per Story 3.5, useful for scripts/agents too):
1. User runs `tymux split myproject:0.0 --horizontal`
2. CLI resolves `TargetString` → `pane_id`, calls `SplitPane` RPC
3. Daemon validates minimum size, mutates `LayoutNode`, returns updated `Session`
4. CLI prints: `Split created: myproject window 0 now has 2 panes (0.0, 0.1).`

**Path B — keybinding, while attached**:
1. User presses `Ctrl-b` → `PrefixState::Armed`, status bar switches to hint mode (§4.2)
2. User presses `%` → `Action::SplitHorizontal` fires, CLI calls `SplitPane` on the focused pane
3. On success: focus moves to the new pane (matches tmux's own default), status bar's window summary updates to show `[0.0|0.1*]`
4. On failure (undersized terminal): see §7.1 — layout is untouched, focus stays put, no split occurs

**Navigating between panes once split**: `prefix o` cycles focus round-robin through the window's leaves in tree order; `prefix n`/`prefix p` cycle windows. Both are local-only (no RPC), matching Story 5.3 task 3.

### 1.3 Error / edge cases
- **Undersized terminal** — see §7.1 (shared error catalog).
- **Kill the focused pane, siblings remain**: `prefix x` on pane 0.1 closes it, `LayoutNode::remove` collapses the parent `Split` back to a single `Leaf{0.0}` (Story 3.2 AC2), focus returns to 0.0 automatically. CLI prints `Pane 0.1 closed. myproject window 0 now has 1 pane.`
- **Kill the last pane in a window**: window becomes empty (`RemoveOutcome::WindowEmpty`) — CLI must decide whether the window itself closes or the session ends if it was the last window. **Not specified in plan.md — flagged in Cross-Check #9.**

---

## 2. Surface: Detach

**Keybinding**: `prefix d` (ADR-005, `requirements.md`'s literal v1.0 success metric).

### 2.1 Flow

```
Attached, interacting  ──Ctrl-b──>  PrefixState::Armed (status bar shows hints, 500ms-ish timeout)
                                          │
                                          d
                                          ▼
                              Action::Detach fires locally
                                          │
                          gRPC Attach call fully CANCELLED
                          (not half-closed — ADR/Story 2.3 contract)
                                          │
                                          ▼
                    terminal restored (raw mode off, DECSTBM region cleared)
                    CLI prints: "detached (myproject:0.0 still running)"
                    process exits 0, returns to outer shell
```

### 2.2 Interaction detail
- Escape hatch: `prefix prefix` (pressing the leader twice) sends the literal leader byte through to the pane instead of arming a binding — required so shells/editors that also bind `Ctrl-b` inside the pane still work (ADR-005 consequence).
- Detach must be indistinguishable in *reliability* from a crash-free path: `tymux ls` immediately afterward must show the session `● live, detached` (journey-map Flow 3's core ask).
- **Do not reuse the `[tymux: pane exited]` message text for detach** — Story 5.3 task 1 already calls this out explicitly; detach needs its own distinct confirmation line, not the exited-pane line at `main.rs:201`.

### 2.3 Error/edge cases
- Detach while `PrefixState` armed but before second key resolves (user presses `Ctrl-b` then waits) — the arm times out back to `Idle` silently (mirrors tmux's `escape-time`); no visible error, status bar simply reverts to normal-mode rendering.
- Detach keybinding pressed while already in copy-mode: **not addressed by any story** — does copy-mode need its own `prefix d`, or must the user exit copy-mode (`q`/`Escape`) first? Flagged in Cross-Check #10.

---

## 3. Surface: Copy-mode

**Entry**: `prefix [`. **Exit**: `q` or `Escape`, unconditionally, same path whether the pane is live or dead (`research/ux.md` §4, Story 5.5 AC3).

### 3.1 Flow diagram

```
Normal mode (live pty forwarding)
        │  prefix [
        ▼
┌─────────────────────────────────────────────────────────────┐
│ COPY-MODE   line 3812/4022 (of scrollback)                   │  ← status bar switches (§4.3)
├─────────────────────────────────────────────────────────────┤
│  ...scrollback content rendered via CapturePane(offset)...   │
│  ...cursor highlight at current navigation position...       │
└─────────────────────────────────────────────────────────────┘
        │ hjkl / arrows          │ /            │ v            │ q or Escape
        ▼ move cursor,           ▼ forward       ▼ start        ▼
        re-CapturePane at        search prompt   char-range     exit copy-mode,
        new offset               (SearchScrollback  selection    return to Normal
                                  RPC)                │
                                                        │ move cursor (extends
                                                        │ selection), then y
                                                        ▼
                                              copy selected range into
                                              local buffer, exit copy-mode
                                                        │
                                              (later) paste keystroke
                                                        ▼
                                              buffer forwarded as
                                              AttachRequest::Input
```

### 3.2 Dead-pane state within copy-mode

```
┌─────────────────────────────────────────────────────────────┐
│ COPY-MODE  ✖ [exited]   line 3812/4022                       │
├─────────────────────────────────────────────────────────────┤
│  ...scrollback content, exactly as for a live pane...        │
└─────────────────────────────────────────────────────────────┘
        │ q or Escape  (IDENTICAL exit path — no special-cased variant)
        ▼
   Normal mode / prompt returns
```

- Entering copy-mode on a pane whose process has already exited is **not an error** — it works over whatever scrollback exists (Story 5.5 AC3, matching tmux's own behavior).
- The only visible difference is the `✖ [exited]` marker in the copy-mode status segment — same color+symbol convention as `tymux ls` (§6).
- **Do not** disable `y`/copy in this state — copying scrollback from a dead pane is a legitimate, common use case (grabbing output before the process died).

### 3.3 Search flow

```
COPY-MODE  line 3812/4022                                        (before)
        │ /
        ▼
COPY-MODE  /search: _                                             (prompt replaces status segment)
        │ type "error", Enter
        ▼
COPY-MODE  line 2001/4022  match 1 of 1 found >  ("no matches" if none)
```

- Forward-search, next-match-only for v1 (Story 5.4 task 4 — no regex, no backward search yet). This limitation should be stated plainly in `tymux --help`/docs, not discovered by trial.

### 3.4 Interaction summary table

| Step | Keys | System response |
|---|---|---|
| Enter | `prefix [` | Status bar → COPY-MODE, screen freezes at current scrollback offset |
| Navigate | `h j k l` / arrows | `ScrollbackOffset` moves, `CapturePane` re-fetches at new offset |
| Search | `/` then query then `Enter` | Jumps to next match via `SearchScrollback`; "no matches" shown inline if none |
| Select | `v` then move | Highlights char-range between anchor and cursor |
| Copy | `y` (or `Enter`) | Selected range → local buffer, copy-mode exits |
| Paste | (in Normal mode) paste keybinding | Buffer contents forwarded as pane input |
| Exit (no copy) | `q` / `Escape` | Returns to Normal mode, no buffer change |

---

## 4. Surface: Status bar — three rendering states

Reserves the terminal's last row via `DECSTBM` (`\x1b[1;{rows-1}r`); pane geometry is always `rows - 1` while the bar is enabled (Story 6.2). `--no-status-bar` / `NO_COLOR` degrade this per §0.

### 4.1 Normal mode

```
┌──────────────────────────────────────────────────────────────────────────┐
│ (pane output — rows-1 rows tall)                                          │
├──────────────────────────────────────────────────────────────────────────┤
│ myproject  [0:0.0*|0.1] [1:vim]   ● LIVE   1 client            12:04 PM  │
└──────────────────────────────────────────────────────────────────────────┘
```
Fields (from `StatusBarModel`, gRPC-introspectable per Story 6.1 — not client-only chrome): session name, per-window pane summary (`*` = focused pane), active pane liveness (color+symbol+text), attached client count, clock.

### 4.2 Prefix-armed mode

```
├──────────────────────────────────────────────────────────────────────────┤
│ PREFIX   d:detach  %:split-h  ":split-v  o:cycle  c:new-win  x:kill  [:copy │
└──────────────────────────────────────────────────────────────────────────┘
```
Replaces the normal-mode content entirely with the live binding table (zellij-style discoverability win, `research/ux.md` §1) for the duration `PrefixState == Armed`, then reverts to §4.1 automatically on timeout or on the second keypress resolving.

### 4.3 Copy-mode

```
├──────────────────────────────────────────────────────────────────────────┤
│ COPY-MODE  ✖ [exited]  line 3812/4022   hjkl:move v:select y:copy /:find │
└──────────────────────────────────────────────────────────────────────────┘
```
Own key-set hint (Story 6.4 AC2), liveness marker only appears when the pane is dead — otherwise that segment is simply omitted (not rendered as `● LIVE`, to avoid visual noise for the common case).

### 4.4 Plain mode (`--no-status-bar` or dumb terminal)

No bottom row reserved, no `DECSTBM`, pane gets the full `rows`. Prefix/copy-mode still function (they are input-side state machines, independent of chrome) but produce **no visual mode indicator at all** — this is the accessibility floor (`research/ux.md` §3), not a fully-equivalent experience. This tradeoff should be one documented line in `--help`, not silent.

---

## 5. Surface: Config file loading

**Location**: `~/.config/tymux/config.toml` (via `dirs` crate). **Format**: TOML, tymux-vocabulary keys (not tmux.conf syntax, per `requirements.md`'s explicit scope cut).

### 5.1 Three cases

**Absent** (first-run / no customization):
```
$ tymux attach myproject
(no config-related output — defaults silently apply: leader=Ctrl-b, ADR-005's table)
```
No message is shown for the common case — a config file is optional, not expected, so silence here is correct (surfacing "no config found, using defaults" on every single run would be noise, not signal).

**Valid, with overrides**:
```toml
# ~/.config/tymux/config.toml
leader = "C-a"

[keybindings]
detach = "C-a d"
```
```
$ tymux attach myproject
(loads silently; Ctrl-a is now armed by prefix, Ctrl-b does nothing special)
```

**Malformed** (recommended behavior — see Cross-Check #7, plan.md does not commit to this explicitly):
```
$ tymux attach myproject
tymux: couldn't parse ~/.config/tymux/config.toml (line 4: invalid TOML) —
       using default keybindings for this session. Fix the file and restart
       to apply your config.
(session proceeds normally with hardcoded defaults, NOT a fatal error)
```
This mirrors the graceful-degradation convention Epic 4 already established for corrupt persisted-session files (log + skip, never fatal to daemon boot) — a malformed *client-side* config file should follow the identical philosophy: never block the user from using tymux at all over a typo in one optional file.

### 5.2 Error/edge cases
- A config referencing an unknown action name (typo in `[keybindings]`) should be a `tracing::warn!`/startup-message naming the specific bad key, not a blanket "config invalid" — actionable per the same "state the constraint, not just that one was violated" principle already applied to the undersized-split message.
- A config binding that collides with the bracketed-paste sentinel bytes or another binding: reject at load time with the specific colliding sequence named, not a silent last-wins overwrite.

---

## 6. Surface: Session persistence restore — `tymux ls` and `tymux revive`

### 6.1 `tymux ls` with mixed live/dead sessions

```
$ tymux ls
NAME        STATUS                        WINDOWS  CREATED
myproject   ● attached (1 client)         2        2026-07-10 09:14
scratch     ○ detached, live              1        2026-07-12 18:02
oldwork     ✖ restored — not running      3        2026-07-08 11:40
```

- `●` / `○` / `✖` pair with the word (`attached`/`detached, live`/`restored — not running`) — never color-only (§0).
- `oldwork`'s row is the direct consumer of Epic 2's `Liveness` field and Epic 4's dead-flagged load — it must never render identically to a live session (`research/ux.md` §4, ADR-002 consequence).

### 6.2 `tymux revive` flow

```
$ tymux revive oldwork
Reviving 'oldwork': 3 panes across 2 windows, layout preserved...
  window 0, pane 0  (bash @ ~/proj)       → respawned (new process)
  window 0, pane 1  (vim @ ~/proj/src)    → respawned (new process)
  window 1, pane 0  (bash @ ~)            → respawned (new process)

Session revived: 3 pane(s) respawned with their original command and
working directory. These are NEW processes — scrollback from before the
restart is not carried forward.

Attach with: tymux attach oldwork
```
```
$ tymux ls
NAME       STATUS                  WINDOWS  CREATED
oldwork    ○ detached, live        2        2026-07-08 11:40
```

**Note on message wording** — plan.md's Story 4.4 task 3 proposes printing *"Session metadata restored. The process itself is not resumed until you revive it..."* as `revive`'s own confirmation text. That sentence is written from the *pre-revive* vantage point (it describes what restore-without-revive means) and is self-contradictory if shown *after* a successful revive, since the process **has** just been resumed (as a new process). The wording above ("Session revived: N pane(s) respawned... these are NEW processes...") is this document's corrected version — see Cross-Check #2.

### 6.3 Error / edge cases

**Attaching to a dead/restored session without reviving first** (`research/ux.md` §4 explicitly calls for this to either offer-to-respawn or fail-fast-with-explanation; no plan.md story currently implements either — see Cross-Check #3):
```
$ tymux attach oldwork
tymux: pane 0.0 in session 'oldwork' has exited (restored from disk, not
       running). Run `tymux revive oldwork` to respawn it, or `tymux ls`
       to see all sessions. Note: reviving starts NEW processes — nothing
       is live-resumed.
```
Recommended behavior: **fail fast with this message** (option (b) from `research/ux.md` §4) rather than silently attaching into a pty that does nothing — silent attach-to-nothing is exactly the Ctrl-D-hang failure shape the journey map already flagged as the worst possible outcome.

**Reviving an already-live session** (edge case named in this design task's brief; no plan.md story addresses it — Cross-Check #8):
```
$ tymux revive myproject
tymux: 'myproject' is already live (1 client attached) — nothing to revive.
       Use `tymux attach myproject` instead.
```
Recommended: no-op with a friendly explanation, not an error exit code — the user's underlying intent ("get me into this session") is still satisfiable, so the message should point at the right command rather than just refusing.

**Reviving a session that doesn't exist / bad ID**: standard `not_found` through `friendly_message` — `tymux: no such session: oldwrok (did you mean 'oldwork'?)` is a nice-to-have (fuzzy match), not required for v1.

**Corrupted persisted file at daemon startup**: invisible to the CLI user directly — surfaces only as that one session being absent from `tymux ls` (daemon logs `tracing::warn!` per Story 4.3, per-file, never fatal to daemon boot). Worth one line in `tymux ls --help` or docs noting "a session missing from this list that you expect may indicate a corrupted state file — check `tymuxd` logs."

---

## 7. Surface: CLI error states (undersized split, dead-pane operations)

Collected here as the cross-cutting catalog requested in Step 1 — each of these also appears inline above where contextually relevant.

### 7.1 Undersized split

```
$ tymux split myproject:0.0 --horizontal
tymux: can't split — pane is 15 rows, minimum for a horizontal split is
       ~20 rows. Resize your terminal or close another pane first.
```
- States the actual numbers, not just "too small" (`research/ux.md` §4's actionable-error principle, already reflected verbatim in Story 3.5 AC2).
- Checked daemon-side (authoritative, protects programmatic/agent callers) and should *also* be pre-checked CLI-side for instant feedback before a round trip — **not currently a scheduled CLI-side task; see Cross-Check #6.**
- Layout is left completely untouched on rejection — no degenerate partial split, no corrupted rendering.

### 7.2 Dead-pane operations

| Operation on a dead pane | Behavior | Message |
|---|---|---|
| `tymux attach <dead session>` | Fail fast (recommended, §6.3) | `pane exited (restored from disk, not running). Run \`tymux revive <id>\` to respawn it.` |
| `tymux split <dead pane target>` | Reject — can't split a pane with no live pty | `can't split: pane 0.0 has exited. Revive the session first (\`tymux revive <id>\`).` |
| Copy-mode entry on a dead pane | **Allowed**, not an error (§3.2) | `[exited]` marker shown inline, same exit key as live case |
| `tymux kill-pane <dead pane target>` | Allowed — removes the dead leaf from the persisted layout too | `Pane 0.0 removed from myproject's saved layout.` |
| `CapturePane` on a dead pane (programmatic) | Returns last-known snapshot + `Liveness::LIVENESS_DEAD` — not an error | (structured, no human message — this is the AI-agent path) |

Every row above must produce a stable `tonic::Status` code + structured detail server-side, and a friendly-message translation client-side (Observability Plan §4's "structured error convention," already a stated cross-cutting requirement in plan.md) — this table is the concrete enumeration that requirement was missing.

---

## 8. UX Acceptance Criteria

Each is independently, humanly testable.

### Splits
- **UX-AC-01**: User can create a horizontal split and see the new pane focused in ≤ 2 keystrokes while attached (`prefix` + `%`), or 1 command line (`tymux split <target> --horizontal`).
- **UX-AC-02**: User can cycle focus between all panes in a window in exactly 1 keystroke per pane (`prefix o`), returning to the original pane after N presses for N panes.
- **UX-AC-03**: Attempting a split in a terminal below the minimum size shows the exact current/required row count and a specific remediation ("resize... or close another pane"), and leaves the existing layout completely unchanged (verify via `tymux ls`/`CapturePane` before and after — identical).
- **UX-AC-04**: Closing a pane that has a sibling collapses the split automatically and moves focus to the surviving pane — no orphaned empty split node, no manual cleanup step required.

### Detach
- **UX-AC-05**: User can detach in exactly 2 keystrokes (`prefix d`) from any Normal-mode state, and the terminal is fully restored (no raw-mode artifacts, prompt returns) within one render frame.
- **UX-AC-06**: Immediately after detach, `tymux ls` shows the session as live/detached (`○`), never absent and never showing stale `attached` status.
- **UX-AC-07**: The detach confirmation message is textually distinct from the "pane exited" message — a user must never have to infer which one occurred from context alone.

### Copy-mode
- **UX-AC-08**: User can enter copy-mode (`prefix [`), navigate with `hjkl`/arrows, and exit (`q` or `Escape`) with **zero effect on the live pane's state** — re-entering copy-mode after exiting shows the same live content as if copy-mode were never entered.
- **UX-AC-09**: The exit key (`q`/`Escape`) works identically — same keys, same immediate effect — whether the pane is live or has exited. No dead end exists in copy-mode under any pane liveness state.
- **UX-AC-10**: User can select and copy text in ≤ 4 discrete actions (enter copy-mode, `v`, move to extend selection, `y`) and paste it into the live pane with 1 additional keystroke.
- **UX-AC-11**: A dead pane's copy-mode view shows a `[exited]` marker (or equivalent) within 1 render frame of entry — a user is never left wondering whether they're looking at a live or dead pane's scrollback.

### Status bar
- **UX-AC-12**: While `prefix` is armed, the status bar shows the complete current binding table (all ~8-10 bindings) — a user should never need external documentation to discover an available action mid-session.
- **UX-AC-13**: The status bar transitions between Normal/Prefix-armed/Copy-mode states within one redraw cycle of the triggering keystroke — no stale hint text lingers after a mode change.
- **UX-AC-14**: `--no-status-bar` and `NO_COLOR` both produce zero ANSI color codes and zero `DECSTBM` scroll-region escapes in the output stream (verifiable by piping to a file and inspecting bytes).
- **UX-AC-15**: No status-bar segment communicates state via color alone — every liveness/mode indicator pairs a symbol or word with any color used (verify by rendering with `NO_COLOR=1` and confirming no information is lost).

### Config
- **UX-AC-16**: With no config file present, tymux starts successfully with zero warnings or errors printed.
- **UX-AC-17**: A single overridden binding in `config.toml` changes only that binding — every other default remains active (verify by testing an unrelated default binding still works).
- **UX-AC-18**: A malformed config file does not prevent `tymux` from starting — it starts with defaults and shows one specific, actionable line naming the file and the parse problem.

### Session persistence
- **UX-AC-19**: `tymux ls` visually distinguishes all three states (`attached`, `detached, live`, `restored — not running`) using both a distinct symbol and distinct wording per state — no two states ever render identically.
- **UX-AC-20**: User can revive a dead session and confirm it's live again in exactly 2 commands (`tymux ls` to find it, `tymux revive <id>`), with the revive command's own output confirming pane count and explicitly stating these are new processes.
- **UX-AC-21**: Attaching directly to a dead/restored session without reviving first never hangs or shows a blank/stale screen — it fails immediately with a message naming the exact remediation command (`tymux revive <id>`).
- **UX-AC-22**: Reviving an already-live session does not error destructively (no crash, no duplicate panes) — it responds with a clear "already live" message pointing at `tymux attach`.
- **UX-AC-23**: A corrupted persisted-session file never prevents `tymuxd` from starting or blocks any other valid session from loading (verify: 1 corrupt + N valid files present → daemon starts, `tymux ls` shows exactly N sessions).

### General / cross-cutting
- **UX-AC-24**: No mode introduced by v1.0 (prefix-armed, copy-mode) has a dead end — every mode has at least one always-working, documented exit key reachable without external help.
- **UX-AC-25**: Every new v1.0 error case (undersized split, dead-pane attach/split, malformed config, revive-already-live) produces a message via the `friendly_message` layer, never a raw `anyhow`/`tonic::Status` Debug dump.
- **UX-AC-26**: Every new error message that names a constraint (size, liveness) includes the actual numbers/state, not just "invalid" or "failed."

---

## 9. Accessibility Acceptance Criteria

- **UX-A11Y-01**: The entire v1.0 feature set (splits, copy-mode, revive, config) is operable via keyboard alone — no feature has a mouse-only or GUI-only path (trivially true for a CLI, stated explicitly as a regression guard).
- **UX-A11Y-02**: `NO_COLOR=1` suppresses all ANSI color codes from the status bar and any new chrome, while all liveness/mode information remains present as text/symbols.
- **UX-A11Y-03**: `--no-status-bar` suppresses `DECSTBM` scroll-region reservation entirely, returning to pure linear passthrough — verify no partial-screen redraw logic executes at all when this flag is set (not just "invisible," actually inert).
- **UX-A11Y-04**: No status-bar or copy-mode redraw ever overwrites bytes the pane's own child process already emitted — output remains append-only from the child program's perspective (protects screen readers and terminal-recording tools per `research/ux.md` §3).
- **UX-A11Y-05**: Every introduced mode is escapable using only `Escape` and/or `q` — no mode requires memorizing a mode-specific exit sequence beyond these two universal keys.
- **UX-A11Y-06**: Documentation includes an explicit "Accessibility" section stating what is and is not supported (keyboard-only operation, `NO_COLOR`, `--no-status-bar` supported; screen-reader-aware split-pane navigation explicitly out of scope) — silence is treated as a failure condition, not neutral.

---

## 10. Plan Cross-Check — UX-incompleteness found in `implementation/plan.md`

1. **[Major] The multi-pane rendering model is never explicitly decided.** The Domain Glossary's `WatchWindow` entry describes a client "composit[ing] them into a single rendered view with correct borders/positions" (implying true tmux-style simultaneous multi-region rendering), but no story in Epic 3, 5, or 6 implements this — Epic 6's Story 6.3 explicitly designs a **single-owner stdout writer forwarding one pty stream at a time**, and Epic 5's `prefix o` is described as client-side "pane-focus cycling," which only makes sense under a focused-pane-at-a-time model. These two framings (composited-view vs. focus-cycling) produce materially different UX and different implementation cost (the former is a substantial new terminal-rendering subsystem; the latter is comparatively cheap). This document designs to the focus-cycling interpretation (§1.1) as the only one the scheduled tasks actually support, but Phase 4/5 should make this an explicit, named decision rather than leaving it implicit — a reviewer reading only the Domain Glossary would reasonably expect the composited-borders behavior and be surprised when it isn't there.

2. **[Moderate] Story 4.4's proposed revive confirmation message is self-contradictory when read post-revive.** Task 3 quotes "Session metadata restored. The process itself is not resumed until you revive it..." as the CLI output *after* a successful `revive` call — but that sentence's own content describes the state *before* revival (the process is explicitly stated as "not resumed until you revive it," despite revival having just happened). This message text was lifted from `research/ux.md` §4's guidance about the *restore* moment (daemon startup / `tymux ls` discovery), not the *revive* moment. §6.2 above proposes corrected wording ("Session revived: N pane(s) respawned... these are NEW processes...").

3. **[Moderate] `tymux attach` on a dead/restored session (without a prior `revive`) is unspecified.** `research/ux.md` §4 explicitly asks for either "(a) offer to respawn... or (b) fail fast with that explanation" as a named design requirement — but no story in Epic 3, 4, or 5 implements either branch for the `attach` command specifically. Epic 2 Story 2.4 gives the `Engine` a `PaneLookup::Dead` distinction and a `failed_precondition` status code exists at the RPC layer, but nothing wires that into `tymux attach`'s CLI-side UX with the recommended actionable message. §6.3 proposes the fail-fast wording as the concrete fix.

4. **[Minor] The dead-pane error message in Story 2.4 lacks the actionable next step.** The example given (`Status::failed_precondition("pane exited")`) doesn't mention `tymux revive <id>` — every other new v1.0 error case in this plan (undersized split, malformed config) is designed with the specific remediation in the message text; this one example text should be held to the same bar (`ux.md`'s own "actionable errors state the constraint" principle, applied consistently).

5. **[Minor] Two different, unreconciled minimum-size thresholds exist for splits.** Story 3.2 AC3's property-based invariant enforces a hard structural floor ("no leaf's computed rect ever falls below a configured minimum, e.g. 2 rows × 10 cols"), while Story 3.5 AC2's human-facing message cites "~20 rows" as the minimum for a horizontal split. These may be intentionally two different tiers (a hard anti-corruption floor vs. a usability threshold) but the plan never states this explicitly or names which constant the CLI message actually reads from — as written, a careful reviewer could reasonably read this as an inconsistency rather than a deliberate two-tier design. Phase 4/5 should name both constants explicitly and document the relationship.

6. **[Minor] `research/ux.md` §4's recommendation for a CLI-side pre-flight size check ("Do this check client-side... for instant feedback, but also enforce it daemon-side") is only half-implemented.** Story 3.5 only "surfaces the daemon's minimum-size rejection... through `friendly_message`" (a round trip), with no corresponding CLI-side pre-check task. Not a blocker for a local-loopback daemon (latency is negligible), but the research doc's own stated rationale (instant feedback) goes unaddressed and should at minimum be a documented, deliberate deferral rather than a silent gap.

7. **[Minor] Config-file malformed-load failure mode (fatal vs. graceful-degrade) is unspecified**, unlike Epic 4's persisted-session corrupt-file handling, which explicitly commits to "never fatal to daemon boot... log and skip." Story 5.1 task 4 only says a malformed TOML "produces a friendly startup error (not a panic)" — this leaves open whether tymux still starts (with defaults) or refuses to start until the config is fixed. §5.1 above recommends the graceful-degrade path for consistency with the established Epic 4 convention, but plan.md does not commit to it.

8. **[Minor] Reviving an already-live session is an unaddressed edge case.** No story's acceptance criteria cover `tymux revive <id>` where `<id>` is currently live — the RPC/CLI behavior (no-op with a message, hard error, or duplicate-spawn bug risk) is undefined. §6.3 above proposes a friendly no-op.

9. **[Minor] Removing the last pane from a window (`RemoveOutcome::WindowEmpty`) has a defined engine-level outcome but no defined user-facing behavior.** Story 3.2 models `WindowEmpty` as a distinct outcome from `Collapsed`, but no story specifies what the CLI shows or does next — does the empty window disappear silently, does the session close if it was the last window, does the user get prompted? §1.3 flags this as open.

10. **[Minor] Interaction between detach and copy-mode is unaddressed.** If a user is in copy-mode and presses `prefix d`, is `Detach` even reachable (since `prefix` may not re-arm while already inside a different mode), or must the user exit copy-mode first? Neither Story 5.2 nor 5.3 states the expected cross-mode behavior.

None of these are blocking for Phase 4 validation planning, but items 1–3 in particular should be resolved as explicit decisions (not silently picked during implementation) given how much of the downstream CLI rendering and error-message design in this document depends on the answer.
