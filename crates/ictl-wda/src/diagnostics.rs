//! Bounded evidence for one native process generation and XCTest runner.
use idevice::dvt::xctest::diagnostics::{ProcessEvent, ProcessObserver, ProcessState};
use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, Mutex, OnceLock},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

/// 拿锁；锁被一个 panic 毒化了也照常拿到里面的数据。这里的锁只保护诊断记录、进度这类可以容忍
/// 写了一半的数据：一次 panic 不能让之后的每次访问（包括 `status`）都跟着 panic
pub fn lock_unpoisoned<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub fn native_start_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| uuid::Uuid::new_v4().to_string())
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerIdentity {
    pub native_pid: u32,
    pub native_start_id: String,
    pub runner_id: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Unattributed,
    StartupTimeout,
    StartupCancellation,
    HealthRestart,
    DeviceOffline,
    DeviceReplacement,
    DaemonShutdown,
    UsbmuxdReplacement,
    WdaReinstalled,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessObservation {
    pub sampled_at_unix_ms: u64,
    pub age_ms: u64,
    pub stale: bool,
    pub state: String,
    pub category: String,
    pub duration_ms: u64,
    pub monitoring: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerSnapshot {
    pub identity: RunnerIdentity,
    pub phone_pid: Option<u64>,
    pub last_process_observation: Option<ProcessObservation>,
    pub stop_reason: Option<StopReason>,
    pub exit_category: Option<String>,
    pub event_sequence: u64,
}
#[derive(Clone)]
pub struct RunnerDiagnostics(Arc<Mutex<State>>);
struct State {
    snapshot: RunnerSnapshot,
    process_time: Option<Instant>,
}
impl RunnerDiagnostics {
    pub fn new(runner_id: u64) -> Self {
        Self(Arc::new(Mutex::new(State {
            snapshot: RunnerSnapshot {
                identity: RunnerIdentity {
                    native_pid: std::process::id(),
                    native_start_id: native_start_id().into(),
                    runner_id,
                },
                phone_pid: None,
                last_process_observation: None,
                stop_reason: None,
                exit_category: None,
                event_sequence: 0,
            },
            process_time: None,
        })))
    }
    pub fn snapshot(&self) -> RunnerSnapshot {
        let s = lock_unpoisoned(&self.0);
        let mut snapshot = s.snapshot.clone();
        if let (Some(observation), Some(at)) = (&mut snapshot.last_process_observation, s.process_time) {
            observation.age_ms = at.elapsed().as_millis() as u64;
            observation.stale = observation.age_ms > 30_000;
        }
        snapshot
    }
    pub fn stop(&self, reason: StopReason) {
        let mut s = lock_unpoisoned(&self.0);
        s.snapshot.stop_reason.get_or_insert(reason);
    }
    pub fn exited(&self, category: &'static str) {
        lock_unpoisoned(&self.0).snapshot.exit_category = Some(category.into());
    }
    pub(crate) fn next_sequence(&self) -> u64 {
        let mut s = lock_unpoisoned(&self.0);
        s.snapshot.event_sequence += 1;
        s.snapshot.event_sequence
    }
    pub fn event(&self, event: &'static str) {
        let mut s = lock_unpoisoned(&self.0);
        s.snapshot.event_sequence += 1;
        let p = &s.snapshot;
        tracing::info!(event, native_pid=p.identity.native_pid, native_start_id=%p.identity.native_start_id, runner_id=p.identity.runner_id, phone_pid=?p.phone_pid, event_seq=p.event_sequence, stop_reason=?p.stop_reason, exit_category=?p.exit_category, process_state=?p.last_process_observation.as_ref().map(|o|o.state.as_str()), process_category=?p.last_process_observation.as_ref().map(|o|o.category.as_str()), process_age_ms=?s.process_time.map(|at|at.elapsed().as_millis() as u64), probe_duration_ms=?p.last_process_observation.as_ref().map(|o|o.duration_ms), monitoring=?p.last_process_observation.as_ref().map(|o|o.monitoring));
    }
}
impl ProcessObserver for RunnerDiagnostics {
    fn observe(&self, event: ProcessEvent) {
        let mut s = lock_unpoisoned(&self.0);
        match event {
            ProcessEvent::Launched { pid } => {
                s.snapshot.phone_pid = Some(pid);
            }
            ProcessEvent::Observed { pid, state, category, duration_ms, monitoring } => {
                s.snapshot.phone_pid = Some(pid);
                s.process_time = Some(Instant::now());
                s.snapshot.last_process_observation = Some(ProcessObservation {
                    sampled_at_unix_ms: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()
                        as u64,
                    age_ms: 0,
                    stale: false,
                    state: match state {
                        ProcessState::Running => "running",
                        ProcessState::NotRunning => "not_running",
                        ProcessState::Unknown => "unknown",
                    }
                    .into(),
                    category: category.into(),
                    duration_ms,
                    monitoring,
                });
            }
        }
        drop(s);
        self.event(match event {
            ProcessEvent::Launched { .. } => "phone_process_launched",
            ProcessEvent::Observed { .. } => "phone_process_observed",
        });
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// 一次 panic 毒化了锁之后，诊断记录照样能读写（`status` 不能跟着 panic）
    #[test]
    fn poisoned_diagnostics_stay_usable() {
        let d = RunnerDiagnostics::new(3);
        let inner = d.0.clone();
        let _ = std::thread::spawn(move || {
            let _held = inner.lock().unwrap();
            panic!("poison");
        })
        .join();
        assert!(d.0.is_poisoned());
        d.event("after_poison");
        assert_eq!(d.snapshot().identity.runner_id, 3);
    }

    #[test]
    fn cleanup_then_socket_error_keeps_requested_first_cause() {
        let d = RunnerDiagnostics::new(1);
        d.stop(StopReason::DaemonShutdown);
        d.exited("socket_error");
        d.stop(StopReason::Unattributed);
        assert_eq!(d.snapshot().stop_reason, Some(StopReason::DaemonShutdown));
        assert_eq!(d.snapshot().exit_category.as_deref(), Some("socket_error"));
    }
    #[test]
    fn runner_generation_and_process_age_are_explicit() {
        let d = RunnerDiagnostics::new(1);
        d.observe(ProcessEvent::Launched { pid: 1234 });
        d.observe(ProcessEvent::Observed {
            pid: 1234,
            state: ProcessState::Unknown,
            category: "request_timeout",
            duration_ms: 5000,
            monitoring: false,
        });
        d.0.lock().unwrap().process_time = Some(Instant::now() - std::time::Duration::from_secs(31));
        let s = d.snapshot();
        assert!(!s.identity.native_start_id.is_empty());
        assert_eq!(s.phone_pid, Some(1234));
        assert!(s.last_process_observation.unwrap().stale);
    }
}
