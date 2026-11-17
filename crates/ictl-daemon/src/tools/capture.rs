//! 看界面：截图（WDA 优先，连不上时经 CoreDevice）、观察、元素树、等界面稳定、等元素出现、查系统弹窗。

use super::*;

pub(super) async fn capture_with_fallback<F, Fut>(
    policy: CapturePolicy,
    wda: Result<Vec<u8>, String>,
    fallback: F,
) -> Result<(Vec<u8>, &'static str), String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>, String>>,
{
    match wda {
        Ok(bytes) => Ok((bytes, "WDA")),
        Err(error) if policy == CapturePolicy::BoundedRemote => {
            tracing::warn!(event = "capture_rejected", category = "remote_fallback_forbidden");
            Err(error)
        }
        Err(_) => fallback().await.map(|bytes| (bytes, "HID")),
    }
}

pub(super) struct CapturedFrame {
    image: Response,
    view: Option<elements::View>,
    source_bytes: usize,
    source_format: &'static str,
}

/// 屏幕中间（上下各去掉一段，留给导航栏、标签栏）几乎没有元素：加载中、空白页
pub(super) fn middle_is_empty(tree: &Tree, screen_h: f64) -> bool {
    let (top, bottom) = ((screen_h * 0.25) as i64, (screen_h * 0.85) as i64);
    tree.elements.iter().filter(|e| e.center.1 > top && e.center.1 < bottom).count() <= 1
}

/// 轮询元素树直到连续两次一样（位置差不超过 [`STILL_TOLERANCE`]，且没有加载圈）或到 `max`。返回第一次读到的树，以及 (结果说明, 最后的树)；
/// 第一次读到的元素很少（游戏、地图）时第二项是 None，由调用方改看画面。
/// 每轮 `/source` 最多等剩下的时间（至少 [`MIN_POLL_ROUND`]）：一次读树可能要等很久，不限的话上限形同虚设。
/// 超时丢掉的只是读请求，不会改变界面
pub(super) async fn poll_stable(
    w: &mut Wda,
    t0: Instant,
    max: Duration,
) -> Result<(Tree, Option<(String, Tree)>), String> {
    // 等稳定期间方向不会变：只问一次
    let view = w.view().await.map_err(|e| e.to_string())?;
    let first = w.tree_in_view_within(view, poll_round(t0, max)).await.map_err(|e| e.to_string())?;
    if first.elements.len() < VISUAL_SETTLE_BELOW {
        return Ok((first, None));
    }
    let mut prev = first.clone();
    let msg = loop {
        if t0.elapsed() >= max {
            break if prev.loading {
                ictl_l10n::trf!(
                    "{:.1} 秒内页面一直在加载(有加载圈),按最后的状态返回;要等结果出来用 iphone_wait_for",
                    "The page kept loading for {:.1} s (spinner shown), returning the last state; to wait for the result use iphone_wait_for",
                    max.as_secs_f64()
                )
            } else {
                ictl_l10n::trf!(
                    "{:.1} 秒内界面一直在变(可能有动画),按最后的状态返回",
                    "The screen kept changing for {:.1} s (perhaps an animation), returning the last state",
                    max.as_secs_f64()
                )
            };
        }
        // 一次读树本身约 0.2 秒，两次之间不用再多等
        tokio::time::sleep(Duration::from_millis(30)).await;
        let cur = match w.tree_in_view_within(view, poll_round(t0, max)).await {
            Ok(cur) => cur,
            // 这一轮读到上限了：按上一次读到的返回（下一圈循环报「一直在变」）
            Err(WdaError::Timeout(_)) if t0.elapsed() >= max => continue,
            Err(e) => return Err(e.to_string()),
        };
        let same = cur.close_to(&prev, STILL_TOLERANCE);
        prev = cur;
        if same && !prev.loading {
            break ictl_l10n::trf!("界面已稳定({:.1} 秒)", "Screen settled ({:.1} s)", t0.elapsed().as_secs_f64());
        }
    };
    Ok((first, Some((msg, prev))))
}

/// 轮询时每轮读树至少给这么久（预算刚好用完时也能读完一次）
pub(super) const MIN_POLL_ROUND: Duration = Duration::from_secs(1);

/// 这一轮读树最多等多久：剩下的预算，至少 [`MIN_POLL_ROUND`]
pub(super) fn poll_round(t0: Instant, budget: Duration) -> Duration {
    budget.saturating_sub(t0.elapsed()).max(MIN_POLL_ROUND)
}

/// 判断画面动没动用的缩略图。解码是 CPU 活，放到阻塞线程
pub(super) async fn thumbnail(png: Vec<u8>) -> Result<Vec<u8>, String> {
    tokio::task::spawn_blocking(move || imaging::thumbnail(&png))
        .await
        .map_err(|e| ictl_l10n::trf!("图像处理任务异常：{e}", "Image processing task failed: {e}"))?
}

impl Daemon {
    pub(crate) async fn screenshot(
        &self,
        g: &mut Inner,
        region: Option<(f64, f64, f64, f64)>,
    ) -> Result<Response, String> {
        // 区域放大要从原始像素裁剪；有界截图策略只许用缩小、压缩过的 MJPEG 帧，裁出来是糊的：直接说不支持
        if region.is_some() && self.cfg.capture_policy == CapturePolicy::BoundedRemote {
            tracing::warn!(event = "capture_rejected", category = "region_under_bounded_policy");
            return Err(ictl_wda::client::FULL_RESOLUTION_UNSUPPORTED.into());
        }
        self.ensure_unlocked(g).await?;
        self.capture_frame(g, region).await.map(|frame| frame.image)
    }

    pub(super) async fn capture_frame(
        &self,
        g: &mut Inner,
        region: Option<(f64, f64, f64, f64)>,
    ) -> Result<CapturedFrame, String> {
        let fetching = StepTimer::new("screenshot_fetch");
        let wda_frame = match self.wda(g) {
            Some(w) => {
                // 区域放大从原始像素裁剪（docs/architecture.md §5）：不用缩小、压缩过的 MJPEG 帧
                let frame =
                    if region.is_some() { w.screenshot_full_resolution().await } else { w.screenshot_png().await };
                if let Err(e) = &frame {
                    self.note_wda_error(g, e);
                }
                frame.map_err(|error| error.to_string())
            }
            None => Err(wda_down(&g.state, g.info.as_ref())),
        };
        fetching.finish(wda_frame.is_ok());
        self.process_frame(g, region, wda_frame).await
    }

    /// Validates (or replaces through HID) a fetched WDA frame, then scales and encodes it.
    pub(super) async fn process_frame(
        &self,
        g: &mut Inner,
        region: Option<(f64, f64, f64, f64)>,
        wda_frame: Result<Vec<u8>, String>,
    ) -> Result<CapturedFrame, String> {
        // HID touches refresh rotation immediately before validating a fresh frame.
        // Keep that auxiliary query off the capture path.
        let (png, via) = capture_with_fallback(self.cfg.capture_policy, wda_frame, || async {
            if !hid_supported(g.info.as_ref()) {
                return Err(wda_down(&g.state, g.info.as_ref()));
            }
            self.hid_screenshot(g).await.map_err(|e| ictl_l10n::trf!("HID 命令失败: {e}", "HID command failed: {e}"))
        })
        .await?;
        let measuring = StepTimer::new("screenshot_view");
        // 截图横竖和最近问到的方向一致就不再问 `/orientation`（动作刚做完时这一问要等界面静止，0.3–1.5 秒）
        let frame = imaging::frame_size(&png);
        let view = match self.wda(g) {
            Some(w) => {
                let view = match frame {
                    Some((width, height)) => w.view_for_frame(width > height).await,
                    None => w.view().await,
                };
                if let Err(e) = &view {
                    self.note_wda_error(g, e);
                }
                view.ok()
            }
            None => None,
        };
        let screen = match view {
            Some(v) => v.screen,
            None => self.screen(g).await?,
        };
        measuring.finish(view.is_some());
        let source_bytes = png.len();
        let source_format = if png.starts_with(&[255, 216, 255]) {
            "jpeg"
        } else if png.starts_with(b"\x89PNG\r\n\x1a\n") {
            "png"
        } else {
            "other"
        };
        // 解码、缩放、JPEG 编码是 CPU 活（1290×2796），放到阻塞线程，不卡住主循环
        // 说明文字在阻塞线程里生成：带上这个请求的语言
        let lang = ictl_l10n::lang();
        let processing = StepTimer::new("image_process_and_encode");
        let processing_span = tracing::Span::current();
        let (jpeg, caption, coordinate_mapping) = tokio::task::spawn_blocking(move || {
            let _span = processing_span.enter();
            let work = StepTimer::new("image_decode_resize_jpeg");
            let output = ictl_l10n::with_lang(lang, || match region {
                None => imaging::full_screen(&png, screen, via),
                Some(r) => imaging::region(&png, screen, r, via),
            });
            work.finish(output.is_ok());
            output
        })
        .await
        .map_err(|e| ictl_l10n::trf!("图像处理任务异常：{e}", "Image processing task failed: {e}"))??;
        processing.finish(true);
        let encoding = StepTimer::new("image_response_base64_encode");
        let jpeg_base64 = base64::engine::general_purpose::STANDARD.encode(&jpeg);
        tracing::info!(
            event = "capture_complete",
            source_bytes,
            source_format,
            jpeg_bytes = jpeg.len(),
            base64_bytes = jpeg_base64.len()
        );
        encoding.finish(true);
        Ok(CapturedFrame {
            image: Response::Image { jpeg_base64, caption, coordinate_mapping: Some(coordinate_mapping) },
            view,
            source_bytes,
            source_format,
        })
    }

    pub(crate) async fn observe(&self, g: &mut Inner, limit: usize, budget: Duration) -> Result<Response, String> {
        let started = Instant::now();
        self.ensure_unlocked(g).await?;
        let wants_elements = limit > 0 && !budget.is_zero();
        // 刚滑动停稳时读到的树就是当前界面：直接用，省一次 `/source`
        let settled = if wants_elements {
            g.settled_tree.take().filter(|(at, _)| at.elapsed() < SETTLED_TREE_REUSE)
        } else {
            None
        };
        // Otherwise read the tree while the frame is captured, not after it.
        let mut source = None;
        let captured = match self.wda(g) {
            Some(w) if wants_elements && settled.is_none() => {
                let fetching = StepTimer::new("screenshot_fetch");
                let (frame, xml) = w.screenshot_with_source(budget).await;
                fetching.finish(frame.is_ok());
                if let Err(e) = &frame {
                    self.note_wda_error(g, e);
                }
                source = Some(xml);
                self.process_frame(g, None, frame.map_err(|error| error.to_string())).await
            }
            _ => self.capture_frame(g, None).await,
        };
        let CapturedFrame { image: frame, view, source_bytes, source_format } = captured?;
        let Response::Image { jpeg_base64, caption, coordinate_mapping } = frame else { return Ok(frame) };
        let mut observation = serde_json::json!({
            "deviceId": g.device.as_ref().map(|d| d.udid.as_str()),
            "imageBase64":jpeg_base64,"caption":caption,"coordinateMapping":coordinate_mapping,"elementsText":"",
            "capturedAt":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis(),
            "timings":{"screenshotMs":started.elapsed().as_millis() as u64,"sourceBytes":source_bytes,"sourceFormat":source_format}
        });
        let reading = Instant::now();
        if wants_elements {
            let read = async {
                let tree = match (settled, source) {
                    (Some((_, tree)), _) => tree,
                    (None, Some(xml)) => {
                        let xml = xml.map_err(|e| e.to_string())?;
                        let view = match view {
                            Some(view) => view,
                            None => self.require_wda(g)?.view().await.map_err(|e| e.to_string())?,
                        };
                        elements::parse_tree(&xml, view)
                    }
                    (None, None) => {
                        let wda = self.require_wda(g)?;
                        let view = match view {
                            Some(view) => view,
                            None => wda.view().await.map_err(|e| e.to_string())?,
                        };
                        wda.tree_in_view(view).await.map_err(|e| e.to_string())?
                    }
                };
                let text = tree.elements.iter().take(limit).map(|e| e.describe()).collect::<Vec<_>>().join("\n");
                g.elements_cache = Some((Instant::now(), tree));
                Ok::<_, String>(text)
            };
            match tokio::time::timeout(budget, read).await {
                Ok(Ok(text)) => observation["elementsText"] = serde_json::json!(text),
                _ => observation["elementsError"] = serde_json::json!("UI elements unavailable"),
            }
        }
        observation["timings"]["elementsMs"] = serde_json::json!(reading.elapsed().as_millis() as u64);
        Ok(Response::text(observation.to_string()))
    }

    /// 当前界面的元素树。缓存不超过 `max_age` 就直接用（0 = 总是重读）；重读的结果写回缓存
    pub(super) async fn tree(&self, g: &mut Inner, max_age: Duration) -> Result<Tree, String> {
        if !max_age.is_zero()
            && let Some((t, tree)) = &g.elements_cache
            && t.elapsed() <= max_age
        {
            return Ok(tree.clone());
        }
        let tree = self.require_wda(g)?.tree().await.map_err(|e| e.to_string())?;
        g.elements_cache = Some((Instant::now(), tree.clone()));
        Ok(tree)
    }

    pub(crate) async fn list_elements(
        &self,
        g: &mut Inner,
        filter: &str,
        limit: usize,
        max_age: Duration,
        snapshot_timeout: Duration,
    ) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        let tree = if snapshot_timeout.is_zero() {
            self.tree(g, max_age).await?
        } else {
            // Bound the device read itself; closing a caller socket only cancels queued
            // daemon work and would otherwise leave this read holding the device lock.
            tokio::time::timeout(snapshot_timeout, self.tree(g, max_age))
                .await
                .map_err(|_| "UI element snapshot budget exceeded".to_owned())??
        };
        let els = tree.elements;
        let shown: Vec<&elements::Element> =
            if filter.is_empty() { els.iter().collect() } else { elements::filter(&els, filter) };
        let lines: Vec<_> = shown.iter().take(limit).map(|e| e.describe()).collect();
        let more = if shown.len() > limit {
            ictl_l10n::trf!(
                "\n……另有 {} 个未列出,可用 filter 缩小范围",
                "\n…{} more not listed, narrow it down with filter",
                shown.len() - limit
            )
        } else {
            String::new()
        };
        Ok(if lines.is_empty() { no_elements_text(filter, els.len()) } else { lines.join("\n") } + &more)
    }

    /// 等界面稳定，最多等 `max`：
    /// - 有元素树的界面：连续两次读到的元素一样，并且没有加载圈。轮询期间让 WDA 读树时不等界面空闲、动画冷却
    ///   （否则滑完第一次读要等惯性滚动停下再冷却 2 秒，页面加载时一次能卡好几秒，超过上限很多）
    /// - 元素很少的界面（游戏、地图，画面变化不反映在元素树里）：改看画面，连续两帧缩略图几乎一样。
    ///   截图约 0.2 秒一张，两帧之间再隔一点，避免两帧落在同一段静止的过渡里
    ///
    /// 屏幕上有弹窗时在结果里提醒（读树时顺带看，不多花时间）
    pub(crate) async fn settle(&self, g: &mut Inner, max: Duration) -> Result<String, String> {
        let t0 = Instant::now();
        let Some(w) = self.wda(g) else {
            tokio::time::sleep(max.min(Duration::from_millis(500))).await;
            return Ok(ictl_l10n::tr!("WDA 不可用,等了 0.5 秒", "WDA unavailable, waited 0.5 s").into());
        };
        w.begin_polling().await.map_err(|e| e.to_string())?;
        let r = poll_stable(w, t0, max).await;
        w.end_polling().await;
        let (first, polled) = r?;
        let Some((msg, tree)) = polled else {
            // 元素很少：改看画面
            let note = alert_note(&first);
            return Ok(self.settle_visually(g, t0, max).await? + &note);
        };
        let note = alert_note(&tree);
        // 自己画的加载圈（不是 ActivityIndicator）不在树里，树稳定了页面可能还在加载。
        // 加载中的页面中间是空的：只在这种时候多看一眼画面（约 0.5 秒），平常不多花时间
        let screen = self.screen(g).await?;
        let msg = if msg.starts_with(ictl_l10n::tr!("界面已稳定", "Screen settled"))
            && middle_is_empty(&tree, screen.height)
        {
            msg + &self.motion_note(g).await
        } else {
            msg
        };
        // 紧接着的 then=elements 直接用这棵树
        g.elements_cache = Some((Instant::now(), tree));
        Ok(msg + &note)
    }

    pub(super) async fn settle_visually(&self, g: &mut Inner, t0: Instant, max: Duration) -> Result<String, String> {
        let deadline = t0 + max;
        let w = self.require_wda(g)?;
        let mut prev: Option<Vec<u8>> = None;
        loop {
            // MJPEG 帧（有就用）比整张 PNG 小得多，解码也快；只用来比画面动没动
            let frame = w.motion_frame().await.map_err(|e| e.to_string())?;
            let cur = thumbnail(frame).await?;
            if let Some(p) = &prev {
                let m = imaging::thumb_motion(p, &cur);
                if m.mean < VISUAL_STILL {
                    let local = if m.max_tile >= LOCAL_MOTION && imaging::mostly_blank(&cur) {
                        ictl_l10n::tr!(
                            ";但有一小块还在动(可能是加载圈或小动画),内容可能还没加载完,要等结果用 iphone_wait_for 或稍后再截图",
                            "; but a small area is still moving (perhaps a spinner or a small animation), content may still be loading; to wait for a result use iphone_wait_for or take a screenshot later"
                        )
                    } else {
                        ""
                    };
                    return Ok(ictl_l10n::trf!(
                        "画面已静止({:.1} 秒){local}",
                        "Picture still ({:.1} s){local}",
                        t0.elapsed().as_secs_f64()
                    ));
                }
            }
            // 不够再截一张（约 0.2 秒）、隔一下再比的时间了
            if deadline.saturating_duration_since(Instant::now()) < Duration::from_millis(300) {
                return Ok(ictl_l10n::trf!(
                    "{:.1} 秒内画面一直在动,按最后的状态返回",
                    "The picture kept moving for {:.1} s, returning the last state",
                    max.as_secs_f64()
                ));
            }
            prev = Some(cur);
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    /// 隔一下截两张图，画面还在动就返回提醒，否则返回空串
    pub(super) async fn motion_note(&self, g: &mut Inner) -> String {
        let Some(w) = self.wda(g) else { return String::new() };
        let mut thumbs = Vec::new();
        for i in 0..2 {
            if i > 0 {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            let Ok(frame) = w.motion_frame().await else { return String::new() };
            let Ok(t) = thumbnail(frame).await else { return String::new() };
            thumbs.push(t);
        }
        let m = imaging::thumb_motion(&thumbs[0], &thumbs[1]);
        if (m.mean >= VISUAL_STILL || m.max_tile >= LOCAL_MOTION) && imaging::mostly_blank(&thumbs[1]) {
            ictl_l10n::tr!(";但画面中间是空的、还有东西在动(可能还在加载),要等结果用 iphone_wait_for 或稍后再截图", "; but the middle of the screen is empty and something is still moving (perhaps still loading); to wait for a result use iphone_wait_for or take a screenshot later").into()
        } else {
            String::new()
        }
    }

    /// 前台是 SpringBoard 时查有没有系统弹窗（权限请求等），有就返回提醒，没有返回空串。
    /// 不带 then 的操作做完后调：弹窗挡着时点击、滑动都会落在弹窗上，WDA 却照样报成功。
    /// 前台是 App 时只多一次约 0.13 秒的查询；在主屏上再多约 0.4 秒
    pub(crate) async fn alert_check(&self, g: &mut Inner) -> Result<String, String> {
        let Some(w) = self.wda(g) else { return Ok(String::new()) };
        if w.active_app().await.map_err(|e| e.to_string())? != SPRINGBOARD {
            return Ok(String::new());
        }
        Ok(match w.alert_text().await {
            Ok(Some(text)) => alert_msg(&text),
            _ => String::new(),
        })
    }

    /// 等包含 `text` 的元素出现
    pub(crate) async fn wait_for(&self, g: &mut Inner, text: &str, timeout: Duration) -> Result<String, String> {
        self.ensure_unlocked(g).await?;
        let t0 = Instant::now();
        let w = self.require_wda(g)?;
        // 轮询时不等界面空闲：页面还在加载、播放动画时每次读树也只要约 0.2 秒
        w.begin_polling().await.map_err(|e| e.to_string())?;
        let r = async {
            // 一次等待里方向不会变：只问一次，之后每轮只拉元素树
            let view = w.view().await.map_err(|e| e.to_string())?;
            loop {
                // 每轮 `/source` 最多等剩下的预算（不然一次就能等 60 秒，上限形同虚设）
                let round = poll_round(t0, timeout);
                let els = match w.tree_in_view_within(view, round).await {
                    Ok(tree) => tree.elements,
                    Err(WdaError::Timeout(_)) if t0.elapsed() >= timeout => Vec::new(),
                    Err(e) => return Err(e.to_string()),
                };
                // 不算输入框里的字：刚输入的搜索词不能算「结果出来了」
                if let Some(e) = elements::find_shown(&els, text).first() {
                    return Ok(ictl_l10n::trf!("已出现 {}({:.1} 秒)", "Appeared: {} ({:.1} s)", e.describe(), t0.elapsed().as_secs_f64()));
                }
                if t0.elapsed() >= timeout {
                    return Err(ictl_l10n::trf!("{:.0} 秒内没出现包含 \"{text}\" 的元素,截图看看屏幕", "No element containing \"{text}\" appeared within {:.0} s; take a screenshot to look at the screen", timeout.as_secs_f64()));
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
        .await;
        w.end_polling().await;
        r
    }
}
