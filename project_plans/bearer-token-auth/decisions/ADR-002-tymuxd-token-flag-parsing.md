# ADR-002: Hand-roll `tymuxd`'s `--token` flag instead of adding `clap`

**Status**: Accepted
**Date**: 2026-08-28

## Context

`requirements.md` asks for `tymuxd` to accept its bearer token via
`--token` CLI flag *or* `TYMUXD_TOKEN` env var. `tymuxd` has zero CLI-flag
parsing today (`crates/tymuxd/Cargo.toml` has no `clap` dependency;
`main.rs` reads all runtime config via bare `std::env::var(...)` —
`TYMUXD_ADDR` at `crates/tymuxd/src/main.rs:1227`,
`TYMUXD_DISCONNECT_REGRESSION_WINDOW_MS`/`TYMUXD_GRACE_PERIOD_MS` at
`main.rs:93-100` — confirmed by direct read, `research/stack.md` §1,
`research/architecture.md` §4, `research/features.md` §3a).

Two research agents disagreed on the resolution:

- `research/stack.md` §1 leans toward adding `clap` to `tymuxd`, framing it
  as "no new crate enters the lockfile, just a new `Cargo.toml` line"
  since `clap` is already a workspace dependency (used by `tymux-cli`).
- `research/architecture.md` §4's final recommendation leans the other
  way: "a minimal manual `std::env::args()` scan consistent with this
  binary's current zero-CLI-parsing style... is the better fit unless a
  second flag is anticipated soon."

## Decision

Hand-roll `--token <value>`/`--token=<value>` parsing in `tymuxd` via a
small `resolve_token(&[String]) -> Option<BearerToken>` function
operating on `std::env::args().collect::<Vec<_>>()`, falling back to
`TYMUXD_TOKEN` if the flag isn't present. No new dependency added to
`crates/tymuxd/Cargo.toml` for this.

**Amended during Phase 3 planning** (adversarial review, architecture
review): two refinements beyond the original decision, neither changing
the "hand-roll, don't add clap" conclusion:
- The parser now accepts both the space-separated (`--token value`) and
  `=`-joined (`--token=value`) forms. The original draft only handled
  the space-separated form; `clap` (used one crate over, in
  `tymux-cli`) supports both for free, and an operator typing
  `tymuxd --token=xxx` getting a silent, confusing fallthrough to
  "no token configured" was flagged as an untested footgun worth the
  ~5 extra minutes to close outright rather than merely document.
- `resolve_token` returns `Option<BearerToken>`, not `Option<String>`.
  `BearerToken` is a newtype (`crates/tymuxd/src/auth.rs`, no `Debug`/
  `PartialEq`/`Eq` derive, `BearerToken::parse(&str) -> Option<Self>` as
  the only constructor) added during architecture review to make "empty
  token" unrepresentable downstream rather than relying on the single
  `.filter(|t| !t.is_empty())` call inside `resolve_token` being the
  *only* enforcement point — see `implementation/plan.md`'s Pattern
  Decisions table.

## Alternative Rejected: add `clap` to `tymuxd`

**Reason**: `tymuxd` has deliberately stayed dependency-light and
flag-free — every existing knob (`TYMUXD_ADDR` included) is env-var-only,
with no precedent of CLI-flag parsing anywhere in the binary. This is one
optional string flag with one fallback env var; the entire feature clap
would add value for here — declarative `env = "..."` precedence — is
reproducible in about three lines of hand-rolled code (see
`resolve_token` in `implementation/plan.md` Story 1.1.2). Adding `clap`
purely for this one flag would:
- Pull in a real dependency (and its own transitive deps, `clap_builder`,
  `clap_derive`, etc.) into a binary that has stayed minimal by design.
- Introduce `--help`'s env-var-value-echo footgun noted in
  `research/pitfalls.md` §1 (`hide_env_values` must be set explicitly, or
  `tymuxd --help` echoes the *current* `TYMUXD_TOKEN` value to
  stdout/terminal scrollback) — a new failure mode this feature's own NFR
  ("must never appear... at any level") specifically warns against. A
  hand-rolled parser has no `--help`-generation step to get this wrong in.
- Set precedent for `tymuxd` to grow a `clap` `Cli` struct that isn't
  otherwise needed yet; `architecture.md`'s framing — reach for `clap`
  "unless a second flag is anticipated soon" — is correct, and no second
  flag is in scope here.

This decision applies to `tymuxd` only. `tymux-cli` (the client,
`crates/tymux-cli/src/main.rs`) already depends on `clap` with an
existing `#[derive(Parser)] struct Cli` (`main.rs:180-192`); adding
`#[arg(long, env = "TYMUXD_TOKEN")] token: Option<String>` there (Story
2.1.1) is a trivial, uncontroversial addition to existing infrastructure
— not a new-dependency decision, and not affected by this ADR.

## Consequences

- `crates/tymuxd/Cargo.toml` gains no `clap` dependency.
- `resolve_token` must independently implement "explicit flag beats env
  var" precedence (clap's `env = "..."` attribute gives this for free;
  hand-rolled code must replicate it deliberately and test it) and
  "empty string counts as absent" (`research/pitfalls.md` §5's sharpest
  named edge case) — both covered by named tests in
  `implementation/plan.md` Story 1.1.2.
- If `tymuxd` later grows a second CLI flag, this decision should be
  revisited — the manual-parsing style stops being the more consistent
  choice once there are two or more flags to keep in sync by hand.
