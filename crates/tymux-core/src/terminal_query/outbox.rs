//! Bounded queue of replies drained by a dedicated writer thread.
//!
//! The pane's reader thread must never block on the pty: it also drains the child's output, so a
//! child that floods queries without reading stdin would otherwise deadlock itself. `offer` only
//! touches an in-memory queue; the (possibly blocking) pty write happens on the writer thread.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

/// Replies allowed in a burst, and how fast the allowance refills.
const RATE_BURST: f64 = 16.0;
const RATE_REFILL_PER_SEC: f64 = 8.0;
/// Pending replies kept; when full the oldest is dropped, so a new reply always has room.
const MAX_PENDING: usize = 32;

/// Where replies end up; `write_reply` may block.
pub(crate) trait ReplyWriter: Send {
    fn write_reply(&mut self, bytes: &[u8]);
}

struct State {
    queue: VecDeque<Vec<u8>>,
    tokens: f64,
    last: Instant,
    closed: bool,
    writer: Option<Box<dyn ReplyWriter>>,
}

struct Shared {
    accepted: AtomicU64,
    state: Mutex<State>,
    wake: Condvar,
}

pub(crate) struct Outbox {
    shared: Arc<Shared>,
}

impl Outbox {
    pub(crate) fn new(writer: Box<dyn ReplyWriter>) -> Self {
        Self {
            shared: Arc::new(Shared {
                accepted: AtomicU64::new(0),
                state: Mutex::new(State {
                    queue: VecDeque::new(),
                    tokens: RATE_BURST,
                    last: Instant::now(),
                    closed: false,
                    writer: Some(writer),
                }),
                wake: Condvar::new(),
            }),
        }
    }

    /// Queues `reply`. An identical pending reply absorbs it for free (so a burst of DA1s costs one
    /// write); otherwise it spends a rate-limit token, and is dropped silently without one.
    pub(crate) fn offer(&self, reply: Vec<u8>, now: Instant) {
        let mut st = self.shared.state.lock().unwrap();
        if st.closed || st.queue.contains(&reply) {
            return;
        }
        let elapsed = now.saturating_duration_since(st.last).as_secs_f64();
        st.last = now;
        st.tokens = (st.tokens + elapsed * RATE_REFILL_PER_SEC).min(RATE_BURST);
        if st.tokens < 1.0 {
            return;
        }
        st.tokens -= 1.0;
        if st.queue.len() >= MAX_PENDING {
            st.queue.pop_front();
        }
        st.queue.push_back(reply);
        self.shared.accepted.fetch_add(1, Ordering::Relaxed);
        let first_reply_writer = st.writer.take();
        drop(st);
        if let Some(writer) = first_reply_writer {
            // First reply: start the writer lazily so panes that never see a query cost nothing.
            let shared = self.shared.clone();
            let spawned = std::thread::Builder::new()
                .name("tymux-query-replies".into())
                .spawn(move || drain(&shared, writer));
            if let Err(err) = spawned {
                // No thread, no replies; never panic here, the caller holds the parser lock.
                tracing::warn!(%err, "cannot start terminal-query reply writer; replies disabled");
                self.shared.state.lock().unwrap().closed = true;
                return;
            }
        }
        self.shared.wake.notify_one();
    }

    pub(crate) fn close(&self) {
        self.shared.state.lock().unwrap().closed = true;
        self.shared.wake.notify_all();
    }

    #[cfg(test)]
    pub(crate) fn accepted(&self) -> u64 {
        self.shared.accepted.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn pending(&self) -> usize {
        self.shared.state.lock().unwrap().queue.len()
    }
}

impl Drop for Outbox {
    fn drop(&mut self) {
        self.close();
    }
}

fn drain(shared: &Shared, mut writer: Box<dyn ReplyWriter>) {
    loop {
        let reply = {
            let mut st = shared.state.lock().unwrap();
            loop {
                if st.closed {
                    return;
                }
                if let Some(reply) = st.queue.pop_front() {
                    break reply;
                }
                st = shared.wake.wait(st).unwrap();
            }
        };
        writer.write_reply(&reply);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    struct Collect(mpsc::Sender<Vec<u8>>);
    impl ReplyWriter for Collect {
        fn write_reply(&mut self, bytes: &[u8]) {
            let _ = self.0.send(bytes.to_vec());
        }
    }

    /// Blocks every write until the test drops the gate, like a full pty; tells the test when
    /// the first write has started.
    struct Stuck {
        gate: mpsc::Receiver<()>,
        started: mpsc::Sender<()>,
    }
    impl ReplyWriter for Stuck {
        fn write_reply(&mut self, _: &[u8]) {
            let _ = self.started.send(());
            let _ = self.gate.recv();
        }
    }

    fn stuck_outbox() -> (Outbox, mpsc::Sender<()>, mpsc::Receiver<()>) {
        let (gate_tx, gate) = mpsc::channel();
        let (started, started_rx) = mpsc::channel();
        (
            Outbox::new(Box::new(Stuck { gate, started })),
            gate_tx,
            started_rx,
        )
    }

    #[test]
    fn offer_should_never_block_even_when_the_writer_is_stuck() {
        let (outbox, _gate, _started) = stuck_outbox();
        let t0 = Instant::now();
        for i in 0..10_000u32 {
            outbox.offer(
                i.to_string().into_bytes(),
                t0 + Duration::from_secs(u64::from(i)),
            );
        }
        assert!(outbox.pending() <= MAX_PENDING);
        assert!(outbox.pending() >= 1, "a new reply always has room");
    }

    #[test]
    fn identical_pending_replies_should_coalesce_without_spending_tokens() {
        let (outbox, _gate, started) = stuck_outbox();
        let t0 = Instant::now();
        outbox.offer(b"first".to_vec(), t0); // taken by the stuck writer
        started.recv_timeout(Duration::from_secs(30)).unwrap();
        for _ in 0..1000 {
            outbox.offer(b"DA1".to_vec(), t0);
        }
        assert_eq!(outbox.pending(), 1, "1000 identical DA1s coalesce into one");
        // tokens were spent only for the two distinct replies, so a fresh reply still fits
        outbox.offer(b"DSR".to_vec(), t0);
        assert_eq!(outbox.pending(), 2);
    }

    #[test]
    fn replies_should_be_rate_limited_and_refill() {
        let (tx, rx) = mpsc::channel();
        let outbox = Outbox::new(Box::new(Collect(tx)));
        let t0 = Instant::now();
        for i in 0..40u32 {
            outbox.offer(i.to_string().into_bytes(), t0);
        }
        outbox.offer(b"later".to_vec(), t0 + Duration::from_secs(1));
        let mut got = Vec::new();
        while let Ok(r) = rx.recv_timeout(Duration::from_millis(300)) {
            got.push(r);
        }
        assert_eq!(
            got.len(),
            16 + 1,
            "burst of 16, then one after a second of refill"
        );
        assert_eq!(got.last().unwrap(), b"later");
    }
}
