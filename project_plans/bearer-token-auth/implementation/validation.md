# Validation Plan: bearer-token-auth

**Date**: 2026-08-28

## Happy Path Scenario

Given `tymuxd` is bound to a non-loopback address (e.g. `0.0.0.0:7419`) with
`TYMUXD_TOKEN` (or `--token`) configured, when a client (`tymux-cli`,
`clients/go`, or `clients/ts`) presents the correct bearer token on a
`TymuxService` call — including the `Attach` bidi stream — then the call
succeeds exactly as it would against an unauthenticated loopback bind, and
the token itself never appears in any log, `tracing` field, or spawned
pane's environment.

## Note on Gap 1 (ux.md) — verified against current plan.md

`design/ux.md`'s "Gaps found" section (Gap 1) says the `--token` field in
Task 2.1.1a has no `///` doc comment pointing operators at
`openssl rand -hex 32`, unlike the `no_status_bar` field next to it. Checked
directly against the plan.md read for this validation pass (not re-asserting
ux.md's claim):

- **Closed, tymux-cli side**: Task 2.1.1b (plan.md lines ~903-916) now gives
  the `token` field exactly this doc comment: `"Generate one with
  `openssl rand -hex 32` if you don't already have one configured on the
  daemon side."` — this renders in `tymux --help` via clap's doc-comment
  convention, closing Gap 1 for `tymux-cli`.
- **Also closed, tymuxd's `resolve_token` doc comment**: Task 1.1.2c's
  `resolve_token` doc comment (plan.md lines ~354-369) adds `"Generate a
  token with `openssl rand -hex 32` if you don't already have one to
  configure."`
- **Now also closed, tymuxd's actual CLI surface**: `tymuxd` still has no
  `--help` (ADR-002: hand-rolled `std::env::args()` scan, deliberately not
  `clap`), but Phase 4 validation's own findings fed back into Task
  1.1.3a's startup-failure error string (Surface 1), which now appends
  `"Generate one with `openssl rand -hex 32` if you don't already have
  one."` to the fail-fast message — the one surface an operator running
  `tymuxd` directly is guaranteed to see. This closes the residual half of
  Gap 1 this validation pass originally found open.

**Net**: Gap 1 is now closed on both sides — `tymux-cli --help` (Task
2.1.1b) and `tymuxd`'s startup-failure text (Task 1.1.3a). Surface 5's UX
acceptance criteria below are scored accordingly (pass for both halves).

## Requirement → Test Mapping

| Requirement | Test File | Test Name | Type | Scenario |
|---|---|---|---|---|
| SM1 / Scope: tymuxd refuses to start non-loopback without a token | `crates/tymuxd/src/auth.rs` | `check_non_loopback_requires_token_returns_ok_when_token_present_on_non_loopback_bind`* | Unit (happy) | non-loopback + `Some(BearerToken)` → `Ok(())` (Task 1.1.3c, AC1) |
| SM1 / Scope: tymuxd refuses to start non-loopback without a token | `crates/tymuxd/src/auth.rs` | `check_non_loopback_requires_token_returns_err_when_non_loopback_and_no_token`* | Unit (error) | non-loopback + `None` → `Err(...)` naming `--token`/`TYMUXD_TOKEN` (Task 1.1.3c, AC2) |
| SM1 / NFR-Security (empty-token edge case) | `crates/tymuxd/src/auth.rs` | `check_non_loopback_requires_token_errs_on_empty_token_via_resolve_token_composition`* | Unit (error) | `resolve_token` on an empty `--token ""` feeds `None` into the gate, which errors identically to no-token (Task 1.1.3c, AC3 — the auth-bypass edge case architecture review flagged) |
| SM1 / SM3 (loopback path through the same gate) | `crates/tymuxd/src/auth.rs` | `check_non_loopback_requires_token_returns_ok_when_loopback_and_no_token`* | Unit (happy) | loopback + `None` → `Ok(())`, unaffected (Task 1.1.3c, AC4) |
| SM1 (end-to-end wiring) | `crates/tymuxd/src/main.rs` | *(regression check, not a new test — see Task 1.2.2e)* | Integration | Full `cargo test -p tymuxd` suite passes unmodified, confirming the gate's `main()` wiring (Task 1.1.3b) doesn't regress loopback startup |
| Scope: token via `--token`/`TYMUXD_TOKEN`, flag beats env | `crates/tymuxd/src/auth.rs` | `resolve_token_prefers_explicit_flag_over_env_var` | Unit (happy) | both sources set → flag wins (Task 1.1.2d) |
| Scope: `--token=value` form supported | `crates/tymuxd/src/auth.rs` | `resolve_token_supports_equals_joined_flag_form` | Unit (happy) | `--token=flagval` parses same as space-separated (Task 1.1.2d) |
| Scope: env-var fallback | `crates/tymuxd/src/auth.rs` | `resolve_token_falls_back_to_env_var_when_no_flag` | Unit (happy) | no flag, `TYMUXD_TOKEN` set → used (Task 1.1.2d) |
| Scope / NFR-Security: empty value treated as absent | `crates/tymuxd/src/auth.rs` | `resolve_token_treats_empty_flag_value_as_absent` | Unit (error) | `--token ""` → `None`, not `Some("")` (Task 1.1.2d) |
| Scope: no source configured | `crates/tymuxd/src/auth.rs` | `resolve_token_returns_none_when_neither_source_present` | Unit (error) | neither flag nor env set → `None` (Task 1.1.2d) |
| SM2 / Scope: interceptor gates every call, `Unauthenticated` on failure | `crates/tymuxd/src/auth.rs` | `bearer_auth_interceptor_accepts_matching_token` | Unit (happy) | correct `Bearer <token>` → `Ok(req)` (Task 1.2.1b) |
| SM2 / Scope: interceptor rejects missing token | `crates/tymuxd/src/auth.rs` | `bearer_auth_interceptor_rejects_missing_token` | Unit (error) | no `authorization` metadata → `Unauthenticated("missing bearer token")`, counter +1 (Task 1.2.1b) |
| SM2 / Scope: interceptor rejects wrong token | `crates/tymuxd/src/auth.rs` | `bearer_auth_interceptor_rejects_wrong_token` | Unit (error) | wrong value → `Unauthenticated("invalid bearer token")`, counter +1 (Task 1.2.1b) |
| SM2 (malformed header, fail-closed) | `crates/tymuxd/src/auth.rs` | `bearer_auth_interceptor_rejects_malformed_authorization_header` | Unit (error) | `"Bearer"` with no trailing value → same as missing-token case (Task 1.2.1b) |
| SM2 / Scope: real daemon rejects unauthenticated unary call | `crates/tymuxd/src/main.rs` | `non_loopback_server_rejects_list_sessions_with_missing_token` | Integration | live non-loopback daemon, `ListSessions` with no token → `Unauthenticated` (Task 1.2.2c) |
| SM2 / Scope: real daemon accepts correct token | `crates/tymuxd/src/main.rs` | `non_loopback_server_accepts_list_sessions_with_correct_token` | Integration | live daemon, correct token → success (Task 1.2.2c) |
| SM2 / Scope: `Attach` bidi stream gated identically, no hang | `crates/tymuxd/src/main.rs` | `non_loopback_server_rejects_attach_stream_with_missing_token_promptly` | Integration | `Attach` with no token fails `Unauthenticated` within a bounded timeout (Task 1.2.2d) |
| NFR-Performance: constant-time compare used, not `==` | `crates/tymuxd/src/auth.rs` | *(code-review AC, no automated test in plan.md)* | Static / code review | Story 1.2.1's last AC: "the code is read" and the compare calls `constant_time_eq::constant_time_eq(...)` — **gap**: plan.md has no benchmark/timing test for the "no measurable per-RPC latency regression" half of this NFR (`grep -n "benchmark\|latency\|criterion" plan.md` returns nothing) |
| NFR-Security: token never printed via `Debug` | `crates/tymuxd/src/auth.rs` | `bearer_token_debug_always_prints_redacted` | Unit | `{:?}` on any `BearerToken` → exactly `"<redacted>"` (Task 1.1.2d) |
| NFR-Security: `BearerToken::parse` invariants | `crates/tymuxd/src/auth.rs` | `bearer_token_parse_rejects_empty_string` | Unit (error) | `""` → `None` (Task 1.1.2d) |
| NFR-Security: `BearerToken::parse` invariants | `crates/tymuxd/src/auth.rs` | `bearer_token_parse_accepts_non_empty_string` | Unit (happy) | `"s3cr3t"` → `Some(...)` (Task 1.1.2d) |
| NFR-Security / Observability: peer logged, token never | `crates/tymuxd/src/auth.rs` | `bearer_auth_interceptor_logs_real_peer_address_when_available` | Unit | rejected call with a real `TcpConnectInfo` peer → `warn` record's `peer` field is the real address, not `"unknown"`, and never the token (Task 1.4.1c) |
| NFR-Security: no `{:?}`/`{}` leak of token/`MetadataMap`/`Request` anywhere | `crates/tymuxd/src/auth.rs` | *(manual/grep check, at implementer's discretion — Task 1.2.1c)* | Static / code review | grep for `{:?}`/`{}` formatting of `self.token`, the full `MetadataMap`, or the whole `Request` in the new code |
| Observability: rejection counter counts rejections only | `crates/tymuxd/src/auth.rs` | `bearer_auth_interceptor_rejection_counter_counts_only_rejections` | Unit | 3 rejected + 2 accepted → counter == 3 (Task 1.4.1b) |
| NFR-Security (discovered leak vector, plan Epic 1.3) | `crates/tymux-core/src/pane.rs` | `spawn_should_not_leak_tymuxd_token_into_pane_environment` | Integration | `TYMUXD_TOKEN` set in daemon env → absent from a spawned pane's `env` output; other vars still inherited (Task 1.3.1b) |
| Scope: `tymux-cli --token`/`TYMUXD_TOKEN` flag | `crates/tymux-cli/src/main.rs` | `token_flag_parses` | Unit (happy) | `--token s3cr3t` → `cli.token == Some("s3cr3t")` (Task 2.1.1c) |
| Scope: `tymux-cli` env fallback | `crates/tymux-cli/src/main.rs` | `token_env_var_used_as_fallback` | Unit (happy) | `TYMUXD_TOKEN` alone → used (Task 2.1.1c) |
| Scope: `tymux-cli` flag beats env | `crates/tymux-cli/src/main.rs` | `token_flag_overrides_env_var` | Unit (happy) | both set → flag wins (Task 2.1.1c) |
| NFR-Security: `--help` never echoes live token value | `crates/tymux-cli/src/main.rs` | `cli_help_does_not_echo_configured_token_value` | Unit | `TYMUXD_TOKEN=s3cr3t-live-value` set, rendered `--help` text never contains it (Task 2.1.1d) |
| Scope: `tymux-cli` attaches token to every unary call | `crates/tymux-cli/src/main.rs` | *(unnamed in plan.md — Task 2.1.2c describes but does not name the test)* | Integration | against Story 1.2.2b's harness: `ListSessions` with correct token succeeds, with `None` fails `Unauthenticated` |
| Scope: `tymux-cli` attaches token to `Attach`'s bidi stream | `crates/tymux-cli/src/main.rs` | *(unnamed in plan.md — Task 2.1.2d describes but does not name the test)* | Integration | same harness, `Attach` opens successfully with the correct token configured |
| SM2 / Scope: clear, specific `Unauthenticated` error (not raw `anyhow` dump) | `crates/tymux-cli/src/main.rs` | `friendly_message_names_the_remedy_for_unauthenticated_status` | Unit (error) | `Unauthenticated("missing bearer token")` → `"tymuxd rejected this connection: missing bearer token (set --token or TYMUXD_TOKEN to authenticate)"` (Task 2.2.1b) |
| Regression: other status codes unaffected by the new branch | `crates/tymux-cli/src/main.rs` | `friendly_message_unaffected_for_other_status_codes` | Unit (happy — regression) | `NotFound("no such session: abc")` → unchanged passthrough (Task 2.2.1b) |
| SM4 / Scope: `clients/go` rejects missing/wrong token, unary | `clients/go/integration/integration_test.go` | `TestListSessionsRejectsMissingOrWrongToken` | Integration (error) | wrong/empty token → `connect.CodeUnauthenticated` (Task 3.1.1d) |
| SM4 / Scope: `clients/go` accepts correct token, unary | `clients/go/integration/integration_test.go` | `TestListSessionsSucceedsWithCorrectToken` | Integration (happy) | correct token → success (Task 3.1.1d) |
| SM4 / Scope: `clients/go` rejects missing/wrong token, `Attach` (proves `WrapStreamingClient` is wired, not just `WrapUnary`) | `clients/go/integration/integration_test.go` | `TestAttachRejectsMissingOrWrongToken` | Integration (error) | wrong/empty token on `Attach` → `Unauthenticated` (Task 3.1.1e) |
| SM4 / Scope: `clients/go` accepts correct token, `Attach` | `clients/go/integration/integration_test.go` | `TestAttachSucceedsWithCorrectToken` | Integration (happy) | correct token → streams normally (Task 3.1.1e) |
| SM4 / Scope: `clients/ts` rejects missing/wrong token, unary | `clients/ts/test/integration.test.ts` | `"listSessions rejects a missing/wrong token"` | Integration (error) | `ConnectError.code === Code.Unauthenticated` (Task 3.2.1c) |
| SM4 / Scope: `clients/ts` accepts correct token, unary | `clients/ts/test/integration.test.ts` | `"listSessions succeeds with the correct token"` | Integration (happy) | correct token → success (Task 3.2.1c) |
| SM4 / Scope: `clients/ts` rejects missing/wrong token, `Attach` | `clients/ts/test/integration.test.ts` | `"attach rejects a missing/wrong token"` | Integration (error) | `Code.Unauthenticated` on `Attach` (Task 3.2.1d) |
| SM4 / Scope: `clients/ts` accepts correct token, `Attach` | `clients/ts/test/integration.test.ts` | `"attach succeeds with the correct token"` | Integration (happy) | correct token → streams normally (Task 3.2.1d) |

\* Test names marked with an asterisk are **not verbatim in plan.md** —
Task 1.1.3c describes 4 test cases in prose ("4 tests covering the four
ACs...") without giving `snake_case` function names the way every other
task in the plan does. Names above follow this plan's own established
naming convention (`<function>_<expected>_<condition>`) so implementers
have a starting point, but are suggestions, not a spec — confirm against
whatever Task 1.1.3c's implementer actually names them.

Two integration tests for `tymux-cli` (Task 2.1.2c, 2.1.2d) are similarly
described but unnamed in plan.md; left unnamed above rather than invented,
per this task's own instruction not to risk a name collision with what
gets implemented.

## UX Acceptance Tests

Manual CLI checklist — every surface is non-interactive text output, no
browser. One row per acceptance-criterion bullet in `design/ux.md`.

| UX Criterion | Test File | Test Name | Tool | Steps |
|---|---|---|---|---|
| Surface 1 AC1: fails fast, before any disk I/O | N/A (manual) | `ux_surface1_fails_fast_before_sessions_dir_prep` | CLI | Run `tymuxd --addr 0.0.0.0:7419` with no token, no `sessions_dir` pre-created; confirm the error returns immediately with no session-loading I/O observed (e.g. `strace -e trace=openat` shows no session-dir access before exit) |
| Surface 1 AC2: names both remedies + concrete consequence | N/A (manual) | `ux_surface1_names_token_and_tokend_env_and_rce_consequence` | CLI | Run `tymuxd --addr 0.0.0.0:7419` with no token; eyeball stderr contains `--token`, `TYMUXD_TOKEN`, and "run arbitrary commands" |
| Surface 1 AC3: states the loopback exemption plainly | N/A (manual) | `ux_surface1_states_loopback_exemption` | CLI | Same run; eyeball stderr contains "Loopback binds ... never require a token" |
| Surface 1 AC4: one clean line, no Debug dump / no tracing+exit pair | N/A (manual) | `ux_surface1_prints_clean_result_error_not_debug_dump` | CLI | Same run; confirm stderr is a plain formatted string (no `Error { .. }` struct-Debug braces, no duplicated `tracing::error!` line before the message) |
| Surface 1 AC5: message itself is the fix, no follow-up command needed | N/A (manual) | `ux_surface1_message_is_self_sufficient` | CLI | Same run; confirm no "see docs" / "run X for more info" — the two flags named are the complete remedy |
| Surface 2 AC1: loopback logs `info`, not `warn` | N/A (manual) | `ux_surface2_loopback_logs_at_info` | CLI | Run `tymuxd --addr 127.0.0.1:0`; confirm the startup log line is `INFO`, not `WARN` |
| Surface 2 AC2: non-loopback keeps `warn`, states consequence | N/A (manual) | `ux_surface2_non_loopback_warns_with_consequence` | CLI | Run `tymuxd --addr 0.0.0.0:0 --token s3cr3t`; confirm `WARN` line states "bearer-token auth is enforced on every call" |
| Surface 2 AC3: neither branch logs the token | N/A (manual) | `ux_surface2_no_token_in_startup_logs` | CLI | Both runs above with `TYMUXD_TOKEN=s3cr3t-marker` set; grep full stdout/stderr for `s3cr3t-marker`, expect zero matches |
| Surface 2 AC4: only one startup-time signal, no extra banner | N/A (manual) | `ux_surface2_single_startup_line_no_extra_banner` | CLI | Confirm exactly one auth-related log line appears at startup in each branch, no separate "auth enabled/disabled" banner |
| Surface 3 AC1: `--help` shows flag and its env fallback | N/A (manual) | `ux_surface3_help_shows_flag_and_env_annotation` | CLI | Run `tymux --help`; confirm `--token <TOKEN>` line shows `[env: TYMUXD_TOKEN=]` |
| Surface 3 AC2: flag-beats-env precedence needs no extra UX surface | N/A (manual) | `ux_surface3_precedence_matches_clap_default_no_extra_doc` | CLI | Confirm `--help` text has no separate precedence explanation beyond the auto-generated `[env: ...]` annotation (clap default behavior is sufficient) |
| Surface 3 AC3 / Gap 1 (tymux-cli half): `--help` shows token-generation guidance | N/A (manual) | `ux_surface3_help_shows_openssl_generation_hint` | CLI | Run `tymux --help`; confirm the `--token` help text includes `openssl rand -hex 32` — **now passes**, closed per Task 2.1.1b (see Gap 1 note above); re-verify against the built binary, not just plan.md prose |
| Surface 3 AC4: consistent terminology, exact flag/env names | N/A (manual) | `ux_surface3_terminology_matches_tymuxd_side_exactly` | CLI | Confirm `--help` uses exactly `--token` / `TYMUXD_TOKEN`, no `--auth-token`/`TYMUX_TOKEN` variants anywhere in `tymux-cli` or `tymuxd` output |
| Surface 4 AC1: three structurally distinct opening clauses | N/A (manual) | `ux_surface4_three_way_error_distinguishability` | CLI | Trigger (a) daemon down: `tymux ls` with no `tymuxd` running, (b) auth-rejected: `tymux ls` against a token-gated daemon with no token, (c) other error: `tymux ls session-that-does-not-exist`; confirm three distinct opening clauses per `research/ux.md` §3's table |
| Surface 4 AC2: remedy named inline on every `Unauthenticated` branch | N/A (manual) | `ux_surface4_remedy_named_for_missing_and_invalid_token` | CLI | Run `tymux ls` (no token) and `tymux ls --token wrongvalue` against a token-gated daemon; both outputs end with `(set --token or TYMUXD_TOKEN to authenticate)` |
| Surface 4 AC3: other status codes provably unaffected | N/A (manual) | `ux_surface4_not_found_error_unchanged` | CLI | `tymux ls no-such-session` against an untokened/loopback daemon; confirm output is exactly `no such session: no-such-session`, no auth-wrapper text |
| Surface 4 AC4: no dead end — states problem and fix in one line | N/A (manual) | `ux_surface4_no_dead_end_single_line` | CLI | Re-confirm both Surface 4 outputs are single lines containing both "rejected this connection" and the remedy clause, no follow-up prompt required |
| Surface 5 AC1 (tymux-cli half): operator finds `openssl` hint without leaving `--help`/README | N/A (manual) | `ux_surface5_tymux_cli_help_is_self_sufficient_for_token_gen` | CLI | Run `tymux --help` only (no other doc); confirm `openssl rand -hex 32` is discoverable — **passes** for tymux-cli per Gap-1 closure above |
| Surface 5 AC1 (tymuxd half) | N/A (manual) | `ux_surface5_tymuxd_startup_error_mentions_openssl_hint` | CLI | Run `tymuxd --addr 0.0.0.0:0` with no token; confirm the fail-fast stderr message contains `openssl rand -hex 32` — **now passes**, closed per Task 1.1.3a's updated message text (Phase 4 validation feedback) |
| Surface 5 AC2: placement matches doc-comment-as-help-text convention | N/A (manual) | `ux_surface5_doc_comment_convention_matches_no_status_bar` | Code read | Diff the `token` field's `///` comment style against `no_status_bar`'s two lines above it in the same `Cli` struct; confirm same `///`-above-`#[arg(...)]` shape |
| Cross-surface AC1: no dead ends anywhere | N/A (manual) | `ux_cross_no_dead_ends_across_all_surfaces` | CLI | Aggregate check across Surfaces 1 and 4 (both pass) and Surface 3 (`--help`, now also passes per Gap-1 closure) |
| Cross-surface AC2: terminology consistency tymuxd ↔ tymux-cli | N/A (manual) | `ux_cross_terminology_consistent_both_sides` | CLI | Compare Surface 1/2 (`tymuxd`) wording against Surface 4 (`tymux-cli`) wording; confirm "bearer token"/"token" used throughout, never "credential"/"auth token" |
| Cross-surface AC3: `Unauthenticated` (not `PermissionDenied`) is the programmatic signal | N/A (manual) | `ux_cross_status_code_is_unauthenticated_not_permission_denied` | CLI + code read | Confirm `tonic::Status::unauthenticated(...)` in Task 1.2.1a, and that `clients/go`/`clients/ts` integration tests assert `Unauthenticated` specifically (already covered by the named Go/TS tests above) |
| Cross-surface AC4: loopback path provably silent (primary backward-compat contract) | N/A (manual) | `ux_cross_loopback_session_has_zero_new_prompts_or_flags` | CLI | Run a full ordinary `tymux` session (loopback, no `--token`, no `TYMUXD_TOKEN`) end to end; confirm zero new log lines, prompts, or flag requirements versus pre-feature behavior |
| Cross-surface AC5: risk stated once, not repeated per call | N/A (manual) | `ux_cross_risk_framing_not_repeated_per_authenticated_call` | CLI | Issue several successful authenticated calls against a non-loopback+token daemon; confirm the "arbitrary commands" framing appears only at startup (Surface 2), never per-call |
| Cross-surface AC6: three-way distinguishability holds against actual code, not just research doc | N/A (manual) | `ux_cross_three_way_distinguishability_verified_against_task_2_2_1a` | Code read | Re-verify Task 2.2.1a's actual `friendly_message` branch strings match the three opening clauses claimed (duplicate check of Surface 4 AC1, against source not prose) |

## Test Stack

- **Unit**: Rust `#[test]` (`cargo test -p tymuxd`, `cargo test -p
  tymux-cli`), all synchronous/pure-function — no live network socket. No
  new test framework introduced by this plan.
- **Integration**: Rust (`cargo test -p tymuxd`, `cargo test -p tymux-cli`)
  against a real bound daemon via the `spawn_test_server`-style harness
  (Task 1.2.2b) and its non-loopback+token variant; Go `go test ./...` in
  `clients/go/integration`; TypeScript via Node's built-in test runner
  (`tsx --test test/*.test.ts` — **not** Jest; confirmed from
  `clients/ts/package.json`'s `"test"` script) in `clients/ts/test`.
- **E2E / UX**: manual checklist (CLI, no browser) — see UX Acceptance
  Tests above.

## Coverage Targets and How to Measure

| Stack | Coverage command | Target |
|---|---|---|
| Rust | `cargo tarpaulin --out Stdout` | ≥80% line — **not currently wired into CI** (`.github/workflows/ci.yml` runs `cargo test --workspace` only; no `tarpaulin`/`llvm-cov` step exists today, confirmed by grep) |
| Go | `go test ./... -coverprofile=coverage.out && go tool cover -func=coverage.out` | ≥80% line |
| TypeScript | `npx tsx --test --experimental-test-coverage test/*.test.ts` | ≥80% line — `clients/ts/package.json`'s `"test"` script is `tsx --test test/*.test.ts` (Node's built-in test runner), not Jest, so `npx jest --coverage` does not apply here |

- All public service methods: happy path + error paths covered (see
  Requirement → Test Mapping — every `TymuxService`-facing surface has both).
- All external integrations: unit mocked (`BearerAuthInterceptor`/
  `BearerAuth` tested with hand-built `Request`s) + at least one real
  integration test per language (`tymuxd`↔`tymux-cli`, `tymuxd`↔Go,
  `tymuxd`↔TS) — satisfied.
- UX acceptance criteria: every criterion in `design/ux.md` (19 per-surface
  + 6 cross-surface = 25) has a corresponding manual test above.
- **Known gaps, addressed after this validation pass's own findings**:
  - NFR-Performance's "no measurable per-RPC latency regression" half has
    no benchmark/timing test anywhere in plan.md — only the "uses
    `constant_time_eq`, not `==`" code-read AC. Resolved not by adding a
    benchmark but by an explicit written rationale in plan.md's
    Observability Plan (added post-validation): the compared value is a
    single fixed-size in-memory byte string with no I/O/allocation/lock
    contention on the hot path, so no plausible input shape under this
    feature's scope could produce a regression a benchmark would catch
    that code review wouldn't — revisit if scoped-token work removes that
    "fixed-size, O(1) per RPC" property.
  - Task 1.1.3c's 4 unit tests and Task 2.1.2c/2.1.2d's 2 integration tests
    have no verbatim names in plan.md — implementer discretion, flagged
    above rather than pre-named to avoid a naming collision.
  - Surface 5 / Gap 1 — now closed on both sides (`tymux-cli --help` and
    `tymuxd`'s startup-failure text) — see the Gap 1 note above.

## Ship Checklist

*(Added per `pre-mortem.md` P1 #5: Epic 1.3 touches a third crate
(`crates/tymux-core`), is explicitly independent/parallelizable with no
dependency on the auth-interceptor epics, and wasn't re-verified by either
Phase 3 review's re-check pass — exactly the shape of task a
subagent-driven implementation run organized around "the auth epics" could
silently drop. Its failure mode is invisible to CI if simply never done
(the regression test only fails if the fix is written wrong, not if it's
missing entirely, since nothing else in the suite exercises
`spawn_internal`'s env handling). Confirm explicitly before considering
this feature shipped, not just "all epics in plan.md say done":)*

- [ ] `cmd.env_remove("TYMUXD_TOKEN")` (or equivalent) is present in
      `crates/tymux-core/src/pane.rs`'s `spawn_internal` (Task 1.3.1a).
- [ ] `spawn_should_not_leak_tymuxd_token_into_pane_environment` (Task
      1.3.1b) exists in `crates/tymux-core/src/pane.rs` and passes.
- [ ] `cargo test -p tymux-core` passes in full (confirms the fix didn't
      regress normal env inheritance for every other variable).

## Migration Plan

N/A — no schema or data changes (plan.md's own Migration Plan section is
explicitly "N/A"; no persisted state changes shape, the token lives only in
process memory and is never written to disk).
