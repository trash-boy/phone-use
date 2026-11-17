//! 各工具请求的实现：优先 WDA，WDA 连不上时坐标类操作退到 HID。调用方持有设备锁。

use std::time::{Duration, Instant};

use base64::Engine;
use ictl_device::{AppInfo, Button, DeviceError, HidSession, Rotation, ScreenSize, hid::ascii_to_usage};
use ictl_unlock::UnlockError;
use ictl_wda::client::SPRINGBOARD;
use ictl_wda::diagnostics::StopReason;
use ictl_wda::elements::{self, TapPoint, Tree};
use ictl_wda::timing::StepTimer;
use ictl_wda::{Launch, WdaError};

use crate::config::CapturePolicy;
use crate::core::{Daemon, Inner, Wda, drop_tunnel, hid_supported, wda_down};
use crate::imaging::{self, fmt_g};
use crate::notify::Event;
use crate::protocol::{ALERT_BLOCKING, Response, TAP_MISSED};

mod apps;
mod capture;
mod gesture;
mod hid_fallback;

pub(crate) use hid_fallback::HID_ROTATION_MAX_AGE;

/// Consecutive `/wda/locked` timeouts (10 s each) after which WDA is restarted
const WDA_LOCK_TIMEOUT_RESTART: u32 = 2;

/// 锁屏后至少等这么久再自动解锁：锁屏过渡没走完就唤醒，WDA 第一下上滑卡约 16 秒后报 `point.x != INFINITY`
pub(crate) const LOCK_SETTLE: Duration = Duration::from_secs(3);
/// 确认没锁屏（或之后又成功触摸过）这么久内不再检查。iOS 自动锁屏最短要空闲 30 秒，触摸会重置计时；
/// 经本进程锁屏会立刻清掉。手动按电源键锁屏最多晚这么久才发现：那时的操作落在锁屏上，下一步观察即可看到
pub(crate) const UNLOCK_REUSE: Duration = Duration::from_secs(20);
/// 滑动停稳后的元素树给紧接着的观察复用的时限
pub(crate) const SETTLED_TREE_REUSE: Duration = Duration::from_secs(2);
/// 元素少于这么多个就按画面判断是否稳定（游戏只有几个状态栏元素；普通 App 页面通常几十个）
const VISUAL_SETTLE_BELOW: usize = 12;
/// 两帧缩略图平均逐像素差低于这个值算静止（0～255）。游戏背景的轻微摆动约 1～2，消除动画几十
const VISUAL_STILL: f64 = 3.0;
/// 整体静止时，某一小块的平均差超过这个值算「还有一小块在动」（加载圈、小动画）。完全没动的两帧差是 0。
/// 只在画面大部分是空白时才提醒（[`imaging::mostly_blank`]），游戏、地图的待机动画不算
const LOCAL_MOTION: f64 = 4.0;
/// 等界面稳定时，前后两次元素位置差不超过这么多点就算没动（惯性滚动的尾巴）
const STILL_TOLERANCE: i64 = 3;
/// WDA 不可用时 status 里说明哪些功能用不了
const NEEDS_WDA: &str = "读界面元素、按名字点击、启动 App、输入中文要等 WDA 恢复";
const NEEDS_WDA_EN: &str =
    "reading elements, tapping by text, launching apps and typing non-ASCII text wait until WDA is back";
/// 列元素一个都没列出来时的说明。带 filter 没匹配上时要说清楚，不然 agent 会以为屏幕上什么都没有
fn no_elements_text(filter: &str, total: usize) -> String {
    if filter.is_empty() || total == 0 {
        // 与 toolkit 的返回文字一致
        ictl_l10n::tr!("没有找到带文字的元素", "No elements with text found").into()
    } else {
        ictl_l10n::trf!(
            "没有找到包含 \"{filter}\" 的元素(屏幕上另有 {total} 个带文字的元素,不带 filter 可全部列出)",
            "No element containing \"{filter}\" ({total} other elements with text on screen; leave out filter to list them all)"
        )
    }
}

fn alert_msg(text: &str) -> String {
    ictl_l10n::trf!(
        "\n注意:{},点击、滑动都会落在弹窗上;{TAP_ALERT}",
        "\nNote: {}, taps and swipes land on the alert; {TAP_ALERT_EN}",
        blocking(text)
    )
}

const TAP_ALERT: &str = "先用 iphone_tap_element 点弹窗上的按钮";
const TAP_ALERT_EN: &str = "first tap a button on the alert with iphone_tap_element";

fn needs_wda() -> &'static str {
    if ictl_l10n::is_en() { NEEDS_WDA_EN } else { NEEDS_WDA }
}

/// How a missed `iphone_tap_element` result starts: the Chinese prefixes are fixed because older MCP servers
/// recognise them; new ones read [`crate::protocol::Outcome`] instead
fn tap_missed_prefixes() -> [&'static str; 3] {
    if ictl_l10n::is_en() { ["Not found", "Only matched", "Covered"] } else { TAP_MISSED }
}

/// 「屏幕上有系统弹窗「…」挡着」：旧版本 MCP 端靠固定的开头认出结果里已经报过弹窗（新版本看 Outcome）
fn blocking(text: &str) -> String {
    ictl_l10n::trf!("{ALERT_BLOCKING}「{text}」挡着", "a system alert is on screen: “{text}”")
}

/// 树里有弹窗就返回提醒
fn alert_note(tree: &Tree) -> String {
    match tree.alert() {
        Some(a) => {
            let title = [&a.label, &a.name].into_iter().find(|s| !s.is_empty()).cloned().unwrap_or_default();
            ictl_l10n::trf!(
                "\n注意:屏幕上有弹窗「{title}」,先点弹窗上的按钮(见元素列表或截图)",
                "\nNote: an alert “{title}” is on screen; tap a button on it first (see the element list or the screenshot)"
            )
        }
        None => String::new(),
    }
}

impl Daemon {
    /// 配置了密码就在锁屏时自动解锁；没配置而手机锁着则报错（与 toolkit 一致）
    pub(crate) async fn ensure_unlocked(&self, g: &mut Inner) -> Result<(), String> {
        let checking = StepTimer::new("unlock_check");
        if g.unlocked_at.is_some_and(|t| t.elapsed() < UNLOCK_REUSE) {
            tracing::debug!(event = "unlock_check_reused");
            checking.finish(true);
            return Ok(());
        }
        let cfg = self.unlock_cfg(g.device.as_ref().map(|d| d.udid.as_str()));
        if let Some(left) = g.last_lock.and_then(|t| LOCK_SETTLE.checked_sub(t.elapsed())) {
            tokio::time::sleep(left).await;
        }
        let Some(wda) = self.wda(g) else { return Ok(()) };
        let outcome = ictl_unlock::ensure_unlocked(wda, &cfg).await;
        checking.finish(outcome.is_ok());
        // 连不上 WDA：这个请求接下来不再碰它（见 Inner::wda_unreachable_until），坐标类操作直接走 HID
        match &outcome {
            Ok(ictl_unlock::Outcome::Unknown) => self.trip_wda_breaker(g),
            Err(UnlockError::Wda(e)) => self.note_wda_error(g, e),
            _ => {}
        }
        // 只有确认过没锁才记下：WDA 连不上（Unknown）时不知道锁没锁，记下的话之后 20 秒内的 HID 操作都不再检查
        g.unlocked_at = outcome.as_ref().is_ok_and(|o| o.confirmed_unlocked()).then(Instant::now);
        // A WDA that keeps timing out on the lock query is stuck (seen after the phone slept through a long
        // confirmation wait). Health probes cannot run while requests hold the device lock, so restart it here.
        if matches!(outcome, Err(UnlockError::Wda(WdaError::Timeout(_)))) {
            g.wda_lock_timeouts += 1;
            if g.wda_lock_timeouts >= WDA_LOCK_TIMEOUT_RESTART {
                tracing::warn!(event = "wda_restart", reason = "consecutive lock query timeouts", runner_alive = true);
                self.log.log("WDA 连续查询锁屏超时，重启 WDA");
                self.drop_session(g, StopReason::HealthRestart).await;
            }
        } else {
            g.wda_lock_timeouts = 0;
        }
        match outcome {
            Ok(_) => Ok(()),
            Err(e @ (UnlockError::WrongPasscode | UnlockError::Uncertain(_))) => {
                self.notify(g, Event::UnlockDisabled);
                Err(ictl_l10n::trf!("{e},已推送通知", "{e}, notification sent"))
            }
            Err(UnlockError::NoPasscode) if self.cfg.legacy_passcode_file().is_file() => {
                Err(ictl_l10n::tr!("手机已锁屏,需要人工解锁:旧版本保存的锁屏密码没记是哪台设备的,同时接着几台时不会用它;\
在这台设备连着时运行 iphone-control passcode set 重新设置", "The phone is locked and has to be unlocked by hand: the passcode saved by an older version has no device, and it isn't used while several devices are connected; run iphone-control passcode set while this device is connected to set it again")
                    .into())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// 接着多台、或选定的那台没接时，说明在控制哪台、怎么切换；只接着一台时为空（输出和原来一样）
    pub(crate) fn devices_line(&self, g: &Inner) -> Option<String> {
        let pinned = self.pinned(g);
        let current = g.device.as_ref().map(|d| d.udid.as_str());
        let others: Vec<String> = self
            .connected_infos(g)
            .iter()
            .filter(|i| Some(i.udid.as_str()) != current)
            .map(crate::core::device_label)
            .collect();
        let how = if self.cfg.udid.is_some() {
            ictl_l10n::tr!(
                "(IPHONE_UDID 固定了设备,要换先改 .env)",
                " (IPHONE_UDID pins the device; change .env to switch)"
            )
        } else {
            ictl_l10n::tr!(";用 iphone_select_device 切换", "; switch with iphone_select_device")
        };
        match (current, &pinned) {
            (None, Some(p)) => Some(ictl_l10n::trf!(
                "设备: 选定的 {p} 没连接{}{how}",
                "Device: the selected {p} is not connected{}{how}",
                if others.is_empty() {
                    String::new()
                } else {
                    ictl_l10n::trf!(",接着的有 {}", ", connected are {}", others.join(ictl_l10n::tr!("、", ", ")))
                }
            )),
            (Some(_), _) if !others.is_empty() => {
                let me = g.info.as_ref().map(crate::core::device_label).unwrap_or_default();
                let mode = if pinned.is_some() {
                    ictl_l10n::tr!("选定", "selected")
                } else {
                    ictl_l10n::tr!("自动选", "automatic")
                };
                Some(ictl_l10n::trf!(
                    "设备: 正在控制 {me}({mode});另外接着 {}{how}",
                    "Device: controlling {me} ({mode}); also connected {}{how}",
                    others.join(ictl_l10n::tr!("、", ", "))
                ))
            }
            _ => None,
        }
    }

    pub(crate) async fn status_text(&self, g: &mut Inner) -> String {
        let mut daemon_line = ictl_l10n::trf!("守护进程: {}", "Daemon: {}", g.state.describe());
        if let Some(l) = self.devices_line(g) {
            daemon_line = format!("{l}\n{daemon_line}");
        }
        let unlock = self.unlock_cfg(g.device.as_ref().map(|d| d.udid.as_str()));
        let Some(w) = self.wda(g) else {
            return format!("{}\n{}\n{daemon_line}", wda_down(&g.state, g.info.as_ref()), needs_wda());
        };
        let r: Result<String, WdaError> = async {
            let ready = w.ready().await?;
            let app = w.active_app().await?;
            let s = w.screen_size().await?;
            let check = w.check_lock(None, false).await?;
            let locked = check.locked;
            let mut lock = if check.uncertain {
                ictl_l10n::tr!("可能已锁屏(屏幕上是锁屏样式的界面,WDA 说没锁;配置了自动解锁时操作前会先尝试解锁)", "possibly locked (the screen looks like the lock screen though WDA says unlocked; with auto-unlock configured, actions try to unlock first)").to_owned()
            } else if !locked {
                ictl_l10n::tr!("未锁屏", "unlocked").to_owned()
            } else if unlock.disabled_by_failure() {
                ictl_l10n::tr!("已锁屏(自动解锁之前失败过已停用,需要人工解锁,核对密码后运行 iphone-control unlock-reset)", "locked (auto-unlock was disabled after an earlier failure; unlock by hand, check the passcode, then run iphone-control unlock-reset)").to_owned()
            } else if unlock.enabled() {
                ictl_l10n::tr!("已锁屏(操作时会自动解锁)", "locked (actions unlock automatically)").to_owned()
            } else {
                ictl_l10n::tr!("已锁屏(未配置自动解锁,需要人工解锁)", "locked (no auto-unlock configured; unlock by hand)").to_owned()
            };
            if !locked && unlock.disabled_by_failure() {
                lock += ictl_l10n::tr!(";自动解锁已停用", "; auto-unlock disabled");
            }
            let ready = if ready { "True" } else { "False" };
            Ok(ictl_l10n::trf!("WDA 就绪: {ready}\n锁屏: {lock}\n前台 App: {app}\n屏幕: {}x{} 点", "WDA ready: {ready}\nLocked: {lock}\nForeground app: {app}\nScreen: {}x{} points",
                fmt_g(s.width),
                fmt_g(s.height)
            ))
        }
        .await;
        match r {
            Ok(t) => format!("{t}\n{daemon_line}"),
            Err(e) => {
                self.note_wda_error(g, &e);
                format!("{e}\n{}\n{daemon_line}", needs_wda())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::capture::*;
    use super::gesture::*;
    use super::hid_fallback::*;
    use super::*;

    #[tokio::test]
    async fn bounded_capture_never_calls_coredevice_fallback_and_preserves_error() {
        let calls = std::cell::Cell::new(0);
        let result = capture_with_fallback(
            CapturePolicy::BoundedRemote,
            Err("MJPEG capture failed: slow capture".into()),
            || async {
                calls.set(calls.get() + 1);
                Ok(b"large PNG".to_vec())
            },
        )
        .await;
        assert_eq!(calls.get(), 0, "remote capture must not fetch CoreDevice PNG");
        assert_eq!(result, Err("MJPEG capture failed: slow capture".into()));
    }

    #[tokio::test]
    async fn local_capture_keeps_coredevice_fallback() {
        let calls = std::cell::Cell::new(0);
        let result = capture_with_fallback(CapturePolicy::Local, Err("WDA unavailable".into()), || async {
            calls.set(calls.get() + 1);
            Ok(b"local PNG".to_vec())
        })
        .await
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(result, (b"local PNG".to_vec(), "HID"));
    }

    #[tokio::test]
    async fn bounded_shared_hid_capture_rejects_before_device_or_tunnel_and_orientation_touch() {
        let dir = std::env::temp_dir().join(format!("ictl-bounded-hid-{}", std::process::id()));
        let mut cfg = crate::config::Config::from_env(&dir);
        cfg.capture_policy = CapturePolicy::BoundedRemote;
        let daemon = Daemon::new(cfg);
        let mut inner = daemon.inner.lock().await;
        let error = daemon.hid_screenshot(&mut inner).await.unwrap_err();
        assert!(error.contains("bounded remote capture"), "{error}");
        let error = daemon.hid_touch_rotation(&mut inner).await.unwrap_err();
        assert!(error.contains("bounded remote capture"), "{error}");
        assert!(inner.tunnel.is_none());
    }

    use crate::testkit;
    use ictl_wda::fake::Reply;

    /// 安全规则（回归用例 #29）：只有连不上（Unavailable）才退到 HID；连不上之后同一请求不再碰 WDA
    #[tokio::test]
    async fn unavailable_tap_and_swipe_fall_back_to_hid_without_touching_wda_again() {
        let (d, dir) = testkit::daemon("tap-unavailable");
        for swipe in [false, true] {
            let fake = testkit::healthy_wda();
            fake.on("POST", "/actions", Reply::unavailable());
            let mut g = d.inner.lock().await;
            testkit::attach(&mut g, &fake);
            g.wda_unreachable_until = None;
            let r = if swipe {
                d.swipe(&mut g, (10.0, 10.0), (10.0, 200.0), 0.3).await
            } else {
                d.tap(&mut g, 10.0, 20.0).await
            };
            // 没接手机：HID 路线报「没有发送触摸」，说明确实退到了 HID
            let e = r.unwrap_err();
            assert!(e.contains("HID") && e.contains("没有发送触摸"), "{e}");
            assert_eq!(fake.count("POST", "/actions"), 1);
            let after = fake.requests().into_iter().skip_while(|(_, p)| p != "/actions").skip(1).collect::<Vec<_>>();
            assert!(after.is_empty(), "WDA touched after it was unreachable: {after:?}");
            assert!(g.wda_unreachable_until.is_some());
            assert!(d.wda(&mut g).is_none());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 安全规则：请求已发出后的超时、中途断开不退到 HID、不重做，原样报错
    #[tokio::test]
    async fn timed_out_or_interrupted_actions_are_never_redone() {
        let (d, dir) = testkit::daemon("tap-timeout");
        for (reply, word) in [(Reply::timeout(), "超时"), (Reply::interrupted(), "断开")] {
            for swipe in [false, true] {
                let fake = testkit::healthy_wda();
                fake.on("POST", "/actions", reply.clone());
                let mut g = d.inner.lock().await;
                testkit::attach(&mut g, &fake);
                let r = if swipe {
                    d.swipe(&mut g, (10.0, 10.0), (10.0, 200.0), 0.3).await
                } else {
                    d.tap(&mut g, 10.0, 20.0).await
                };
                let e = r.unwrap_err();
                assert!(e.contains(word) && !e.contains("HID"), "{e}");
                assert_eq!(fake.count("POST", "/actions"), 1, "the action must not be sent twice");
                assert!(g.tunnel.is_none() && g.hid_used.is_none(), "no HID session for a sent action");
                assert!(g.wda_unreachable_until.is_none());
            }
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 回归：WDA 连不上时不能记成「确认没锁」，否则之后 20 秒的 HID 操作都不检查锁屏
    #[tokio::test]
    async fn unreachable_lock_check_is_not_a_confirmed_unlock() {
        let (d, dir) = testkit::daemon("unlock-unknown");
        let fake = testkit::healthy_wda();
        fake.on("GET", "/wda/locked", Reply::unavailable());
        let mut g = d.inner.lock().await;
        testkit::attach(&mut g, &fake);
        d.ensure_unlocked(&mut g).await.unwrap();
        assert!(g.unlocked_at.is_none());
        // 断路：接下来的点击不再碰 WDA，直接走 HID
        let e = d.tap(&mut g, 1.0, 2.0).await.unwrap_err();
        assert!(e.contains("HID"), "{e}");
        assert_eq!(fake.count("POST", "/actions"), 0);
        // 能连上、确认没锁时才记下
        drop(g);
        let fake = testkit::healthy_wda();
        let mut g = d.inner.lock().await;
        testkit::attach(&mut g, &fake);
        g.wda_unreachable_until = None;
        d.ensure_unlocked(&mut g).await.unwrap();
        assert!(g.unlocked_at.is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 有界截图策略下区域放大直接报不支持，不拿 MJPEG 帧去裁
    #[tokio::test]
    async fn region_zoom_is_refused_under_the_bounded_policy() {
        let dir = std::env::temp_dir().join(format!("ictl-bounded-region-{}", std::process::id()));
        let mut cfg = crate::config::Config::from_env(&dir);
        cfg.capture_policy = CapturePolicy::BoundedRemote;
        let d = Daemon::new(cfg);
        let fake = testkit::healthy_wda();
        let mut g = d.inner.lock().await;
        testkit::attach(&mut g, &fake);
        let e = d.screenshot(&mut g, Some((0.0, 0.0, 100.0, 100.0))).await.unwrap_err();
        assert!(e.contains("full-resolution"), "{e}");
        assert!(fake.requests().is_empty());
    }

    /// 等元素出现：方向只问一次；每轮读树按剩下的预算限时，超过上限就报没出现
    #[tokio::test]
    async fn wait_for_bounds_each_round_by_the_remaining_budget() {
        let (d, dir) = testkit::daemon("wait-for");
        let fake = testkit::healthy_wda();
        fake.on("GET", "/source", Reply::Hang);
        let mut g = d.inner.lock().await;
        testkit::attach(&mut g, &fake);
        g.unlocked_at = Some(Instant::now());
        let t0 = Instant::now();
        let e = d.wait_for(&mut g, "x", Duration::from_secs(1)).await.unwrap_err();
        assert!(e.contains("没出现"), "{e}");
        assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
        assert!(fake.count("GET", "/orientation") <= 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn poll_rounds_use_the_remaining_budget_with_a_floor() {
        let t0 = Instant::now();
        assert!(poll_round(t0, Duration::from_secs(30)) > Duration::from_secs(29));
        assert_eq!(poll_round(t0 - Duration::from_secs(40), Duration::from_secs(30)), MIN_POLL_ROUND);
    }

    /// HID 触摸前核对横竖：2 秒内截过 HID 图就用它的宽高，不再截一整张
    #[tokio::test]
    async fn hid_orientation_check_reuses_a_recent_frame_size() {
        let (d, dir) = testkit::daemon("hid-frame");
        let mut g = d.inner.lock().await;
        g.hid_frame = Some((Instant::now(), (1290, 2796)));
        assert_eq!(d.hid_touch_rotation(&mut g).await, Ok(Rotation::Portrait));
        g.hid_frame = Some((Instant::now() - HID_FRAME_REUSE, (1290, 2796)));
        // 旧了：要重新截图（没接手机，截不了）
        assert!(d.hid_touch_rotation(&mut g).await.is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn hid_rotation_requires_fresh_matching_landscape() {
        use Rotation::*;
        let fresh = |r| Some((Duration::ZERO, r));
        assert_eq!(hid_rotation(2796, 1290, fresh(TopToLeft)), Ok(TopToLeft));
        assert_eq!(hid_rotation(2796, 1290, fresh(TopToRight)), Ok(TopToRight));
        assert!(hid_rotation(2796, 1290, None).is_err());
        assert!(hid_rotation(2796, 1290, fresh(Portrait)).is_err());
        assert!(hid_rotation(2796, 1290, Some((Duration::from_secs(31), TopToLeft))).is_err());
        assert_eq!(hid_rotation(1290, 2796, fresh(TopToLeft)), Ok(Portrait));
        assert_eq!(hid_rotation(1290, 2796, fresh(UpsideDown)), Ok(UpsideDown));
        assert_eq!(hid_rotation(1290, 2796, None), Ok(Portrait));
        assert!(hid_rotation(0, 2796, fresh(Portrait)).is_err());
        assert!(hid_rotation(100, 100, fresh(Portrait)).is_err());
    }

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn no_match_with_filter_names_the_filter() {
        assert_eq!(no_elements_text("", 0), "没有找到带文字的元素");
        assert_eq!(no_elements_text("通用", 0), "没有找到带文字的元素");
        let t = no_elements_text("通用", 12);
        assert!(t.contains("\"通用\"") && t.contains("12 个"), "{t}");
    }

    #[test]
    fn typed_text_is_found_ignoring_spaces() {
        assert!(typed_into(&v(&["搜索", "红烧肉做法"]), "红烧肉做法"));
        assert!(typed_into(&v(&["130 1234 5678"]), "13012345678"));
        assert!(!typed_into(&v(&["故宫博"]), "故宫博物院"));
    }

    #[test]
    fn dropped_tail_is_detected_after_existing_text() {
        assert_eq!(typed_prefix(&v(&["故宫博"]), "故宫博物院"), Some(3));
        // 输入框原来就有字
        assert_eq!(typed_prefix(&v(&["abc故宫"]), "故宫博物院"), Some(2));
        // 一个字都没进去（或者是占位文字）：不猜，报对不上
        assert_eq!(typed_prefix(&v(&["搜索"]), "故宫博物院"), None);
        assert_eq!(typed_prefix(&v(&[""]), "故宫"), None);
    }
}
