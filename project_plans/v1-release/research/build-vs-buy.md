# Build vs. Buy — v1.0 Gap Items

Research for Phase 2 (SDD) of the v1-release plan. Evaluates, for each
major in-scope item in `requirements.md`, whether to adopt an existing
crate/tool or build bespoke. Grounded in the README's stated
differentiator: **the structured gRPC API is the product**, not
feature-parity with tmux. That framing biases every verdict below toward
"spend build effort on the layers unique to tymux (engine indexing, proto
surface, structured capture); buy/borrow everything else that's a solved
problem."

---

## 1. Splits / layout engine

**Options considered:**
- `binary-space-partition` (crates.io, kvark) — abstract BSP tree
- `zellij-utils` — Zellij's own utility crate, published standalone, includes `input::layout::Layout`
- Bespoke binary-tree split model in `tymux-core`

**binary-space-partition**
- Pros: does exactly the abstract-tree shape (recursive split nodes) tmux/zellij use.
- Cons: last published version is 0.1.2, **updated 2017-08-02** — effectively unmaintained (381K lifetime downloads, all historical, from its use in `gfx-rs`/`webrender`-adjacent crates for geometry splitting, not terminal layout). It's generic over a `PlaneSplit` trait for CSG/rendering use cases, not panes — would need a nontrivial adapter layer and inherits no terminal-specific logic (resize semantics, min-size, pane addressing) anyway. Adopting a 9-year-stale geometry crate as a load-bearing dependency for a project's core data structure is worse than owning ~150 lines of tree code outright.
- Verdict: **Not recommended.**

**zellij-utils**
- Pros: `Layout` struct (`direction`, `parts: Vec<Layout>`, `split_size`, `run`, `borderless`) is a real, battle-tested tree-split model, actively maintained (0.44.3, updated 2026-05-13), used in production by Zellij itself.
- Cons: it's not a standalone layout-only crate — it's Zellij's general utility crate (config loading, CLI arg parsing, KDL parsing, session-layout-file semantics, plugin permissions, etc.), pulled in as a whole dependency graph. Adopting it means either (a) depending on a large, evolving crate whose public API is designed for Zellij's own daemon/client split, not tymux's gRPC-first model, or (b) copying just the `Layout`/split-resize logic out — which is effectively "read zellij's source as a reference implementation," not "adopt a library."
- The pane-tree model itself (recursive `Direction`/`parts`/percentage-or-fixed `split_size`) is small enough (a tagged tree + a resize-propagation function) that reading zellij's implementation as prior art and writing a tymux-specific version against `tymux-core`'s existing `Pane`/`SessionState` types is more tractable than wedging in an external crate's ownership model.
- Verdict: **Viable as a design reference, not as a dependency.** Recommendation: build the tree in `tymux-core`, informed directly by zellij's `zellij-utils::input::layout::Layout` shape (same recursive `Direction { Horizontal, Vertical }` + `parts: Vec<Node>` + split-size representation) rather than reinventing the shape from first principles. This gets most of the correctness benefit of prior art (see item 8) without the dependency-fit cost.

---

## 2. Persistence

**Options considered:** `redb`, `rusqlite` (SQLite), `sled`, hand-rolled `serde_json`/`serde` + atomic file write.

| Crate | Latest | Last updated | Notes |
|---|---|---|---|
| `redb` | 4.1.0 | 2026-04-19 | Pure-Rust embedded KV store, actively maintained, stable file format |
| `rusqlite` | 0.40.1 | 2026-06-06 | SQLite bindings, extremely mature/battle-tested |
| `sled` | 0.34.7 | 2024-10-11 | Still labeled beta; development resumed after a gap but cadence is slower |

- Scale reality check per requirements.md: "single-daemon, personal/small-team scale... not designed for high concurrent session counts." The durability contract under discussion (per Rabbit Holes) is realistically "session/window/pane metadata + config survives a restart," explicitly **not** full scrollback replay or live-process resume (acknowledged as likely infeasible without CRIU). That's a handful of small structs (session list, window/pane tree, working directory, maybe last-known-size), written infrequently (on session/window/pane create/destroy/resize, not per-keystroke).
- Pros of a DB (redb/rusqlite): transactional writes, crash-safety guarantees, schema evolution tooling, no hand-rolled atomic-write logic.
- Cons of a DB: a new dependency + failure mode (corruption/lock contention/schema migration) for a workload that's "serialize one struct, write it out, occasionally." Adds attack surface and a new thing to get wrong (e.g. redb's format stability commitment is good but still a new subsystem for a personal side-project daemon). SQLite in particular is the heaviest of the three for this problem — its relational/query features are pure overhead when there's no querying, only "load whole state on startup, save whole state on change."
- Hand-rolled: `serde` (already a transitive dep via `prost`) + `serde_json` or `toml`, write to a temp file + atomic rename (`std::fs::rename` same-filesystem is atomic on both Linux and macOS), on a debounced timer or on each mutating RPC. This is genuinely simple for "small struct, single writer (the daemon), infrequent updates."
- Verdict: **Hand-rolled serde+file persistence recommended; DB dependency not justified at this scale.** This is the clearest over-engineering risk in the list — the "durability contract" framing in requirements.md itself (metadata survives, live output doesn't) fits a flat-file snapshot model, not a database. Revisit only if/when session counts or write frequency grow enough that atomic-rename-on-save becomes a measured bottleneck (unlikely for a personal daemon). If a DB is wanted later purely for its crash-safety story rather than query needs, `redb` (pure Rust, no C dependency, stable format) is the better fit of the three over `rusqlite`/`sled`.

---

## 3. Scrollback / copy-mode

**Existing dependency:** `vt100 = "0.15"` (Cargo.toml pins 0.15; **current published version is 0.16.2**, note for planning — a minor-version bump is available).

Confirmed by reading `crates/tymux-core/src/pane.rs`:
```rust
/// vt100's third `Parser::new` arg: how many scrolled-off lines it keeps
/// for scrollback. 0 for now — `CapturePane` only ever reads the current
/// on-screen grid...
const SCROLLBACK_LINES: usize = 0;
...
let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, SCROLLBACK_LINES)));
```

- `vt100::Parser::new(rows, cols, scrollback_len)` **already supports scrollback natively** — the third constructor argument is exactly this, and it's currently hardcoded to `0` by deliberate choice (per the comment: nothing consumed it yet), not a library limitation.
- `vt100::Screen` supports scrollback navigation via a scroll-offset API — `screen.set_scrollback(offset)` and cell/content reads (e.g. `screen.cell(row, col)`, `contents_formatted()`) are offset-aware, returning the view as scrolled. This is precisely the primitive copy-mode navigation needs (move a view offset into history, read cells at that offset).
- Pros: zero new dependency. This is a "flip a constant and wire up the RPC/CLI surface" job, not a "find a new terminal-emulation crate" job. `contents_formatted()` also directly supports a `CapturePane`-style structured/re-playable dump of a scrollback slice, consistent with the project's "structured, not scraped" positioning.
- Cons: none material — the crate already does what's needed. The nontrivial work is elsewhere: sizing the buffer (memory cost per pane × scrollback lines × session count), exposing scrollback offset through the proto (`CapturePane` needs an offset/range parameter it doesn't have today), and the CLI-side copy-mode UX (key interception for navigate/select/yank), which is genuinely new code regardless of the underlying crate.
- Verdict: **Recommended: use existing `vt100` capability, no new crate.** Bump `vt100` to `0.16.2` and change `SCROLLBACK_LINES` from `0` to a real bound (make it configurable) as part of this epic; do not evaluate alternate terminal-emulation crates — there's no gap to fill.

---

## 4. Status bar rendering

**Options considered:** `ratatui` (full TUI framework), raw ANSI escape-sequence composition, a narrower "reserve a status line" crate.

- `ratatui` (0.30.2, actively maintained, huge ecosystem: widgets, buffers, diffed rendering) is the standard answer for "build a Rust TUI." But tymux-cli's current architecture (per README/status: "thin client... writes bytes straight to stdout," a **pure PTY passthrough**) is fundamentally different from what ratatui assumes: ratatui owns the whole screen and drives an immediate-mode redraw loop against its own buffer. tymux-cli's actual problem is the opposite — the *server-side child process* owns and drives arbitrary ANSI output into a region of the screen that the *client* doesn't control, and the CLI must reserve one line without corrupting that stream. That's "coexist with a foreign PTY's output," not "own the whole render loop," which is what ratatui is built for.
- Pulling in ratatui to draw one reserved line means either (a) fighting ratatui's ownership-of-the-terminal model to coexist with the raw pty passthrough (a real integration cost, since ratatui expects to be the sole thing drawing), or (b) using ratatui only for the status line and hand-rolling the passthrough scroll-region math anyway — at which point ratatui adds weight without removing the hard part.
- The actual well-understood technique for "reserve N lines at the bottom, let a subprocess own the rest" is DECSTBM (`ESC[<top>;<bottom>r`, the same primitive tmux itself uses for its own status bar) to set a scrolling region excluding the bottom line, plus manual cursor save/restore (`ESC[s`/`ESC[u` or `CUP`) around drawing the status line. This is ~30-50 lines of raw escape sequences, already representable with the project's existing `crossterm` dependency (which exposes cursor positioning/style primitives) — no new crate needed for the actual bytes.
- No dedicated "reserve a status line" crate turned up in search that's a good fit (this is normally solved either by a full TUI framework taking over the screen, or hand-rolled DECSTBM, with nothing lightweight and popular in between for Rust specifically).
- Verdict: **Raw ANSI/DECSTBM approach recommended over ratatui, using the already-present `crossterm` dependency for cursor primitives.** Pulling in ratatui here would be the "LLM reaches for the big framework" failure mode the README's own positioning argues against — it solves a different problem (owning the whole screen) than the one tymux-cli actually has (reserving a line around a passthrough it doesn't own). Revisit only if a future scope expansion needs tymux-cli to render richer chrome (multi-pane borders, an actual interactive copy-mode overlay) beyond a single status line — copy-mode navigation specifically may independently justify a small amount of raw-mode UI code, at which point re-evaluate, but don't front-load ratatui for the status bar alone.

---

## 5. Config / key-bindings

**Options considered:** `toml` + `serde`, `serde_yaml`, `figment`, `config`, dedicated keybinding crates (`keybinds`, `keyboard-types`), hand-rolled parser.

| Crate | Latest | Last updated | Fit |
|---|---|---|---|
| `toml` | 1.1.2+spec-1.1.0 | 2026-04-01 | Standard TOML parser, matches `Cargo.toml`'s own format for consistency |
| `serde` | 1.0.228 | 2025-09-27 | Already a transitive dependency (via `prost`/`tonic`); zero marginal cost to use directly |
| `serde_yaml` | 0.9.34 (**deprecated**, last updated 2024-03) | — | Explicitly deprecated upstream — do not adopt |
| `figment` | 0.10.19 | 2024-05 | Layered config (env+file+defaults) — more power than needed for a single local config file |
| `config` | 0.15.25 | 2026-06-26 | Same category as figment, multi-source layering — overkill for one file |
| `keybinds` | 0.2.0 | 2025-05 | Purpose-built keybinding dispatcher/parser; very low adoption (12.9K downloads), young (0.2.0) |
| `keyboard-types` | 0.8.3 | 2025-10 | Widely used (25M downloads) shared key-event type definitions (used by e.g. `winit`, `muda`) — a types crate, not a parser |

- This is the clearest "use existing library" case in the whole list, as requirements.md's own framing anticipated. `toml` + `serde` (via `#[derive(Deserialize)]` on a `TymuxConfig` struct) is the idiomatic, boring, correct choice — matches the Rust ecosystem's dominant convention for CLI-tool config (used by cargo itself, rustfmt, etc.), and both crates are already effectively "free" (serde is a transitive dep already; toml is a single new dependency of a very stable, extremely widely-used crate).
- `figment`/`config` are for apps needing config layered across multiple sources (env vars + multiple files + CLI overrides merged with precedence) — tymux's config is one file at one location; that layering isn't a stated requirement, so those crates would be solving a problem tymux doesn't have.
- For key-binding *syntax* specifically (mapping a string like `"C-b d"` or `"detach"` to an action, matching a keystroke sequence against bound sequences): no need for a dedicated crate. Key-binding config is naturally represented as `HashMap<String, Action>` deserialized straight out of TOML by serde (e.g. `[keybindings]\ndetach = "C-b d"`), with a small hand-rolled sequence-matcher (parse `"C-b d"` into a `Vec<KeyEvent>`, compare against captured input) — this is maybe 50-100 lines, well within "worth owning" territory, especially since `crossterm` (already a dependency) already provides the `KeyEvent`/`KeyCode`/`KeyModifiers` types needed to represent a captured keystroke, making a separate `keyboard-types` dependency redundant. The `keybinds` crate is too new/low-adoption (0.2.0, ~13K downloads) to trust for something this central, and doesn't obviously save meaningful code over hand-rolling given crossterm's types are already present.
- Verdict: **Recommended: `toml` (^1) + `serde` (^1) for config file parsing/schema; hand-rolled key-sequence matching against `crossterm`'s existing `KeyEvent` types for keybindings** — no dedicated keybinding crate. Do not adopt `serde_yaml` (deprecated) or `figment`/`config` (solving unneeded multi-source layering).

---

## 6. Cross-language client (TypeScript)

**Options considered:** `@connectrpc/connect` + `@bufbuild/protoc-gen-es` (buf-generated), hand-written raw gRPC-web/`@grpc/grpc-js` client.

- Connect-ES 2.0 (GA) simplified this specifically: previously two plugins were needed (`protoc-gen-es` + `protoc-gen-connect-es`); as of 2.0 a single `protoc-gen-es` invocation generates both the message types and the Connect client/server code. This directly resolves the "buf.gen.yaml currently has zero plugins configured" gap flagged in requirements.md's Rabbit Holes with a known-working, current toolchain rather than an unproven one.
- Bidirectional streaming (needed for `Attach`) is a first-class supported RPC type in the Connect protocol/connect-es — not a bolted-on afterthought — but it requires HTTP/2 specifically (unary/server-streaming can fall back to HTTP/1.1, bidi cannot). tymuxd currently serves via `tonic` (HTTP/2-native, gRPC), so this should line up, but it's the one integration point requiring explicit verification in Phase 4/5 (validate `Attach` bidi streaming end-to-end from a real generated TS client against tonic — the exact discovery risk requirements.md already calls out), not assumed to work purely from documentation.
- Hand-writing a raw `@grpc/grpc-js` or grpc-web client would mean reimplementing message (de)serialization and stream framing by hand against the existing `.proto` — throwing away buf's entire codegen value proposition and directly contradicting the README's stated differentiator ("One proto schema, buf-managed — add a TS/Python/Go client without touching the Rust core"). This would also be strictly more implementation work for a *worse* outcome (hand-maintained, drift-prone client code) than the generated path.
- Verdict: **Recommended: buf-generated `@connectrpc/connect` client via `protoc-gen-es` (Connect-ES 2.0).** This is production-ready for the stated use case and is the only choice consistent with the project's own value proposition — the only real open item is empirically validating the bidirectional-streaming `Attach` path specifically (flagged correctly as a discovery risk in requirements.md), which is a validation task, not a reason to pick a different toolchain.

---

## 7. Release automation

**Options considered:** `taiki-e/upload-rust-binary-action` (+ optionally `taiki-e/create-gh-release-action`), `cargo-dist`, `cross-rs/cross`, fully hand-rolled release workflow.

- `taiki-e/upload-rust-binary-action`: focused GitHub Action — builds and uploads a named binary to a GitHub Release for a given target triple; designed to be combined with a matrix strategy (one job per OS/arch) and `create-gh-release-action` for the release object itself. Actively maintained, widely used, minimal footprint — it's a build-and-upload step you drop into an existing/new CI workflow, not a new tool that owns the whole pipeline.
- `cargo-dist`: a full release-pipeline generator (plan → build → host → publish → announce), generates its own `release.yml`, adds installer generation (shell/powershell installers, `npm` wrapper, Homebrew formula generation), changelogs, etc. Actively maintained (0.32.0, 2026-05), and a legitimate choice — but it's meaningfully more machinery than requirements.md actually asks for ("a tagged v1.0.0 GitHub release exists with prebuilt binaries for macOS and Linux, installable without a Rust toolchain"). Adopting cargo-dist means also adopting its opinions about installer scripts, its own generated workflow file (harder to hand-tune), and a steeper initial learning curve for a solo/side-project pace.
- `cross-rs/cross`: solves a different sub-problem — Docker-based cross-*compilation* toolchains for targets your host can't natively build (e.g. building `aarch64-unknown-linux-gnu` from an `x86_64` CI runner without native cross-toolchain setup). This matters if the release matrix includes Linux ARM64 built from an x86_64 GitHub-hosted runner; GitHub Actions now has native `macos-14`/`ubuntu-24.04-arm` runners for Apple Silicon and Linux ARM64 respectively, which reduces (but doesn't fully eliminate, e.g. musl/glibc variants) the need for `cross`'s Docker cross-toolchains specifically for the requirements.md matrix (macOS + Linux x86_64/arm64).
- Hand-rolled from scratch (raw `cargo build --release --target ...` steps + manual `actions/upload-release-asset`): fully avoidable busywork — this exact problem (matrix build → archive → attach to a GH release) is thoroughly solved and this project has zero need to reinvent it, especially given "No CI currently runs for real" is already flagged as a prerequisite gap to close.
- Verdict: **Recommended: `taiki-e/upload-rust-binary-action` + `taiki-e/create-gh-release-action`**, using GitHub's native ARM64/Apple-Silicon runners for the two architectures per OS rather than `cross`'s Docker cross-toolchain path (simpler CI, no Docker-in-CI overhead, sufficient for the stated macOS+Linux x86_64/arm64 matrix). **`cargo-dist` is Viable** as a later upgrade if the project wants installer scripts/Homebrew formula generation post-1.0, but is more than requirements.md's stated bar needs right now — start minimal, most consistent with the "sequence work so master stays buildable/shippable at every step" risk-control framing.

---

## 8. LLM-generated vs. battle-tested: the layout tree specifically

This item is the one place in the whole v1.0 gap where "just write it" carries real, non-obvious correctness risk, and deserves explicit call-out beyond item 1's crate survey.

**Why this is genuinely hard, not "add more panes to a Vec" (ADR 0001's own words):**
- A tmux-style split layout is a recursive tree (alternating horizontal/vertical split directions, arbitrary nesting depth), not a flat grid — the data structure itself has edge cases (a node with one child after a pane close — does it collapse? a resize that pushes a child below a minimum size — does it clamp, redistribute, or refuse?) that are easy to get subtly wrong on a first pass and easy to *believe* are correct because the common 2-3 pane cases work fine while a deeper nested case silently misbehaves.
- Resize propagation specifically is the sharp edge: resizing one pane in a tree must proportionally adjust siblings/cousins along the correct axis without violating other panes' constraints, recursively. This is exactly the kind of "looks obviously right, has a subtle off-by-one or wrong-axis bug three levels deep" code that benefits enormously from either (a) a property-based/fuzz test suite asserting invariants (total size always equals parent size, no pane below min-size, tree stays balanced after any sequence of split/close operations) or (b) copying the shape of an implementation that's already absorbed years of real-world bug reports.
- Two real-world Rust prior-art implementations exist and are directly inspectable: **zellij's** `zellij-utils::input::layout::Layout` (tree of `Direction`/`parts`/`split_size`, actively maintained, has resolved years of resize/collapse edge cases via its own issue tracker) and, as a non-Rust but even more battle-tested reference, **tmux's own C `layout.c`** (the actual algorithm this project is explicitly modeled on, per the README's framing: "tmux's model, rebuilt with a typed API"). Both are read-only references, not dependencies — see item 1 for why importing `zellij-utils` wholesale is impractical, but reading its resize/split logic (and tmux's) before writing tymux's own version is low-cost and directly de-risks the sharp edges above.
- Verdict on the meta-question: **write bespoke code in `tymux-core`, but treat it as adapted-from-prior-art, not written from a blank page.** Concretely for Phase 3/4 planning: (a) budget explicit design time for the tree shape and resize-propagation algorithm referencing zellij's `layout.rs` and/or tmux's `layout.c` rather than deriving it fresh; (b) require property-based tests (invariant: child sizes always sum to parent size; no pane ever below a minimum row/col count; every split/close/resize sequence leaves the tree in a valid state) as a hard gate in `validation.md`, not just example-based unit tests, specifically because this is the one place in the v1.0 scope where a plausible-looking implementation can hide a real bug; (c) do not let this be the first thing implemented by an agent without a human/reviewer pass specifically on the resize-propagation function, given it's flagged as the highest correctness-risk item in the entire epic.

---

## Summary table

| Item | Verdict | Recommendation |
|---|---|---|
| 1. Splits/layout engine | Viable as reference, build bespoke | Bespoke tree in `tymux-core`, shaped after `zellij-utils::Layout`; skip `binary-space-partition` (stale since 2017) |
| 2. Persistence | Hand-rolled recommended | `serde` + `toml`/`serde_json` + atomic file rename; DB (redb/rusqlite/sled) is over-engineering at this scale |
| 3. Scrollback/copy-mode | Recommended: use existing dep | `vt100`'s `Parser::new(_, _, scrollback_len)` already supports this; bump 0.15→0.16.2, raise `SCROLLBACK_LINES` from 0 |
| 4. Status bar | Recommended: raw ANSI | DECSTBM scroll-region + `crossterm` cursor primitives; ratatui is the wrong shape for a passthrough architecture |
| 5. Config/key-bindings | Recommended: existing libs | `toml` (1.1.2) + `serde` (1.0.228) for config; hand-rolled sequence matcher against `crossterm`'s `KeyEvent` for bindings |
| 6. TS client | Recommended: buf-generated | `@connectrpc/connect` + `protoc-gen-es` (Connect-ES 2.0); validate bidi `Attach` streaming explicitly |
| 7. Release automation | Recommended: focused action | `taiki-e/upload-rust-binary-action` + `create-gh-release-action` on native runners; `cargo-dist` viable later, not needed now |
| 8. Layout tree correctness | Bespoke, but adapt from prior art | Reference zellij's `layout.rs`/tmux's `layout.c`; require property-based invariant tests as a validation gate, not just unit tests |
