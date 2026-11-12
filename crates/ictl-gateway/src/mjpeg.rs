//! Bounded WDA MJPEG reader and a latest-frame channel shared by preview clients.
use anyhow::{Result, anyhow, ensure};
use axum::{
    body::{Body, Bytes},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Weak},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    sync::{Mutex, watch},
};
use tokio_util::sync::CancellationToken;

const MAX_HEADER: usize = 16 * 1024;
const MAX_FRAME: usize = 2 * 1024 * 1024;
const FRAME_TIMEOUT: Duration = Duration::from_secs(15);

struct Reader<R> {
    input: R,
}
impl<R: AsyncBufRead + Unpin> Reader<R> {
    fn new(input: R) -> Self {
        Self { input }
    }
    async fn line(&mut self, remaining: &mut usize) -> Result<String> {
        let mut bytes = Vec::new();
        (&mut self.input).take((*remaining + 1) as u64).read_until(b'\n', &mut bytes).await?;
        ensure!(!bytes.is_empty() && bytes.len() <= *remaining, "Invalid MJPEG headers");
        *remaining -= bytes.len();
        Ok(std::str::from_utf8(&bytes)?.trim_end_matches(['\r', '\n']).to_owned())
    }
    async fn headers(&mut self) -> Result<()> {
        let mut remaining = MAX_HEADER;
        let status = self.line(&mut remaining).await?;
        ensure!(status.split_whitespace().nth(1) == Some("200"), "WDA MJPEG unavailable");
        let mut multipart = false;
        loop {
            let line = self.line(&mut remaining).await?;
            if line.is_empty() {
                break;
            }
            if let Some((key, value)) = line.split_once(':') {
                if key.eq_ignore_ascii_case("content-type") {
                    multipart = value.trim().to_ascii_lowercase().starts_with("multipart/x-mixed-replace");
                }
                ensure!(!key.eq_ignore_ascii_case("transfer-encoding"), "Unsupported WDA MJPEG encoding");
            }
        }
        ensure!(multipart, "WDA did not return MJPEG");
        Ok(())
    }
    async fn frame(&mut self) -> Result<Vec<u8>> {
        let mut remaining = MAX_HEADER;
        let mut length = None;
        loop {
            let line = self.line(&mut remaining).await?;
            if line.is_empty() {
                if length.is_some() {
                    break;
                }
                continue;
            }
            if let Some((key, value)) = line.split_once(':')
                && key.eq_ignore_ascii_case("content-length")
            {
                ensure!(length.is_none(), "Duplicate MJPEG length");
                let size: usize = value.trim().parse()?;
                ensure!((4..=MAX_FRAME).contains(&size), "MJPEG frame exceeds limit");
                length = Some(size);
            }
        }
        let mut bytes = vec![0; length.ok_or_else(|| anyhow!("Missing MJPEG length"))?];
        self.input.read_exact(&mut bytes).await?;
        ensure!(bytes.starts_with(b"\xff\xd8") && bytes.ends_with(b"\xff\xd9"), "Invalid MJPEG JPEG frame");
        Ok(bytes)
    }
}

pub(crate) struct Frame {
    bytes: Vec<u8>,
    at: Instant,
    sequence: u64,
}
type FrameSender = watch::Sender<Option<Arc<Frame>>>;

#[derive(Default)]
pub struct Preview {
    // Only the reader task owns the sender. Its exit must close HTTP responses
    // immediately, including when the source settings cannot be acknowledged.
    sender: Mutex<Option<Weak<FrameSender>>>,
}

impl Preview {
    pub async fn status(&self) -> Value {
        let sender = self.sender.lock().await;
        let frame = sender.as_ref().and_then(Weak::upgrade).and_then(|s| s.borrow().clone());
        match frame {
            Some(frame) => {
                json!({"state":"streaming","ageMs":frame.at.elapsed().as_millis() as u64,"frames":frame.sequence,"frameBytes":frame.bytes.len()})
            }
            None => json!({"state":"waiting"}),
        }
    }
    pub(crate) async fn subscribe<F, Fut>(
        &self,
        address: String,
        cancel: CancellationToken,
        prepare: F,
    ) -> watch::Receiver<Option<Arc<Frame>>>
    where
        F: Fn() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send,
    {
        let mut current = self.sender.lock().await;
        if let Some(sender) = current.as_ref().and_then(Weak::upgrade).filter(|s| s.receiver_count() > 0) {
            return sender.subscribe();
        }
        let (sender, receiver) = watch::channel(None);
        let sender = Arc::new(sender);
        *current = Some(Arc::downgrade(&sender));
        tokio::spawn(async move {
            let mut sequence = 0;
            loop {
                // Gate every connection, not just the first subscriber: a WDA
                // restart resets its process-wide MJPEG limits to the defaults.
                let prepared = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    _ = sender.closed() => break,
                    result = prepare() => result,
                };
                if prepared.is_err() {
                    // Dropping the channel ends an existing response or makes
                    // the first-frame wait fail immediately for screenshot fallback.
                    break;
                }
                let capture = async {
                    let mut socket =
                        tokio::time::timeout(FRAME_TIMEOUT, crate::mux::connect_port(&address, 9100)).await??;
                    socket.write_all(b"GET / HTTP/1.0\r\nHost: 127.0.0.1:9100\r\nConnection: close\r\n\r\n").await?;
                    let mut reader = Reader::new(BufReader::new(socket));
                    tokio::time::timeout(FRAME_TIMEOUT, reader.headers()).await??;
                    loop {
                        let bytes = tokio::time::timeout(FRAME_TIMEOUT, reader.frame()).await??;
                        sequence += 1;
                        sender.send_replace(Some(Arc::new(Frame { bytes, at: Instant::now(), sequence })));
                    }
                    #[allow(unreachable_code)]
                    Ok::<(), anyhow::Error>(())
                };
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    _ = sender.closed() => break,
                    _ = tokio::time::timeout(Duration::from_secs(3600), capture) => {}
                }
                sender.send_replace(None);
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    _ = sender.closed() => break,
                    _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                }
            }
        });
        receiver
    }
}

pub(crate) fn response(receiver: watch::Receiver<Option<Arc<Frame>>>, cancel: CancellationToken) -> Response {
    let stream = futures_util::stream::unfold((receiver, cancel), |(mut receiver, cancel)| async move {
        loop {
            let changed =
                tokio::select! { biased; _ = cancel.cancelled() => return None, result = receiver.changed() => result };
            if changed.is_err() {
                return None;
            }
            let frame = receiver.borrow_and_update().clone();
            if let Some(frame) = frame {
                let mut bytes = format!(
                    "--phone-frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                    frame.bytes.len()
                )
                .into_bytes();
                bytes.extend_from_slice(&frame.bytes);
                bytes.extend_from_slice(b"\r\n");
                return Some((Ok::<_, std::io::Error>(Bytes::from(bytes)), (receiver, cancel)));
            }
        }
    });
    let mut response = Body::from_stream(stream).into_response();
    let headers = response.headers_mut();
    headers.insert("content-type", "multipart/x-mixed-replace; boundary=phone-frame".parse().unwrap());
    headers.insert("cache-control", "no-store, private".parse().unwrap());
    headers.insert("x-accel-buffering", "no".parse().unwrap());
    headers.insert("referrer-policy", "no-referrer".parse().unwrap());
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn rejected_preparation_closes_waiters_without_connecting_or_retrying() {
        let preview = Preview::default();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = calls.clone();
        let mut receiver = preview
            .subscribe("invalid-mux-address".into(), CancellationToken::new(), move || {
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { anyhow::bail!("settings rejected") }
            })
            .await;
        assert!(
            tokio::time::timeout(Duration::from_millis(200), receiver.wait_for(|f| f.is_some()))
                .await
                .expect("failed preparation must promptly close the preview")
                .is_err()
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(preview.status().await["state"], "waiting");
    }

    #[tokio::test]
    async fn disconnecting_the_last_subscriber_cancels_pending_preparation() {
        struct OnDrop(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let started = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let preview = Preview::default();
        let receiver = preview
            .subscribe("invalid-mux-address".into(), CancellationToken::new(), {
                let started = started.clone();
                let dropped = dropped.clone();
                move || {
                    let started = started.clone();
                    let dropped = dropped.clone();
                    async move {
                        let _on_drop = OnDrop(dropped);
                        started.notify_one();
                        std::future::pending::<Result<()>>().await
                    }
                }
            })
            .await;
        tokio::time::timeout(Duration::from_millis(200), started.notified()).await.unwrap();
        drop(receiver);
        tokio::time::timeout(Duration::from_millis(200), async {
            while !dropped.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("no preview task may retain a pending native request after the client disconnects");
    }

    #[tokio::test]
    async fn reads_fragmented_frames_without_waiting_for_the_next_boundary() {
        let (mut tx, rx) = tokio::io::duplex(64);
        tokio::spawn(async move {
            let frame = b"HTTP/1.0 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\n\r\n--frame\r\nContent-Type: image/jpeg\r\nContent-Length: 4\r\n\r\n\xff\xd8\xff\xd9";
            for byte in frame {
                tx.write_all(&[*byte]).await.unwrap();
            }
        });
        let mut reader = Reader::new(BufReader::new(rx));
        reader.headers().await.unwrap();
        assert_eq!(reader.frame().await.unwrap(), b"\xff\xd8\xff\xd9");
    }

    #[tokio::test]
    async fn rejects_upstream_http_errors_and_oversized_headers() {
        for input in [b"HTTP/1.1 500 Error\r\n\r\n".to_vec(), vec![b'x'; 16385]] {
            let mut reader = Reader::new(BufReader::new(input.as_slice()));
            assert!(reader.headers().await.is_err());
        }
    }

    #[tokio::test]
    async fn rejects_missing_or_oversized_frame_lengths_and_non_jpeg() {
        for headers in ["Content-Type: image/jpeg\r\n", "Content-Length: 999999999\r\n", "Content-Length: 4\r\n"] {
            let input = format!("--frame\r\n{headers}\r\nxxxx");
            let mut reader = Reader::new(BufReader::new(input.as_bytes()));
            assert!(reader.frame().await.is_err());
        }
    }
}
