# ADR 0002: `portable-pty` links and runs cleanly on musl — spike result

## Status
Accepted (spike outcome recorded; `-musl` release targets confirmed viable)

## Context
Plan Decision #13 (`project_plans/v1-release/implementation/plan.md`)
commits Epic 8's Linux release binaries to
`x86_64-unknown-linux-musl`/`aarch64-unknown-linux-musl` static targets, to
avoid glibc-version coupling. Whether `portable-pty`'s C dependencies would
even statically link against musl was explicitly unvalidated risk
(`pitfalls.md` §7) — Epic 1 Story 1.4 required resolving this before any
later epic's work depended on it.

## Spike
A throwaway `spike-musl-pty` binary crate (single `portable-pty`
dependency, opens a pty, spawns `/bin/echo`, reads the output back) was:

1. Cross-compiled to `x86_64-unknown-linux-musl` via the `clux/muslrust:stable`
   Docker image — succeeded with no linker flags or workarounds needed.
2. Run inside a minimal `alpine:latest` container (non-glibc) — the static
   binary executed successfully, opened a real pty, and the child process's
   output (`musl-pty-ok`) was read back correctly.

## Decision
`portable-pty` is confirmed to link and run cleanly against musl. Plan
Decision #13's `-musl` release target choice stands as-is — the documented
`-gnu` fallback is not needed. Epic 1 Story 1.2's standing
`cargo build --workspace --target x86_64-unknown-linux-musl` CI job
(build-only, every PR) is the ongoing regression guard for this; the
throwaway `spike-musl-pty` crate itself is deleted after this result was
recorded, per the story's own instructions.
