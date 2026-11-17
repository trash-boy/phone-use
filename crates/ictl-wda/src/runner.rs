//! 启动手机上的 WebDriverAgent（XCTest runner）。
//!
//! 用 idevice 的 `XCUITestService` 跑测试计划；它自己经 usbmuxd 建 CoreDeviceProxy 隧道，和设备层的隧道互不影响。
//! 就绪判定 = 本进程持有的 runner 任务还活着 **且** `/status` ready（手机上残留的 WDA 在 runner 退出后还会响应几十秒）。
//! 启动时还要等新 runner 开始执行测试计划后才问 `/status`：在那之前应答的只可能是残留的旧 WDA。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::Instrument;

use idevice::{
    IdeviceError, IdeviceService,
    dvt::xctest::{TestConfig, XCUITestService, listener::XCUITestListener},
    installation_proxy::InstallationProxyClient,
    provider::IdeviceProvider,
};
use tokio::task::JoinHandle;

use crate::diagnostics::{RunnerDiagnostics, StopReason, native_start_id};
use crate::http::DeviceTransport;
use crate::{Result, WdaClient, WdaError, is_automation_pin_error};

pub const WDA_PORT: u16 = 8100;
static NEXT_RUNNER: AtomicU64 = AtomicU64::new(1);

/// runner 等「Enable UI Automation」密码的时长是 60 秒（runner 自己的超时），多留 5 秒收它的报错
const AUTOMATION_WAIT: Duration = Duration::from_secs(65);
/// 连接 installation_proxy、查 runner 的安装信息的时限（idevice 读应答没有超时）
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(20);
/// 一直没收到「开始执行测试计划」回调时，过了这么久也开始问 `/status`：
/// 万一某个 iOS 版本上 idevice 解不出这个回调，不能因此永远起不来
const PLAN_START_FALLBACK: Duration = Duration::from_secs(30);

/// runner 调试日志里和「Enable UI Automation」相关的信号。
/// 手机重启后 runner 一启动就进入 `enabling automation mode` 等待（手机上弹密码框），60 秒没输密码报
/// `Timed out while enabling automation mode.`。这个报错经 `_XCT_initializationForUITestingDidFailWithError:`
/// 发回，idevice 解码失败（只给出 unexpected response，不调回调），所以从调试日志里认
#[derive(Debug, PartialEq, Clone, Copy)]
enum AutomationSignal {
    /// 开始等 automation mode（已授权时会马上结束）
    Waiting,
    /// 等待结束（成功或超时都有这一行）
    Done,
    /// UI 测试初始化失败，消息里带原因
    Failed,
}

fn automation_signal(msg: &str) -> Option<AutomationSignal> {
    if msg.contains("Failed to initialize for UI testing") {
        Some(AutomationSignal::Failed)
    } else if msg.contains("entering wait loop") && msg.contains("enabling automation mode") {
        Some(AutomationSignal::Waiting)
    } else if msg.contains("cleaning up enabling automation mode") {
        Some(AutomationSignal::Done)
    } else {
        None
    }
}

/// 手机拒绝启动 runner 时回的是 NSError，但 idevice 收到的是没解开的归档（字段都是 `Uid(n)` 引用），读不出文字，
/// 只报 `unexpected response`。首次安装后最常见的原因是还没信任开发者（iPad mini 5 / iPadOS 26.6.1 真机：
/// 点「信任」之前这样报，之后下一次就能启动），在报错里提示一句
fn launch_hint(msg: String) -> String {
    if msg.contains("unexpected response") {
        ictl_l10n::trf!(
            "{msg}(如果是第一次在这台手机上装 WDA:到 设置 → 通用 → VPN 与设备管理 里信任你的 Apple ID 开发者,需要联网)",
            "{msg} (if this is the first WDA install on this phone: trust your Apple ID developer under Settings → General → VPN & Device Management, which needs internet)"
        )
    } else {
        msg
    }
}

/// XCTest 过程中收集的信息：报错文字（识别密码框超时用），以及 runner 是否正在等 automation mode
#[derive(Default)]
struct Progress {
    runner_id: u64,
    diagnostics: Option<RunnerDiagnostics>,
    event_seq: u64,
    initialization_count: u64,
    authorization_wait_count: u64,
    plan_start_count: u64,
    log_messages: u64,
    debug_messages: u64,
    errors: Vec<String>,
    exit_reason: Option<String>,
    automation_wait: Option<tokio::time::Instant>,
    /// 新 runner 已开始执行测试计划（WDA 的 HTTP 服务在测试里启动，一定晚于它）
    plan_started: bool,
}

fn emit_exit_summary(p: &Progress, diagnostics: &RunnerDiagnostics, lifetime: Duration, category: &'static str) {
    // The gateway can cancel transport before native observes shutdown intent.
    // An absent local reason is unknown, never proof of an unrequested exit.
    let local_cleanup_attribution = match diagnostics.snapshot().stop_reason {
        Some(reason) if reason != StopReason::Unattributed => "requested",
        _ => "unknown",
    };
    tracing::warn!(
        event = "xctest_exit_summary",
        runner_id = p.runner_id,
        native_pid = std::process::id(),
        native_start_id = native_start_id(),
        lifetime_ms = lifetime.as_millis() as u64,
        category,
        scope = "xctest_session",
        local_cleanup_attribution,
        phone_log_messages = p.log_messages,
        phone_debug_messages = p.debug_messages,
        initialization_count = p.initialization_count,
        authorization_wait_count = p.authorization_wait_count,
        plan_start_count = p.plan_start_count,
        callback_events = p.event_seq
    );
}

/// `ICTL_XCTEST_REPORT_ACTIVITIES=0` (or `false`/`off`) asks the runner not to report each
/// XCTest activity to the host. Experimental: a real session sent ~16k debug messages
/// (~12 MB) over the browser uplink in 12 minutes. Unset keeps the upstream default.
fn report_activities(value: Option<&str>) -> bool {
    !matches!(value.map(str::trim), Some("0" | "false" | "off"))
}

struct ErrorListener(Arc<Mutex<Progress>>);

impl ErrorListener {
    fn push(&self, s: &str) {
        crate::diagnostics::lock_unpoisoned(&self.0).errors.push(s.to_owned());
    }

    fn event(&self, event: &'static str) {
        let mut p = crate::diagnostics::lock_unpoisoned(&self.0);
        p.event_seq += 1;
        let sequence = p.diagnostics.as_ref().map_or(p.event_seq, RunnerDiagnostics::next_sequence);
        let phone_pid = p.diagnostics.as_ref().and_then(|d| d.snapshot().phone_pid);
        match event {
            "xctest_ui_initializing" => p.initialization_count += 1,
            "xctest_authorization_wait" => p.authorization_wait_count += 1,
            "xctest_plan_started" => p.plan_start_count += 1,
            _ => {}
        }
        // Fixed lifecycle categories only. Never promote raw device messages,
        // activity titles, typed text or callback error descriptions to INFO.
        tracing::info!(
            event,
            runner_id = p.runner_id,
            native_pid = std::process::id(),
            native_start_id = native_start_id(),
            event_seq = sequence,
            phone_pid = ?phone_pid,
            initialization_count = p.initialization_count,
            authorization_wait_count = p.authorization_wait_count,
            plan_start_count = p.plan_start_count
        );
    }
}

// 生命周期以固定字段记录到 INFO；原始手机消息只留在 DEBUG。
impl XCUITestListener for ErrorListener {
    async fn log_message(&mut self, message: &str) -> std::result::Result<(), IdeviceError> {
        crate::diagnostics::lock_unpoisoned(&self.0).log_messages += 1;
        tracing::debug!("xctest log: {message}");
        Ok(())
    }

    async fn log_debug_message(&mut self, message: &str) -> std::result::Result<(), IdeviceError> {
        tracing::debug!("xctest debug: {message}");
        let signal = automation_signal(message);
        {
            let mut p = crate::diagnostics::lock_unpoisoned(&self.0);
            p.debug_messages += 1;
            match signal {
                Some(AutomationSignal::Waiting) => p.automation_wait = Some(tokio::time::Instant::now()),
                Some(AutomationSignal::Done) => p.automation_wait = None,
                Some(AutomationSignal::Failed) => p.errors.push(message.to_owned()),
                None => {}
            }
        }
        match signal {
            Some(AutomationSignal::Waiting) => self.event("xctest_authorization_wait"),
            Some(AutomationSignal::Done) => self.event("xctest_authorization_wait_ended"),
            Some(AutomationSignal::Failed) => self.event("xctest_ui_initialization_failed"),
            None => {}
        }
        Ok(())
    }

    async fn did_begin_initializing_for_ui_testing(&mut self) -> std::result::Result<(), IdeviceError> {
        self.event("xctest_ui_initializing");
        Ok(())
    }

    async fn test_bundle_ready(&mut self) -> std::result::Result<(), IdeviceError> {
        self.event("xctest_bundle_ready");
        Ok(())
    }

    async fn test_runner_ready_with_capabilities(&mut self) -> std::result::Result<(), IdeviceError> {
        self.event("xctest_runner_ready");
        Ok(())
    }

    async fn did_begin_executing_test_plan(&mut self) -> std::result::Result<(), IdeviceError> {
        self.event("xctest_plan_started");
        crate::diagnostics::lock_unpoisoned(&self.0).plan_started = true;
        Ok(())
    }

    async fn did_finish_executing_test_plan(&mut self) -> std::result::Result<(), IdeviceError> {
        self.event("xctest_plan_finished");
        Ok(())
    }

    async fn test_bundle_ready_with_protocol_version(
        &mut self,
        version: u64,
        minimum: u64,
    ) -> std::result::Result<(), IdeviceError> {
        self.event("xctest_bundle_protocol_ready");
        tracing::info!(
            event = "xctest_protocol_version",
            runner_id = crate::diagnostics::lock_unpoisoned(&self.0).runner_id,
            version,
            minimum
        );
        Ok(())
    }

    async fn initialization_for_ui_testing_did_fail(
        &mut self,
        description: &str,
    ) -> std::result::Result<(), IdeviceError> {
        self.push(description);
        self.event("xctest_ui_initialization_failed");
        Ok(())
    }

    async fn did_fail_to_bootstrap(&mut self, description: &str) -> std::result::Result<(), IdeviceError> {
        self.push(description);
        self.event("xctest_bootstrap_failed");
        Err(IdeviceError::UnexpectedResponse(format!("test runner failed to bootstrap: {description}")))
    }
}

pub struct WdaRunner {
    task: JoinHandle<std::result::Result<(), IdeviceError>>,
    progress: Arc<Mutex<Progress>>,
    started: std::time::Instant,
    diagnostics: RunnerDiagnostics,
    ready: bool,
}

impl WdaRunner {
    /// 启动 runner 并等到 `/status` ready。`bundle_id` 是签名后带团队 ID 的 xctrunner Bundle ID。
    pub async fn start(
        provider: Arc<dyn IdeviceProvider>,
        bundle_id: &str,
        timeout: Duration,
    ) -> Result<(Self, WdaClient<DeviceTransport>)> {
        Self::start_with_diagnostics(provider, bundle_id, timeout, Self::diagnostic_session()).await
    }

    pub fn diagnostic_session() -> RunnerDiagnostics {
        RunnerDiagnostics::new(NEXT_RUNNER.fetch_add(1, Ordering::Relaxed))
    }

    pub async fn start_with_diagnostics(
        provider: Arc<dyn IdeviceProvider>,
        bundle_id: &str,
        timeout: Duration,
        diagnostics: RunnerDiagnostics,
    ) -> Result<(Self, WdaClient<DeviceTransport>)> {
        let runner_id = diagnostics.snapshot().identity.runner_id;
        diagnostics.event("xctest_start_requested");
        let lookup = async {
            let mut ip =
                InstallationProxyClient::connect(&*provider).await.map_err(|e| WdaError::Unavailable(e.to_string()))?;
            TestConfig::from_installation_proxy(&mut ip, bundle_id, None).await.map_err(|e| match e {
                IdeviceError::AppNotInstalled => WdaError::NotInstalled(bundle_id.to_owned()),
                other => WdaError::StartFailed(other.to_string()),
            })
        };
        let mut cfg = tokio::time::timeout(LOOKUP_TIMEOUT, lookup).await.map_err(|_| {
            WdaError::StartFailed(ictl_l10n::trf!(
                "查 WDA 安装信息超时（{} 秒）",
                "querying the WDA install info timed out ({} s)",
                LOOKUP_TIMEOUT.as_secs()
            ))
        })??;
        cfg.report_activities = report_activities(std::env::var("ICTL_XCTEST_REPORT_ACTIVITIES").ok().as_deref());
        tracing::info!(event = "xctest_report_activities", enabled = cfg.report_activities);

        let progress =
            Arc::new(Mutex::new(Progress { runner_id, diagnostics: Some(diagnostics.clone()), ..Progress::default() }));
        let task = {
            let (provider, progress, diagnostics) = (provider.clone(), progress.clone(), diagnostics.clone());
            let span = tracing::info_span!("xctest_runner", runner_id, native_start_id = native_start_id());
            let run = async move {
                let started = std::time::Instant::now();
                diagnostics.event("xctest_started");
                let result = XCUITestService::new(provider)
                    .with_process_observer(Arc::new(diagnostics.clone()))
                    .run(cfg, &mut ErrorListener(progress.clone()), None)
                    .await;
                let category = match &result {
                    Ok(()) => "test_plan_completed",
                    Err(IdeviceError::TestRunnerDisconnected) => "runner_transport_disconnected",
                    Err(IdeviceError::XcTestTimeout(_)) => "test_plan_timeout",
                    Err(IdeviceError::Socket(_)) => "socket_error",
                    Err(_) => "xctest_error",
                };
                diagnostics.exited(category);
                diagnostics.event("xctest_exited");
                let reason = match &result {
                    Ok(()) => "test plan completed".to_owned(),
                    Err(e) => e.to_string(),
                };
                crate::diagnostics::lock_unpoisoned(&progress).exit_reason = Some(reason.clone());
                emit_exit_summary(
                    &crate::diagnostics::lock_unpoisoned(&progress),
                    &diagnostics,
                    started.elapsed(),
                    category,
                );
                result
            };
            tokio::spawn(run.instrument(span))
        };
        let mut runner =
            Self { task, progress: progress.clone(), started: std::time::Instant::now(), diagnostics, ready: false };
        let client = WdaClient::new(DeviceTransport::new(provider, WDA_PORT));

        let failure = |extra: Option<String>| {
            let p = crate::diagnostics::lock_unpoisoned(&progress);
            let mut text = p.errors.clone();
            text.extend(extra);
            let joined = text.join("; ");
            // 报错文字没收到、但 runner 停在等 automation mode 也算
            if is_automation_pin_error(&joined) || p.automation_wait.is_some() {
                WdaError::NeedsAutomationPin
            } else {
                WdaError::StartFailed(launch_hint(joined))
            }
        };

        let t0 = tokio::time::Instant::now();
        let mut deadline = t0 + timeout;
        let mut announced = false;
        let mut fallback_logged = false;
        loop {
            // runner 在等手机上输密码：等满它自己的 60 秒，期间输了密码就能直接就绪
            if let Some(since) = crate::diagnostics::lock_unpoisoned(&progress).automation_wait {
                deadline = deadline.max(since + AUTOMATION_WAIT);
                if !announced {
                    announced = true;
                    tracing::info!("runner 在等手机上的「Enable UI Automation」密码");
                }
            }
            if runner.task.is_finished() {
                let r = (&mut runner.task).await;
                let msg = match r {
                    Ok(Ok(())) => ictl_l10n::tr!("runner 提前结束", "the runner ended early").to_owned(),
                    Ok(Err(e)) => e.to_string(),
                    Err(e) => e.to_string(),
                };
                return Err(failure(Some(msg)));
            }
            let plan_started = crate::diagnostics::lock_unpoisoned(&progress).plan_started;
            if !plan_started && t0.elapsed() >= PLAN_START_FALLBACK && !fallback_logged {
                fallback_logged = true;
                tracing::info!(
                    "{} 秒内没收到 runner 开始执行测试计划的回调，改为直接查 /status",
                    PLAN_START_FALLBACK.as_secs()
                );
            }
            if (plan_started || fallback_logged) && client.ready().await.unwrap_or(false) {
                tracing::info!(event = "xctest_http_ready", runner_id, elapsed_ms = t0.elapsed().as_millis() as u64);
                runner.ready = true;
                return Ok((runner, client));
            }
            if tokio::time::Instant::now() >= deadline {
                runner.diagnostics.stop(StopReason::StartupTimeout);
                runner.task.abort();
                return Err(failure(Some(
                    ictl_l10n::tr!("WDA 在限定时间内没有就绪", "WDA didn't become ready in time").to_owned(),
                )));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// 单测用：一个一直活着的 runner（不连设备），配合 [`crate::fake`] 的假传输
    #[cfg(any(test, feature = "test-util"))]
    pub fn fake_alive() -> Self {
        let diagnostics = Self::diagnostic_session();
        Self {
            task: tokio::spawn(std::future::pending()),
            progress: Arc::new(Mutex::new(Progress::default())),
            started: std::time::Instant::now(),
            diagnostics,
            ready: true,
        }
    }

    /// runner 任务是否还活着。死了就算手机上的 WDA 还在响应也不能用
    pub fn alive(&self) -> bool {
        !self.task.is_finished()
    }

    pub fn exit_reason(&self) -> Option<String> {
        crate::diagnostics::lock_unpoisoned(&self.progress).exit_reason.clone()
    }

    /// 停掉 runner（即丢掉它，见 Drop）
    pub fn diagnostics(&self) -> RunnerDiagnostics {
        self.diagnostics.clone()
    }
    pub fn stop(self) {
        self.stop_with_reason(StopReason::Unattributed);
    }
    pub fn stop_with_reason(self, reason: StopReason) {
        self.diagnostics.stop(reason);
    }
}

/// 丢掉就停：守护进程的后台启动任务可能在 start() 中途被取消，不能留下一个没人管的 XCTest 会话
impl Drop for WdaRunner {
    fn drop(&mut self) {
        self.diagnostics.stop(if self.ready || self.task.is_finished() {
            StopReason::Unattributed
        } else {
            StopReason::StartupCancellation
        });
        self.diagnostics.event("xctest_stop_requested");
        let runner_id = self.progress.lock().map(|p| p.runner_id).unwrap_or(0);
        tracing::info!(
            event = "xctest_stop_summary",
            runner_id,
            native_pid = std::process::id(),
            lifetime_ms = self.started.elapsed().as_millis() as u64,
            finished = self.task.is_finished()
        );
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn report_activities_is_only_disabled_explicitly() {
        for on in [None, Some(""), Some("1"), Some("true")] {
            assert!(report_activities(on), "{on:?}");
        }
        for off in [Some("0"), Some(" false "), Some("off")] {
            assert!(!report_activities(off), "{off:?}");
        }
    }

    #[test]
    fn transport_closes_before_native_stop_emits_unknown_local_attribution() {
        use std::collections::BTreeMap;
        use tracing_subscriber::{Layer, layer::Context, prelude::*};
        type Fields = BTreeMap<String, String>;
        #[derive(Clone, Default)]
        struct Capture(Arc<Mutex<Vec<Fields>>>);
        struct Visitor(Fields);
        impl tracing::field::Visit for Visitor {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.0.insert(field.name().into(), value.into());
            }
            fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
                self.0.insert(field.name().into(), value.to_string());
            }
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.insert(field.name().into(), format!("{value:?}"));
            }
        }
        impl<S: tracing::Subscriber> Layer<S> for Capture {
            fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
                let mut fields = Visitor(Fields::new());
                event.record(&mut fields);
                if fields.0.get("event").map(String::as_str) == Some("xctest_exit_summary") {
                    self.0.lock().unwrap().push(fields.0);
                }
            }
        }
        for initial_reason in [None, Some(StopReason::Unattributed)] {
            let capture = Capture::default();
            let subscriber = tracing_subscriber::registry().with(capture.clone());
            let diagnostics = RunnerDiagnostics::new(7);
            if let Some(reason) = initial_reason {
                diagnostics.stop(reason);
            }
            let progress = Progress { runner_id: 7, ..Progress::default() };
            tracing::subscriber::with_default(subscriber, || {
                tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                    // Model gateway cancellation dropping an active native transport.
                    // Native shutdown-intent delivery deliberately occurs only after
                    // its transport error and exit summary have been observed.
                    use tokio::io::AsyncReadExt;
                    let (mut native_transport, gateway_transport) = tokio::io::duplex(16);
                    let read = tokio::spawn(async move { native_transport.read_exact(&mut [0]).await });
                    drop(gateway_transport);
                    assert!(read.await.unwrap().is_err());
                    diagnostics.exited("socket_error");
                    emit_exit_summary(&progress, &diagnostics, Duration::from_millis(1), "socket_error");
                });
            });
            let fields = capture.0.lock().unwrap();
            assert_eq!(fields.len(), 1);
            assert_eq!(
                fields[0].get("local_cleanup_attribution").map(String::as_str),
                Some("unknown"),
                "emitted transport-before-stop evidence: {:?}",
                fields[0]
            );
            assert!(!fields[0].contains_key("requested_cleanup"));
            drop(fields);
            // Only after the error event does the same runner receive native's
            // explicit stop. The earlier emitted event remains unknown. A sticky
            // unattributed fallback must also remain honest about its uncertainty.
            diagnostics.stop(StopReason::DaemonShutdown);
            let subscriber = tracing_subscriber::registry().with(capture.clone());
            tracing::subscriber::with_default(subscriber, || {
                emit_exit_summary(&progress, &diagnostics, Duration::from_millis(2), "socket_error")
            });
            let fields = capture.0.lock().unwrap();
            assert_eq!(fields[0].get("local_cleanup_attribution").map(String::as_str), Some("unknown"));
            assert_eq!(
                fields[1].get("local_cleanup_attribution").map(String::as_str),
                Some(if initial_reason.is_none() { "requested" } else { "unknown" })
            );
        }
    }

    #[test]
    fn repeated_phone_callbacks_are_visible_at_production_info_level() {
        use std::io::Write;
        #[derive(Clone, Default)]
        struct Output(Arc<Mutex<Vec<u8>>>);
        impl Write for Output {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Output {
            type Writer = Self;
            fn make_writer(&'a self) -> Self {
                self.clone()
            }
        }
        let output = Output::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(output.clone())
            .with_env_filter("info,ictl_wda::runner=info,idevice=warn")
            .finish();
        let progress = Arc::new(Mutex::new(Progress { runner_id: 7, ..Progress::default() }));
        let mut listener = ErrorListener(progress.clone());
        tracing::subscriber::with_default(subscriber, || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                for _ in 0..2 {
                    listener.did_begin_initializing_for_ui_testing().await.unwrap();
                    listener
                        .log_debug_message("entering wait loop enabling automation mode PRIVATE_AUTH_PAYLOAD")
                        .await
                        .unwrap();
                    listener.log_debug_message("cleaning up enabling automation mode").await.unwrap();
                }
                listener.test_bundle_ready().await.unwrap();
                listener.test_bundle_ready_with_protocol_version(36, 22).await.unwrap();
                listener.test_runner_ready_with_capabilities().await.unwrap();
                listener.did_begin_executing_test_plan().await.unwrap();
                listener.did_finish_executing_test_plan().await.unwrap();
                listener.initialization_for_ui_testing_did_fail("PRIVATE_ERROR_PAYLOAD").await.unwrap();
                listener.log_message("PRIVATE_PHONE_PAYLOAD").await.unwrap();
            });
        });
        let text = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
        for event in [
            "xctest_ui_initializing",
            "xctest_authorization_wait",
            "xctest_authorization_wait_ended",
            "xctest_bundle_ready",
            "xctest_bundle_protocol_ready",
            "xctest_protocol_version",
            "xctest_runner_ready",
            "xctest_plan_started",
            "xctest_plan_finished",
            "xctest_ui_initialization_failed",
        ] {
            assert!(text.contains(event), "missing {event}: {text}");
        }
        assert!(
            text.contains("runner_id=7")
                && text.contains("initialization_count=2")
                && text.contains("authorization_wait_count=2")
                && text.contains("plan_start_count=1"),
            "{text}"
        );
        assert!(!text.contains("PRIVATE"), "{text}");
        let p = progress.lock().unwrap();
        assert!(p.plan_started && p.automation_wait.is_none());
        assert_eq!(p.errors, ["PRIVATE_ERROR_PAYLOAD"]);
        assert_eq!((p.log_messages, p.debug_messages, p.event_seq), (1, 4, 12));
    }

    #[test]
    fn untrusted_developer_hint_only_for_unexpected_response() {
        let m = launch_hint("unexpected response from device: unexpected response".into());
        assert!(m.contains("VPN 与设备管理"), "{m}");
        assert_eq!(launch_hint("WDA 在限定时间内没有就绪".into()), "WDA 在限定时间内没有就绪");
    }

    #[test]
    fn automation_signals_from_runner_debug_log() {
        // iOS 27.0.1 手机重启后真机日志（去掉时间和进程号前缀）
        let wait =
            "<XCTWaiter: 0x1089cebb0> entering wait loop for 60.00s with expectations: `enabling automation mode`";
        let done = "cleaning up enabling automation mode";
        let fail = "Failed to initialize for UI testing: Error Domain=com.apple.dt.XCTest.XCTFuture Code=1000 \
                    \"Timed out while enabling automation mode.\" UserInfo={NSLocalizedDescription=Timed out while enabling automation mode.}";
        assert_eq!(automation_signal(wait), Some(AutomationSignal::Waiting));
        assert_eq!(automation_signal(done), Some(AutomationSignal::Done));
        assert_eq!(automation_signal(fail), Some(AutomationSignal::Failed));
        assert!(is_automation_pin_error(fail));
        // 创建 future 那一行不算开始等待（已授权时也有）
        assert_eq!(automation_signal("Creating future for 'enabling automation mode' with timeout 60.00"), None);
        assert_eq!(automation_signal("Loading test bundle at: file:///..."), None);
    }
}
