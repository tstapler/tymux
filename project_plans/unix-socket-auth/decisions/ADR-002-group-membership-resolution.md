# ADR-002: Linux `/proc`-based supplementary-group check, primary-gid-only fallback elsewhere

**Status**: Accepted
**Date**: 2026-08-29

## Context

`requirements.md`'s Success Metrics call for "a configurable group grants
access to specific other local users... e.g. a shared service account
scenario where a small team needs access without being the same uid."
Operationally, granting a user access to a shared resource via a POSIX
group almost always means adding that user to the group as a
**supplementary** group (`usermod -aG <group> <user>`), not changing their
**primary** group — changing someone's primary group is disruptive (affects
every file they create) and is not how `docker`/similar groups are granted
in practice (`research/pitfalls.md` §6's own Docker-group precedent).

The kernel-verified identity source this feature is built on,
`SO_PEERCRED` (surfaced via `tokio::net::UnixStream::peer_cred()` /
`UCred`), reports only the connecting process's **uid and (primary/
effective) gid** — `UCred` has no supplementary-group list, because
`SO_PEERCRED`'s underlying `struct ucred` doesn't carry one. A naive
`peer.gid() == configured_gid` check would therefore only grant access to
someone whose *primary* group happens to be the configured group — missing
the exact "add a teammate to the group" scenario the requirement describes,
on the one platform (Linux) this project treats as primary.

## Decision

- **On Linux**, when `UCred::pid()` is available (documented as always
  `Some` on Linux), resolve full supplementary-group membership by
  reading `/proc/<pid>/status`'s `Groups:` line (a space-separated list of
  every gid the kernel has attached to that process, primary and
  supplementary alike) and checking whether the configured gid appears in
  it. This needs no new FFI surface — `/proc/<pid>/status` is a plain text
  file, read via `std::fs::read_to_string`.
- **Fallback (macOS, BSD, or a Linux sandbox that hides `/proc` — e.g.
  `hidepid=2`, or the read simply failing for any reason)**: fall back to
  `peer.gid() == configured_gid` — primary/effective group only. This is a
  narrower, not a less-safe, degradation: a legitimate group member whose
  membership is only supplementary is rejected (annoying, safe — matches
  `requirements.md`'s "fail toward more restrictive" posture) rather than
  an unintended party being accepted.
- This fallback behavior — "group access on macOS requires the configured
  group be the connecting user's *primary* group" — is documented
  explicitly in the `--socket-group` flag's help text and in the plan's
  Deployment Guidance, not left to be discovered by a confused operator.

## Alternatives Rejected

- **`libc::getgrouplist()` (resolve username via `getpwuid_r`, then the
  full group list) on every platform.** This is the fully portable,
  fully correct answer — it's what `getgrouplist(3)` exists for, and
  mirrors how PostgreSQL's own `peer` auth resolves group membership
  (`research/features.md` §2's closest industry precedent). Rejected for
  v1 as disproportionate *new* unsafe/FFI surface (two additional C calls
  with buffer-sizing/`errno` edge cases to get right, on a project whose
  own `research/architecture.md` already flagged "new unsafe/FFI surface
  this repo doesn't currently have" as a real cost) for a feature whose
  primary target platform (Linux, per `requirements.md`'s Constraints)
  already has a zero-new-FFI answer via `/proc`. Revisit if macOS
  supplementary-group parity is ever requested as its own follow-up.
- **`peer.gid() == configured_gid` on every platform, no `/proc` path.**
  Rejected outright: fails the requirement's own worked scenario (a
  teammate added to a shared group) on Linux, the platform this project
  is scoped to get fully right.

## Consequences

- New functions in `crates/tymuxd/src/auth.rs`: `peer_is_authorized`,
  `peer_is_group_member` (`#[cfg(target_os = "linux")]` variant reading
  `/proc/<pid>/status`, a fallback variant for everything else).
- Unit tests cover the parsing of a synthetic `Groups:` line and the
  membership-check logic directly; the `/proc` read itself is exercised
  against the *test process's own* pid (always resolvable, real data) —
  see the plan's Story on `UdsPeerCredInterceptor` for the exact test
  list.
- No new crate dependency.
