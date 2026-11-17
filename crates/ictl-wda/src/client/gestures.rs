//! 手势（W3C actions，期间临时调小等待设置）、输入、按键、系统弹窗。

use super::*;

impl<T: Transport> WdaClient<T> {
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

    /// 发一个手势，期间把等待设置临时换成 `during`（见 [`Self::tap`]），做完恢复
    pub(super) async fn quick_gesture(&mut self, steps: &[PointerStep], during: IdleSettings) -> Result<()> {
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
}
