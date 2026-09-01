# Validation Plan: unix-socket-auth

**Date**: 2026-08-29

## Happy Path Scenario

Given a single-user machine running `tymuxd` with its loopback TCP bind
(today's baseline, per `requirements.md`'s Baseline section), when the
operator starts `tymuxd` with zero new configuration and a same-uid
`tymux-cli`/`clients/go`/`clients/ts` client connects, then the client
dials the daemon's newly-created, owner-only (`0600`) Unix domain socket
at its default path, the daemon accepts the connection after verifying
the peer's kernel-reported uid via `SO_PEERCRED` matches its own, and
`ListSessions`/`tymux ls` succeeds with output identical to the
pre-feature TCP-only baseline — proving the "both-by-default, zero
required config change" success metric end-to-end.

Error paths and edge cases below are variations on this core scenario: a
mismatched-uid peer (Surface 9), a group-member peer (Story 5.1.2), a
missing/unreachable socket falling back to TCP (Surface 7), and the
various startup-time misconfigurations (Surfaces 3-5) that must fail
loudly rather than silently degrade below today's baseline.

## Requirement → Test Mapping

Requirement IDs below are derived from `requirements.md`'s Success
Metrics and In-Scope sections (that document has no pre-numbered
requirement list). Test names/files are copied verbatim from
`implementation/plan.md`'s already-specified tasks — nothing here is
newly invented naming; this table is the requirement-centric cross-index
plan.md itself doesn't provide.

| Requirement | Test File | Test Name | Type | Scenario |
|---|---|---|---|---|
| **R1** — UDS listener active by default alongside TCP, both accept RPCs concurrently, SIGTERM drains both | `crates/tymuxd/tests/uds_socket_lifecycle.rs` | `tymuxd_dual_listener_drains_concurrent_tcp_and_uds_attach_streams_on_sigterm` (Task 5.2.3a — **closes Gap 1 below**; Task 4.2.2c's manual/scripted check remains as a pre-implementation dry run, no longer the only proof) | Integration | Start a real `tymuxd` subprocess with both listeners enabled, open one `Attach` stream over TCP and one over UDS concurrently, send SIGTERM, assert both drain gracefully before the process exits. |
| R1 (partial automated coverage) | `crates/tymuxd/src/main.rs` | `uds_server_accepts_matching_uid_client` (Task 5.1.1b) | Integration | Proves UDS-side accept via a custom harness (`spawn_uds_test_server`), not the real dual-listener `main()`. |
| **R2** — Default UDS socket path algorithm (`$XDG_RUNTIME_DIR`/`$TMPDIR`/`/tmp` fallback, uid-scoped), mirrored byte-for-byte across `tymuxd`/`tymux-cli`/`clients/go`/`clients/ts` | `crates/tymuxd/src/auth.rs` | `default_uds_socket_path_prefers_xdg_runtime_dir` (Task 1.1.1b) | Unit (happy) | `XDG_RUNTIME_DIR=/run/user/1000` → `/run/user/1000/tymuxd/tymuxd.sock`. |
| R2 | `crates/tymuxd/src/auth.rs` | `default_uds_socket_path_treats_empty_xdg_runtime_dir_as_unset` (Task 1.1.1b) | Unit (error/edge) | Empty-string env var falls through to the `/tmp` fallback, not honored literally. |
| R2 | `crates/tymux-cli/src/main.rs`, `clients/go/udsdialer/udsdialer_test.go`, `clients/ts/test/socketpath.test.ts` | Same 5 cases per language (Tasks 6.1.1b/7.1.1b/8.2.1b) | Unit (cross-language parity) | All four implementations read the one shared `project_plans/unix-socket-auth/socket-path-fixtures.json` fixture file, not four independently hand-typed tables. |
| **R3** — `--socket-path`/`TYMUXD_SOCKET_PATH` override precedence | `crates/tymuxd/src/auth.rs` | `resolve_uds_socket_path_prefers_flag_over_env` (Task 1.1.2b) | Unit (happy) | Flag beats env beats default. |
| R3 | `crates/tymuxd/src/auth.rs` | `resolve_uds_socket_path_falls_back_to_default_when_neither_set` (Task 1.1.2b) | Unit (error/edge) | Neither source present falls back to `default_uds_socket_path`. |
| R3 (tymux-cli, `clap`-based) | `crates/tymux-cli/src/main.rs` | `cli_help_output_lists_socket_path_flag_with_env_annotation` (Task 6.1.1d) | Unit | `--socket-path`/`TYMUXD_SOCKET_PATH` render in `tymux --help` (closes ux.md Gap 2 for this one flag). |
| **R4** — Owner-only (`0600`) socket creation via TOCTOU-safe `umask`-before-`bind` | `crates/tymuxd/src/auth.rs` | `bind_uds_listener_creates_owner_only_socket_at_mode_0600` (Task 2.2.1b) | Unit (happy) | No group configured → mode exactly `0o600`. |
| R4 | `crates/tymuxd/src/auth.rs` | `bind_uds_listener_never_touches_permissions_of_a_pre_existing_grandparent_directory` / `..._un_nested_parent_directory` (Task 2.2.1b) | Unit (error/invariant) | A pre-existing, `tymuxd`-non-owned directory is never `chmod`ed. |
| R4 | `crates/tymuxd/tests/` | `main_exits_nonzero_with_clean_message_when_uds_socket_path_unwritable` (Task 4.2.1b) | Integration | End-to-end fatal-bind-failure via a real subprocess (Surface 3). |
| **R5** — `--socket-group`/`TYMUXD_SOCKET_GROUP` group-relaxed access (`0660` + `chown`), unknown group fails loudly | `crates/tymuxd/src/auth.rs` | `bind_uds_listener_creates_group_socket_at_mode_0660_with_configured_gid` (Task 2.2.1b) | Unit (happy) | Configured gid → mode `0o660`, correct group ownership. |
| R5 | `crates/tymuxd/src/auth.rs` | `resolve_gid_by_name_returns_none_for_unknown_group` (Task 1.2.1c); `bind_uds_listener_returns_distinct_message_when_chown_group_permission_denied` (Task 2.2.1b) | Unit (error) | Unknown group name / daemon not a member of the target group both fail with a distinct, actionable message, never a silent owner-only fallback. |
| R5 | `crates/tymuxd/tests/` | `main_exits_nonzero_with_clear_message_when_socket_group_unknown` (Task 4.2.1c), `main_exits_nonzero_with_clear_message_when_socket_group_membership_denied` (Task 4.2.1d) | Integration | End-to-end fatal-exit wiring (Surface 4), not just the unit-level `None` return. |
| R5 (group access works, real socket) | `crates/tymuxd/src/main.rs` | `uds_server_accepts_group_member_when_uid_differs_but_gid_matches` (Task 5.1.2a) | Integration | Real accepted connection, mismatched uid, matching gid, Linux `/proc`-based path. |
| **R6** — Peer-credential extraction via `SO_PEERCRED`/`peer_cred()` at accept time, never client-supplied | `crates/tymuxd/src/auth.rs` | `pre_authorized_unix_stream_caches_authorized_decision_at_construction` (Task 3.2.1a-test) | Unit (happy) | Real `UnixStream::pair()` → decision computed from kernel-reported `UCred`, not any request field. |
| R6 | `crates/tymuxd/src/auth.rs` | `pre_authorized_unix_stream_caches_unauthorized_decision_when_uid_mismatched` (Task 3.2.1a-test) | Unit (error) | Mismatched `daemon_uid` at construction → cached `authorized: false`. |
| R6 | `crates/tymuxd/src/main.rs` | `uds_server_rejects_when_daemon_uid_does_not_match_wiring` (Task 5.1.1b) | Integration | Real socket round-trip proves the server-side wiring rejects, not just the pure function. |
| **R7** — `peer_is_authorized` decision (uid match OR configured-group membership; Linux full supplementary-group `/proc` check, other platforms primary-gid fallback — ADR-002) | `crates/tymuxd/src/auth.rs` | `peer_is_authorized_grants_daemon_own_uid_always` (Task 3.1.2b) | Unit (happy) | Matching uid always authorized regardless of group config. |
| R7 | `crates/tymuxd/src/auth.rs` | `peer_is_authorized_rejects_different_uid_no_group_configured`, `peer_is_authorized_rejects_different_uid_not_in_configured_group` (Task 3.1.2b) | Unit (error) | Mismatched uid, no/failed group match → rejected. |
| R7 | `crates/tymuxd/src/auth.rs` | `peer_is_group_member_linux_finds_own_real_gid_via_own_pid`, `..._does_not_find_an_absent_gid`, `..._returns_false_for_nonexistent_pid` (Task 3.1.1b) | Unit (Linux-specific, `#[cfg(target_os = "linux")]`) | `/proc/<pid>/status`-based full supplementary-group check. |
| R7 (macOS/BSD fallback) | `crates/tymuxd/src/auth.rs` | Same `peer_is_authorized_*` tests (Task 3.1.2b), non-`cfg`-gated | Unit (platform-matrix, INFERRED) | These tests are not Linux-gated, so on a `macos-latest` CI runner they compile/exercise the `peer.gid == gid` primary-gid-only branch automatically — **not independently confirmed by running CI**, flagged INFERRED from reading the `#[cfg(target_os = "linux")]` gating in Task 3.1.2a's source, not from an actual macOS CI run. |
| **R8** — Every UDS RPC gated before reaching the handler; decision computed once per connection, never per RPC (Performance SLO) | `crates/tymuxd/src/auth.rs` | `uds_peer_cred_interceptor_accepts_authorized_decision` (Task 3.2.1b-test) | Unit (happy) | Authorized decision in extensions → request passes through unchanged. |
| R8 | `crates/tymuxd/src/auth.rs` | `uds_peer_cred_interceptor_rejects_unauthorized_decision`, `uds_peer_cred_interceptor_rejects_missing_decision` (Task 3.2.1b-test) | Unit (error, fail-closed) | Unauthorized/missing decision → `PermissionDenied`, never a panic. |
| R8 (performance/no-recomputation) | `crates/tymuxd/src/auth.rs` | `uds_peer_cred_interceptor_never_calls_peer_is_authorized_itself` (Task 3.2.1b-test) | Unit (structural) | Proves the interceptor only reads the cached bool — the specific bug Gate-2 review caught and this test guards against regressing. |
| **R9** — TOCTOU-safe stale-socket/lock handling (concurrent-start races, crash-stale reconciliation — ADR-001) | `crates/tymuxd/src/auth.rs` | `acquire_socket_lock_succeeds_for_first_caller` (Task 2.1.1b) | Unit (happy) | First caller acquires the `flock`. |
| R9 | `crates/tymuxd/src/auth.rs` | `acquire_socket_lock_fails_fast_for_concurrent_second_caller` (Task 2.1.1b), `reconcile_stale_socket_errs_and_leaves_a_live_listener_untouched` (Task 2.1.2b) | Unit (error) | Second racing caller fails fast (no hang); a live listener is never stolen. |
| R9 | `crates/tymuxd/src/auth.rs` | `reconcile_stale_socket_removes_a_genuinely_stale_file` (Task 2.1.2b) | Unit (happy — recovery) | A real bind-then-drop stale file is silently removed. |
| R9 | `crates/tymuxd/tests/uds_socket_lifecycle.rs` | `Task 5.2.1a` (unnamed in plan; a second real `tymuxd` subprocess refuses to steal a live socket) | Integration | Real subprocess race, first instance keeps serving. |
| R9 | `crates/tymuxd/tests/uds_socket_lifecycle.rs` | `tymuxd_restart_with_open_uds_attach_stream_resumes_cleanly` (Task 5.2.2a), `tymuxd_restart_after_unclean_exit_with_open_uds_attach_stream_resumes_cleanly` (Task 5.2.2b) | Integration | Clean and `SIGKILL`-unclean restart both re-bind and resume an in-flight `Attach` stream. |
| **R10** — TCP-loopback startup deprecation warning (once, `warn` level, caveat in the same sentence) | `crates/tymuxd/src/main.rs` | `tcp_deprecation_warning_fires_at_warn_level_with_disable_flag_named` (Task 4.3.1a) | Unit (happy) | Default config → `warn`-level record naming `--disable-tcp-loopback`. |
| R10 | `crates/tymuxd/src/main.rs` | `tcp_deprecation_warning_skipped_and_info_logged_when_tcp_disabled` (Task 4.3.1a) | Unit (alt path) | With TCP disabled, the `warn` is replaced by an `info`, never both (Surface 2's mutual-exclusivity AC). |
| **R11** — `--disable-tcp-loopback`/`TYMUXD_DISABLE_TCP_LOOPBACK` off-switch | `crates/tymuxd/src/auth.rs` | `resolve_tcp_disabled_true_when_flag_present`, `resolve_tcp_disabled_true_when_env_nonempty` (Task 1.3.1b) | Unit (happy) | Flag or non-empty env disables TCP. |
| R11 | `crates/tymuxd/src/auth.rs` | `resolve_tcp_disabled_false_by_default` (Task 1.3.1b) | Unit (error/default) | Neither present → TCP stays on (today's behavior, backward-compat). |
| R11 (interaction with bearer-token) | `crates/tymuxd/src/main.rs` | `tcp_disabled_and_token_configured_logs_warning_naming_token_unused` (Task 4.2.2d) | Unit | A configured `--token` becomes inert when TCP is disabled — fails loud, not silent. |
| R11 (bounded shutdown, no join hang) | `crates/tymuxd/tests/uds_socket_lifecycle.rs` | Story 4.2.2 AC2 — now proven by Task 5.2.3a's dual-transport SIGTERM-drain test (same test as R1, above) | Integration | Same test as R1 — closes what was previously the same gap. |
| **R12** — Zero-config backward compatibility (single-user machine, no behavior change) | — | No dedicated regression test; satisfied transitively by R2/R13's dial tests plus Surface 6's UX acceptance criteria | UX/behavioral | See UX Acceptance Tests table, Surface 6. |
| **R13** — Client UDS-first dialing + logged TCP fallback, `isatty()`-independent, mirrored across 3 clients | `crates/tymux-cli/src/main.rs` | `dial_channel_uses_uds_when_reachable`, `dial_channel_falls_back_to_tcp_with_notice_when_uds_unreachable`, `dial_channel_skips_uds_entirely_when_addr_explicit` (Task 6.2.1c) | Unit (happy/error/edge) | UDS-first, one-line fallback notice, explicit `--addr` bypasses UDS entirely. |
| R13 | `crates/tymux-cli/tests/uds_integration.rs` | `tymux_ls_succeeds_via_uds_when_tcp_disabled`, `tymux_ls_falls_back_to_tcp_and_logs_notice_when_uds_unreachable` (Task 6.4.1b) | Integration | Real dual-listener daemon, both branches. |
| R13 (Go) | `clients/go/udsdialer/udsdialer_test.go`, `clients/go/integration/integration_test.go` | `TestDialUnixHTTPClientRoundTripsListSessions` (Task 7.1.2b), `TestListSessionsSucceedsOverUDS` (Task 7.3.1a) | Integration | Go client UDS round-trip against a real daemon. |
| R13 (TS) | `clients/ts/test/integration.test.ts` | Task 8.2.2b (UDS-first + fallback), Task 8.3.1b (accept) | Integration | Node client UDS round-trip. |
| R13 (`isatty()` independence) | — | ux.md AC6 — verified by code-absence grep (no `is_terminal`/`isatty` call), not a runtime test | Verification-by-inspection | Not a `cargo test`/`go test`/`tsx --test` — a static grep check; see UX table cross-surface AC6. |
| **R14** — Client-side friendly error on `PermissionDenied`, distinct from `Unauthenticated` | `crates/tymux-cli/src/main.rs` | `friendly_message_names_the_remedy_for_permission_denied_status` (Task 6.3.1b) | Unit (happy) | Exact documented remedy string. |
| R14 | `crates/tymux-cli/src/main.rs` | `friendly_message_names_the_remedy_for_unauthenticated_status` (pre-existing, re-asserted byte-identical per Task 6.3.1's AC2) | Unit (regression) | Bearer-token rejection message is unaffected by this feature. |
| R14 | `crates/tymux-cli/tests/uds_integration.rs`, `clients/go/integration/integration_test.go`, `clients/ts/test/integration.test.ts` | Task 6.4.1c, `TestListSessionsRejectsOverUDSWithMismatchedUID` (Task 7.3.1b), Task 8.3.1c | Integration (CI-privilege-gated) | True cross-uid reject proof; `#[ignore]`/`t.Skip`/skipped on this repo's actual `ubuntu-latest`/`macos-latest` CI (no root/`CAP_SETUID`) — see R15. |
| **R15** — Cross-client integration parity, with a documented CI-privilege fallback for the true reject path | — | Unresolved Questions entry (resolved during planning): Tasks 6.4.1c/7.3.1b/8.3.1c ship skipped by default; `peer_is_authorized`'s unit tests (R7) are the accepted substitute proof | Documented fallback | Confirmed against `.github/workflows/ci.yml` (no `container:` directive) at plan time — not re-verified here. |
| **R16** — Observability: rejection logs (`peer_uid`/`peer_gid`, never request content) + `tymux_socket_peercred_rejection_total` counter | `crates/tymuxd/src/auth.rs` | `uds_peer_cred_interceptor_logs_peer_uid_gid_on_rejection` (Task 3.2.1b-test) | Unit (happy) | `#[tracing_test::traced_test]` asserts the log record contains `peer_uid`/the counter field and no session/pane identifiers. |
| R16 | `crates/tymuxd/src/auth.rs` | Counter increment assertion embedded in `uds_peer_cred_interceptor_rejects_unauthorized_decision` (Task 3.2.1b-test) | Unit | Counter increments exactly once per rejection. |
| **R17** — `TymuxDaemon: Clone` shares state across both listeners (no split counters/trackers) | `crates/tymuxd/src/main.rs` | `cloned_daemon_shares_attached_sessions_gauge_with_original` (Task 4.1.1b) | Unit (happy) | An operation driven through the clone is visible from the original. |

### Gap 1 (test-coverage gap, distinct from ux.md's Gaps 1/2) — RESOLVED

**Story 4.2.2's own ACs — "both listeners accept RPCs concurrently" and
"SIGTERM drains both listeners before the process exits" — had no
automated `cargo test` proving them.** Task 4.2.2a implements the
dual-listener `tokio::join!` wiring; Task 4.2.2b is verification-only
(confirms three pre-existing TCP-only test harnesses are unaffected, adds
no new test); Task 4.2.2c is explicitly a manual/scripted local
run-through ("Not a `cargo test` unit test... this task itself adds no
test code"), whose stated purpose is only to feed findings into writing
the real automated test next. Phase 5 (Epics 5.1/5.2) as originally
planned never actually added that test: Epic 5.1 uses a custom
`spawn_uds_test_server` harness (UDS-only, bypassing the real `main()`
dual-listener path entirely), and Epic 5.2's restart/resume tests drive
only a UDS client against the real subprocess, never asserting a
concurrent TCP client succeeds at the same time or that SIGTERM drains an
in-flight stream on *both* transports at once. Notably, Story 5.2.2's own
prose says "sent SIGTERM (clean shutdown, draining as Story 4.2.2 already
proves)" — asserting a proof that, per the above, did not actually exist
as an automated test.

**Fix applied**: `implementation/plan.md`'s Epic 5.2 gained a new **Story
5.2.3 / Task 5.2.3a**
(`crates/tymuxd/tests/uds_socket_lifecycle.rs`,
`tymuxd_dual_listener_drains_concurrent_tcp_and_uds_attach_streams_on_sigterm`)
— one subprocess-based integration test that starts a real `tymuxd` with
both listeners enabled, opens one `Attach` stream on TCP and one on UDS
concurrently, sends SIGTERM, and asserts both drain gracefully before the
process exits, directly automating what Task 4.2.2c previously only
checked by hand. Story 4.2.2's AC text itself was updated to cite this
task instead of describing the manual check as the only proof, and the
R1/R11 rows above now point at it.

## UX Acceptance Tests

One row per acceptance-criterion bullet in `design/ux.md`'s 11 surfaces,
plus the 9 cross-surface criteria. Tool is CLI transcript / log-line
inspection throughout (this feature has no browser UI) — either a manual
run against a locally built `tymuxd`/`tymux`, or (where noted) the same
check automated as a `#[tracing_test::traced_test]`/subprocess-stdout
assertion already listed in the Requirement → Test Mapping above.

| UX Criterion | Test File | Test Name | Tool | Steps |
|---|---|---|---|---|
| S1-AC1: deprecation caveat in the same sentence as the notice | (manual + Task 4.3.1a) | `surface1_caveat_same_sentence_as_deprecation_notice` | CLI/log inspection | Start `tymuxd` default config; grep startup stderr/log for one line containing both "is deprecated" and "no credential check". |
| S1-AC2: names the concrete off-switch inline | (manual + Task 4.3.1a) | `surface1_names_disable_flag_inline` | CLI/log inspection | Same line contains `--disable-tcp-loopback`/`TYMUXD_DISABLE_TCP_LOOPBACK=1`. |
| S1-AC3: fires at `warn`, survives default `EnvFilter::new("info")` | Task 4.3.1a (automated) | `tcp_deprecation_warning_fires_at_warn_level_with_disable_flag_named` | `cargo test` | Already automated — see R10 row. |
| S1-AC4: fires once per startup, not per-connection | (manual) | `surface1_fires_once_not_per_connection` | CLI/log inspection | Start `tymuxd`, make 5 `ListSessions` calls, count warning-line occurrences in log — must be 1. |
| S1-AC5: single-user operator sees no other behavior change | (manual) | `surface1_no_other_behavior_change_for_single_user` | CLI diff | Diff `tymux ls` output/exit code against a pre-feature build on the same fixture session — must be byte-identical apart from the one new warning line. |
| S2-AC1: `--disable-tcp-loopback` logs at `info`, not `warn` | Task 4.3.1a (automated) | `tcp_deprecation_warning_skipped_and_info_logged_when_tcp_disabled` | `cargo test` | Already automated — see R10 row. |
| S2-AC2: Surface 1's warning does not also fire (mutual exclusivity) | Task 4.3.1a (automated) | (same test, negative assertion) | `cargo test` | Same test asserts no `warn`-level TCP-deprecation record when disabled. |
| S2-AC3: names the flag that produced this state | (manual) | `surface2_names_producing_flag` | CLI/log inspection | `tymuxd --disable-tcp-loopback`; confirm the info line names both spellings. |
| S3-AC1: fails fast, before the TCP listener ever spawns | Task 4.2.1b (automated) | `main_exits_nonzero_with_clean_message_when_uds_socket_path_unwritable` | `cargo test` | Already automated — see R4 row; additionally confirm (manual) no TCP port is bound after the failed exit (`ss -ltn` / `lsof`). |
| S3-AC2: names the concrete remedy inline | Task 4.2.1b (automated) | (same test, stderr substring assertion) | `cargo test` | Asserts `--socket-path`/`TYMUXD_SOCKET_PATH` appear in stderr. |
| S3-AC3: clean literal text via `eprintln!`, never a `Debug` dump | (manual) | `surface3_clean_text_no_debug_dump` | CLI inspection | Visually confirm stderr has no `Err(...)`/`Result`/backtrace framing. |
| S3-AC4: daemon-side failure, client sees Surface 8 instead | (manual, cross-referenced with Task 4.2.1b) | `surface3_client_sees_surface8_not_surface3` | CLI inspection | Point a `tymux-cli` at the failed-to-start daemon's expected addr; confirm it prints Surface 8's "couldn't connect" text, never Surface 3's. |
| S4-AC1: fails loudly, never silently falls back to owner-only | Task 4.2.1c (automated) | `main_exits_nonzero_with_clear_message_when_socket_group_unknown` | `cargo test` | Already automated — see R5 row. |
| S4-AC2: names both spellings (`--socket-group`/`TYMUXD_SOCKET_GROUP`) | Task 4.2.1c (automated) | (same test, stderr substring) | `cargo test` | Asserts both spellings appear. |
| S4-AC3: echoes the exact bad value | Task 4.2.1c (automated) | (same test, stderr substring `tyypo-group`) | `cargo test` | Asserts the literal typo'd string is echoed. |
| S5-AC1: two live-conflict cases get textually distinct messages | Task 5.2.1a (automated) + manual for the "already starting" case | `surface5_distinct_messages_for_racing_vs_already_running` | `cargo test` + manual | Task 5.2.1a covers "already running"; manually race two `tymuxd` starts against a held lock file to observe the "already starting" text (no plan.md task exercises this exact race — see note below). |
| S5-AC2: stale-file reconciliation is silent (no message) | Task 2.1.2b (automated, at the unit level) | `reconcile_stale_socket_removes_a_genuinely_stale_file` | `cargo test` + manual full-daemon confirmation | Unit-level proven; manually confirm a real `tymuxd` restart after `kill -9` prints no "recovery" message. |
| S5-AC3: both live-conflict messages name the exact socket path | Task 5.2.1a (automated) | (same test, stderr substring) | `cargo test` | Asserts the path appears in the "already running" message. |
| S6-AC1: zero new output on the success path | Task 6.2.1c (automated) | `dial_channel_uses_uds_when_reachable` (negative: no fallback notice printed) | `cargo test` + manual diff | Automated absence-of-notice check; manually diff full `tymux ls` transcript against pre-feature baseline. |
| S6-AC2: identical behavior interactive vs. piped (no `isatty()` branch) | (verification-by-inspection) | `surface6_no_isatty_branching` | `grep`/static check | `grep -rn 'is_terminal\|isatty\|IsTerminal' crates/tymux-cli/src/main.rs` — must return no hits in `dial_channel`. |
| S6-AC3: all 3 clients resolve the identical default path | Tasks 1.1.1b/6.1.1b/7.1.1b/8.2.1b (automated, shared fixture) | (fixture-driven tests, 4 languages) | `cargo test`/`go test`/`tsx --test` | Already automated — see R2 row. |
| S7-AC1: exactly one fallback line per invocation, not repeated per-RPC | Task 6.2.1c / 6.4.1b (automated) | `dial_channel_falls_back_to_tcp_with_notice_when_uds_unreachable` | `cargo test` | Already automated — see R13 row; extend assertion to count exactly 1 line across a multi-RPC command if not already covered. |
| S7-AC2: names the exact path that was tried | Task 6.2.1c (automated) | (same test, stderr substring) | `cargo test` | Asserts the resolved socket path string appears. |
| S7-AC3: uses the word "deprecated" | Task 6.2.1c (automated) | (same test, stderr substring) | `cargo test` | Asserts "deprecated" appears. |
| S7-AC4: command still succeeds (exit 0) after fallback | Task 6.4.1b (automated) | `tymux_ls_falls_back_to_tcp_and_logs_notice_when_uds_unreachable` | `cargo test` | Already automated — see R13 row. |
| S7-AC5: fires identically piped vs. interactive | (verification-by-inspection) | `surface7_no_isatty_branching` | `grep`/static check | Same grep as S6-AC2, applied to the fallback branch specifically. |
| S8-AC1: unchanged text/trigger, now also covers stale-socket | (manual, no plan.md task — explicitly "no new case needed") | `surface8_unchanged_message_covers_stale_and_absent` | CLI inspection | Point `tymux-cli` at both a genuinely-nothing-running daemon and a stale-socket-file daemon; confirm identical message both times. |
| S8-AC2: distinct opening clause from Surfaces 9/token-reject | (manual) | `surface8_distinct_opening_clause` | CLI inspection | Visual diff of "couldn't connect" vs. "tymuxd rejected this connection". |
| S8-AC3: also covers Surface 3's daemon-side bind failure, client-side | (manual) | `surface8_covers_surface3_from_client_pov` | CLI inspection | Same as S3-AC4. |
| S9-AC1: exact text matches Task 6.3.1a's AC verbatim | Task 6.3.1b (automated) | `friendly_message_names_the_remedy_for_permission_denied_status` | `cargo test` | Already automated — see R14 row. |
| S9-AC2: shares prefix with bearer-token rejection, differs after colon + status code | Task 6.3.1b + existing `Unauthenticated` test (automated) | (byte-comparison of both outputs) | `cargo test` | Both tests' outputs compared for shared prefix, distinct suffix/status. |
| S9-AC3: plain-language remedy, no jargon (`SO_PEERCRED`, raw uid/gid) in the printed message | Task 6.3.1b (automated, string-exact match) | (same test, full-string equality) | `cargo test` | The exact-match assertion structurally rules out jargon leaking in. |
| S9-AC4: containerized-uid caveat lives in a doc comment, not the printed message (known, accepted gap) | (manual code inspection) | `surface9_container_caveat_is_doc_comment_only` | Code read | Confirm the caveat is present as a `///` doc comment above the branch, absent from the runtime string — matches ux.md's Gap 1 finding, not a defect to fix here. |
| S10-AC1: `tymuxd --help` is a no-op, no regression | (manual) | `surface10_tymuxd_help_noop` | CLI inspection | `tymuxd --help` starts the daemon normally; no flag-specific output. |
| S10-AC2: `tymux-cli --help` **does** show `--socket-path` (fixed vs. ux.md's flagged Gap 2) | Task 6.1.1d (automated) | `cli_help_output_lists_socket_path_flag_with_env_annotation` | `cargo test` | Already automated — see R3 row; note `--socket-group`/`--disable-tcp-loopback` remain undiscoverable via `--help` since they're `tymuxd`-only flags with no client-side equivalent — not a defect. |
| S10-AC3: documentation uses exact flag/env spellings, no alternates | (manual doc review) | `surface10_docs_use_canonical_spellings` | Doc grep | `grep` README/CHANGELOG for `--socket-path`/`--socket-group`/`--disable-tcp-loopback` and their env vars; confirm no alternate spelling anywhere. |
| S11-AC1: TCP-loopback caveat present at the one universally-seen surface | Task 4.3.1a (automated, indirectly) | (same warning-text test as S1-AC1) | `cargo test` | Already satisfied — see S1 rows. |
| S11-AC2: remaining 3 caveats reach *some* operator-visible surface before shipping to a multi-user audience | Tasks 4.3.2a (startup log) + 9.1.1a (README) | `surface11_remaining_caveats_operator_visible` | Doc/code audit + `cargo test` (4.3.2b) | **RESOLVED** — the `--socket-group` "full control" and macOS-primary-gid-only caveats are logged once at startup (Task 4.3.2a, tested by 4.3.2b); the containerized-uid note now has an explicit README section (`implementation/plan.md`'s Task 9.1.1a, closing this gap — previously all three existed only as Rust doc comments). |
| Cross-AC1: no dead ends — every error names its own remedy | Tasks 4.2.1b/c/d, 5.2.1a, 4.2.1b (all automated) | (aggregate of Surfaces 3/4/5/8/9's individual rows above) | `cargo test` + manual | Already covered row-by-row above; this is the rollup check. |
| Cross-AC2: terminology consistency ("Unix socket", never "UDS socket"/"domain socket") | (manual code grep) | `cross_ac2_terminology_consistency` | Doc/code grep | `grep -rn "UDS socket\|domain socket" crates/tymuxd/src crates/tymux-cli/src` — expect no hits outside internal comments/type names. |
| Cross-AC3: `PermissionDenied` provably distinct from `Unauthenticated` at the status-code level | Task 6.3.1b + existing interceptor tests (automated) | (status-code equality assertions) | `cargo test` | Already automated across R8/R14 rows. |
| Cross-AC4: baseline path is provably silent (zero output/timing change) | S6-AC1's test (automated + manual) | `dial_channel_uses_uds_when_reachable` + manual diff | `cargo test` + manual | Already covered — see S6-AC1/R12. |
| Cross-AC5: "both-by-default reads as fully isolated" risk addressed in the one universal surface | S1-AC1's test | (same as S1-AC1) | `cargo test` + manual | Already covered. |
| Cross-AC6: no `isatty()`-conditional transport selection | (verification-by-inspection) | `cross_ac6_no_isatty_conditional_transport` | `grep`/static check | Same grep as S6-AC2/S7-AC5, run once against the whole `dial_channel`/daemon-side equivalent. |
| Cross-AC7: no color-only signaling anywhere | (manual code grep) | `cross_ac7_no_color_only_signaling` | Doc/code grep | `grep -rn "colored\|owo_colors\|termcolor\|\\x1b\[" crates/tymuxd/src crates/tymux-cli/src` for all new code paths — expect no hits. |
| Cross-AC8: rejected connection diagnosable in one command, no `--verbose` needed | Task 6.3.1b (automated) + manual | `friendly_message_names_the_remedy_for_permission_denied_status` + manual single-command run | `cargo test` + manual | Automated string proof; manually confirm `tymux ls` alone (no extra flag) against a rejecting daemon prints the full remedy on the first try. |
| Cross-AC9: five rejection/failure openings stay visually distinct | (manual transcript comparison) | `cross_ac9_five_distinct_opening_clauses` | CLI transcript diff | Collect all 5 opening clauses from a real run of each scenario (Surfaces 7, 8, 9, existing bearer-token reject, an arbitrary other RPC error) side by side; confirm no two share a prefix. |

## Test Stack

- **Unit**: Rust — built-in `cargo test` (`#[test]`, `#[tracing_test::traced_test]` for log-assertion tests, matching this crate's existing `BearerAuthInterceptor` test precedent). No new test-only crate is introduced anywhere in this plan (confirmed: Task 2.1.1b reuses the existing `uuid` dependency instead of adding `tempfile`).
- **Integration**: Rust — real-subprocess tests spawning the actual `tymuxd`/`tymux` binaries (mirroring `restart_persistence.rs`'s and `clients/go/integration/integration_test.go`'s existing patterns), plus in-process real-`UnixListener`/`UnixStream::pair()` harnesses (`spawn_uds_test_server`) for the socket-level accept/reject proofs that don't need a full subprocess.
- **Go**: `go test` — table-driven (`clients/go/udsdialer/udsdialer_test.go`), real-subprocess integration tests (`clients/go/integration/integration_test.go`), using `exec.Cmd.SysProcAttr.Credential` for the CI-privilege-gated cross-uid reject test.
- **TS**: `tsx --test` (this package's existing `npm test` script, per Task 8.2.1b's note) — `clients/ts/test/socketpath.test.ts`, `clients/ts/test/integration.test.ts`, real-subprocess daemon spawning via `clients/ts/test/daemon.ts`.
- **E2E / UX**: No browser UI exists in this feature (condensed per `design/ux.md`'s own scope note) — the "E2E" layer is the manual/scripted CLI-transcript and log-inspection checklist in the UX Acceptance Tests table above, run against a locally built `tymuxd`/`tymux-cli`/example clients before shipping to a multi-user-host audience.

## Coverage Targets and How to Measure

| Stack | Coverage command | Target |
|---|---|---|
| Rust (`tymuxd`, `tymux-cli`) | `cargo tarpaulin --out Stdout -p tymuxd -p tymux-cli` | ≥80% line, with `auth.rs`'s new functions (Epics 1-3) at effectively 100% given every AC in Phase 1-3 maps to a listed unit test above |
| Go (`clients/go`) | `go test ./... -coverprofile=coverage.out && go tool cover -func=coverage.out` | ≥80% line for `udsdialer/` |
| TypeScript (`clients/ts`) | Repo's actual test runner — confirm exact coverage invocation against `clients/ts/package.json`'s `test` script (plan.md specifies `tsx --test` but does not name a coverage flag; `node --experimental-test-coverage` is the most likely fit for a `tsx --test`-based suite and should be confirmed at implementation time, not assumed) | ≥80% line |

- All public service methods (`peer_is_authorized`, `bind_uds_listener`, `dial_channel`, `resolve_*`, `default_uds_socket_path`, `UdsPeerCredInterceptor::call`): happy path + error paths covered — confirmed row-by-row in the Requirement → Test Mapping table above.
- All external integrations (`/proc/<pid>/status` read, `getgrnam`/`getsockopt(SO_PEERCRED)` via `peer_cred()`, real `UnixListener`/`UnixStream`, subprocess daemon spawning): unit-mocked-or-real-syscall-backed AND at least one integration test — confirmed for R5-R9, R13-R15 above.
- UX acceptance criteria: every criterion in `design/ux.md` (38 per-surface bullets + 9 cross-surface) has a corresponding row in the UX Acceptance Tests table above — either an already-automated `cargo test`/`go test`/`tsx --test` cross-referenced from the Requirement → Test Mapping table, or an explicit manual/grep-based check where no automated test exists (S1-AC1/2/4/5, S3-AC3/4, S4 fully automated, S5-AC1's racing-startup case, S8 fully manual by design, S9-AC4, S10-AC1/3 — S11-AC2, formerly a known-failing criterion per ux.md's own Gap 1, is now resolved by Tasks 4.3.2a and 9.1.1a, see the row above).
