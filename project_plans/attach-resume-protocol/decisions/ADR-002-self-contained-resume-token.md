# ADR-002: Resume token is a self-contained `(pane_id, resume_from_seq)` pair, not server-tracked

**Status**: Accepted
**Date**: 2026-08-24
**Context**: attach-resume-protocol, Phase 3 planning

## Context

A reconnecting client needs to tell the daemon "I already have output through seq N" so the daemon can serve only what's missing. Two broad shapes exist for this in the industry (features.md's research):

- **Server-tracked identity**: the server remembers a session/client identity and its associated cursor, keyed by something the client presents on reconnect (e.g. a session ID it looks up).
- **Self-contained token**: the client presents everything needed to resume in the request itself, with no server-side per-client state to look up.

This codebase already has a keyed-by-identity tracker with a documented weakness: `disconnect_tracker: Arc<Mutex<HashMap<Uuid, Instant>>>` (`crates/tymuxd/src/main.rs:38-47`) is keyed only by `pane_id`, and its own doc comment admits: "a pane with multiple concurrently attached clients can produce a false positive if one client detaches right before the pane legitimately exits while another client is still watching." That's an accepted simplification there because the consequence is just a spurious warning log. A resume-cursor tracker with the same shortcut would have a materially worse consequence: wrong replay content served to the wrong client.

## Decision

The resume token is `resume_from_seq: u64`, sent on the first `AttachRequest`, implicitly bound to whatever `pane_id` is set in that same first message (`AttachRequest.pane_id` already exists as the oneof's first-message field). No new server-side `HashMap<ClientId, Cursor>` or equivalent is introduced. This mirrors Discord Gateway's `Resume` opcode precedent (features.md): a compound `(session_id, seq)` token, where binding the seq to an identity closes the cross-identity replay-confusion edge case "by construction," not by a runtime check.

Concretely: the replay buffer itself is `Pane`-scoped (Epic 2.1), not per-subscriber — exactly like `output_tx`/`output_seq` already are. A resume request's only job is to say which seq, *within this pane's own buffer*, the client has already seen; there is no cross-pane ambiguity because the buffer being read is already resolved to one specific pane by the time `resume_from_seq` is interpreted.

## Rationale

- **No generation guard needed.** `pane_id` is already a `Uuid`, never reused (confirmed: `tymux revive` respawns a pane at the *same* id deliberately, per `Pane::spawn_with_id`'s doc comment, but that's an explicit revival of a specific pane's identity, not id reuse across unrelated panes). A resume token can't be replayed against the "wrong" pane's history by construction — it's always interpreted against the one pane it's already scoped to.
- **Consistent with the existing architectural shape.** Every `Attach` call is already fully independent and anonymous at the RPC layer (no session/identity concept exists anywhere else in this proto). Adding server-tracked per-client state for this one feature would introduce the first such concept into an otherwise stateless-per-call API surface, for a benefit (potentially richer future bookkeeping) this project doesn't need.
- **Security classification is internal, no auth change** (requirements.md). A signed/opaque token (HMAC'd blob, expiry, etc.) was considered and rejected — it would add real complexity (a signing key to manage, a verification path, a new failure mode for "signature invalid") for a threat model this project explicitly isn't addressing.

## Consequences

- A malformed or out-of-range `resume_from_seq` (e.g. from a buggy client, or one that raced with a pane it no longer has current data about) degrades to exactly the same `GapExceeded` fallback as a legitimately-expired token — there is no separate "invalid token" error class. This is deliberate (features.md edge case 2): a single, simple fallback path is easier to reason about and test than a second error taxonomy for a case that isn't security-sensitive.
- If a future project genuinely needs server-side per-client cursor tracking (e.g. multiple resume streams per pane needing independent progress, which doesn't exist today), that would be a new design decision revisiting this ADR — not a natural extension of the current shape, since the whole point here is that no such state exists to extend.
