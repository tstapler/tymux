//! Epic 2.1: a bounded, byte-budgeted, append-only per-pane log of recent
//! output chunks, retained so a reconnecting `Attach` client can resume
//! from its last-seen `output_seq` instead of always re-syncing from
//! scratch via a full `CapturePane`/snapshot round-trip.
//!
//! `ReplayBuffer` is deliberately a pure, I/O-free type (PoEAA Transaction
//! Script shape) so its eviction and gap-check boundary logic — the
//! 1-indexed off-by-one and byte-budget edge cases pitfalls.md §2 calls
//! out — is provably correct in isolation before [`crate::pane::Pane`]'s
//! reader thread ever touches it (Story 2.1.1).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Typical per-pane grant under normal, non-exhausted conditions. Purely
/// documentation/test value — [`allocate_replay_budget`] does **not**
/// floor its return value at this constant; see that function's doc
/// comment for why. Only referenced from `#[cfg(test)]` code, hence the
/// explicit allow rather than leaving it to trip `dead_code` in
/// non-test builds.
#[allow(dead_code)]
pub(crate) const MIN_REPLAY_BUFFER_BYTES: usize = 16 * 1024;
/// Default per-pane replay-buffer byte budget, granted in full whenever
/// [`GLOBAL_REPLAY_BUFFER_BUDGET_BYTES`] has enough headroom left.
pub(crate) const DEFAULT_REPLAY_BUFFER_BYTES: usize = 256 * 1024;
/// Process-wide ceiling summed across every live pane's [`ReplayBuffer`].
/// Mirrors `pane.rs`'s `GLOBAL_SCROLLBACK_BUDGET_LINES`, but — unlike that
/// budget — is enforced as a genuine hard cap; see [`allocate_replay_budget`].
pub(crate) const GLOBAL_REPLAY_BUFFER_BUDGET_BYTES: usize = 64 * 1024 * 1024;

static GLOBAL_REPLAY_BUFFER_USED_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Grants a replay-buffer byte budget to a newly spawned pane, enforcing
/// [`GLOBAL_REPLAY_BUFFER_BUDGET_BYTES`] as a **genuine hard cap** on
/// total retained replay-buffer memory across every live pane.
///
/// This is deliberately *not* `allocate_scrollback_budget`'s
/// `remaining.max(MIN)` floor-grant shape: that formula always grants at
/// least `MIN_SCROLLBACK_LINES` even once the global budget is exhausted,
/// which is justified there (zero scrollback breaks copy-mode entirely)
/// but has no equivalent justification here. Nothing caps pane count, so
/// an unconditional floor-grant would let total replay memory grow
/// without bound as pane count grows — exactly what the "must not risk
/// unbounded memory growth" NFR forbids. A pane granted `0` bytes is a
/// safe, fully-supported degraded state: its buffer can never retain an
/// entry, so [`ReplayBuffer::replay_since`] always returns
/// [`ReplayOutcome::GapExceeded`], which the daemon's resume-handling
/// fallback path (Epic 2.2.2) already handles.
pub(crate) fn allocate_replay_budget() -> usize {
    let used = GLOBAL_REPLAY_BUFFER_USED_BYTES.load(Ordering::Relaxed);
    let remaining = GLOBAL_REPLAY_BUFFER_BUDGET_BYTES.saturating_sub(used);
    let granted = DEFAULT_REPLAY_BUFFER_BYTES.min(remaining);
    GLOBAL_REPLAY_BUFFER_USED_BYTES.fetch_add(granted, Ordering::Relaxed);
    granted
}

/// Returns a pane's granted replay-buffer budget to the global pool.
/// Mirrors `pane.rs`'s `release_scrollback_budget` exactly.
pub(crate) fn release_replay_budget(bytes: usize) {
    GLOBAL_REPLAY_BUFFER_USED_BYTES.fetch_sub(bytes, Ordering::Relaxed);
}

/// Result of [`ReplayBuffer::replay_since`]: either the requested
/// `resume_from_seq` is still covered by what's retained (`InWindow`), or
/// it has already been evicted / is otherwise out of range
/// (`GapExceeded`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayOutcome {
    InWindow {
        chunks: Vec<(u64, Vec<u8>)>,
        tail_seq: u64,
    },
    GapExceeded {
        oldest_available_seq: Option<u64>,
    },
}

/// A bounded, append-only, per-pane log of recent `(seq, data)` output
/// chunks — same tuple shape as [`crate::pane::Pane`]'s broadcast payload.
/// Evicts from the front once `total_bytes` exceeds `budget_bytes`, always
/// keeping at least one (the most recent) entry.
pub(crate) struct ReplayBuffer {
    entries: VecDeque<(u64, Vec<u8>)>,
    total_bytes: usize,
    budget_bytes: usize,
}

impl ReplayBuffer {
    pub(crate) fn new(budget_bytes: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            total_bytes: 0,
            budget_bytes,
        }
    }

    /// Appends one chunk, then evicts from the front until `total_bytes
    /// <= budget_bytes` — except the just-pushed entry is never itself
    /// evicted, even if it alone exceeds `budget_bytes` (a single
    /// oversized chunk must not empty the buffer).
    pub(crate) fn push(&mut self, seq: u64, data: &[u8]) {
        self.entries.push_back((seq, data.to_vec()));
        self.total_bytes += data.len();
        while self.total_bytes > self.budget_bytes && self.entries.len() > 1 {
            if let Some((_, evicted)) = self.entries.pop_front() {
                self.total_bytes -= evicted.len();
            }
        }
    }

    /// The seq of the oldest entry still retained, or `None` if empty.
    pub(crate) fn oldest_seq(&self) -> Option<u64> {
        self.entries.front().map(|(seq, _)| *seq)
    }

    /// `resume_from_seq`: the last seq the client already has.
    /// `latest_seq`: the pane's current `output_seq` at the moment of the
    /// call (i.e. `Pane::output_seq`, not derived from this buffer).
    ///
    /// Boundary convention (matches the existing `seq <= snapshot_seq`
    /// dedup check in `main.rs`): `InWindow` requires `resume_from_seq >=
    /// oldest_retained_seq` (or an empty buffer with `resume_from_seq ==
    /// latest_seq`, the fresh-pane case). This is deliberately
    /// conservative/`>=`-only — a request whose `resume_from_seq` is one
    /// less than the oldest retained entry is `GapExceeded`, even though
    /// that oldest entry is technically present, because everything
    /// *before* it (which the client would need to reconstruct the gap
    /// itself) is already gone.
    pub(crate) fn replay_since(&self, resume_from_seq: u64, latest_seq: u64) -> ReplayOutcome {
        if resume_from_seq > latest_seq {
            // Malformed or future token — degrade to the same fallback
            // signal as any other out-of-range request.
            return ReplayOutcome::GapExceeded {
                oldest_available_seq: self.oldest_seq(),
            };
        }
        let available_from = self.oldest_seq().unwrap_or(latest_seq);
        if resume_from_seq >= available_from {
            let chunks = self
                .entries
                .iter()
                .filter(|(seq, _)| *seq > resume_from_seq)
                .cloned()
                .collect();
            ReplayOutcome::InWindow {
                chunks,
                tail_seq: latest_seq,
            }
        } else {
            ReplayOutcome::GapExceeded {
                oldest_available_seq: self.oldest_seq(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Story 2.1.1 AC1 / Task 2.1.1c — eviction under budget pressure: a
    /// buffer already holding 90 bytes against a 100-byte budget must
    /// evict from the front until back under budget once a new 30-byte
    /// chunk arrives, and the newly pushed entry must always survive.
    #[test]
    fn replay_buffer_push_should_evict_oldest_entries_until_under_budget_when_new_chunk_arrives() {
        let mut buf = ReplayBuffer::new(100);
        // Three 30-byte chunks (90 bytes total), seqs 1..=3.
        buf.push(1, &[0u8; 30]);
        buf.push(2, &[0u8; 30]);
        buf.push(3, &[0u8; 30]);
        assert_eq!(buf.total_bytes, 90);

        buf.push(4, &[0u8; 30]);

        assert!(
            buf.total_bytes <= 100,
            "total_bytes ({}) must be back under the 100-byte budget after eviction",
            buf.total_bytes
        );
        assert_eq!(
            buf.entries.back().map(|(seq, _)| *seq),
            Some(4),
            "the newly pushed entry must always be retained"
        );
        assert!(
            !buf.entries.iter().any(|(seq, _)| *seq == 1),
            "the oldest entry (seq 1) must have been evicted to make room"
        );
    }

    /// Story 2.1.1 AC1 — a single chunk larger than the whole budget must
    /// still be retained alone, not evicted down to zero entries.
    #[test]
    fn replay_buffer_push_should_retain_newest_entry_even_when_it_alone_exceeds_budget() {
        let mut buf = ReplayBuffer::new(10);
        buf.push(1, &[0u8; 5]);
        buf.push(2, &[0u8; 50]);

        assert_eq!(buf.entries.len(), 1);
        assert_eq!(buf.oldest_seq(), Some(2));
    }

    /// Story 2.1.1 AC2 / Task 2.1.1c — `resume_from_seq` exactly at the
    /// oldest retained seq is `InWindow`.
    #[test]
    fn replay_buffer_replay_since_should_return_in_window_when_resume_from_seq_equals_oldest_retained(
    ) {
        let mut buf = ReplayBuffer::new(1_000);
        for seq in 5..=9u64 {
            buf.push(seq, b"x");
        }

        let outcome = buf.replay_since(5, 9);

        match outcome {
            ReplayOutcome::InWindow { chunks, tail_seq } => {
                assert_eq!(tail_seq, 9);
                assert_eq!(
                    chunks.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
                    vec![6, 7, 8, 9]
                );
            }
            other => panic!("expected InWindow, got {other:?}"),
        }
    }

    /// Story 2.1.1 AC2 / Task 2.1.1c — `resume_from_seq` one less than
    /// the oldest retained seq is `GapExceeded`, per the conservative
    /// `>=`-only boundary, even though that oldest chunk is technically
    /// present.
    #[test]
    fn replay_buffer_replay_since_should_return_gap_exceeded_when_resume_from_seq_is_one_less_than_oldest_retained(
    ) {
        let mut buf = ReplayBuffer::new(1_000);
        for seq in 5..=9u64 {
            buf.push(seq, b"x");
        }
        assert_eq!(buf.oldest_seq(), Some(5));

        let outcome = buf.replay_since(5u64.saturating_sub(1), 9);

        assert_eq!(
            outcome,
            ReplayOutcome::GapExceeded {
                oldest_available_seq: Some(5)
            }
        );
    }

    /// Story 2.1.1 AC2, third bullet / Task 2.1.1c — a fresh pane with no
    /// output yet (empty buffer, `latest_seq == 0`) must return
    /// `InWindow` with no entries and no subtraction underflow.
    #[test]
    fn replay_buffer_replay_since_should_return_in_window_empty_when_fresh_pane_has_no_output_yet()
    {
        let buf = ReplayBuffer::new(1_000);

        let outcome = buf.replay_since(0, 0);

        assert_eq!(
            outcome,
            ReplayOutcome::InWindow {
                chunks: vec![],
                tail_seq: 0
            }
        );
    }

    /// Story 2.1.1 AC2, fourth bullet / Task 2.1.1c — a `resume_from_seq`
    /// greater than `latest_seq` (malformed or future token) degrades to
    /// `GapExceeded`, same as any other out-of-range request.
    #[test]
    fn replay_buffer_replay_since_should_return_gap_exceeded_when_resume_from_seq_exceeds_latest_seq(
    ) {
        let mut buf = ReplayBuffer::new(1_000);
        buf.push(1, b"x");
        buf.push(2, b"y");

        let outcome = buf.replay_since(100, 2);

        assert_eq!(
            outcome,
            ReplayOutcome::GapExceeded {
                oldest_available_seq: Some(1)
            }
        );
    }

    /// Story 2.1.1 AC3 / Pattern Decisions — `allocate_replay_budget` is
    /// a genuine hard cap: once the global budget is already fully
    /// allocated, a new pane must receive `0` bytes, never the
    /// `MIN_REPLAY_BUFFER_BYTES` floor that `allocate_scrollback_budget`
    /// would grant in the equivalent situation.
    #[test]
    fn allocate_replay_budget_should_return_zero_not_min_floor_when_global_budget_already_exhausted(
    ) {
        // Drive the shared global budget to exhaustion using
        // allocate_replay_budget itself (the same call path every real
        // pane uses), rather than writing the private counter directly —
        // that would risk an absolute store/restore racing with, and
        // underflowing against, any other test concurrently allocating
        // from the same process-wide static.
        let mut granted_so_far = Vec::new();
        loop {
            let granted = allocate_replay_budget();
            if granted == 0 {
                break;
            }
            granted_so_far.push(granted);
        }

        let granted = allocate_replay_budget();

        assert_eq!(
            granted, 0,
            "once the global budget is exhausted, allocate_replay_budget must grant 0, \
             not MIN_REPLAY_BUFFER_BYTES"
        );

        for bytes in granted_so_far {
            release_replay_budget(bytes);
        }
    }
}
