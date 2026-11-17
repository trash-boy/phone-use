//! 截图（MJPEG 单帧或 `/screenshot`）和元素树。

use super::*;

impl<T: Transport> WdaClient<T> {
    /// Encoded screenshot bytes. The legacy method name is retained; low-latency sessions may return JPEG.
    pub async fn screenshot_png(&mut self) -> Result<Vec<u8>> {
        if self.mjpeg_capture() {
            self.session_id().await?;
        }
        self.capture_encoded().await
    }

    /// 原始分辨率的截图（`/screenshot`，PNG），不用缩小、压缩过的 MJPEG 帧。区域放大要从原始像素裁剪
    /// （docs/architecture.md §5）；只许用 MJPEG 的有界截图策略下不支持，报错而不是给一张糊的图
    pub async fn screenshot_full_resolution(&mut self) -> Result<Vec<u8>> {
        if self.bounded_capture {
            tracing::warn!(event = "capture_rejected", category = "full_resolution_forbidden");
            return Err(WdaError::Unavailable(FULL_RESOLUTION_UNSUPPORTED.into()));
        }
        if self.low_latency {
            self.session_id().await?;
        }
        self.png_screenshot().await
    }

    /// 判断画面动没动用的一帧：能用 MJPEG（9100 端口，和控制连接互不影响）就用它，
    /// 比整张 PNG 小得多、解码也快；不支持或读失败一次后就一直用普通截图
    pub async fn motion_frame(&mut self) -> Result<Vec<u8>> {
        if self.mjpeg_capture() {
            return self.screenshot_png().await;
        }
        if !self.mjpeg_motion_broken {
            match self.transport.screenshot_jpeg().await {
                Ok(Some(jpeg)) => return Ok(jpeg),
                Ok(None) => self.mjpeg_motion_broken = true,
                Err(e) => {
                    tracing::info!(event = "mjpeg_motion_unavailable", error = %e);
                    self.mjpeg_motion_broken = true;
                }
            }
        }
        self.png_screenshot().await
    }

    /// 截图和元素树 XML。`/source` 最多等 `source_budget`，截图有它自己的时限。
    /// - MJPEG 截图走自己的连接（9100 端口），和 `/source` 真正并行
    /// - 否则两者共用一条控制连接（请求排队），并行只会让 `/source` 排在约 11MB 的截图后面白白耗掉预算：
    ///   先截图，截完再开始算 `/source` 的预算
    pub async fn screenshot_with_source(&mut self, source_budget: Duration) -> (Result<Vec<u8>>, Result<String>) {
        if !self.mjpeg_capture() {
            let frame = self.png_screenshot().await;
            return (frame, self.source_within(source_budget).await);
        }
        if let Err(error) = self.session_id().await {
            let source = Err(WdaError::Unavailable(error.to_string()));
            return (Err(error), source);
        }
        let (jpeg, source) = tokio::join!(self.mjpeg_frame(), self.source_within(source_budget));
        let frame = match jpeg {
            Ok(Some(jpeg)) => Ok(jpeg),
            // 传输不支持 MJPEG：`/source` 已经读完，再走普通截图，不和它抢连接
            Ok(None) if !self.bounded_capture => self.png_screenshot().await,
            Ok(None) => Err(bounded_unsupported()),
            Err(e) => Err(e),
        };
        (frame, source)
    }

    /// 截图走 MJPEG（低延迟、有界截图）
    pub(super) fn mjpeg_capture(&self) -> bool {
        self.low_latency || self.bounded_capture
    }

    /// `/source`，HTTP 请求本身的时限就是 `budget`（超时的请求连同连接一起丢掉，不留在连接池里）
    pub(super) async fn source_within(&self, budget: Duration) -> Result<String> {
        self.source_xml_timeout(budget).await.map_err(|e| match e {
            WdaError::Timeout(_) => WdaError::Timeout("UI element snapshot budget exceeded".into()),
            other => other,
        })
    }

    /// One MJPEG frame; `None` when the transport has no MJPEG support. A failed read is an error and
    /// must not trigger a multi-megabyte PNG fallback.
    pub(super) async fn mjpeg_frame(&self) -> Result<Option<Vec<u8>>> {
        self.transport.screenshot_jpeg().await.map_err(|error| {
            tracing::warn!(event = "capture_rejected", category = "mjpeg_failed");
            WdaError::Unavailable(format!("MJPEG capture failed: {error}"))
        })
    }

    /// Encoded screenshot bytes; requires an existing session for MJPEG capture.
    pub(super) async fn capture_encoded(&self) -> Result<Vec<u8>> {
        if self.mjpeg_capture() {
            // WDA 16.15.3 converts /screenshot back to PNG even when the source
            // quality is JPEG. Capture one MJPEG frame before the WAN instead.
            if let Some(jpeg) = self.mjpeg_frame().await? {
                return Ok(jpeg);
            }
            if self.bounded_capture {
                return Err(bounded_unsupported());
            }
        }
        self.png_screenshot().await
    }

    /// `/screenshot`：原始分辨率 PNG
    pub(super) async fn png_screenshot(&self) -> Result<Vec<u8>> {
        // base64 约 11MB：从 JSON 里直接拿走字符串，原地去掉换行，不再另外复制
        let Value::String(mut b64) = self.get("/screenshot").await? else {
            return Err(WdaError::Unavailable(
                ictl_l10n::tr!("截图响应不是字符串", "screenshot response is not a string").into(),
            ));
        };
        let decoding = StepTimer::new("screenshot_base64_decode");
        let encoded_bytes = b64.len();
        b64.retain(|c| !c.is_whitespace());
        let decoded = base64::engine::general_purpose::STANDARD.decode(b64).map_err(|e| {
            WdaError::Unavailable(ictl_l10n::trf!("截图不是有效的 base64：{e}", "screenshot is not valid base64: {e}"))
        });
        tracing::info!(
            event = "screenshot_decoded",
            encoded_bytes,
            decoded_bytes = decoded.as_ref().map_or(0, |data| data.len())
        );
        decoding.finish(decoded.is_ok());
        decoded
    }

    /// 元素树 XML，排除可见性等慢属性（完整树 7～8 秒，排除后约 0.3 秒）
    pub async fn source_xml(&self) -> Result<String> {
        self.source_xml_timeout(SOURCE_TIMEOUT).await
    }

    /// 同 [`Self::source_xml`]，请求时限由调用方给（轮询时按剩下的预算）
    pub async fn source_xml_timeout(&self, timeout: Duration) -> Result<String> {
        let path = format!("/source?format=xml&excluded_attributes={}", elements::SOURCE_EXCLUDED);
        let v = self.call("GET", &path, None, timeout).await?;
        Ok(v["value"].as_str().unwrap_or_default().to_owned())
    }

    pub async fn elements(&mut self) -> Result<Vec<elements::Element>> {
        Ok(self.tree().await?.elements)
    }

    /// 元素树，带上「谁盖住谁」「有没有加载圈」等信息
    pub async fn tree(&mut self) -> Result<elements::Tree> {
        let view = self.view().await?;
        self.tree_in_view(view).await
    }
    /// Reuse orientation measured for the image in the same locked observation.
    pub async fn tree_in_view(&self, view: elements::View) -> Result<elements::Tree> {
        Ok(elements::parse_tree(&self.source_xml().await?, view))
    }

    /// 同 [`Self::tree_in_view`]，`/source` 最多等 `timeout`（报 [`WdaError::Timeout`]）
    pub async fn tree_in_view_within(&self, view: elements::View, timeout: Duration) -> Result<elements::Tree> {
        Ok(elements::parse_tree(&self.source_xml_timeout(timeout).await?, view))
    }
}
