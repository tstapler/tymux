# Adversarial Review: bearer-token-auth (blocker re-check)

**Date**: 2026-08-28
**Verdict**: CLEAN

## Blockers
(none — both prior blockers confirmed resolved)

- [x] **Blocker 1 (clap `env` feature not enabled)** — RESOLVED. Task 2.1.1a
  (`plan.md:893-901`) changes the workspace `Cargo.toml`'s `clap` dependency
  from `features = ["derive"]` to `features = ["derive", "env"]`, and appears
  *before* Task 2.1.1b (`plan.md:903-916`), which is the first task to add
  `#[arg(..., env = "TYMUXD_TOKEN", hide_env_values = true)]`. Verified against
  the actual repo state: root `Cargo.toml:28` currently reads `clap = {
  version = "4", features = ["derive"] }`, and `crates/tymux-cli/Cargo.toml:16`
  inherits `clap` via `{ workspace = true }` — so the workspace-level edit is
  both necessary and sufficient; no other `Cargo.toml` needs touching. Cross-
  checked against the pinned `clap_builder-4.6.6` source (local registry
  cache): `Arg::env` (`src/builder/arg.rs:2205`) and `Arg::hide_env_values`
  (`src/builder/arg.rs:2667`) are both `#[cfg(feature = "env")]`-gated —
  without the feature, `.env(...)` doesn't exist and the derive macro's
  generated call fails to compile, exactly as Blocker 1 originally described.
  Task 2.1.1a's single feature-flag addition fixes both the `env=` and the
  `hide_env_values` compile paths at once.

- [x] **Blocker 2 (`--help` leaking live `TYMUXD_TOKEN` value)** — RESOLVED.
  Task 2.1.1b (`plan.md:903-916`) puts `hide_env_values = true` directly on
  the field attribute: `#[arg(long, global = true, env = "TYMUXD_TOKEN",
  hide_env_values = true)] token: Option<String>`. Task 2.1.1d
  (`plan.md:925-933`) adds a genuinely testable AC:
  `cli_help_does_not_echo_configured_token_value` sets `TYMUXD_TOKEN` via
  `std::env::set_var`, calls `Cli::command().render_help()`, and asserts the
  rendered text doesn't contain the live value. Verified this is a real,
  correct clap 4.6.6 API, not a hallucinated one: `CommandFactory::command()
  -> Command` (`clap_builder-4.6.6/src/derive.rs:120`) and
  `Command::render_help(&mut self) -> StyledStr`
  (`clap_builder-4.6.6/src/builder/command.rs:1004`), and `StyledStr`
  implements `Display` (`src/builder/styled_str.rs:213`), so
  `.to_string().contains(...)` (or an equivalent `Display`-based assertion)
  works as described. Traced the actual leak/fix mechanism in
  `help_template.rs:756-783`: the live env value is captured via
  `env::var_os(&name)` inside `Arg::env()` at `Command`-build time
  (`src/builder/arg.rs:2205-2211`) — so a test that sets the env var *before*
  calling `Cli::command()` correctly captures it — and is only interpolated
  into the rendered `[env: NAME=value]` string when
  `!a.is_hide_env_values_set()` (`help_template.rs:770`); with
  `hide_env_values = true` set, the value is omitted entirely, confirming the
  fix actually suppresses the leak, not just adds an assertion that happens
  to pass for unrelated reasons.

## Concerns
(none within scope of this re-check)

## Minors
- None found specific to these two items. (Not evaluated: whether
  `cargo build -p tymux-cli` has actually been run against this plan's
  code — the plan is pre-implementation, so this is expected, not a gap.)

## New-problem check (per task brief)
- **Task ordering**: correct — the feature-flag task (2.1.1a) precedes the
  field-definition task (2.1.1b) that consumes it.
- **`BearerToken` newtype integration with the `env`/`hide_env_values` fix**:
  clean. The plan does *not* attempt to have clap's derive macro parse
  directly into `Option<BearerToken>`. `Cli.token` stays `Option<String>`
  (Task 2.1.1b, `plan.md:911`) — clap only ever sees a plain `String`, which
  it already knows how to parse/env-fallback/hide without a custom
  `value_parser`. The newtype is applied only at the one construction
  boundary, Task 2.1.2b (`plan.md:1019-1038`): `cli.token.as_deref()
  .and_then(BearerToken::parse)` when building `BearerAuth`. This sidesteps
  the real constraint correctly — `BearerToken` has no `FromStr`/`Clone`+
  `ValueParserFactory` wiring, so clap could not have parsed into it directly
  without additional (unplanned) machinery, and the plan doesn't ask it to.
- No other new compile or logic problem found in the fixed sections.

## Note
This is a scoped re-review of the 2 previously-BLOCKED items only (clap env
feature; hide_env_values leak). The prior round's 5 CONCERNS and minors were
addressed by the same repair pass per the fix subagent's summary but are not
re-verified here — see git history / the fix subagent's own report for that
detail.
