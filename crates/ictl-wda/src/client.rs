//! WDA HTTP 客户端。会话失效（WDA 重启后）自动重建一次。

use std::time::Duration;

use base64::Engine;
use ictl_device::ScreenSize;
use serde_json::{Value, json};

use crate::http::Transport;
use crate::timing::StepTimer;
use crate::{Result, WdaError, elements};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_CALL: AtomicU64 = AtomicU64::new(1);

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// MJPEG 抓帧的缩放比例。整屏最终缩到「点」尺寸（约原始像素的 1/3）给模型看，40% 仍比点尺寸略大，
/// 不损失画质；每帧经浏览器上行的字节比 50% 少约 36%
const MJPEG_SCALING_PERCENT: u32 = 40;
const MJPEG_FRAMERATE: u32 = 5;
/// 同一操作内（滑动后轮询元素树、输入后核对等）复用刚问到的方向
const ORIENTATION_REUSE: Duration = Duration::from_millis(1500);
/// 截图横竖和缓存一致时，方向最多复用这么久。横竖一变（转屏、进出横屏 App）截图形状就变了，会立刻重新问；
/// 这个时限只兜底同为横屏的左右翻转。真机任务（2026-11-13）：一步常超过 15 秒，58 步问了 31 次，
/// 慢链路上每次平均 3.2 秒、最长 9 秒（WDA 要等界面静止才回答）
const FRAME_ORIENTATION_REUSE: Duration = Duration::from_secs(120);
/// `/source` 的默认时限：完整树要 7～8 秒，排除慢属性后约 0.3 秒；页面加载中可能更久
const SOURCE_TIMEOUT: Duration = Duration::from_secs(60);
/// 有界截图策略下要原始分辨率截图（区域放大）时的说明
pub const FULL_RESOLUTION_UNSUPPORTED: &str =
    "region zoom needs a full-resolution screenshot, which the bounded remote capture policy does not allow";

fn bounded_unsupported() -> WdaError {
    tracing::warn!(event = "capture_rejected", category = "mjpeg_unsupported");
    WdaError::Unavailable("bounded MJPEG capture is unsupported".into())
}

/// 普通点击按下的时长（毫秒）。太短可能不被识别成点击；解锁按键另有自己的时长
const TAP_PRESS_MS: u64 = 50;

/// W3C pointer 动作的一步
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PointerStep {
    Move { x: f64, y: f64, ms: u64 },
    Down,
    Up,
    Pause { ms: u64 },
}

/// 单指点一下：按下停 `press_ms` 再抬起
pub fn tap_steps(x: f64, y: f64, press_ms: u64) -> Vec<PointerStep> {
    vec![PointerStep::Move { x, y, ms: 0 }, PointerStep::Down, PointerStep::Pause { ms: press_ms }, PointerStep::Up]
}

/// 滑动：按下、停 50ms、在 `ms` 毫秒内移到终点、抬起（与 toolkit 一致）
pub fn swipe_steps(from: (f64, f64), to: (f64, f64), ms: u64) -> Vec<PointerStep> {
    vec![
        PointerStep::Move { x: from.0, y: from.1, ms: 0 },
        PointerStep::Down,
        PointerStep::Pause { ms: 50 },
        PointerStep::Move { x: to.0, y: to.1, ms },
        PointerStep::Up,
    ]
}

/// `POST /session/{id}/actions` 的请求体
pub fn actions_body(steps: &[PointerStep]) -> Value {
    let actions: Vec<Value> = steps
        .iter()
        .map(|s| match *s {
            PointerStep::Move { x, y, ms } => json!({"type": "pointerMove", "duration": ms, "x": x, "y": y}),
            PointerStep::Down => json!({"type": "pointerDown", "button": 0}),
            PointerStep::Up => json!({"type": "pointerUp", "button": 0}),
            PointerStep::Pause { ms } => json!({"type": "pause", "duration": ms}),
        })
        .collect();
    json!({"actions": [{"type": "pointer", "id": "finger1", "parameters": {"pointerType": "touch"}, "actions": actions}]})
}

/// appium settings 中与等待有关的两项。锁屏解锁、点击期间要临时调小。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IdleSettings {
    pub wait_for_idle_timeout: f64,
    pub animation_cool_off_timeout: f64,
}

impl IdleSettings {
    /// WDA 的默认值
    pub const DEFAULT: Self = Self { wait_for_idle_timeout: 10.0, animation_cool_off_timeout: 2.0 };
    /// 临时调小的值（0 会导致锁屏唤醒失败，所以不是 0）
    pub const QUICK: Self = Self { wait_for_idle_timeout: 1.0, animation_cool_off_timeout: 0.2 };
    /// 不等动画冷却：滑动、以及「等界面稳定」时轮询元素树用（由调用方比较前后两次判断稳定）。真机（iOS 27，设置列表）：
    /// - 上下滑 0.3 秒：冷却 0 约 1.0 秒；默认 2 秒约 3 秒；0.2～1.5 秒反而要 18～19 秒（惯性滚动期间一直等）
    /// - 滑完马上读元素树：冷却 0 约 0.2 秒；默认约 2.2 秒；0.2 秒要约 18 秒
    pub const NO_COOL_OFF: Self = Self { wait_for_idle_timeout: 1.0, animation_cool_off_timeout: 0.0 };
}

/// 主屏、锁屏、系统弹窗所属的进程
pub const SPRINGBOARD: &str = "com.apple.springboard";
/// IOHID 消费者页（Consumer）
const HID_PAGE_CONSUMER: u32 = 0x0C;
/// 消费者页里的电源键
const HID_POWER: u32 = 0x30;
/// 消费者页里的 AC Home
const HID_AC_HOME: u32 = 0x223;
/// 启动 App 后至少过这么久才返回：App 切到前台的放大动画约 0.5 秒，期间的点击会被吞
const LAUNCH_MIN: Duration = Duration::from_millis(800);

/// [`WdaClient::launch_app`] 的结果
#[derive(Debug, Clone, PartialEq)]
pub enum Launch {
    /// 到了前台
    Foreground,
    /// 系统弹窗挡着（弹窗文字）
    Blocked(String),
    /// 8 秒内没到前台，也没有弹窗
    Slow,
}

/// [`WdaClient::check_lock`] 的结果
#[derive(Debug)]
pub struct LockCheck {
    pub locked: bool,
    /// 拿不准：WDA 说没锁，屏幕上却是锁屏样式的界面，认不出是锁屏还是通知中心（见 [`WdaClient::check_lock`]）。
    /// `locked` 这时为 false；能自动解锁的调用方应当按可能锁着处理
    pub uncertain: bool,
    /// 核对时取到的 SpringBoard 元素树（没核对或前台不是 SpringBoard 时为 None）
    pub tree: Option<String>,
}

pub struct WdaClient<T> {
    transport: T,
    session: Option<String>,
    /// `/wda/screen` 第一次给的尺寸（当时的方向）。当前方向下的宽高见 [`Self::view`]
    screen: Option<ScreenSize>,
    /// 本客户端（对应一次 WDA 启动）是否确认过一次「确实没锁屏」，确认后 `/wda/locked` 就可信了
    lock_state_trusted: bool,
    /// 最近一次问到的方向和时间。每次 `/orientation` 是一个经浏览器转发的往返（约 0.2–1.5 秒，
    /// 动作刚做完时 WDA 要等界面静止才回答），同一操作内的重复查询和横竖没变的截图都复用它
    orientation_cache: Option<(elements::Orientation, tokio::time::Instant)>,
    /// 点击前后要恢复的等待设置（第一次点击时读一次）
    idle_baseline: Option<IdleSettings>,
    idle_current: Option<IdleSettings>,
    low_latency: bool,
    bounded_capture: bool,
    profile_applied: bool,
    /// 判断画面动没动时 MJPEG 读不到（传输不支持或读失败过），之后都用普通截图
    mjpeg_motion_broken: bool,
}

impl<T: Transport> WdaClient<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            session: None,
            screen: None,
            lock_state_trusted: false,
            orientation_cache: None,
            idle_baseline: None,
            idle_current: None,
            low_latency: false,
            bounded_capture: false,
            profile_applied: false,
            mjpeg_motion_broken: false,
        }
    }
    /// 换一层传输（例如包进调用方自己的枚举），会话、缓存的尺寸和设置原样保留
    pub fn map_transport<U>(self, f: impl FnOnce(T) -> U) -> WdaClient<U> {
        WdaClient {
            transport: f(self.transport),
            session: self.session,
            screen: self.screen,
            lock_state_trusted: self.lock_state_trusted,
            orientation_cache: self.orientation_cache,
            idle_baseline: self.idle_baseline,
            idle_current: self.idle_current,
            low_latency: self.low_latency,
            bounded_capture: self.bounded_capture,
            profile_applied: self.profile_applied,
            mjpeg_motion_broken: self.mjpeg_motion_broken,
        }
    }

    /// 底层传输。只读探测（`/status`）可以拿它的副本另开一个客户端，在设备锁外面做
    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// Dedicated browser-control session. Ordinary MCP clients retain their defaults.
    pub fn with_low_latency(mut self, enabled: bool) -> Self {
        self.low_latency = enabled;
        self
    }

    /// Require bounded MJPEG for every capture, including hidden unlock/settle reads.
    /// Independent of gesture timing and other low-latency settings.
    pub fn with_bounded_capture(mut self, enabled: bool) -> Self {
        self.bounded_capture = enabled;
        self
    }

    #[tracing::instrument(name = "wda_call", skip_all, fields(call_id = NEXT_CALL.fetch_add(1, Ordering::Relaxed), method = method, route = path.split('?').next().unwrap_or(path)))]
    async fn call(&self, method: &str, path: &str, body: Option<Value>, timeout: Duration) -> Result<Value> {
        let encoding = StepTimer::new("request_json_encode");
        let body = body.map(|b| b.to_string().into_bytes());
        encoding.finish(true);
        let response = if method == "GET" && path == "/status" {
            self.transport.health().await
        } else {
            self.transport.send(method, path, body, timeout).await
        };
        let (status, raw) = response.map_err(|e| match e.kind() {
            std::io::ErrorKind::TimedOut => WdaError::Timeout(e.to_string()),
            crate::http::AFTER_SENT => WdaError::Interrupted(e.to_string()),
            _ => WdaError::Unavailable(e.to_string()),
        })?;
        let parsing = StepTimer::new("response_json_decode");
        let parsed = serde_json::from_slice(&raw);
        parsing.finish(parsed.is_ok());
        let v: Value = match parsed {
            Ok(v) => v,
            Err(_) if status >= 400 => {
                return Err(WdaError::Remote { kind: "http".into(), message: status.to_string() });
            }
            Err(e) => {
                return Err(WdaError::Interrupted(ictl_l10n::trf!("响应不是 JSON：{e}", "response is not JSON: {e}")));
            }
        };
        if let Some(err) = v.get("value").and_then(|x| x.get("error")).and_then(|e| e.as_str()) {
            let msg = v["value"].get("message").and_then(|m| m.as_str()).unwrap_or_default();
            return Err(WdaError::Remote {
                kind: err.to_owned(),
                message: msg.lines().next().unwrap_or_default().to_owned(),
            });
        }
        Ok(v)
    }

    async fn get(&self, path: &str) -> Result<Value> {
        Ok(self.call("GET", path, None, DEFAULT_TIMEOUT).await?["value"].take())
    }

    async fn session_id(&mut self) -> Result<String> {
        if let Some(s) = &self.session {
            return Ok(s.clone());
        }
        let r =
            self.call("POST", "/session", Some(json!({"capabilities": {"alwaysMatch": {}}})), DEFAULT_TIMEOUT).await?;
        let id = r
            .get("sessionId")
            .and_then(|s| s.as_str())
            .or_else(|| r["value"].get("sessionId").and_then(|s| s.as_str()))
            .ok_or_else(|| {
                WdaError::Unavailable(
                    ictl_l10n::tr!("创建会话没有返回 sessionId", "creating a session returned no sessionId").into(),
                )
            })?
            .to_owned();
        self.session = Some(id.clone());
        if self.low_latency {
            // A one-frame capture waits for the server's next tick: 5 fps bounds that wait at 200 ms
            // (3 fps: 333 ms) while keeping the opt-in continuous preview light on the shared USB link.
            let settings = json!({"settings":{"screenshotQuality":1,"waitForIdleTimeout":1.0,"animationCoolOffTimeout":0.0,
                    "mjpegServerFramerate":MJPEG_FRAMERATE,"mjpegScalingFactor":MJPEG_SCALING_PERCENT,"mjpegServerScreenshotQuality":40}});
            match self.call("POST", &format!("/session/{id}/appium/settings"), Some(settings), DEFAULT_TIMEOUT).await {
                Ok(value)
                    if value["value"]["screenshotQuality"].as_f64() == Some(1.0)
                        && value["value"]["waitForIdleTimeout"].as_f64() == Some(1.0)
                        && value["value"]["animationCoolOffTimeout"].as_f64() == Some(0.0) =>
                {
                    self.profile_applied = true;
                    self.idle_baseline = Some(IdleSettings::NO_COOL_OFF);
                    self.idle_current = Some(IdleSettings::NO_COOL_OFF);
                }
                _ => {
                    // Unknown/older WDA settings fail soft. Do not assume an unacknowledged profile.
                    self.profile_applied = false;
                    self.idle_current = None;
                    tracing::warn!("WDA low-latency settings were not acknowledged; using normal gesture settings");
                }
            }
        }
        Ok(id)
    }

    /// 带会话的请求；会话失效时重建一次再试
    pub async fn in_session(
        &mut self,
        method: &str,
        path: &str,
        body: Option<Value>,
        timeout: Duration,
    ) -> Result<Value> {
        for attempt in 0..2 {
            let sid = self.session_id().await?;
            match self.call(method, &format!("/session/{sid}{path}"), body.clone(), timeout).await {
                Err(WdaError::Remote { kind, .. }) if attempt == 0 && kind.contains("session") => {
                    self.session = None;
                    self.idle_baseline = None;
                    self.idle_current = None;
                    self.profile_applied = false;
                    self.lock_state_trusted = false;
                }
                other => return other.map(|mut v| v["value"].take()),
            }
        }
        unreachable!()
    }

    async fn post(&mut self, path: &str, body: Value) -> Result<Value> {
        self.in_session("POST", path, Some(body), DEFAULT_TIMEOUT).await
    }

    pub async fn ready(&self) -> Result<bool> {
        Ok(self.call("GET", "/status", None, Duration::from_secs(15)).await?["value"]["ready"] == true)
    }

    /// 当前方向下的屏幕点尺寸（转屏后宽高跟着变）
    pub async fn screen_size(&mut self) -> Result<ScreenSize> {
        Ok(self.view().await?.screen)
    }

    /// 当前方向下的屏幕尺寸和方向。尺寸只问一次 `/wda/screen`（约 0.3 秒），方向每次问（约 20 毫秒）：
    /// 尺寸缓存的是问的那一刻的方向，转屏后不换算的话截图会被压扁、屏幕下半截的元素被当成屏幕外。
    /// 问不到方向（旧版 WDA 等）就按缓存的尺寸、方向未知（不换算元素坐标）
    pub async fn view(&mut self) -> Result<elements::View> {
        self.view_reusing(|_| true, ORIENTATION_REUSE).await
    }

    /// 截图用的视图：截图本身的横竖和缓存的方向一致、且不太旧时不再问 `/orientation`。
    /// 横竖一变（转屏、进出横屏 App）就重新问；同为横屏的左右翻转靠 `max_age` 兜底
    pub async fn view_for_frame(&mut self, landscape_frame: bool) -> Result<elements::View> {
        self.view_reusing(move |o| o.is_landscape() == landscape_frame, FRAME_ORIENTATION_REUSE).await
    }

    /// 下一次取视图时一定重新问方向（转屏、按键等可能改变方向的操作之后调用）
    pub fn forget_orientation(&mut self) {
        self.orientation_cache = None;
    }

    async fn view_reusing(
        &mut self,
        reusable: impl Fn(elements::Orientation) -> bool,
        max_age: Duration,
    ) -> Result<elements::View> {
        let base = match self.screen {
            Some(s) => s,
            None => {
                let v = self.get("/wda/screen").await?;
                let s = &v["screenSize"];
                let size = match (s["width"].as_f64(), s["height"].as_f64()) {
                    (Some(width), Some(height)) => ScreenSize { width, height },
                    _ => {
                        return Err(WdaError::Unavailable(
                            ictl_l10n::tr!("/wda/screen 没有 screenSize", "/wda/screen has no screenSize").into(),
                        ));
                    }
                };
                self.screen = Some(size);
                size
            }
        };
        if let Some((o, at)) = self.orientation_cache
            && at.elapsed() < max_age
            && reusable(o)
        {
            return Ok(elements::View { screen: o.orient(base), orientation: Some(o) });
        }
        Ok(match self.orientation().await {
            Ok(o) => {
                self.orientation_cache = Some((o, tokio::time::Instant::now()));
                elements::View { screen: o.orient(base), orientation: Some(o) }
            }
            Err(e) => {
                self.orientation_cache = None;
                tracing::debug!("读屏幕方向失败，按缓存的屏幕尺寸算：{e}");
                elements::View { screen: base, orientation: None }
            }
        })
    }

    /// 界面相对设备竖放转了多少（`GET /rotation` 的 z）。能分出两个横屏方向，`/orientation` 分不出
    pub async fn rotation(&mut self) -> Result<ictl_device::Rotation> {
        let v = self.in_session("GET", "/rotation", None, Duration::from_secs(10)).await?;
        v["z"].as_f64().and_then(ictl_device::Rotation::from_degrees).ok_or_else(|| {
            WdaError::Unavailable(ictl_l10n::trf!(
                "看不懂 WDA 给的旋转角：{v}",
                "Can't make sense of the rotation WDA reported: {v}"
            ))
        })
    }

    /// 前台界面的方向
    pub async fn orientation(&mut self) -> Result<elements::Orientation> {
        let v = self.in_session("GET", "/orientation", None, Duration::from_secs(10)).await?;
        v.as_str().and_then(elements::Orientation::from_wda).ok_or_else(|| {
            WdaError::Unavailable(ictl_l10n::trf!(
                "看不懂 WDA 给的方向：{v}",
                "Can't make sense of the orientation WDA reported: {v}"
            ))
        })
    }

    /// 是否锁屏。WDA 的 `/wda/locked` 只靠 `com.apple.springboard.lockstate` 的变化通知维护
    /// （WebDriverAgent `XCUIDevice+FBHelpers.m`），有两处误报，用元素树纠正（[`elements::cover_sheet`]）：
    /// - 初值为 false：WDA 在手机锁着时启动就一直报未锁屏，直到下一次锁屏/解锁。确认过一次「确实没锁屏」之后
    ///   WDA 就跟上了，此后它说没锁就直接信，不多发请求
    /// - 下拉通知中心时报 true（和锁屏同一个界面）：WDA 说锁了时核对一次，是通知中心就算没锁
    ///
    /// 核对失败时按 WDA 的结果；需要「拿不准就算锁着」时用 [`Self::check_lock`] 的 strict 模式
    pub async fn locked(&mut self) -> Result<bool> {
        Ok(self.check_lock(None, false).await?.locked)
    }

    /// 锁屏判断，并带回核对时取到的元素树（解锁流程接着用，省一次 `/source`）。
    /// - `reported`：调用方刚问过的 `/wda/locked`，省一次请求
    /// - `strict`：拿不准就算锁着。确认整串密码是否解开时用：把仍锁着误判成已解锁，下一次会把同一个错误密码再输一遍
    pub async fn check_lock(&mut self, reported: Option<bool>, strict: bool) -> Result<LockCheck> {
        let wda_locked = match reported {
            Some(v) => v,
            None => self.wda_reports_locked().await?,
        };
        if self.lock_state_trusted && (!wda_locked || strict) {
            // 已同步过：WDA 说没锁就是没锁；strict 场景在密码键盘上，WDA 说锁了也不会是通知中心
            return Ok(LockCheck { locked: wda_locked, uncertain: false, tree: None });
        }
        let (sheet, tree) = match self.cover_tree().await {
            Ok(x) => x,
            Err(e) => {
                tracing::debug!("核对锁屏界面失败，按{}处理：{e}", if strict { "锁着" } else { " WDA 的结果" });
                return Ok(LockCheck { locked: strict || wda_locked, uncertain: false, tree: None });
            }
        };
        let (locked, uncertain) = match (wda_locked, sheet) {
            (true, elements::CoverSheet::NotificationCenter) => (false, false),
            (true, _) => (true, false),
            (false, elements::CoverSheet::Locked) => {
                tracing::info!("WDA 报未锁屏，但屏幕上是锁屏界面（WDA 在锁屏时启动，状态未同步）");
                (true, false)
            }
            // 锁屏样式、却没有锁的标记：可能是通知中心，也可能是 Touch ID 机型熄屏、有通知时的锁屏
            // （iPad mini 5 / iPadOS 26.6.1 真机：没有「按下主屏幕按钮以解锁」那行）。WDA 在锁屏时启动的话一直报 false，
            // 不能在这里认定它可信，否则之后再也不核对、永远不会自动解锁
            // 严格核对时拿不准就算锁着（同 `strict` 的说明）
            (false, elements::CoverSheet::NotificationCenter) => (strict, !strict),
            // 普通界面上确认过一次：WDA 的状态是同步的
            (false, elements::CoverSheet::None) => {
                self.lock_state_trusted = true;
                (false, false)
            }
        };
        Ok(LockCheck { locked, uncertain, tree })
    }

    /// WDA 自己报的锁屏状态，不核对。可能误报，见 [`Self::locked`]；配合 [`Self::lock_state_trusted`] 做快速判断
    pub async fn wda_reports_locked(&self) -> Result<bool> {
        Ok(self.call("GET", "/wda/locked", None, Duration::from_secs(10)).await?["value"] == true)
    }

    /// 本次 WDA 启动后是否确认过一次没锁屏。确认过且 WDA 说没锁，就确定没锁
    pub fn lock_state_trusted(&self) -> bool {
        self.lock_state_trusted
    }

    /// 前台是 SpringBoard 时看元素树是锁屏、通知中心还是普通界面，并带回元素树；前台是别的 App 就是普通界面
    async fn cover_tree(&self) -> Result<(elements::CoverSheet, Option<String>)> {
        if self.active_app().await? != SPRINGBOARD {
            return Ok((elements::CoverSheet::None, None));
        }
        let xml = self.source_xml().await?;
        Ok((elements::cover_sheet(&xml), Some(xml)))
    }

    pub async fn active_app(&self) -> Result<String> {
        Ok(self.get("/wda/activeAppInfo").await?["bundleId"].as_str().unwrap_or_default().to_owned())
    }

    /// Encoded screenshot bytes. The legacy method name is retained; low-latency sessions may return JPEG.
    pub async fn screenshot_png(&mut self) -> Result<Vec<u8>> {
        if self.mjpeg_capture() {
            self.session_id().await?;
        }
        self.capture_encoded().await
    }

    /// 原始分辨率的截图（`/screenshot`，PNG），不用缩小、压缩过的 MJPEG 帧。区域放大要从原始像素裁剪
    /// （docs/architecture.md §5）；只许用 MJPEG 的有界截图策略下不支持，报错而不是给一张糊的图
    pub async fn screenshot_full_resolution(&mut self) -> Result<Vec<u8>> {
        if self.bounded_capture {
            tracing::warn!(event = "capture_rejected", category = "full_resolution_forbidden");
            return Err(WdaError::Unavailable(FULL_RESOLUTION_UNSUPPORTED.into()));
        }
        if self.low_latency {
            self.session_id().await?;
        }
        self.png_screenshot().await
    }

    /// 判断画面动没动用的一帧：能用 MJPEG（9100 端口，和控制连接互不影响）就用它，
    /// 比整张 PNG 小得多、解码也快；不支持或读失败一次后就一直用普通截图
    pub async fn motion_frame(&mut self) -> Result<Vec<u8>> {
        if self.mjpeg_capture() {
            return self.screenshot_png().await;
        }
        if !self.mjpeg_motion_broken {
            match self.transport.screenshot_jpeg().await {
                Ok(Some(jpeg)) => return Ok(jpeg),
                Ok(None) => self.mjpeg_motion_broken = true,
                Err(e) => {
                    tracing::info!(event = "mjpeg_motion_unavailable", error = %e);
                    self.mjpeg_motion_broken = true;
                }
            }
        }
        self.png_screenshot().await
    }

    /// 截图和元素树 XML。`/source` 最多等 `source_budget`，截图有它自己的时限。
    /// - MJPEG 截图走自己的连接（9100 端口），和 `/source` 真正并行
    /// - 否则两者共用一条控制连接（请求排队），并行只会让 `/source` 排在约 11MB 的截图后面白白耗掉预算：
    ///   先截图，截完再开始算 `/source` 的预算
    pub async fn screenshot_with_source(&mut self, source_budget: Duration) -> (Result<Vec<u8>>, Result<String>) {
        if !self.mjpeg_capture() {
            let frame = self.png_screenshot().await;
            return (frame, self.source_within(source_budget).await);
        }
        if let Err(error) = self.session_id().await {
            let source = Err(WdaError::Unavailable(error.to_string()));
            return (Err(error), source);
        }
        let (jpeg, source) = tokio::join!(self.mjpeg_frame(), self.source_within(source_budget));
        let frame = match jpeg {
            Ok(Some(jpeg)) => Ok(jpeg),
            // 传输不支持 MJPEG：`/source` 已经读完，再走普通截图，不和它抢连接
            Ok(None) if !self.bounded_capture => self.png_screenshot().await,
            Ok(None) => Err(bounded_unsupported()),
            Err(e) => Err(e),
        };
        (frame, source)
    }

    /// 截图走 MJPEG（低延迟、有界截图）
    fn mjpeg_capture(&self) -> bool {
        self.low_latency || self.bounded_capture
    }

    /// `/source`，HTTP 请求本身的时限就是 `budget`（超时的请求连同连接一起丢掉，不留在连接池里）
    async fn source_within(&self, budget: Duration) -> Result<String> {
        self.source_xml_timeout(budget).await.map_err(|e| match e {
            WdaError::Timeout(_) => WdaError::Timeout("UI element snapshot budget exceeded".into()),
            other => other,
        })
    }

    /// One MJPEG frame; `None` when the transport has no MJPEG support. A failed read is an error and
    /// must not trigger a multi-megabyte PNG fallback.
    async fn mjpeg_frame(&self) -> Result<Option<Vec<u8>>> {
        self.transport.screenshot_jpeg().await.map_err(|error| {
            tracing::warn!(event = "capture_rejected", category = "mjpeg_failed");
            WdaError::Unavailable(format!("MJPEG capture failed: {error}"))
        })
    }

    /// Encoded screenshot bytes; requires an existing session for MJPEG capture.
    async fn capture_encoded(&self) -> Result<Vec<u8>> {
        if self.mjpeg_capture() {
            // WDA 16.15.3 converts /screenshot back to PNG even when the source
            // quality is JPEG. Capture one MJPEG frame before the WAN instead.
            if let Some(jpeg) = self.mjpeg_frame().await? {
                return Ok(jpeg);
            }
            if self.bounded_capture {
                return Err(bounded_unsupported());
            }
        }
        self.png_screenshot().await
    }

    /// `/screenshot`：原始分辨率 PNG
    async fn png_screenshot(&self) -> Result<Vec<u8>> {
        // base64 约 11MB：从 JSON 里直接拿走字符串，原地去掉换行，不再另外复制
        let Value::String(mut b64) = self.get("/screenshot").await? else {
            return Err(WdaError::Unavailable(
                ictl_l10n::tr!("截图响应不是字符串", "screenshot response is not a string").into(),
            ));
        };
        let decoding = StepTimer::new("screenshot_base64_decode");
        let encoded_bytes = b64.len();
        b64.retain(|c| !c.is_whitespace());
        let decoded = base64::engine::general_purpose::STANDARD.decode(b64).map_err(|e| {
            WdaError::Unavailable(ictl_l10n::trf!("截图不是有效的 base64：{e}", "screenshot is not valid base64: {e}"))
        });
        tracing::info!(
            event = "screenshot_decoded",
            encoded_bytes,
            decoded_bytes = decoded.as_ref().map_or(0, |data| data.len())
        );
        decoding.finish(decoded.is_ok());
        decoded
    }

    /// 元素树 XML，排除可见性等慢属性（完整树 7～8 秒，排除后约 0.3 秒）
    pub async fn source_xml(&self) -> Result<String> {
        self.source_xml_timeout(SOURCE_TIMEOUT).await
    }

    /// 同 [`Self::source_xml`]，请求时限由调用方给（轮询时按剩下的预算）
    pub async fn source_xml_timeout(&self, timeout: Duration) -> Result<String> {
        let path = format!("/source?format=xml&excluded_attributes={}", elements::SOURCE_EXCLUDED);
        let v = self.call("GET", &path, None, timeout).await?;
        Ok(v["value"].as_str().unwrap_or_default().to_owned())
    }

    pub async fn elements(&mut self) -> Result<Vec<elements::Element>> {
        Ok(self.tree().await?.elements)
    }

    /// 元素树，带上「谁盖住谁」「有没有加载圈」等信息
    pub async fn tree(&mut self) -> Result<elements::Tree> {
        let view = self.view().await?;
        self.tree_in_view(view).await
    }
    /// Reuse orientation measured for the image in the same locked observation.
    pub async fn tree_in_view(&self, view: elements::View) -> Result<elements::Tree> {
        Ok(elements::parse_tree(&self.source_xml().await?, view))
    }

    /// 同 [`Self::tree_in_view`]，`/source` 最多等 `timeout`（报 [`WdaError::Timeout`]）
    pub async fn tree_in_view_within(&self, view: elements::View, timeout: Duration) -> Result<elements::Tree> {
        Ok(elements::parse_tree(&self.source_xml_timeout(timeout).await?, view))
    }

    /// 点击：一个 W3C actions 单指手势（按下 50ms），期间把等待设置临时调小。真机计时（iOS 27）：
    /// - `/wda/tap` 先等界面空闲：主屏点图标约 1.7 秒（调小等待也没用），App 里约 0.6 秒，锁屏上约 2.9 秒
    /// - actions 手势做完要等动画冷却（`animationCoolOffTimeout`，默认 2 秒）：App 里切标签约 1.9 秒
    /// - actions + 临时调小等待：App 里约 0.7 秒，主屏点图标约 0.9 秒
    ///
    /// 和 `/wda/tap` 一样不等点击引起的动画结束，调用方要看结果就再截图
    pub async fn tap(&mut self, x: f64, y: f64) -> Result<()> {
        self.quick_gesture(&tap_steps(x, y, TAP_PRESS_MS), IdleSettings::QUICK).await
    }

    /// 长按：按下、停 `seconds` 秒、抬起，一个 actions 手势。`/wda/touchAndHold` 前后都等界面空闲，
    /// 真机上主屏长按图标 1.2 秒要约 12 秒才返回
    pub async fn touch_and_hold(&mut self, x: f64, y: f64, seconds: f64) -> Result<()> {
        self.quick_gesture(&tap_steps(x, y, (seconds * 1000.0) as u64), IdleSettings::QUICK).await
    }

    /// 滑动：一个 actions 手势，期间用 [`IdleSettings::NO_COOL_OFF`]。做完时列表可能还在惯性滚动，
    /// 要看结果的调用方自己等界面稳定
    pub async fn swipe(&mut self, from: (f64, f64), to: (f64, f64), ms: u64) -> Result<()> {
        self.quick_gesture(&swipe_steps(from, to, ms), IdleSettings::NO_COOL_OFF).await
    }

    /// 开始轮询界面：读元素树时不等界面空闲、动画冷却（约 0.2 秒一次），由调用方比较前后两次判断稳定。
    /// 用完调 [`Self::end_polling`] 恢复
    pub async fn begin_polling(&mut self) -> Result<()> {
        self.baseline().await?;
        self.set_idle_settings(IdleSettings::NO_COOL_OFF).await
    }

    pub async fn end_polling(&mut self) {
        let b = self.idle_baseline.unwrap_or(IdleSettings::DEFAULT);
        if let Err(e) = self.set_idle_settings(b).await {
            tracing::warn!("轮询后恢复 WDA 等待设置失败：{e}");
        }
    }

    /// 平时的等待设置（第一次用时读一次）。临时调小、改回之前都按它恢复
    pub async fn baseline(&mut self) -> Result<IdleSettings> {
        if self.low_latency {
            self.session_id().await?;
        }
        if let Some(b) = self.idle_baseline {
            return Ok(b);
        }
        // 上次没恢复成功的话读到的就是调小的值：按 WDA 默认值算
        let b = match self.idle_settings().await? {
            s if s == IdleSettings::QUICK || s == IdleSettings::NO_COOL_OFF => IdleSettings::DEFAULT,
            s => s,
        };
        self.idle_baseline = Some(b);
        Ok(b)
    }

    /// 发一个手势，期间把等待设置临时换成 `during`（见 [`Self::tap`]），做完恢复
    async fn quick_gesture(&mut self, steps: &[PointerStep], during: IdleSettings) -> Result<()> {
        let baseline = self.baseline().await?;
        if self.profile_applied {
            self.set_idle_settings(IdleSettings::NO_COOL_OFF).await?;
            self.actions(steps).await?;
            // Keep a render window for taps/long presses without two settings round trips.
            if during == IdleSettings::QUICK {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            return Ok(());
        }
        self.set_idle_settings(during).await?;
        let r = self.actions(steps).await;
        if matches!(r, Err(WdaError::Unavailable(_))) {
            // 连不上：恢复设置也连不上（每次最长要等 10 秒建连接），不试了；下次手势照常先设再恢复
            self.idle_current = None;
            return r;
        }
        if let Err(e) = self.set_idle_settings(baseline).await {
            tracing::warn!("手势后恢复 WDA 等待设置失败：{e}");
        }
        r
    }

    /// 一次 W3C actions 请求。注意：多次点击合并成一个请求会漏键，解锁时每位单独发
    pub async fn actions(&mut self, steps: &[PointerStep]) -> Result<()> {
        self.post("/actions", actions_body(steps)).await.map(drop)
    }

    /// 屏幕上的系统弹窗（定位权限、通知权限等）的文字；没有弹窗返回 None。
    /// 弹窗挡着时 Home 不生效、App 也到不了前台，WDA 却照样报成功
    pub async fn alert_text(&mut self) -> Result<Option<String>> {
        match self.in_session("GET", "/alert/text", None, DEFAULT_TIMEOUT).await {
            Ok(v) => Ok(v.as_str().map(str::to_owned).filter(|t| !t.is_empty())),
            Err(WdaError::Remote { kind, .. }) if kind.contains("alert") => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// 屏幕上有没有软键盘（有输入框获得了焦点）。`/wda/keys` 在没有焦点时也返回成功，什么都没输进去
    pub async fn keyboard_shown(&mut self) -> Result<bool> {
        let v = self.post("/elements", json!({"using": "class name", "value": "XCUIElementTypeKeyboard"})).await?;
        Ok(v.as_array().is_some_and(|a| !a.is_empty()))
    }

    pub async fn keys(&mut self, text: &str) -> Result<()> {
        let chars: Vec<String> = text.chars().map(String::from).collect();
        self.in_session("POST", "/wda/keys", Some(json!({"value": chars})), Duration::from_secs(120)).await.map(drop)
    }

    /// home / volumeUp / volumeDown，走 XCTest 的 `XCUIDevice pressButton`。
    /// iPhone 12 mini（iOS 26.4.1）上 home 不起作用但照样报成功，见 [`Self::press_home_hid`]
    pub async fn press_button(&mut self, name: &str) -> Result<()> {
        self.post("/wda/pressButton", json!({"name": name})).await.map(drop)
    }

    /// 直接发一个 IOHID 按键事件（`/wda/performIoHidEvent`），按住 `seconds` 秒
    pub async fn hid_event(&mut self, page: u32, usage: u32, seconds: f64) -> Result<()> {
        self.post("/wda/performIoHidEvent", json!({"page": page, "usage": usage, "duration": seconds})).await.map(drop)
    }

    /// 经 IOHID 按 Home（消费者页 AC Home）。iPhone 12 mini（iOS 26.4.1）实测：XCTest 的 home 无效时
    /// 它能从 App 回到主屏、关掉 Spotlight、点亮熄灭的屏幕；同页的 Menu（0x40）没反应
    pub async fn press_home_hid(&mut self) -> Result<()> {
        self.hid_event(HID_PAGE_CONSUMER, HID_AC_HOME, 0.05).await
    }

    /// 经 IOHID 按电源键（按住 0.5 秒）。WDA 自己的 `/wda/lock` 在 iOS 27 上也是这么做的，
    /// iOS 27 以下仍用 XCTest 的 `pressLockButton`，iPhone 12 mini（iOS 26.4.1）上锁不了屏。
    /// 屏幕已熄灭时按它会把屏幕点亮，调用方要先确认没锁屏
    pub async fn press_power_hid(&mut self) -> Result<()> {
        self.hid_event(HID_PAGE_CONSUMER, HID_POWER, 0.5).await
    }

    pub async fn lock(&mut self) -> Result<()> {
        self.post("/wda/lock", json!({})).await.map(drop)
    }

    /// 启动并等到前台、界面稳定（冷启动后立即点击会被吞），最多等 8 秒。
    /// 等待期间查前台出错不算失败（启动命令已经成功了）
    pub async fn launch_app(&mut self, bundle_id: &str) -> Result<Launch> {
        let started = tokio::time::Instant::now();
        self.in_session("POST", "/wda/apps/launch", Some(json!({"bundleId": bundle_id})), Duration::from_secs(60))
            .await?;
        let t0 = tokio::time::Instant::now();
        let deadline = t0 + Duration::from_secs(8);
        let mut foreground = false;
        while tokio::time::Instant::now() < deadline {
            match self.active_app().await {
                Ok(a) if a == bundle_id => {
                    foreground = true;
                    break;
                }
                // 系统弹窗（权限请求）挡着时前台一直是 SpringBoard，原来要白等满 8 秒。
                // 刚启动的一瞬间前台也还是 SpringBoard，过 1 秒还是才去查弹窗（查一次约 0.4 秒）
                Ok(a) if a == SPRINGBOARD && bundle_id != SPRINGBOARD && t0.elapsed() >= Duration::from_secs(1) => {
                    if let Ok(Some(text)) = self.alert_text().await {
                        return Ok(Launch::Blocked(text));
                    }
                }
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        if !foreground {
            return Ok(match self.alert_text().await {
                Ok(Some(text)) => Launch::Blocked(text),
                _ => Launch::Slow,
            });
        }
        self.settle_launched(started).await;
        Ok(Launch::Foreground)
    }

    /// App 到了前台之后再等它画出来。`started`：发出启动请求的时刻
    async fn settle_launched(&mut self, started: tokio::time::Instant) {
        // 冷启动后立即点击会被吞（回归用例 #13）。原来固定再等 1 秒；改为等界面稳定：连续两次元素树一样
        // （读元素树时 WDA 本身也等界面空闲），最多仍是 1 秒
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let mut prev = None;
        while tokio::time::Instant::now() < deadline {
            let cur = self.source_xml().await.ok();
            if cur.is_some() && cur == prev {
                break;
            }
            prev = cur;
        }
        // 切回已在后台的 App 时元素树马上就稳定了（约 0.4 秒），放大进场的动画还没完，这时点击会被吞（真机：小红书、高德各一次）
        if let Some(left) = LAUNCH_MIN.checked_sub(started.elapsed()) {
            tokio::time::sleep(left).await;
        }
    }

    pub async fn terminate_app(&mut self, bundle_id: &str) -> Result<()> {
        self.post("/wda/apps/terminate", json!({"bundleId": bundle_id})).await.map(drop)
    }

    /// 打开网址或 URL Scheme，等打开它的 App 到前台、画出来再返回（最多约 8 秒），同 [`Self::launch_app`]。
    /// 不等的话 App 冷启动时还是黑屏，紧接着截图会把两帧黑屏当成「画面已静止」（iPad Safari 真机）。
    /// 返回结果和前台 App；在原来的前台 App 里打开（Safari 里开网址）时等 1 秒没换就算打开了
    pub async fn open_url(&mut self, url: &str) -> Result<(Launch, String)> {
        let started = tokio::time::Instant::now();
        let before = self.active_app().await.unwrap_or_default();
        self.in_session("POST", "/url", Some(json!({"url": url})), Duration::from_secs(60)).await?;
        let deadline = started + Duration::from_secs(8);
        // 前台要连续两次是同一个才算：Safari 冷启动的一瞬间前台是 `com.apple.DocumentManager.DockFolderViewService`（iPad 真机）
        let mut last = String::new();
        loop {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let waited = started.elapsed() >= Duration::from_secs(1);
            let now = self.active_app().await;
            let steady = now.as_ref().is_ok_and(|a| *a == last);
            if let Ok(a) = &now {
                last.clone_from(a);
            }
            match now {
                Ok(a) if a != before && a != SPRINGBOARD && steady => {
                    self.settle_launched(started).await;
                    return Ok((Launch::Foreground, a));
                }
                Ok(a) if a == before && a != SPRINGBOARD && waited => {
                    self.settle_launched(started).await;
                    return Ok((Launch::Foreground, a));
                }
                // 「要在某 App 中打开吗」之类的系统弹窗挡着时前台一直是 SpringBoard
                Ok(a) if a == SPRINGBOARD && waited => {
                    if let Ok(Some(text)) = self.alert_text().await {
                        return Ok((Launch::Blocked(text), a));
                    }
                }
                _ => {}
            }
            if tokio::time::Instant::now() >= deadline {
                let now = self.active_app().await.unwrap_or_default();
                return Ok((Launch::Slow, now));
            }
        }
    }

    pub async fn idle_settings(&mut self) -> Result<IdleSettings> {
        let v = self.in_session("GET", "/appium/settings", None, DEFAULT_TIMEOUT).await?;
        let settings = IdleSettings {
            wait_for_idle_timeout: v["waitForIdleTimeout"].as_f64().unwrap_or(10.0),
            animation_cool_off_timeout: v["animationCoolOffTimeout"].as_f64().unwrap_or(2.0),
        };
        self.idle_current = Some(settings);
        Ok(settings)
    }

    /// 改等待设置；已经是这个值就不发请求（同一请求里先后调小、恢复时常常是同一个值）。
    /// 会话重建、请求出错时 `idle_current` 会清掉，下次照常发
    pub async fn set_idle_settings(&mut self, s: IdleSettings) -> Result<()> {
        if self.idle_current == Some(s) {
            return Ok(());
        }
        let body = json!({"settings": {"waitForIdleTimeout": s.wait_for_idle_timeout, "animationCoolOffTimeout": s.animation_cool_off_timeout}});
        self.idle_current = None;
        self.post("/appium/settings", body).await?;
        self.idle_current = Some(s);
        Ok(())
    }

    /// 锁屏密码栏的 (总位数, 已输入位数)；找不到密码栏返回 None
    pub async fn passcode_field(&mut self) -> Result<Option<(u32, u32)>> {
        let els =
            self.post("/elements", json!({"using": "class name", "value": "XCUIElementTypeSecureTextField"})).await?;
        for e in els.as_array().into_iter().flatten() {
            let Some(eid) = e.as_object().and_then(|o| o.values().next()).and_then(|v| v.as_str()) else { continue };
            let v = self.in_session("GET", &format!("/element/{eid}/attribute/value"), None, DEFAULT_TIMEOUT).await?;
            if let Some(p) = v.as_str().and_then(elements::parse_passcode_value) {
                return Ok(Some(p));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// 按顺序返回预设响应，并记录收到的请求
    #[derive(Default)]
    struct Mock {
        replies: Mutex<VecDeque<(u16, String)>>,
        seen: Mutex<Vec<(String, String, Option<String>)>>,
    }

    impl Mock {
        fn with(replies: &[(u16, &str)]) -> Self {
            Self {
                replies: Mutex::new(replies.iter().map(|(s, b)| (*s, b.to_string())).collect()),
                ..Default::default()
            }
        }
    }

    impl Transport for &Mock {
        async fn send(
            &self,
            method: &str,
            path: &str,
            body: Option<Vec<u8>>,
            _: Duration,
        ) -> std::io::Result<(u16, Vec<u8>)> {
            self.seen.lock().unwrap().push((method.into(), path.into(), body.map(|b| String::from_utf8(b).unwrap())));
            let (s, b) = self.replies.lock().unwrap().pop_front().ok_or_else(|| std::io::Error::other("连接被拒绝"))?;
            Ok((s, b.into_bytes()))
        }
    }

    #[tokio::test]
    async fn session_is_recreated_once_after_invalid_session() {
        let m = Mock::with(&[
            (200, r#"{"sessionId":"A","value":{}}"#),
            (404, r#"{"value":{"error":"invalid session id","message":"gone\nmore"}}"#),
            (200, r#"{"value":{"sessionId":"B"}}"#),
            (200, r#"{"value":null}"#),
        ]);
        let mut c = WdaClient::new(&m);
        c.press_button("home").await.unwrap();
        let paths: Vec<_> = m.seen.lock().unwrap().iter().map(|s| s.1.clone()).collect();
        assert_eq!(paths, ["/session", "/session/A/wda/pressButton", "/session", "/session/B/wda/pressButton"]);
    }

    #[tokio::test]
    async fn hid_buttons_send_consumer_page_events() {
        let ok = r#"{"value":null}"#;
        let m = Mock::with(&[(200, r#"{"sessionId":"A"}"#), (200, ok), (200, ok)]);
        let mut c = WdaClient::new(&m);
        c.press_home_hid().await.unwrap();
        c.press_power_hid().await.unwrap();
        let seen = m.seen.lock().unwrap().clone();
        assert_eq!(seen[1].1, "/session/A/wda/performIoHidEvent");
        let body = |i: usize| serde_json::from_str::<Value>(seen[i].2.as_deref().unwrap()).unwrap();
        assert_eq!((body(1)["page"].as_u64(), body(1)["usage"].as_u64()), (Some(0x0C), Some(0x223)));
        assert_eq!((body(2)["page"].as_u64(), body(2)["usage"].as_u64()), (Some(0x0C), Some(0x30)));
        assert_eq!(body(2)["duration"], 0.5);
    }

    #[tokio::test]
    async fn tap_is_one_gesture_with_quick_waits_then_restores_baseline() {
        let ok = r#"{"value":null}"#;
        let m = Mock::with(&[
            (200, r#"{"sessionId":"A"}"#),
            (200, r#"{"value":{"waitForIdleTimeout":10,"animationCoolOffTimeout":2}}"#),
            (200, ok),
            (200, ok),
            (200, ok),
            // 第二次点击不再读设置
            (200, ok),
            (200, ok),
            (200, ok),
        ]);
        let mut c = WdaClient::new(&m);
        c.tap(10.0, 20.0).await.unwrap();
        c.tap(30.0, 40.0).await.unwrap();
        let seen = m.seen.lock().unwrap().clone();
        let paths: Vec<_> = seen.iter().map(|s| s.1.as_str()).collect();
        assert_eq!(
            paths,
            [
                "/session",
                "/session/A/appium/settings",
                "/session/A/appium/settings",
                "/session/A/actions",
                "/session/A/appium/settings",
                "/session/A/appium/settings",
                "/session/A/actions",
                "/session/A/appium/settings",
            ]
        );
        let body = |i: usize| serde_json::from_str::<Value>(seen[i].2.as_deref().unwrap()).unwrap();
        assert_eq!(body(2)["settings"]["animationCoolOffTimeout"], 0.2);
        assert_eq!(body(4)["settings"]["animationCoolOffTimeout"], 2.0);
        let a = body(3);
        let steps = a["actions"][0]["actions"].as_array().unwrap();
        assert_eq!(steps.iter().filter(|s| s["type"] == "pointerDown").count(), 1);
    }

    #[tokio::test]
    async fn browser_profile_requests_source_jpeg_once_and_reuses_gesture_settings() {
        let m = Mock::with(&[
            (200, r#"{"sessionId":"A"}"#),
            (200, r#"{"value":{"screenshotQuality":1,"waitForIdleTimeout":1,"animationCoolOffTimeout":0}}"#),
            (200, r#"{"value":"/9j/"}"#),
            (200, r#"{"value":null}"#),
            (200, r#"{"value":null}"#),
        ]);
        let mut client = WdaClient::new(&m).with_low_latency(true);
        assert_eq!(client.screenshot_png().await.unwrap(), [255, 216, 255]);
        client.tap(10.0, 20.0).await.unwrap();
        client.tap(30.0, 40.0).await.unwrap();
        let seen = m.seen.lock().unwrap();
        assert_eq!(
            seen.iter().map(|r| r.1.as_str()).collect::<Vec<_>>(),
            ["/session", "/session/A/appium/settings", "/screenshot", "/session/A/actions", "/session/A/actions"]
        );
        let profile: Value = serde_json::from_str(seen[1].2.as_deref().unwrap()).unwrap();
        assert_eq!(profile["settings"]["screenshotQuality"], 1);
        assert_eq!(profile["settings"]["mjpegServerFramerate"], 5);
        assert_eq!(profile["settings"]["mjpegScalingFactor"], MJPEG_SCALING_PERCENT);
        assert_eq!(profile["settings"]["mjpegServerScreenshotQuality"], 40);
    }

    struct JpegTransport {
        http: Mock,
        fail: bool,
        captures: AtomicU64,
    }

    impl Transport for &JpegTransport {
        async fn send(
            &self,
            method: &str,
            path: &str,
            body: Option<Vec<u8>>,
            timeout: Duration,
        ) -> std::io::Result<(u16, Vec<u8>)> {
            (&self.http).send(method, path, body, timeout).await
        }

        async fn screenshot_jpeg(&self) -> std::io::Result<Option<Vec<u8>>> {
            self.captures.fetch_add(1, Ordering::Relaxed);
            if self.fail {
                Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "slow capture"))
            } else {
                Ok(Some(vec![255, 216, 255, 217]))
            }
        }
    }

    #[tokio::test]
    async fn browser_capture_uses_one_jpeg_instead_of_requesting_a_large_png() {
        for fail in [false, true] {
            let transport = JpegTransport {
                http: Mock::with(&[
                    (200, r#"{"sessionId":"A"}"#),
                    (200, r#"{"value":{"screenshotQuality":1,"waitForIdleTimeout":1,"animationCoolOffTimeout":0}}"#),
                ]),
                fail,
                captures: AtomicU64::new(0),
            };
            let mut client = WdaClient::new(&transport).with_low_latency(true);
            let result = client.screenshot_png().await;
            if fail {
                assert!(matches!(result, Err(WdaError::Unavailable(_))));
            } else {
                assert_eq!(result.unwrap(), [255, 216, 255, 217]);
            }
            assert_eq!(transport.captures.load(Ordering::Relaxed), 1);
            assert_eq!(
                transport.http.seen.lock().unwrap().iter().map(|r| r.1.as_str()).collect::<Vec<_>>(),
                ["/session", "/session/A/appium/settings"],
                "even a failed JPEG read must not start a PNG transfer"
            );
        }
    }

    #[tokio::test]
    async fn screenshot_with_source_returns_the_frame_and_tree_from_one_call() {
        let transport = JpegTransport {
            http: Mock::with(&[
                (200, r#"{"sessionId":"A"}"#),
                (200, r#"{"value":{"screenshotQuality":1,"waitForIdleTimeout":1,"animationCoolOffTimeout":0}}"#),
                (200, r#"{"value":"<XCUIElementTypeApplication/>"}"#),
            ]),
            fail: false,
            captures: AtomicU64::new(0),
        };
        let mut client = WdaClient::new(&transport).with_low_latency(true);
        let (frame, source) = client.screenshot_with_source(Duration::from_secs(1)).await;
        assert_eq!(frame.unwrap(), [255, 216, 255, 217]);
        assert_eq!(source.unwrap(), "<XCUIElementTypeApplication/>");
        assert_eq!(transport.captures.load(Ordering::Relaxed), 1);
        let seen = transport.http.seen.lock().unwrap();
        assert!(seen[2].1.starts_with("/source?"), "tree read must not wait for a PNG: {seen:?}");
    }

    #[tokio::test]
    async fn screenshot_with_source_reports_a_session_failure_for_both_reads() {
        let transport = JpegTransport {
            http: Mock::with(&[(500, r#"{"value":{"error":"unknown error","message":"no"}}"#)]),
            fail: false,
            captures: AtomicU64::new(0),
        };
        let mut client = WdaClient::new(&transport).with_low_latency(true);
        let (frame, source) = client.screenshot_with_source(Duration::from_secs(1)).await;
        assert!(frame.is_err() && source.is_err());
        assert_eq!(transport.captures.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn ordinary_clients_keep_the_original_full_resolution_screenshot_path() {
        let transport = JpegTransport {
            http: Mock::with(&[(200, r#"{"value":"cG5n"}"#)]),
            fail: false,
            captures: AtomicU64::new(0),
        };
        let mut client = WdaClient::new(&transport);
        assert_eq!(client.screenshot_png().await.unwrap(), b"png");
        assert_eq!(transport.captures.load(Ordering::Relaxed), 0);
        assert_eq!(transport.http.seen.lock().unwrap()[0].1, "/screenshot");
    }

    #[tokio::test]
    async fn bounded_capture_is_independent_of_latency_and_preserves_mjpeg_failure() {
        for fail in [false, true] {
            let transport = JpegTransport {
                http: Mock::with(&[(200, r#"{"sessionId":"A"}"#), (200, r#"{"value":"cG5n"}"#)]),
                fail,
                captures: AtomicU64::new(0),
            };
            let mut client = WdaClient::new(&transport).with_low_latency(false).with_bounded_capture(true);
            let result = client.screenshot_png().await;
            assert_eq!(
                transport.captures.load(Ordering::Relaxed),
                1,
                "bounded capture must use MJPEG even with latency disabled"
            );
            if fail {
                assert!(
                    matches!(result, Err(WdaError::Unavailable(ref e)) if e.contains("slow capture")),
                    "{result:?}"
                );
            } else {
                assert_eq!(result.unwrap(), [255, 216, 255, 217]);
            }
            assert!(!transport.http.seen.lock().unwrap().iter().any(|r| r.1 == "/screenshot"));
        }
    }

    #[tokio::test]
    async fn bounded_capture_rejects_unsupported_transport_without_png() {
        let transport = Mock::with(&[(200, r#"{"sessionId":"A"}"#)]);
        let mut client = WdaClient::new(&transport).with_bounded_capture(true);
        let result = client.screenshot_png().await;
        assert!(
            matches!(result, Err(WdaError::Unavailable(ref e)) if e.contains("bounded MJPEG capture is unsupported")),
            "{result:?}"
        );
        assert!(!transport.seen.lock().unwrap().iter().any(|r| r.1 == "/screenshot"));
    }

    /// 截图和 `/source` 共用一条连接的传输：一次只处理一个请求，时限含排队
    struct SharedPool {
        pool: tokio::sync::Mutex<()>,
        seen: Mutex<Vec<String>>,
    }

    impl Transport for &SharedPool {
        async fn send(&self, _: &str, path: &str, _: Option<Vec<u8>>, t: Duration) -> std::io::Result<(u16, Vec<u8>)> {
            let path = path.to_owned();
            tokio::time::timeout(t, async {
                let _busy = self.pool.lock().await;
                self.seen.lock().unwrap().push(path.clone());
                if path == "/screenshot" {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    return Ok((200, br#"{"value":"cG5n"}"#.to_vec()));
                }
                Ok((200, br#"{"value":"<XCUIElementTypeApplication/>"}"#.to_vec()))
            })
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "slow"))?
        }
    }

    /// 没有 MJPEG 时截图和 `/source` 共用连接：`/source` 等截图完了才开始算预算，不会因为排在 PNG 后面超时
    #[tokio::test(start_paused = true)]
    async fn without_mjpeg_source_budget_starts_after_the_screenshot() {
        let t = SharedPool { pool: tokio::sync::Mutex::new(()), seen: Mutex::new(Vec::new()) };
        let mut client = WdaClient::new(&t);
        let (frame, source) = client.screenshot_with_source(Duration::from_secs(1)).await;
        assert_eq!(frame.unwrap(), b"png");
        assert_eq!(source.unwrap(), "<XCUIElementTypeApplication/>");
        let seen = t.seen.lock().unwrap();
        assert_eq!(seen[0], "/screenshot");
        assert!(seen[1].starts_with("/source?"));
    }

    /// 区域放大要原始像素：低延迟会话也走 `/screenshot`，不拿 40% 的 MJPEG 帧；有界策略下明确报不支持
    #[tokio::test]
    async fn region_capture_uses_full_resolution_png_or_refuses_under_bounded_policy() {
        let transport = JpegTransport {
            http: Mock::with(&[
                (200, r#"{"sessionId":"A"}"#),
                (200, r#"{"value":{"screenshotQuality":1,"waitForIdleTimeout":1,"animationCoolOffTimeout":0}}"#),
                (200, r#"{"value":"cG5n"}"#),
            ]),
            fail: false,
            captures: AtomicU64::new(0),
        };
        let mut client = WdaClient::new(&transport).with_low_latency(true);
        assert_eq!(client.screenshot_full_resolution().await.unwrap(), b"png");
        assert_eq!(transport.captures.load(Ordering::Relaxed), 0);
        assert_eq!(transport.http.seen.lock().unwrap()[2].1, "/screenshot");

        let m = Mock::with(&[]);
        let mut bounded = WdaClient::new(&m).with_bounded_capture(true);
        let error = bounded.screenshot_full_resolution().await.unwrap_err();
        assert!(error.to_string().contains("bounded remote capture policy"), "{error}");
        assert!(m.seen.lock().unwrap().is_empty());
    }

    /// 判断画面动没动优先用 MJPEG 帧；读不到一次就一直用普通截图
    #[tokio::test]
    async fn motion_frames_prefer_mjpeg_and_fall_back_once_it_fails() {
        let ok = JpegTransport { http: Mock::with(&[]), fail: false, captures: AtomicU64::new(0) };
        let mut client = WdaClient::new(&ok);
        assert_eq!(client.motion_frame().await.unwrap(), [255, 216, 255, 217]);
        assert_eq!(client.motion_frame().await.unwrap(), [255, 216, 255, 217]);
        assert!(ok.http.seen.lock().unwrap().is_empty());

        let failing = JpegTransport {
            http: Mock::with(&[(200, r#"{"value":"cG5n"}"#), (200, r#"{"value":"cG5n"}"#)]),
            fail: true,
            captures: AtomicU64::new(0),
        };
        let mut client = WdaClient::new(&failing);
        assert_eq!(client.motion_frame().await.unwrap(), b"png");
        assert_eq!(client.motion_frame().await.unwrap(), b"png");
        assert_eq!(failing.captures.load(Ordering::Relaxed), 1);
    }

    /// 等待设置已经是要设的值就不发请求（不管有没有应用低延迟配置）
    #[tokio::test]
    async fn idle_settings_are_not_resent_when_unchanged() {
        let ok = r#"{"value":null}"#;
        let m = Mock::with(&[
            (200, r#"{"sessionId":"A"}"#),
            (200, r#"{"value":{"waitForIdleTimeout":10,"animationCoolOffTimeout":2}}"#),
            (200, ok),
            (200, ok),
            (200, ok),
        ]);
        let mut c = WdaClient::new(&m);
        c.tap(1.0, 2.0).await.unwrap();
        // 点完已经恢复成平时的值：结束轮询不用再发
        c.end_polling().await;
        c.set_idle_settings(IdleSettings::DEFAULT).await.unwrap();
        assert_eq!(m.seen.lock().unwrap().len(), 5);
    }

    #[tokio::test]
    async fn rejected_browser_profile_preserves_normal_gesture_restoration() {
        let m = Mock::with(&[
            (200, r#"{"sessionId":"A"}"#),
            (400, r#"{"value":{"error":"invalid argument","message":"unsupported setting"}}"#),
            (200, r#"{"value":{"waitForIdleTimeout":10,"animationCoolOffTimeout":2}}"#),
            (200, r#"{"value":null}"#),
            (200, r#"{"value":null}"#),
            (200, r#"{"value":null}"#),
        ]);
        let mut client = WdaClient::new(&m).with_low_latency(true);
        client.tap(10.0, 20.0).await.unwrap();
        assert!(!client.profile_applied);
        let seen = m.seen.lock().unwrap();
        let restore: Value = serde_json::from_str(seen.last().unwrap().2.as_deref().unwrap()).unwrap();
        assert_eq!(restore["settings"]["animationCoolOffTimeout"], 2.0);
    }

    #[tokio::test]
    async fn a_shared_image_view_avoids_a_second_orientation_request() {
        let m = Mock::with(&[(200, r#"{"value":"<AppiumAUT/>"}"#)]);
        let client = WdaClient::new(&m);
        let view = elements::View {
            screen: ScreenSize { width: 844.0, height: 390.0 },
            orientation: Some(elements::Orientation::LandscapeLeft),
        };
        client.tree_in_view(view).await.unwrap();
        let seen = m.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].1.starts_with("/source?"));
    }

    #[tokio::test]
    async fn remote_error_keeps_first_line_and_unreachable_maps_to_unavailable() {
        let m = Mock::with(&[
            (200, r#"{"sessionId":"A"}"#),
            (500, r#"{"value":{"error":"unknown error","message":"boom\nstack"}}"#),
        ]);
        let mut c = WdaClient::new(&m);
        match c.press_button("home").await {
            Err(WdaError::Remote { kind, message }) => {
                assert_eq!((kind.as_str(), message.as_str()), ("unknown error", "boom"))
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(c.active_app().await, Err(WdaError::Unavailable(_))));
    }

    /// 传输层报的错误按「请求送没送到」分类：只有没送到的才是 Unavailable（调用方会退到 HID 重做）
    #[tokio::test]
    async fn errors_after_sending_are_not_unavailable() {
        struct Fails(std::io::ErrorKind);
        impl Transport for Fails {
            async fn send(&self, _: &str, _: &str, _: Option<Vec<u8>>, _: Duration) -> std::io::Result<(u16, Vec<u8>)> {
                Err(std::io::Error::new(self.0, "x"))
            }
        }
        let kind = |k| async move { WdaClient::new(Fails(k)).active_app().await };
        assert!(matches!(kind(crate::http::AFTER_SENT).await, Err(WdaError::Interrupted(_))));
        assert!(matches!(kind(std::io::ErrorKind::TimedOut).await, Err(WdaError::Timeout(_))));
        assert!(matches!(kind(std::io::ErrorKind::NotConnected).await, Err(WdaError::Unavailable(_))));
        // 应答了但不是 JSON：WDA 收到了请求
        let m = Mock::with(&[(200, "<html>")]);
        assert!(matches!(WdaClient::new(&m).active_app().await, Err(WdaError::Interrupted(_))));
    }

    #[tokio::test]
    async fn screen_size_is_cached_and_status_ready() {
        let unknown = r#"{"value":{"error":"unknown command","message":"no"}}"#;
        let m = Mock::with(&[
            (200, r#"{"value":{"screenSize":{"width":430,"height":932},"scale":3}}"#),
            (200, r#"{"sessionId":"S"}"#),
            (404, unknown),
            (404, unknown),
            (200, r#"{"value":{"ready":true}}"#),
        ]);
        let mut c = WdaClient::new(&m);
        // 问不到方向：按缓存的尺寸
        assert_eq!(c.screen_size().await.unwrap(), ScreenSize { width: 430.0, height: 932.0 });
        assert_eq!(c.screen_size().await.unwrap().width, 430.0);
        assert!(c.ready().await.unwrap());
        let paths: Vec<_> = m.seen.lock().unwrap().iter().map(|s| s.1.clone()).collect();
        assert_eq!(paths.iter().filter(|p| *p == "/wda/screen").count(), 1);
    }

    #[tokio::test]
    async fn rotation_reads_z_angle() {
        let m = Mock::with(&[
            (200, r#"{"sessionId":"S"}"#),
            (200, r#"{"value":{"x":0,"y":0,"z":270}}"#),
            (200, r#"{"value":{"x":0,"y":0,"z":12}}"#),
        ]);
        let mut c = WdaClient::new(&m);
        assert_eq!(c.rotation().await.unwrap(), ictl_device::Rotation::TopToLeft);
        assert!(c.rotation().await.is_err());
        assert_eq!(m.seen.lock().unwrap()[1].1, "/session/S/rotation");
    }

    /// WDA 在横屏时启动，缓存的是 1024x768；转成竖屏后要给 768x1024
    #[tokio::test]
    async fn screen_size_follows_orientation_after_rotation() {
        let m = Mock::with(&[
            (200, r#"{"value":{"screenSize":{"width":1024,"height":768},"scale":2}}"#),
            (200, r#"{"sessionId":"S"}"#),
            (200, r#"{"value":"LANDSCAPE"}"#),
            (200, r#"{"value":"PORTRAIT"}"#),
        ]);
        let mut c = WdaClient::new(&m);
        let v = c.view().await.unwrap();
        assert_eq!(
            (v.screen, v.orientation),
            (ScreenSize { width: 1024.0, height: 768.0 }, Some(elements::Orientation::LandscapeLeft))
        );
        // 同一操作内复用；过了复用时间再问就跟上了转屏
        assert_eq!(c.screen_size().await.unwrap(), ScreenSize { width: 1024.0, height: 768.0 });
        assert_eq!(m.seen.lock().unwrap().len(), 3);
        tokio::time::pause();
        tokio::time::advance(ORIENTATION_REUSE).await;
        assert_eq!(c.screen_size().await.unwrap(), ScreenSize { width: 768.0, height: 1024.0 });
        assert_eq!(m.seen.lock().unwrap()[3].1, "/session/S/orientation");
    }

    /// 截图横竖没变就复用方向；横竖一变、超时或被要求忘掉时重新问
    #[tokio::test(start_paused = true)]
    async fn frame_view_reuses_orientation_until_the_frame_shape_changes_or_it_expires() {
        let m = Mock::with(&[
            (200, r#"{"value":{"screenSize":{"width":390,"height":844},"scale":3}}"#),
            (200, r#"{"sessionId":"S"}"#),
            (200, r#"{"value":"PORTRAIT"}"#),
            (200, r#"{"value":"LANDSCAPE"}"#),
            (200, r#"{"value":"LANDSCAPE"}"#),
            (200, r#"{"value":"PORTRAIT"}"#),
        ]);
        let mut c = WdaClient::new(&m);
        let orientation_calls =
            |m: &Mock| m.seen.lock().unwrap().iter().filter(|r| r.1.ends_with("/orientation")).count();
        assert_eq!(c.view_for_frame(false).await.unwrap().orientation, Some(elements::Orientation::Portrait));
        tokio::time::advance(Duration::from_secs(30)).await;
        // 相邻一步（慢链路上一步可能超过 15 秒）、截图仍是竖的：不问
        assert_eq!(c.view_for_frame(false).await.unwrap().screen, ScreenSize { width: 390.0, height: 844.0 });
        assert_eq!(orientation_calls(&m), 1);
        // 截图变成横的：马上重新问
        assert_eq!(c.view_for_frame(true).await.unwrap().orientation, Some(elements::Orientation::LandscapeLeft));
        assert_eq!(orientation_calls(&m), 2);
        // 横屏但缓存过期：重新问
        tokio::time::advance(FRAME_ORIENTATION_REUSE).await;
        c.view_for_frame(true).await.unwrap();
        assert_eq!(orientation_calls(&m), 3);
        // 被要求忘掉：重新问
        c.forget_orientation();
        assert_eq!(c.view_for_frame(false).await.unwrap().orientation, Some(elements::Orientation::Portrait));
        assert_eq!(orientation_calls(&m), 4);
    }

    /// 从主屏打开网址：等 Safari 到前台、元素树稳定才返回，不是发完请求就走；一闪而过的系统服务不算
    #[tokio::test]
    async fn open_url_waits_for_the_opening_app() {
        let m = Mock::with(&[
            (200, r#"{"value":{"bundleId":"com.apple.springboard"}}"#),
            (200, r#"{"sessionId":"S"}"#),
            (200, r#"{"value":null}"#),
            (200, r#"{"value":{"bundleId":"com.apple.DocumentManager.DockFolderViewService"}}"#),
            (200, r#"{"value":{"bundleId":"com.apple.mobilesafari"}}"#),
            (200, r#"{"value":{"bundleId":"com.apple.mobilesafari"}}"#),
            (200, r#"{"value":"<a/>"}"#),
            (200, r#"{"value":"<a/>"}"#),
        ]);
        let mut c = WdaClient::new(&m);
        let t0 = std::time::Instant::now();
        let (r, app) = c.open_url("https://example.com").await.unwrap();
        assert_eq!((r, app.as_str()), (Launch::Foreground, "com.apple.mobilesafari"));
        assert!(t0.elapsed() >= LAUNCH_MIN);
        let paths: Vec<_> = m.seen.lock().unwrap().iter().map(|s| s.1.clone()).collect();
        assert_eq!(paths[2], "/session/S/url");
        assert!(paths[6].starts_with("/source"), "{paths:?}");
    }

    #[tokio::test]
    async fn source_request_excludes_slow_attributes() {
        let m = Mock::with(&[(200, r#"{"value":"<x/>"}"#)]);
        let c = WdaClient::new(&m);
        c.source_xml().await.unwrap();
        let path = m.seen.lock().unwrap()[0].1.clone();
        assert!(path.starts_with("/source?format=xml&excluded_attributes=visible,"), "{path}");
    }

    #[tokio::test]
    async fn passcode_field_reads_secure_text_field_value() {
        let m = Mock::with(&[
            (200, r#"{"sessionId":"S"}"#),
            (200, r#"{"value":[{"ELEMENT":"E1","element-6066-11e4-a52e-4f735466cecf":"E1"}]}"#),
            (200, r#"{"value":"共6位数，已输入2位"}"#),
        ]);
        let mut c = WdaClient::new(&m);
        assert_eq!(c.passcode_field().await.unwrap(), Some((6, 2)));
        assert_eq!(m.seen.lock().unwrap()[2].1, "/session/S/element/E1/attribute/value");
    }

    #[test]
    fn actions_body_is_one_pointer_with_steps() {
        let b = actions_body(&tap_steps(10.0, 20.0, 80));
        let a = &b["actions"][0];
        assert_eq!(a["parameters"]["pointerType"], "touch");
        let types: Vec<_> = a["actions"].as_array().unwrap().iter().map(|s| s["type"].as_str().unwrap()).collect();
        assert_eq!(types, ["pointerMove", "pointerDown", "pause", "pointerUp"]);
        assert_eq!(a["actions"][2]["duration"], 80);
        let s = swipe_steps((1.0, 2.0), (3.0, 4.0), 300);
        assert_eq!(s[3], PointerStep::Move { x: 3.0, y: 4.0, ms: 300 });
    }

    #[tokio::test]
    async fn locked_checks_tree_until_unlocked_once_then_trusts_wda() {
        let lock_xml = serde_json::to_string(include_str!("../tests/fixtures/lockscreen-ios27.xml")).unwrap();
        let home_xml = serde_json::to_string(include_str!("../tests/fixtures/home.xml")).unwrap();
        let sb = r#"{"value":{"bundleId":"com.apple.springboard"}}"#;
        let no = r#"{"value":false}"#;
        let (lock_src, home_src) = (format!(r#"{{"value":{lock_xml}}}"#), format!(r#"{{"value":{home_xml}}}"#));
        let m = Mock::with(&[
            // 1. WDA 在锁屏时启动：/wda/locked 误报 false，元素树是锁屏
            (200, no),
            (200, sb),
            (200, &lock_src),
            // 2. 人工解锁后：主屏幕，确认一次
            (200, no),
            (200, sb),
            (200, &home_src),
            // 3. 之后 WDA 说没锁就直接信
            (200, no),
            // 4. WDA 说锁了：核对一次，确实是锁屏
            (200, r#"{"value":true}"#),
            (200, sb),
            (200, &lock_src),
        ]);
        let mut c = WdaClient::new(&m);
        assert!(c.locked().await.unwrap());
        assert!(!c.locked().await.unwrap());
        assert!(!c.locked().await.unwrap());
        assert!(c.locked().await.unwrap());
        let paths: Vec<String> = m.seen.lock().unwrap().iter().map(|(_, p, _)| p.clone()).collect();
        assert_eq!(paths.iter().filter(|p| p.starts_with("/source")).count(), 3, "{paths:?}");
    }

    #[tokio::test]
    async fn notification_center_is_not_locked_even_if_wda_says_so() {
        let nc = serde_json::to_string(include_str!("../tests/fixtures/notification-center-ios27.xml")).unwrap();
        let pin = serde_json::to_string(include_str!("../tests/fixtures/passcode-view-ios27.xml")).unwrap();
        let sb = r#"{"value":{"bundleId":"com.apple.springboard"}}"#;
        let yes = r#"{"value":true}"#;
        let (nc, pin) = (format!(r#"{{"value":{nc}}}"#), format!(r#"{{"value":{pin}}}"#));
        let m = Mock::with(&[(200, yes), (200, sb), (200, &nc), (200, yes), (200, sb), (200, &pin), (200, yes)]);
        let mut c = WdaClient::new(&m);
        assert!(!c.locked().await.unwrap(), "通知中心");
        assert!(c.locked().await.unwrap(), "密码键盘界面");
        // 核对失败时按 WDA 的结果
        assert!(c.locked().await.unwrap());
    }

    #[tokio::test]
    async fn locked_falls_back_to_wda_when_tree_check_fails() {
        // 核对失败（例如取元素树超时）不能把可用的手机误判成锁屏，也不能就此信任 WDA
        let m = Mock::with(&[(200, r#"{"value":false}"#)]);
        let mut c = WdaClient::new(&m);
        assert!(!c.locked().await.unwrap());
        assert!(!c.lock_state_trusted);
    }
}
