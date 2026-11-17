//! 请求、会话（失效时重建一次）、appium 等待设置、屏幕尺寸和方向、前台 App。

use super::*;

impl<T: Transport> WdaClient<T> {
    #[tracing::instrument(name = "wda_call", skip_all, fields(call_id = NEXT_CALL.fetch_add(1, Ordering::Relaxed), method = method, route = path.split('?').next().unwrap_or(path)))]
    pub(super) async fn call(&self, method: &str, path: &str, body: Option<Value>, timeout: Duration) -> Result<Value> {
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

    pub(super) async fn get(&self, path: &str) -> Result<Value> {
        Ok(self.call("GET", path, None, DEFAULT_TIMEOUT).await?["value"].take())
    }

    pub(super) async fn session_id(&mut self) -> Result<String> {
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

    pub(super) async fn post(&mut self, path: &str, body: Value) -> Result<Value> {
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

    pub(super) async fn view_reusing(
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

    pub async fn active_app(&self) -> Result<String> {
        Ok(self.get("/wda/activeAppInfo").await?["bundleId"].as_str().unwrap_or_default().to_owned())
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
}
