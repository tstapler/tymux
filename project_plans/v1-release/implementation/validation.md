# Validation Plan: v1-release

**Date**: 2026-07-14

## Happy Path Scenario
Given the Baseline (a single-pane-per-session `tymux` MVP with no splits, no persistence, no config/keybindings, no status bar, and only a Rust client ever exercised), when a user creates a session, splits it into two panes, detaches, kills and restarts `tymuxd`, revives the session, reattaches, enters copy-mode to review scrollback, and a TypeScript client independently calls `CreateSession`/`Attach`/`CapturePane` against the same daemon, then every one of those steps succeeds observably (`tymux ls`, the status bar, and the TS client's own output all agree on session/pane state) — proving the full v1.0 feature set end-to-end, not epic-by-epic in isolation.

---

## Requirement → Test Mapping

### Epic 1 — Foundation: GitHub remote + real CI

| Requirement | Test File | Test Name | Type | Scenario |
|---|---|---|---|---|
| Story 1.1 AC1/AC2: CI runs for real, branch protection gates merge | `.github/workflows/ci.yml` (manual verification) | `ci_should_trigger_and_gate_merge_when_pr_opened_against_main` | Manual/scripted-terminal | Push `main`, open throwaway PR, confirm `test` job runs and branch protection blocks merge until green |
| Story 1.2 AC1: cross-platform compile matrix (Linux/macOS/macOS-13) | `.github/workflows/ci.yml` | `ci_matrix_should_build_and_test_on_ubuntu_and_macos` | Integration | CI run against a PR touching `crates/tymux-cli/src/main.rs`'s `#[cfg(unix)]` block, confirm all 3 matrix legs pass |
| Story 1.2 error path: macOS-specific compile break | `crates/tymuxd/src/main.rs` (CI log) | `ci_matrix_should_fail_loudly_when_macos_compile_breaks` | Integration | Deliberately introduce a macOS-incompatible change on a throwaway branch, confirm CI fails that leg specifically, not silently |
| Story 1.3 AC1: tag/version drift check | `.github/workflows/release.yml` (or `ci.yml` tag job) | `tag_push_should_fail_when_git_tag_mismatches_workspace_version` | Integration | Push a tag not matching `Cargo.toml`'s `[workspace.package] version`, assert CI job fails loudly |
| Story 1.4 AC1: `portable-pty` cross-compiles to musl | `spike-musl-pty` throwaway crate | `spike_musl_pty_should_build_and_run_in_alpine_container` | Integration | Cross-compile spike crate to `x86_64-unknown-linux-musl`, run resulting binary inside `alpine`, assert it opens a pty and echoes a byte |
| Story 1.4 AC2 (error/fallback path): musl cross-compile fails | `spike-musl-pty` throwaway crate | `spike_musl_pty_should_trigger_gnu_fallback_when_musl_link_fails` | Integration | Simulate/observe musl link failure, assert the documented `-gnu` fallback is what gets adopted (recorded outcome, not silently improvised) |

### Epic 2 — Protocol/liveness foundation

| Requirement | Test File | Test Name | Type | Scenario |
|---|---|---|---|---|
| Story 2.1 AC1: exited pane reports `LIVENESS_DEAD` | `crates/tymuxd/src/main.rs` | `list_sessions_should_report_liveness_dead_when_pane_child_process_exited` | Unit | Happy path |
| Story 2.1 AC2: fresh session reports `LIVENESS_LIVE` | `crates/tymuxd/src/main.rs` | `create_session_should_report_liveness_live_when_pane_freshly_spawned` | Unit | Happy path |
| Story 2.1: liveness field wiring error path | `crates/tymux-core/src/pane.rs` | `is_exited_should_return_false_when_process_still_running` | Unit | Error/negative path — confirms `is_exited()` doesn't false-positive before exit |
| Story 2.1: end-to-end proto round trip | `crates/tymuxd/src/main.rs` | `session_to_proto_should_map_exited_pane_to_liveness_dead_field` | Integration | tonic in-process server: create session, kill child, call `ListSessions`, assert wire-level `Pane.liveness` |
| Story 2.2 AC1: `output_gap` emitted on `Lagged` consumer | `crates/tymuxd/src/main.rs` | `attach_should_emit_output_gap_event_when_consumer_lags_behind_broadcast_channel` | Unit | Happy path (gap detected + signaled) |
| Story 2.2 error path: normal consumer never sees spurious `output_gap` | `crates/tymuxd/src/main.rs` | `attach_should_not_emit_output_gap_event_when_consumer_keeps_pace` | Unit | Error/negative path |
| Story 2.2: CLI renders `[tymux: output dropped]` | `crates/tymux-cli/src/main.rs` | `attach_event_match_should_render_output_dropped_message_on_output_gap_variant` | Unit | Happy path |
| Story 2.2: slow-consumer simulation end-to-end | `crates/tymuxd/src/main.rs` | `attach_stream_should_observe_output_gap_before_output_resumes_when_consumer_lags` | Integration | Small channel capacity + burst sender, real tonic in-process `Attach` stream, assert `OutputGap` event ordering |
| Story 2.3 AC1: `Attach` RPC doc comment states ordering contract | `proto/tymux/v1/tymux.proto` (manual review) | `attach_rpc_doc_comment_should_state_first_message_and_cancellation_contract` | Manual/scripted-terminal | Read generated Connect-ES docs, confirm contract text present on the RPC declaration itself |
| Story 2.3 AC2: `KillSession` signals attached client before teardown | `crates/tymuxd/src/main.rs` | `kill_session_should_emit_clean_terminal_event_to_attached_client_before_stream_closes` | Unit | Happy path |
| Story 2.3 error path: `KillSession` teardown never bare-errors the stream | `crates/tymuxd/src/main.rs` | `kill_session_should_not_produce_raw_stream_error_when_client_still_attached` | Unit | Error/negative path — asserts absence of the Ctrl-D-hang failure class |
| Story 2.3: two-simulated-client end-to-end | `crates/tymuxd/src/main.rs` | `kill_session_should_close_attached_stream_cleanly_when_second_client_kills_session` | Integration | tonic in-process server: attach client A, `KillSession` from simulated client B, assert A's stream ends cleanly |
| Story 2.4 AC1: unknown `pane_id` returns `PaneLookup::Unknown` | `crates/tymux-core/src/engine.rs` | `pane_lookup_should_return_unknown_when_pane_id_never_created` | Unit | Happy path |
| Story 2.4 AC2: exited-but-recorded pane returns `PaneLookup::Dead` | `crates/tymux-core/src/engine.rs` | `pane_lookup_should_return_dead_when_pane_process_exited_but_record_exists` | Unit | Happy path |
| Story 2.4: live pane returns `PaneLookup::Live` | `crates/tymux-core/src/engine.rs` | `pane_lookup_should_return_live_when_pane_process_still_running` | Unit | Happy path (third variant, completes the 3-way match) |
| Story 2.4 error path: `capture_pane`/`attach` map `Dead` vs `Unknown` to distinct `tonic::Status` | `crates/tymuxd/src/main.rs` | `capture_pane_should_return_failed_precondition_when_pane_lookup_is_dead_vs_not_found_when_unknown` | Unit | Error path — asserts the two codes are never collapsed |

### Epic 3 — Splits (layout tree, breaking proto change, daemon, CLI addressing)

| Requirement | Test File | Test Name | Type | Scenario |
|---|---|---|---|---|
| Story 3.1 AC1: exact 2-child horizontal split geometry | `crates/tymux-core/src/layout.rs` | `compute_geometry_should_split_evenly_when_horizontal_split_has_equal_ratios` | Unit | Happy path |
| Story 3.1 AC2: odd-remainder column assigned to last child | `crates/tymux-core/src/layout.rs` | `compute_geometry_should_assign_remainder_column_to_last_child_when_width_is_odd` | Unit | Edge/error-adjacent path (rounding correctness) |
| Story 3.1: nested split geometry sums correctly | `crates/tymux-core/src/layout.rs` | `compute_geometry_should_sum_to_parent_rect_when_tree_has_nested_splits` | Unit | Happy path (3-leaf nested case) |
| **Story 3.2 AC1/AC2: `LayoutNode::split`/`remove` mutation semantics** | `crates/tymux-core/src/layout.rs` | `layout_node_should_nest_split_under_target_leaf_when_split_called` | Unit | Happy path |
| Story 3.2 AC2 error/collapse path | `crates/tymux-core/src/layout.rs` | `layout_node_should_collapse_split_when_sibling_pane_closes` | Unit | Error/edge path — 2-child collapse |
| Story 3.2: nested 3-way collapse | `crates/tymux-core/src/layout.rs` | `layout_node_should_collapse_nested_split_when_grandchild_pane_closes` | Unit | Edge path |
| Story 3.2 error path: split rejected below minimum size | `crates/tymux-core/src/layout.rs` | `layout_node_split_should_reject_with_actual_dimensions_when_below_minimum_size` | Unit | Error path — typed error carrying actual rows/cols |
| **Story 3.2 AC3 — HARD MERGE GATE: property-based invariant suite** | `crates/tymux-core/tests/layout_invariants.rs` | `layout_node_invariants_should_hold_after_any_sequence_of_split_remove_resize_ops` (proptest, `LayoutNode`) | Unit (property-based) | Arbitrary sequences of split/remove/resize; asserts ratio-sum ≈ 1.0, min-size floor never violated, no zero/one-child `Split` ever exists. **This is Epic 3's mandatory merge gate per plan.md §5 Risk Control — cannot be waived as "tests exist."** |
| Story 3.3 AC1: `SplitPane` RPC produces correct wire-level `Layout` tree | `crates/tymuxd/src/main.rs` | `split_pane_rpc_should_produce_two_leaf_layout_visible_in_list_sessions` | Integration | tonic in-process server round trip |
| Story 3.3 AC2: `WatchWindow` streams layout change without polling | `crates/tymuxd/src/main.rs` | `watch_window_should_emit_layout_event_when_another_client_calls_split_pane` | Integration | tonic in-process bidi/stream test, two simulated clients |
| Story 3.3 error path: `SplitPane` on nonexistent pane | `crates/tymuxd/src/main.rs` | `split_pane_rpc_should_return_not_found_when_pane_id_unknown` | Unit | Error path |
| Story 3.4 AC1: recursive `session_to_proto` reflects nested tree shape | `crates/tymuxd/src/main.rs` | `session_to_proto_should_reflect_nested_split_tree_shape_for_multi_window_session` | Unit | Happy path |
| Story 3.4 AC2: dimension-wise-minimum geometry applied atomically across two clients | `crates/tymux-core/src/engine.rs` | `recompute_window_geometry_should_apply_dimension_wise_minimum_when_two_clients_report_different_viewports` | Integration | Two simulated attach clients, differing viewport sizes, mock `Pane::resize` |
| Story 3.4 error path: no torn intermediate geometry state | `crates/tymux-core/src/engine.rs` | `recompute_window_geometry_should_never_produce_torn_state_when_resize_interleaves_with_read` | Integration | Concurrency test asserting all siblings observe one consistent final geometry, never mixed |
| Story 3.4 AC3: concurrent `ListSessions` on unrelated session not blocked during resize syscalls | `crates/tymux-core/src/engine.rs` | `list_sessions_should_not_block_on_unrelated_session_while_window_resize_syscalls_in_flight` | Integration | `SlowMockPane::resize()` test double (mirrors `SlowMockPersistenceBackend` pattern), asserts lock released around blocking calls |
| Story 3.5 AC1: `TargetString` resolves `session:window.pane` correctly | `crates/tymux-cli/src/main.rs` | `target_string_should_resolve_specific_pane_when_addressing_by_session_window_pane` | Unit | Happy path |
| Story 3.5 error path: out-of-range `TargetString` index | `crates/tymux-cli/src/main.rs` | `target_string_should_return_bounds_checked_error_when_pane_index_out_of_range` | Unit | Error path — preserves ADR-0001's no-panic safety property |
| Story 3.5 AC2: undersized-terminal split shows exact numeric remediation | `crates/tymux-cli/src/main.rs` | `split_command_should_show_exact_row_counts_when_terminal_below_minimum_size` | Integration | CLI + daemon round trip via `friendly_message` |

### Epic 4 — Persistence (Tier 0)

| Requirement | Test File | Test Name | Type | Scenario |
|---|---|---|---|---|
| Story 4.1 AC1: serialize/deserialize round trip preserves `LayoutNode` shape | `crates/tymux-core/src/persistence.rs` | `persisted_session_record_should_round_trip_identical_layout_shape_when_serialized_and_deserialized` | Unit | Happy path |
| Story 4.1 AC2 error path: unknown `schema_version` rejected | `crates/tymux-core/src/persistence.rs` | `persisted_session_record_should_reject_load_when_schema_version_unknown` | Unit | Error path |
| Story 4.1 AC3 error path: structurally malformed layout rejected (3-child split, bad ratio sum, zero-child split) | `crates/tymux-core/src/persistence.rs` | `persisted_layout_node_validate_structure_should_reject_when_split_has_three_children` / `..._when_ratios_do_not_sum_to_one` / `..._when_split_has_zero_children` | Unit | Error path (table-driven/parameterized — 3 named cases sharing one test body) |
| Story 4.1: `PersistenceBackend` trait abstraction is substitutable | `crates/tymux-core/src/persistence.rs` | `engine_should_accept_any_persistence_backend_implementation_via_trait_object` | Unit | Happy path — confirms DI seam exists (architecture-review.md Blocker #2 fix) |
| Story 4.2 AC1: save writes temp-file-then-rename, never in-place truncate | `crates/tymux-core/src/persistence.rs` | `fs_persistence_backend_save_should_write_temp_file_and_rename_never_truncate_in_place` | Unit | Happy path |
| **Story 4.2 AC2 — concurrency regression: lock released before slow write** | `crates/tymux-core/src/persistence.rs` | `engine_list_sessions_should_not_block_when_slow_mock_persistence_backend_is_saving` | Integration | `SlowMockPersistenceBackend` test double (sleeps before returning) injected into test `Engine`; asserts `list_sessions` on a different session proceeds unblocked |
| Story 4.3 AC1: mixed valid/corrupt-JSON files at startup | `crates/tymuxd/src/main.rs` | `daemon_startup_should_load_valid_sessions_dead_flagged_and_skip_corrupt_json_file` | Integration | Temp dir fixture: 3 valid + 1 malformed-JSON file, start test daemon, assert `ListSessions` shows 3 dead sessions, daemon doesn't crash |
| Story 4.3 AC2 error path: structurally-invalid-but-parseable file skipped | `crates/tymuxd/src/main.rs` | `daemon_startup_should_skip_structurally_invalid_layout_file_without_reaching_engine_session_map` | Integration | Fixture with valid JSON + current schema_version + 3-child split; assert it never reaches `compute_geometry`/`ReviveSession` |
| Story 4.3: combined fixture (2 valid + 1 corrupt + 1 structurally-invalid) | `crates/tymuxd/src/main.rs` | `daemon_startup_should_start_successfully_and_report_exactly_two_sessions_when_two_of_four_files_are_bad` | Integration | The explicit combined scenario named in Story 4.3's own task 4 |
| Story 4.4 AC1: `ReviveSession` respawns ptys matching persisted `LayoutNode` shape | `crates/tymuxd/src/main.rs` | `revive_session_should_respawn_ptys_matching_persisted_layout_shape_and_mark_live` | Integration | tonic in-process: dead session fixture, call `ReviveSession`, assert `Liveness::LIVE` + tree shape preserved |
| Story 4.4 AC2: bare daemon restart never auto-revives | `crates/tymuxd/src/main.rs` | `daemon_restart_should_leave_session_dead_flagged_when_revive_never_called` | Integration | Restart daemon twice without calling `revive`, assert still `Dead` |
| **Story 4.4/4.x — HARD MERGE GATE: daemon-restart-mid-session persistence test** | `crates/tymuxd/src/main.rs` | `session_layout_should_match_pre_restart_shape_when_daemon_is_killed_and_restarted_mid_session` | Integration | Kill and restart a real daemon process mid-session (not just `Engine`-level), assert the reloaded record's `LayoutNode` shape matches the pre-restart shape exactly. **This is the mandatory persistence gate plan.md §5 Risk Control names explicitly — "Epic 4 cannot merge without a test that kills and restarts a daemon mid-session and asserts the reloaded record matches the pre-restart `LayoutNode` shape."** |
| Story 4.5 AC1: SIGTERM triggers final persistence flush before exit | `crates/tymuxd/src/main.rs` | `shutdown_signal_should_flush_pending_persistence_before_process_exits_on_sigterm` | Integration | Send SIGTERM to test daemon with pending unflushed state, assert flush completes before exit |
| Story 4.5 AC2: `tymux ls` visually distinguishes live vs dead sessions | `crates/tymux-cli/src/main.rs` | `ls_command_should_render_distinct_status_strings_for_live_versus_dead_restored_session` | Unit | Happy path |
| Story 4.6 AC1: `attach` on dead session fails fast with remediation | `crates/tymux-cli/src/main.rs` | `attach_should_fail_fast_with_revive_remediation_message_when_target_session_is_dead` | Unit | Error path — asserts non-zero exit, no `Attach` stream opened |
| Story 4.6 AC2: `attach` succeeds normally post-revive | `crates/tymux-cli/src/main.rs` | `attach_should_succeed_normally_when_target_session_is_live_after_revive` | Integration | CLI + daemon round trip: revive then attach |
| Story 4.6: daemon-side defense-in-depth rejection | `crates/tymuxd/src/main.rs` | `attach_rpc_should_reject_with_failed_precondition_when_pane_lookup_is_dead` | Unit | Error path — authoritative guard independent of CLI pre-check |

### Epic 5 — Config/key-bindings + copy-mode

| Requirement | Test File | Test Name | Type | Scenario |
|---|---|---|---|---|
| Story 5.1 AC1: absent config uses hardcoded defaults, no error | `crates/tymux-cli/src/config.rs` | `tymux_config_load_or_default_should_use_hardcoded_defaults_when_no_file_present` | Unit | Happy path |
| Story 5.1 AC2: single override applies, others stay default | `crates/tymux-cli/src/config.rs` | `tymux_config_should_override_only_detach_binding_when_config_specifies_one_binding` | Unit | Happy path |
| Story 5.1 AC3 error path: one bad binding falls back, rest still load | `crates/tymux-cli/src/config.rs` | `tymux_config_should_warn_and_fallback_to_default_for_single_malformed_binding_while_others_apply` | Unit | Error path |
| Story 5.1: whole-file malformed TOML never panics | `crates/tymux-cli/src/config.rs` | `tymux_config_load_should_produce_friendly_error_not_panic_when_toml_syntax_invalid` | Unit | Error path |
| Story 5.1: config loading integration with `dirs` crate resolution | `crates/tymux-cli/src/config.rs` | `tymux_config_should_resolve_xdg_config_dir_path_when_locating_config_toml` | Integration | Real filesystem, temp `$XDG_CONFIG_HOME` |
| Story 5.2 AC1: split-across-`read()` keystroke reassembly | `crates/tymux-cli/src/input.rs` | `keystroke_reassembler_should_fire_detach_action_exactly_once_when_leader_and_key_split_across_reads` | Unit | Happy path |
| Story 5.2 AC1 (coalesced case) | `crates/tymux-cli/src/input.rs` | `keystroke_reassembler_should_fire_detach_action_exactly_once_when_leader_and_key_arrive_in_one_read` | Unit | Happy path (parameterized alongside split-across-reads case) |
| Story 5.2 AC2 error/edge path: bracketed paste containing collision bytes passes through unmodified | `crates/tymux-cli/src/input.rs` | `keystroke_reassembler_should_forward_paste_unmodified_when_pasted_bytes_match_a_binding_sequence` | Unit | Error/edge path |
| Story 5.2 AC3: escape hatch (`prefix prefix`) forwards literal byte | `crates/tymux-cli/src/input.rs` | `prefix_state_should_forward_literal_leader_byte_and_return_to_idle_when_leader_pressed_twice` | Unit | Happy path |
| Story 5.3 AC1: `Detach` action cancels stream, pane stays live | `crates/tymux-cli/src/main.rs` | `detach_action_should_fully_cancel_attach_call_while_remote_pane_keeps_running` | Integration | CLI attach loop + daemon round trip, `tymux ls` post-detach check |
| Story 5.3 AC2: `SplitHorizontal` dispatches to same RPC path as CLI subcommand | `crates/tymux-cli/src/main.rs` | `split_horizontal_action_should_call_split_pane_rpc_via_same_path_as_split_subcommand` | Unit | Happy path — no duplicated RPC logic |
| Story 5.3 error path: action dispatch on invalid state (e.g. split with no focused pane) | `crates/tymux-cli/src/main.rs` | `split_action_should_no_op_gracefully_when_no_pane_currently_focused` | Unit | Error path |
| Story 5.4 AC1: offset-aware `CapturePane` returns historical content | `crates/tymux-core/src/pane.rs` | `capture_pane_should_return_historical_grid_when_scrollback_offset_specified` | Unit | Happy path |
| Story 5.4 AC2: global scrollback ceiling evicts oldest-inactive pane | `crates/tymux-core/src/pane.rs` | `scrollback_ceiling_should_evict_oldest_inactive_pane_when_global_budget_exceeded` | Unit | Error/edge path (resource exhaustion handling) |
| Story 5.4: `SearchScrollback` RPC | `crates/tymuxd/src/main.rs` | `search_scrollback_rpc_should_return_matching_line_range_when_pattern_present` | Integration | tonic in-process round trip |
| Story 5.4 error path: `SearchScrollback` no-match case | `crates/tymuxd/src/main.rs` | `search_scrollback_rpc_should_return_no_matches_when_pattern_absent` | Unit | Error path |
| Story 5.5 AC1: copy-mode navigation moves offset without forwarding keys to pane | `crates/tymux-cli/src/copy_mode.rs` | `copy_mode_should_move_scrollback_offset_without_forwarding_keystrokes_when_navigating` | Unit | Happy path |
| Story 5.5 AC2: select-then-copy-then-paste round trip | `crates/tymux-cli/src/copy_mode.rs` | `copy_mode_should_copy_selected_range_into_buffer_when_visual_select_then_yank` | Integration | Enter/navigate/select/copy/paste full round trip |
| Story 5.5 AC3: dead-pane copy-mode shares identical exit path | `crates/tymux-cli/src/copy_mode.rs` | `copy_mode_exit_key_should_behave_identically_when_pane_is_dead_versus_live` | Unit | Error/edge path — no special-cased dead-pane exit branch |

### Epic 6 — Status bar

| Requirement | Test File | Test Name | Type | Scenario |
|---|---|---|---|---|
| Story 6.1 AC1: `StatusBarModel` is gRPC-introspectable, no ANSI scraping needed | `crates/tymuxd/src/main.rs` | `status_bar_model_rpc_should_return_structured_data_reflecting_two_attached_clients` | Integration | tonic in-process round trip |
| Story 6.1 error path: model reflects zero clients when none attached | `crates/tymuxd/src/main.rs` | `status_bar_model_rpc_should_return_zero_attached_client_count_when_none_attached` | Unit | Error/edge path |
| Story 6.2 AC1: attach reserves row via `Resize(rows-1)` + `DECSTBM` | `crates/tymux-cli/src/status_bar.rs` | `attach_should_send_resize_for_rows_minus_one_when_status_bar_enabled` | Unit | Happy path |
| Story 6.2 AC2: SIGWINCH triggers coordinated pty-resize + status-bar repaint | `crates/tymux-cli/src/status_bar.rs` | `sigwinch_should_coordinate_pty_resize_and_status_bar_repaint_as_single_update` | Integration | Simulated resize event, assert no interleaved-write frame |
| Story 6.2: `--no-status-bar` skips scroll-region reservation entirely | `crates/tymux-cli/src/status_bar.rs` | `attach_should_send_full_rows_and_skip_decstbm_when_no_status_bar_flag_set` | Unit | Error/edge path (accessibility floor) |
| **Story 6.3 AC1: single-owner stdout writer** | `crates/tymux-cli/src/main.rs` | `attach_loop_should_route_all_stdout_writes_through_single_owning_task_never_directly` | Unit | Structural test — asserts no direct `stdout.write_all()` call site exists outside the owner (grep-based static assertion in test, per ADR-006) |
| Story 6.4 AC1: prefix-armed redraw shows live binding table | `crates/tymux-cli/src/status_bar.rs` | `status_bar_should_render_full_binding_table_when_prefix_state_armed` | Unit | Happy path |
| Story 6.4 AC2: copy-mode redraw shows copy-mode's own key set | `crates/tymux-cli/src/status_bar.rs` | `status_bar_should_render_copy_mode_key_set_when_input_mode_is_copy_mode` | Unit | Happy path |
| Story 6.4 error path: no stale hint text lingers after mode change | `crates/tymux-cli/src/status_bar.rs` | `status_bar_should_not_render_stale_prefix_hints_after_mode_reverts_to_normal` | Unit | Error/edge path |
| Story 6.4: manual vim-compatibility spike | (manual) | `status_bar_should_not_corrupt_output_when_vim_resets_scroll_region_itself` | Manual/scripted-terminal | Run `vim` inside an attached, status-bar-enabled session; visually confirm no corruption |

### Epic 7 — Cross-language TypeScript client

| Requirement | Test File | Test Name | Type | Scenario |
|---|---|---|---|---|
| Story 7.1 AC1: `buf generate` produces clean TS types, `npm run build` succeeds | `clients/ts/` (build pipeline) | `buf_generate_should_produce_ts_types_that_compile_without_manual_edits` | Integration | CI step: `buf generate && npm install && npm run build` |
| Story 7.1: CI drift check on generated directory | `.github/workflows/ci.yml` | `buf_generate_drift_check_should_fail_when_generated_ts_differs_from_committed` | Integration | `buf generate && git diff --exit-code` |
| **Story 7.2 AC1 — Epic 7's Stage 1 gate: unary RPC validation** | `clients/ts/examples/list-sessions.ts` + smoke test | `ts_client_list_sessions_should_match_rust_cli_output_for_same_daemon_state` | Integration | Node smoke test against running `tymuxd`; asserts parity with `tymux ls` |
| Story 7.2 error path: TS client against unreachable daemon | `clients/ts/` smoke test | `ts_client_list_sessions_should_surface_connection_error_when_daemon_unreachable` | Unit | Error path |
| **Story 7.3 AC1 — Epic 7's Stage 2 gate: bidi-stream `Attach` validation (separate, required, not folded into Stage 1)** | `clients/ts/examples/attach.ts` + integration test | `ts_client_attach_should_observe_echoed_output_and_leave_pane_live_after_full_cancellation` | Integration | TS client bidi `attach()`: send `pane_id`, send input, read output, fully cancel; assert `tymux ls`/`ListSessions` shows pane still live. **Per plan.md §5: "Epic 7's `Attach` validation cannot be marked done on the strength of the unary checkpoint alone — the bidi-stream checkpoint is a separate, required gate."** |
| Story 7.3 AC2: TS client `capturePane` completes all 3 named RPCs | `clients/ts/examples/capture-pane.ts` | `ts_client_capture_pane_should_return_nonempty_snapshot_reflecting_live_screen_content` | Integration | Completes `CreateSession`/`Attach`/`CapturePane` coverage per `requirements.md` Success Metric #3 |
| Story 7.3 AC3 (fallback branch): bidi-streaming proves unworkable | `clients/ts/README.md` + `requirements.md` (documentation, not code) | `attach_fallback_should_be_documented_as_rust_client_only_when_bidi_streaming_unworkable` | Manual/scripted-terminal | If AC1 investigation fails: verify `clients/ts/README.md`, main `README.md` Known Limitations, and `requirements.md` Success Metric #3 are all updated to state the actually-achieved scope — this is the explicit decision-tree deliverable, not silent abandonment |
| Story 7.4 AC1: new contributor can follow `clients/ts/README.md` quick-start unaided | `clients/ts/README.md` | `ts_client_readme_quickstart_should_run_unary_and_attach_examples_without_reading_rust_source` | Manual/scripted-terminal | Fresh-eyes walkthrough of the quick-start against a locally running `tymuxd` |

### Epic 8 — Release pipeline

| Requirement | Test File | Test Name | Type | Scenario |
|---|---|---|---|---|
| Story 8.1 AC1: tag push produces 4-target binary matrix | `.github/workflows/release.yml` | `release_workflow_should_upload_four_target_archives_when_tag_pushed` | Integration | Push `v1.0.0-alpha.N` tag, assert GitHub Release contains all 4 target archives |
| Story 8.1 error path: musl cross-compile failure on release matrix | `.github/workflows/release.yml` | `release_workflow_should_fail_loudly_when_portable_pty_fails_to_link_against_musl` | Integration | Confirms failures surface, not silently produce broken binaries |
| Story 8.2 AC1: README Status section accuracy | `README.md` (manual review) | `readme_status_section_should_accurately_reflect_shipped_v1_feature_set` | Manual/scripted-terminal | Cross-check every claim (splits, Tier-0 persistence, Node-only TS client, loopback trust) against actual behavior |
| **Story 8.3 AC1 — final release gate: manual end-to-end verification against tagged binaries** | `docs/reviews/v1-release-verification.md` (checklist) | `v1_0_0_release_binaries_should_pass_full_manual_verification_checklist` | Manual/scripted-terminal | Downloaded (not `cargo run`) binaries: split a session, kill+restart daemon, revive, attach TS client, use copy-mode, observe status bar — every `requirements.md` in-scope item verified working end-to-end |

---

## Migration Test

| Requirement | Test File | Test Name | Type | Scenario |
|---|---|---|---|---|
| plan.md §3: breaking `Window.panes`→`Window.layout` proto change must not silently break the existing regression suite | `crates/tymuxd/src/main.rs`, `crates/tymux-core/src/pane.rs` | `proto_breaking_change_should_not_break_existing_regression_tests` | Migration | After the Epic 3 Story 3.3/3.4 `Window.layout` migration lands, confirm `attach_streams_output_and_signals_exit` (`crates/tymuxd/src/main.rs`) and `wait_exit_resolves_after_child_exits` (`crates/tymux-core/src/pane.rs`) — the two regression tests already in the codebase from the is-it-ready fix pass — still compile and pass unmodified in behavior (their assertions, not just their names, must still hold). Run this as an explicit CI check on the Epic 3 PR, not assumed by omission. Since this is pre-1.0 with zero external consumers, "migration reversibility" here means exactly this: proof that the one-shot breaking proto change didn't regress already-fixed behavior, not a DB up/down migration. |

---

## UX Acceptance Tests

All UX-AC and UX-A11Y criteria are human-verifiable CLI scenarios. **Tool note**: tymux is a terminal passthrough CLI with no browser/GUI surface — Playwright/browser automation does not apply here. The tool for all UX verification is manual interactive terminal use, optionally scripted via a pty-driving harness (e.g. a test wrapping `portable-pty` or `expect`-style byte-level assertions on captured output) where the check can be made deterministic; genuinely visual/subjective checks (redraw timing, absence of corruption) stay manual.

### Splits

| UX Criterion | Test File | Test Name | Tool | Steps |
|---|---|---|---|---|
| UX-AC-01 | (manual/scripted) | `split_should_focus_new_pane_within_two_keystrokes_or_one_command` | Manual/scripted-terminal | Attach, press `Ctrl-b %`; separately run `tymux split <target> --horizontal`; confirm new pane focused in both paths |
| UX-AC-02 | (manual/scripted) | `pane_focus_cycle_should_return_to_original_pane_after_n_presses_for_n_panes` | Manual/scripted-terminal | Create N panes, press `prefix o` N times, assert focus is back on the original pane |
| UX-AC-03 | (scripted, byte-level assertable) | `undersized_split_should_show_exact_dimensions_and_leave_layout_unchanged` | Manual/scripted-terminal | Resize terminal below minimum, attempt split, capture `tymux ls`/`CapturePane` before and after, assert byte-identical |
| UX-AC-04 | (manual/scripted) | `closing_sibling_pane_should_collapse_split_and_move_focus_automatically` | Manual/scripted-terminal | Split, kill one pane via `prefix x`, confirm survivor is focused with no orphaned split node (cross-check via `tymux ls`) |

### Detach

| UX Criterion | Test File | Test Name | Tool | Steps |
|---|---|---|---|---|
| UX-AC-05 | (manual/scripted) | `detach_should_restore_terminal_within_one_render_frame_using_two_keystrokes` | Manual/scripted-terminal | Attach, press `Ctrl-b d`, confirm raw mode is off and prompt returns immediately |
| UX-AC-06 | (scripted) | `tymux_ls_should_show_detached_live_status_immediately_after_detach` | Manual/scripted-terminal | Detach, immediately run `tymux ls`, assert `○ detached, live` (never absent, never stale `attached`) |
| UX-AC-07 | (scripted, byte-level assertable) | `detach_message_should_be_textually_distinct_from_pane_exited_message` | Manual/scripted-terminal | Trigger detach and, separately, a real pane exit; diff the two printed messages for distinctness |

### Copy-mode

| UX Criterion | Test File | Test Name | Tool | Steps |
|---|---|---|---|---|
| UX-AC-08 | (scripted) | `copy_mode_navigate_then_exit_should_have_zero_effect_on_live_pane_state` | Manual/scripted-terminal | Enter copy-mode, navigate, exit; re-enter and diff content against a pre-entry capture |
| UX-AC-09 | (scripted) | `copy_mode_exit_key_should_work_identically_for_live_and_dead_pane` | Manual/scripted-terminal | Enter copy-mode on a live pane and, separately, a dead pane; confirm `q`/`Escape` behave identically in both |
| UX-AC-10 | (manual/scripted) | `copy_mode_select_copy_paste_should_complete_in_four_actions_plus_one_paste_keystroke` | Manual/scripted-terminal | Enter, `v`, move, `y`, then one paste keystroke in Normal mode; confirm pasted text matches selection |
| UX-AC-11 | (scripted) | `copy_mode_should_show_exited_marker_within_one_render_frame_on_dead_pane_entry` | Manual/scripted-terminal | Enter copy-mode on a dead pane, capture first render frame, assert `[exited]` marker present |

### Status bar

| UX Criterion | Test File | Test Name | Tool | Steps |
|---|---|---|---|---|
| UX-AC-12 | (manual/scripted) | `status_bar_should_show_complete_binding_table_while_prefix_armed` | Manual/scripted-terminal | Press `Ctrl-b`, capture status bar row, assert all ~8-10 bindings listed |
| UX-AC-13 | (scripted) | `status_bar_should_transition_modes_within_one_redraw_cycle_of_keystroke` | Manual/scripted-terminal | Trigger mode change, capture consecutive redraw frames, assert no stale-hint frame lingers |
| UX-AC-14 | (scripted, byte-level assertable) | `no_status_bar_and_no_color_should_emit_zero_ansi_and_decstbm_bytes` | Manual/scripted-terminal | Pipe attach output to a file with `--no-status-bar` and `NO_COLOR=1`, inspect raw bytes for absence of escape sequences |
| UX-AC-15 | (scripted) | `status_bar_liveness_and_mode_indicators_should_retain_meaning_under_no_color` | Manual/scripted-terminal | Render with `NO_COLOR=1`, confirm every liveness/mode indicator still carries symbol+text, no information loss |

### Config

| UX Criterion | Test File | Test Name | Tool | Steps |
|---|---|---|---|---|
| UX-AC-16 | (scripted) | `tymux_should_start_with_zero_warnings_when_no_config_file_present` | Manual/scripted-terminal | Start with no `~/.config/tymux/config.toml`, capture stdout/stderr, assert empty of warnings |
| UX-AC-17 | (scripted) | `single_overridden_binding_should_not_affect_unrelated_default_bindings` | Manual/scripted-terminal | Override `detach` only, confirm an unrelated default (e.g. split) still works unmodified |
| UX-AC-18 | (scripted) | `malformed_config_should_start_with_defaults_and_show_one_actionable_line` | Manual/scripted-terminal | Write invalid TOML, start tymux, assert it starts (defaults applied) and exactly one line names the file + parse problem |

### Session persistence

| UX Criterion | Test File | Test Name | Tool | Steps |
|---|---|---|---|---|
| UX-AC-19 | (scripted) | `tymux_ls_should_render_all_three_liveness_states_with_distinct_symbol_and_wording` | Manual/scripted-terminal | Produce one session in each of attached/detached-live/restored-dead states, assert `tymux ls` renders all three distinctly |
| UX-AC-20 | (scripted) | `revive_should_confirm_pane_count_and_new_process_status_in_two_commands` | Manual/scripted-terminal | `tymux ls` then `tymux revive <id>`, assert revive's own output states pane count and "new processes" wording |
| UX-AC-21 | (scripted) | `attach_to_dead_session_should_fail_immediately_naming_revive_remediation` | Manual/scripted-terminal | Attempt `tymux attach` on a dead/restored session, assert immediate failure (no hang/blank screen) naming `tymux revive <id>` |
| UX-AC-22 | (scripted) | `revive_on_already_live_session_should_respond_with_friendly_no_op_message` | Manual/scripted-terminal | `tymux revive` on a live session, assert non-destructive response pointing at `tymux attach` |
| UX-AC-23 | (scripted) | `corrupted_persisted_file_should_never_block_daemon_start_or_other_valid_sessions` | Manual/scripted-terminal | 1 corrupt + N valid persisted files, start daemon, assert `tymux ls` shows exactly N sessions |

### General / cross-cutting

| UX Criterion | Test File | Test Name | Tool | Steps |
|---|---|---|---|---|
| UX-AC-24 | (scripted) | `every_v1_mode_should_have_at_least_one_always_working_documented_exit_key` | Manual/scripted-terminal | For prefix-armed and copy-mode, confirm `Escape`/`q` exits with no external help needed |
| UX-AC-25 | (scripted) | `every_new_v1_error_case_should_route_through_friendly_message_never_raw_debug_dump` | Manual/scripted-terminal | Trigger each of undersized split, dead-pane attach/split, malformed config, revive-already-live; assert no raw `anyhow`/`tonic::Status` Debug text appears |
| UX-AC-26 | (scripted) | `every_constraint_naming_error_should_include_actual_numbers_or_state_not_just_invalid` | Manual/scripted-terminal | Cross-check each size/liveness error message for concrete numbers/state, not vague wording |

### Accessibility

| UX Criterion | Test File | Test Name | Tool | Steps |
|---|---|---|---|---|
| UX-A11Y-01 | (manual) | `full_v1_feature_set_should_be_operable_via_keyboard_alone` | Manual/scripted-terminal | Exercise splits/copy-mode/revive/config using only keyboard input; regression guard, trivially true for CLI but stated explicitly |
| UX-A11Y-02 | (scripted, byte-level assertable) | `no_color_env_should_suppress_all_ansi_while_retaining_liveness_mode_text` | Manual/scripted-terminal | `NO_COLOR=1`, capture output bytes, assert no ANSI color codes and all text/symbol info intact |
| UX-A11Y-03 | (scripted, byte-level assertable) | `no_status_bar_flag_should_suppress_decstbm_and_execute_zero_partial_redraw_logic` | Manual/scripted-terminal | `--no-status-bar`, capture output bytes, assert no `DECSTBM` sequences; additionally instrument (debug log/counter) that partial-redraw code path is never invoked, not merely invisible |
| UX-A11Y-04 | (scripted, byte-level assertable) | `status_bar_and_copy_mode_redraw_should_never_overwrite_child_process_emitted_bytes` | Manual/scripted-terminal | Run a program producing known output while status bar/copy-mode redraw; diff captured stream against expected append-only child output |
| UX-A11Y-05 | (scripted) | `every_introduced_mode_should_be_escapable_using_only_escape_or_q` | Manual/scripted-terminal | For each new mode, confirm no mode-specific exit sequence beyond `Escape`/`q` is required |
| UX-A11Y-06 | (manual/scripted) | `docs_should_include_explicit_accessibility_section_stating_supported_and_unsupported` | Manual/scripted-terminal | Review `README.md`/docs for an explicit Accessibility section; absence is treated as a failing check, not neutral |

---

## Test Stack
- **Unit**: `cargo test` (built-in), `proptest` for `LayoutNode` invariants (Story 3.2's hard merge gate)
- **Integration**: `cargo test` with test doubles (`SlowMockPersistenceBackend`, a `SlowMockPane`/mock resize test double for Story 3.4 AC3, mirroring the same pattern), `tonic` in-process server tests (see existing `attach_streams_output_and_signals_exit` pattern in `crates/tymuxd/src/main.rs`); real daemon-process kill/restart tests for the Epic 4 persistence gate; Node smoke tests (`clients/ts/`) against a running or CI-started `tymuxd`
- **E2E / UX**: manual verification checklist (CLI tool, no browser surface — Playwright/browser automation is explicitly not applicable) + TS client integration tests (Node) for the cross-language proof + a byte-level scripted-terminal harness (pty-driving or `expect`-style) for the UX criteria that can be made deterministic (ANSI/DECSTBM presence, message text, exit codes)

## Coverage Targets and How to Measure

| Stack | Coverage command | Target |
|---|---|---|
| Rust | `cargo tarpaulin --out Stdout` | ≥80% line |
| TypeScript (`clients/ts`) | `npx vitest --coverage` or equivalent | best-effort, not gated |

- All public service methods: happy path + error paths covered
- All external integrations: unit mocked + at least one integration test
- UX acceptance criteria: each of `ux.md`'s 32 criteria (26 UX-AC + 6 UX-A11Y) has a corresponding test or manual step above
- **Non-negotiable gates called out explicitly** (plan.md §5 Risk Control, reproduced here so Phase 5 cannot silently drop them):
  1. `layout_node_invariants_should_hold_after_any_sequence_of_split_remove_resize_ops` (property-based, Story 3.2) — Epic 3 cannot merge without it.
  2. `session_layout_should_match_pre_restart_shape_when_daemon_is_killed_and_restarted_mid_session` (Story 4.4/4.x) — Epic 4 cannot merge without it.
  3. `ts_client_list_sessions_should_match_rust_cli_output_for_same_daemon_state` (Story 7.2, unary) **and** `ts_client_attach_should_observe_echoed_output_and_leave_pane_live_after_full_cancellation` (Story 7.3, bidi) — Epic 7's `Attach` validation is not done on the unary gate alone; both must pass (or the documented AC3 fallback must be exercised and recorded).
  4. `proto_breaking_change_should_not_break_existing_regression_tests` (Migration, Epic 3) — the two existing regression tests (`attach_streams_output_and_signals_exit`, `wait_exit_resolves_after_child_exits`) must keep passing through the `Window.layout` migration.
