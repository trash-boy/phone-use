use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

pub const MAX_PAYLOAD: usize = 1024 * 1024;
pub const MAX_USB_DATA: usize = 3 * 16384 - 16 - 20;
const STREAM_BUFFER: usize = 64 * 1024;

#[derive(Debug)]
pub struct Frame {
    pub kind: u8,
    pub request_id: u32,
    pub stream_id: u32,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn json(kind: u8, request_id: u32, stream_id: u32, value: &Value) -> Self {
        Self { kind, request_id, stream_id, payload: serde_json::to_vec(value).expect("JSON value serializes") }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = vec![0; 16];
        bytes[0] = 1;
        bytes[1] = self.kind;
        bytes[4..8].copy_from_slice(&self.request_id.to_be_bytes());
        bytes[8..12].copy_from_slice(&self.stream_id.to_be_bytes());
        bytes[12..16].copy_from_slice(&(self.payload.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&self.payload);
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 16 || bytes[0] != 1 || bytes[2] != 0 || bytes[3] != 0 {
            bail!("Invalid relay header");
        }
        let read = |start| u32::from_be_bytes(bytes[start..start + 4].try_into().expect("header bounded"));
        let size = read(12) as usize;
        if size > MAX_PAYLOAD || bytes.len() != size + 16 {
            bail!("Invalid relay length");
        }
        Ok(Self { kind: bytes[1], request_id: read(4), stream_id: read(8), payload: bytes[16..].to_vec() })
    }
}

struct Entry {
    opened: bool,
    result: Option<oneshot::Sender<bool>>,
    // Dropping the sender marks peer EOF; the receiver drains queued bytes first.
    incoming: Option<mpsc::Sender<Vec<u8>>>,
    queued: Arc<AtomicUsize>,
    cancel: CancellationToken,
}

struct Streams {
    next_id: u64,
    entries: HashMap<u32, Entry>,
}

struct Inner {
    streams: Mutex<Streams>,
    outbound: mpsc::Sender<Frame>,
    cancel: CancellationToken,
}

#[derive(Clone)]
pub struct Relay(Arc<Inner>);

impl Relay {
    pub fn new(outbound: mpsc::Sender<Frame>, cancel: CancellationToken) -> Self {
        Self(Arc::new(Inner { streams: Mutex::new(Streams { next_id: 1, entries: HashMap::new() }), outbound, cancel }))
    }

    async fn send(&self, frame: Frame) -> Result<()> {
        tokio::select! {
            biased;
            _ = self.0.cancel.cancelled() => bail!("Relay closed"),
            result = self.0.outbound.send(frame) => result.map_err(|_| anyhow!("Relay closed")),
        }
    }

    async fn retire(&self, id: u32, remote: bool) {
        if let Some(entry) = self.0.streams.lock().await.entries.remove(&id) {
            entry.cancel.cancel();
            if !remote && !self.0.cancel.is_cancelled() {
                // Closing a stream must never block cleanup behind a stalled socket.
                if self
                    .0
                    .outbound
                    .try_send(Frame { kind: 5, request_id: 0, stream_id: id, payload: Vec::new() })
                    .is_err()
                {
                    self.0.cancel.cancel();
                }
            }
        }
    }

    pub async fn open(&self, port: u16) -> Result<DuplexStream> {
        let (result_tx, result_rx) = oneshot::channel();
        let (incoming_tx, mut incoming_rx) = mpsc::channel::<Vec<u8>>(256);
        let cancel = self.0.cancel.child_token();
        let queued = Arc::new(AtomicUsize::new(0));
        let id = {
            let mut streams = self.0.streams.lock().await;
            if self.0.cancel.is_cancelled() || streams.entries.len() >= 64 || streams.next_id > u32::MAX as u64 {
                bail!("Relay stream limit reached or closed");
            }
            let id = streams.next_id as u32;
            streams.next_id += 1;
            streams.entries.insert(
                id,
                Entry {
                    opened: false,
                    result: Some(result_tx),
                    incoming: Some(incoming_tx),
                    queued: queued.clone(),
                    cancel: cancel.clone(),
                },
            );
            id
        };
        let opened = async {
            self.send(Frame::json(2, id, id, &json!({ "port": port }))).await?;
            match result_rx.await {
                Ok(true) => Ok(()),
                _ => bail!("Device refused stream"),
            }
        };
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(anyhow!("Relay closed")),
            result = tokio::time::timeout(Duration::from_secs(20), opened) => result.map_err(|_| anyhow!("Stream open timeout")).and_then(|r| r),
        };
        if let Err(error) = result {
            self.retire(id, false).await;
            return Err(error);
        }
        let (local, remote) = tokio::io::duplex(STREAM_BUFFER);
        let (mut read, mut write) = tokio::io::split(remote);
        let inbound_cancel = cancel.clone();
        let inbound_relay = self.clone();
        tokio::spawn(async move {
            let result: Result<()> = async {
                loop {
                    let data = tokio::select! {
                        biased;
                        _ = inbound_cancel.cancelled() => break,
                        data = incoming_rx.recv() => match data { Some(data) => data, None => break },
                    };
                    tokio::select! {
                        biased;
                        _ = inbound_cancel.cancelled() => break,
                        result = write.write_all(&data) => result?,
                    }
                    queued.fetch_sub(data.len(), Ordering::AcqRel);
                }
                Ok(())
            }
            .await;
            if result.is_ok() {
                let _ = write.shutdown().await;
            }
            inbound_relay.retire(id, false).await;
        });
        let outbound_relay = self.clone();
        tokio::spawn(async move {
            let result: Result<()> = async {
                let mut buffer = vec![0; MAX_USB_DATA];
                loop {
                    let length = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => break,
                        result = read.read(&mut buffer) => result?,
                    };
                    if length == 0 { break; }
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => break,
                        result = outbound_relay.send(Frame { kind: 4, request_id: 0, stream_id: id, payload: buffer[..length].to_vec() }) => result?,
                    }
                }
                Ok(())
            }.await;
            let _ = result;
            outbound_relay.retire(id, false).await;
        });
        Ok(local)
    }

    pub async fn handle(&self, frame: Frame) -> Result<()> {
        let mut streams = self.0.streams.lock().await;
        if frame.stream_id == 0 || frame.stream_id as u64 >= streams.next_id {
            bail!("Unknown relay stream");
        }
        match frame.kind {
            3 => {
                if frame.request_id != frame.stream_id {
                    bail!("Invalid OpenResult");
                }
                let result: Value = serde_json::from_slice(&frame.payload)?;
                let ok = result.get("ok").and_then(Value::as_bool).ok_or_else(|| anyhow!("Invalid OpenResult"))?;
                let Some(entry) = streams.entries.get_mut(&frame.stream_id) else { return Ok(()) };
                if entry.opened || entry.result.is_none() {
                    bail!("Unexpected OpenResult");
                }
                if ok {
                    entry.opened = true;
                    if let Some(sender) = entry.result.take() {
                        let _ = sender.send(true);
                    }
                } else {
                    let entry = streams.entries.remove(&frame.stream_id).expect("entry exists");
                    entry.cancel.cancel();
                    if let Some(sender) = entry.result {
                        let _ = sender.send(false);
                    }
                }
            }
            4 => {
                if frame.request_id != 0 {
                    bail!("Invalid StreamData");
                }
                let Some(entry) = streams.entries.get(&frame.stream_id) else { return Ok(()) };
                if !entry.opened {
                    bail!("Unknown or unopened stream");
                }
                let Some(incoming) = entry.incoming.as_ref() else { return Ok(()) };
                let length = frame.payload.len();
                let previous = entry.queued.fetch_add(length, Ordering::AcqRel);
                // The duplex has STREAM_BUFFER bytes in addition to the queue.
                if previous.saturating_add(length) > MAX_PAYLOAD - STREAM_BUFFER {
                    entry.queued.fetch_sub(length, Ordering::AcqRel);
                    bail!("Stream receive buffer exceeded");
                }
                if incoming.try_send(frame.payload).is_err() {
                    entry.queued.fetch_sub(length, Ordering::AcqRel);
                    bail!("Stream receive buffer exceeded");
                }
            }
            5 => {
                if frame.request_id != 0 {
                    bail!("Invalid StreamClose");
                }
                if let Some(entry) = streams.entries.get_mut(&frame.stream_id) {
                    if entry.opened {
                        // Data and Close arrive in order, but the receive pump may
                        // not have run yet. Drain before EOF; session cancellation
                        // can still interrupt a stalled reader immediately.
                        entry.incoming.take();
                    } else if let Some(entry) = streams.entries.remove(&frame.stream_id) {
                        entry.cancel.cancel();
                    }
                }
            }
            _ => bail!("Unsupported stream frame"),
        }
        Ok(())
    }

    pub async fn close(&self) {
        self.0.cancel.cancel();
        for (_, entry) in self.0.streams.lock().await.entries.drain() {
            entry.cancel.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_frames_reject_flags_lengths_and_versions() {
        let frame = Frame::json(2, 17, 19, &json!({"port":8100}));
        let bytes = frame.encode();
        let decoded = Frame::decode(&bytes).unwrap();
        assert_eq!(decoded.request_id, 17);
        assert_eq!(decoded.stream_id, 19);
        assert_eq!(&bytes[4..8], &17u32.to_be_bytes());
        for index in [0, 2, 3, 15] {
            let mut bad = bytes.clone();
            bad[index] ^= 1;
            assert!(Frame::decode(&bad).is_err());
        }
        assert!(Frame::decode(&bytes[..15]).is_err());
    }

    #[tokio::test]
    async fn stream_pumps_bytes_and_bounds_usb_payloads() {
        let (tx, mut rx) = mpsc::channel(8);
        let relay = Relay::new(tx, CancellationToken::new());
        let opening = tokio::spawn({
            let relay = relay.clone();
            async move { relay.open(8100).await }
        });
        let frame = rx.recv().await.unwrap();
        assert_eq!(frame.kind, 2);
        relay.handle(Frame::json(3, frame.stream_id, frame.stream_id, &json!({"ok":true}))).await.unwrap();
        let mut stream = opening.await.unwrap().unwrap();
        relay
            .handle(Frame { kind: 4, request_id: 0, stream_id: frame.stream_id, payload: b"phone".to_vec() })
            .await
            .unwrap();
        let mut word = [0; 5];
        stream.read_exact(&mut word).await.unwrap();
        assert_eq!(&word, b"phone");
        let data = vec![31; 140_000];
        let writer = tokio::spawn(async move {
            stream.write_all(&data).await.unwrap();
            stream
        });
        let mut received = 0;
        while received < 140_000 {
            let frame = rx.recv().await.unwrap();
            assert_eq!(frame.kind, 4);
            assert!(frame.payload.len() <= MAX_USB_DATA);
            received += frame.payload.len();
        }
        drop(writer.await.unwrap());
        relay.close().await;
    }

    #[tokio::test]
    async fn rejects_unopened_and_unknown_streams_and_closes_waiters() {
        let (tx, mut rx) = mpsc::channel(8);
        let relay = Relay::new(tx, CancellationToken::new());
        let opening = tokio::spawn({
            let relay = relay.clone();
            async move { relay.open(8100).await }
        });
        let frame = rx.recv().await.unwrap();
        assert!(
            relay.handle(Frame { kind: 4, request_id: 0, stream_id: frame.stream_id, payload: vec![] }).await.is_err()
        );
        assert!(relay.handle(Frame { kind: 5, request_id: 0, stream_id: 100, payload: vec![] }).await.is_err());
        relay.close().await;
        assert!(opening.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn peer_close_drains_final_bytes_before_eof_even_under_backpressure() {
        for length in [19, STREAM_BUFFER * 3] {
            let (tx, mut rx) = mpsc::channel(8);
            let relay = Relay::new(tx, CancellationToken::new());
            let opening = tokio::spawn({
                let relay = relay.clone();
                async move { relay.open(8100).await }
            });
            let id = rx.recv().await.unwrap().stream_id;
            relay.handle(Frame::json(3, id, id, &json!({"ok":true}))).await.unwrap();
            let mut stream = opening.await.unwrap().unwrap();
            let expected = vec![42; length];
            // No yield between Data and Close: the receive pump has not run yet.
            for chunk in expected.chunks(MAX_USB_DATA) {
                relay.handle(Frame { kind: 4, request_id: 0, stream_id: id, payload: chunk.to_vec() }).await.unwrap();
            }
            relay.handle(Frame { kind: 5, request_id: 0, stream_id: id, payload: vec![] }).await.unwrap();
            // Duplicate Close and late Data cannot revive or append to the stream.
            relay.handle(Frame { kind: 5, request_id: 0, stream_id: id, payload: vec![] }).await.unwrap();
            relay.handle(Frame { kind: 4, request_id: 0, stream_id: id, payload: vec![99] }).await.unwrap();
            let mut received = Vec::new();
            tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut received)).await.unwrap().unwrap();
            assert_eq!(received, expected);
            assert!(relay.0.streams.lock().await.entries.is_empty());
            relay.close().await;
        }
    }

    #[tokio::test]
    async fn session_cancel_interrupts_a_peer_close_blocked_on_a_full_buffer() {
        let (tx, mut rx) = mpsc::channel(8);
        let relay = Relay::new(tx, CancellationToken::new());
        let opening = tokio::spawn({
            let relay = relay.clone();
            async move { relay.open(8100).await }
        });
        let id = rx.recv().await.unwrap().stream_id;
        relay.handle(Frame::json(3, id, id, &json!({"ok":true}))).await.unwrap();
        let mut stream = opening.await.unwrap().unwrap();
        relay
            .handle(Frame { kind: 4, request_id: 0, stream_id: id, payload: vec![7; STREAM_BUFFER * 2] })
            .await
            .unwrap();
        relay.handle(Frame { kind: 5, request_id: 0, stream_id: id, payload: vec![] }).await.unwrap();
        tokio::task::yield_now().await;
        relay.close().await;
        let mut received = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut received)).await.unwrap().unwrap();
        assert!(received.len() <= STREAM_BUFFER);
        assert!(relay.0.streams.lock().await.entries.is_empty());
    }

    #[tokio::test]
    async fn peer_close_cancels_an_unfinished_open() {
        let (tx, mut rx) = mpsc::channel(8);
        let relay = Relay::new(tx, CancellationToken::new());
        let opening = tokio::spawn({
            let relay = relay.clone();
            async move { relay.open(8100).await }
        });
        let id = rx.recv().await.unwrap().stream_id;
        relay.handle(Frame { kind: 5, request_id: 0, stream_id: id, payload: vec![] }).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(1), opening).await.unwrap().unwrap().is_err());
        assert!(relay.0.streams.lock().await.entries.is_empty());
        relay.close().await;
    }
}
