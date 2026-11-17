//! 锁屏判断（用元素树纠正 `/wda/locked` 的误报）、锁屏、密码栏。

use super::*;

impl<T: Transport> WdaClient<T> {
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
    pub(super) async fn cover_tree(&self) -> Result<(elements::CoverSheet, Option<String>)> {
        if self.active_app().await? != SPRINGBOARD {
            return Ok((elements::CoverSheet::None, None));
        }
        let xml = self.source_xml().await?;
        Ok((elements::cover_sheet(&xml), Some(xml)))
    }

    pub async fn lock(&mut self) -> Result<()> {
        self.post("/wda/lock", json!({})).await.map(drop)
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
