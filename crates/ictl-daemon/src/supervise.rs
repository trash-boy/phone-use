//! 巡检和后台任务：usbmuxd 看护、每 10 秒推进状态机、后台启动 WDA、重签、Apple ID 会话检查。

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use ictl_device::{Device, DeviceError, SYSFS_USB, Tunnel, UsbDevice};
use ictl_wda::{WdaClient, WdaError, WdaRunner, http::DeviceTransport};

use crate::core::{Daemon, DeviceProfile, Inner, close_hid};
use crate::machine::DeviceState;
use crate::notify::Event;
use ictl_wda::diagnostics::StopReason;

/// 正常 2～10 秒就绪。手机重启后未授权 automation 时，testmanagerd 握手每次卡 30 秒、要重试一两次，
/// 60 秒常常等不到 runner 启动，也就认不出「需要输入密码」
const WDA_START_TIMEOUT: Duration = Duration::from_secs(120);
/// HID 会话空闲这么久就关掉。它持有屏幕媒体流，开着时 iOS 当作屏幕被远程查看：不会自动锁屏，
/// 还会弹「viewed remotely」通知。重开约 0.5 秒
pub(crate) const HID_IDLE: Duration = Duration::from_secs(30);
/// usbmuxd 起来后这么久内就退出，算「起不来」，按退避重试
const USBMUXD_QUICK_EXIT: Duration = Duration::from_secs(30);
const USBMUXD_MAX_DELAY: Duration = Duration::from_secs(300);
/// 连续这么多轮列不出设备才当作离线
const LIST_ERRORS_OFFLINE: u32 = 3;
/// 开发者镜像连续挂载失败这么多次就清缓存重新下载
const DDI_FAILURES_INVALIDATE: u32 = 3;
/// 整个重签（登录会话、注册设备、签名、上传安装）的时限：苹果服务器或安装卡住时不能永远「正在重签」
const RESIGN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Apple ID 会话检查的时限
const SESSION_CHECK_TIMEOUT: Duration = Duration::from_secs(3 * 60);
/// 换了设备或重新接上后，这么久内就检查一次要不要重签（离线期间可能过期了，不能等满 RESIGN_CHECK_HOURS）
const RESIGN_CHECK_AFTER_CONNECT: Duration = Duration::from_secs(60);

/// 刚拉起 usbmuxd 后等这么久再列设备（让它建好 socket）。在锁外等
const USBMUXD_SETTLE: Duration = Duration::from_secs(1);
/// 巡检时 WDA 旋转角旧于这么久才重读：到下一轮巡检（10 秒后）它还在 HID 能用的时限（30 秒）内
const ROTATION_REFRESH_AFTER: Duration =
    Duration::from_secs(crate::tools::HID_ROTATION_MAX_AGE.as_secs() - crate::core::TICK.as_secs() - 5);

/// 巡检要在锁外做的 WDA 健康探测：只带传输的副本（`/status` 走自己的连接，不碰请求用的连接池）和当时的代数
pub(crate) struct HealthCheck {
    generation: u64,
    udid: String,
    /// 本进程 runner 活着时 WDA 客户端传输的副本；None 表示不用探测（没有 runner 或 runner 已退出）
    link: Option<crate::core::WdaLink>,
    device: Device,
    bundle: String,
}

impl HealthCheck {
    /// 问一次 `/status`，返回结果和用时
    pub(crate) async fn probe(&self) -> (ictl_wda::Result<bool>, Duration) {
        let started = Instant::now();
        let health = match &self.link {
            Some(link) => WdaClient::new(link.clone()).ready().await,
            None => Ok(false),
        };
        (health, started.elapsed())
    }
}

/// 后台启动 WDA 的结果
pub(crate) enum StartOutcome {
    Ready {
        runner: WdaRunner,
        client: WdaClient<DeviceTransport>,
        mounted: Option<String>,
    },
    NeedsUnlock,
    /// 开发者镜像没准备好。`mount_failed`：是挂载这一步失败（才可能是缓存坏了）；
    /// 建隧道、查询、下载失败都是 false
    Ddi {
        error: String,
        mount_failed: bool,
    },
    Wda(WdaError),
}

impl Daemon {
    // ------------------------------------------------------------------ usbmuxd

    /// 看护 usbmuxd：退出了就（按退避）重新拉起，`restart` 时强制重启。返回 true 表示刚拉起，
    /// 要等它建好 socket（[`USBMUXD_SETTLE`]）再列设备；调用方在锁外等，不持锁睡
    pub(crate) async fn ensure_usbmuxd(&self, g: &mut Inner, restart: bool) -> bool {
        if !self.cfg.manage_usbmuxd {
            return false;
        }
        let exited = match g.usbmuxd.as_mut().map(|c| c.try_wait()) {
            Some(Ok(None)) => None,
            Some(Ok(Some(st))) => Some(st.to_string()),
            Some(Err(e)) => Some(e.to_string()),
            None => Some(String::new()),
        };
        if exited.is_none() && !restart {
            return false;
        }
        if !restart {
            // 起来就退出（例如宿主机上已有 usbmuxd 占着 socket）：退避重试，不要每 10 秒拆一次会话、写一行日志
            if let Some(why) = exited.filter(|w| !w.is_empty())
                && g.usbmuxd_started.is_some_and(|t| t.elapsed() < USBMUXD_QUICK_EXIT)
            {
                g.usbmuxd_delay = (g.usbmuxd_delay * 2).clamp(Duration::from_secs(10), USBMUXD_MAX_DELAY);
                g.usbmuxd_retry_at = Some(Instant::now() + g.usbmuxd_delay);
                g.usbmuxd_started = None;
                self.log.log(&format!(
                    "usbmuxd 启动后很快就退出了（{why}），{} 秒后再试；宿主机已在运行 usbmuxd 时设 ICTL_MANAGE_USBMUXD=0",
                    g.usbmuxd_delay.as_secs()
                ));
            }
            if g.usbmuxd_retry_at.is_some_and(|t| Instant::now() < t) {
                return false;
            }
        }
        // 经旧 usbmuxd 建立的连接（隧道、runner、WDA）都要失效：先停 HID 媒体流（还要经它），再停它
        self.drop_session(g, StopReason::UsbmuxdReplacement).await;
        if let Some(mut c) = g.usbmuxd.take() {
            let _ = c.kill().await;
        }
        let log = std::fs::OpenOptions::new().create(true).append(true).open(self.cfg.logs().join("usbmuxd.log"));
        let mut cmd = tokio::process::Command::new(&self.cfg.usbmuxd_bin);
        cmd.args(["-U", "root", "-f"]).kill_on_drop(true);
        if let Ok(f) = log
            && let Ok(f2) = f.try_clone()
        {
            cmd.stdout(f).stderr(f2);
        }
        match cmd.spawn() {
            Ok(c) => {
                g.usbmuxd = Some(c);
                g.usbmuxd_started = Some(Instant::now());
                if g.usbmuxd_retry_at.take().is_none() {
                    g.usbmuxd_delay = Duration::ZERO;
                }
                self.log.log("usbmuxd 已(重新)启动");
                true
            }
            Err(e) => {
                self.log.log(&format!("启动 usbmuxd 失败：{e}"));
                false
            }
        }
    }

    // ------------------------------------------------------------------ 状态机

    /// 每 10 秒一次。有请求在用设备就跳过这一轮（巡检不排队，下一轮再来）。
    ///
    /// 持锁只做改状态的部分，分几段：
    /// 1. 持锁：看护 usbmuxd、关空闲的 HID 会话、收后台启动 WDA 的结果
    /// 2. 不持锁：（刚拉起 usbmuxd 时先等它）列设备、数 sysfs 上的 iPhone/iPad
    /// 3. 持锁：推进状态机；要查 WDA 健康时只取出传输的副本和代数（[`HealthCheck`]）
    /// 4. 不持锁：`/status`（走单独的连接，最长约 15 秒）
    /// 5. 持锁：代数还对得上才用探测结果，再安排重签、会话检查
    pub async fn tick(self: Arc<Self>) {
        let started_usbmuxd = {
            let Ok(mut g) = self.inner.try_lock() else {
                crate::diagnostics::lock(&self.diagnostics).skip();
                return;
            };
            let g = &mut *g;
            let started = self.ensure_usbmuxd(g, false).await;
            if g.tunnel.as_ref().is_some_and(Tunnel::hid_open) && g.hid_used.is_none_or(|t| t.elapsed() >= HID_IDLE) {
                close_hid(g).await;
                tracing::info!("HID 会话空闲，已关闭（停止屏幕媒体流）");
            }
            self.poll_start(g).await;
            if started {
                self.publish(g);
            }
            started
        };
        if started_usbmuxd {
            tokio::time::sleep(USBMUXD_SETTLE).await;
        }
        let listed = ictl_device::list_devices().await;
        let on_usb = ictl_device::ios_usb_count(Path::new(SYSFS_USB));

        let check = {
            let Some(mut g) = self.relock().await else { return };
            let g = &mut *g;
            let devices = match listed {
                Ok(d) => {
                    g.list_errors = 0;
                    d
                }
                Err(e) => {
                    // usbmuxd 抖一下就当离线会拆掉整个会话：连续几轮都失败才算
                    g.list_errors += 1;
                    tracing::debug!("列设备失败（第 {} 次）：{e}", g.list_errors);
                    if g.list_errors < LIST_ERRORS_OFFLINE {
                        self.publish(g);
                        return;
                    }
                    Vec::new()
                }
            };
            self.note_connected(g, devices).await;
            let pinned = self.pinned(g);
            let udids: Vec<&str> = g.connected.iter().map(|d| d.udid.as_str()).collect();
            let current = g.device.as_ref().map(|d| d.udid.as_str());
            let target =
                crate::machine::pick_device(&udids, pinned.as_deref(), current).map(|i| g.connected[i].clone());
            // 只在容器里（自己看护 usbmuxd、没有 udev）需要；用系统的 usbmuxd 服务时插拔由 udev 处理，也重启不了它
            if self.cfg.manage_usbmuxd && g.mux.observe(on_usb, g.connected.len()) {
                self.log.log(&format!(
                    "USB 上有 {on_usb} 台 iPhone/iPad,usbmuxd 只认到 {} 台,重启 usbmuxd",
                    g.connected.len()
                ));
                // 刚拉起的 usbmuxd 还没认到设备，这一轮的列表作废，下一轮再看
                self.ensure_usbmuxd(g, true).await;
                self.publish(g);
                return;
            }
            let check = match target {
                None => {
                    self.offline(g).await;
                    None
                }
                Some(d) => {
                    let check = self.online(g, &d).await;
                    self.adopt_legacy_passcode(g);
                    check
                }
            };
            if check.is_none() {
                self.schedule_background(g);
                self.publish(g);
                return;
            }
            check
        };
        let Some(check) = check else { return };
        let health = check.probe().await;
        let Some(mut g) = self.relock().await else { return };
        let g = &mut *g;
        self.apply_health(g, check, health).await;
        self.schedule_background(g);
        self.publish(g);
    }

    /// 巡检在锁外做完一段之后重新拿锁。请求正占着时等它做完（最多一轮巡检的时间），等不到就放弃这一轮
    async fn relock(&self) -> Option<tokio::sync::MutexGuard<'_, Inner>> {
        match tokio::time::timeout(crate::core::TICK, self.inner.lock()).await {
            Ok(g) => Some(g),
            Err(_) => {
                crate::diagnostics::lock(&self.diagnostics).skip();
                None
            }
        }
    }

    /// 到时间就在后台做重签检查、验证 Apple ID 会话
    fn schedule_background(self: &Arc<Self>, g: &mut Inner) {
        let now = Instant::now();
        // 正在重签时有强制重签在等：不取走标记、不推迟，等这次重签结束后下一轮再来
        // （否则这次检查只会返回「正在重签」，标记就丢了）
        if now >= g.next_resign && !(g.resigning && g.resign_force_pending) {
            g.next_resign = now + self.cfg.resign_check;
            if g.device.is_some() && g.persisted.wda_bundle.is_some() && !g.resigning {
                let force = std::mem::take(&mut g.resign_force_pending);
                let d = self.clone();
                tokio::task::spawn_local(async move {
                    if let Err(e) = d.resign_now(force, None).await {
                        let mut g = d.inner.lock().await;
                        d.notify(&mut g, Event::ResignFailed(e));
                    }
                });
            }
        }
        if now >= g.next_session_check {
            g.next_session_check = now + self.cfg.session_check;
            tokio::task::spawn_local(self.clone().check_session());
        }
    }

    async fn offline(&self, g: &mut Inner) {
        let now = Instant::now();
        g.device_profile = DeviceProfile::NotChecked;
        g.wda_rotation = None;
        if g.device.take().is_some() {
            self.log.log("手机离线");
            // 名字、型号、iOS 版本是这台的：留着的话状态、提示文字会把它当成还在控制的设备
            g.info = None;
            self.drop_session(g, StopReason::DeviceOffline).await;
            g.offline_since = Some(now);
            g.apps.clear();
        }
        self.set_state(g, DeviceState::Absent);
        let since = *g.offline_since.get_or_insert(now);
        let alert = self.cfg.offline_alert;
        if !alert.is_zero() && now.duration_since(since) >= alert {
            self.notify(g, Event::DeviceOffline { minutes: alert.as_secs() / 60 });
        }
    }

    /// 持锁推进在线设备的状态机，直到要查 WDA 健康为止：返回要在锁外做的探测（[`HealthCheck`]）；
    /// 在前面某一步就有了结论（没配对、没签名……）返回 None
    pub(crate) async fn online(&self, g: &mut Inner, seen: &UsbDevice) -> Option<HealthCheck> {
        // 只比 UDID 不够：断开几秒又接上（短于离线判定、落在两轮巡检之间）时 UDID 不变，
        // usbmuxd 却换了编号，旧句柄的每个连接都报 bad device，永远恢复不了
        let fresh = !g.device.as_ref().is_some_and(|d| d.is_connection(seen));
        if fresh {
            match g.device.take() {
                Some(old) if old.udid == seen.udid => self.log.log(&format!(
                    "手机重新接入（usbmuxd 编号 {} → {}），重建连接",
                    old.device_id(),
                    seen.device_id
                )),
                Some(old) => self.log.log(&format!("改为控制 {}（原来是 {}）", seen.udid, old.udid)),
                None => {}
            }
            self.drop_session(g, StopReason::DeviceReplacement).await;
            g.device_profile = DeviceProfile::NotChecked;
            // App 列表是上一台的（同一台重新接入也重读，便宜）
            g.apps.clear();
            g.wda_rotation = None;
            let d = Device::from_usb(seen);
            g.info = d.info().await.ok();
            if let Some(i) = &g.info {
                g.infos.insert(seen.udid.clone(), i.clone());
            }
            let model =
                g.info.as_ref().map(|i| format!(" {} iOS {}", i.product_type, i.product_version)).unwrap_or_default();
            self.log.log(&format!("手机在线{model}"));
            g.device = Some(d);
            g.backoff.reset();
            g.next_resign = g.next_resign.min(Instant::now() + RESIGN_CHECK_AFTER_CONNECT);
            if let Some(since) = g.offline_since.take()
                && g.dedup.was_sent("offline")
            {
                g.dedup.clear("offline");
                self.notify(g, Event::DeviceOnline { minutes: since.elapsed().as_secs() / 60 });
            }
        }
        let device = g.device.clone().expect("刚设置过");

        // 就绪时不再每轮查配对记录、开发者模式：WDA 能用就说明都好着。刚换的连接照常查
        let settled = g.state == DeviceState::Ready && !fresh;
        if !settled {
            match device.paired().await {
                Ok(true) => {}
                Ok(false) => {
                    self.set_state(g, DeviceState::NeedsTrust);
                    return None;
                }
                // usbmuxd 出错：这一轮不下结论
                Err(e) => {
                    tracing::debug!("查配对记录失败：{e}");
                    return None;
                }
            }
        }
        if g.device_profile == DeviceProfile::NotChecked {
            self.refresh_device_profile(g).await;
        }
        if !settled {
            match device.developer_mode_enabled().await {
                Ok(false) => {
                    self.set_state(g, DeviceState::DeveloperModeOff);
                    return None;
                }
                Ok(true) => {}
                // 查不到时照常往下走（WDA 能起来就说明开着），但要留下原因
                Err(e) => tracing::warn!(event = "developer_mode_query_failed", error = %e),
            }
        }
        let Some(bundle) = g.persisted.wda_bundle.clone() else {
            self.set_state(g, DeviceState::NeedsSigning);
            return None;
        };
        let runner_alive = g.runner.as_ref().is_some_and(|r| r.alive());
        let link = g.wda.as_ref().filter(|_| runner_alive).map(|w| w.transport().clone());
        Some(HealthCheck { generation: g.wda_generation, udid: device.udid.clone(), link, device, bundle })
    }

    /// 请求里（已经持锁）推进：探测也在锁里做（[`Self::online`] → [`HealthCheck::probe`] → [`Self::apply_health`]）
    pub(crate) async fn online_now(&self, g: &mut Inner, seen: &UsbDevice) {
        if let Some(check) = self.online(g, seen).await {
            let health = check.probe().await;
            self.apply_health(g, check, health).await;
        }
    }

    /// 用锁外探测到的 WDA 健康推进状态机。探测期间换了客户端或设备（代数变了）就作废，下一轮再查
    pub(crate) async fn apply_health(
        &self,
        g: &mut Inner,
        check: HealthCheck,
        probe: (ictl_wda::Result<bool>, Duration),
    ) {
        let (mut health, probe_time) = probe;
        if g.wda_generation != check.generation || g.device.as_ref().map(|d| d.udid.as_str()) != Some(&check.udid) {
            tracing::debug!(event = "wda_health_discarded", reason = "session changed during the probe");
            return;
        }
        // WDA 健康：本进程的 runner 还活着，且 /status ready。探测期间 runner 退出了也不算就绪
        let runner_alive = check.link.is_some() && g.runner.as_ref().is_some_and(|r| r.alive());
        if !runner_alive {
            health = Ok(false);
        }
        if let (Some(runner), Some(_)) = (&g.runner, &g.wda)
            && runner_alive
        {
            let evidence = runner.diagnostics();
            let identity = evidence.snapshot().identity;
            let category = health_failure(&health).0;
            let mut d = crate::diagnostics::lock(&self.diagnostics);
            let recovered = d.complete(identity.runner_id, probe_time, category);
            let snapshot = d.snapshot();
            tracing::info!(event="wda_health_completed", scope="wda_status", native_pid=identity.native_pid, native_start_id=%identity.native_start_id, runner_id=identity.runner_id, phone_pid=?evidence.snapshot().phone_pid, sequence=snapshot.last_probe.as_ref().map(|p|p.sequence), duration_ms=probe_time.as_millis() as u64, category, consecutive_failures=snapshot.consecutive_failures, last_success_age_ms=?snapshot.last_success.as_ref().map(|p|p.age_ms));
            if recovered {
                evidence.event("wda_health_recovered");
            }
        }
        if matches!(health, Ok(true)) {
            g.wda_misses.reset();
            g.wda_link_down.reset();
            g.wda_unreachable_until = None;
            // 旋转角只给 WDA 短暂掉线时的 HID 用，30 秒内有效：快过期了才重读，不是每轮都问
            if g.wda_rotation.is_none_or(|(at, _)| at.elapsed() >= ROTATION_REFRESH_AFTER) {
                self.refresh_hid_rotation(g).await;
            }
            if g.state != DeviceState::Ready {
                if g.state == DeviceState::NeedsAutomationPin || g.dedup.was_sent("pin") {
                    g.dedup.clear("pin");
                    self.notify(g, Event::WdaRecovered);
                }
                self.set_state(g, DeviceState::Ready);
                g.backoff.reset();
                if let Some(w) = g.wda.as_mut()
                    && let Ok(s) = w.screen_size().await
                {
                    // 存竖放尺寸：WDA 在横屏时就绪的话这里拿到的是横的
                    g.persisted.screen = Some(s.portrait());
                    g.persisted.screen_udid = g.device.as_ref().map(|d| d.udid.clone());
                    g.persisted.save(&self.cfg.state_file());
                }
            }
            return;
        }
        let mut link_down_restart = false;
        if runner_alive {
            let (category, response_failure) = health_failure(&health);
            let unreachable = matches!(health, Err(WdaError::Unavailable(_)));
            let now = Instant::now();
            // 连得上但答得不对：按 Misses 判断；连不上：短暂的不算，持续太久（LinkDown）才算
            link_down_restart = g.wda_link_down.observe(now, unreachable);
            let restart = if response_failure {
                g.wda_misses.hit(now)
            } else {
                g.wda_misses.reset();
                link_down_restart
            };
            tracing::info!(
                event = "wda_health_failed",
                scope = "wda_status",
                native_pid = std::process::id(),
                native_start_id = ictl_wda::diagnostics::native_start_id(),
                category,
                consecutive_failures = g.wda_misses.count(),
                failure_window_ms = g.wda_misses.elapsed(now).as_millis() as u64,
                confirm_window_ms = crate::machine::Misses::CONFIRM_WINDOW.as_millis() as u64,
                link_down_ms = g.wda_link_down.elapsed(now).as_millis() as u64,
                restart,
                runner_alive = true
            );
            if !restart {
                return;
            }
        }
        if g.runner.is_some() {
            let reason = g.runner.as_ref().and_then(WdaRunner::exit_reason).unwrap_or_else(|| {
                if link_down_restart {
                    format!("WDA unreachable for over {} seconds", crate::machine::LINK_DOWN_RESTART.as_secs())
                } else if runner_alive {
                    "sustained WDA response failures (at least 3 probes over 60 seconds)".to_owned()
                } else {
                    "runner task finished".to_owned()
                }
            });
            self.log.log(&format!("WDA 不可用 event=wda_restart runner_alive={runner_alive} reason={reason}"));
            // 隧道和 HID 也一起丢：WDA 掉线常常是底层连接断了（例如 usbmuxd 重启），
            // 留着旧隧道，下次查镜像就报 socket io failed 并白白退避 60 秒
            self.drop_session(g, if runner_alive { StopReason::HealthRestart } else { StopReason::Unattributed }).await;
        }
        g.wda_misses.reset();
        if g.starting.is_some() || !g.backoff.ready(Instant::now()) {
            return;
        }
        self.spawn_start(g, check.device, check.bundle);
    }

    /// 在后台启动 WDA：挂开发者镜像（单独开一条隧道，用完关掉）→ 启动 runner 等就绪。
    /// 最长约 2 分钟（等「Enable UI Automation」密码时），期间不持锁：工具调用照常走 HID 后备路线
    fn spawn_start(&self, g: &mut Inner, device: Device, bundle: String) {
        // 正在等密码、等解锁时保持这个状态，免得每次重试都在两个状态间来回切、刷日志
        if !matches!(g.state, DeviceState::NeedsAutomationPin | DeviceState::NeedsUnlock) {
            self.set_state(g, DeviceState::WdaStarting { last_error: None });
        }
        self.log.log("启动 WDA");
        let (cache, url, user_dir) = (self.cfg.ddi_cache(), self.cfg.ddi_url.clone(), self.cfg.ddi_dir.clone());
        let diagnostics = WdaRunner::diagnostic_session();
        crate::diagnostics::lock(&self.diagnostics).set_runner(diagnostics.clone());
        g.starting = Some(tokio::task::spawn_local(async move {
            // 手机重启后开发者镜像可能掉了；已挂载时只是一次查询。错误带上「是不是挂载这一步」
            let ddi: Result<Option<String>, (DeviceError, bool)> = async {
                let mut t = device.open_tunnel().await.map_err(|e| (e, false))?;
                let r = async {
                    if t.ddi_mounted().await.map_err(|e| (e, false))?.is_some() {
                        return Ok(None);
                    }
                    // iOS 26.3 的 cryptexd 收完镜像就断开、不回安装结果：先经 mobile_image_mounter
                    // 个性化挂载（pymobiledevice3 在 iOS 17+ 的做法），不行再走 cryptexd。
                    // ICTL_DDI_DIR 是用户自己的 Cryptex 目录，只走 cryptexd
                    if user_dir.is_none() {
                        match mount_personalized(&device, &cache).await {
                            Ok(mounted) => return Ok(mounted),
                            Err(e) => tracing::warn!(event = "personalized_ddi_failed", error = %e),
                        }
                    }
                    let dir = match &user_dir {
                        Some(d) => ictl_device::ddi::require_complete(d),
                        None => ictl_device::ddi::ensure_cache(&cache, &url).await,
                    }
                    .map_err(|e| (e, false))?;
                    Ok(Some(t.mount_ddi(&dir).await.map_err(|e| (e, true))?))
                }
                .await;
                t.close().await;
                r
            }
            .await;
            let mounted = match ddi {
                Ok(v) => v,
                Err((DeviceError::PasswordProtected, _)) => return StartOutcome::NeedsUnlock,
                Err((e, mount_failed)) => return StartOutcome::Ddi { error: e.to_string(), mount_failed },
            };
            match WdaRunner::start_with_diagnostics(device.provider(), &bundle, WDA_START_TIMEOUT, diagnostics).await {
                Ok((runner, client)) => StartOutcome::Ready { runner, client, mounted },
                Err(e) => StartOutcome::Wda(e),
            }
        }));
    }

    /// 后台启动做完了就收结果
    async fn poll_start(&self, g: &mut Inner) {
        if !g.starting.as_ref().is_some_and(|h| h.is_finished()) {
            return;
        }
        let h = g.starting.take().expect("刚检查过");
        let outcome =
            h.await.unwrap_or_else(|e| StartOutcome::Wda(WdaError::StartFailed(format!("启动任务异常：{e}"))));
        match outcome {
            StartOutcome::Ready { runner, client, mounted } => {
                if let Some(v) = mounted {
                    self.log.log(&format!("开发者镜像已挂载 {v}"));
                }
                g.ddi_failures = 0;
                g.runner = Some(runner);
                g.wda_generation += 1;
                g.wda_unreachable_until = None;
                g.wda_link_down.reset();
                g.wda = Some(
                    client
                        .with_low_latency(self.cfg.low_latency)
                        .with_bounded_capture(self.cfg.capture_policy == crate::config::CapturePolicy::BoundedRemote)
                        .map_transport(crate::core::WdaLink::Device),
                );
                // 状态在下面 online() 的健康检查里变成 Ready
            }
            StartOutcome::NeedsUnlock => {
                // 不退避的话，紧接着的 online() 会立刻重新启动，状态被改回「正在启动」，用户看不到要解锁
                g.backoff.retry_after(Instant::now(), crate::machine::UNLOCK_RETRY);
                self.set_state(g, DeviceState::NeedsUnlock);
            }
            StartOutcome::Ddi { error: e, mount_failed } => {
                // 只有挂载失败才可能是缓存坏了；ICTL_DDI_DIR 是用户自己的文件，从不删
                if mount_failed && self.cfg.ddi_dir.is_none() {
                    g.ddi_failures += 1;
                    if g.ddi_failures >= DDI_FAILURES_INVALIDATE {
                        self.log.log(&format!("开发者镜像连续 {} 次挂载失败，清掉缓存，下次重新下载", g.ddi_failures));
                        ictl_device::ddi::invalidate(&self.cfg.ddi_cache());
                        g.ddi_failures = 0;
                    }
                }
                g.backoff.failed(Instant::now());
                self.set_state(g, DeviceState::WdaStarting { last_error: Some(format!("开发者镜像：{e}")) });
            }
            StartOutcome::Wda(WdaError::NeedsAutomationPin) => {
                g.backoff.waiting_for_pin(Instant::now());
                self.set_state(g, DeviceState::NeedsAutomationPin);
                self.notify(g, Event::NeedsAutomationPin);
            }
            StartOutcome::Wda(e) => {
                g.backoff.failed(Instant::now());
                self.log.log(&format!("WDA 启动失败：{e}"));
                let mut last_error = e.to_string();
                // The profile check cannot see a revoked certificate: iOS then refuses to spawn the
                // runner on every retry. Re-sign and reinstall once instead of retrying forever.
                if launch_rejected(&last_error) && g.persisted.wda_bundle.is_some() {
                    let now = unix_now();
                    if launch_resign_allowed(g.persisted.launch_resign_at, now) {
                        g.persisted.launch_resign_at = Some(now);
                        g.persisted.save(&self.cfg.state_file());
                        g.resign_force_pending = true;
                        g.next_resign = Instant::now();
                        self.log.log("iOS 拒绝启动已安装的 WDA（签名可能已失效），自动重签并重新安装");
                        last_error = ictl_l10n::tr!(
                            "iOS 拒绝启动已安装的 WDA（签名可能已失效），正在自动重签并重新安装",
                            "iOS refused to launch the installed WDA (its signature may be invalid); re-signing and reinstalling automatically"
                        )
                        .into();
                    } else {
                        last_error = ictl_l10n::trf!(
                            "iOS 仍拒绝启动 WDA：请在手机 设置 → 通用 → VPN 与设备管理 中信任开发者 App，并保持手机解锁（{e}）",
                            "iOS still refuses to launch WDA: trust the developer app in Settings → General → VPN & Device Management and keep the phone unlocked ({e})"
                        );
                    }
                }
                self.set_state(g, DeviceState::WdaStarting { last_error: Some(last_error) });
            }
        }
    }

    // ------------------------------------------------------------------ 重签与会话

    async fn check_session(self: Arc<Self>) {
        let account = self.account();
        // plume_core 的 future 不是 Send，放到阻塞线程里 block_on（同重签）；也不持锁
        let r: Result<(), Option<String>> = tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(async move {
                match tokio::time::timeout(SESSION_CHECK_TIMEOUT, account.session()).await {
                    Ok(Ok(_)) => Ok(()),
                    Ok(Err(ictl_sign::SignError::NotLoggedIn)) => Err(None),
                    Ok(Err(e)) => Err(Some(e.to_string())),
                    Err(_) => Err(Some(format!("{} 分钟内没有完成", SESSION_CHECK_TIMEOUT.as_secs() / 60))),
                }
            })
        })
        .await
        .unwrap_or_else(|e| Err(Some(format!("会话检查任务异常：{e}"))));
        let mut g = self.inner.lock().await;
        match r {
            Ok(()) => g.dedup.clear("session"),
            Err(None) => {}
            Err(Some(e)) => {
                self.log.log(&format!("Apple ID 会话检查失败：{e}"));
                self.notify(&mut g, Event::SessionExpired(e));
            }
        }
    }

    /// 检查描述文件并按需重签（`force` 立即重签）。分三段：持锁取参数 → 不持锁签名、安装（几十秒）→ 持锁写结果
    pub(crate) async fn resign_now(
        self: &Arc<Self>,
        force: bool,
        admission: Option<(Instant, Option<ictl_protocol::ActionDeadline>)>,
    ) -> Result<String, String> {
        let (device, name) = {
            let mut g = self.inner.lock().await;
            if let Some((queued_at, deadline)) = admission {
                crate::core::check_action_admission(queued_at, deadline, "daemon_resign_lock")?;
            }
            let g = &mut *g;
            // 看这台手机上装的那份，不看最近一次签名：给别的手机签过之后，这台装的还是更早签的、更早到期
            if !force && g.persisted.wda_bundle.is_some() {
                self.refresh_device_profile(g).await;
                if g.device_profile == DeviceProfile::Missing {
                    // 自动重签不替没装过的手机注册设备（占 Apple ID 的设备名额），交给用户决定
                    return Ok(
                        ictl_l10n::tr!("这台手机上没有装当前 Apple ID 签的 WDA,不自动重签;要装就运行 iphone-control setup", "No WDA signed by the current Apple ID on this phone, not re-signing automatically; run iphone-control setup to install it").into()
                    );
                }
                if let Some(left) = g.device_profile.left(&g.persisted)
                    && left > self.cfg.resign_min
                {
                    return Ok(ictl_l10n::trf!(
                        "描述文件还剩 {} 小时,无需重签",
                        "{} hours left on the provisioning profile, no need to re-sign",
                        left.as_secs() / 3600
                    ));
                }
            }
            if g.resigning {
                return Ok(ictl_l10n::tr!(
                    "正在重签,稍后用 iphone-control status 查看结果",
                    "Re-signing, check the result later with iphone-control status"
                )
                .into());
            }
            let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
            let name = g
                .info
                .as_ref()
                .map(|i| i.name.clone())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| "iphone-control".into());
            g.resigning = true;
            self.log.log("开始重签");
            (device, name)
        };
        let signed_udid = device.udid.clone();
        let account = self.account();
        let (wda_dir, unsigned, signed_path) =
            (self.cfg.wda_dir(), self.cfg.wda_unsigned.clone(), self.cfg.wda_signed());
        let fetch = self.cfg.wda_unsigned_is_cache() && !unsigned.is_file();
        if fetch {
            self.log.log(&format!("没有未签名的 WDA 包（不在镜像里运行），先下载 {}", crate::wda_pkg::WDA_VERSION));
        }
        // plume_core 的签名 future 不是 Send（持有 &mut dyn FnMut 和非 Send 的私钥），放到阻塞线程里 block_on
        let r: Result<ictl_sign::Signed, String> = tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current()
                .block_on(tokio::time::timeout(RESIGN_TIMEOUT, async move {
                    if fetch {
                        crate::wda_pkg::ensure(&unsigned).await?;
                    }
                    let session = account.session().await.map_err(|e| e.to_string())?;
                    session.ensure_device(&device.udid, &name).await.map_err(|e| e.to_string())?;
                    std::fs::create_dir_all(&wda_dir).map_err(|e| e.to_string())?;
                    let tmp = wda_dir.join("WDA-signing.ipa");
                    let signed = session.sign_ipa(&unsigned, &tmp, &device.udid).await.map_err(|e| e.to_string())?;
                    device.install(&tmp).await.map_err(|e| format!("安装到手机失败(手机锁屏?)：{e}"))?;
                    std::fs::rename(&tmp, &signed_path).map_err(|e| e.to_string())?;
                    Ok(signed)
                }))
                .unwrap_or_else(|_| {
                    Err(format!("{} 分钟内没有完成(苹果服务器或安装卡住)", RESIGN_TIMEOUT.as_secs() / 60))
                })
        })
        .await
        .unwrap_or_else(|e| Err(format!("签名任务异常退出：{e}")));

        let mut g = self.inner.lock().await;
        let g = &mut *g;
        g.resigning = false;
        let r = match r {
            Ok(s) => {
                let exp = s.profile.expires.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
                g.persisted.team_id = Some(s.team_id);
                g.persisted.wda_bundle = Some(s.bundle_id);
                g.persisted.profile_expires = Some(exp);
                g.persisted.last_resign = Some(format!("{} 成功", crate::eventlog::timestamp()));
                g.persisted.save(&self.cfg.state_file());
                // 新包装上后旧的 runner 会失效，下一轮巡检重新拉起。签名期间切到了别的设备就不动它的会话
                if g.device.as_ref().is_some_and(|d| d.udid == signed_udid) {
                    self.drop_session(g, StopReason::WdaReinstalled).await;
                    self.refresh_device_profile(g).await;
                    g.backoff.reset();
                }
                g.dedup.clear("resign");
                let msg = format!("重签完成,新描述文件到期 {} UTC", crate::eventlog::format_utc(exp));
                self.log.log(&msg);
                Ok(msg)
            }
            Err(e) => {
                g.persisted.last_resign = Some(format!("{} 失败：{e}", crate::eventlog::timestamp()));
                g.persisted.save(&self.cfg.state_file());
                self.log.log(&format!("重签失败：{e}"));
                Err(e)
            }
        };
        self.publish(g);
        r
    }
}

/// After an automatic re-sign, a further refusal within this window is reported (trust the
/// developer, unlock the phone) instead of re-signing again.
const LAUNCH_RESIGN_COOLDOWN_SECS: u64 = 6 * 3600;

/// iOS refused to spawn the installed runner itself (bad or untrusted code signature),
/// as opposed to transport, locked-phone or automation-passcode failures.
fn launch_rejected(error: &str) -> bool {
    error.contains("Launchd job spawn failed") || error.contains("denied by service delegate")
}

fn launch_resign_allowed(last: Option<u64>, now: u64) -> bool {
    last.is_none_or(|at| now.saturating_sub(at) >= LAUNCH_RESIGN_COOLDOWN_SECS)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// Connection/dispatch failures say nothing about whether XCTest is responsive.
fn health_failure(result: &ictl_wda::Result<bool>) -> (&'static str, bool) {
    use ictl_wda::WdaError;
    match result {
        Err(WdaError::Unavailable(_)) => ("transport_unavailable", false),
        Err(WdaError::Timeout(_)) => ("response_timeout", true),
        Err(WdaError::Interrupted(_)) => ("response_interrupted", true),
        Err(_) => ("response_error", true),
        Ok(false) => ("not_ready", true),
        Ok(true) => ("ready", false),
    }
}

/// 经 mobile_image_mounter 个性化挂载开发者镜像。已挂载返回 `Ok(None)`，挂上了返回 `Ok(Some(说明))`
async fn mount_personalized(device: &Device, cache: &Path) -> Result<Option<String>, DeviceError> {
    if device.personalized_ddi_mounted().await? {
        return Ok(None);
    }
    let dir = cache.join("personalized");
    let dir = ictl_device::ddi::ensure_personalized_cache(&dir, ictl_device::ddi::PERSONALIZED_BASE_URL).await?;
    device.mount_personalized_ddi(&dir).await?;
    Ok(Some("Personalized".into()))
}

#[cfg(test)]
mod state_tests {
    use super::*;
    use crate::testkit;
    use ictl_wda::fake::Reply;

    fn usb() -> UsbDevice {
        UsbDevice { udid: "00008130-TEST".into(), device_id: 7 }
    }

    /// 模拟 online() 交出来的探测（不经 usbmuxd）：当前代数、当前设备、当前 WDA 的传输
    fn check(g: &mut Inner) -> HealthCheck {
        let device = g.device.clone().unwrap();
        let alive = g.runner.as_ref().is_some_and(|r| r.alive());
        HealthCheck {
            generation: g.wda_generation,
            udid: device.udid.clone(),
            link: g.wda.as_ref().filter(|_| alive).map(|w| w.transport().clone()),
            device,
            bundle: "com.example.WebDriverAgentRunner.xctrunner".into(),
        }
    }

    fn prepare(g: &mut Inner, fake: &ictl_wda::fake::ScriptedWda) {
        g.device = Some(Device::from_usb(&usb()));
        testkit::attach(g, fake);
        // 不在测试里真去拉起 WDA
        g.backoff.failed(Instant::now());
    }

    #[tokio::test]
    async fn healthy_probe_makes_the_device_ready_and_records_the_screen() {
        let (d, dir) = testkit::daemon("ready");
        let fake = testkit::healthy_wda();
        let mut g = d.inner.lock().await;
        prepare(&mut g, &fake);
        g.state = DeviceState::WdaStarting { last_error: None };
        g.wda_unreachable_until = Some(Instant::now() + Duration::from_secs(60));
        let c = check(&mut g);
        let health = c.probe().await;
        assert!(matches!(health.0, Ok(true)));
        d.apply_health(&mut g, c, health).await;
        assert_eq!(g.state, DeviceState::Ready);
        assert!(g.wda_unreachable_until.is_none(), "a ready probe closes the request breaker");
        assert!(g.wda_rotation.is_some());
        assert_eq!(g.persisted.screen.map(|s| s.width), Some(430.0));
        // 旋转角还新：下一轮不再问
        let rotations = fake.count("GET", "/rotation");
        let c = check(&mut g);
        let health = c.probe().await;
        d.apply_health(&mut g, c, health).await;
        assert_eq!(fake.count("GET", "/rotation"), rotations);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 探测期间 WDA 客户端换了（代数变了）：结果作废，不改状态
    #[tokio::test]
    async fn probe_result_is_discarded_when_the_session_changed_meanwhile() {
        let (d, dir) = testkit::daemon("stale-probe");
        let fake = testkit::healthy_wda();
        fake.on("GET", "/status", Reply::Json(500, serde_json::json!({"value": {"error": "x", "message": "x"}})));
        let mut g = d.inner.lock().await;
        prepare(&mut g, &fake);
        g.state = DeviceState::Ready;
        let c = check(&mut g);
        let health = c.probe().await;
        testkit::attach(&mut g, &fake);
        d.apply_health(&mut g, c, health).await;
        assert_eq!(g.wda_misses.count(), 0);
        assert!(g.runner.is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// runner 活着、WDA 一直连不上：短暂的不算（Misses 清零、状态不变），持续超过 LINK_DOWN_RESTART 才重启
    #[tokio::test]
    async fn sustained_unreachable_wda_with_a_live_runner_is_restarted() {
        let (d, dir) = testkit::daemon("link-down");
        let fake = testkit::healthy_wda();
        fake.on("GET", "/status", Reply::unavailable());
        let mut g = d.inner.lock().await;
        prepare(&mut g, &fake);
        g.state = DeviceState::Ready;
        let c = check(&mut g);
        let health = c.probe().await;
        assert!(matches!(health.0, Err(WdaError::Unavailable(_))));
        d.apply_health(&mut g, c, health).await;
        assert!(g.runner.is_some(), "a brief link failure keeps the runner");
        assert_eq!(g.wda_misses.count(), 0);
        // 假装已经连不上很久了
        g.wda_link_down.reset();
        g.wda_link_down.observe(Instant::now() - crate::machine::LINK_DOWN_RESTART, true);
        let c = check(&mut g);
        let health = c.probe().await;
        d.apply_health(&mut g, c, health).await;
        assert!(g.runner.is_none() && g.wda.is_none(), "sustained link failure restarts WDA");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 连得上但答得不对：照旧要 3 次、跨 60 秒才重启
    #[tokio::test]
    async fn response_failures_need_the_confirmation_window() {
        let (d, dir) = testkit::daemon("misses");
        let fake = testkit::healthy_wda();
        fake.on("GET", "/status", Reply::timeout());
        let mut g = d.inner.lock().await;
        prepare(&mut g, &fake);
        g.state = DeviceState::Ready;
        for _ in 0..3 {
            let c = check(&mut g);
            let health = c.probe().await;
            d.apply_health(&mut g, c, health).await;
        }
        assert!(g.runner.is_some(), "three quick misses can't kill a live runner");
        assert_eq!(g.wda_misses.count(), 3);
        std::fs::remove_dir_all(dir).unwrap();
    }

    async fn finished(outcome: StartOutcome) -> tokio::task::JoinHandle<StartOutcome> {
        let h = tokio::task::spawn_local(async move { outcome });
        while !h.is_finished() {
            tokio::task::yield_now().await;
        }
        h
    }

    /// 后台启动 WDA 的结果由巡检收：锁屏 → NeedsUnlock（固定间隔重试），等自动化密码 → NeedsAutomationPin，
    /// 挂镜像连续失败 3 次清缓存计数
    #[tokio::test]
    async fn poll_start_moves_the_state_machine() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (d, dir) = testkit::daemon("poll-start");
                let mut g = d.inner.lock().await;
                g.state = DeviceState::WdaStarting { last_error: None };
                g.starting = Some(finished(StartOutcome::NeedsUnlock).await);
                d.poll_start(&mut g).await;
                assert_eq!(g.state, DeviceState::NeedsUnlock);
                assert!(g.starting.is_none());
                assert!(!g.backoff.ready(Instant::now()));
                assert!(g.backoff.ready(Instant::now() + crate::machine::UNLOCK_RETRY));

                g.starting = Some(finished(StartOutcome::Wda(WdaError::NeedsAutomationPin)).await);
                d.poll_start(&mut g).await;
                assert_eq!(g.state, DeviceState::NeedsAutomationPin);
                assert!(g.dedup.was_sent("pin"));

                for n in 1..=3 {
                    g.starting =
                        Some(finished(StartOutcome::Ddi { error: "mount failed".into(), mount_failed: true }).await);
                    d.poll_start(&mut g).await;
                    assert_eq!(g.ddi_failures, n % 3);
                }
                assert!(
                    matches!(&g.state, DeviceState::WdaStarting { last_error: Some(e) } if e.contains("mount failed"))
                );
                // 还没做完的不收
                g.starting = Some(tokio::task::spawn_local(std::future::pending()));
                d.poll_start(&mut g).await;
                assert!(g.starting.is_some());
                g.starting.take().unwrap().abort();
                drop(g);
                std::fs::remove_dir_all(dir).unwrap();
            })
            .await;
    }

    /// 正在重签时有强制重签在等：不取走标记（否则这次只会返回「正在重签」，标记就丢了）
    #[tokio::test]
    async fn pending_forced_resign_survives_a_resign_in_progress() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (d, dir) = testkit::daemon("resign-force");
                let mut g = d.inner.lock().await;
                g.device = Some(Device::from_usb(&usb()));
                g.persisted.wda_bundle = Some("b".into());
                g.resigning = true;
                g.resign_force_pending = true;
                g.next_resign = Instant::now();
                d.schedule_background(&mut g);
                assert!(g.resign_force_pending);
                assert!(g.next_resign <= Instant::now(), "retried on the next tick");
                drop(g);
                std::fs::remove_dir_all(dir).unwrap();
            })
            .await;
    }
}

#[cfg(test)]
mod health_tests {
    use super::*;
    #[test]
    fn only_ios_spawn_refusals_trigger_a_rate_limited_resign() {
        let refused = "process launch failed: com.apple.dt.deviceprocesscontrolservice (code 2): The request was denied by service delegate (SBMainWorkspace).; caused by NSPOSIXErrorDomain (code 85): Launchd job spawn failed";
        assert!(launch_rejected(refused));
        for other in ["手机上没有安装 WDA（x），先运行 iphone-control sign", "connect timeout", "Enable UI Automation"]
        {
            assert!(!launch_rejected(other), "{other}");
        }
        assert!(launch_resign_allowed(None, 1_000));
        assert!(!launch_resign_allowed(Some(1_000), 1_000 + LAUNCH_RESIGN_COOLDOWN_SECS - 1));
        assert!(launch_resign_allowed(Some(1_000), 1_000 + LAUNCH_RESIGN_COOLDOWN_SECS));
    }
    #[test]
    fn a_link_failure_cannot_trigger_a_live_runner_restart() {
        assert_eq!(
            health_failure(&Err(ictl_wda::WdaError::Unavailable("connect timeout".into()))),
            ("transport_unavailable", false)
        );
        assert_eq!(health_failure(&Err(ictl_wda::WdaError::Timeout("read".into()))), ("response_timeout", true));
        assert_eq!(health_failure(&Ok(false)), ("not_ready", true));
    }
}
