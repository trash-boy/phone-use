//! 发往 WDA 的极简 HTTP/1.1。
//!
//! WDA 直接经 usbmuxd 连手机上的端口，本机不监听任何端口（docs/decisions.md D3）。
//! idevice 自带的 `WdaClient` 不支持 `excluded_attributes`、appium settings、W3C actions 等，
//! 所以这里自己发请求，按响应边界复用连接；取消、超时或不完整响应会丢弃连接。

use crate::timing::StepTimer;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

static NEXT_HTTP: AtomicU64 = AtomicU64::new(1);

struct HttpTrace {
    started: Instant,
    phase: Option<StepTimer>,
    phase_name: &'static str,
    outcome: &'static str,
    attempt: usize,
    first_byte_ms: Option<u64>,
    response_bytes: usize,
    header_bytes: usize,
    read_count: usize,
    max_read_wait_ms: u64,
    status: Option<u16>,
    read_started: Option<Instant>,
    dispatch: tracing::Dispatch,
    span: tracing::Span,
}

impl HttpTrace {
    fn new(request_bytes: usize) -> Self {
        tracing::info!(event = "http_started", request_bytes);
        Self {
            started: Instant::now(),
            phase: Some(StepTimer::new("http_pool_wait")),
            phase_name: "http_pool_wait",
            outcome: "cancelled",
            attempt: 0,
            first_byte_ms: None,
            response_bytes: 0,
            header_bytes: 0,
            read_count: 0,
            max_read_wait_ms: 0,
            status: None,
            read_started: None,
            dispatch: tracing::dispatcher::get_default(Clone::clone),
            span: tracing::Span::current(),
        }
    }

    fn phase(&mut self, name: &'static str) {
        if let Some(previous) = self.phase.take() {
            previous.finish(true);
        }
        self.phase_name = name;
        self.phase = Some(StepTimer::new(name));
    }

    fn read(&mut self, bytes: usize, wait_ms: u64) {
        self.max_read_wait_ms = self.max_read_wait_ms.max(wait_ms);
        self.read_count += 1;
        self.response_bytes += bytes;
        if bytes > 0 && self.first_byte_ms.is_none() {
            let elapsed = self.started.elapsed().as_millis() as u64;
            self.first_byte_ms = Some(elapsed);
            tracing::info!(event = "http_first_byte", attempt = self.attempt, first_byte_ms = elapsed);
            self.phase("http_headers_read");
        }
        tracing::debug!(
            event = "http_read",
            attempt = self.attempt,
            bytes,
            wait_ms,
            received_bytes = self.response_bytes,
            read_count = self.read_count
        );
    }
}

impl Drop for HttpTrace {
    fn drop(&mut self) {
        let dispatch = self.dispatch.clone();
        let span = self.span.clone();
        tracing::dispatcher::with_default(&dispatch, || {
            span.in_scope(|| {
                if self.outcome != "cancelled"
                    && let Some(phase) = self.phase.take()
                {
                    phase.finish(self.outcome == "ok");
                }
                tracing::info!(event = "http_complete", outcome = self.outcome, attempt = self.attempt,
            elapsed_ms = self.started.elapsed().as_millis() as u64, phase = self.phase_name,
            first_byte_ms = ?self.first_byte_ms, wire_response_bytes = self.response_bytes,
            response_body_bytes = self.response_bytes.saturating_sub(self.header_bytes),
            read_count = self.read_count, max_read_wait_ms = self.max_read_wait_ms,
            pending_read_ms = ?self.read_started.map(|at| at.elapsed().as_millis() as u64), status = ?self.status);
            })
        });
    }
}

use idevice::provider::IdeviceProvider;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 一次 HTTP 请求的传输。抽象出来便于单测里换成假的。
///
/// 错误的 kind 区分请求有没有送到 WDA（决定能不能退到 HID 重做）：
/// [`ErrorKind::TimedOut`](std::io::ErrorKind::TimedOut) 是发出后没在时限内应答，
/// [`AFTER_SENT`] 是发出后连接断了或响应不完整、看不懂；其他 kind 都表示请求没送到
pub trait Transport: Send + Sync {
    /// An optional one-frame JPEG capture, independent of the control HTTP pool.
    /// Unsupported test/custom transports retain the ordinary screenshot path.
    fn screenshot_jpeg(&self) -> impl Future<Output = std::io::Result<Option<Vec<u8>>>> + Send {
        async { Ok(None) }
    }

    /// Read-only liveness probe. Device transports reserve a separate connection
    /// and give the response its own budget after connecting.
    fn health(&self) -> impl Future<Output = std::io::Result<(u16, Vec<u8>)>> + Send {
        self.send("GET", "/status", None, Duration::from_secs(15))
    }

    fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
        timeout: Duration,
    ) -> impl Future<Output = std::io::Result<(u16, Vec<u8>)>> + Send;
}

/// 经 usbmuxd 直连手机上 WDA 端口
#[derive(Clone)]
pub struct DeviceTransport {
    provider: Arc<dyn IdeviceProvider>,
    port: u16,
    idle: Arc<Mutex<Option<IdleConnection>>>,
    health_idle: Arc<Mutex<Option<IdleConnection>>>,
}

struct IdleConnection {
    socket: Box<dyn idevice::ReadWrite>,
    at: Instant,
}

impl DeviceTransport {
    pub fn new(provider: Arc<dyn IdeviceProvider>, port: u16) -> Self {
        Self { provider, port, idle: Arc::new(Mutex::new(None)), health_idle: Arc::new(Mutex::new(None)) }
    }
}

/// 建立到 WDA 端口的连接的时限。连不上（或连接超时）算 WDA 不可用，调用方可以退到 HID；
/// 连上之后的超时是「请求已发出、手机可能还在执行」，不能退到 HID 重做一遍
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 响应（含头部）的大小上限
const MAX_RESPONSE: usize = 32 * 1024 * 1024;

/// 请求已经整个发出、之后读响应出错的 kind：WDA 可能已经执行了这个操作
pub const AFTER_SENT: std::io::ErrorKind = std::io::ErrorKind::ConnectionAborted;

fn after_sent(e: std::io::Error) -> std::io::Error {
    std::io::Error::new(AFTER_SENT, e.to_string())
}

impl Transport for DeviceTransport {
    async fn screenshot_jpeg(&self) -> std::io::Result<Option<Vec<u8>>> {
        let started = Instant::now();
        let capture = async {
            let mut socket = self
                .provider
                .connect(9100)
                .await
                .map_err(std::io::Error::other)?
                .get_socket()
                .ok_or_else(|| std::io::Error::other("MJPEG socket unavailable"))?;
            crate::mjpeg::capture(&mut *socket).await
        };
        let result = tokio::time::timeout(Duration::from_secs(8), capture)
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "MJPEG frame capture timed out"))
            .and_then(|result| result);
        tracing::info!(
            event = "mjpeg_capture",
            elapsed_ms = started.elapsed().as_millis() as u64,
            frame_bytes = result.as_ref().map_or(0, |frame| frame.len()),
            outcome = if result.is_ok() { "ok" } else { "error" }
        );
        result.map(Some)
    }

    async fn health(&self) -> std::io::Result<(u16, Vec<u8>)> {
        let transport = Self { idle: self.health_idle.clone(), ..self.clone() };
        transport.request("GET", "/status", None, Duration::from_secs(15), Some(Duration::from_secs(5))).await
    }

    async fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
        timeout: Duration,
    ) -> std::io::Result<(u16, Vec<u8>)> {
        self.request(method, path, body, timeout, None).await
    }
}

impl DeviceTransport {
    #[tracing::instrument(name = "wda_http", skip_all, fields(http_id = NEXT_HTTP.fetch_add(1, Ordering::Relaxed), native_pid = std::process::id(), method = method, route = path.split('?').next().unwrap_or(path)))]
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
        timeout: Duration,
        response_timeout: Option<Duration>,
    ) -> std::io::Result<(u16, Vec<u8>)> {
        let started = Instant::now();
        let req = build_request(method, path, body.as_deref());
        tracing::info!(event = "http_budget", health = response_timeout.is_some(), total_budget_ms = timeout.as_millis() as u64,
            response_budget_ms = ?response_timeout.map(|t| t.as_millis() as u64));
        let mut trace = HttpTrace::new(req.len());
        let mut stage = "pool";
        let mut write_started = false;
        let mut reused_connection = false;
        let mut timed_out = false;
        let operation = async {
            let mut idle = self.idle.lock().await;
            // Take the socket OUT of the pool before awaiting. Dropping this future
            // after cancellation cannot return a half-consumed response to the pool.
            let mut connection = idle.take().filter(|c| c.at.elapsed() < Duration::from_secs(15));
            if let Some(c) = connection.as_mut() {
                stage = "probe";
                trace.phase("http_reuse_probe");
                let mut probe = [0u8; 1];
                // Detect an already-closed idle socket before sending a mutation.
                // An uncertain failure after writing still never replays a mutation.
                if tokio::time::timeout(Duration::from_millis(1), c.socket.read(&mut probe)).await.is_ok() {
                    connection = None;
                }
            }
            let reused = connection.is_some();
            reused_connection = reused;
            for attempt in 0..2 {
                trace.attempt = attempt + 1;
                trace.first_byte_ms = None;
                trace.response_bytes = 0;
                trace.header_bytes = 0;
                trace.read_count = 0;
                tracing::info!(
                    event = "http_attempt",
                    attempt = trace.attempt,
                    reused = reused_connection && attempt == 0
                );
                let mut socket = match connection.take() {
                    Some(c) => c.socket,
                    None => {
                        stage = "connect";
                        trace.phase("http_connect");
                        tokio::time::timeout(CONNECT_TIMEOUT, self.provider.connect(self.port))
                            .await
                            .map_err(|_| {
                                timed_out = true;
                                std::io::Error::new(std::io::ErrorKind::NotConnected, "WDA connection timed out")
                            })?
                            .map_err(|e| std::io::Error::new(std::io::ErrorKind::NotConnected, e.to_string()))?
                            .get_socket()
                            .ok_or_else(|| {
                                std::io::Error::new(std::io::ErrorKind::NotConnected, "WDA socket unavailable")
                            })?
                    }
                };
                // From the first write onward a mutation may have reached WDA.
                // Keep this true even if a GET retries its stale connection.
                write_started = true;
                let response = if let Some(budget) = response_timeout {
                    match tokio::time::timeout(budget, exchange(&mut *socket, &req, &mut stage, &mut trace)).await {
                        Ok(result) => result,
                        Err(_) => {
                            timed_out = true;
                            return Err(std::io::ErrorKind::TimedOut.into());
                        }
                    }
                } else {
                    exchange(&mut *socket, &req, &mut stage, &mut trace).await
                };
                match response {
                    Ok((status, body, reusable)) => {
                        if reusable {
                            *idle = Some(IdleConnection { socket, at: Instant::now() });
                        }
                        return Ok((status, body));
                    }
                    // A stale persistent connection may be retried ONLY for GET,
                    // under the original deadline. POST/actions are never replayed.
                    Err(_) if reused && attempt == 0 && method == "GET" => {}
                    Err(error) => return Err(error),
                }
            }
            unreachable!()
        };
        let short = path.split('?').next().unwrap_or(path);
        let mut result = match tokio::time::timeout(timeout, operation).await {
            Ok(result) => result,
            Err(_) => {
                timed_out = true;
                Err(std::io::ErrorKind::TimedOut.into())
            }
        };
        if timed_out {
            let kind = if write_started { std::io::ErrorKind::TimedOut } else { std::io::ErrorKind::NotConnected };
            let detail = format!(
                "WDA request timed out: {method} {short}, stage={stage}, reused={reused_connection}, elapsed={:.2}s",
                started.elapsed().as_secs_f64()
            );
            tracing::warn!("{detail}");
            result = Err(std::io::Error::new(kind, detail));
        }
        // Failure before a health response starts is link/dispatch uncertainty,
        // not evidence that a live XCTest process should be terminated.
        if response_timeout.is_some() && stage != "read" {
            result = result.map_err(|e| std::io::Error::new(std::io::ErrorKind::NotConnected, e.to_string()));
        }
        tracing::debug!(
            "wda {method} {short} ({:.2}s), stage={stage}, reused={reused_connection}, ok={}",
            started.elapsed().as_secs_f64(),
            result.is_ok()
        );
        trace.outcome = if result.is_ok() {
            "ok"
        } else if timed_out {
            "timeout"
        } else {
            "error"
        };
        result
    }
}

async fn exchange<S: AsyncReadExt + AsyncWriteExt + Unpin + ?Sized>(
    socket: &mut S,
    request: &[u8],
    stage: &mut &'static str,
    trace: &mut HttpTrace,
) -> std::io::Result<(u16, Vec<u8>, bool)> {
    // Once writing starts, classify failure as uncertain, including flush errors.
    // The native engine must not fall back to HID and duplicate a sent action.
    *stage = "write";
    trace.phase("http_write");
    socket.write_all(request).await.map_err(after_sent)?;
    *stage = "flush";
    trace.phase("http_flush");
    socket.flush().await.map_err(after_sent)?;
    *stage = "read";
    trace.phase("http_first_byte_wait");
    let mut buffer = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    let mut head = None;
    loop {
        if head.is_none() {
            head = parse_head(&buffer);
            if let Some((split, framing)) = head {
                // 正文定长时一次分配够（截图约 11MB，按 64KB 一块地追加会反复扩容、复制）；声明的长度超过上限的不信
                if let Framing::Length(n) = framing {
                    let want = split.saturating_add(n).min(MAX_RESPONSE);
                    buffer.reserve(want.saturating_sub(buffer.len()));
                }
                trace.header_bytes = split;
                tracing::info!(event = "http_headers", header_bytes = split, framing = ?framing);
                trace.phase("http_body_read");
            }
            if head.is_none() && buffer.len() >= 64 * 1024 {
                return Err(after_sent(std::io::Error::other("WDA response headers too large")));
            }
        }
        if let Some((split, framing)) = head
            && framing.complete(&buffer[split..])
        {
            break;
        }
        trace.read_started = Some(Instant::now());
        let read = socket.read(&mut chunk).await;
        let wait_ms = trace.read_started.take().unwrap().elapsed().as_millis() as u64;
        let n = read.map_err(after_sent)?;
        trace.read(n, wait_ms);
        if n == 0 {
            break;
        }
        if buffer.len() + n > MAX_RESPONSE {
            return Err(after_sent(std::io::Error::other("WDA response too large")));
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
    let reusable = head.is_some_and(|(split, framing)| {
        let headers = String::from_utf8_lossy(&buffer[..split]).to_ascii_lowercase();
        let exact = match framing {
            Framing::Length(n) => split.checked_add(n) == Some(buffer.len()),
            Framing::Chunked => framing.complete(&buffer[split..]),
            Framing::UntilEof => false,
        };
        exact
            && headers.starts_with("http/1.1 ")
            && !headers.lines().any(|line| {
                line.split_once(':').is_some_and(|(key, value)| {
                    key.trim() == "connection" && value.split(',').any(|token| token.trim() == "close")
                })
            })
    });
    trace.phase("http_response_parse");
    let (status, body) = parse_response(buffer).map_err(after_sent)?;
    trace.status = Some(status);
    tracing::info!(event = "http_body_decoded", response_body_bytes = body.len());
    Ok((status, body, reusable))
}

pub(crate) fn build_request(method: &str, path: &str, body: Option<&[u8]>) -> Vec<u8> {
    let body = body.unwrap_or_default();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\nAccept: application/json\r\n"
    );
    if method != "GET" || !body.is_empty() {
        req.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    let mut out = req.into_bytes();
    out.extend_from_slice(body);
    out
}

/// 响应正文怎么定长
#[derive(Debug, Clone, Copy, PartialEq)]
enum Framing {
    Length(usize),
    Chunked,
    /// 两者都没有：只能读到对方关连接
    UntilEof,
}

impl Framing {
    fn complete(self, body: &[u8]) -> bool {
        match self {
            Self::Length(n) => body.len() >= n,
            // 先看结尾是不是结束块，是了才完整解一遍（避免每读 64KB 就把整个正文解一遍）
            Self::Chunked => body.ends_with(b"0\r\n\r\n") && dechunk(body).is_some(),
            Self::UntilEof => false,
        }
    }
}

/// 头部收齐了就返回 (正文起点, 定长方式)。头部很短，只在前 64KB 里找结束标记
fn parse_head(raw: &[u8]) -> Option<(usize, Framing)> {
    let window = &raw[..raw.len().min(64 * 1024)];
    let split = window.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
    let mut framing = Framing::UntilEof;
    for l in head.split("\r\n").skip(1) {
        let Some((k, v)) = l.split_once(':') else { continue };
        match (k.trim(), v.trim()) {
            ("content-length", n) => {
                if let Ok(n) = n.parse() {
                    framing = Framing::Length(n);
                }
            }
            ("transfer-encoding", "chunked") => return Some((split + 4, Framing::Chunked)),
            _ => {}
        }
    }
    Some((split + 4, framing))
}

/// 已经收到一个完整的响应了吗：头部结束，并且正文按 Content-Length 收齐或 chunked 收到结束块。
/// 两者都没有时只能等对方关连接（返回 false 继续读）。读循环里直接用 [`parse_head`] + [`Framing::complete`]
#[cfg(test)]
fn response_complete(raw: &[u8]) -> bool {
    parse_head(raw).is_some_and(|(split, f)| f.complete(&raw[split..]))
}

/// 解析完整的 HTTP 响应：状态码和正文。支持 Content-Length、chunked 和读到 EOF 三种正文。
/// 拿走缓冲区，正文原地截取不再复制（截图正文约 11MB）
pub(crate) fn parse_response(raw: impl Into<Vec<u8>>) -> std::io::Result<(u16, Vec<u8>)> {
    let mut raw = raw.into();
    let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m.to_owned());
    let (split, framing) = parse_head(&raw)
        .ok_or_else(|| bad(ictl_l10n::tr!("HTTP 响应没有头部结束标记", "HTTP response has no end of headers")))?;
    let status: u16 = std::str::from_utf8(&raw[..split])
        .map_err(|_| bad(ictl_l10n::tr!("HTTP 头不是 UTF-8", "HTTP headers are not UTF-8")))?
        .split("\r\n")
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| bad(ictl_l10n::tr!("HTTP 状态行无效", "invalid HTTP status line")))?;
    match framing {
        Framing::Chunked => Ok((
            status,
            dechunk(&raw[split..])
                .ok_or_else(|| bad(ictl_l10n::tr!("chunked 正文不完整", "incomplete chunked body")))?,
        )),
        Framing::Length(n) if raw.len() - split < n => Err(bad(ictl_l10n::tr!(
            "正文比 Content-Length 短，连接提前断了",
            "body shorter than Content-Length, the connection closed early"
        ))),
        Framing::Length(n) => {
            raw.truncate(split + n);
            raw.drain(..split);
            Ok((status, raw))
        }
        Framing::UntilEof => {
            raw.drain(..split);
            Ok((status, raw))
        }
    }
}

fn dechunk(mut body: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = body.windows(2).position(|w| w == b"\r\n")?;
        let size_str = std::str::from_utf8(&body[..line_end]).ok()?;
        let size = usize::from_str_radix(size_str.split(';').next()?.trim(), 16).ok()?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(body.get(..size)?);
        body = body.get(size + 2..)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use idevice::{Idevice, IdeviceError, pairing_file::PairingFile};
    use std::{
        collections::VecDeque,
        future::Future,
        pin::Pin,
        sync::{
            Mutex as SyncMutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    #[derive(Debug)]
    struct Provider {
        sockets: SyncMutex<VecDeque<tokio::io::DuplexStream>>,
        connects: AtomicUsize,
        connect_delay: Duration,
    }
    impl IdeviceProvider for Provider {
        fn connect(&self, _: u16) -> Pin<Box<dyn Future<Output = Result<Idevice, IdeviceError>> + Send>> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            let socket = self.sockets.lock().unwrap().pop_front();
            let delay = self.connect_delay;
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                socket
                    .map(|s| Idevice::new(Box::new(s), "fixture"))
                    .ok_or_else(|| IdeviceError::UnexpectedResponse("no fixture socket".into()))
            })
        }
        fn label(&self) -> &str {
            "fixture"
        }
        fn get_pairing_file(&self) -> Pin<Box<dyn Future<Output = Result<PairingFile, IdeviceError>> + Send>> {
            Box::pin(async { Err(IdeviceError::UnexpectedResponse("not used by HTTP fixture".into())) })
        }
    }
    fn transport(count: usize) -> (DeviceTransport, Arc<Provider>, Vec<tokio::io::DuplexStream>) {
        let (clients, peers): (VecDeque<_>, Vec<_>) = (0..count).map(|_| tokio::io::duplex(65536)).unzip();
        let provider = Arc::new(Provider {
            sockets: SyncMutex::new(clients),
            connects: AtomicUsize::new(0),
            connect_delay: Duration::ZERO,
        });
        (DeviceTransport::new(provider.clone(), 8100), provider, peers)
    }
    async fn request(socket: &mut tokio::io::DuplexStream) -> Vec<u8> {
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            bytes.push(socket.read_u8().await.unwrap());
        }
        let text = String::from_utf8_lossy(&bytes).to_lowercase();
        let length = text
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .map(|n| n.trim().parse::<usize>().unwrap())
            .unwrap_or(0);
        let at = bytes.len();
        bytes.resize(at + length, 0);
        socket.read_exact(&mut bytes[at..]).await.unwrap();
        bytes
    }
    async fn ok(socket: &mut tokio::io::DuplexStream) {
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}").await.unwrap();
    }

    #[derive(Clone)]
    struct LogWriter(Arc<SyncMutex<Vec<u8>>>);
    impl std::io::Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn diagnostics_split_first_byte_body_and_cancelled_reads_without_payloads() {
        // Other parallel HTTP tests may create disabled spans before this test
        // installs its subscriber. Isolate log capture, including Drop events,
        // so callsite registration cannot race with subscriber initialization.
        const CHILD: &str = "ICTL_HTTP_DIAGNOSTICS_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "http::tests::diagnostics_split_first_byte_body_and_cancelled_reads_without_payloads",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .await
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let logs = Arc::new(SyncMutex::new(Vec::new()));
        let writer = LogWriter(logs.clone());
        // The child has exactly one test and installs tracing before any request.
        tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .with_ansi(false)
                .without_time()
                .with_max_level(tracing::Level::TRACE)
                .with_writer(move || writer.clone())
                .finish(),
        )
        .unwrap();
        for cancelled in [false, true] {
            let offset = logs.lock().unwrap().len();
            let (transport, _, mut peers) = transport(1);
            let mut peer = peers.pop().unwrap();
            let server = tokio::spawn(async move {
                request(&mut peer).await;
                tokio::time::sleep(Duration::from_millis(10)).await;
                peer.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 23\r\n\r\n").await.unwrap();
                tokio::time::sleep(Duration::from_millis(50)).await;
                let _ = peer.write_all(b"very-private-image-data").await;
            });
            let operation =
                transport.send("GET", "/diagnostics-regression?token=private-token", None, Duration::from_secs(1));
            if cancelled {
                assert!(tokio::time::timeout(Duration::from_millis(30), operation).await.is_err());
            } else {
                assert_eq!(operation.await.unwrap().0, 200);
            }
            server.await.unwrap();
            let text = String::from_utf8(logs.lock().unwrap()[offset..].to_vec()).unwrap();
            let text =
                text.lines().filter(|line| line.contains("/diagnostics-regression")).collect::<Vec<_>>().join("\n");
            assert!(text.contains("http_first_byte"), "{text}");
            assert!(text.contains("http_headers"), "{text}");
            assert!(text.contains("http_complete"), "{text}");
            assert!(text.contains("first_byte_ms="), "{text}");
            assert!(text.contains("max_read_wait_ms="), "{text}");
            assert!(text.contains(if cancelled { "cancelled" } else { "response_body_bytes=23" }), "{text}");
            assert!(!text.contains("private-token") && !text.contains("very-private-image-data"), "{text}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn health_has_a_separate_pool_and_response_budget_after_slow_connect() {
        let (client, peer) = tokio::io::duplex(65536);
        let provider = Arc::new(Provider {
            sockets: SyncMutex::new(VecDeque::from([client])),
            connects: AtomicUsize::new(0),
            connect_delay: Duration::from_secs(4),
        });
        let transport = DeviceTransport::new(provider, 8100);
        let _busy_action_pool = transport.idle.lock().await;
        let server = tokio::spawn(async move {
            let mut peer = peer;
            let bytes = request(&mut peer).await;
            assert!(bytes.starts_with(b"GET /status "));
            tokio::time::sleep(Duration::from_secs(2)).await;
            ok(&mut peer).await;
        });
        let started = tokio::time::Instant::now();
        assert_eq!(transport.health().await.unwrap().0, 200);
        assert_eq!(started.elapsed(), Duration::from_secs(6));
        server.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn health_response_deadline_and_connection_deadline_remain_bounded() {
        for connect_delay in [Duration::from_secs(4), Duration::from_secs(20)] {
            let (client, _peer) = tokio::io::duplex(65536);
            let provider = Arc::new(Provider {
                sockets: SyncMutex::new(VecDeque::from([client])),
                connects: AtomicUsize::new(0),
                connect_delay,
            });
            let transport = DeviceTransport::new(provider, 8100);
            let started = tokio::time::Instant::now();
            let error = transport.health().await.unwrap_err();
            if connect_delay.as_secs() == 4 {
                assert_eq!(started.elapsed(), Duration::from_secs(9));
                assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
                assert!(error.to_string().contains("stage=read"));
            } else {
                assert_eq!(started.elapsed(), Duration::from_secs(10));
                assert_eq!(error.kind(), std::io::ErrorKind::NotConnected);
                assert!(error.to_string().contains("stage=connect"));
            }
            assert!(transport.health_idle.lock().await.is_none());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_before_connect_completes_is_not_reported_as_sent() {
        let provider = Arc::new(Provider {
            sockets: SyncMutex::new(VecDeque::new()),
            connects: AtomicUsize::new(0),
            connect_delay: Duration::from_secs(1),
        });
        let transport = DeviceTransport::new(provider, 8100);
        let error = transport.send("GET", "/status?token=private", None, Duration::from_millis(20)).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotConnected);
        assert!(error.to_string().contains("stage=connect"));
        assert!(error.to_string().contains("GET /status"));
        assert!(!error.to_string().contains("private"));
    }

    #[tokio::test(start_paused = true)]
    async fn inner_connect_timeout_also_reports_its_stage() {
        let provider = Arc::new(Provider {
            sockets: SyncMutex::new(VecDeque::new()),
            connects: AtomicUsize::new(0),
            connect_delay: Duration::from_secs(20),
        });
        let transport = DeviceTransport::new(provider, 8100);
        let error = transport.send("GET", "/status", None, Duration::from_secs(30)).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotConnected);
        assert!(error.to_string().contains("stage=connect"));
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_waiting_for_pool_has_not_sent_the_request() {
        let (transport, provider, _peers) = transport(0);
        let _guard = transport.idle.lock().await;
        let error =
            transport.send("POST", "/actions", Some(b"{}".to_vec()), Duration::from_millis(20)).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotConnected);
        assert!(error.to_string().contains("stage=pool"));
        assert_eq!(provider.connects.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_after_writing_does_not_allow_mutation_replay() {
        let (transport, provider, mut peers) = transport(1);
        let mut peer = peers.pop().unwrap();
        let server = tokio::spawn(async move {
            request(&mut peer).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        let error =
            transport.send("POST", "/actions", Some(b"{}".to_vec()), Duration::from_millis(30)).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("stage=read"));
        assert_eq!(provider.connects.load(Ordering::SeqCst), 1);
        assert!(transport.idle.lock().await.is_none());
        server.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_during_partial_write_remains_uncertain() {
        let (transport, provider, _peers) = transport(1);
        let error = transport
            .send("POST", "/actions", Some(vec![b'x'; 128 * 1024]), Duration::from_millis(20))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("stage=write"));
        assert_eq!(provider.connects.load(Ordering::SeqCst), 1);
        assert!(transport.idle.lock().await.is_none());
    }

    #[tokio::test]
    async fn repeated_requests_reuse_one_connection() {
        let (transport, provider, mut peers) = transport(1);
        let mut peer = peers.pop().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..4 {
                request(&mut peer).await;
                ok(&mut peer).await;
            }
        });
        for _ in 0..4 {
            assert_eq!(transport.send("GET", "/status", None, Duration::from_secs(1)).await.unwrap().0, 200);
        }
        server.await.unwrap();
        assert_eq!(provider.connects.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_stale_get_reconnects_once_but_a_sent_post_never_replays() {
        for method in ["GET", "POST"] {
            let (transport, provider, mut peers) = transport(2);
            let mut second = peers.pop().unwrap();
            let mut first = peers.pop().unwrap();
            let server = tokio::spawn(async move {
                request(&mut first).await;
                ok(&mut first).await;
                request(&mut first).await;
            });
            let retry = tokio::spawn(async move {
                request(&mut second).await;
                ok(&mut second).await;
            });
            transport.send("GET", "/status", None, Duration::from_secs(1)).await.unwrap();
            let result = transport.send(method, "/actions", Some(b"{}".to_vec()), Duration::from_secs(1)).await;
            if method == "GET" {
                assert!(result.is_ok());
                assert_eq!(provider.connects.load(Ordering::SeqCst), 2);
                retry.await.unwrap();
            } else {
                assert_eq!(result.unwrap_err().kind(), AFTER_SENT);
                assert_eq!(provider.connects.load(Ordering::SeqCst), 1);
                retry.abort();
            }
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn cancelled_read_discards_its_connection_and_late_body() {
        let (transport, provider, mut peers) = transport(2);
        let mut second = peers.pop().unwrap();
        let mut first = peers.pop().unwrap();
        let server = tokio::spawn(async move {
            request(&mut first).await;
            ok(&mut first).await;
            request(&mut first).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = first.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nlate").await;
        });
        let next = tokio::spawn(async move {
            request(&mut second).await;
            ok(&mut second).await;
        });
        transport.send("GET", "/status", None, Duration::from_secs(1)).await.unwrap();
        assert_eq!(
            transport.send("GET", "/source", None, Duration::from_millis(20)).await.unwrap_err().kind(),
            std::io::ErrorKind::TimedOut
        );
        assert_eq!(transport.send("GET", "/status", None, Duration::from_secs(1)).await.unwrap().1, b"{}");
        assert_eq!(provider.connects.load(Ordering::SeqCst), 2);
        next.await.unwrap();
        server.await.unwrap();
    }

    /// 有 Content-Length 时按它预先分配；声明得离谱的不照着分配
    #[tokio::test]
    async fn response_buffer_is_preallocated_from_content_length_with_a_cap() {
        for (declared, body) in [(200_000usize, vec![b'x'; 200_000]), (usize::MAX, b"{}".to_vec())] {
            let (mut client, mut server) = tokio::io::duplex(1 << 20);
            let writer = tokio::spawn(async move {
                let mut req = vec![0u8; 4];
                server.read_exact(&mut req).await.unwrap();
                let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {declared}\r\n\r\n");
                server.write_all(head.as_bytes()).await.unwrap();
                server.write_all(&body).await.unwrap();
                drop(server);
            });
            let mut stage = "";
            let mut trace = HttpTrace::new(4);
            let result = exchange(&mut client, b"GET ", &mut stage, &mut trace).await;
            writer.await.unwrap();
            if declared == 200_000 {
                assert_eq!(result.unwrap().1.len(), 200_000);
            } else {
                // 正文比声明的短：连接提前断了（照常报错），但没有按声明的长度分配内存
                assert!(result.is_err());
            }
        }
    }

    #[test]
    fn request_has_length_only_with_body() {
        let get = String::from_utf8(build_request("GET", "/status", None)).unwrap();
        assert!(get.starts_with("GET /status HTTP/1.1\r\n"));
        assert!(!get.contains("Content-Length"));
        let post = String::from_utf8(build_request("POST", "/session", Some(b"{}"))).unwrap();
        assert!(post.contains("Content-Length: 2\r\n") && post.ends_with("\r\n\r\n{}"));
    }

    #[test]
    fn parses_content_length_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nServer: x\r\n\r\nhelloEXTRA";
        assert_eq!(parse_response(raw).unwrap(), (200, b"hello".to_vec()));
    }

    #[test]
    fn parses_chunked_body() {
        let raw = b"HTTP/1.1 404 Not Found\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        assert_eq!(parse_response(raw).unwrap(), (404, b"abcde".to_vec()));
    }

    #[test]
    fn completeness_does_not_wait_for_eof() {
        // WDA 回 Content-Length 但不关连接：收齐正文就算完整
        assert!(response_complete(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}"));
        assert!(!response_complete(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n{}"));
        assert!(!response_complete(b"HTTP/1.1 200 OK\r\nContent-Len"));
        assert!(response_complete(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n"));
        assert!(!response_complete(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n"));
        // 两者都没有：只能读到 EOF
        assert!(!response_complete(b"HTTP/1.1 200 OK\r\n\r\n{}"));
    }

    #[test]
    fn body_until_eof_and_truncated_body() {
        assert_eq!(parse_response(b"HTTP/1.1 200 OK\r\n\r\n{}").unwrap(), (200, b"{}".to_vec()));
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort").is_err());
        assert!(parse_response(b"garbage").is_err());
    }
}
