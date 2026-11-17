//! 守护进程控制接口：Unix socket（0600，默认位置见 [`default_socket`]），每行一个 JSON。
//! MCP 进程和 CLI 都通过它访问设备；本机不开 TCP 端口。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub mod tools;
pub use tools::{ArgPolicy, ToolArgError, tool_to_request};

/// 以 root 运行（容器、系统服务）时的控制接口和数据目录
pub const SYSTEM_SOCKET: &str = "/run/iphone-control/ctl.sock";
pub const SYSTEM_DATA_DIR: &str = "/var/lib/iphone-control";

pub fn running_as_root() -> bool {
    // SAFETY: geteuid 没有前置条件，也不会失败
    unsafe { libc::geteuid() == 0 }
}

/// 默认数据目录：root 用 [`SYSTEM_DATA_DIR`]；普通用户（从源码运行）用 `$XDG_DATA_HOME/iphone-control`，
/// 即 `~/.local/share/iphone-control`
pub fn default_data_dir() -> PathBuf {
    if running_as_root() {
        return SYSTEM_DATA_DIR.into();
    }
    let xdg = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()).map(PathBuf::from);
    match xdg
        .or_else(|| std::env::var_os("HOME").filter(|v| !v.is_empty()).map(|h| PathBuf::from(h).join(".local/share")))
    {
        Some(base) => base.join("iphone-control"),
        None => SYSTEM_DATA_DIR.into(),
    }
}

/// 默认控制接口：root 用 [`SYSTEM_SOCKET`]；普通用户用 `$XDG_RUNTIME_DIR/iphone-control/ctl.sock`，
/// 没有这个变量时放进默认数据目录。守护进程、MCP、CLI 用同一个函数算，不用另外配置就能找到彼此
pub fn default_socket() -> PathBuf {
    if running_as_root() {
        return SYSTEM_SOCKET.into();
    }
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        Some(run) => PathBuf::from(run).join("iphone-control/ctl.sock"),
        None => default_data_dir().join("ctl.sock"),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// MCP 的 iphone_status：与 toolkit 相同的文字
    Status,
    /// 区域单位为点；None 表示整屏
    Screenshot {
        region: Option<(f64, f64, f64, f64)>,
    },
    /// One device-locked observation: unlock once, reuse image orientation for its element tree.
    Observe {
        limit: usize,
        snapshot_timeout_ms: u64,
    },
    ListElements {
        filter: String,
        limit: usize,
        /// 守护进程缓存的元素树不超过这么旧就直接用，不再读（0 = 总是重读）。
        /// 用于「等界面稳定」之后紧接着列元素：Settle 刚读过
        #[serde(default)]
        max_age_ms: u64,
        /// Optional budget for the read-only accessibility snapshot, after unlock checks.
        /// Zero preserves the normal WDA timeout for existing clients.
        #[serde(default)]
        snapshot_timeout_ms: u64,
    },
    Tap {
        x: f64,
        y: f64,
    },
    TapElement {
        text: String,
        index: usize,
        /// 同 ListElements。调用方声明这一串点击不改变界面布局（例如在同一个键盘上连按）时才给
        #[serde(default)]
        max_age_ms: u64,
    },
    LongPress {
        x: f64,
        y: f64,
        seconds: f64,
    },
    Swipe {
        from: (f64, f64),
        to: (f64, f64),
        seconds: f64,
    },
    TypeText {
        text: String,
        submit: bool,
    },
    PressButton {
        button: String,
    },
    ListApps {
        include_system: bool,
    },
    LaunchApp {
        app: String,
    },
    TerminateApp {
        app: String,
    },
    OpenUrl {
        url: String,
    },
    /// 等界面稳定：连续两次读到的元素一样，最多等 `max_ms`。操作后顺带返回界面时先调它
    Settle {
        max_ms: u64,
    },
    /// 查屏幕上有没有挡着的系统弹窗：有就返回提醒文字，没有返回空文字。不带 then 的操作做完后调
    AlertCheck,
    /// 等包含 `text` 的元素出现，最多等 `timeout_ms`（代替 agent 反复截图轮询）
    WaitFor {
        text: String,
        timeout_ms: u64,
    },
    /// 运维：守护进程和设备的状态快照（JSON 文本）
    Daemon,
    /// 运维：逐层诊断，返回多行文字
    Doctor,
    Resign {
        force: bool,
    },
    UnlockReset,
    /// 列出 USB 上接着的全部设备，标出正在控制哪台
    ListDevices,
    /// 改为控制另一台设备：`device` 是 UDID、设备名或 `auto`（取消选定，改回自动选）
    SelectDevice {
        device: String,
    },
    /// 组合请求：先做 `action`（一个操作类请求，可以没有），再按 `then` 等界面稳定、返回操作后的界面，
    /// 守护进程在同一次持锁里做完（原来要 3～4 次调用，每次重新连接、重新排队等锁）。
    /// `action` 出错就只回这个错误，不再看界面。旧版本守护进程不认识它，回「请求格式错误」，什么都没做
    Then {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        action: Option<Box<Request>>,
        then: ThenView,
    },
}

/// [`Request::Then`] 做完操作后返回什么
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "view", rename_all = "snake_case")]
pub enum ThenView {
    /// 不返回界面；`alert_check` 时（操作的结果里没报过弹窗才）查一下有没有系统弹窗挡着
    None {
        #[serde(default)]
        alert_check: bool,
    },
    /// 等界面稳定（最多 `settle_ms`），再列元素（同 [`Request::ListElements`]，复用不超过 `max_age_ms` 的元素树）
    Elements { filter: String, limit: usize, settle_ms: u64, max_age_ms: u64 },
    /// 等界面稳定（最多 `settle_ms`），再截图（同 [`Request::Screenshot`]）
    Screenshot { region: Option<(f64, f64, f64, f64)>, settle_ms: u64 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Text {
        text: String,
        /// What the text reports, as fields, so clients don't have to read the text (which depends on the
        /// language). Daemons before this field existed don't send it; their text is always Chinese
        #[serde(default, skip_serializing_if = "Option::is_none")]
        outcome: Option<Outcome>,
    },
    /// JPEG base64 + 说明文字（尺寸、放大倍数、坐标换算式）
    Image {
        jpeg_base64: String,
        caption: String,
        #[serde(default, rename = "coordinateMapping", skip_serializing_if = "Option::is_none")]
        coordinate_mapping: Option<CoordinateMapping>,
    },
    Error {
        message: String,
    },
    /// [`Request::Then`] 带截图的结果：操作和等待的说明文字，加截图（同 [`Response::Image`]）
    TextAndImage {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        outcome: Option<Outcome>,
        jpeg_base64: String,
        caption: String,
        #[serde(default, rename = "coordinateMapping", skip_serializing_if = "Option::is_none")]
        coordinate_mapping: Option<CoordinateMapping>,
    },
}

/// Maps screenshot raster coordinates to device points on each axis.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoordinateMapping {
    pub offset_x: f64,
    pub offset_y: f64,
    pub scale_x: f64,
    pub scale_y: f64,
}

/// Facts about a [`Response::Text`] that clients act on (see [`tapped_nothing`], [`alert_reported`])
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    /// `iphone_tap_element` tapped nothing: no match, index out of range, or the element is covered
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tapped_nothing: bool,
    /// The text already reports a system alert blocking the screen
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub alert_reported: bool,
}

impl Response {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into(), outcome: None }
    }
    /// Text with its [`Outcome`] (the daemon answers every text request this way)
    pub fn text_with(s: impl Into<String>, outcome: Outcome) -> Self {
        Self::Text { text: s.into(), outcome: Some(outcome) }
    }
    pub fn error(s: impl Into<String>) -> Self {
        Self::Error { message: s.into() }
    }
}

impl Request {
    /// 请求的 `op`（序列化后的标签），不用为了它把整个请求（含输入的文字）序列化一遍
    pub fn op(&self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Screenshot { .. } => "screenshot",
            Self::Observe { .. } => "observe",
            Self::ListElements { .. } => "list_elements",
            Self::Tap { .. } => "tap",
            Self::TapElement { .. } => "tap_element",
            Self::LongPress { .. } => "long_press",
            Self::Swipe { .. } => "swipe",
            Self::TypeText { .. } => "type_text",
            Self::PressButton { .. } => "press_button",
            Self::ListApps { .. } => "list_apps",
            Self::LaunchApp { .. } => "launch_app",
            Self::TerminateApp { .. } => "terminate_app",
            Self::OpenUrl { .. } => "open_url",
            Self::Settle { .. } => "settle",
            Self::AlertCheck => "alert_check",
            Self::WaitFor { .. } => "wait_for",
            Self::Daemon => "daemon",
            Self::Doctor => "doctor",
            Self::Resign { .. } => "resign",
            Self::UnlockReset => "unlock_reset",
            Self::ListDevices => "list_devices",
            Self::SelectDevice { .. } => "select_device",
            Self::Then { .. } => "then",
        }
    }

    /// Commands that can change the device, including reads that automatically unlock.
    pub fn requires_control(&self) -> bool {
        self.is_device_action()
            || matches!(
                self,
                Self::Screenshot { .. }
                    | Self::Observe { .. }
                    | Self::ListElements { .. }
                    | Self::WaitFor { .. }
                    | Self::UnlockReset
                    | Self::Resign { .. }
                    | Self::Then { .. }
            )
    }

    /// 会改变手机状态的操作。守护进程忙时排队太久就放弃，免得点击落到已经变了的屏幕上
    pub fn is_device_action(&self) -> bool {
        if let Self::Then { action, .. } = self {
            return action.as_ref().is_some_and(|a| a.is_device_action());
        }
        matches!(
            self,
            Self::Tap { .. }
                | Self::TapElement { .. }
                | Self::LongPress { .. }
                | Self::Swipe { .. }
                | Self::TypeText { .. }
                | Self::PressButton { .. }
                | Self::LaunchApp { .. }
                | Self::TerminateApp { .. }
                | Self::OpenUrl { .. }
                | Self::SelectDevice { .. }
        )
    }
}

/// 工具结果里报告系统弹窗挡着屏幕的固定写法（后面接「弹窗文字」）。MCP 端据此判断结果里是否已经报过弹窗，
/// 不能只找「系统弹窗」几个字：按名字点到的元素文字里也可能有
pub const ALERT_BLOCKING: &str = "屏幕上有系统弹窗";

/// `iphone_tap_element` 没点到时结果开头的几种写法（不算出错，单独调用时是普通结果；批量里要停下）
pub const TAP_MISSED: [&str; 3] = ["没找到", "只匹配到", "被挡住"];

/// Did `iphone_tap_element` tap nothing? Daemons that send [`Outcome`] say so there; older ones only write
/// Chinese text, recognised by its [`TAP_MISSED`] prefix
pub fn tapped_nothing(text: &str, outcome: Option<Outcome>) -> bool {
    match outcome {
        Some(o) => o.tapped_nothing,
        None => TAP_MISSED.iter().any(|p| text.starts_with(p)),
    }
}

/// Does the result already report a blocking system alert? As [`tapped_nothing`], with [`ALERT_BLOCKING`]
pub fn alert_reported(text: &str, outcome: Option<Outcome>) -> bool {
    match outcome {
        Some(o) => o.alert_reported,
        None => text.contains(ALERT_BLOCKING),
    }
}

/// 请求里带上这个键（值为 true）：客户端发完请求不半关闭连接，守护进程据此把「读到 EOF」当成调用方已经走了，
/// 还在排队等设备的请求就不再执行。旧版本的客户端发完就半关闭、不带这个键，守护进程对它们照旧处理完。
/// 放在请求对象里而不是改协议格式：旧版本守护进程会忽略这个多出来的键
pub const CANCEL_ON_CLOSE: &str = "cancel_on_close";

/// 请求里带上这个键（值为 UDID）：守护进程正在控制的不是这台就不执行。`iphone_batch` 用它防止中途换了设备
/// （自动选设备时拔掉正在控制的那台，会改控另一台）后，剩下的步骤落到另一台上
pub const EXPECT_UDID: &str = "expect_udid";
/// Optional numeric correlation ID for gateway and daemon diagnostics.
pub const DIAGNOSTIC_ID: &str = "diagnostic_id";

/// Internal gateway/native deadline; same-host CLOCK_MONOTONIC nanoseconds.
pub const ACTION_DEADLINE: &str = "action_deadline_monotonic_ns";
pub const ACTION_LIFETIME: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionDeadline(u64);
impl ActionDeadline {
    pub fn admit() -> std::io::Result<Self> {
        Self::admit_at(monotonic_ns()?)
    }
    pub fn admit_at(now_ns: u64) -> std::io::Result<Self> {
        now_ns
            .checked_add(ACTION_LIFETIME.as_nanos() as u64)
            .map(Self)
            .ok_or_else(|| std::io::Error::other("Action deadline overflow"))
    }
    pub fn parse(value: serde_json::Value) -> std::io::Result<Self> {
        Self::parse_at(value, monotonic_ns()?)
    }
    pub fn parse_at(value: serde_json::Value, now_ns: u64) -> std::io::Result<Self> {
        let ns = value.as_u64().ok_or_else(|| std::io::Error::other("Invalid action deadline metadata"))?;
        let latest = Self::admit_at(now_ns)?;
        if ns > latest.0 {
            return Err(std::io::Error::other("Action deadline exceeds local maximum"));
        }
        Ok(Self(ns))
    }
    pub fn wire_ns(self) -> u64 {
        self.0
    }
    pub fn expired(self) -> std::io::Result<bool> {
        Ok(self.expired_at(monotonic_ns()?))
    }
    pub fn expired_at(self, now_ns: u64) -> bool {
        now_ns >= self.0
    }
}

pub fn monotonic_ns() -> std::io::Result<u64> {
    let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: clock_gettime writes an initialized timespec on success.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, time.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the successful clock_gettime call initialized time.
    let time = unsafe { time.assume_init() };
    u64::try_from(time.tv_sec)
        .ok()
        .and_then(|s| s.checked_mul(1_000_000_000))
        .and_then(|ns| u64::try_from(time.tv_nsec).ok().filter(|n| *n < 1_000_000_000).and_then(|n| ns.checked_add(n)))
        .ok_or_else(|| std::io::Error::other("Invalid monotonic clock"))
}

/// Request key carrying the client's language (`zh` / `en`). The daemon answers in it; requests without it
/// (older clients) get Chinese, as before. Older daemons ignore the extra key
pub const LANG: &str = "lang";

/// 发一个请求、读一个响应（客户端用）。`timeout` 为 None 时一直等（例如 `sign` 要等签名装完）。
/// 返回之前被丢掉（超时、调用方取消）时连接随之关闭，守护进程会放弃还在排队的这个请求
pub async fn call(
    socket: &std::path::Path,
    req: &Request,
    timeout: Option<std::time::Duration>,
) -> std::io::Result<Response> {
    call_expecting(socket, req, timeout, None).await
}

/// 同 [`call`]；`expect_udid` 见 [`EXPECT_UDID`]
pub async fn call_expecting(
    socket: &std::path::Path,
    req: &Request,
    timeout: Option<std::time::Duration>,
    expect_udid: Option<&str>,
) -> std::io::Result<Response> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let exchange = async {
        let stream = tokio::net::UnixStream::connect(socket).await?;
        let (r, mut w) = stream.into_split();
        let mut v = serde_json::to_value(req)?;
        if let Some(o) = v.as_object_mut() {
            o.insert(CANCEL_ON_CLOSE.into(), true.into());
            o.insert(LANG.into(), ictl_l10n::lang().code().into());
            if let Some(u) = expect_udid {
                o.insert(EXPECT_UDID.into(), u.into());
            }
        }
        let mut line = v.to_string();
        line.push('\n');
        w.write_all(line.as_bytes()).await?;
        let mut resp = String::new();
        BufReader::new(r).read_line(&mut resp).await?;
        serde_json::from_str(&resp).map_err(std::io::Error::other)
    };
    match timeout {
        None => exchange.await,
        Some(t) => tokio::time::timeout(t, exchange).await.map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                ictl_l10n::trf!("守护进程 {} 秒内没有应答（可能正忙，例如正在启动 WDA 或重签）", "The daemon didn't answer within {} seconds (it may be busy, for example starting WDA or re-signing)", t.as_secs()),
            )
        })?,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_deadline_rejects_malformed_future_and_expired_metadata_without_renewing() {
        let now = 100_000_000_000;
        for value in [
            serde_json::json!(null),
            serde_json::json!("130000000000"),
            serde_json::json!(-1),
            serde_json::json!(1.2),
            serde_json::json!(true),
            serde_json::json!(u64::MAX),
        ] {
            assert!(ActionDeadline::parse_at(value.clone(), now).is_err(), "accepted {value}");
        }
        let deadline = ActionDeadline::admit_at(now).unwrap();
        assert!(!deadline.expired_at(now + 29_999_999_999));
        assert!(deadline.expired_at(now + 30_000_000_000));
        let forwarded = ActionDeadline::parse_at(serde_json::json!(deadline.wire_ns()), now + 20_000_000_000).unwrap();
        assert_eq!(forwarded, deadline);
        assert!(forwarded.expired_at(now + 30_000_000_001));
        assert!(ActionDeadline::admit_at(u64::MAX).is_err());
    }

    #[test]
    fn control_classification_includes_automatic_unlock_reads() {
        for req in [
            Request::Screenshot { region: None },
            Request::Observe { limit: 0, snapshot_timeout_ms: 0 },
            Request::ListElements { filter: String::new(), limit: 0, max_age_ms: 0, snapshot_timeout_ms: 0 },
            Request::WaitFor { text: String::new(), timeout_ms: 0 },
            Request::UnlockReset,
            Request::Resign { force: false },
        ] {
            assert!(req.requires_control(), "{req:?} can change device state");
        }
        for req in [Request::Daemon, Request::Status, Request::ListApps { include_system: false }, Request::ListDevices]
        {
            assert!(!req.requires_control());
        }
    }

    #[test]
    fn op_matches_the_serialized_tag() {
        let then = ThenView::Elements { filter: String::new(), limit: 1, settle_ms: 0, max_age_ms: 0 };
        for req in [
            Request::Status,
            Request::Screenshot { region: None },
            Request::Observe { limit: 0, snapshot_timeout_ms: 0 },
            Request::ListElements { filter: String::new(), limit: 0, max_age_ms: 0, snapshot_timeout_ms: 0 },
            Request::Tap { x: 0.0, y: 0.0 },
            Request::TapElement { text: String::new(), index: 0, max_age_ms: 0 },
            Request::LongPress { x: 0.0, y: 0.0, seconds: 0.0 },
            Request::Swipe { from: (0.0, 0.0), to: (0.0, 0.0), seconds: 0.0 },
            Request::TypeText { text: "secret".into(), submit: false },
            Request::PressButton { button: String::new() },
            Request::ListApps { include_system: false },
            Request::LaunchApp { app: String::new() },
            Request::TerminateApp { app: String::new() },
            Request::OpenUrl { url: String::new() },
            Request::Settle { max_ms: 0 },
            Request::AlertCheck,
            Request::WaitFor { text: String::new(), timeout_ms: 0 },
            Request::Daemon,
            Request::Doctor,
            Request::Resign { force: false },
            Request::UnlockReset,
            Request::ListDevices,
            Request::SelectDevice { device: String::new() },
            Request::Then { action: Some(Box::new(Request::Tap { x: 0.0, y: 0.0 })), then },
        ] {
            assert_eq!(serde_json::to_value(&req).unwrap()["op"], req.op(), "{req:?}");
        }
    }

    /// 组合请求的线上格式；没有 action 时不带这个键；操作类与否看里面的 action
    #[test]
    fn then_requests_round_trip_and_classify_by_their_action() {
        let req = Request::Then {
            action: Some(Box::new(Request::Tap { x: 1.0, y: 2.0 })),
            then: ThenView::Screenshot { region: None, settle_ms: 1500 },
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains(r#""action":{"op":"tap""#) && s.contains(r#""view":"screenshot""#), "{s}");
        assert_eq!(serde_json::from_str::<Request>(&s).unwrap(), req);
        assert!(req.is_device_action() && req.requires_control());
        let bare = Request::Then { action: None, then: ThenView::None { alert_check: true } };
        let s = serde_json::to_string(&bare).unwrap();
        assert!(!s.contains("action"), "{s}");
        assert_eq!(serde_json::from_str::<Request>(&s).unwrap(), bare);
        assert!(!bare.is_device_action() && bare.requires_control());
        let both = Response::TextAndImage {
            text: "t".into(),
            outcome: None,
            jpeg_base64: "AAAA".into(),
            caption: "c".into(),
            coordinate_mapping: None,
        };
        let s = serde_json::to_string(&both).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&s).unwrap(), both);
    }

    #[test]
    fn requests_round_trip_as_tagged_json() {
        let r = Request::Swipe { from: (1.0, 2.0), to: (3.0, 4.0), seconds: 0.3 };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.starts_with(r#"{"op":"swipe""#), "{s}");
        assert_eq!(serde_json::from_str::<Request>(&s).unwrap(), r);
        let e: Response = serde_json::from_str(r#"{"kind":"error","message":"x"}"#).unwrap();
        assert_eq!(e, Response::error("x"));
    }

    #[test]
    fn screenshot_mapping_round_trips_and_older_images_remain_readable() {
        let wire = serde_json::json!({
            "kind": "image", "jpeg_base64": "AAAA", "caption": "crop",
            "coordinateMapping": {"offsetX": 1.0, "offsetY": 2.0, "scaleX": 0.25, "scaleY": 0.5}
        });
        let image: Response = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(image).unwrap(), wire);
        let old = serde_json::json!({"kind":"image", "jpeg_base64":"AAAA", "caption":"full"});
        let image: Response = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(serde_json::to_value(image).unwrap(), old);
    }

    #[test]
    fn old_element_requests_keep_the_default_snapshot_budget() {
        let old: Request = serde_json::from_str(r#"{"op":"list_elements","filter":"","limit":80}"#).unwrap();
        assert!(matches!(old, Request::ListElements { snapshot_timeout_ms: 0, .. }));
        let bounded: Request =
            serde_json::from_str(r#"{"op":"list_elements","filter":"","limit":80,"snapshot_timeout_ms":1000}"#)
                .unwrap();
        assert!(matches!(bounded, Request::ListElements { snapshot_timeout_ms: 1000, .. }));
    }

    /// Outcome travels as fields; flags that are off and a missing outcome (older daemons) stay off the wire,
    /// and older clients that don't know the field ignore it
    #[test]
    fn outcome_is_optional_on_the_wire() {
        let s = serde_json::to_string(&Response::text("x")).unwrap();
        assert_eq!(s, r#"{"kind":"text","text":"x"}"#);
        let s = serde_json::to_string(&Response::text_with("x", Outcome::default())).unwrap();
        assert_eq!(s, r#"{"kind":"text","text":"x","outcome":{}}"#);
        let missed = Response::text_with("Not found", Outcome { tapped_nothing: true, ..Default::default() });
        let s = serde_json::to_string(&missed).unwrap();
        assert_eq!(serde_json::from_str::<Response>(&s).unwrap(), missed);

        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum OldResponse {
            Text { text: String },
        }
        let OldResponse::Text { text } = serde_json::from_str(&s).unwrap();
        assert_eq!(text, "Not found");
    }

    /// The field decides; without it (older daemon) the Chinese text is read
    #[test]
    fn outcome_wins_over_text() {
        let missed = Outcome { tapped_nothing: true, ..Default::default() };
        assert!(tapped_nothing("Not found: no element contains \"x\"", Some(missed)));
        assert!(!tapped_nothing("没找到包含 \"x\" 的元素", Some(Outcome::default())));
        assert!(tapped_nothing("没找到包含 \"x\" 的元素", None));
        assert!(tapped_nothing("被挡住:Button \"x\" 整个被「浮层」盖住,没有点", None));
        assert!(!tapped_nothing("Not found: no element contains \"x\"", None));
        let alert = Outcome { alert_reported: true, ..Default::default() };
        assert!(alert_reported("Launched x, but a system alert is on screen: “Allow?”", Some(alert)));
        assert!(alert_reported("已启动 x,但屏幕上有系统弹窗「允许?」挡着", None));
        assert!(!alert_reported("Launched x", Some(Outcome::default())));
    }

    /// 旧版本守护进程直接反序列化请求：多出来的 cancel_on_close、lang 键不能让它报错（含没有字段的请求）
    #[test]
    fn extra_cancel_key_is_ignored_when_parsing() {
        for s in [
            r#"{"op":"status","cancel_on_close":true}"#,
            r#"{"op":"tap","x":1.0,"y":2.0,"cancel_on_close":true}"#,
            r#"{"op":"status","cancel_on_close":true,"lang":"en"}"#,
        ] {
            serde_json::from_str::<Request>(s).unwrap_or_else(|e| panic!("{s}: {e}"));
        }
    }
}
