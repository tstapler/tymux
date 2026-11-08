//! Dedicated work pools for blocking pane work, and poll-time monitoring for async tasks.
//!
//! Blocking pane operations (writing to a pty, reflowing a grid, rendering a snapshot) take
//! locks a busy pane's reader thread holds almost continuously. Running them on tokio's shared
//! blocking pool lets slow captures queue ahead of a keystroke, so each latency class gets its
//! own rayon pool: `Input` (pty writes), `Resize` (reflow, can stall on the parser lock) and
//! `Work` (captures and attach snapshots, work-stealing across panes). `Input` and `Resize` are
//! sized generously because their jobs mostly sleep: a pane whose pty is full or whose parser lock
//! is held parks one thread, and the pool must outlast many such panes (tokio's blocking pool
//! allowed 512). The per-pane parser lock
//! still serialises work within one pane, so parallelism is across panes; no job here may hold
//! that lock across a hop back to async.
//!
//! Callers get the result through a oneshot channel, never blocking a tokio worker. A job whose
//! caller is gone (its future was dropped) is skipped before it runs. Queues are bounded.
//!
//! This is the one module allowed to block (see `clippy.toml`).
#![allow(clippy::disallowed_methods)]

use futures::future::BoxFuture;
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

/// A latency class with its own thread pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Input,
    Resize,
    Work,
}

impl Class {
    pub const ALL: [Class; 3] = [Class::Input, Class::Resize, Class::Work];

    pub fn name(self) -> &'static str {
        match self {
            Class::Input => "input",
            Class::Resize => "resize",
            Class::Work => "work",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum PoolError {
    /// The class's queue is full; the caller should shed load.
    Rejected,
    /// The job panicked; the worker survives.
    Panicked,
    /// The job was skipped because the caller went away (or the pool shut down).
    Cancelled,
}

#[derive(Default)]
struct ClassStats {
    queue_depth: AtomicUsize,
    /// Jobs currently executing on a pool thread.
    running: AtomicUsize,
    /// Must-land jobs sent to tokio's blocking pool because every pool thread was busy.
    overflowed: AtomicU64,
    submitted: AtomicU64,
    completed: AtomicU64,
    skipped_cancelled: AtomicU64,
    panicked: AtomicU64,
    rejected: AtomicU64,
    wait_micros_total: AtomicU64,
    wait_micros_max: AtomicU64,
    run_micros_total: AtomicU64,
    run_micros_max: AtomicU64,
}

/// A point-in-time copy of one class's counters.
#[derive(Debug, Clone, Default)]
pub struct ClassSnapshot {
    pub class: &'static str,
    pub threads: usize,
    pub queue_depth: u64,
    pub submitted: u64,
    pub completed: u64,
    pub skipped_cancelled: u64,
    pub panicked: u64,
    pub rejected: u64,
    pub overflowed: u64,
    pub running: u64,
    pub wait_micros_total: u64,
    pub wait_micros_max: u64,
    pub run_micros_total: u64,
    pub run_micros_max: u64,
}

struct ClassPool {
    pool: rayon::ThreadPool,
    threads: usize,
    queue_limit: usize,
    stats: Arc<ClassStats>,
}

pub struct Pools {
    input: ClassPool,
    resize: ClassPool,
    work: ClassPool,
    /// Test support: run jobs on tokio's blocking pool, which a paused test clock waits for
    /// (auto-advance would otherwise skip past timers while a rayon job is in flight).
    #[cfg(test)]
    tokio_backed: bool,
}

/// Thread counts and queue bound; every field has an env override (see [`PoolConfig::from_env`]).
#[derive(Debug, Clone, Copy)]
pub struct PoolConfig {
    pub input_threads: usize,
    pub resize_threads: usize,
    pub work_threads: usize,
    pub queue_limit: usize,
}

impl PoolConfig {
    /// `TYMUXD_INPUT_THREADS` (default 64), `TYMUXD_RESIZE_THREADS` (default 16),
    /// `TYMUXD_WORK_THREADS` (default available parallelism, at least 2) and
    /// `TYMUXD_POOL_QUEUE_LIMIT` (default 1024 queued jobs per class).
    pub fn from_env() -> Self {
        let env = |key: &str| {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
        };
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        Self {
            input_threads: env("TYMUXD_INPUT_THREADS").unwrap_or(64).max(1),
            resize_threads: env("TYMUXD_RESIZE_THREADS").unwrap_or(16).max(1),
            work_threads: env("TYMUXD_WORK_THREADS").unwrap_or(cores.max(2)).max(1),
            queue_limit: env("TYMUXD_POOL_QUEUE_LIMIT").unwrap_or(1024).max(1),
        }
    }
}

fn build_pool(class: Class, threads: usize, queue_limit: usize) -> ClassPool {
    let name = class.name();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(move |i| format!("tymuxd-{name}-{i}"))
        // Jobs catch their own panics; this only stops a stray one from aborting the process.
        .panic_handler(|_| {})
        .build()
        .expect("failed to build a tymuxd work pool");
    ClassPool {
        pool,
        threads,
        queue_limit,
        stats: Arc::new(ClassStats::default()),
    }
}

impl Pools {
    pub fn new(config: PoolConfig) -> Self {
        Self {
            input: build_pool(Class::Input, config.input_threads, config.queue_limit),
            resize: build_pool(Class::Resize, config.resize_threads, config.queue_limit),
            work: build_pool(Class::Work, config.work_threads, config.queue_limit),
            #[cfg(test)]
            tokio_backed: false,
        }
    }

    #[cfg(test)]
    pub fn tokio_backed() -> Self {
        let mut pools = Self::new(PoolConfig {
            input_threads: 1,
            resize_threads: 1,
            work_threads: 1,
            queue_limit: 1024,
        });
        pools.tokio_backed = true;
        pools
    }

    pub fn from_env() -> Self {
        Self::new(PoolConfig::from_env())
    }

    fn class_pool(&self, class: Class) -> &ClassPool {
        match class {
            Class::Input => &self.input,
            Class::Resize => &self.resize,
            Class::Work => &self.work,
        }
    }

    /// Runs `job` on the class's pool. If the returned future is dropped before the job starts,
    /// the job is skipped; one already running finishes (it cannot be interrupted).
    pub fn run<T, F>(&self, class: Class, job: F) -> BoxFuture<'static, Result<T, PoolError>>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        #[cfg(test)]
        if self.tokio_backed {
            let handle = tokio::task::spawn_blocking(job);
            return Box::pin(async move { handle.await.map_err(|_| PoolError::Panicked) });
        }
        let cp = self.class_pool(class);
        let stats = cp.stats.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let limit = cp.queue_limit;
        let admitted = if stats
            .queue_depth
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |d| {
                (d < limit).then_some(d + 1)
            })
            .is_ok()
        {
            stats.submitted.fetch_add(1, Ordering::Relaxed);
            let submitted_at = Instant::now();
            cp.pool.spawn(move || {
                stats.queue_depth.fetch_sub(1, Ordering::SeqCst);
                if tx.is_closed() {
                    stats.skipped_cancelled.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                record_max(
                    &stats.wait_micros_total,
                    &stats.wait_micros_max,
                    submitted_at.elapsed(),
                );
                let started = Instant::now();
                stats.running.fetch_add(1, Ordering::SeqCst);
                let outcome = catch_unwind(AssertUnwindSafe(job));
                stats.running.fetch_sub(1, Ordering::SeqCst);
                record_max(
                    &stats.run_micros_total,
                    &stats.run_micros_max,
                    started.elapsed(),
                );
                match outcome {
                    Ok(value) => {
                        stats.completed.fetch_add(1, Ordering::Relaxed);
                        let _ = tx.send(Ok(value));
                    }
                    Err(_) => {
                        stats.panicked.fetch_add(1, Ordering::Relaxed);
                        let _ = tx.send(Err(PoolError::Panicked));
                    }
                }
            });
            true
        } else {
            stats.rejected.fetch_add(1, Ordering::Relaxed);
            false
        };
        Box::pin(async move {
            if !admitted {
                return Err(PoolError::Rejected);
            }
            rx.await.map_err(|_| PoolError::Cancelled)?
        })
    }

    /// Like [`Pools::run`] for work that must not be dropped (keystrokes, resizes). A full queue
    /// is waited out with a short backoff (backpressure) instead of rejected. When every pool
    /// thread is busy (stalled panes park threads in uninterruptible writes), the job goes to
    /// tokio's much larger blocking pool instead of queueing behind them, so a healthy pane is
    /// never starved by stalled ones.
    pub async fn run_must_land<T, F>(&self, class: Class, job: F) -> Result<T, PoolError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + Clone + 'static,
    {
        let cp = self.class_pool(class);
        let busy =
            cp.stats.running.load(Ordering::SeqCst) + cp.stats.queue_depth.load(Ordering::SeqCst);
        if busy >= cp.threads {
            cp.stats.overflowed.fetch_add(1, Ordering::Relaxed);
            return tokio::task::spawn_blocking(job)
                .await
                .map_err(|_| PoolError::Panicked);
        }
        loop {
            match self.run(class, job.clone()).await {
                Err(PoolError::Rejected) => tokio::time::sleep(Duration::from_millis(5)).await,
                other => return other,
            }
        }
    }

    pub fn snapshot(&self) -> Vec<ClassSnapshot> {
        Class::ALL
            .iter()
            .map(|&class| {
                let cp = self.class_pool(class);
                let s = &cp.stats;
                ClassSnapshot {
                    class: class.name(),
                    threads: cp.threads,
                    queue_depth: s.queue_depth.load(Ordering::Relaxed) as u64,
                    submitted: s.submitted.load(Ordering::Relaxed),
                    completed: s.completed.load(Ordering::Relaxed),
                    skipped_cancelled: s.skipped_cancelled.load(Ordering::Relaxed),
                    panicked: s.panicked.load(Ordering::Relaxed),
                    rejected: s.rejected.load(Ordering::Relaxed),
                    overflowed: s.overflowed.load(Ordering::Relaxed),
                    running: s.running.load(Ordering::Relaxed) as u64,
                    wait_micros_total: s.wait_micros_total.load(Ordering::Relaxed),
                    wait_micros_max: s.wait_micros_max.load(Ordering::Relaxed),
                    run_micros_total: s.run_micros_total.load(Ordering::Relaxed),
                    run_micros_max: s.run_micros_max.load(Ordering::Relaxed),
                }
            })
            .collect()
    }
}

fn record_max(total: &AtomicU64, max: &AtomicU64, elapsed: Duration) {
    let micros = elapsed.as_micros() as u64;
    total.fetch_add(micros, Ordering::Relaxed);
    max.fetch_max(micros, Ordering::Relaxed);
}

// ---- poll-time monitoring -------------------------------------------------------------------

/// Per task class poll counters: how many polls, how many exceeded the slow threshold, and the
/// longest. A poll that blocks an async worker (a sync lock, sleep or file read in an async fn)
/// shows up here as a slow poll.
pub struct PollMonitor {
    pub class: &'static str,
    threshold: Duration,
    polls: AtomicU64,
    slow_polls: AtomicU64,
    total_poll_micros: AtomicU64,
    max_poll_micros: AtomicU64,
    last_log: Mutex<Option<Instant>>,
}

#[derive(Debug, Clone, Default)]
pub struct PollSnapshot {
    pub class: &'static str,
    pub polls: u64,
    pub slow_polls: u64,
    pub total_poll_micros: u64,
    pub max_poll_micros: u64,
}

impl PollMonitor {
    pub fn new(class: &'static str, threshold: Duration) -> Arc<Self> {
        Arc::new(Self {
            class,
            threshold,
            polls: AtomicU64::new(0),
            slow_polls: AtomicU64::new(0),
            total_poll_micros: AtomicU64::new(0),
            max_poll_micros: AtomicU64::new(0),
            last_log: Mutex::new(None),
        })
    }

    /// Wraps `fut` so each of its polls is timed.
    pub fn instrument<F: Future>(self: &Arc<Self>, fut: F) -> Monitored<F> {
        Monitored {
            inner: fut,
            monitor: self.clone(),
        }
    }

    fn record(&self, elapsed: Duration) {
        let micros = elapsed.as_micros() as u64;
        self.polls.fetch_add(1, Ordering::Relaxed);
        self.total_poll_micros.fetch_add(micros, Ordering::Relaxed);
        self.max_poll_micros.fetch_max(micros, Ordering::Relaxed);
        if elapsed < self.threshold {
            return;
        }
        self.slow_polls.fetch_add(1, Ordering::Relaxed);
        let mut last = self.last_log.lock().unwrap();
        if last.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
            *last = Some(Instant::now());
            tracing::warn!(
                task_class = self.class,
                poll_micros = micros,
                slow_polls = self.slow_polls.load(Ordering::Relaxed),
                "slow poll: an async task held a worker thread; blocking work belongs on the pool"
            );
        }
    }

    pub fn snapshot(&self) -> PollSnapshot {
        PollSnapshot {
            class: self.class,
            polls: self.polls.load(Ordering::Relaxed),
            slow_polls: self.slow_polls.load(Ordering::Relaxed),
            total_poll_micros: self.total_poll_micros.load(Ordering::Relaxed),
            max_poll_micros: self.max_poll_micros.load(Ordering::Relaxed),
        }
    }
}

pin_project_lite::pin_project! {
    pub struct Monitored<F> {
        #[pin]
        inner: F,
        monitor: Arc<PollMonitor>,
    }
}

impl<F: Future> Future for Monitored<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let started = Instant::now();
        let result = this.inner.poll(cx);
        this.monitor.record(started.elapsed());
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn small() -> Pools {
        Pools::new(PoolConfig {
            input_threads: 1,
            resize_threads: 1,
            work_threads: 2,
            queue_limit: 64,
        })
    }

    #[tokio::test]
    async fn run_should_return_the_jobs_value_and_count_it() {
        let pools = small();
        assert_eq!(pools.run(Class::Work, || 21 * 2).await, Ok(42));
        let work = pools
            .snapshot()
            .into_iter()
            .find(|s| s.class == "work")
            .unwrap();
        assert_eq!(
            (work.submitted, work.completed, work.queue_depth),
            (1, 1, 0)
        );
    }

    #[tokio::test]
    async fn a_panicking_job_should_report_panicked_and_leave_the_worker_alive() {
        let pools = small();
        assert_eq!(
            pools.run(Class::Input, || -> u8 { panic!("boom") }).await,
            Err(PoolError::Panicked)
        );
        // The single input worker must still run the next job.
        assert_eq!(pools.run(Class::Input, || 7).await, Ok(7));
        let input = pools
            .snapshot()
            .into_iter()
            .find(|s| s.class == "input")
            .unwrap();
        assert_eq!(input.panicked, 1);
    }

    #[tokio::test]
    async fn a_job_whose_caller_was_dropped_should_be_skipped_before_it_runs() {
        let pools = small();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        // Occupy the only resize worker so the next job stays queued.
        let blocker = pools.run(Class::Resize, move || {
            let _ = release_rx.lock().unwrap().recv();
        });
        let ran = Arc::new(AtomicBool::new(false));
        let ran_in_job = ran.clone();
        let queued = pools.run(Class::Resize, move || {
            ran_in_job.store(true, Ordering::SeqCst)
        });
        drop(queued);
        release_tx.send(()).unwrap();
        blocker.await.unwrap();
        // The pool is single-threaded, so this job runs after the skipped one.
        pools.run(Class::Resize, || ()).await.unwrap();
        assert!(
            !ran.load(Ordering::SeqCst),
            "a dropped caller's job must not run"
        );
        let resize = pools
            .snapshot()
            .into_iter()
            .find(|s| s.class == "resize")
            .unwrap();
        assert_eq!(resize.skipped_cancelled, 1);
    }

    #[tokio::test]
    async fn a_full_queue_should_reject_instead_of_growing() {
        let pools = Pools::new(PoolConfig {
            input_threads: 1,
            resize_threads: 1,
            work_threads: 1,
            queue_limit: 2,
        });
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let blocker = pools.run(Class::Work, move || {
            let _ = release_rx.lock().unwrap().recv();
        });
        // Wait for the blocker to leave the queue and occupy the worker.
        while pools
            .snapshot()
            .iter()
            .any(|s| s.class == "work" && s.queue_depth > 0)
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let a = pools.run(Class::Work, || ());
        let b = pools.run(Class::Work, || ());
        let overflow = pools.run(Class::Work, || ()).await;
        assert_eq!(overflow, Err(PoolError::Rejected));
        release_tx.send(()).unwrap();
        blocker.await.unwrap();
        a.await.unwrap();
        b.await.unwrap();
    }

    #[tokio::test]
    async fn stalled_panes_should_not_starve_a_healthy_pane_of_input_threads() {
        let pools = Arc::new(Pools::new(PoolConfig {
            input_threads: 2,
            resize_threads: 1,
            work_threads: 1,
            queue_limit: 64,
        }));
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Arc::new(Mutex::new(release_rx));
        // Two stalled panes park both input threads in uninterruptible "writes".
        let stalled: Vec<_> = (0..2)
            .map(|_| {
                let rx = release_rx.clone();
                pools.run(Class::Input, move || {
                    let _ = rx.lock().unwrap().recv();
                })
            })
            .collect();
        while pools
            .snapshot()
            .iter()
            .any(|s| s.class == "input" && s.running < 2)
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let started = Instant::now();
        pools.run_must_land(Class::Input, || ()).await.unwrap();
        assert!(started.elapsed() < Duration::from_millis(500));
        let input = pools
            .snapshot()
            .into_iter()
            .find(|s| s.class == "input")
            .unwrap();
        assert_eq!(input.overflowed, 1);
        drop(release_tx);
        drop(stalled);
    }

    /// The point of the split: input stays fast while slow work is queued, and the negative
    /// control shows a single shared pool does not.
    #[tokio::test]
    async fn input_latency_should_stay_low_while_slow_captures_are_queued() {
        const SLOW_JOBS: usize = 8;
        const SLOW: Duration = Duration::from_millis(100);

        async fn input_latency(shared: bool) -> Duration {
            let pools = Pools::new(PoolConfig {
                input_threads: 1,
                resize_threads: 1,
                work_threads: 2,
                queue_limit: 64,
            });
            let slow_class = Class::Work;
            // Negative control: send the keystroke through the same pool the captures use.
            let input_class = if shared { Class::Work } else { Class::Input };
            let slow: Vec<_> = (0..SLOW_JOBS)
                .map(|_| pools.run(slow_class, || std::thread::sleep(SLOW)))
                .collect();
            let started = Instant::now();
            pools.run(input_class, || ()).await.unwrap();
            let latency = started.elapsed();
            for job in slow {
                job.await.unwrap();
            }
            latency
        }

        let isolated = input_latency(false).await;
        let shared = input_latency(true).await;
        assert!(
            isolated < Duration::from_millis(150),
            "isolated input took {isolated:?}"
        );
        assert!(
            shared > Duration::from_millis(250),
            "negative control should queue behind the slow jobs, took {shared:?}"
        );
    }

    #[tokio::test]
    async fn a_blocking_poll_should_trip_the_slow_poll_counter() {
        let monitor = PollMonitor::new("test", Duration::from_millis(10));
        monitor
            .instrument(async {
                std::thread::sleep(Duration::from_millis(30));
            })
            .await;
        monitor.instrument(async {}).await;
        let snap = monitor.snapshot();
        assert_eq!(snap.polls, 2);
        assert_eq!(snap.slow_polls, 1);
        assert!(snap.max_poll_micros >= 30_000);
    }
}
