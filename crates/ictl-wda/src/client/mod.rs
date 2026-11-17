//! WDA HTTP 客户端。会话失效（WDA 重启后）自动重建一次。

use std::time::Duration;

use base64::Engine;
use ictl_device::ScreenSize;
use serde_json::{Value, json};

use crate::http::Transport;
use crate::timing::StepTimer;
use crate::{Result, WdaError, elements};
use std::sync::atomic::{AtomicU64, Ordering};

mod capture;
mod gestures;
mod launch;
mod lock;
mod session;

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
        let lock_xml = serde_json::to_string(include_str!("../../tests/fixtures/lockscreen-ios27.xml")).unwrap();
        let home_xml = serde_json::to_string(include_str!("../../tests/fixtures/home.xml")).unwrap();
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
        let nc = serde_json::to_string(include_str!("../../tests/fixtures/notification-center-ios27.xml")).unwrap();
        let pin = serde_json::to_string(include_str!("../../tests/fixtures/passcode-view-ios27.xml")).unwrap();
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
