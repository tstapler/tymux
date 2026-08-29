# ADR-001: Use `constant_time_eq` for bearer-token comparison, not `subtle`

**Status**: Accepted
**Date**: 2026-08-28

## Context

`requirements.md`'s NFR states the token comparison "must be compared in
constant time (avoid a timing side-channel on a byte-by-byte compare)."
Two research agents reached different conclusions on which crate to use:

- `research/stack.md` §5 leans toward `subtle` (dalek-cryptography), citing
  it as "the community-standard crate for this."
- `research/build-vs-buy.md` §2 did a dedicated, deeper build-vs-buy
  comparison of both candidates and recommends `constant_time_eq` instead.

Neither hand-rolled compare is acceptable: `research/stack.md` §5's own
hand-rolled XOR-accumulate sketch is exactly the shape build-vs-buy.md
warns "looks constant-time" but risks the compiler reintroducing a branch
at higher optimization levels without an explicit barrier.

## Decision

Use `constant_time_eq = "0.5"` (crates.io, MIT/Apache-2.0/CC0, ~18.3M
downloads/month, used in 13,839 crates as of build-vs-buy.md's research —
`research/build-vs-buy.md` §2).

## Alternative Rejected: `subtle`

**Reason**: `subtle`'s `ConstantTimeEq`/`Choice`/`ConditionallySelectable`
API is a general framework for building larger constant-time algorithms
(curve arithmetic, conditional selection) — this project needs exactly one
operation (compare two byte strings). `constant_time_eq` is a single-
purpose crate modeled on the Linux kernel's `crypto_memneq`, doing that one
operation with less API surface to misuse, no further transitive
dependencies, and a download count showing it's exercised at least as
widely. `subtle` is not wrong — both correctly solve the actual timing-
side-channel risk — but it is more machinery than this feature's single
compare needs. `research/build-vs-buy.md` §2 is the more rigorous,
dedicated treatment of this specific decision; its analysis is followed
here over `research/stack.md`'s more general recommendation.

## Consequences

- `constant_time_eq = "0.5"` added to `crates/tymuxd/Cargo.toml`.
- Call site: `constant_time_eq::constant_time_eq(supplied.as_bytes(),
  configured.as_bytes())` — the crate handles unequal-length inputs safely
  (returns `false`, no panic), so no separate length pre-check is required
  before calling it.
