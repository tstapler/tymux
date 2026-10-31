//! Drives [`PaneSupervisor`] from real pane exits and timers (ADR-0007).
//!
//! Locking: `Supervision::inner` is a leaf lock. While holding it this module touches only
//! pane-internal locks (parser, replay, pty handles, `spawn_lock`), never the engine's
//! `sessions`, `panes` or `window_index`, so engine code may call in while holding those. It
//! stays held across `Pane::respawn` on purpose: a stop, close or policy change then either
//! happens before the spawn (and prevents it) or after (and sees the new process), never in
//! between.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::sync::Notify;
use uuid::Uuid;

use crate::engine::EngineError;
use crate::pane::Pane;
use crate::restart::{
    Clock, ExitKind, Jitter, LifecycleEvent, LifecycleEventKind, LifecycleState, PaneStatusView,
    PaneSupervisor, PersistedRestartState, RestartMode, RestartPolicy,
};

/// Written into the stream when a restarted process starts, so a screen left in the alternate
/// buffer or a mouse mode by the dead process does not leak into its successor: SGR reset, leave
/// alt screen, show cursor, mouse and focus reporting off, bracketed paste off, normal cursor
/// keys and keypad.
const TERMINAL_RESET: &str = "\x1b[0m\x1b[?1049l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\
                              \x1b[?1006l\x1b[?1004l\x1b[?2004l\x1b[?1l\x1b>";

/// A pane's supervision state plus facts only the live pane knows.
#[derive(Clone, Debug)]
pub struct PaneStatus {
    pub pane_id: Uuid,
    pub view: PaneStatusView,
    /// Set only while a process is running.
    pub pid: Option<u32>,
    pub now_ms: u64,
}

/// What `stop` / `restart` did, so the caller knows whether to wait for the process to die.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalOutcome {
    /// A running process was signaled; await `Pane::wait_exit` for it to go.
    Signaled,
    /// There was no running process; the change is already complete.
    Immediate,
}

struct Entry {
    sup: PaneSupervisor,
    /// Empty for a pane restored from disk that has not been started yet.
    pane: Weak<Pane>,
}

impl Entry {
    fn is_pane(&self, pane: &Pane) -> bool {
        std::ptr::eq(self.pane.as_ptr(), pane)
    }

    fn running_pane(&self) -> Option<Arc<Pane>> {
        self.pane.upgrade().filter(|p| !p.is_exited())
    }
}

#[derive(Default)]
struct Inner {
    entries: HashMap<Uuid, Entry>,
    /// Panes whose supervision changed since the engine last persisted it.
    dirty: HashSet<Uuid>,
}

pub struct Supervision {
    inner: Mutex<Inner>,
    clock: Arc<dyn Clock>,
    jitter: Arc<dyn Jitter>,
    wake: Arc<Notify>,
}

impl Supervision {
    pub fn new(clock: Arc<dyn Clock>, jitter: Arc<dyn Jitter>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            clock,
            jitter,
            wake: Arc::new(Notify::new()),
        })
    }

    /// Pulsed whenever a pane exits or a timer may need re-arming; the daemon's supervisor task
    /// waits on it.
    pub fn wake(&self) -> Arc<Notify> {
        self.wake.clone()
    }

    pub fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// Starts supervising a freshly spawned pane under `policy`.
    pub fn register_live(self: &Arc<Self>, pane: &Arc<Pane>, policy: RestartPolicy) {
        let mut sup = PaneSupervisor::new(policy);
        sup.on_started(self.clock.now_ms());
        let mut inner = self.inner.lock().unwrap();
        inner.entries.insert(
            pane.id,
            Entry {
                sup,
                pane: Arc::downgrade(pane),
            },
        );
        inner.dirty.insert(pane.id);
        self.install_hook(pane, &mut inner);
    }

    /// Records a pane known only from a persisted record: no process, state restored.
    pub fn register_restored(
        &self,
        pane_id: Uuid,
        persisted: Option<PersistedRestartState>,
        default_policy: RestartPolicy,
    ) {
        let sup = match persisted {
            Some(p) => PaneSupervisor::restore(p),
            None => PaneSupervisor::new(default_policy),
        };
        self.inner.lock().unwrap().entries.insert(
            pane_id,
            Entry {
                sup,
                pane: Weak::new(),
            },
        );
    }

    /// Hands a restored pane its new process (ReviveSession / StartPane on a restored record).
    /// The policy is kept; the budget starts over, as for any operator start.
    pub fn adopt_started(self: &Arc<Self>, pane: &Arc<Pane>, fallback_policy: RestartPolicy) {
        let now = self.clock.now_ms();
        let mut inner = self.inner.lock().unwrap();
        let entry = inner.entries.entry(pane.id).or_insert_with(|| Entry {
            sup: PaneSupervisor::new(fallback_policy),
            pane: Weak::new(),
        });
        entry.sup.reset_for_start();
        entry.sup.on_started(now);
        entry.pane = Arc::downgrade(pane);
        inner.dirty.insert(pane.id);
        self.install_hook(pane, &mut inner);
    }

    /// Stops supervising `pane_id` and refuses any restart already in flight. Call before
    /// killing the pane.
    pub fn forget(&self, pane_id: Uuid) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(entry) = inner.entries.remove(&pane_id) {
            if let Some(pane) = entry.pane.upgrade() {
                pane.retire();
            }
        }
        inner.dirty.remove(&pane_id);
    }

    fn install_hook(self: &Arc<Self>, pane: &Arc<Pane>, inner: &mut Inner) {
        let this = self.clone();
        let exited_before_hook =
            pane.set_exit_hook(Arc::new(move |pane, code| this.handle_exit(pane, code)));
        if exited_before_hook {
            // The child died before the hook existed, so the reader thread skipped it. We hold
            // the lock handle_exit would take, so run its body directly.
            self.account_exit(inner, pane, pane.exit_code());
        }
    }

    /// Reader-thread entry point. Returns whether a restart is pending.
    fn handle_exit(&self, pane: &Pane, code: Option<i32>) -> bool {
        let mut inner = self.inner.lock().unwrap();
        self.account_exit(&mut inner, pane, code)
    }

    fn account_exit(&self, inner: &mut Inner, pane: &Pane, code: Option<i32>) -> bool {
        let now = self.clock.now_ms();
        let unit = self.jitter.unit();
        let Some(entry) = inner.entries.get_mut(&pane.id) else {
            return false;
        };
        if !entry.is_pane(pane) {
            return false;
        }
        let signal = pane.signal_sent();
        let kind = entry.sup.on_exit(now, code, signal, unit);
        announce_exit(entry, pane, kind, code, signal, now);
        let pending = entry.sup.restart_pending();
        // Under the supervision lock, so it orders with Stop and set_policy.
        pane.set_restart_pending(pending);
        inner.dirty.insert(pane.id);
        self.wake.notify_one();
        pending
    }

    /// Spawns the next process for `entry`. `automatic` restarts are charged to the budget.
    fn do_respawn(&self, entry: &mut Entry, automatic: bool) -> Result<Option<u32>, EngineError> {
        let pane_id = entry.pane.upgrade().map_or_else(Uuid::nil, |pane| pane.id);
        let Some(pane) = entry.pane.upgrade().filter(|p| !p.is_retired()) else {
            return Err(EngineError::PaneNotFound(pane_id));
        };
        let now = self.clock.now_ms();
        entry.sup.begin_restart(now, automatic);
        let restart_count = entry.sup.restart_count();
        let banner = |pid: Option<u32>| {
            let pid = pid.map_or_else(|| "?".to_string(), |p| p.to_string());
            format!(
                "{TERMINAL_RESET}\r\n\x1b[2m[tymux: restarted (pid {pid}, restart #{restart_count})]\x1b[0m\r\n"
            )
            .into_bytes()
        };
        match pane.respawn_with(&banner) {
            Ok(pid) => {
                entry.sup.on_started(now);
                pane.set_restart_pending(false);
                pane.emit_lifecycle(event(LifecycleEventKind::Restarted, &entry.sup, now, |e| {
                    e.pid = pid
                }));
                Ok(pid)
            }
            Err(e) => {
                let kind = entry.sup.on_spawn_failed(now, self.jitter.unit());
                let message = e.to_string();
                tracing::warn!(pane_id = %pane.id, error = %message, ?kind, "pane restart failed");
                if !pane.is_retired() {
                    announce_failed_spawn(entry, &pane, kind, &message, now);
                }
                pane.set_restart_pending(entry.sup.restart_pending());
                Err(EngineError::PaneStart {
                    pane_id: pane.id,
                    message,
                })
            }
        }
    }

    /// Runs every restart that is due and returns when the next one will be (unix ms), if any.
    /// The daemon's supervisor task calls this on every wake; tests call it after advancing a
    /// [`crate::ManualClock`].
    pub fn run_due(&self) -> Option<u64> {
        let now = self.clock.now_ms();
        let mut inner = self.inner.lock().unwrap();
        let due: Vec<Uuid> = inner
            .entries
            .iter()
            .filter(|(_, e)| e.sup.due(now))
            // The reader publishes `exited` just after the exit hook; a restart that is due
            // this instant waits for it (revisited at the deadline returned below).
            .filter(|(_, e)| e.pane.upgrade().is_some_and(|p| p.is_exited()))
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            let Inner { entries, dirty } = &mut *inner;
            let Some(entry) = entries.get_mut(&id) else {
                continue;
            };
            let automatic = entry.sup.state() == LifecycleState::Backoff;
            // A failed spawn is already recorded in the entry's state by do_respawn.
            let _ = self.do_respawn(entry, automatic);
            dirty.insert(id);
        }
        inner
            .entries
            .values()
            .filter(|e| e.sup.restart_pending())
            .filter_map(|e| e.sup.next_restart_at_ms())
            // A due restart skipped above is retried a millisecond later, not at once.
            .map(|at| at.max(now + 1))
            .min()
    }

    /// Panes whose supervision changed since the last call.
    pub fn take_dirty(&self) -> Vec<Uuid> {
        self.inner.lock().unwrap().dirty.drain().collect()
    }

    pub fn state_of(&self, pane_id: Uuid) -> Option<LifecycleState> {
        self.inner
            .lock()
            .unwrap()
            .entries
            .get(&pane_id)
            .map(|e| e.sup.state())
    }

    /// Every supervised pane's state, in one pass under one lock acquisition.
    pub fn states(&self) -> HashMap<Uuid, LifecycleState> {
        self.inner
            .lock()
            .unwrap()
            .entries
            .iter()
            .map(|(id, e)| (*id, e.sup.state()))
            .collect()
    }

    pub fn persisted_for(&self, pane_ids: &[Uuid]) -> HashMap<Uuid, PersistedRestartState> {
        let inner = self.inner.lock().unwrap();
        pane_ids
            .iter()
            .filter_map(|id| inner.entries.get(id).map(|e| (*id, e.sup.persisted())))
            .collect()
    }

    pub fn status(&self, pane_id: Uuid) -> Option<PaneStatus> {
        let inner = self.inner.lock().unwrap();
        let entry = inner.entries.get(&pane_id)?;
        let now_ms = self.clock.now_ms();
        Some(PaneStatus {
            pane_id,
            view: entry.sup.status(now_ms),
            pid: entry.running_pane().and_then(|p| p.pid()),
            now_ms,
        })
    }

    pub fn set_policy(&self, pane_id: Uuid, policy: RestartPolicy) -> Result<(), EngineError> {
        policy
            .validate()
            .map_err(EngineError::InvalidRestartPolicy)?;
        let mut inner = self.inner.lock().unwrap();
        let entry = inner
            .entries
            .get_mut(&pane_id)
            .ok_or(EngineError::PaneNotFound(pane_id))?;
        entry.sup.set_policy(policy);
        if let Some(pane) = entry.pane.upgrade() {
            pane.set_restart_pending(entry.sup.restart_pending());
        }
        inner.dirty.insert(pane_id);
        self.wake.notify_one();
        Ok(())
    }

    /// Stops the pane and keeps it from restarting until `start`. The process gets SIGTERM and
    /// SIGKILL after `grace`.
    pub fn stop(&self, pane_id: Uuid, grace: Duration) -> Result<SignalOutcome, EngineError> {
        let mut inner = self.inner.lock().unwrap();
        let entry = inner
            .entries
            .get_mut(&pane_id)
            .ok_or(EngineError::PaneNotFound(pane_id))?;
        let needs_signal = entry.sup.request_stop();
        let pane = entry.pane.upgrade();
        if let Some(pane) = &pane {
            pane.set_restart_pending(false);
        }
        let outcome = if needs_signal {
            if let Some(pane) = pane.filter(|p| !p.is_exited()) {
                // The exit hook emits the Stopped event once the process is really gone.
                pane.terminate(grace).map_err(|e| EngineError::PaneStart {
                    pane_id,
                    message: e.to_string(),
                })?;
            }
            SignalOutcome::Signaled
        } else {
            if let Some(pane) = pane {
                let now = self.clock.now_ms();
                pane.emit_lifecycle(event(LifecycleEventKind::Stopped, &entry.sup, now, |_| {}));
            }
            SignalOutcome::Immediate
        };
        inner.dirty.insert(pane_id);
        self.wake.notify_one();
        Ok(outcome)
    }

    /// Restarts the pane. A running process is signaled and replaced once it exits (reported as
    /// `Signaled`); a dead one is respawned now (`Immediate`).
    pub fn restart(
        &self,
        pane_id: Uuid,
        reset_backoff: bool,
        grace: Duration,
    ) -> Result<SignalOutcome, EngineError> {
        let mut inner = self.inner.lock().unwrap();
        let Inner { entries, dirty } = &mut *inner;
        let entry = entries
            .get_mut(&pane_id)
            .ok_or(EngineError::PaneNotFound(pane_id))?;
        let signaled = entry.sup.request_restart(reset_backoff);
        dirty.insert(pane_id);
        if signaled {
            if let Some(pane) = entry.running_pane() {
                pane.terminate(grace).map_err(|e| EngineError::PaneStart {
                    pane_id,
                    message: e.to_string(),
                })?;
            }
            self.wake.notify_one();
            return Ok(SignalOutcome::Signaled);
        }
        self.do_respawn(entry, false)?;
        Ok(SignalOutcome::Immediate)
    }

    /// Starts a pane that is not running. Fails with `PaneRunning` if it is.
    pub fn start(&self, pane_id: Uuid) -> Result<(), EngineError> {
        let mut inner = self.inner.lock().unwrap();
        let Inner { entries, dirty } = &mut *inner;
        let entry = entries
            .get_mut(&pane_id)
            .ok_or(EngineError::PaneNotFound(pane_id))?;
        if entry.running_pane().is_some() {
            return Err(EngineError::PaneRunning(pane_id));
        }
        entry.sup.reset_for_start();
        dirty.insert(pane_id);
        self.do_respawn(entry, false).map(|_| ())
    }

    pub fn reset_budget(&self, pane_id: Uuid) -> Result<(), EngineError> {
        let mut inner = self.inner.lock().unwrap();
        let entry = inner
            .entries
            .get_mut(&pane_id)
            .ok_or(EngineError::PaneNotFound(pane_id))?;
        entry.sup.reset_budget();
        inner.dirty.insert(pane_id);
        Ok(())
    }
}

fn event(
    kind: LifecycleEventKind,
    sup: &PaneSupervisor,
    now: u64,
    extra: impl FnOnce(&mut LifecycleEvent),
) -> LifecycleEvent {
    let last = sup.last_exit();
    let mut e = LifecycleEvent {
        kind,
        state: sup.state(),
        exit_code: last.and_then(|l| l.code),
        exit_signal: last.and_then(|l| l.signal),
        delay_ms: None,
        next_restart_at_ms: sup.next_restart_at_ms(),
        pid: None,
        restart_count: sup.restart_count(),
        at_ms: now,
    };
    extra(&mut e);
    e
}

fn describe_exit(code: Option<i32>, signal: Option<i32>) -> String {
    match (code, signal) {
        (_, Some(sig)) => format!("stopped by signal {sig}"),
        (Some(c), None) => format!("exited with code {c}"),
        (None, None) => "exited".to_string(),
    }
}

fn marker(text: &str) -> Vec<u8> {
    format!("\r\n\x1b[2m[tymux: {text}]\x1b[0m\r\n").into_bytes()
}

/// Writes the visible marker and the lifecycle events for an exit. A pane left on the default
/// `Never` policy with no operator action in play gets neither: its exit looks exactly as it
/// always has.
fn announce_exit(
    entry: &Entry,
    pane: &Pane,
    kind: ExitKind,
    code: Option<i32>,
    signal: Option<i32>,
    now: u64,
) {
    let what = describe_exit(code, signal);
    let sup = &entry.sup;
    pane.emit_lifecycle(event(LifecycleEventKind::Exited, sup, now, |_| {}));
    match kind {
        ExitKind::NotRestarted => {
            if sup.policy().mode != RestartMode::Never {
                pane.inject_output(&marker(&format!("process {what}; not restarting")));
            }
        }
        ExitKind::Scheduled { delay_ms, at_ms } => {
            pane.inject_output(&marker(&format!(
                "process {what}; restarting in {:.1}s",
                delay_ms as f64 / 1000.0
            )));
            pane.emit_lifecycle(event(LifecycleEventKind::RestartScheduled, sup, now, |e| {
                e.delay_ms = Some(delay_ms);
                e.next_restart_at_ms = Some(at_ms);
            }));
        }
        ExitKind::ManualRestart => {
            pane.inject_output(&marker(&format!("process {what}; restarting")));
        }
        ExitKind::Failed => {
            let p = sup.policy();
            pane.inject_output(&marker(&format!(
                "process {what}; gave up after {} restarts within {:.0}s",
                p.max_restarts,
                p.window_ms as f64 / 1000.0
            )));
            pane.emit_lifecycle(event(LifecycleEventKind::Failed, sup, now, |_| {}));
        }
        ExitKind::Stopped => {
            pane.inject_output(&marker(&format!("process {what}; stopped")));
            pane.emit_lifecycle(event(LifecycleEventKind::Stopped, sup, now, |_| {}));
        }
    }
}

/// Like [`announce_exit`], for a restart whose spawn failed.
fn announce_failed_spawn(entry: &Entry, pane: &Pane, kind: ExitKind, error: &str, now: u64) {
    let sup = &entry.sup;
    match kind {
        ExitKind::Scheduled { delay_ms, at_ms } => {
            pane.inject_output(&marker(&format!(
                "restart failed ({error}); retrying in {:.1}s",
                delay_ms as f64 / 1000.0
            )));
            pane.emit_lifecycle(event(LifecycleEventKind::RestartScheduled, sup, now, |e| {
                e.delay_ms = Some(delay_ms);
                e.next_restart_at_ms = Some(at_ms);
            }));
        }
        ExitKind::Failed => {
            pane.inject_output(&marker(&format!(
                "restart failed ({error}); restart budget exhausted"
            )));
            pane.emit_lifecycle(event(LifecycleEventKind::Failed, sup, now, |_| {}));
        }
        _ => pane.inject_output(&marker(&format!("start failed ({error})"))),
    }
}
