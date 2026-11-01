//! Which attached clients will actually run a given output chunk through their emulator.
//!
//! `broadcast::Sender::receiver_count` cannot answer that: an attach subscribes before it takes
//! its snapshot (chunks at or below the snapshot's seq are dropped, never shown to the emulator),
//! and a lagged or stalled receiver keeps the count up. A client therefore registers separately,
//! and only counts as the answerer for chunks past the seq it went live at.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

struct Slot {
    /// Chunks with `seq > live_from` reach the client's emulator. `u64::MAX` until the attach has
    /// computed its snapshot/replay threshold, so an unfinished attach never counts.
    live_from: AtomicU64,
    answering: AtomicBool,
}

/// An attached client's claim to answer terminal queries. Dropping it releases the claim.
pub struct LiveClient(Arc<Slot>);

impl LiveClient {
    /// The client will see (and its emulator answer queries in) every chunk with `seq > seq`.
    pub fn go_live_after(&self, seq: u64) {
        self.0.live_from.store(seq, Ordering::Release);
    }

    /// A lagged client skipped output it never saw, so it must not be relied on to answer; the
    /// daemon answers (and strips the queries from what clients see) until it catches up.
    pub fn set_lagged(&self, lagged: bool) {
        self.0.answering.store(!lagged, Ordering::Release);
    }
}

#[derive(Default)]
pub(crate) struct ClientRegistry {
    slots: Mutex<Vec<Weak<Slot>>>,
}

impl ClientRegistry {
    pub(crate) fn register(&self) -> LiveClient {
        let slot = Arc::new(Slot {
            live_from: AtomicU64::new(u64::MAX),
            answering: AtomicBool::new(true),
        });
        let mut slots = self.slots.lock().unwrap();
        slots.retain(|w| w.strong_count() > 0);
        slots.push(Arc::downgrade(&slot));
        LiveClient(slot)
    }

    /// Whether some client's emulator will see the chunk with this seq.
    pub(crate) fn has_answerer(&self, seq: u64) -> bool {
        self.slots
            .lock()
            .unwrap()
            .iter()
            .filter_map(Weak::upgrade)
            .any(|s| {
                s.answering.load(Ordering::Acquire) && s.live_from.load(Ordering::Acquire) < seq
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_should_answer_only_chunks_after_its_live_seq() {
        let reg = ClientRegistry::default();
        let client = reg.register();
        assert!(!reg.has_answerer(5), "an unfinished attach must not count");
        client.go_live_after(4);
        assert!(!reg.has_answerer(4), "chunk 4 is in the snapshot");
        assert!(reg.has_answerer(5));
    }

    #[test]
    fn a_lagged_client_should_not_count_until_it_recovers() {
        let reg = ClientRegistry::default();
        let client = reg.register();
        client.go_live_after(0);
        client.set_lagged(true);
        assert!(!reg.has_answerer(1));
        client.set_lagged(false);
        assert!(reg.has_answerer(1));
    }

    #[test]
    fn a_dropped_client_should_stop_counting() {
        let reg = ClientRegistry::default();
        let client = reg.register();
        client.go_live_after(0);
        drop(client);
        assert!(!reg.has_answerer(1));
    }
}
