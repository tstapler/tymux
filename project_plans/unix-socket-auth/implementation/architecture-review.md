# Architecture Review: unix-socket-auth
**Date**: 2026-08-29 (iteration 3 — final)
**Verdict**: CLEAN (0 blockers, 0 concerns, 2 nitpicks)

## Blockers

None. The one iteration-2 Blocker is resolved and independently
re-verified:

- **`bind_uds_listener` unconditional `chmod` of `socket_path.parent()`**
  (was Blocker). Fixed: Task 2.2.1a (`plan.md:858-932`) now guards the
  mutation with `if !parent.exists() { create_dir_all(...); \
  set_permissions(...); }` (`plan.md:893-897`), so "never `chmod` a
  directory `tymuxd` doesn't itself own" is an invariant of the function
  for *any* `socket_path` input, not just the nested default — verified
  by reading the actual code, not just the task description.
  - **New test, not a rename**: Task 2.2.1b's test list
    (`plan.md:934-951`) now has both
    `bind_uds_listener_never_touches_permissions_of_a_pre_existing_grandparent_directory`
    (the original, pre-existing test — nested-default shape, parent is
    two levels below a pre-existing grandparent) and a genuinely new,
    distinct test,
    `bind_uds_listener_never_touches_permissions_of_a_pre_existing_un_nested_parent_directory`
    (the socket path's *immediate* parent is the pre-existing directory,
    exercising the un-nested-override shape the blocker was about) — its
    own AC is spelled out separately at `plan.md:823-839`, confirming
    this is new coverage, not the old test relabeled.
  - **Five stale example paths corrected**: spot-checked `grep -rn
    'XDG_RUNTIME_DIR/tymuxd\.sock'` (the un-nested form) across
    `requirements.md` and `research/*.md` — zero remaining hits in those
    files. All five previously-cited locations now read the corrected
    nested form:
    - `requirements.md:97` → `` `$XDG_RUNTIME_DIR/tymuxd/tymuxd.sock` ``
    - `research/stack.md:113` → `` `$XDG_RUNTIME_DIR/tymuxd/tymuxd.sock` ``
    - `research/pitfalls.md:24,211` → both now `tymuxd/tymuxd.sock`
    - `research/ux.md:64,187` → both now `tymuxd/tymuxd.sock`
    - `research/features.md:139` → `tymuxd/tymuxd.sock`
    (The only remaining un-nested-string hits anywhere in the project are
    inside this review document itself, describing the historical bug,
    and inside `plan.md`'s own doc comments/AC text, which use the
    un-nested form only as the explicit *counter-example* the fix guards
    against — not as a recommended path.)
  - **Doc-comment caveats added**, closing the remediation's last item:
    `resolve_uds_socket_path`'s doc comment (`plan.md:507-516`) now notes
    an override should prefer a `tymuxd`-owned subdirectory (documentation
    nicety, not a safety requirement, since the function-level guard
    makes it safe either way), and `tymux-cli`'s `--socket-path` clap
    field doc comment (Task 6.1.1c, `plan.md:2035-2041`) carries the same
    caveat plus the containerized-uid-mismatch pointer.

## Concerns

None. The transient-`accept()`-error finding (adversarial-review.md
iteration-2 Concern) remains resolved by verification, and that
verification was independently re-confirmed here against the actual
crate source rather than trusting the plan's citation:
`~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/tonic-0.12.3/src/transport/server/mod.rs:617-652`,
inside `Server::serve_with_shutdown` (line 530), which
`Router::serve_with_incoming_shutdown` (line 874) calls directly
(`self.server.serve_with_shutdown(...)` at line 895). The accept loop's
`tokio::select!` arm reads exactly:
```rust
io = incoming.next() => {
    let io = match io {
        Some(Ok(io)) => io,
        Some(Err(e)) => {
            trace!("error accepting connection: {:#}", e);
            continue;
        },
        None => { break }
    };
    ...
}
```
— a `Some(Err(e))` item is `trace!`-logged and the loop `continue`s; only
`None` (stream exhaustion) breaks it. This matches the plan's citation
(Observability Plan, `plan.md:170-190`) verbatim. The claim is accurate
and complete: no further design change is needed for this finding. The
plan's own noted residual gap — tonic logs the dropped error only at
`trace!`, a level this project's default filter doesn't surface — is
correctly scoped as a documentation callout, not a correctness or design
issue, since the listener keeps accepting either way.

## Nitpicks

- (Carried forward, not addressed — optional, still optional)
  `crates/tymuxd/src/auth.rs` continues to grow across this plan
  (path/group/tcp-disable resolution, lock-file lifecycle, TOCTOU bind,
  gid resolution, group-membership decision, `PreAuthorizedUnixStream`,
  both interceptors) with no submodule grouping. Consider a private `mod
  socket_lifecycle` for the OS-plumbing functions, re-exported at
  `auth::`, once implementation is underway and the file's real shape is
  visible.
- (Carried forward, not addressed — optional) `socket_path`,
  `allowed_gid`, and `tcp_disabled` are still threaded through `main()`
  as three loose local variables (Tasks 4.2.1a/4.2.2a). A single
  `UdsStartupConfig { path, allowed_gid, tcp_disabled }` struct, resolved
  once early in `main()`, would reduce the parameter-passing surface —
  low priority, no correctness risk today.
