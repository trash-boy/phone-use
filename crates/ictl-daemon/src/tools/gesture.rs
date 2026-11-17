//! 改界面的操作：点击、按名字点击、长按、滑动、输入、按键、锁屏。只有 WDA 连不上才退到 HID。

use super::*;

/// 滑动手势结束后先等这么久再开始比较元素树：短滑动多半已经停了，少拉一两次 `/source`（每次约 35KB 上行）
pub(super) const SCROLL_FIRST_POLL: Duration = Duration::from_millis(300);
/// 滑动后最多等这么久让列表停下（快速滑动的惯性滚动约 2 秒）
pub(super) const SCROLL_SETTLE: Duration = Duration::from_millis(2500);
/// 按 Home 后最多等这么久，确认主屏到了前台（iPhone 12 mini 上 IOHID 的 Home 约 1.3～1.5 秒）
pub(super) const HOME_SETTLE: Duration = Duration::from_secs(2);
/// 按电源键后最多等这么久，等 WDA 报锁屏（iPhone 12 mini 实测约 1 秒）
pub(super) const LOCK_WAIT: Duration = Duration::from_secs(3);

/// 按 Home，等主屏真的到前台再说「已按」（紧接着查状态时前台不再是旧 App）。
/// 先经 IOHID 按（XCTest 的 home 在 iPhone 12 mini / iOS 26.4.1 上不起作用却报成功），
/// [`HOME_SETTLE`] 内没回到主屏再用 XCTest 的 home 补一次；IOHID 这一下报错也改用 XCTest 的
pub(super) async fn press_home(w: &mut Wda) -> Result<String, WdaError> {
    let mut app = String::new();
    for via_hid in [true, false] {
        let sent = if via_hid { w.press_home_hid().await } else { w.press_button("home").await };
        match sent {
            Ok(()) => {}
            Err(e @ WdaError::Unavailable(_)) => return Err(e),
            Err(e) if via_hid => {
                tracing::info!("经 IOHID 按 Home 报错,改用 XCTest：{e}");
                continue;
            }
            Err(e) => return Err(e),
        }
        let deadline = Instant::now() + HOME_SETTLE;
        loop {
            app = w.active_app().await.unwrap_or_default();
            if app == SPRINGBOARD {
                return Ok(ictl_l10n::tr!("已按 home", "Pressed home").into());
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        if via_hid {
            tracing::info!("经 IOHID 按 Home 后前台还是 {app},改用 XCTest 再按一次");
        }
    }
    Ok(ictl_l10n::trf!(
        "已按 home,但前台还是 {app},先截图看看",
        "Pressed home, but the foreground is still {app}; take a screenshot"
    ))
}

/// 经 IOHID 按电源键锁屏，等 WDA 报锁屏，最多 [`LOCK_WAIT`]。锁上了返回 true。
/// WDA 的 `/wda/lock` 在 iOS 27 以下用 XCTest 的 `pressLockButton`，iPhone 12 mini（iOS 26.4.1）上锁不了屏
pub(super) async fn lock_by_power_hid(w: &mut Wda) -> bool {
    if let Err(e) = w.press_power_hid().await {
        tracing::info!("经 IOHID 按电源键报错：{e}");
        return false;
    }
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        if w.wda_reports_locked().await.unwrap_or(false) {
            return true;
        }
        if Instant::now() >= deadline {
            tracing::info!("经 IOHID 按电源键后 {} 秒还没锁屏,改用 WDA 的锁屏", LOCK_WAIT.as_secs());
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// 滑动后等列表停下（最多 [`SCROLL_SETTLE`]）：沿滑动方向的位置连续两次差不超过 [`STILL_TOLERANCE`]。
/// 没停下返回提醒；元素很少（游戏、地图）看不出来，不等
/// 返回结果说明，以及停稳时的元素树（没停稳或读失败时为 None）
pub(super) async fn wait_scrolled(w: &mut Wda, vertical: bool) -> (String, Option<ictl_wda::elements::Tree>) {
    if w.begin_polling().await.is_err() {
        return (String::new(), None);
    }
    let t0 = Instant::now();
    let r: Result<(bool, Option<ictl_wda::elements::Tree>), WdaError> = async {
        // 一次滑动里方向不会变：只问一次，之后每次轮询只拉元素树
        let view = w.view().await?;
        tokio::time::sleep(SCROLL_FIRST_POLL).await;
        let mut prev = w.tree_in_view(view).await?;
        if prev.elements.len() < VISUAL_SETTLE_BELOW {
            return Ok((true, None));
        }
        while t0.elapsed() < SCROLL_SETTLE {
            tokio::time::sleep(Duration::from_millis(30)).await;
            let cur = w.tree_in_view(view).await?;
            if cur.still_along(&prev, vertical, STILL_TOLERANCE) {
                return Ok((true, Some(cur)));
            }
            prev = cur;
        }
        Ok((false, None))
    }
    .await;
    w.end_polling().await;
    match r {
        Ok((false, _)) => (
            ictl_l10n::trf!(
                "({:.1} 秒后列表还在动,可能还在滚动)",
                " (the list was still moving after {:.1} s, it may still be scrolling)",
                SCROLL_SETTLE.as_secs_f64()
            ),
            None,
        ),
        Ok((true, settled)) => (String::new(), settled),
        Err(_) => (String::new(), None),
    }
}

/// 输入后核对的结果
#[derive(Debug, PartialEq)]
pub(super) enum Typed {
    Ok,
    /// 核对不了：没读到输入框、有密码框、或读树失败
    Unknown,
    /// 结尾有几个字没输进去，已补上
    Fixed(usize),
    /// 对不上（输入框里现在的内容）
    Mismatch(String),
}

pub(super) fn squash(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// 输入框里的内容有没有包含刚输入的文字（忽略空白：手机号之类会被自动加空格）
pub(super) fn typed_into(values: &[String], text: &str) -> bool {
    let want = squash(text);
    values.iter().any(|v| squash(v).contains(&want))
}

/// 输入框结尾是 `text` 的前几个字（后面的丢了）：返回输进去了几个字
pub(super) fn typed_prefix(values: &[String], text: &str) -> Option<usize> {
    let chars: Vec<char> = text.chars().collect();
    values
        .iter()
        .filter_map(|v| {
            let v = squash(v);
            (1..chars.len()).rev().find(|&k| {
                let p = squash(&chars[..k].iter().collect::<String>());
                !p.is_empty() && v.ends_with(&p)
            })
        })
        .max()
}

pub(super) async fn verify_typed(w: &mut Wda, text: &str) -> Typed {
    if squash(text).is_empty() {
        return Typed::Unknown;
    }
    let mut values = None;
    // 树可能比输入框慢一拍：第一次对不上隔一下再读一次，确认真的少了再补，免得补重
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        let Some(v) = w.elements().await.ok().and_then(|e| elements::field_values(&e)) else { return Typed::Unknown };
        if v.is_empty() {
            return Typed::Unknown;
        }
        if typed_into(&v, text) {
            return Typed::Ok;
        }
        values = Some(v);
    }
    let values = values.unwrap_or_default();
    let shown = || values.iter().find(|v| !v.is_empty()).cloned().unwrap_or_default();
    let Some(k) = typed_prefix(&values, text) else { return Typed::Mismatch(shown()) };
    // 剩下的一个字一个字慢慢补
    let rest: Vec<char> = text.chars().skip(k).collect();
    for c in &rest {
        if w.keys(&c.to_string()).await.is_err() {
            return Typed::Mismatch(shown());
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    match w.elements().await.ok().and_then(|e| elements::field_values(&e)) {
        Some(v) if typed_into(&v, text) => Typed::Fixed(rest.len()),
        Some(v) => Typed::Mismatch(v.into_iter().find(|v| !v.is_empty()).unwrap_or_default()),
        None => Typed::Unknown,
    }
}

impl Daemon {
    pub(crate) async fn tap(&self, g: &mut Inner, x: f64, y: f64) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        let pos = format!("({},{})", fmt_g(x), fmt_g(y));
        if let Some(w) = self.wda(g) {
            // 只有连不上才退到 HID；超时说明请求已发出，再用 HID 做一遍可能点两次
            match w.tap(x, y).await {
                Ok(()) => return Ok(ictl_l10n::trf!("已点击 {pos}", "Tapped {pos}")),
                Err(WdaError::Unavailable(_)) => self.trip_wda_breaker(g),
                Err(e) => return Err(e.to_string()),
            }
        }
        let rot = self.hid_touch_rotation(g).await?;
        self.hid_do(g, move |h| {
            Box::pin(async move {
                h.rotation = rot;
                h.tap(x, y).await
            })
        })
        .await?;
        Ok(ictl_l10n::trf!("已点击 {pos}(经 HID)", "Tapped {pos} (via HID)"))
    }

    pub(crate) async fn tap_element(
        &self,
        g: &mut Inner,
        text: &str,
        index: usize,
        max_age: Duration,
    ) -> Result<(String, bool), String> {
        self.ensure_unlocked(g).await?;
        let tree = self.tree(g, max_age).await?;
        let matches = elements::find(&tree.elements, text);
        if matches.is_empty() {
            return Ok((
                ictl_l10n::trf!(
                    "{}包含 \"{text}\" 的元素,先用 iphone_list_elements 或截图看看屏幕",
                    "{}: no element contains \"{text}\"; look at the screen with iphone_list_elements or a screenshot first",
                    tap_missed_prefixes()[0]
                ),
                true,
            ));
        }
        if index >= matches.len() {
            let list: Vec<_> = matches.iter().map(|e| e.describe()).collect();
            return Ok((
                ictl_l10n::trf!(
                    "{} {} 个:\n{}",
                    "{} {} (index out of range):\n{}",
                    tap_missed_prefixes()[1],
                    matches.len(),
                    list.join("\n")
                ),
                true,
            ));
        }
        let e = matches[index];
        // 中心被悬浮栏、键盘、浮层之类盖住时，点上去会落在盖着的东西上
        let ((x, y), moved) = match tree.tap_point(e) {
            TapPoint::Center(p) => (p, String::new()),
            TapPoint::Moved { at, by } => (
                at,
                ictl_l10n::trf!(
                    ";它的中心被「{by}」盖住,改点没被盖住的 ({},{})",
                    "; its center is covered by “{by}”, tapped an uncovered part at ({},{}) instead",
                    fmt_g(at.0),
                    fmt_g(at.1)
                ),
            ),
            TapPoint::Covered { by } => {
                return Ok((
                    ictl_l10n::trf!(
                        "{}:{} 整个被「{by}」盖住,没有点;先滑动让它露出来,或截图后按坐标点",
                        "{}: {} is completely covered by “{by}”, not tapped; scroll to reveal it first, or tap by coordinates from a screenshot",
                        tap_missed_prefixes()[2],
                        e.describe()
                    ),
                    true,
                ));
            }
        };
        self.require_wda(g)?.tap(x, y).await.map_err(|e| e.to_string())?;
        let note = if matches.len() > 1 {
            ictl_l10n::trf!("(共匹配 {} 个,点的是第 {index} 个)", " ({} matches, tapped number {index})", matches.len())
        } else {
            String::new()
        };
        Ok((ictl_l10n::trf!("已点击 {}{note}{moved}", "Tapped {}{note}{moved}", e.describe()), false))
    }

    pub(crate) async fn long_press(&self, g: &mut Inner, x: f64, y: f64, seconds: f64) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        self.require_wda(g)?.touch_and_hold(x, y, seconds).await.map_err(|e| e.to_string())?;
        Ok(ictl_l10n::trf!("已长按 ({},{}) {} 秒", "Long-pressed ({},{}) for {} s", fmt_g(x), fmt_g(y), fmt_g(seconds)))
    }

    pub(crate) async fn swipe(
        &self,
        g: &mut Inner,
        from: (f64, f64),
        to: (f64, f64),
        seconds: f64,
    ) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        let desc = format!("({},{}) → ({},{})", fmt_g(from.0), fmt_g(from.1), fmt_g(to.0), fmt_g(to.1));
        if let Some(w) = self.wda(g) {
            match w.swipe(from, to, (seconds * 1000.0) as u64).await {
                // 滑动请求不等惯性滚动（等的话设置不对能卡十几秒），这里等列表停下再返回：
                // 列表还在滚时下一次点击只会让它停住，点不到东西
                Ok(()) => {
                    let vertical = (to.1 - from.1).abs() >= (to.0 - from.0).abs();
                    let (note, settled) = wait_scrolled(w, vertical).await;
                    g.settled_tree = settled.map(|tree| (Instant::now(), tree));
                    return Ok(ictl_l10n::trf!("已滑动 {desc}{}", "Swiped {desc}{}", note));
                }
                Err(WdaError::Unavailable(_)) => self.trip_wda_breaker(g),
                Err(e) => return Err(e.to_string()),
            }
        }
        let rot = self.hid_touch_rotation(g).await?;
        self.hid_do(g, move |h| {
            Box::pin(async move {
                h.rotation = rot;
                h.drag(from, to, seconds).await
            })
        })
        .await?;
        Ok(ictl_l10n::trf!("已滑动 {desc}(经 HID)", "Swiped {desc} (via HID)"))
    }

    pub(crate) async fn type_text(&self, g: &mut Inner, text: &str, submit: bool) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        let n = text.chars().count();
        if let Some(w) = self.wda(g) {
            // /wda/keys 没有输入框获得焦点时也返回成功，其实什么都没输：先看屏幕上有没有键盘
            let typed = match w.keyboard_shown().await {
                Ok(false) => {
                    return Err(
                        ictl_l10n::tr!("屏幕上没有键盘(没有输入框获得焦点),什么都没输;先点一下输入框,截图确认键盘出来了再输入", "No keyboard on screen (no text field has focus), nothing typed; tap a text field first and check with a screenshot that the keyboard is up").into(),
                    );
                }
                Ok(true) if text.is_empty() => Ok(()),
                Ok(true) => w.keys(text).await,
                Err(e) => Err(e),
            };
            match typed {
                Ok(()) => {
                    // 有的 App 边输入边刷新输入框，会吞掉一部分字（高德搜索框「故宫博物院」只剩「故宫博」），WDA 照样报成功：
                    // 输完读回输入框核对，少了结尾几个字就补上；对不上就不按回车
                    let note = match verify_typed(w, text).await {
                        Typed::Ok | Typed::Unknown => String::new(),
                        Typed::Fixed(k) => ictl_l10n::trf!(
                            "(第一次有 {k} 个字没输进去,已补上)",
                            " ({k} characters were dropped the first time and typed again)"
                        ),
                        Typed::Mismatch(now) => {
                            return Err(ictl_l10n::trf!(
                                "输入了 {n} 个字符,但输入框里现在是「{now}」,和要输入的「{text}」对不上(App 可能吞掉了部分字符){};截图确认后再处理",
                                "Typed {n} characters, but the field now contains “{now}”, which doesn't match “{text}” (the app may have swallowed some characters){}; check with a screenshot before going on",
                                if submit { ictl_l10n::tr!(",没有按回车", ", Return not pressed") } else { "" }
                            ));
                        }
                    };
                    if submit {
                        w.keys("\n").await.map_err(|e| {
                            ictl_l10n::trf!(
                                "文字已输入{note},按回车失败:{e}",
                                "Text typed{note}, pressing Return failed: {e}"
                            )
                        })?;
                    }
                    return Ok(ictl_l10n::trf!(
                        "已输入 {n} 个字符{note}{}",
                        "Typed {n} characters{note}{}",
                        if submit { ictl_l10n::tr!(",并按了回车", " and pressed Return") } else { "" }
                    ));
                }
                Err(WdaError::Unavailable(_)) => self.trip_wda_breaker(g),
                Err(e) => return Err(e.to_string()),
            }
        }
        let payload = if submit { format!("{text}\n") } else { text.to_owned() };
        // 先检查，一个都不输；不打开 HID 会话
        if let Some(c) = payload.chars().find(|c| ascii_to_usage(*c).is_none()) {
            return Err(ictl_l10n::trf!(
                "WDA 不可用,HID 只能输入美式键盘上的英文和符号,不能输入 {c:?}",
                "WDA unavailable; HID can only type English letters and symbols of a US keyboard, not {c:?}"
            ));
        }
        self.hid_do(g, move |h| Box::pin(async move { h.type_ascii(&payload).await })).await?;
        Ok(ictl_l10n::trf!("已输入 {n} 个字符(经 HID)", "Typed {n} characters (via HID)"))
    }

    pub(crate) async fn press_button(&self, g: &mut Inner, button: &str) -> Result<String, String> {
        let hid_button = Button::parse(button).ok_or(ictl_l10n::tr!(
            "button 只能是 home / volume_up / volume_down / lock",
            "button must be home / volume_up / volume_down / lock"
        ))?;
        let wda_name = match hid_button {
            Button::Lock => return self.lock_screen(g).await,
            Button::Home => "home",
            Button::VolumeUp => "volumeUp",
            Button::VolumeDown => "volumeDown",
        };
        self.ensure_unlocked(g).await?;
        if let Some(w) = self.wda(g) {
            // Home 在系统弹窗（定位、通知权限等）挡着时不生效，WDA 却照样返回成功。弹窗属于 SpringBoard，
            // 看前台 App 发现不了，要单独查；按之前查（App 静止时约 0.4 秒，按下后动画中查要好几秒）
            if hid_button == Button::Home
                && let Ok(Some(text)) = w.alert_text().await
            {
                return Ok(ictl_l10n::trf!(
                    "{},按 Home 不会生效,没有按;{TAP_ALERT}",
                    "{}, pressing Home would have no effect, not pressed; {TAP_ALERT_EN}",
                    blocking(&text)
                ));
            }
            let r = if hid_button == Button::Home {
                press_home(w).await
            } else {
                w.press_button(wda_name).await.map(|()| ictl_l10n::trf!("已按 {button}", "Pressed {button}"))
            };
            match r {
                Ok(text) => return Ok(text),
                Err(WdaError::Unavailable(_)) => self.trip_wda_breaker(g),
                Err(e) => return Err(e.to_string()),
            }
        }
        self.hid_do(g, |h| Box::pin(h.press(hid_button))).await?;
        Ok(ictl_l10n::trf!("已按 {button}", "Pressed {button}"))
    }

    pub(super) async fn lock_screen(&self, g: &mut Inner) -> Result<String, String> {
        if let Some(w) = self.wda(g) {
            // 已经锁着就不按：熄屏时再按电源键会把屏幕点亮
            let already = w.locked().await;
            if let Err(e @ WdaError::Unavailable(_)) = &already {
                // 连不上：后面几个 WDA 请求也连不上（每个最长要等 10 秒），直接走 HID
                tracing::info!("查锁屏连不上 WDA，改用 HID 锁屏：{e}");
                self.trip_wda_breaker(g);
                return self.lock_by_hid(g).await;
            }
            let locked = matches!(already, Ok(true)) || lock_by_power_hid(w).await || {
                // WDA 锁屏偶尔等 5 秒报 500（手机其实已锁）：确认锁了就不退到 HID（HID 会开屏幕媒体流）
                match w.lock().await {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::info!("WDA 锁屏报错：{e}");
                        w.wda_reports_locked().await.unwrap_or(false)
                    }
                }
            };
            if locked {
                g.last_lock = Some(Instant::now());
                g.unlocked_at = None;
                return Ok(ictl_l10n::tr!("已锁屏", "Locked").into());
            }
        }
        self.lock_by_hid(g).await
    }

    pub(super) async fn lock_by_hid(&self, g: &mut Inner) -> Result<String, String> {
        self.hid_do(g, |h| Box::pin(h.press(Button::Lock))).await?;
        g.last_lock = Some(Instant::now());
        g.unlocked_at = None;
        Ok(ictl_l10n::tr!("已锁屏", "Locked").into())
    }
}
