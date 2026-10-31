//! Restart policy and the per-pane lifecycle state machine (ADR-0007).
//!
//! Everything here is pure: no threads, no I/O, no wall clock. Callers pass `now` (unix
//! milliseconds from a [`Clock`]) and a jitter sample in `[0, 1)` from a [`Jitter`], so the whole
//! crash-loop / backoff / sliding-window behavior is tested with a [`ManualClock`] and
//! [`FixedJitter`] and no sleeps. `supervision.rs` is the glue that drives this from real pane
//! exits and timers.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// Source of "now" in unix milliseconds. Wall time, not monotonic: it has to survive in the
/// persisted record and be shown to clients as `next_restart_at`. A backwards clock step only
/// delays a pending restart; it can never fire one early.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64)
    }
}

/// A clock that only moves when told to. For tests.
pub struct ManualClock(AtomicU64);

impl ManualClock {
    pub fn new(start_ms: u64) -> Self {
        Self(AtomicU64::new(start_ms))
    }

    pub fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }

    pub fn set(&self, ms: u64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Source of uniform samples in `[0, 1)` for backoff jitter.
pub trait Jitter: Send + Sync {
    fn unit(&self) -> f64;
}

/// splitmix64 seeded from the clock; jitter only needs to decorrelate restarts, not be secret.
pub struct SystemJitter(AtomicU64);

impl Default for SystemJitter {
    fn default() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0x9E37_79B9_7F4A_7C15, |d| d.as_nanos() as u64);
        Self(AtomicU64::new(seed))
    }
}

impl Jitter for SystemJitter {
    fn unit(&self) -> f64 {
        let mut z = self
            .0
            .fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed)
            .wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Always returns the same sample. `FixedJitter(0.5)` means "no jitter" (the delay is exactly
/// the unjittered backoff).
pub struct FixedJitter(pub f64);

impl Jitter for FixedJitter {
    fn unit(&self) -> f64 {
        self.0
    }
}

/// When a pane whose process died is restarted automatically.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartMode {
    /// Never (today's behavior, and the default).
    #[default]
    Never,
    /// Only after a non-zero exit (or an exit with no known code).
    OnFailure,
    /// After every exit, including exit code 0.
    Always,
    /// Same as `Always`. An explicit stop is honored in every mode (it parks the pane in
    /// `Stopped` until `StartPane`), so the two behave identically; this spelling exists for
    /// callers that expect Docker's name.
    UnlessStopped,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RestartPolicy {
    pub mode: RestartMode,
    /// Automatic restarts allowed within `window_ms`; the next failure after that is `Failed`.
    pub max_restarts: u32,
    /// Sliding window the budget is counted over. 0 = no window: the budget is lifetime, until
    /// `ResetRestartBudget` / `StartPane`.
    pub window_ms: u64,
    pub initial_delay_ms: u64,
    pub multiplier: f64,
    pub max_delay_ms: u64,
    /// Fraction of the delay randomized either way, in `[0, 1]`.
    pub jitter: f64,
    /// A run at least this long counts as healthy and resets the backoff delay to
    /// `initial_delay_ms`.
    pub healthy_after_ms: u64,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            mode: RestartMode::Never,
            max_restarts: 5,
            window_ms: 60_000,
            initial_delay_ms: 1_000,
            multiplier: 2.0,
            max_delay_ms: 30_000,
            jitter: 0.2,
            healthy_after_ms: 10_000,
        }
    }
}

impl RestartPolicy {
    pub fn validate(&self) -> Result<(), String> {
        if !self.multiplier.is_finite() || self.multiplier < 1.0 {
            return Err(format!(
                "multiplier must be a finite number >= 1.0, got {}",
                self.multiplier
            ));
        }
        if !self.jitter.is_finite() || !(0.0..=1.0).contains(&self.jitter) {
            return Err(format!("jitter must be in [0, 1], got {}", self.jitter));
        }
        if self.max_delay_ms < self.initial_delay_ms {
            return Err(format!(
                "max_delay_ms ({}) must be >= initial_delay_ms ({})",
                self.max_delay_ms, self.initial_delay_ms
            ));
        }
        Ok(())
    }

    /// The delay before restart number `consecutive_failures + 1`, with `unit` in `[0, 1)`
    /// choosing a point in the jitter band. The result lies in
    /// `[base * (1 - jitter), min(base * (1 + jitter), max_delay_ms)]` where `base` is
    /// `initial_delay_ms * multiplier^consecutive_failures` capped at `max_delay_ms`.
    pub fn delay_ms(&self, consecutive_failures: u32, unit: f64) -> u64 {
        // 0 * multiplier^n is 0, but 0 * inf (a huge n) is NaN, which would pick the maximum.
        if self.initial_delay_ms == 0 {
            return 0;
        }
        let exp = consecutive_failures.min(1024) as i32;
        let base = (self.initial_delay_ms as f64 * self.multiplier.powi(exp))
            .min(self.max_delay_ms as f64);
        let jittered = base * (1.0 + self.jitter * (2.0 * unit.clamp(0.0, 1.0) - 1.0));
        jittered.clamp(0.0, self.max_delay_ms as f64).round() as u64
    }
}

/// Where a pane is in its life. `Live` and `Restarting` have (or are about to have) a process;
/// the rest do not, except `Stopped` for the moment between a stop request and the process
/// actually dying.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    /// The process is running.
    Live,
    /// The process exited and the policy says not to restart it.
    #[default]
    Exited,
    /// Waiting out a backoff delay; `next_restart_at_ms` says until when.
    Backoff,
    /// A manual restart was requested and the old process has not died yet, or the restart is
    /// due and being spawned.
    Restarting,
    /// The restart budget is exhausted. No automatic restart until an operator acts.
    Failed,
    /// Explicitly stopped; will not restart until `StartPane`.
    Stopped,
}

impl LifecycleState {
    /// True when a restart is on its way, so attached clients should stay attached.
    pub fn restart_pending(self) -> bool {
        matches!(self, Self::Backoff | Self::Restarting)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitRecord {
    pub code: Option<i32>,
    /// Set only when the daemon itself delivered the terminating signal (StopPane or
    /// RestartPane): portable-pty reports any signal death as code 1, so a signal the process
    /// took on its own is not distinguishable.
    pub signal: Option<i32>,
    pub at_ms: u64,
}

/// What the supervisor should do about an exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitKind {
    /// Policy says no restart (`Exited`), or nothing is supervised the policy cares about.
    NotRestarted,
    /// Backoff scheduled.
    Scheduled { delay_ms: u64, at_ms: u64 },
    /// An operator-requested restart: respawn as soon as possible, no delay, no budget cost.
    ManualRestart,
    /// Budget exhausted.
    Failed,
    /// Operator stop honored.
    Stopped,
}

/// A durable copy of everything the supervisor must remember across a daemon restart.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PersistedRestartState {
    pub policy: RestartPolicy,
    pub state: LifecycleState,
    pub restart_count: u32,
    pub restart_log_ms: Vec<u64>,
    pub consecutive_failures: u32,
    pub last_exit: Option<ExitRecord>,
}

/// A read-only view of one pane's supervision, for the status RPC.
#[derive(Clone, Debug, PartialEq)]
pub struct PaneStatusView {
    pub state: LifecycleState,
    pub policy: RestartPolicy,
    pub restart_count: u32,
    pub restarts_in_window: u32,
    pub consecutive_failures: u32,
    pub last_exit: Option<ExitRecord>,
    pub next_restart_at_ms: Option<u64>,
    pub started_at_ms: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct PaneSupervisor {
    policy: RestartPolicy,
    state: LifecycleState,
    restart_count: u32,
    /// Times (unix ms) of automatic restarts, oldest first; the budget is the count still
    /// inside the window.
    restart_log: VecDeque<u64>,
    consecutive_failures: u32,
    started_at_ms: Option<u64>,
    last_exit: Option<ExitRecord>,
    next_restart_at_ms: Option<u64>,
    stop_requested: bool,
    restart_requested: bool,
}

impl PaneSupervisor {
    /// A supervisor for a pane that has not started yet (call [`Self::on_started`] after the
    /// spawn succeeds).
    pub fn new(policy: RestartPolicy) -> Self {
        Self {
            policy,
            state: LifecycleState::Exited,
            restart_count: 0,
            restart_log: VecDeque::new(),
            consecutive_failures: 0,
            started_at_ms: None,
            last_exit: None,
            next_restart_at_ms: None,
            stop_requested: false,
            restart_requested: false,
        }
    }

    /// Rebuilds a supervisor from a persisted record after a daemon restart. The process did
    /// not survive, and ADR-002 forbids reviving automatically, so `Live`, `Backoff` and
    /// `Restarting` all come back as `Exited` (a pending backoff timer is dropped, not
    /// resumed); `Stopped` and `Failed` stay as they were.
    pub fn restore(p: PersistedRestartState) -> Self {
        let state = match p.state {
            LifecycleState::Stopped => LifecycleState::Stopped,
            LifecycleState::Failed => LifecycleState::Failed,
            _ => LifecycleState::Exited,
        };
        Self {
            policy: p.policy,
            state,
            restart_count: p.restart_count,
            restart_log: p.restart_log_ms.into(),
            consecutive_failures: p.consecutive_failures,
            started_at_ms: None,
            last_exit: p.last_exit,
            next_restart_at_ms: None,
            stop_requested: false,
            restart_requested: false,
        }
    }

    pub fn persisted(&self) -> PersistedRestartState {
        PersistedRestartState {
            policy: self.policy.clone(),
            state: self.state,
            restart_count: self.restart_count,
            restart_log_ms: self.restart_log.iter().copied().collect(),
            consecutive_failures: self.consecutive_failures,
            last_exit: self.last_exit,
        }
    }

    pub fn state(&self) -> LifecycleState {
        self.state
    }

    pub fn policy(&self) -> &RestartPolicy {
        &self.policy
    }

    pub fn next_restart_at_ms(&self) -> Option<u64> {
        self.next_restart_at_ms
    }

    pub fn restart_count(&self) -> u32 {
        self.restart_count
    }

    pub fn last_exit(&self) -> Option<ExitRecord> {
        self.last_exit
    }

    pub fn restart_pending(&self) -> bool {
        self.state.restart_pending()
    }

    fn window_start(&self, now: u64) -> u64 {
        if self.policy.window_ms == 0 {
            0
        } else {
            now.saturating_sub(self.policy.window_ms)
        }
    }

    fn restarts_in_window(&self, now: u64) -> u32 {
        let start = self.window_start(now);
        self.restart_log.iter().filter(|&&t| t > start).count() as u32
    }

    pub fn status(&self, now: u64) -> PaneStatusView {
        PaneStatusView {
            state: self.state,
            policy: self.policy.clone(),
            restart_count: self.restart_count,
            restarts_in_window: self.restarts_in_window(now),
            consecutive_failures: self.consecutive_failures,
            last_exit: self.last_exit,
            next_restart_at_ms: self.next_restart_at_ms,
            started_at_ms: self.started_at_ms,
        }
    }

    /// The process is running (first spawn, or after a restart).
    pub fn on_started(&mut self, now: u64) {
        self.state = LifecycleState::Live;
        self.started_at_ms = Some(now);
        self.next_restart_at_ms = None;
        self.stop_requested = false;
        self.restart_requested = false;
    }

    /// The process exited (`signal` is the signal the daemon sent, if any).
    pub fn on_exit(
        &mut self,
        now: u64,
        code: Option<i32>,
        signal: Option<i32>,
        unit: f64,
    ) -> ExitKind {
        self.last_exit = Some(ExitRecord {
            code,
            signal,
            at_ms: now,
        });
        let ran_for = self.started_at_ms.map(|s| now.saturating_sub(s));
        self.started_at_ms = None;
        self.next_restart_at_ms = None;

        if self.stop_requested {
            self.stop_requested = false;
            self.restart_requested = false;
            self.state = LifecycleState::Stopped;
            return ExitKind::Stopped;
        }
        if self.restart_requested {
            self.restart_requested = false;
            self.state = LifecycleState::Restarting;
            self.next_restart_at_ms = Some(now);
            return ExitKind::ManualRestart;
        }
        self.after_failure(now, code, ran_for, unit)
    }

    /// Spawning the process failed. Counts as an immediate failed run, so a pane whose launch
    /// can never succeed (missing binary, deleted argument file) still ends in `Failed`.
    pub fn on_spawn_failed(&mut self, now: u64, unit: f64) -> ExitKind {
        self.started_at_ms = None;
        self.next_restart_at_ms = None;
        self.after_failure(now, None, Some(0), unit)
    }

    fn after_failure(
        &mut self,
        now: u64,
        code: Option<i32>,
        ran_for: Option<u64>,
        unit: f64,
    ) -> ExitKind {
        let wants_restart = match self.policy.mode {
            RestartMode::Never => false,
            RestartMode::OnFailure => code != Some(0),
            RestartMode::Always | RestartMode::UnlessStopped => true,
        };
        if !wants_restart {
            self.state = LifecycleState::Exited;
            return ExitKind::NotRestarted;
        }
        if ran_for.is_some_and(|d| d >= self.policy.healthy_after_ms) {
            self.consecutive_failures = 0;
        }
        let start = self.window_start(now);
        while self.restart_log.front().is_some_and(|&t| t <= start) {
            self.restart_log.pop_front();
        }
        if self.restart_log.len() as u64 >= u64::from(self.policy.max_restarts) {
            self.state = LifecycleState::Failed;
            return ExitKind::Failed;
        }
        let delay_ms = self.policy.delay_ms(self.consecutive_failures, unit);
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let at_ms = now.saturating_add(delay_ms);
        self.state = LifecycleState::Backoff;
        self.next_restart_at_ms = Some(at_ms);
        ExitKind::Scheduled { delay_ms, at_ms }
    }

    /// True when a scheduled (or manual) restart is due.
    pub fn due(&self, now: u64) -> bool {
        self.state.restart_pending() && self.next_restart_at_ms.is_some_and(|t| t <= now)
    }

    /// A restart is about to be spawned. `automatic` restarts are charged to the budget.
    pub fn begin_restart(&mut self, now: u64, automatic: bool) {
        self.restart_count = self.restart_count.saturating_add(1);
        if automatic {
            self.restart_log.push_back(now);
        }
        self.next_restart_at_ms = None;
        self.state = LifecycleState::Restarting;
    }

    /// A restart is due as soon as the process that just exited is reaped (its exit was already
    /// accounted for but not yet published). Not charged to the budget.
    pub fn schedule_manual_restart_now(&mut self, now: u64) {
        self.stop_requested = false;
        self.restart_requested = false;
        self.state = LifecycleState::Restarting;
        self.next_restart_at_ms = Some(now);
    }

    /// Operator stop. Returns true if a live process must be signaled.
    pub fn request_stop(&mut self) -> bool {
        self.restart_requested = false;
        self.next_restart_at_ms = None;
        let process_running = matches!(
            self.state,
            LifecycleState::Live | LifecycleState::Restarting
        ) && self.started_at_ms.is_some()
            || self.stop_requested;
        if process_running {
            self.stop_requested = true;
        }
        self.state = LifecycleState::Stopped;
        process_running
    }

    /// Operator restart. Returns true if a live process must be signaled first; false means
    /// the process is already dead and the caller should respawn now. `reset_backoff` clears
    /// the backoff delay and the window budget.
    pub fn request_restart(&mut self, reset_backoff: bool) -> bool {
        if reset_backoff {
            self.consecutive_failures = 0;
            self.restart_log.clear();
        }
        self.stop_requested = false;
        let process_running = self.started_at_ms.is_some();
        if process_running {
            self.restart_requested = true;
            self.state = LifecycleState::Restarting;
        }
        process_running
    }

    /// Operator start of a dead pane: a fresh run, so the budget starts over.
    pub fn reset_for_start(&mut self) {
        self.consecutive_failures = 0;
        self.restart_log.clear();
        self.stop_requested = false;
        self.restart_requested = false;
        self.next_restart_at_ms = None;
    }

    /// Clears the backoff delay and the window budget. A `Failed` pane becomes `Exited` (the
    /// process is still dead; use `StartPane` to run it).
    pub fn reset_budget(&mut self) {
        self.consecutive_failures = 0;
        self.restart_log.clear();
        if self.state == LifecycleState::Failed {
            self.state = LifecycleState::Exited;
        }
    }

    /// Replaces the policy. A pending backoff the new policy would not have scheduled is
    /// cancelled; a pending one it would have is left to fire on its original schedule.
    pub fn set_policy(&mut self, policy: RestartPolicy) {
        self.policy = policy;
        if self.state == LifecycleState::Backoff {
            let still_wanted = match self.policy.mode {
                RestartMode::Never => false,
                RestartMode::OnFailure => self.last_exit.and_then(|e| e.code) != Some(0),
                RestartMode::Always | RestartMode::UnlessStopped => true,
            };
            if !still_wanted {
                self.state = LifecycleState::Exited;
                self.next_restart_at_ms = None;
            }
        }
    }
}

/// What happened, for a client watching a pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleEventKind {
    /// The process exited (always sent, whatever happens next).
    Exited,
    /// A restart was scheduled after a delay.
    RestartScheduled,
    /// The process was restarted.
    Restarted,
    /// The restart budget ran out.
    Failed,
    /// An operator stop took effect.
    Stopped,
}

/// One lifecycle transition, broadcast on the pane's lifecycle channel.
#[derive(Clone, Debug, PartialEq)]
pub struct LifecycleEvent {
    pub kind: LifecycleEventKind,
    /// The pane's state after this event.
    pub state: LifecycleState,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<i32>,
    pub delay_ms: Option<u64>,
    pub next_restart_at_ms: Option<u64>,
    /// The new process id (`Restarted` only).
    pub pid: Option<u32>,
    pub restart_count: u32,
    pub at_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(mode: RestartMode) -> RestartPolicy {
        RestartPolicy {
            mode,
            max_restarts: 3,
            window_ms: 60_000,
            initial_delay_ms: 1_000,
            multiplier: 2.0,
            max_delay_ms: 8_000,
            jitter: 0.0,
            healthy_after_ms: 10_000,
        }
    }

    /// Runs one failed run of `ran_ms` ending at `*now + ran_ms`, restarting if scheduled.
    fn fail_after(s: &mut PaneSupervisor, now: &mut u64, ran_ms: u64, code: i32) -> ExitKind {
        s.on_started(*now);
        *now += ran_ms;
        let kind = s.on_exit(*now, Some(code), None, 0.5);
        if let ExitKind::Scheduled { at_ms, .. } = kind {
            *now = at_ms;
            assert!(s.due(*now));
            s.begin_restart(*now, true);
        }
        kind
    }

    #[test]
    fn never_policy_should_leave_an_exited_pane_exited() {
        let mut s = PaneSupervisor::new(policy(RestartMode::Never));
        s.on_started(0);
        assert_eq!(s.on_exit(10, Some(1), None, 0.5), ExitKind::NotRestarted);
        assert_eq!(s.state(), LifecycleState::Exited);
        assert!(!s.restart_pending());
    }

    #[test]
    fn on_failure_should_restart_nonzero_and_unknown_codes_but_not_zero() {
        let mut s = PaneSupervisor::new(policy(RestartMode::OnFailure));
        s.on_started(0);
        assert_eq!(s.on_exit(10, Some(0), None, 0.5), ExitKind::NotRestarted);
        s.on_started(20);
        assert!(matches!(
            s.on_exit(30, Some(2), None, 0.5),
            ExitKind::Scheduled { .. }
        ));
        s.on_started(40);
        assert!(matches!(
            s.on_exit(50, None, None, 0.5),
            ExitKind::Scheduled { .. }
        ));
    }

    #[test]
    fn always_should_restart_a_clean_exit() {
        let mut s = PaneSupervisor::new(policy(RestartMode::Always));
        s.on_started(0);
        assert!(matches!(
            s.on_exit(10, Some(0), None, 0.5),
            ExitKind::Scheduled { .. }
        ));
    }

    #[test]
    fn backoff_should_double_each_consecutive_failure_up_to_the_cap() {
        let mut s = PaneSupervisor::new(policy(RestartMode::Always));
        let mut p = policy(RestartMode::Always);
        p.max_restarts = 100;
        s.set_policy(p);
        let mut now = 0;
        let delays: Vec<u64> = (0..6)
            .map(|_| match fail_after(&mut s, &mut now, 1, 1) {
                ExitKind::Scheduled { delay_ms, .. } => delay_ms,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(delays, vec![1_000, 2_000, 4_000, 8_000, 8_000, 8_000]);
    }

    #[test]
    fn crash_loop_should_end_in_failed_when_the_budget_is_exhausted() {
        let mut s = PaneSupervisor::new(policy(RestartMode::Always));
        let mut now = 0;
        for _ in 0..3 {
            assert!(matches!(
                fail_after(&mut s, &mut now, 1, 1),
                ExitKind::Scheduled { .. }
            ));
        }
        assert_eq!(fail_after(&mut s, &mut now, 1, 1), ExitKind::Failed);
        assert_eq!(s.state(), LifecycleState::Failed);
        assert!(!s.restart_pending());
        assert!(!s.due(u64::MAX));
    }

    #[test]
    fn a_healthy_run_should_reset_the_backoff_delay() {
        let mut s = PaneSupervisor::new(policy(RestartMode::Always));
        let mut now = 0;
        let first = fail_after(&mut s, &mut now, 1, 1);
        let second = fail_after(&mut s, &mut now, 1, 1);
        assert!(matches!(
            first,
            ExitKind::Scheduled {
                delay_ms: 1_000,
                ..
            }
        ));
        assert!(matches!(
            second,
            ExitKind::Scheduled {
                delay_ms: 2_000,
                ..
            }
        ));
        let after_healthy = fail_after(&mut s, &mut now, 10_000, 1);
        assert!(matches!(
            after_healthy,
            ExitKind::Scheduled {
                delay_ms: 1_000,
                ..
            }
        ));
    }

    #[test]
    fn a_run_just_under_healthy_after_should_not_reset_the_backoff() {
        let mut s = PaneSupervisor::new(policy(RestartMode::Always));
        let mut now = 0;
        fail_after(&mut s, &mut now, 1, 1);
        let next = fail_after(&mut s, &mut now, 9_999, 1);
        assert!(matches!(
            next,
            ExitKind::Scheduled {
                delay_ms: 2_000,
                ..
            }
        ));
    }

    #[test]
    fn restarts_older_than_the_window_should_stop_counting_against_the_budget() {
        let mut s = PaneSupervisor::new(policy(RestartMode::Always));
        let mut now = 0;
        for _ in 0..3 {
            fail_after(&mut s, &mut now, 1, 1);
        }
        // Three restarts are spent; wait out the window and the next failure is allowed again.
        now += 60_001;
        assert!(matches!(
            fail_after(&mut s, &mut now, 1, 1),
            ExitKind::Scheduled { .. }
        ));
    }

    #[test]
    fn a_restart_exactly_at_the_window_edge_should_have_expired() {
        let mut s = PaneSupervisor::new(policy(RestartMode::Always));
        s.on_started(0);
        s.on_exit(0, Some(1), None, 0.5);
        s.begin_restart(1_000, true);
        s.on_started(1_000);
        assert_eq!(s.status(61_000).restarts_in_window, 0);
        assert_eq!(s.status(60_999).restarts_in_window, 1);
    }

    #[test]
    fn zero_window_should_make_the_budget_a_lifetime_budget() {
        let mut p = policy(RestartMode::Always);
        p.window_ms = 0;
        let mut s = PaneSupervisor::new(p);
        let mut now = 0;
        for _ in 0..3 {
            fail_after(&mut s, &mut now, 1, 1);
        }
        now += 10_000_000_000;
        assert_eq!(fail_after(&mut s, &mut now, 1, 1), ExitKind::Failed);
    }

    #[test]
    fn zero_max_restarts_should_fail_on_the_first_exit_that_wants_a_restart() {
        let mut p = policy(RestartMode::Always);
        p.max_restarts = 0;
        let mut s = PaneSupervisor::new(p);
        s.on_started(0);
        assert_eq!(s.on_exit(1, Some(1), None, 0.5), ExitKind::Failed);
    }

    #[test]
    fn jitter_should_stay_inside_the_documented_band() {
        let mut p = policy(RestartMode::Always);
        p.jitter = 0.25;
        p.max_delay_ms = 100_000_000;
        for failures in 0..8u32 {
            let base = 1_000.0 * 2f64.powi(failures as i32);
            for step in 0..=100 {
                let unit = f64::from(step) / 100.0;
                let d = p.delay_ms(failures, unit) as f64;
                assert!(
                    d >= (base * 0.75).floor() - 1.0,
                    "{d} < band for {failures}"
                );
                assert!(d <= (base * 1.25).ceil() + 1.0, "{d} > band for {failures}");
            }
        }
        assert_eq!(p.delay_ms(0, 0.0), 750);
        assert_eq!(p.delay_ms(0, 0.5), 1_000);
    }

    #[test]
    fn a_zero_initial_delay_should_stay_zero_however_many_failures() {
        let mut p = policy(RestartMode::Always);
        p.initial_delay_ms = 0;
        assert_eq!(p.delay_ms(0, 0.5), 0);
        assert_eq!(p.delay_ms(1100, 0.5), 0);
        assert_eq!(p.delay_ms(u32::MAX, 0.99), 0);
    }

    #[test]
    fn jitter_should_never_push_the_delay_past_the_max() {
        let mut p = policy(RestartMode::Always);
        p.jitter = 1.0;
        p.max_delay_ms = 5_000;
        assert!(p.delay_ms(30, 0.999_999) <= 5_000);
        assert_eq!(p.delay_ms(u32::MAX, 0.5), 5_000);
    }

    #[test]
    fn system_jitter_should_produce_values_in_the_unit_interval() {
        let j = SystemJitter::default();
        let samples: Vec<f64> = (0..1000).map(|_| j.unit()).collect();
        assert!(samples.iter().all(|u| (0.0..1.0).contains(u)));
        assert!(samples.windows(2).any(|w| w[0] != w[1]));
    }

    #[test]
    fn stop_should_suppress_the_restart_even_under_always() {
        let mut s = PaneSupervisor::new(policy(RestartMode::Always));
        s.on_started(0);
        assert!(s.request_stop());
        assert_eq!(s.state(), LifecycleState::Stopped);
        assert_eq!(s.on_exit(5, Some(143), Some(15), 0.5), ExitKind::Stopped);
        assert_eq!(s.state(), LifecycleState::Stopped);
        assert!(!s.restart_pending());
        assert_eq!(s.last_exit().unwrap().signal, Some(15));
    }

    #[test]
    fn stop_during_backoff_should_cancel_the_pending_restart() {
        let mut s = PaneSupervisor::new(policy(RestartMode::Always));
        s.on_started(0);
        s.on_exit(1, Some(1), None, 0.5);
        assert_eq!(s.state(), LifecycleState::Backoff);
        assert!(!s.request_stop(), "no process to signal during backoff");
        assert_eq!(s.state(), LifecycleState::Stopped);
        assert!(!s.due(u64::MAX));
    }

    #[test]
    fn manual_restart_of_a_live_pane_should_respawn_without_delay_or_budget_cost() {
        let mut s = PaneSupervisor::new(policy(RestartMode::Never));
        s.on_started(0);
        assert!(s.request_restart(false));
        assert_eq!(s.state(), LifecycleState::Restarting);
        assert_eq!(
            s.on_exit(5, Some(143), Some(15), 0.5),
            ExitKind::ManualRestart
        );
        assert!(s.due(5));
        s.begin_restart(5, false);
        s.on_started(5);
        assert_eq!(s.status(5).restarts_in_window, 0);
        assert_eq!(s.restart_count(), 1);
    }

    #[test]
    fn a_second_restart_request_should_not_stack() {
        let mut s = PaneSupervisor::new(policy(RestartMode::Never));
        s.on_started(0);
        assert!(s.request_restart(false));
        assert!(s.request_restart(false));
        assert_eq!(s.on_exit(5, Some(0), None, 0.5), ExitKind::ManualRestart);
        s.begin_restart(5, false);
        s.on_started(5);
        assert_eq!(s.restart_count(), 1);
    }

    #[test]
    fn stop_after_restart_request_should_win_and_restart_after_stop_should_win_back() {
        let mut s = PaneSupervisor::new(policy(RestartMode::Never));
        s.on_started(0);
        s.request_restart(false);
        s.request_stop();
        assert_eq!(s.on_exit(5, Some(0), None, 0.5), ExitKind::Stopped);

        let mut s = PaneSupervisor::new(policy(RestartMode::Never));
        s.on_started(0);
        s.request_stop();
        s.request_restart(false);
        assert_eq!(s.on_exit(5, Some(0), None, 0.5), ExitKind::ManualRestart);
    }

    #[test]
    fn restart_with_reset_should_clear_the_backoff_and_budget() {
        let mut s = PaneSupervisor::new(policy(RestartMode::Always));
        let mut now = 0;
        for _ in 0..3 {
            fail_after(&mut s, &mut now, 1, 1);
        }
        assert_eq!(fail_after(&mut s, &mut now, 1, 1), ExitKind::Failed);
        assert!(!s.request_restart(true));
        s.begin_restart(now, false);
        s.on_started(now);
        assert_eq!(s.status(now).restarts_in_window, 0);
        assert_eq!(s.status(now).consecutive_failures, 0);
    }

    #[test]
    fn reset_budget_should_move_failed_to_exited_without_starting_anything() {
        let mut s = PaneSupervisor::new(policy(RestartMode::Always));
        let mut now = 0;
        for _ in 0..4 {
            fail_after(&mut s, &mut now, 1, 1);
        }
        assert_eq!(s.state(), LifecycleState::Failed);
        s.reset_budget();
        assert_eq!(s.state(), LifecycleState::Exited);
        assert_eq!(s.status(now).restarts_in_window, 0);
    }

    #[test]
    fn spawn_failures_should_walk_the_same_backoff_into_failed() {
        let mut s = PaneSupervisor::new(policy(RestartMode::Always));
        s.on_started(0);
        let mut now = 0;
        let mut kind = s.on_exit(now, Some(1), None, 0.5);
        let mut scheduled = 0;
        while let ExitKind::Scheduled { at_ms, .. } = kind {
            scheduled += 1;
            now = at_ms;
            s.begin_restart(now, true);
            kind = s.on_spawn_failed(now, 0.5);
        }
        assert_eq!(kind, ExitKind::Failed);
        assert_eq!(scheduled, 3);
    }

    #[test]
    fn set_policy_to_never_should_cancel_a_pending_backoff() {
        let mut s = PaneSupervisor::new(policy(RestartMode::Always));
        s.on_started(0);
        s.on_exit(1, Some(1), None, 0.5);
        s.set_policy(policy(RestartMode::Never));
        assert_eq!(s.state(), LifecycleState::Exited);
        assert!(!s.due(u64::MAX));
    }

    #[test]
    fn set_policy_keeping_restarts_should_leave_the_pending_schedule_alone() {
        let mut s = PaneSupervisor::new(policy(RestartMode::Always));
        s.on_started(0);
        s.on_exit(1, Some(1), None, 0.5);
        let at = s.next_restart_at_ms();
        s.set_policy(policy(RestartMode::OnFailure));
        assert_eq!(s.state(), LifecycleState::Backoff);
        assert_eq!(s.next_restart_at_ms(), at);
    }

    #[test]
    fn restore_should_keep_counters_and_demote_in_flight_states_to_exited() {
        let mut s = PaneSupervisor::new(policy(RestartMode::Always));
        let mut now = 0;
        fail_after(&mut s, &mut now, 1, 7);
        s.on_started(now);
        s.on_exit(now + 1, Some(7), None, 0.5);
        assert_eq!(s.state(), LifecycleState::Backoff);
        let p = s.persisted();
        let r = PaneSupervisor::restore(p.clone());
        assert_eq!(r.state(), LifecycleState::Exited);
        assert_eq!(r.next_restart_at_ms(), None);
        assert_eq!(r.restart_count(), s.restart_count());
        assert_eq!(r.persisted().restart_log_ms, p.restart_log_ms);
        assert_eq!(r.persisted().consecutive_failures, p.consecutive_failures);
        assert_eq!(r.last_exit().unwrap().code, Some(7));

        for kept in [LifecycleState::Stopped, LifecycleState::Failed] {
            let mut p = p.clone();
            p.state = kept;
            assert_eq!(PaneSupervisor::restore(p).state(), kept);
        }
    }

    #[test]
    fn policy_should_round_trip_through_json_and_accept_a_sparse_record() {
        let p = policy(RestartMode::UnlessStopped);
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<RestartPolicy>(&json).unwrap(), p);
        let sparse: RestartPolicy = serde_json::from_str(r#"{"mode":"on_failure"}"#).unwrap();
        assert_eq!(sparse.mode, RestartMode::OnFailure);
        assert_eq!(sparse.max_restarts, RestartPolicy::default().max_restarts);
    }

    #[test]
    fn validate_should_reject_nonsense() {
        let mut p = RestartPolicy::default();
        assert!(p.validate().is_ok());
        p.multiplier = 0.5;
        assert!(p.validate().is_err());
        p.multiplier = f64::NAN;
        assert!(p.validate().is_err());
        p = RestartPolicy::default();
        p.jitter = 1.5;
        assert!(p.validate().is_err());
        p = RestartPolicy::default();
        p.max_delay_ms = 1;
        assert!(p.validate().is_err());
    }

    #[test]
    fn manual_clock_should_only_move_when_told() {
        let c = ManualClock::new(100);
        assert_eq!(c.now_ms(), 100);
        c.advance(50);
        assert_eq!(c.now_ms(), 150);
        c.set(7);
        assert_eq!(c.now_ms(), 7);
    }
}
