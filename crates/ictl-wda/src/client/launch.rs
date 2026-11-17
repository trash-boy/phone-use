//! 启动、结束 App，打开网址，并等它到前台、画出来。

use super::*;

impl<T: Transport> WdaClient<T> {
    /// 启动并等到前台、界面稳定（冷启动后立即点击会被吞），最多等 8 秒。
    /// 等待期间查前台出错不算失败（启动命令已经成功了）
    pub async fn launch_app(&mut self, bundle_id: &str) -> Result<Launch> {
        let started = tokio::time::Instant::now();
        self.in_session("POST", "/wda/apps/launch", Some(json!({"bundleId": bundle_id})), Duration::from_secs(60))
            .await?;
        let t0 = tokio::time::Instant::now();
        let deadline = t0 + Duration::from_secs(8);
        let mut foreground = false;
        while tokio::time::Instant::now() < deadline {
            match self.active_app().await {
                Ok(a) if a == bundle_id => {
                    foreground = true;
                    break;
                }
                // 系统弹窗（权限请求）挡着时前台一直是 SpringBoard，原来要白等满 8 秒。
                // 刚启动的一瞬间前台也还是 SpringBoard，过 1 秒还是才去查弹窗（查一次约 0.4 秒）
                Ok(a) if a == SPRINGBOARD && bundle_id != SPRINGBOARD && t0.elapsed() >= Duration::from_secs(1) => {
                    if let Ok(Some(text)) = self.alert_text().await {
                        return Ok(Launch::Blocked(text));
                    }
                }
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        if !foreground {
            return Ok(match self.alert_text().await {
                Ok(Some(text)) => Launch::Blocked(text),
                _ => Launch::Slow,
            });
        }
        self.settle_launched(started).await;
        Ok(Launch::Foreground)
    }

    /// App 到了前台之后再等它画出来。`started`：发出启动请求的时刻
    pub(super) async fn settle_launched(&mut self, started: tokio::time::Instant) {
        // 冷启动后立即点击会被吞（回归用例 #13）。原来固定再等 1 秒；改为等界面稳定：连续两次元素树一样
        // （读元素树时 WDA 本身也等界面空闲），最多仍是 1 秒
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let mut prev = None;
        while tokio::time::Instant::now() < deadline {
            let cur = self.source_xml().await.ok();
            if cur.is_some() && cur == prev {
                break;
            }
            prev = cur;
        }
        // 切回已在后台的 App 时元素树马上就稳定了（约 0.4 秒），放大进场的动画还没完，这时点击会被吞（真机：小红书、高德各一次）
        if let Some(left) = LAUNCH_MIN.checked_sub(started.elapsed()) {
            tokio::time::sleep(left).await;
        }
    }

    pub async fn terminate_app(&mut self, bundle_id: &str) -> Result<()> {
        self.post("/wda/apps/terminate", json!({"bundleId": bundle_id})).await.map(drop)
    }

    /// 打开网址或 URL Scheme，等打开它的 App 到前台、画出来再返回（最多约 8 秒），同 [`Self::launch_app`]。
    /// 不等的话 App 冷启动时还是黑屏，紧接着截图会把两帧黑屏当成「画面已静止」（iPad Safari 真机）。
    /// 返回结果和前台 App；在原来的前台 App 里打开（Safari 里开网址）时等 1 秒没换就算打开了
    pub async fn open_url(&mut self, url: &str) -> Result<(Launch, String)> {
        let started = tokio::time::Instant::now();
        let before = self.active_app().await.unwrap_or_default();
        self.in_session("POST", "/url", Some(json!({"url": url})), Duration::from_secs(60)).await?;
        let deadline = started + Duration::from_secs(8);
        // 前台要连续两次是同一个才算：Safari 冷启动的一瞬间前台是 `com.apple.DocumentManager.DockFolderViewService`（iPad 真机）
        let mut last = String::new();
        loop {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let waited = started.elapsed() >= Duration::from_secs(1);
            let now = self.active_app().await;
            let steady = now.as_ref().is_ok_and(|a| *a == last);
            if let Ok(a) = &now {
                last.clone_from(a);
            }
            match now {
                Ok(a) if a != before && a != SPRINGBOARD && steady => {
                    self.settle_launched(started).await;
                    return Ok((Launch::Foreground, a));
                }
                Ok(a) if a == before && a != SPRINGBOARD && waited => {
                    self.settle_launched(started).await;
                    return Ok((Launch::Foreground, a));
                }
                // 「要在某 App 中打开吗」之类的系统弹窗挡着时前台一直是 SpringBoard
                Ok(a) if a == SPRINGBOARD && waited => {
                    if let Ok(Some(text)) = self.alert_text().await {
                        return Ok((Launch::Blocked(text), a));
                    }
                }
                _ => {}
            }
            if tokio::time::Instant::now() >= deadline {
                let now = self.active_app().await.unwrap_or_default();
                return Ok((Launch::Slow, now));
            }
        }
    }
}
