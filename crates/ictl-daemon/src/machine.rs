//! 状态机的纯逻辑部分：设备状态、WDA 启动退避、usbmuxd 漏设备判定。可单测，不碰设备。

use std::time::{Duration, Instant};

/// 设备状态（docs/architecture.md §3）
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DeviceState {
    /// usbmuxd 看不到手机
    Absent,
    /// 没有配对记录，需要在手机上点「信任」（iphone-control setup）
    NeedsTrust,
    DeveloperModeOff,
    /// 锁屏时建不了隧道（BFU 只能人工解锁）
    NeedsUnlock,
    /// 还没签名安装 WDA（iphone-control sign）
    NeedsSigning,
    /// 正在建隧道、挂镜像、启动 WDA，或启动失败在退避
    WdaStarting {
        last_error: Option<String>,
    },
    /// 手机重启后首次启用 UI 自动化，等用户在手机上输锁屏密码
    NeedsAutomationPin,
    Ready,
}

impl DeviceState {
    pub fn describe(&self) -> String {
        match self {
            Self::Absent => ictl_l10n::tr!("手机未连接", "Phone not connected").into(),
            Self::NeedsTrust => ictl_l10n::tr!("未配对：运行 iphone-control setup，在手机上点「信任」", "Not paired: run iphone-control setup and tap “Trust” on the phone").into(),
            Self::DeveloperModeOff => ictl_l10n::tr!("开发者模式未打开：运行 iphone-control setup", "Developer Mode is off: run iphone-control setup").into(),
            Self::NeedsUnlock => ictl_l10n::tr!("手机锁屏，隧道建不起来：请解锁手机", "The phone is locked, the tunnel can't be opened: unlock the phone").into(),
            Self::NeedsSigning => ictl_l10n::tr!("WDA 还没签名安装：运行 iphone-control sign", "WDA not signed and installed yet: run iphone-control sign").into(),
            Self::WdaStarting { last_error: None } => ictl_l10n::tr!("正在启动 WDA", "Starting WDA").into(),
            Self::WdaStarting { last_error: Some(e) } => ictl_l10n::trf!("WDA 启动失败，稍后重试：{e}", "WDA failed to start, retrying later: {e}"),
            Self::NeedsAutomationPin => ictl_l10n::tr!("WDA 卡在手机上的「Enable UI Automation」密码框：请在手机上输入锁屏密码", "WDA is stuck at the “Enable UI Automation” passcode prompt on the phone: enter the passcode on the phone").into(),
            Self::Ready => ictl_l10n::tr!("就绪", "Ready").into(),
        }
    }
}

/// WDA 启动失败退避：60s 起翻倍，上限 300s；等自动化密码时固定 60s；手机重新连上时重置
#[derive(Debug, Clone)]
pub struct Backoff {
    next_at: Option<Instant>,
    delay: Duration,
}

pub const BACKOFF_START: Duration = Duration::from_secs(60);
pub const BACKOFF_MAX: Duration = Duration::from_secs(300);
pub const PIN_RETRY: Duration = Duration::from_secs(60);
/// 手机锁着、建不了隧道时多久再试（不用等 60 秒那么久：用户解锁后很快就能起来）
pub const UNLOCK_RETRY: Duration = Duration::from_secs(20);

impl Default for Backoff {
    fn default() -> Self {
        Self { next_at: None, delay: BACKOFF_START }
    }
}

impl Backoff {
    pub fn ready(&self, now: Instant) -> bool {
        self.next_at.is_none_or(|t| now >= t)
    }

    pub fn failed(&mut self, now: Instant) {
        self.next_at = Some(now + self.delay);
        self.delay = (self.delay * 2).min(BACKOFF_MAX);
    }

    pub fn waiting_for_pin(&mut self, now: Instant) {
        self.retry_after(now, PIN_RETRY);
    }

    /// 等用户在手机上操作（输密码、解锁）：固定间隔重试，不翻倍，用户弄好后很快就能起来
    pub fn retry_after(&mut self, now: Instant, d: Duration) {
        self.next_at = Some(now + d);
        self.delay = BACKOFF_START;
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    #[cfg(test)]
    pub fn current_delay(&self) -> Duration {
        self.delay
    }
}

/// 容器里没有 udev，usbmuxd 收不到热插拔：sysfs 上有 Apple 设备但 usbmuxd 看不到，连续 3 次就该重启它
#[derive(Debug, Default)]
pub struct MuxWatch {
    misses: u32,
}

pub const USBMUXD_MISS_LIMIT: u32 = 3;

/// 接着几台设备时控制哪一台，返回在 `udids` 里的下标：
/// - 选定了（`IPHONE_UDID` 或运行中选的）：只认那一台，没接就是没有（不换成别的，免得操作、自动解锁落到另一台上）
/// - 没选定：正在控制的那台还接着就继续用它，不因 usbmuxd 列表顺序变化换台；没有正在控制的才取第一台
pub fn pick_device(udids: &[&str], pinned: Option<&str>, current: Option<&str>) -> Option<usize> {
    let pos = |u: &str| udids.iter().position(|d| d.eq_ignore_ascii_case(u));
    match pinned {
        Some(p) => pos(p),
        None => current.and_then(pos).or(if udids.is_empty() { None } else { Some(0) }),
    }
}

impl MuxWatch {
    /// 返回 true 表示该重启 usbmuxd 了。`on_usb`：sysfs 里的 iPhone/iPad 台数；`seen`：usbmuxd 认到的台数。
    /// 比台数而不是「要控制的那台在不在」：那台没接、另一台接着时不该重启；
    /// 也不是「看没看到任何一台」：已经接着一台时再插一台，没有 udev 的 usbmuxd 认不到它
    pub fn observe(&mut self, on_usb: usize, seen: usize) -> bool {
        if seen >= on_usb {
            self.misses = 0;
            return false;
        }
        self.misses += 1;
        if self.misses >= USBMUXD_MISS_LIMIT {
            self.misses = 0;
            return true;
        }
        false
    }
}

/// Only sustained response failures justify restarting a still-live runner.
/// Link/connect failures reset this evidence; they do not prove WDA is hung.
#[derive(Debug, Default)]
pub struct Misses {
    count: u32,
    since: Option<Instant>,
}

impl Misses {
    pub const LIMIT: u32 = 3;

    pub const CONFIRM_WINDOW: Duration = Duration::from_secs(60);

    pub fn hit(&mut self, now: Instant) -> bool {
        let since = *self.since.get_or_insert(now);
        self.count = self.count.saturating_add(1);
        self.count >= Self::LIMIT && now.saturating_duration_since(since) >= Self::CONFIRM_WINDOW
    }

    pub fn count(&self) -> u32 {
        self.count
    }

    pub fn elapsed(&self, now: Instant) -> Duration {
        self.since.map_or(Duration::ZERO, |since| now.saturating_duration_since(since))
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// 本进程的 runner 还活着、`/status` 却一直连不上（连接建立前就失败）多久，才算手机上的 WDA 卡死、要重启。
/// 短暂连不上（usbmuxd 抖一下、USB 链路忙）按 [`Misses`] 的规则不算数；但宿主侧 runner 活着、手机上的 HTTP 服务
/// 一直不通时，只看 [`Misses`] 永远不会重启，状态也一直是就绪
pub const LINK_DOWN_RESTART: Duration = Duration::from_secs(150);

/// 连续连不上 WDA 的起点（见 [`LINK_DOWN_RESTART`]）。任何一次连上了（不管 WDA 怎么答）都清掉
#[derive(Debug, Default)]
pub struct LinkDown {
    since: Option<Instant>,
}

impl LinkDown {
    /// 记一次探测：`unreachable` 是这次连不上。返回 true 表示连不上已经持续够久，该重启 WDA 了
    pub fn observe(&mut self, now: Instant, unreachable: bool) -> bool {
        if !unreachable {
            self.since = None;
            return false;
        }
        let since = *self.since.get_or_insert(now);
        now.saturating_duration_since(since) >= LINK_DOWN_RESTART
    }

    pub fn elapsed(&self, now: Instant) -> Duration {
        self.since.map_or(Duration::ZERO, |since| now.saturating_duration_since(since))
    }

    pub fn reset(&mut self) {
        self.since = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_cap_and_resets() {
        let t = Instant::now();
        let mut b = Backoff::default();
        assert!(b.ready(t));
        let delays: Vec<_> = (0..5)
            .map(|_| {
                b.failed(t);
                b.next_at.unwrap() - t
            })
            .collect();
        assert_eq!(delays.iter().map(|d| d.as_secs()).collect::<Vec<_>>(), [60, 120, 240, 300, 300]);
        assert!(!b.ready(t + Duration::from_secs(10)));
        b.reset();
        assert!(b.ready(t));
    }

    /// 手机锁着、建不了隧道：固定间隔再试，不翻倍（用户解锁后很快就能起来）
    #[test]
    fn unlock_wait_retries_at_fixed_interval() {
        let t = Instant::now();
        let mut b = Backoff::default();
        for _ in 0..3 {
            b.retry_after(t, UNLOCK_RETRY);
            assert!(!b.ready(t + UNLOCK_RETRY - Duration::from_secs(1)));
            assert!(b.ready(t + UNLOCK_RETRY));
        }
        assert_eq!(b.current_delay(), BACKOFF_START);
    }

    /// 回归用例 #8：等自动化密码时固定 60 秒重试，不指数退避
    #[test]
    fn pin_wait_retries_every_minute() {
        let t = Instant::now();
        let mut b = Backoff::default();
        b.failed(t);
        b.failed(t);
        b.waiting_for_pin(t);
        assert!(!b.ready(t + Duration::from_secs(59)));
        assert!(b.ready(t + Duration::from_secs(60)));
        assert_eq!(b.current_delay(), BACKOFF_START);
    }

    #[test]
    fn device_pick_is_pinned_or_sticky() {
        let (a, b) = ("00008130-AAAA", "00008020-BBBB");
        // 没选定：第一台；正在控制 B 时列表顺序变了也还是 B
        assert_eq!(pick_device(&[a, b], None, None), Some(0));
        assert_eq!(pick_device(&[a, b], None, Some(b)), Some(1));
        // 正在控制的拔了：换成还在的
        assert_eq!(pick_device(&[a], None, Some(b)), Some(0));
        // 选定了 B：只认 B（大小写不敏感），B 没接就没有，不换成 A
        assert_eq!(pick_device(&[a, b], Some("00008020-bbbb"), Some(a)), Some(1));
        assert_eq!(pick_device(&[a], Some(b), Some(a)), None);
        assert_eq!(pick_device(&[], None, None), None);
    }

    /// 回归用例 #7：sysfs 有设备但 usbmuxd 看不到，连续 3 次才重启
    #[test]
    fn usbmuxd_restart_after_three_misses() {
        let mut w = MuxWatch::default();
        assert!(!w.observe(1, 0));
        assert!(!w.observe(1, 0));
        assert!(w.observe(1, 0));
        assert!(!w.observe(1, 0), "重启后重新计数");
        assert!(!w.observe(0, 0), "USB 上没有设备不算漏");
        assert!(!w.observe(1, 1));
    }

    /// 接着 iPad 时再插 iPhone：usbmuxd 只认到 1 台，也要重启
    #[test]
    fn usbmuxd_restart_when_a_second_device_is_missed() {
        let mut w = MuxWatch::default();
        assert!(!w.observe(2, 1));
        assert!(!w.observe(2, 1));
        assert!(w.observe(2, 1));
        assert!(!w.observe(2, 2));
        // 拔掉一台、usbmuxd 还没更新列表：不算漏
        assert!(!w.observe(1, 2));
    }

    #[test]
    fn misses_need_sustained_failures_and_reset_after_recovery_or_link_uncertainty() {
        let now = Instant::now();
        let mut m = Misses::default();
        assert!(!m.hit(now));
        assert!(!m.hit(now + Duration::from_secs(10)));
        assert!(!m.hit(now + Duration::from_secs(20)), "three fast misses cannot kill a live runner");
        assert!(m.hit(now + Duration::from_secs(60)));
        m.reset(); // A successful response OR an unconfirmed link failure breaks the sequence.
        assert!(!m.hit(now + Duration::from_secs(61)));
        assert!(!m.hit(now + Duration::from_secs(121)), "also require at least three failures");
        assert!(m.hit(now + Duration::from_secs(122)));
    }

    /// 短暂连不上不算；连不上持续超过 LINK_DOWN_RESTART 才重启；中间连上过一次就重新计时
    #[test]
    fn sustained_link_failure_restarts_but_brief_blips_do_not() {
        let t = Instant::now();
        let mut l = LinkDown::default();
        for s in [0, 10, 60, 140] {
            assert!(!l.observe(t + Duration::from_secs(s), true), "{s}s");
        }
        assert!(l.observe(t + LINK_DOWN_RESTART, true));
        assert_eq!(l.elapsed(t + LINK_DOWN_RESTART), LINK_DOWN_RESTART);
        // 连上了一次（哪怕 WDA 答得不对）：重新计时
        assert!(!l.observe(t + Duration::from_secs(160), false));
        assert!(!l.observe(t + Duration::from_secs(170), true));
        assert!(!l.observe(t + Duration::from_secs(170) + LINK_DOWN_RESTART - Duration::from_secs(1), true));
        assert!(l.observe(t + Duration::from_secs(170) + LINK_DOWN_RESTART, true));
        l.reset();
        assert_eq!(l.elapsed(t), Duration::ZERO);
    }

    #[test]
    fn state_serializes_with_tag() {
        let s = serde_json::to_string(&DeviceState::WdaStarting { last_error: None }).unwrap();
        assert_eq!(s, r#"{"state":"wda_starting","last_error":null}"#);
    }
}
