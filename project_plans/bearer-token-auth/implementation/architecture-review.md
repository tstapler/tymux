# Architecture Review: bearer-token-auth
**Date**: 2026-08-28
**Verdict**: CONCERNS → **PASS** (resolved 2026-08-29)

**Resolution note**: all three Concerns below were fixed by commits `7de8ebd`
and `827f72d` — verified directly, not just claimed: `crates/tymuxd/src/auth.rs`
module exists (Concern 2), `BearerToken` newtype exists in both
`crates/tymuxd/src/auth.rs:19` and `crates/tymux-cli/src/main.rs:291`
(Concern 1), and `clients/go/authinterceptor/authinterceptor.go` is a shared
package imported by all three former duplication sites — confirmed via
`grep` that no site still defines its own `authInterceptor` type (Concern 3).
Checkboxes below marked done retroactively; the doc just wasn't updated when
the fixes landed.

## Constitution Check

`docs/adr/ADR-000-architecture-constitution.md` does not exist in this repository
(checked at `~/Programming/tymux/docs/adr/` — only `ADR-001-constant-time-eq-crate.md`
and `ADR-002-tymuxd-token-flag-parsing.md` exist under `project_plans/bearer-token-auth/decisions/`,
and there is no `docs/adr/` directory at all). No constitution to check against — this
section is skipped as instructed.

## Blockers

None. Nothing in the plan, as specified, produces broken or insecure code against its
own acceptance criteria — the concerns below are about latent risk and maintainability,
not currently-broken behavior.

## Concerns

- [x] **Story 1.1.2/1.1.3/1.2.1 — token is a raw `String`/`Option<String>` everywhere;
  "empty is absent" is a runtime filter in one function, not a type-level guarantee.**
  `resolve_token` (Task 1.1.2a) is the *only* place that enforces "empty string counts as
  absent" (`.filter(|t| !t.is_empty())`). `check_non_loopback_requires_token` (Task 1.1.3a)
  receives `Option<&str>` and only checks `.is_none()` — it does **not** independently
  reject `Some("")`. If any future call site ever constructs `configured_token =
  Some("".to_string())` without going through `resolve_token` — and the plan's own
  Unresolved Questions section already earmarks exactly this shape of change
  (`TYMUXD_TOKEN_FILE`, "only `resolve_token` would need a third source checked") — the
  gate silently treats the daemon as configured-and-secure while
  `BearerAuthInterceptor`'s `constant_time_eq(supplied.as_bytes(), "".as_bytes())` would
  accept any client presenting `authorization: Bearer ` (empty value) as valid, i.e. a full
  non-loopback auth bypass on the exact RCE surface this feature exists to close. Today's
  design is safe only because there is exactly one call site and it is well-tested (Tasks
  1.1.2b/1.1.3c) — the invariant is "parse, don't validate" in spirit but not in the type.
  This is also the answer to the plan's own type-driven-design question (task point 5):
  a `BearerToken` newtype is *not* over-engineering here — it's the direct fix for this
  finding, not a separate concern.
  **Remediation**: introduce a newtype in `crates/tymuxd/src/main.rs` (and mirrored in
  `crates/tymux-cli/src/main.rs`):
  ```rust
  #[derive(Clone)]
  struct BearerToken(String); // no derive(Debug), no derive(PartialEq/Eq) — see below

  impl std::fmt::Debug for BearerToken {
      fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
          write!(f, "<redacted>")
      }
  }

  impl BearerToken {
      /// Smart constructor: the ONLY way to produce a `BearerToken` is through
      /// this parse, which makes "empty token" unrepresentable downstream.
      fn parse(raw: &str) -> Option<Self> {
          (!raw.is_empty()).then(|| Self(raw.to_string()))
      }
      fn as_bytes(&self) -> &[u8] { self.0.as_bytes() }
  }
  ```
  Change `resolve_token` to return `Option<BearerToken>` (calling `BearerToken::parse` at
  the point it currently calls `.filter(...)`), change
  `check_non_loopback_requires_token(is_loopback: bool, token: Option<&BearerToken>)`, and
  change `BearerAuthInterceptor::new`/`BearerAuth`'s field type to `BearerToken`. This
  merges two of the plan's stated risks (primitive obsession, illegal empty-token state)
  into one small type, and makes the manual "grep for token leaks" step (Task 1.2.1c)
  partially redundant — a `Debug`-derived struct containing `BearerToken` can no longer
  leak the value through `{:?}` formatting, only through explicit misuse of `.0`.
  **Deliberately do not derive `PartialEq`/`Eq`** on `BearerToken`: a derived `==` would be
  a second, non-constant-time equality path sitting right next to the required
  `constant_time_eq` call, and it's exactly the kind of accidental substitution ("looks
  the same, isn't constant-time") ADR-001 already warns about for hand-rolled compares.

- [x] **`crates/tymuxd/src/main.rs` is already 4,511 lines with no domain/infra module
  split, and this plan adds another ~150 lines of auth-specific structs, functions, and
  tests to it (Epics 1.1/1.2/1.4 — all `Files: crates/tymuxd/src/main.rs`).** This is a
  pre-existing condition, not introduced by this feature, and the plan's own reasoning for
  keeping the token as a plain field on `TymuxDaemon`-adjacent state (not
  `Arc<Mutex<String>>`) correctly keeps auth out of `Engine`/`TymuxDaemon`'s business logic
  — that boundary is respected. But every new type in this plan
  (`BearerAuthInterceptor`, `resolve_token`, `check_non_loopback_requires_token`) is placed
  in the same single file as connection handling, session engine wiring, and ~10 RPC
  handler impls, growing an already-hard-to-navigate god-file rather than carving out even
  a `mod auth;` submodule. Not a correctness problem for this feature, but it compounds
  the next feature's cost (scoped tokens / per-session ownership are explicitly on the
  roadmap as immediate follow-ups).
  **Remediation**: extract `resolve_token`, `check_non_loopback_requires_token`, and
  `BearerAuthInterceptor` (plus their unit tests) into `crates/tymuxd/src/auth.rs` /
  `mod auth;`, imported into `main.rs` at the two call sites (startup gate,
  server-construction branch). Zero behavior change, but stops main.rs from absorbing yet
  another concern wholesale, and gives the next auth-adjacent feature (scoped tokens) an
  obvious place to land instead of the same file.

- [x] **Epic 3.1 (Go client) duplicates the entire `authInterceptor` implementation three
  times with no shared package** — Task 3.1.1a defines it in
  `clients/go/integration/integration_test.go`; Task 3.1.1e says to "apply the same
  `authInterceptor` type... to `clients/go/examples/list-sessions/main.go` and
  `clients/go/examples/attach/main.go`" — i.e., copy-paste the same ~20-line struct + 3
  methods (with the same pinned-source comment) into three separate files. `clients/go`
  has no hand-written library package today (confirmed:
  `find clients/go -maxdepth 2 -type d` shows only `bin/`, `gen/` (generated),
  `examples/`, `integration/` — no `pkg/`/`client/` package), so this duplication is partly
  forced by existing repo structure, not newly introduced by this plan. But it directly
  works against the plan's own stated Pattern Decision — "build one thin reusable
  constructor per language" (build-vs-buy.md §4, Pattern Decisions row) — and is exactly
  the copy-paste-across-sibling-implementations failure mode the
  `code-architecture-best-practices` reuse check names as the single highest-value thing
  to catch before it ships three times: the next fix to `WrapStreamingClient`'s behavior
  (or any bug in it) needs to land in three places, with three chances to diverge.
  **Remediation**: since this plan already touches all three call sites, use it as the
  point to create `clients/go/authinterceptor/authinterceptor.go` (small standalone
  package) containing `authInterceptor` once, and have Task 3.1.1a's test file and Task
  3.1.1e's two example mains import it. TypeScript already avoids this — `authInterceptor`
  lives once in `clients/ts/examples/client.ts` and is reused via the exported
  `tymuxClient()` factory from both `attach.ts` and `integration.test.ts` (Tasks
  3.2.1a/3.2.1d) — Go's plan should match that shape rather than the Rust
  `tymuxd`/`tymux-cli` split (which is justified there because they're genuinely different
  binaries with different `Interceptor` trait implementations, not copies of the same one).

## Nitpicks

- Story 1.2.2a's two `Server::builder()` arms (with-token / without-token) duplicate the
  `.http2_keepalive_interval(...)`/`.http2_keepalive_timeout(...)` configuration verbatim.
  The plan explicitly chose this over type-erasure via `Router`/`BoxCloneService` for
  readability, which is a reasonable call for two arms — but if either keepalive value
  ever changes, both arms must be updated in lockstep with no compiler help. Low risk at
  this size; worth a one-line comment ("keep both arms' keepalive settings in sync") if not
  worth the `Router` abstraction.
- Constructor-API inconsistency between the two nearly-identical interceptor types:
  `BearerAuthInterceptor::new(token, rejection_count)` (Task 1.2.1a) is a proper
  constructor function, while `BearerAuth` (Task 2.1.2a) is constructed via bare struct
  literal (`BearerAuth { token: cli.token.clone() }`, Task 2.1.2b). Both are
  private/module-local types with no external consumers, so this isn't a real API-contract
  risk (point 10 in the task brief is otherwise low-stakes here — none of the four
  interceptor types are exported public API, they're all binary-internal or
  test/example-internal), but a `BearerAuth::new(token: Option<BearerToken>) -> Self`
  constructor would match the server-side convention and give the (recommended)
  `BearerToken` newtype one canonical construction path.
- `resolve_token` and `check_non_loopback_requires_token` remain two separate pure
  functions a caller must remember to chain in the right order (task point 7). Given the
  `BearerToken` remediation above closes the actual illegal-state gap, and both functions
  live five lines apart in one straight-line `main()` with dedicated integration tests
  proving the composition (Task 1.1.3c), this is a low-priority follow-up: consider folding
  them into one `fn establish_auth(is_loopback: bool, args: &[String]) -> Result<Option<BearerToken>, String>`
  so a future refactor of `main()` can't accidentally reorder or drop the gate call — but
  it's not worth blocking on given the current single-call-site risk is already low.
- `BearerAuthInterceptor::call`'s three test cases (Task 1.2.1b) construct
  `Request::new(())` directly, which has no `remote_addr()` — meaning the "peer" logging
  path (`req.remote_addr().map(...).unwrap_or_else(|| "unknown")`) is only ever exercised
  via the `"unknown"` fallback in unit tests, never the real-address branch. The
  integration tests (Task 1.2.2c/d) go through a real bound socket, so the address-present
  path is covered end-to-end, just not asserted on directly (no test checks the logged
  `peer` field actually contains a real address rather than `"unknown"`). Minor test-gap,
  not a design issue.

## Positive notes (not findings, worth recording)

- Interceptor pattern choice (task point 9) is well-justified: Step 0.5's Alternatives
  Considered reads pinned `tonic 0.12.3` source directly rather than trusting docs, and
  documents the one real trade-off (no RPC-method-name-in-logs) as a scoped, explicit
  Unresolved Question rather than silently dropping it. This is the kind of pattern
  justification the `design-patterns` skill asks for — solving a recurring problem
  (single, un-skippable enforcement point across ~10 handlers + a bidi stream) rather than
  pattern-matching for its own sake.
- Token storage as a plain owned `String`/`BearerToken` field (no `Arc<Mutex<...>>`) on
  the interceptor, kept entirely out of `Engine`/`TymuxDaemon`, correctly respects the
  Hexagonal/Clean-Architecture boundary: auth is a transport-adapter concern that never
  reaches business logic, and the plan explicitly cites the existing
  `disconnect_regression_window`/`grace_period_duration` precedent for this shape rather
  than inventing a new one.
- `constant_time_eq` over `subtle` (ADR-001) and hand-rolled `--token` parsing over `clap`
  for `tymuxd` (ADR-002) both match `research/build-vs-buy.md`'s recommendations and are
  followed consistently in the actual task list (Story 1.1.1, Story 1.1.2) — no
  build-vs-buy drift found there.
