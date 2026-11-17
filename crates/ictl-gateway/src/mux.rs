use std::io::Cursor;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use base64::Engine;
use plist::{Dictionary, Value};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Semaphore, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::identity::{Device, canonical_uid};
use crate::relay::{MAX_PAYLOAD, Relay};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairRecord {
    pub udid: String,
    pub pair_record_base64: String,
}

pub fn read_pair_record(record: &PairRecord, udid: &str) -> Result<(Vec<u8>, String)> {
    if canonical_uid(&record.udid)? != canonical_uid(udid)? || record.pair_record_base64.len() > MAX_PAYLOAD {
        bail!("Invalid pair record");
    }
    let bytes = base64::engine::general_purpose::STANDARD.decode(&record.pair_record_base64)?;
    if bytes.is_empty() {
        bail!("Empty pair record");
    }
    let value = Value::from_reader(Cursor::new(&bytes))?;
    let buid = value
        .as_dictionary()
        .and_then(|d| d.get("SystemBUID"))
        .and_then(Value::as_string)
        .filter(|buid| !buid.is_empty())
        .ok_or_else(|| anyhow!("Pair record needs SystemBUID"))?
        .to_owned();
    Ok((bytes, buid))
}

struct Stored {
    bytes: Option<Vec<u8>>,
    buid: String,
}

/// A usbmuxd endpoint for exactly one relayed device. It listens on a Unix socket
/// (mode 0600) inside the session's private 0700 directory, never on TCP: the mux
/// hands out the host pair record and connects to unauthenticated device ports
/// such as WDA, so only this gateway user and its native child may reach it.
pub struct VirtualMux {
    address: String,
    path: PathBuf,
    cancel: CancellationToken,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl VirtualMux {
    pub async fn start(
        device: Device,
        record: Option<PairRecord>,
        relay: Relay,
        pair_sender: mpsc::Sender<PairRecord>,
        cancel: CancellationToken,
        socket_path: &Path,
    ) -> Result<Self> {
        let (bytes, buid) = match record {
            Some(record) => {
                let (bytes, buid) = read_pair_record(&record, &device.serial_number)?;
                (Some(bytes), buid)
            }
            None => (None, Uuid::new_v4().to_string()),
        };
        let stored = Arc::new(Mutex::new(Stored { bytes, buid }));
        // idevice reads a USBMUXD_SOCKET_ADDRESS without ':' as a Unix socket path.
        let address = socket_path
            .to_str()
            .filter(|path| !path.contains(':'))
            .ok_or_else(|| anyhow!("Invalid mux socket path"))?
            .to_owned();
        let _ = std::fs::remove_file(socket_path);
        let listener = UnixListener::bind(socket_path)?;
        std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
        let task_cancel = cancel.child_token();
        let serving_cancel = task_cancel.clone();
        let task = tokio::spawn(async move {
            let permits = Arc::new(Semaphore::new(64));
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    _ = serving_cancel.cancelled() => break,
                    result = connections.join_next(), if !connections.is_empty() => { let _ = result; }
                    accepted = listener.accept() => {
                        let Ok((socket, _)) = accepted else { break };
                        let Ok(permit) = permits.clone().try_acquire_owned() else { continue };
                        let device = device.clone(); let stored = stored.clone(); let relay = relay.clone();
                        let pair_sender = pair_sender.clone(); let cancel = serving_cancel.clone();
                        connections.spawn(async move {
                            let _permit = permit;
                            tokio::select! {
                                biased;
                                _ = cancel.cancelled() => {}
                                _ = serve(socket, device, stored, relay, pair_sender) => {}
                            }
                        });
                    }
                }
            }
            connections.abort_all();
            while connections.join_next().await.is_some() {}
        });
        Ok(Self { address, path: socket_path.to_owned(), cancel: task_cancel, task: Mutex::new(Some(task)) })
    }

    pub fn address(&self) -> String {
        self.address.clone()
    }

    pub async fn close(&self) {
        self.cancel.cancel();
        if let Some(task) = self.task.lock().await.take() {
            let _ = task.await;
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for VirtualMux {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn dict(pairs: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Dictionary(pairs.into_iter().map(|(key, value)| (key.to_string(), value)).collect())
}

fn integer(value: u64) -> Value {
    Value::Integer(value.into())
}
fn string(value: impl Into<String>) -> Value {
    Value::String(value.into())
}
fn number(value: &Dictionary, name: &str) -> Option<u64> {
    value.get(name).and_then(Value::as_unsigned_integer)
}

async fn send(socket: &mut (impl AsyncWrite + Unpin), value: Value, tag: u32) -> Result<()> {
    let mut payload = Vec::new();
    value.to_writer_xml(&mut payload)?;
    if payload.len() > MAX_PAYLOAD - 16 {
        bail!("usbmuxd response too large");
    }
    let mut packet = Vec::with_capacity(16 + payload.len());
    for number in [(16 + payload.len()) as u32, 1, 8, tag] {
        packet.extend_from_slice(&number.to_le_bytes());
    }
    packet.extend_from_slice(&payload);
    socket.write_all(&packet).await?;
    Ok(())
}

async fn result(socket: &mut (impl AsyncWrite + Unpin), code: u64, tag: u32) -> Result<()> {
    send(socket, dict([("MessageType", string("Result")), ("Number", integer(code))]), tag).await
}

fn attached(device: &Device) -> Value {
    dict([
        ("MessageType", string("Attached")),
        ("DeviceID", integer(1)),
        (
            "Properties",
            dict([
                ("DeviceID", integer(1)),
                ("SerialNumber", string(&device.serial_number)),
                ("ProductID", integer(device.product_id as u64)),
                ("ConnectionType", string("USB")),
                ("LocationID", integer(1)),
            ]),
        ),
    ])
}

async fn serve(
    mut socket: UnixStream,
    device: Device,
    stored: Arc<Mutex<Stored>>,
    relay: Relay,
    pair_sender: mpsc::Sender<PairRecord>,
) -> Result<()> {
    loop {
        let packet = tokio::time::timeout(Duration::from_secs(30), read_packet(&mut socket)).await??;
        let (value, tag) = packet;
        let value = value.as_dictionary().ok_or_else(|| anyhow!("usbmuxd dictionary required"))?;
        let record_matches = value
            .get("PairRecordID")
            .and_then(Value::as_string)
            .and_then(|id| canonical_uid(id).ok())
            .is_some_and(|id| id == device.serial_number);
        match value.get("MessageType").and_then(Value::as_string) {
            Some("ListDevices") => {
                send(&mut socket, dict([("DeviceList", Value::Array(vec![attached(&device)]))]), tag).await?
            }
            Some("Listen") => {
                result(&mut socket, 0, tag).await?;
                send(&mut socket, attached(&device), tag).await?;
            }
            Some("ReadBUID") => {
                let buid = stored.lock().await.buid.clone();
                send(&mut socket, dict([("BUID", string(buid))]), tag).await?;
            }
            Some("ReadPairRecord") => {
                let bytes = stored.lock().await.bytes.clone();
                match bytes.filter(|_| record_matches) {
                    Some(bytes) => send(&mut socket, dict([("PairRecordData", Value::Data(bytes))]), tag).await?,
                    None => result(&mut socket, 2, tag).await?,
                }
            }
            Some("SavePairRecord") => {
                let Some(bytes) = value.get("PairRecordData").and_then(Value::as_data).filter(|_| record_matches)
                else {
                    result(&mut socket, 2, tag).await?;
                    continue;
                };
                let record = PairRecord {
                    udid: device.serial_number.clone(),
                    pair_record_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                };
                let Ok((bytes, buid)) = read_pair_record(&record, &device.serial_number) else {
                    result(&mut socket, 1, tag).await?;
                    continue;
                };
                {
                    let mut stored = stored.lock().await;
                    stored.bytes = Some(bytes);
                    stored.buid = buid;
                }
                result(&mut socket, 0, tag).await?;
                pair_sender.send(record).await.map_err(|_| anyhow!("Pair record recipient closed"))?;
            }
            Some("Connect") => {
                if number(value, "DeviceID") != Some(1) {
                    result(&mut socket, 2, tag).await?;
                    continue;
                }
                let Some(port) = number(value, "PortNumber").filter(|port| *port > 0 && *port <= 65535) else {
                    result(&mut socket, 1, tag).await?;
                    continue;
                };
                match relay.open((port as u16).swap_bytes()).await {
                    Ok(mut remote) => {
                        result(&mut socket, 0, tag).await?;
                        // read_exact consumed precisely the plist. Any coalesced raw bytes remain in the socket.
                        tokio::io::copy_bidirectional(&mut socket, &mut remote).await?;
                        return Ok(());
                    }
                    Err(_) => result(&mut socket, 3, tag).await?,
                }
            }
            _ => result(&mut socket, 1, tag).await?,
        }
    }
}

async fn read_packet(socket: &mut (impl AsyncRead + Unpin)) -> Result<(Value, u32)> {
    let mut head = [0; 16];
    socket.read_exact(&mut head).await?;
    let number = |start| u32::from_le_bytes(head[start..start + 4].try_into().expect("bounded header"));
    let size = number(0) as usize;
    if !(16..=MAX_PAYLOAD).contains(&size) || number(4) != 1 || number(8) != 8 {
        bail!("Invalid usbmuxd frame");
    }
    let tag = number(12);
    let mut body = vec![0; size - 16];
    socket.read_exact(&mut body).await?;
    Ok((Value::from_reader(Cursor::new(body))?, tag))
}

/// Connect to the one device owned by this virtual mux. Never accepts a host from an API request.
pub(crate) async fn connect_port(address: &str, port: u16) -> Result<UnixStream> {
    let mut socket = UnixStream::connect(address).await?;
    send(
        &mut socket,
        dict([
            ("MessageType", string("Connect")),
            ("DeviceID", integer(1)),
            ("PortNumber", integer(port.swap_bytes() as u64)),
        ]),
        1,
    )
    .await?;
    let (reply, tag) = read_packet(&mut socket).await?;
    anyhow::ensure!(
        tag == 1 && reply.as_dictionary().and_then(|v| number(v, "Number")) == Some(0),
        "Device refused preview connection"
    );
    Ok(socket)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_wrong_identity_and_missing_buid() {
        let mut bytes = Vec::new();
        dict([("SystemBUID", string("test-buid"))]).to_writer_xml(&mut bytes).unwrap();
        let record = PairRecord {
            udid: "a".repeat(40),
            pair_record_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        };
        assert_eq!(read_pair_record(&record, &"a".repeat(40)).unwrap().1, "test-buid");
        assert!(read_pair_record(&record, &"b".repeat(40)).is_err());
        assert!(
            read_pair_record(&PairRecord { pair_record_base64: "eA==".into(), ..record }, &"a".repeat(40)).is_err()
        );
    }

    #[tokio::test]
    async fn lists_only_owned_device_and_returns_buid() {
        let (outbound, _) = mpsc::channel(8);
        let relay = Relay::new(outbound, CancellationToken::new(), "fixture".into());
        let (pair_sender, _) = mpsc::channel(8);
        let device = Device {
            platform: crate::identity::Platform::Ios,
            serial_number: "a".repeat(40),
            vendor_id: 1452,
            product_id: 4776,
            product_name: None,
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mux.sock");
        let mux =
            VirtualMux::start(device.clone(), None, relay, pair_sender, CancellationToken::new(), &path).await.unwrap();
        assert_eq!(mux.address(), path.to_str().unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let mut socket = UnixStream::connect(mux.address()).await.unwrap();
        send(&mut socket, dict([("MessageType", string("ListDevices"))]), 7).await.unwrap();
        let (reply, tag) = read_packet(&mut socket).await.unwrap();
        assert_eq!(tag, 7);
        let list = reply.as_dictionary().unwrap()["DeviceList"].as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(
            list[0].as_dictionary().unwrap()["Properties"].as_dictionary().unwrap()["SerialNumber"].as_string(),
            Some(device.serial_number.as_str())
        );
        send(&mut socket, dict([("MessageType", string("ReadBUID"))]), 8).await.unwrap();
        let (reply, _) = read_packet(&mut socket).await.unwrap();
        assert!(!reply.as_dictionary().unwrap()["BUID"].as_string().unwrap().is_empty());
        mux.close().await;
        assert!(socket.read_u8().await.is_err());
        assert!(!path.exists(), "closing the mux removes its socket");
    }
}
