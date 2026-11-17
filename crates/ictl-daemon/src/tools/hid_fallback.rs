//! HID 后备路线（WDA 不可用时）：屏幕尺寸、方向核对、CoreDevice 截图、HID 会话。

use super::*;

/// HID 触摸前核对横竖时，这么久内截过的 HID 图的宽高直接用
pub(crate) const HID_FRAME_REUSE: Duration = Duration::from_secs(2);
/// 只给 WDA 短暂掉线留窗口。缓存不能跨进程重启、跨设备或长期代替方向查询。
pub(crate) const HID_ROTATION_MAX_AGE: Duration = Duration::from_secs(30);

/// 当前截图必须能确定横竖；横屏还需近期 WDA 旋转角。仅凭宽高无法识别 WDA 离线期间的 180° 翻转，
/// 所以后备操作期间不要翻转设备；恢复 WDA 后重新确认方向。
pub(crate) fn hid_rotation(w: u32, h: u32, known: Option<(Duration, Rotation)>) -> Result<Rotation, String> {
    if w == 0 || h == 0 || w == h {
        return Err(ictl_l10n::tr!(
            "截图尺寸无效，无法核对 HID 方向，没有发送触摸",
            "Invalid screenshot size, can't check the HID orientation, no touch sent"
        )
        .into());
    }
    let recent = known.filter(|(age, _)| *age <= HID_ROTATION_MAX_AGE).map(|(_, r)| r);
    if w > h {
        return recent.filter(|r| r.is_landscape()).ok_or_else(|| {
            ictl_l10n::tr!("当前为横屏，但没有近期且匹配的 WDA 旋转角，没有发送 HID 触摸；等 WDA 恢复后重试，或转回竖屏", "The screen is landscape but there is no recent matching WDA rotation, no HID touch sent; retry once WDA is back, or rotate to portrait").into()
        });
    }
    Ok(if recent == Some(Rotation::UpsideDown) { Rotation::UpsideDown } else { Rotation::Portrait })
}

impl Daemon {
    pub(super) async fn screen(&self, g: &mut Inner) -> Result<ScreenSize, String> {
        if let Some(w) = self.wda(g) {
            match w.screen_size().await {
                Ok(s) => return Ok(s),
                Err(e) => self.note_wda_error(g, &e),
            }
        }
        // 记下的尺寸是别的设备的就不用（旧版本没记是哪台，照用）
        let same = match (&g.persisted.screen_udid, &g.device) {
            (Some(u), Some(d)) => *u == d.udid,
            (Some(_), None) => false,
            (None, _) => true,
        };
        g.persisted
            .screen
            .filter(|_| same)
            .or(self.cfg.screen_fallback)
            .ok_or_else(|| ictl_l10n::tr!("WDA 不可用,也不知道屏幕尺寸;等 WDA 起来一次,或在 .env 里设 SCREEN_W/SCREEN_H", "WDA is unavailable and the screen size is unknown; wait for WDA to start once, or set SCREEN_W/SCREEN_H in .env").into())
    }

    /// 记下 WDA 报的界面旋转角：WDA 挂了以后 HID 点击要靠它换算横屏坐标（约 20 毫秒）
    pub(crate) async fn refresh_hid_rotation(&self, g: &mut Inner) {
        let Some(w) = self.wda(g) else { return };
        match w.rotation().await {
            Ok(r) => g.wda_rotation = Some((Instant::now(), r)),
            Err(e) => self.note_wda_error(g, &e),
        }
    }

    /// 每次 HID 触摸前重新截图核对横竖，不依赖上次截图（转屏、回主屏、解锁都可能改变方向）。
    /// 截图只给宽高，不能分辨横屏左右；短暂掉线时借用近期 WDA 方向，过期或不匹配就不触摸。
    pub(super) async fn hid_touch_rotation(&self, g: &mut Inner) -> Result<Rotation, String> {
        self.refresh_hid_rotation(g).await;
        // 只要宽高：[`HID_FRAME_REUSE`] 内截过 HID 图就用它的，不再为此截一整张 PNG（约 1 秒）
        let (w, h) = match g.hid_frame.filter(|(at, _)| at.elapsed() < HID_FRAME_REUSE) {
            Some((_, size)) => size,
            None => {
                let png = self.hid_screenshot(g).await.map_err(|e| {
                    ictl_l10n::trf!(
                        "核对 HID 方向失败，没有发送触摸：{e}",
                        "Checking the HID orientation failed, no touch sent: {e}"
                    )
                })?;
                imaging::png_size(&png).ok_or(ictl_l10n::tr!(
                    "无法读取截图尺寸，没有发送触摸",
                    "Can't read the screenshot size, no touch sent"
                ))?
            }
        };
        let known = g.wda_rotation.map(|(at, r)| (at.elapsed(), r));
        hid_rotation(w, h, known)
    }

    /// 经隧道截图（WDA 不可用时）。失败就丢掉隧道，下次重建
    pub(super) async fn hid_screenshot(&self, g: &mut Inner) -> Result<Vec<u8>, String> {
        if self.cfg.capture_policy == CapturePolicy::BoundedRemote {
            tracing::warn!(event = "capture_rejected", category = "coredevice_png_forbidden");
            return Err("CoreDevice PNG is forbidden by bounded remote capture policy".into());
        }
        let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        let t = self.ensure_tunnel(g, &device).await.map_err(|e| e.to_string())?;
        match t.screenshot_png().await {
            Ok(p) => {
                g.hid_frame = imaging::png_size(&p).map(|size| (Instant::now(), size));
                Ok(p)
            }
            Err(e) => {
                g.hid_frame = None;
                drop_tunnel(g).await;
                Err(e.to_string())
            }
        }
    }

    /// HID 会话（WDA 不可用时的后备路线）。开不起来就丢掉隧道，下次重建
    pub(super) async fn hid<'a>(&self, g: &'a mut Inner) -> Result<&'a mut HidSession, String> {
        // 低版本建隧道只会报 service not found：直接说清楚
        if !hid_supported(g.info.as_ref()) {
            return Err(wda_down(&g.state, g.info.as_ref()));
        }
        let screen = self.screen(g).await?;
        let device = g.device.clone().ok_or(ictl_l10n::tr!("手机未连接", "Phone not connected"))?;
        // HID 触摸按设备竖放的坐标归一化
        let screen = screen.portrait();
        let t = self
            .ensure_tunnel(g, &device)
            .await
            .map_err(|e| ictl_l10n::trf!("HID 后备路线不可用：{e}", "HID fallback unavailable: {e}"))?;
        if let Err(e) = t.hid(screen).await {
            drop_tunnel(g).await;
            return Err(ictl_l10n::trf!("HID 后备路线不可用：{e}", "HID fallback unavailable: {e}"));
        }
        // 第二次调用直接返回已开好的会话（这样写是为了绕开借用检查对条件返回的限制）
        g.tunnel.as_mut().expect("刚建好").hid(screen).await.map_err(|e| e.to_string())
    }

    pub(super) async fn hid_do<F>(&self, g: &mut Inner, f: F) -> Result<(), String>
    where
        F: for<'h> FnOnce(
            &'h mut HidSession,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = ictl_device::Result<()>> + Send + 'h>>,
    {
        let r = f(self.hid(g).await?).await;
        g.hid_used = Some(Instant::now());
        match r {
            Ok(()) => Ok(()),
            // 参数问题（例如不能输入的字符），连接没坏，不用重建
            Err(e @ DeviceError::InvalidInput(_)) => Err(e.to_string()),
            Err(e) => {
                drop_tunnel(g).await;
                Err(ictl_l10n::trf!("HID 命令失败: {e}", "HID command failed: {e}"))
            }
        }
    }
}
