//! Metadata kept outside the device operation lock.
use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub(crate) use ictl_wda::diagnostics::lock_unpoisoned as lock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthProbe {
    pub sequence: u64,
    pub runner_id: u64,
    pub sampled_at_unix_ms: u64,
    pub age_ms: u64,
    pub stale: bool,
    pub duration_ms: u64,
    pub category: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActiveOperation {
    pub operation_id: u64,
    pub tool: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeSkip {
    pub sampled_at_unix_ms: u64,
    pub age_ms: u64,
    pub active_operation: Option<ActiveOperation>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HealthDiagnostics {
    pub runner: Option<ictl_wda::diagnostics::RunnerSnapshot>,
    pub last_probe: Option<HealthProbe>,
    pub last_success: Option<HealthProbe>,
    pub consecutive_failures: u32,
    pub last_skip: Option<ProbeSkip>,
    pub stale: bool,
}
#[derive(Default)]
pub(crate) struct Diagnostics {
    health: HealthDiagnostics,
    runner_id: Option<u64>,
    active: Option<ActiveOperation>,
    probe_time: Option<Instant>,
    success_time: Option<Instant>,
    skip_time: Option<Instant>,
    runner: Option<ictl_wda::diagnostics::RunnerDiagnostics>,
}
impl Diagnostics {
    pub fn reset(&mut self, runner_id: u64) {
        if self.runner_id != Some(runner_id) {
            self.runner_id = Some(runner_id);
            self.health = HealthDiagnostics::default();
            self.probe_time = None;
            self.success_time = None;
            self.skip_time = None;
        }
    }
    pub fn stop(&self, reason: ictl_wda::diagnostics::StopReason) {
        if let Some(runner) = &self.runner {
            runner.stop(reason);
        }
    }
    pub fn set_runner(&mut self, runner: ictl_wda::diagnostics::RunnerDiagnostics) {
        self.reset(runner.snapshot().identity.runner_id);
        self.runner = Some(runner);
    }
    pub fn complete(&mut self, runner_id: u64, duration: Duration, category: &'static str) -> bool {
        self.reset(runner_id);
        let recovered = category == "ready" && self.health.consecutive_failures > 0;
        let probe = HealthProbe {
            sequence: self.health.last_probe.as_ref().map_or(1, |p| p.sequence + 1),
            runner_id,
            sampled_at_unix_ms: unix_ms(),
            age_ms: 0,
            stale: false,
            duration_ms: duration.as_millis() as u64,
            category: category.into(),
        };
        let now = Instant::now();
        if category == "ready" {
            self.health.last_success = Some(probe.clone());
            self.success_time = Some(now);
            self.health.consecutive_failures = 0;
        } else {
            self.health.consecutive_failures = self.health.consecutive_failures.saturating_add(1);
        }
        self.health.last_probe = Some(probe);
        self.probe_time = Some(now);
        recovered
    }
    pub fn skip(&mut self) {
        self.skip_time = Some(Instant::now());
        self.health.last_skip =
            Some(ProbeSkip { sampled_at_unix_ms: unix_ms(), age_ms: 0, active_operation: self.active.clone() });
        let runner = self.runner.as_ref().map(|r| r.snapshot());
        tracing::info!(event="wda_health_probe_skipped",scope="supervisor_device_lock",runner_id=?runner.as_ref().map(|r|r.identity.runner_id),phone_pid=?runner.as_ref().and_then(|r|r.phone_pid),native_pid=std::process::id(),native_start_id=ictl_wda::diagnostics::native_start_id(),operation_id=?self.active.as_ref().map(|a|a.operation_id),tool=?self.active.as_ref().map(|a|a.tool.as_str()));
    }
    pub fn snapshot(&self) -> HealthDiagnostics {
        let mut s = self.health.clone();
        s.runner = self.runner.as_ref().map(|r| r.snapshot());
        for (probe, at) in [(&mut s.last_probe, self.probe_time), (&mut s.last_success, self.success_time)] {
            if let (Some(probe), Some(at)) = (probe, at) {
                probe.age_ms = at.elapsed().as_millis() as u64;
                probe.stale = probe.age_ms > 30_000;
            }
        }
        s.stale = s.last_probe.as_ref().is_none_or(|p| p.stale);
        if let (Some(skip), Some(at)) = (&mut s.last_skip, self.skip_time) {
            skip.age_ms = at.elapsed().as_millis() as u64;
        }
        s
    }
    pub fn active(shared: &Arc<Mutex<Self>>, operation_id: u64, tool: String) -> OperationGuard {
        lock(shared).active = Some(ActiveOperation { operation_id, tool });
        OperationGuard(shared.clone())
    }
}
pub(crate) struct OperationGuard(Arc<Mutex<Diagnostics>>);
impl Drop for OperationGuard {
    fn drop(&mut self) {
        lock(&self.0).active = None;
    }
}
fn unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn success_then_failure_preserves_last_success_and_recovery() {
        let mut d = Diagnostics::default();
        d.complete(1, Duration::from_millis(12), "ready");
        d.complete(1, Duration::from_millis(20), "response_timeout");
        let s = d.snapshot();
        assert_eq!(s.last_probe.unwrap().sequence, 2);
        assert_eq!(s.last_success.unwrap().sequence, 1);
        assert_eq!(s.consecutive_failures, 1);
        assert!(d.complete(1, Duration::ZERO, "ready"));
        assert_eq!(d.snapshot().consecutive_failures, 0);
    }
    #[test]
    fn stale_without_probe_and_runner_replacement_resets_health() {
        let mut d = Diagnostics::default();
        assert!(d.snapshot().stale);
        d.complete(1, Duration::ZERO, "ready");
        d.probe_time = Some(Instant::now() - Duration::from_secs(31));
        assert!(d.snapshot().last_probe.unwrap().stale);
        d.reset(2);
        assert!(d.snapshot().last_probe.is_none());
        assert!(d.snapshot().stale);
    }
    #[test]
    fn busy_skip_carries_active_identity_without_fabricating_probe() {
        let shared = Arc::new(Mutex::new(Diagnostics::default()));
        let guard = Diagnostics::active(&shared, 42, "tap".into());
        shared.lock().unwrap().skip();
        let s = shared.lock().unwrap().snapshot();
        assert!(s.last_probe.is_none());
        assert_eq!(s.last_skip.unwrap().active_operation.unwrap().operation_id, 42);
        drop(guard);
        assert!(shared.lock().unwrap().active.is_none());
    }
}
