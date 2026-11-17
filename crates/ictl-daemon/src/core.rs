//! 守护进程核心：唯一持有设备的进程。共享状态、请求分发、快照。
//!
//! 并发模型（全部跑在主循环的 LocalSet 里，单线程协作）：
//! - 设备相关的状态在 [`Inner`] 里，一把锁串行访问：HID 会话、WDA 会话只能有一个持有者，
//!   也不会出现 toolkit 那种「长请求期间被巡检误判 WDA 挂了」的问题
//! - 每个控制请求一个任务，拿到锁才执行；巡检（[`crate::supervise`]）拿不到锁就跳过这一轮
//! - 巡检持锁只做改状态的部分：列设备、WDA 的 `/status` 探测（走单独的连接）在锁外做，回来时按
//!   [`Inner::wda_generation`] 核对 WDA 客户端没换过才用结果；从不持锁睡眠
//! - 请求里碰到 WDA 连不上，之后一小段时间（[`WDA_UNREACHABLE_HOLD`]）不再碰它，直接走 HID 后备，
//!   免得一次请求里几个 WDA 调用各等一次建连接超时，加起来超过 [`STALE`]
//! - 耗时的事不持锁：启动 WDA（含挂开发者镜像，最长约 2 分钟）、重签、会话检查在后台跑，做完再持锁写结果
//! - `status` 读发布出来的快照（watch 通道），不碰锁
//! - 改变手机状态的请求排队超过 [`STALE`] 就放弃，免得点击落到已经变了的屏幕上

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ictl_device::{AppInfo, Device, DeviceError, DeviceInfo, Rotation, ScreenSize, Tunnel, UsbDevice};
use ictl_unlock::UnlockConfig;
use ictl_wda::timing::StepTimer;
use ictl_wda::{WdaClient, WdaRunner, http::DeviceTransport, http::Transport};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_OPERATION: AtomicU64 = AtomicU64::new(1);
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::diagnostics::{Diagnostics, HealthDiagnostics};
use crate::eventlog::EventLog;
use crate::machine::{Backoff, DeviceState, LinkDown, Misses, MuxWatch};
use crate::notify::{self, Dedup, Event};
use crate::protocol::{Request, Response};
use crate::supervise::StartOutcome;
use ictl_wda::diagnostics::StopReason;

pub const TICK: Duration = Duration::from_secs(10);
/// 改变手机状态的请求排队超过这么久就放弃
pub const STALE: Duration = ictl_protocol::ACTION_LIFETIME;
/// 请求里碰到 WDA 连不上之后多久内不再碰它（见 [`Inner::wda_unreachable_until`]）
pub const WDA_UNREACHABLE_HOLD: Duration = Duration::from_secs(5);
/// 长按、滑动的时长上限（秒）：HID 拖动按时长换算采样点，不设上限一个请求就能占住设备很久
pub const MAX_GESTURE_SECONDS: f64 = 10.0;
/// 等界面稳定、等元素出现的上限（毫秒）：持着设备锁等，不能太久
pub const MAX_WAIT_MS: u64 = 30_000;

/// 守护进程持有的 WDA 客户端用的传输：正式是经 usbmuxd 直连设备的 [`DeviceTransport`]，
/// 单测里换成 [`ictl_wda::fake::ScriptedWda`]，不用真机就能测工具层的规则（例如什么时候能退到 HID）
#[derive(Clone)]
pub(crate) enum WdaLink {
    Device(DeviceTransport),
    #[cfg(test)]
    Fake(ictl_wda::fake::ScriptedWda),
}

impl Transport for WdaLink {
    async fn screenshot_jpeg(&self) -> std::io::Result<Option<Vec<u8>>> {
        match self {
            Self::Device(t) => t.screenshot_jpeg().await,
            #[cfg(test)]
            Self::Fake(t) => t.screenshot_jpeg().await,
        }
    }

    async fn health(&self) -> std::io::Result<(u16, Vec<u8>)> {
        match self {
            Self::Device(t) => t.health().await,
            #[cfg(test)]
            Self::Fake(t) => t.health().await,
        }
    }

    async fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
        timeout: Duration,
    ) -> std::io::Result<(u16, Vec<u8>)> {
        match self {
            Self::Device(t) => t.send(method, path, body, timeout).await,
            #[cfg(test)]
            Self::Fake(t) => t.send(method, path, body, timeout).await,
        }
    }
}

pub(crate) type Wda = WdaClient<WdaLink>;

/// 持久化到 `state.json` 的状态（不含秘密）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Persisted {
    pub team_id: Option<String>,
    pub wda_bundle: Option<String>,
    /// 描述文件到期时间（Unix 秒）
    pub profile_expires: Option<u64>,
    /// 设备竖放时的屏幕尺寸（WDA 不可用时 HID 换算坐标用；旧版本可能存的是横屏尺寸，用的时候都先转成竖放）
    pub screen: Option<ScreenSize>,
    /// `screen` 是哪台设备的；换了设备就不能拿来给 HID 换算坐标
    pub screen_udid: Option<String>,
    pub last_resign: Option<String>,
    /// 运行中选定要控制的设备（`iphone-control select`、`iphone_select_device`）；None 是自动选。
    /// 设了 `IPHONE_UDID` 时以它为准，这个不起作用
    pub selected_udid: Option<String>,
    /// Unix seconds of the last automatic re-sign triggered by iOS refusing to launch the
    /// installed WDA. Survives daemon restarts so each reconnect cannot re-sign again.
    pub launch_resign_at: Option<u64>,
}

impl Persisted {
    pub fn load(path: &Path) -> Self {
        let Ok(bytes) = std::fs::read(path) else { return Self::default() };
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            // 损坏时按空状态启动（会显示「WDA 还没签名」），但要留痕
            tracing::warn!("state.json 解析失败，按空状态启动：{e}");
            Self::default()
        })
    }

    pub(crate) fn save(&self, path: &Path) {
        let tmp = path.with_extension("json.tmp");
        let r = std::fs::write(&tmp, serde_json::to_vec_pretty(self).unwrap_or_default())
            .and_then(|_| std::fs::rename(&tmp, path));
        if let Err(e) = r {
            tracing::warn!("保存 state.json 失败：{e}");
        }
    }

    /// 描述文件还剩多久；不知道返回 None
    pub fn profile_left(&self) -> Option<Duration> {
        self.profile_expires.and_then(left_until)
    }
}

/// 离 Unix 时间 `at`（秒）还有多久，过了是 0
fn left_until(at: u64) -> Option<Duration> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(Duration::from_secs(at.saturating_sub(now)))
}

/// 这台手机上装着的 WDA 描述文件（经 misagent 读）。[`Persisted`] 只记最近一次签名：给别的手机签过之后，
/// 它的到期时间不是这台手机上那份的（这台装的还是更早签的），按它判断会该重签时不重签
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub enum DeviceProfile {
    /// 还没读（手机没接、刚接上还没配对）
    #[default]
    NotChecked,
    /// 读不出来：按签名记录算
    Unknown(String),
    /// 手机上没有当前 Apple ID 给 WDA 签的、包含这台手机的描述文件：没装过 WDA，或装的是别的账号签的
    Missing,
    /// 到期时间（Unix 秒）
    Expires(u64),
}

impl DeviceProfile {
    /// 还剩多久：读到了用手机上的；还没读、读不出来按签名记录；手机上没有返回 None
    pub fn left(&self, persisted: &Persisted) -> Option<Duration> {
        match self {
            Self::Expires(e) => left_until(*e),
            Self::Missing => None,
            Self::NotChecked | Self::Unknown(_) => persisted.profile_left(),
        }
    }
}

/// 从手机上的描述文件（原始内容）里挑 WDA 的：app-id 是「团队 ID.WDA Bundle ID」（不知道团队 ID 时只看 Bundle ID）、
/// 设备列表里有这台手机，取最晚到期的那份。装过几次会留下几份，旧的过期了也还在
pub fn wda_profile_expiry(profiles: &[Vec<u8>], team: Option<&str>, bundle: &str, udid: &str) -> Option<SystemTime> {
    profiles
        .iter()
        .filter_map(|p| ictl_sign::Profile::parse(p))
        .filter(|p| {
            p.application_identifier
                .split_once('.')
                .is_some_and(|(t, b)| b == bundle && team.is_none_or(|team| t == team))
                && p.devices.iter().any(|d| d.eq_ignore_ascii_case(udid))
        })
        .map(|p| p.expires)
        .max()
}

pub(crate) fn check_action_admission(
    queued_at: Instant,
    deadline: Option<ictl_protocol::ActionDeadline>,
    stage: &'static str,
) -> Result<(), String> {
    if queued_at.elapsed() >= STALE || deadline.is_some_and(|d| d.expired().unwrap_or(true)) {
        tracing::warn!(
            event = "action_rejected",
            category = "expired",
            stage,
            queue_age_ms = queued_at.elapsed().as_millis() as u64,
            budget_ms = STALE.as_millis() as u64
        );
        return Err(ictl_l10n::trf!(
            "操作已过期：从首次接收起总等待超过 {} 秒，未发送到手机；先截图看看再发起新操作",
            "Action expired: the total admission lifetime exceeds {} seconds; the request was dropped before dispatch. Take a screenshot before a new action",
            STALE.as_secs()
        ));
    }
    Ok(())
}

/// 一次请求的附带条件（来自请求里的 [`ictl_protocol::CANCEL_ON_CLOSE`]、[`ictl_protocol::EXPECT_UDID`]）
#[derive(Default)]
pub struct CallCtx {
    /// 调用方断开时收到信号。还在排队等设备锁的请求就不执行了；
    /// 已经开始执行的照常做完（中途打断会留下改了一半的 WDA 设置）
    pub gone: Option<tokio::sync::oneshot::Receiver<()>>,
    /// 正在控制的不是这台就不执行
    pub expect_udid: Option<String>,
    pub diagnostic_id: Option<u64>,
    pub action_deadline: Option<ictl_protocol::ActionDeadline>,
}

/// 快照：`iphone-control status` 显示
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub state: DeviceState,
    pub device: Option<DeviceInfo>,
    /// USB 上接着的全部设备（读不到名字的只有 UDID）
    #[serde(default)]
    pub devices: Vec<DeviceInfo>,
    /// 选定的设备：`IPHONE_UDID` 或运行中选的；None 是自动选
    #[serde(default)]
    pub pinned: Option<String>,
    #[serde(default)]
    pub pinned_by_env: bool,
    pub persisted: Persisted,
    /// 当前这台手机上的 WDA 描述文件
    #[serde(default)]
    pub device_profile: DeviceProfile,
    pub auto_unlock: bool,
    pub unlock_disabled: bool,
    pub notify_configured: bool,
    pub log_tail: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<HealthDiagnostics>,
}

pub(crate) struct Inner {
    pub persisted: Persisted,
    /// 当前这台手机上的 WDA 描述文件；换连接、离线时清成 NotChecked
    pub device_profile: DeviceProfile,
    pub state: DeviceState,
    pub device: Option<Device>,
    pub info: Option<DeviceInfo>,
    /// 最近一轮巡检时 USB 上接着的全部设备
    pub connected: Vec<UsbDevice>,
    /// 各设备的名字、型号（第一次见到时读一次）
    pub infos: HashMap<String, DeviceInfo>,
    /// 读不出名字、型号的设备（例如没信任）下次什么时候再读、下次再失败等多久（按 UDID 退避）
    pub info_retry: HashMap<String, (Instant, Duration)>,
    /// HID 和截图用的隧道（HID 会话归它所有）
    pub tunnel: Option<Tunnel>,
    /// WDA 最近一次报的界面旋转及读取时间。只在短暂掉线时给 HID 使用，不持久化；换设备时清掉
    pub wda_rotation: Option<(Instant, Rotation)>,
    pub runner: Option<WdaRunner>,
    pub wda: Option<Wda>,
    /// 每换一次 WDA 客户端（拉起、丢掉）加 1。巡检在锁外探测 `/status`，回来时代数变了就不用那个结果
    pub wda_generation: u64,
    /// 请求里刚碰到 WDA 连不上的时刻往后这段时间内，同一请求和紧接着的请求不再碰 WDA，直接走 HID 后备
    /// （每次连不上最长要等 10 秒建连接，几次叠起来就超过 [`STALE`]）。巡检确认 WDA 就绪时清掉
    pub wda_unreachable_until: Option<Instant>,
    /// 连续连不上 WDA 的起点（巡检的 `/status`），持续太久就重启 WDA（[`crate::machine::LINK_DOWN_RESTART`]）
    pub wda_link_down: LinkDown,
    /// 后台启动 WDA 的任务（含挂开发者镜像），巡检里收结果
    pub starting: Option<JoinHandle<StartOutcome>>,
    /// 开发者镜像连续挂载失败次数，到 3 次清缓存重新下载
    pub ddi_failures: u32,
    pub resigning: bool,
    pub wda_misses: Misses,
    /// Consecutive `/wda/locked` timeouts; WDA is restarted when they pile up
    pub wda_lock_timeouts: u32,
    pub backoff: Backoff,
    pub mux: MuxWatch,
    pub usbmuxd: Option<tokio::process::Child>,
    pub usbmuxd_started: Option<Instant>,
    /// usbmuxd 起来就退出时，下次重启不早于这个时间（退避）
    pub usbmuxd_retry_at: Option<Instant>,
    pub usbmuxd_delay: Duration,
    /// 连续列不出设备的次数（usbmuxd 抖一下不立刻当成离线）
    pub list_errors: u32,
    pub offline_since: Option<Instant>,
    /// 最近一次经本进程锁屏的时间（自动解锁要等锁屏过渡走完，见 [`crate::tools::LOCK_SETTLE`]）
    pub last_lock: Option<Instant>,
    /// 最近一次确认手机没锁屏的时间。[`crate::tools::UNLOCK_REUSE`] 内的下一个操作不再问 `/wda/locked`
    /// （每次是一个经浏览器转发的往返）；锁屏、换设备、检查出错都会清掉
    pub unlocked_at: Option<Instant>,
    /// 滑动后等到的、已经停稳的元素树。只给紧接着的一次观察用（[`crate::tools::SETTLED_TREE_REUSE`] 内），
    /// 省一次 `/source`；其它操作开始前就清掉
    pub settled_tree: Option<(Instant, ictl_wda::elements::Tree)>,
    /// HID 会话最近一次使用的时间；空闲超过 [`crate::supervise::HID_IDLE`] 就关掉（它持有屏幕媒体流）
    pub hid_used: Option<Instant>,
    /// 最近一次 HID 截图的像素宽高和时间。HID 触摸前核对横竖只要宽高，
    /// [`crate::tools::HID_FRAME_REUSE`] 内不再为了它截一整张 PNG
    pub hid_frame: Option<(Instant, (u32, u32))>,
    /// 最近一次读到的元素树。只在调用方允许（max_age_ms）时复用；改变界面的操作做完就清掉
    pub elements_cache: Option<(Instant, ictl_wda::elements::Tree)>,
    pub dedup: Dedup,
    pub next_resign: Instant,
    /// The next scheduled re-sign must sign and install even if the profile still looks valid.
    pub resign_force_pending: bool,
    pub next_session_check: Instant,
    pub apps: HashMap<bool, Vec<AppInfo>>,
}

pub struct Daemon {
    pub cfg: Config,
    pub log: EventLog,
    pub(crate) inner: Mutex<Inner>,
    snap: watch::Sender<Snapshot>,
    pub(crate) diagnostics: Arc<std::sync::Mutex<Diagnostics>>,
}

/// 停掉 HID 会话（屏幕媒体流），隧道保留。媒体流不停的话手机一直以为「配对的电脑」在看屏幕：
/// 灵动岛常驻图标、不会自动锁屏，本进程退出、拔插数据线都清不掉（iOS 27 真机）
pub(crate) async fn close_hid(g: &mut Inner) {
    if let Some(t) = g.tunnel.as_mut() {
        t.close_hid().await;
    }
}

/// 丢掉隧道。一律走这里：先停 HID 媒体流再断（见 [`close_hid`]）
pub(crate) async fn drop_tunnel(g: &mut Inner) {
    if let Some(t) = g.tunnel.take() {
        t.close().await;
    }
}

/// 一台设备的简短说明：`名字(型号 iOS 版本,UDID)`；读不到名字时只有 UDID
pub fn device_label(i: &DeviceInfo) -> String {
    if i.name.is_empty() && i.product_type.is_empty() {
        i.udid.clone()
    } else {
        format!("{}({} iOS {},{})", i.name, i.product_type, i.product_version, i.udid)
    }
}

/// WDA 不可用时的说明。HID 后备路线靠 iOS 27 起才有的 CoreDevice 屏幕服务，低版本不能说「照常用」
pub(crate) fn wda_down(state: &DeviceState, info: Option<&DeviceInfo>) -> String {
    // 没接设备时 WDA 不会自己起来，也没有 HID 后备
    if matches!(state, DeviceState::Absent) {
        return ictl_l10n::tr!(
            "WDA 不可用(手机未连接);接上设备后守护进程会自动拉起 WDA",
            "WDA unavailable (phone not connected); the daemon starts WDA once a device is plugged in"
        )
        .into();
    }
    let fallback = if hid_supported(info) {
        ictl_l10n::tr!("截图/点击/滑动/按键可照常用", "screenshots/taps/swipes/buttons still work")
    } else {
        ictl_l10n::tr!(
            "这台手机 iOS 低于 27,没有 HID 后备路线,WDA 恢复前截图、点击等操作都用不了",
            "this phone runs iOS below 27 and has no HID fallback, so screenshots, taps etc. don't work until WDA is back"
        )
    };
    // 状态还是「就绪」：WDA 在两次巡检之间刚掉线，下一轮巡检才会发现并重新拉起
    let why = if *state == DeviceState::Ready {
        ictl_l10n::tr!("刚掉线", "just dropped").to_owned()
    } else {
        state.describe()
    };
    ictl_l10n::trf!(
        "WDA 不可用({why}),守护进程一般 1~2 分钟内会拉起;{fallback}",
        "WDA unavailable ({why}), the daemon usually restarts it within 1-2 minutes; {fallback}"
    )
}

/// HID 后备路线（免 WDA 截图、点击）只有 iOS 27 起能用；不知道版本时照常尝试
pub(crate) fn hid_supported(info: Option<&DeviceInfo>) -> bool {
    info.is_none_or(|i| matches!(i.major_version(), 0 | 27..))
}

impl Daemon {
    pub fn new(cfg: Config) -> Arc<Self> {
        let log = EventLog::new(&cfg.logs());
        let persisted = Persisted::load(&cfg.state_file());
        let now = Instant::now();
        let inner = Inner {
            persisted,
            device_profile: DeviceProfile::NotChecked,
            state: DeviceState::Absent,
            device: None,
            info: None,
            connected: Vec::new(),
            infos: HashMap::new(),
            info_retry: HashMap::new(),
            tunnel: None,
            wda_rotation: None,
            runner: None,
            wda: None,
            wda_generation: 0,
            wda_unreachable_until: None,
            wda_link_down: LinkDown::default(),
            starting: None,
            ddi_failures: 0,
            resigning: false,
            wda_misses: Misses::default(),
            wda_lock_timeouts: 0,
            backoff: Backoff::default(),
            mux: MuxWatch::default(),
            usbmuxd: None,
            usbmuxd_started: None,
            usbmuxd_retry_at: None,
            usbmuxd_delay: Duration::ZERO,
            list_errors: 0,
            offline_since: Some(now),
            last_lock: None,
            unlocked_at: None,
            settled_tree: None,
            hid_used: None,
            hid_frame: None,
            elements_cache: None,
            dedup: Dedup::default(),
            // 启动一分钟后再做第一次重签检查和会话验证，先让设备就绪
            next_resign: now + Duration::from_secs(60),
            resign_force_pending: false,
            next_session_check: now + Duration::from_secs(90),
            apps: HashMap::new(),
        };
        let first = Snapshot {
            state: DeviceState::Absent,
            device: None,
            devices: Vec::new(),
            pinned: cfg.udid.clone().or_else(|| inner.persisted.selected_udid.clone()),
            pinned_by_env: cfg.udid.is_some(),
            persisted: inner.persisted.clone(),
            device_profile: DeviceProfile::NotChecked,
            auto_unlock: false,
            unlock_disabled: cfg.unlock_disabled_flag().exists(),
            notify_configured: cfg.notify.any(),
            log_tail: Vec::new(),
            diagnostics: None,
        };
        let (snap, _) = watch::channel(first);
        Arc::new(Self {
            cfg,
            log,
            inner: Mutex::new(inner),
            snap,
            diagnostics: Arc::new(std::sync::Mutex::new(Diagnostics::default())),
        })
    }

    /// 自动解锁配置：用设备 `udid` 自己的密码。没接设备时密码文件不存在（自动解锁不可用）
    pub(crate) fn unlock_cfg(&self, udid: Option<&str>) -> UnlockConfig {
        UnlockConfig {
            passcode_file: self.cfg.passcode_file_for(udid.unwrap_or("_none")),
            disabled_flag: self.cfg.unlock_disabled_flag(),
            timing: Default::default(),
        }
    }

    /// 选定的设备：`IPHONE_UDID` 优先，其次是运行中选的
    pub(crate) fn pinned(&self, g: &Inner) -> Option<String> {
        self.cfg.udid.clone().or_else(|| g.persisted.selected_udid.clone())
    }

    /// 接着的全部设备，读不到名字的只有 UDID
    pub(crate) fn connected_infos(&self, g: &Inner) -> Vec<DeviceInfo> {
        g.connected
            .iter()
            .map(|d| {
                g.infos
                    .get(&d.udid)
                    .cloned()
                    .unwrap_or_else(|| DeviceInfo { udid: d.udid.clone(), ..Default::default() })
            })
            .collect()
    }

    /// 旧版本那份不知道是哪台的密码：USB 上只接着一台、正好是要控制的这台、这台还没有自己的密码时，归到它名下。
    /// 接着多台时不动它（也不用它）
    pub(crate) fn adopt_legacy_passcode(&self, g: &Inner) {
        let legacy = self.cfg.legacy_passcode_file();
        let ([only], Some(d)) = (&g.connected[..], &g.device) else { return };
        if only.udid != d.udid || !legacy.is_file() {
            return;
        }
        let dst = self.cfg.passcode_file_for(&d.udid);
        if dst.exists() {
            return;
        }
        let r = self.cfg.ensure_passcodes_dir().and_then(|_| std::fs::rename(&legacy, &dst));
        match r {
            Ok(()) => self.log.log(&format!("旧的锁屏密码只有一份，USB 上只接着 {}，归到它名下", d.udid)),
            Err(e) => tracing::warn!("把旧的锁屏密码归到 {} 名下失败：{e}", d.udid),
        }
    }

    pub(crate) fn notify(&self, g: &mut Inner, e: Event) {
        if !g.dedup.should_send(&e, Instant::now()) {
            return;
        }
        let (title, body) = e.title_body();
        self.log.log(&format!("通知：{title}"));
        let cfg = self.cfg.notify.clone();
        if cfg.any() {
            tokio::spawn(async move {
                notify::send(&cfg, &title, &body).await;
            });
        }
    }

    pub(crate) fn set_state(&self, g: &mut Inner, s: DeviceState) {
        if g.state != s {
            self.log.log(&format!("状态：{}", s.describe()));
            g.state = s;
        }
    }

    /// 重读这台手机上的 WDA 描述文件（misagent，一次约几十毫秒）
    pub(crate) async fn refresh_device_profile(&self, g: &mut Inner) {
        let Some(d) = g.device.clone() else {
            g.device_profile = DeviceProfile::NotChecked;
            return;
        };
        let Some(bundle) = g.persisted.wda_bundle.clone() else {
            g.device_profile = DeviceProfile::Missing;
            return;
        };
        g.device_profile = match d.installed_profiles().await {
            Ok(ps) => match wda_profile_expiry(&ps, g.persisted.team_id.as_deref(), &bundle, &d.udid) {
                Some(t) => DeviceProfile::Expires(t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()),
                None => DeviceProfile::Missing,
            },
            Err(e) => {
                tracing::warn!("读手机上的描述文件失败，按签名记录算：{e}");
                DeviceProfile::Unknown(e.to_string())
            }
        };
    }

    /// 丢掉整个会话：runner、WDA 客户端、正在后台启动的任务、隧道（先停 HID 媒体流）
    pub(crate) async fn drop_session(&self, g: &mut Inner, reason: StopReason) {
        crate::diagnostics::lock(&self.diagnostics).stop(reason);
        if let Some(h) = g.starting.take() {
            h.abort();
        }
        if let Some(r) = g.runner.take() {
            r.stop_with_reason(reason);
        }
        g.wda = None;
        g.wda_generation += 1;
        g.wda_unreachable_until = None;
        g.wda_link_down.reset();
        g.hid_frame = None;
        g.elements_cache = None;
        g.settled_tree = None;
        g.unlocked_at = None;
        drop_tunnel(g).await;
        g.wda_misses.reset();
        g.wda_lock_timeouts = 0;
    }

    pub(crate) async fn ensure_tunnel<'a>(
        &self,
        g: &'a mut Inner,
        device: &Device,
    ) -> Result<&'a mut Tunnel, DeviceError> {
        if g.tunnel.is_none() {
            g.tunnel = Some(device.open_tunnel().await?);
        }
        Ok(g.tunnel.as_mut().expect("刚建好"))
    }

    /// WDA 可用就返回客户端（本进程 runner 存活，且刚才没碰到连不上，见 [`Inner::wda_unreachable_until`]）
    pub(crate) fn wda<'a>(&self, g: &'a mut Inner) -> Option<&'a mut Wda> {
        if g.wda_unreachable_until.is_some_and(|t| Instant::now() < t) {
            return None;
        }
        if g.runner.as_ref().is_some_and(|r| r.alive()) { g.wda.as_mut() } else { None }
    }

    /// 记下 WDA 连不上（[`ictl_wda::WdaError::Unavailable`]）：之后 [`WDA_UNREACHABLE_HOLD`] 内不再碰 WDA。
    /// 只有连不上才算；超时、中途断开说明请求可能已经发出，照旧报错、不退到 HID
    pub(crate) fn note_wda_error(&self, g: &mut Inner, e: &ictl_wda::WdaError) {
        if matches!(e, ictl_wda::WdaError::Unavailable(_)) {
            self.trip_wda_breaker(g);
        }
    }

    pub(crate) fn trip_wda_breaker(&self, g: &mut Inner) {
        if g.wda_unreachable_until.is_none_or(|t| Instant::now() >= t) {
            tracing::info!(event = "wda_breaker_open", hold_ms = WDA_UNREACHABLE_HOLD.as_millis() as u64);
        }
        g.wda_unreachable_until = Some(Instant::now() + WDA_UNREACHABLE_HOLD);
    }

    pub(crate) fn require_wda<'a>(&self, g: &'a mut Inner) -> Result<&'a mut Wda, String> {
        let msg = wda_down(&g.state, g.info.as_ref());
        self.wda(g).ok_or(msg)
    }

    pub(crate) fn account(&self) -> ictl_sign::AccountDir {
        ictl_sign::AccountDir { dir: self.cfg.account_dir(), anisette_url: self.cfg.anisette_url.clone() }
    }

    /// 发布快照给不拿锁的读者（`status`）。日志尾巴在读的时候再取
    pub(crate) fn publish(&self, g: &Inner) {
        let unlock = self.unlock_cfg(g.device.as_ref().map(|d| d.udid.as_str()));
        let pinned = self.pinned(g);
        self.snap.send_replace(Snapshot {
            state: g.state.clone(),
            device: g.info.clone(),
            devices: self.connected_infos(g),
            pinned,
            pinned_by_env: self.cfg.udid.is_some(),
            persisted: g.persisted.clone(),
            device_profile: g.device_profile.clone(),
            auto_unlock: unlock.enabled(),
            unlock_disabled: unlock.disabled_by_failure(),
            notify_configured: self.cfg.notify.any(),
            log_tail: Vec::new(),
            diagnostics: None,
        });
    }

    // ------------------------------------------------------------------ 请求处理

    /// 处理一个控制请求。`queued_at`：请求到达的时间，用来放弃排队太久的操作
    pub async fn handle(self: &Arc<Self>, req: Request, queued_at: Instant, ctx: CallCtx) -> Response {
        use tracing::Instrument;
        let operation = serde_json::to_value(&req)
            .ok()
            .and_then(|value| value["op"].as_str().map(str::to_owned))
            .unwrap_or_default();
        let operation_id = NEXT_OPERATION.fetch_add(1, Ordering::Relaxed);
        let span = tracing::info_span!(
            "device_request",
            operation_id,
            native_pid = std::process::id(),
            native_start_id = ictl_wda::diagnostics::native_start_id(),
            operation,
            diagnostic_id = ?ctx.diagnostic_id
        );
        async {
            let timer = StepTimer::new("device_request");
            tracing::info!(event = "device_request_received", queued_ms = queued_at.elapsed().as_millis() as u64);
            let response = self.handle_request(req, queued_at, ctx, operation_id, operation).await;
            timer.finish(!matches!(response, Response::Error { .. }));
            response
        }
        .instrument(span)
        .await
    }

    async fn handle_request(
        self: &Arc<Self>,
        req: Request,
        queued_at: Instant,
        ctx: CallCtx,
        operation_id: u64,
        operation: String,
    ) -> Response {
        // 不需要设备锁的请求
        match req {
            Request::Daemon => {
                let mut s = self.snap.borrow().clone();
                let unlock = self.unlock_cfg(s.device.as_ref().map(|d| d.udid.as_str()));
                s.unlock_disabled = unlock.disabled_by_failure();
                s.auto_unlock = unlock.enabled();
                s.log_tail = self.log.tail(15);
                s.diagnostics = Some(crate::diagnostics::lock(&self.diagnostics).snapshot());
                return Response::text(serde_json::to_string_pretty(&s).unwrap_or_default());
            }
            // 重签自己分段持锁：签名、安装要几十秒，期间不挡住别的请求
            Request::Resign { force } => {
                return self
                    .resign_now(force, Some((queued_at, ctx.action_deadline)))
                    .await
                    .map(Response::text)
                    .unwrap_or_else(Response::error);
            }
            _ => {}
        }
        if let Err(e) = check_request(&req) {
            return Response::error(e);
        }

        let waiting = StepTimer::new("device_lock_wait");
        let mut g = match ctx.gone {
            Some(mut gone) => tokio::select! {
                biased;
                g = self.inner.lock() => g,
                Ok(()) = &mut gone => return Response::error(ictl_l10n::tr!("调用方已取消,请求没有执行", "The caller cancelled; the request was not carried out")),
            },
            None => self.inner.lock().await,
        };
        let _active = Diagnostics::active(&self.diagnostics, operation_id, operation);
        waiting.finish(true);
        let g = &mut *g;
        if let Some(want) = &ctx.expect_udid {
            let now = g.device.as_ref().map(|d| d.udid.as_str());
            if now != Some(want.as_str()) {
                return Response::error(ictl_l10n::trf!(
                    "正在控制的设备变了(原来是 {want},现在{}),没有执行;先用 iphone_list_devices 确认",
                    "The controlled device changed (it was {want}, now {}); not carried out. Check with iphone_list_devices first",
                    now.map_or(ictl_l10n::tr!("没有设备", "there is no device").into(), |u| ictl_l10n::trf!(
                        "是 {u}",
                        "it is {u}"
                    ))
                ));
            }
        }
        if req.requires_control()
            && let Err(error) = check_action_admission(queued_at, ctx.action_deadline, "daemon_lock")
        {
            return Response::error(error);
        }
        // 改变界面的操作做完后缓存的元素树就作废；只有调用方声明「不改变布局」的按名字点击（max_age_ms > 0）保留
        let keeps_cache = matches!(req, Request::TapElement { max_age_ms, .. } if max_age_ms > 0);
        let invalidates = req.is_device_action() && !keeps_cache;
        if !matches!(req, Request::Observe { .. }) {
            g.settled_tree = None;
        }
        // what the text reports, for clients that act on it (Response::Text.outcome)
        let mut outcome = crate::protocol::Outcome::default();
        let r = match req {
            Request::Status => Ok(self.status_text(g).await),
            Request::Screenshot { region } => {
                let resp = self.screenshot(g, region).await.unwrap_or_else(Response::error);
                self.publish(g);
                return resp;
            }
            Request::Observe { limit, snapshot_timeout_ms } => {
                let response = self
                    .observe(g, limit, Duration::from_millis(snapshot_timeout_ms))
                    .await
                    .unwrap_or_else(Response::error);
                self.publish(g);
                return response;
            }
            Request::ListElements { filter, limit, max_age_ms, snapshot_timeout_ms } => {
                self.list_elements(
                    g,
                    &filter,
                    limit,
                    Duration::from_millis(max_age_ms),
                    Duration::from_millis(snapshot_timeout_ms),
                )
                .await
            }
            Request::Tap { x, y } => self.tap(g, x, y).await,
            Request::TapElement { text, index, max_age_ms } => {
                self.tap_element(g, &text, index, Duration::from_millis(max_age_ms)).await.map(|(t, missed)| {
                    outcome.tapped_nothing = missed;
                    t
                })
            }
            Request::LongPress { x, y, seconds } => self.long_press(g, x, y, seconds).await,
            Request::Swipe { from, to, seconds } => self.swipe(g, from, to, seconds).await,
            Request::TypeText { text, submit } => self.type_text(g, &text, submit).await,
            Request::PressButton { button } => self.press_button(g, &button).await,
            Request::ListApps { include_system } => self
                .list_apps(g, include_system)
                .await
                .map(|a| a.iter().map(|a| format!("{}\t{}", a.name, a.bundle_id)).collect::<Vec<_>>().join("\n")),
            Request::LaunchApp { app } => self.launch_app(g, &app).await.map(|(t, alert)| {
                outcome.alert_reported = alert;
                t
            }),
            Request::TerminateApp { app } => self.terminate_app(g, &app).await,
            Request::OpenUrl { url } => self.open_url(g, &url).await.map(|(t, alert)| {
                outcome.alert_reported = alert;
                t
            }),
            Request::Settle { max_ms } => self.settle(g, Duration::from_millis(max_ms)).await,
            Request::AlertCheck => self.alert_check(g).await,
            Request::WaitFor { text, timeout_ms } => self.wait_for(g, &text, Duration::from_millis(timeout_ms)).await,
            Request::Doctor => Ok(self.doctor(g).await),
            Request::UnlockReset => {
                let _ = std::fs::remove_file(self.cfg.unlock_disabled_flag());
                g.dedup.clear("unlock");
                Ok(ictl_l10n::tr!("已恢复自动解锁", "Auto-unlock re-enabled").into())
            }
            Request::ListDevices => Ok(self.list_devices_text(g)),
            Request::SelectDevice { device } => self.select_device(g, &device).await,
            Request::Daemon | Request::Resign { .. } => unreachable!("上面已处理"),
        };
        if invalidates {
            g.elements_cache = None;
            // 触摸成功会重置手机的自动锁屏计时，确认过的「没锁」顺延；经本进程锁屏时这里已是 None
            if r.is_ok() && g.unlocked_at.is_some() {
                g.unlocked_at = Some(Instant::now());
            }
        }
        self.publish(g);
        r.map(|t| Response::text_with(t, outcome)).unwrap_or_else(Response::error)
    }

    pub async fn shutdown(&self) {
        let mut g = self.inner.lock().await;
        self.log.log("收到停止信号,退出");
        // 先停 HID 媒体流、runner（经 usbmuxd），再停 usbmuxd
        self.drop_session(&mut g, StopReason::DaemonShutdown).await;
        if let Some(mut c) = g.usbmuxd.take() {
            let _ = c.kill().await;
        }
    }
}

/// 参数检查（与设备无关，不必拿锁）
fn check_request(req: &Request) -> Result<(), String> {
    match req {
        Request::Observe { limit, snapshot_timeout_ms } if *limit > 1000 || *snapshot_timeout_ms > 5000 => {
            return Err("Observation limit or snapshot budget exceeded".into());
        }
        Request::ListElements { snapshot_timeout_ms, .. } if *snapshot_timeout_ms > MAX_WAIT_MS => {
            return Err(format!("snapshot_timeout_ms can't exceed {MAX_WAIT_MS}"));
        }
        Request::Settle { max_ms } if *max_ms > MAX_WAIT_MS => {
            return Err(ictl_l10n::trf!("max_ms 不能超过 {MAX_WAIT_MS}", "max_ms can't exceed {MAX_WAIT_MS}"));
        }
        Request::WaitFor { timeout_ms, .. } if *timeout_ms > MAX_WAIT_MS => {
            return Err(ictl_l10n::trf!(
                "timeout 不能超过 {} 秒",
                "timeout can't exceed {} seconds",
                MAX_WAIT_MS / 1000
            ));
        }
        Request::WaitFor { text, .. } if text.is_empty() => {
            return Err(ictl_l10n::tr!("text 不能为空", "text can't be empty").into());
        }
        _ => {}
    }
    let seconds = match req {
        Request::LongPress { seconds, .. } | Request::Swipe { seconds, .. } => *seconds,
        _ => return Ok(()),
    };
    if !seconds.is_finite() || !(0.0..=MAX_GESTURE_SECONDS).contains(&seconds) {
        return Err(ictl_l10n::trf!(
            "seconds 要在 0 到 {MAX_GESTURE_SECONDS} 之间",
            "seconds must be between 0 and {MAX_GESTURE_SECONDS}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_daemon(tag: &str) -> (Arc<Daemon>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("ictl-core-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        (Daemon::new(Config::from_env(&dir)), dir)
    }

    #[tokio::test]
    async fn status_diagnostics_and_busy_tick_do_not_wait_for_device_lock() {
        let (d, dir) = test_daemon("diagnostics-busy");
        let runner = ictl_wda::diagnostics::RunnerDiagnostics::new(7);
        d.diagnostics.lock().unwrap().set_runner(runner);
        d.diagnostics.lock().unwrap().complete(7, Duration::from_millis(12), "ready");
        let busy = d.inner.lock().await;
        let _active = Diagnostics::active(&d.diagnostics, 99, "tap".into());
        tokio::time::timeout(Duration::from_millis(50), d.clone().tick()).await.unwrap();
        let response = tokio::time::timeout(
            Duration::from_millis(50),
            d.handle(Request::Daemon, Instant::now(), CallCtx::default()),
        )
        .await
        .unwrap();
        let Response::Text { text, .. } = response else {
            panic!("expected status text");
        };
        let status: Snapshot = serde_json::from_str(&text).unwrap();
        let evidence = status.diagnostics.unwrap();
        assert_eq!(evidence.last_probe.unwrap().sequence, 1);
        assert_eq!(evidence.last_skip.unwrap().active_operation.unwrap().operation_id, 99);
        assert_eq!(evidence.runner.unwrap().identity.runner_id, 7);
        drop(busy);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 调用方已经断开：还在排队等设备锁的请求不执行
    #[tokio::test]
    async fn queued_request_is_dropped_when_caller_is_gone() {
        let (d, dir) = test_daemon("gone");
        let busy = d.inner.lock().await;
        let (tx, rx) = tokio::sync::oneshot::channel();
        tx.send(()).unwrap();
        let r = d.handle(Request::Status, Instant::now(), CallCtx { gone: Some(rx), ..CallCtx::default() }).await;
        drop(busy);
        assert!(matches!(&r, Response::Error { message } if message.contains("已取消")), "{r:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 批量执行要求的设备不是正在控制的那台（例如拔线后改控了另一台）：不执行
    #[tokio::test]
    async fn request_for_another_device_is_refused() {
        let (d, dir) = test_daemon("expect");
        let ctx = CallCtx { expect_udid: Some("00008130-AAAA".into()), ..CallCtx::default() };
        let r = d.handle(Request::Status, Instant::now(), ctx).await;
        assert!(matches!(&r, Response::Error { message } if message.contains("设备变了")), "{r:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn upstream_deadline_is_rechecked_after_daemon_lock_without_local_renewal() {
        let (d, dir) = test_daemon("expired-upstream");
        let deadline =
            ictl_protocol::ActionDeadline::parse(serde_json::json!(ictl_protocol::monotonic_ns().unwrap() - 1))
                .unwrap();
        let busy = d.inner.lock().await;
        let operation = d.handle(
            Request::Tap { x: 1.0, y: 2.0 },
            Instant::now(),
            CallCtx { action_deadline: Some(deadline), ..CallCtx::default() },
        );
        tokio::pin!(operation);
        assert!(tokio::time::timeout(Duration::from_millis(5), &mut operation).await.is_err());
        drop(busy);
        let response = operation.await;
        assert!(
            matches!(&response, Response::Error { message } if message.contains("expired") || message.contains("已过期")),
            "{response:?}"
        );
        let fresh = d
            .handle(
                Request::Tap { x: 1.0, y: 2.0 },
                Instant::now(),
                CallCtx {
                    action_deadline: Some(ictl_protocol::ActionDeadline::admit().unwrap()),
                    ..CallCtx::default()
                },
            )
            .await;
        assert!(matches!(&fresh, Response::Error { message } if !message.contains("expired")), "{fresh:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn expired_resign_is_rejected_before_its_separate_lock_path() {
        let (d, dir) = test_daemon("expired-resign");
        let deadline =
            ictl_protocol::ActionDeadline::parse(serde_json::json!(ictl_protocol::monotonic_ns().unwrap() - 1))
                .unwrap();
        let response = d
            .handle(
                Request::Resign { force: false },
                Instant::now(),
                CallCtx { action_deadline: Some(deadline), ..CallCtx::default() },
            )
            .await;
        assert!(matches!(response, Response::Error { message } if message.contains("已过期")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn split_gateway_and_daemon_waits_share_one_budget_and_old_clients_keep_local_guard() {
        let (d, dir) = test_daemon("split-expiry");
        // 19s before IPC plus 12s in the native queue; neither queue alone exceeds 30s.
        let deadline =
            ictl_protocol::ActionDeadline::admit_at(ictl_protocol::monotonic_ns().unwrap() - 31_000_000_000).unwrap();
        let queued_at = Instant::now() - Duration::from_secs(12);
        let split = d
            .handle(
                Request::Tap { x: 1.0, y: 2.0 },
                queued_at,
                CallCtx { action_deadline: Some(deadline), ..CallCtx::default() },
            )
            .await;
        assert!(matches!(split, Response::Error { message } if message.contains("已过期")));
        let old = d
            .handle(Request::Tap { x: 1.0, y: 2.0 }, Instant::now() - Duration::from_secs(31), CallCtx::default())
            .await;
        assert!(matches!(old, Response::Error { message } if message.contains("已过期")));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 手机上的描述文件：CMS 外壳里包着 XML plist（同 ictl-sign 的解析方式）
    fn profile(app_id: &str, devices: &[&str], expires: &str) -> Vec<u8> {
        let devs: String = devices.iter().map(|d| format!("<string>{d}</string>")).collect();
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict>
<key>Name</key><string>iOS Team Provisioning Profile</string>
<key>TeamIdentifier</key><array><string>TEAM1</string></array>
<key>ExpirationDate</key><date>{expires}</date>
<key>ProvisionedDevices</key><array>{devs}</array>
<key>Entitlements</key><dict><key>application-identifier</key><string>{app_id}</string></dict>
</dict></plist>"#
        );
        [&[0x30u8, 0x82][..], xml.as_bytes(), &[0x00, 0xa0]].concat()
    }

    #[test]
    fn wda_down_when_no_device_does_not_promise_recovery() {
        assert!(wda_down(&DeviceState::Ready, None).starts_with("WDA 不可用(刚掉线)"));
        let m = wda_down(&DeviceState::Absent, None);
        assert!(m.contains("手机未连接") && !m.contains("1~2 分钟") && !m.contains("可照常用"), "{m}");
    }

    #[test]
    fn wda_down_mentions_hid_fallback_only_from_ios_27() {
        let info = |v: &str| DeviceInfo { product_version: v.into(), ..Default::default() };
        let st = DeviceState::WdaStarting { last_error: None };
        assert!(wda_down(&st, Some(&info("27.0.1"))).contains("可照常用"));
        assert!(wda_down(&st, None).contains("可照常用"));
        let old = wda_down(&st, Some(&info("26.6.1")));
        assert!(old.contains("没有 HID 后备路线") && !old.contains("可照常用"), "{old}");
    }

    const WDA: &str = "com.facebook.WebDriverAgentRunner.xctrunner.TEAM1";

    /// 和上面 XML 里的日期对应的时刻
    fn at(s: &str) -> SystemTime {
        let secs = match s {
            "2026-11-10T15:35:00Z" => 1794324900,
            "2026-11-12T00:00:00Z" => 1794441600,
            _ => unreachable!(),
        };
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// 真机情况：iPhone 上装的是 11-03 签的（11-10 到期），之后给 iPad 签的那份（11-13）只装到了 iPad 上
    #[test]
    fn wda_profile_is_the_one_on_this_phone() {
        let iphone = "00008130-0019295E012B803A";
        let ps = vec![
            profile(&format!("TEAM1.{WDA}"), &[iphone], "2026-11-10T15:35:00Z"),
            profile("TEAM1.com.example.other", &[iphone], "2027-01-01T00:00:00Z"),
            profile(&format!("TEAM2.{WDA}"), &[iphone], "2027-01-01T00:00:00Z"),
            profile(&format!("TEAM1.{WDA}"), &["00008020-000925043499002E"], "2026-11-13T12:26:31Z"),
            b"not a profile".to_vec(),
        ];
        assert_eq!(wda_profile_expiry(&ps, Some("TEAM1"), WDA, iphone), Some(at("2026-11-10T15:35:00Z")));
        // UDID 大小写不敏感；装过两次取晚的
        let mut twice = ps.clone();
        twice.push(profile(&format!("TEAM1.{WDA}"), &[&iphone.to_lowercase()], "2026-11-12T00:00:00Z"));
        assert_eq!(wda_profile_expiry(&twice, Some("TEAM1"), WDA, iphone), Some(at("2026-11-12T00:00:00Z")));
        // 没装过：没有包含这台手机的
        assert_eq!(wda_profile_expiry(&ps, Some("TEAM1"), WDA, "00008101-AAAA"), None);
        // 不知道团队 ID 时只看 Bundle ID
        assert!(wda_profile_expiry(&ps, None, WDA, iphone).is_some());
    }

    #[test]
    fn device_profile_left_falls_back_to_signing_record_only_when_unknown() {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let p = Persisted { profile_expires: Some(now + 6 * 86400), ..Default::default() };
        let days = |d: Option<Duration>| d.map(|d| (d.as_secs() + 3600) / 86400);
        assert_eq!(days(DeviceProfile::Expires(now + 4 * 86400).left(&p)), Some(4));
        assert_eq!(days(DeviceProfile::Unknown("x".into()).left(&p)), Some(6));
        assert_eq!(days(DeviceProfile::NotChecked.left(&p)), Some(6));
        assert_eq!(DeviceProfile::Missing.left(&p), None);
        assert_eq!(DeviceProfile::Expires(now - 10).left(&p), Some(Duration::ZERO));
    }

    #[test]
    fn gesture_seconds_are_bounded() {
        let swipe = |s| Request::Swipe { from: (0.0, 0.0), to: (1.0, 1.0), seconds: s };
        assert!(check_request(&swipe(0.3)).is_ok());
        assert!(check_request(&swipe(1e6)).is_err());
        assert!(check_request(&swipe(-1.0)).is_err());
        assert!(check_request(&swipe(f64::NAN)).is_err());
        assert!(check_request(&Request::LongPress { x: 0.0, y: 0.0, seconds: 10.0 }).is_ok());
    }

    #[test]
    fn corrupt_state_file_loads_as_default() {
        let p = std::env::temp_dir().join(format!("ictl-state-{}.json", std::process::id()));
        std::fs::write(&p, b"{not json").unwrap();
        assert!(Persisted::load(&p).wda_bundle.is_none());
        std::fs::remove_file(&p).unwrap();
    }
}
