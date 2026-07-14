# ADR-005: Prefix-key binding model (tmux vocabulary), status bar borrows zellij's mode-reactive hints

## Status
Accepted

## Context
tymux's CLI today (`crates/tymux-cli/src/main.rs:169-186`) is a pure byte
passthrough — every stdin byte becomes an `AttachRequest::Input` with zero
local interpretation. `requirements.md`'s Success Metrics require "at
minimum a working detach keybinding," which is impossible under pure
passthrough (`pitfalls.md` §5, `stack.md` §5, `ux.md` §2 all independently
confirm this needs genuinely new local input-interception machinery, not a
config value).

Two real architectural models exist (`features.md` §5, `ux.md` §1):
- **Prefix-key** (tmux's own model): one leader key (`Ctrl-b` by default),
  then a second key selects the action. Simple armed/not-armed state
  machine; matches the muscle memory of tymux's stated primary evaluator
  audience ("interactive terminal users evaluating tymux as a tmux
  alternative").
- **Modal/hybrid** (zellij's model): no prefix by default; direct
  `Ctrl+<letter>` combinations enter named modes (Pane mode, Scroll mode,
  etc.), each with its own status-bar-rendered key set; an optional
  lockable prefix variant exists specifically to reduce collisions with
  keys the terminal/apps inside panes already use.

`ux.md` §1/§2 separately identifies zellij's **status bar as live
keybinding cheat sheet** (content changes per active mode) as the single
most transferable UX insight in the whole research set — it eliminates the
"I forgot the split key" failure mode by deferring memorization to the
moment of need, and is disproportionately valuable for tymux specifically
because it has zero existing muscle memory to lean on (a brand-new,
non-tmux-compatible binding system, per `requirements.md`'s own scope cut
against tmux.conf-syntax compatibility).

## Decision

**Binding model: prefix-key, tmux-vocabulary-compatible verbs, default
leader `Ctrl-b`** (configurable). `requirements.md`'s own success metric
names "a working detach keybinding" as the literal bar, and `ux.md` §2
states plainly: "prefix d = detach... do not pick a different letter
without a strong reason." Minimal v1 binding table (~8-10 bindings, per
`features.md` §5's sizing):

| Sequence | Action | RPC/local |
|---|---|---|
| `prefix d` | Detach | local (stream cancel) |
| `prefix [` | Enter copy-mode | local |
| `prefix %` | Split horizontal (side-by-side) | `SplitPane` RPC |
| `prefix "` | Split vertical (stacked) | `SplitPane` RPC |
| `prefix o` | Cycle pane focus | local (client-side focus, no RPC) |
| `prefix c` | New window | `CreateWindow` RPC |
| `prefix n` / `prefix p` | Next/prev window | local (client-side focus) |
| `prefix x` | Kill active pane | `ClosePane` RPC |
| `prefix prefix` | Send the literal leader byte through to the pane | local (escape hatch) |

**Status bar: mode-reactive, zellij-style, layered on top of the
prefix-key model** — not a full modal replacement of it. While `prefix` is
armed (waiting for the second key), the status bar shows the live binding
table above; while in copy-mode, it shows copy-mode's own key set. This
gets zellij's discoverability win without adopting zellij's no-prefix
default, which would cost more implementation complexity and diverge
further from the target audience's tmux muscle memory than the benefit
justifies for v1.

**Every action bound to a key that has server-side effect must have a
first-class RPC underneath it** (`SplitPane`, `ClosePane`, `CreateWindow`,
etc. — already required by Epic 3's proto surface) — an AI agent never
sends raw keystrokes through the prefix interceptor; it calls the RPC
directly. Purely local actions (detach, copy-mode entry/exit, pane-focus
cycling) have no RPC equivalent by design, since they're client-local
terminal state with nothing for the daemon to authorize.

## Consequences
- Requires a genuinely new keystroke-reassembly layer in `tymux-cli`
  independent of raw `read()` chunk boundaries (`pitfalls.md` §5): multi-
  byte escape sequences can split across reads or coalesce; a naive
  per-`read()`-chunk byte matcher will misfire on both. This is the same
  underlying machinery copy-mode's local navigation needs (Epic 5) —
  designed and built together, not as separate efforts.
- Bracketed paste (`ESC[200~...ESC[201~`) must never be scanned for
  keybinding matches — pasted text containing the leader byte sequence by
  coincidence must not trigger a binding.
- An explicit escape hatch (`prefix prefix`) must exist so a user can send
  the literal leader byte through to a shell/editor inside the pane that
  also binds it.
- This is a real, documented behavior change from today's "100% passthrough"
  README claim and must be called out in the updated README/docs (Epic 8),
  not silently introduced.

## Alternatives considered
- **Modal/zellij-style with no default prefix**: rejected as the *primary*
  binding model — higher implementation complexity (N mode state machines
  instead of one armed/not-armed flag) and a bigger departure from the
  primary evaluator audience's existing tmux muscle memory, for a
  discoverability benefit this ADR captures anyway via the status bar
  without adopting the full modal architecture.
- **Bare single-key bindings (no prefix)**: rejected outright — real,
  immediate collision risk with common shell/editor/readline bindings
  running inside every pane (`pitfalls.md` §5).
- **Adopting the `keybinds` crate**: rejected per `build-vs-buy.md` §5 — low
  adoption (0.2.0, ~13K downloads), and a hand-rolled sequence matcher
  against `crossterm`'s already-present `KeyEvent`/`KeyCode`/`KeyModifiers`
  types is not meaningfully more code for a ~10-binding table.
