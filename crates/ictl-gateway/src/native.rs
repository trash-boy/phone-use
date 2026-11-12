use std::collections::HashMap;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::identity::Device;

const MAX_RESPONSE: usize = 12 * 1024 * 1024;
const MAX_TRANSCRIPT: usize = 32 * 1024;
const MAX_DAEMON_LOG: u64 = 5 * 1024 * 1024;

struct Group {
    pid: i32,
    cleanup: Mutex<()>,
    cleaned: AtomicBool,
}

struct State {
    ready: bool,
    daemon: &'static str,
    setup: Value,
    setup_pid: Option<i32>,
    transcript: Vec<u8>,
}

struct Inner {
    command: PathBuf,
    device: Device,
    data_dir: PathBuf,
    socket_path: PathBuf,
    mux_address: String,
    cancel: CancellationToken,
    state: Mutex<State>,
    groups: Mutex<HashMap<i32, Arc<Group>>>,
    stdin: Mutex<Option<(i32, ChildStdin)>>,
    start_lock: Mutex<()>,
    setup_lock: Mutex<()>,
}

#[derive(Clone)]
pub struct NativeController(Arc<Inner>);

impl NativeController {
    pub fn new(
        command: PathBuf,
        device: Device,
        data_dir: PathBuf,
        socket_path: PathBuf,
        mux_address: String,
        cancel: CancellationToken,
    ) -> Self {
        Self(Arc::new(Inner {
            command,
            device,
            data_dir,
            socket_path,
            mux_address,
            cancel,
            state: Mutex::new(State {
                ready: false,
                daemon: "idle",
                setup: json!({"state":"idle","transcript":"","exitCode":null}),
                setup_pid: None,
                transcript: Vec::new(),
            }),
            groups: Mutex::new(HashMap::new()),
            stdin: Mutex::new(None),
            start_lock: Mutex::new(()),
            setup_lock: Mutex::new(()),
        }))
    }

    async fn spawn(&self, mode: &str) -> Result<(Child, Arc<Group>)> {
        if self.0.cancel.is_cancelled() {
            bail!("Controller closed");
        }
        let mut command = Command::new(&self.0.command);
        command
            .arg(mode)
            .env("IPHONE_UDID", &self.0.device.serial_number)
            .env("ICTL_DATA_DIR", &self.0.data_dir)
            .env("ICTL_SOCKET", &self.0.socket_path)
            .env("ICTL_MANAGE_USBMUXD", "0")
            .env("ICTL_LOW_LATENCY", std::env::var("ICTL_LOW_LATENCY").unwrap_or_else(|_| "1".into()))
            .env("USBMUXD_SOCKET_ADDRESS", &self.0.mux_address)
            .stdin(if mode == "setup" { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command.process_group(0);
        // Registration and shutdown share this lock, so close cannot miss a newly spawned child.
        let mut groups = self.0.groups.lock().await;
        if self.0.cancel.is_cancelled() {
            bail!("Controller closed");
        }
        let child = command.spawn().context("Could not start native process")?;
        let group = Arc::new(Group {
            pid: child.id().ok_or_else(|| anyhow!("Native process has no PID"))? as i32,
            cleanup: Mutex::new(()),
            cleaned: AtomicBool::new(false),
        });
        groups.insert(group.pid, group.clone());
        Ok((child, group))
    }

    pub async fn start(&self) -> Result<()> {
        let _start = self.0.start_lock.lock().await;
        if self.0.cancel.is_cancelled() {
            bail!("Controller closed");
        }
        {
            let state = self.0.state.lock().await;
            if state.ready {
                return Ok(());
            }
            if state.daemon != "idle" {
                bail!("Native daemon already started");
            }
        }
        tokio::fs::create_dir_all(&self.0.data_dir).await?;
        tokio::fs::set_permissions(&self.0.data_dir, std::fs::Permissions::from_mode(0o700)).await?;
        self.0.state.lock().await.daemon = "starting";
        let (mut child, group) = match self.spawn("daemon").await {
            Ok(child) => child,
            Err(error) => {
                self.0.state.lock().await.daemon = "failed";
                return Err(error);
            }
        };
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(drain(stdout, self.0.data_dir.join("logs/daemon.stdout.log")));
        }
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(drain(stderr, self.0.data_dir.join("logs/daemon.stderr.log")));
        }
        let controller = self.clone();
        tokio::spawn(async move {
            let _ = child.wait().await;
            {
                let mut state = controller.0.state.lock().await;
                state.ready = false;
                state.daemon = if controller.0.cancel.is_cancelled() { "closed" } else { "failed" };
            }
            let _ = stop_group(&group).await;
            controller.0.groups.lock().await.remove(&group.pid);
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if self.0.cancel.is_cancelled() {
                bail!("Controller closed");
            }
            if self.0.state.lock().await.daemon == "failed" {
                bail!("Native daemon exited");
            }
            if self.request(json!({"op":"daemon"}), Duration::from_secs(1), self.0.cancel.clone()).await.is_ok() {
                let mut state = self.0.state.lock().await;
                if state.daemon != "starting" || self.0.cancel.is_cancelled() {
                    bail!("Native daemon exited");
                }
                state.ready = true;
                state.daemon = "running";
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("Native startup timeout");
            }
            tokio::select! {
                biased;
                _ = self.0.cancel.cancelled() => bail!("Controller closed"),
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }
    }

    pub async fn request(&self, mut request: Value, timeout: Duration, cancel: CancellationToken) -> Result<Value> {
        let object = request.as_object_mut().ok_or_else(|| anyhow!("Invalid native request"))?;
        object.insert("expect_udid".into(), json!(self.0.device.serial_number));
        object.insert("cancel_on_close".into(), json!(true));
        object.insert("lang".into(), json!("en"));
        let operation = async {
            let mut socket = UnixStream::connect(&self.0.socket_path).await?;
            let mut request = serde_json::to_vec(&request)?;
            request.push(b'\n');
            socket.write_all(&request).await?;
            let mut bytes = Vec::new();
            let mut chunk = [0; 16384];
            loop {
                let length = socket.read(&mut chunk).await?;
                if length == 0 {
                    bail!("Native response interrupted; sent action is not retried");
                }
                if bytes.len() + length > MAX_RESPONSE {
                    bail!("Native response too large");
                }
                let previous = bytes.len();
                bytes.extend_from_slice(&chunk[..length]);
                if let Some(offset) = chunk[..length].iter().position(|byte| *byte == b'\n') {
                    let end = previous + offset;
                    let value: Value = serde_json::from_slice(&bytes[..end])?;
                    if !matches!(value.get("kind").and_then(Value::as_str), Some("text" | "image" | "error")) {
                        bail!("Invalid native response");
                    }
                    return Ok(value);
                }
            }
        };
        tokio::select! {
            biased;
            _ = self.0.cancel.cancelled() => bail!("Controller closed"),
            _ = cancel.cancelled() => bail!("Request cancelled; sent action is not retried"),
            result = tokio::time::timeout(timeout, operation) => result.map_err(|_| anyhow!("Native request timeout; sent action is not retried"))?,
        }
    }

    pub async fn snapshot(&self) -> Value {
        let state = self.0.state.lock().await;
        let mut setup = state.setup.clone();
        setup["transcript"] = json!(String::from_utf8_lossy(&state.transcript));
        json!({"ready":state.ready,"daemon":state.daemon,"setup":setup})
    }

    pub async fn start_setup(&self) -> Result<()> {
        let _setup = self.0.setup_lock.lock().await;
        if self.0.cancel.is_cancelled() {
            bail!("Controller closed");
        }
        if self.0.state.lock().await.setup["state"] == "running" {
            return Ok(());
        }
        // A previous process group's cleanup must finish before starting a new setup.
        let previous = self.0.state.lock().await.setup_pid;
        if let Some(pid) = previous {
            let group = self.0.groups.lock().await.get(&pid).cloned();
            if let Some(group) = group {
                stop_group(&group).await?;
            }
        }
        let (mut child, group) = self.spawn("setup").await?;
        let pid = group.pid;
        {
            let mut state = self.0.state.lock().await;
            state.setup = json!({"state":"running","transcript":"","exitCode":null});
            state.setup_pid = Some(pid);
            state.transcript.clear();
        }
        *self.0.stdin.lock().await = child.stdin.take().map(|stdin| (pid, stdin));
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(self.clone().read_setup(pid, stdout));
        }
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(self.clone().read_setup(pid, stderr));
        }
        let controller = self.clone();
        tokio::spawn(async move {
            let mut timed_out = false;
            let status = tokio::select! {
                result = child.wait() => result.ok(),
                _ = controller.0.cancel.cancelled() => {
                    let _ = stop_group(&group).await;
                    child.wait().await.ok()
                }
                _ = tokio::time::sleep(Duration::from_secs(15 * 60)) => {
                    timed_out = true;
                    let _ = stop_group(&group).await;
                    child.wait().await.ok()
                }
            };
            {
                let mut state = controller.0.state.lock().await;
                if state.setup_pid == Some(pid) {
                    state.setup["exitCode"] = json!(status.as_ref().and_then(|status| status.code()));
                    state.setup["state"] = json!(if controller.0.cancel.is_cancelled() {
                        "stopped"
                    } else if !timed_out && status.is_some_and(|status| status.success()) {
                        "complete"
                    } else {
                        "failed"
                    });
                    if timed_out {
                        state.setup["error"] = json!("Setup timeout");
                    }
                }
            }
            {
                let mut stdin = controller.0.stdin.lock().await;
                if stdin.as_ref().is_some_and(|(input_pid, _)| *input_pid == pid) {
                    *stdin = None;
                }
            }
            let _ = stop_group(&group).await;
            controller.0.groups.lock().await.remove(&pid);
        });
        Ok(())
    }

    async fn read_setup(self, pid: i32, mut input: impl AsyncRead + Unpin) {
        let mut chunk = [0; 4096];
        loop {
            let length = tokio::select! {
                biased;
                _ = self.0.cancel.cancelled() => break,
                result = input.read(&mut chunk) => match result { Ok(0) | Err(_) => break, Ok(length) => length },
            };
            let mut state = self.0.state.lock().await;
            if state.setup_pid != Some(pid) {
                break;
            }
            state.transcript.extend_from_slice(&chunk[..length]);
            if state.transcript.len() > MAX_TRANSCRIPT {
                let mut remove = state.transcript.len() - MAX_TRANSCRIPT;
                while remove < state.transcript.len() && state.transcript[remove] & 0xc0 == 0x80 {
                    remove += 1;
                }
                state.transcript.drain(..remove);
            }
        }
    }

    pub async fn setup_input(&self, line: &str) -> Result<()> {
        if line.encode_utf16().count() > 4096 || line.contains(['\r', '\n', '\0']) {
            bail!("Invalid setup input");
        }
        if self.0.cancel.is_cancelled() || self.0.state.lock().await.setup["state"] != "running" {
            bail!("Setup is not running");
        }
        let mut input = self.0.stdin.lock().await;
        let Some((_, stdin)) = input.as_mut() else { bail!("Setup is not running") };
        let bytes = format!("{line}\n");
        tokio::select! {
            biased;
            _ = self.0.cancel.cancelled() => bail!("Controller closed"),
            result = stdin.write_all(bytes.as_bytes()) => result?,
        }
        Ok(())
    }

    pub async fn close(&self) -> Result<()> {
        self.0.cancel.cancel();
        {
            let mut state = self.0.state.lock().await;
            state.ready = false;
            state.daemon = "closed";
            if state.setup["state"] == "running" {
                state.setup["state"] = json!("stopped");
            }
        }
        self.0.stdin.lock().await.take();
        let groups: Vec<_> = self.0.groups.lock().await.values().cloned().collect();
        let mut tasks = tokio::task::JoinSet::new();
        for group in groups {
            tasks.spawn(async move { stop_group(&group).await });
        }
        while let Some(result) = tasks.join_next().await {
            result??;
        }
        Ok(())
    }
}

async fn drain(mut input: impl AsyncRead + Unpin, path: PathBuf) {
    // Continue draining even if logging fails: a full pipe must never stop the daemon.
    let mut logging = true;
    let mut buffer = [0; 8192];
    while let Ok(length) = input.read(&mut buffer).await {
        if length == 0 {
            break;
        }
        if logging && let Err(error) = append_daemon_log(&path, &buffer[..length]).await {
            eprintln!("Could not persist native daemon diagnostics: {error}");
            logging = false;
        }
    }
}

async fn append_daemon_log(path: &std::path::Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let length = tokio::fs::metadata(path).await.map(|meta| meta.len()).unwrap_or(0);
    if length + bytes.len() as u64 > MAX_DAEMON_LOG {
        tokio::fs::rename(path, path.with_extension("log.1")).await?;
    }
    let mut file = tokio::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(path).await?;
    file.write_all(bytes).await?;
    // Tokio may still have a blocking write in flight after write_all returns.
    // Finish it before dropping the handle or reading/rotating this path again.
    file.flush().await
}

fn signal_group(pid: i32, signal: i32) -> Result<()> {
    // SAFETY: pid is an owned child process-group ID, positive and representable by pid_t.
    if unsafe { libc::kill(-pid, signal) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error.into())
}

fn group_alive(pid: i32) -> Result<bool> {
    // SAFETY: signal zero only probes an owned process-group ID.
    if unsafe { libc::kill(-pid, 0) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(false);
    }
    Err(error.into())
}

async fn stop_group(group: &Group) -> Result<()> {
    let _cleanup = group.cleanup.lock().await;
    if group.cleaned.load(Ordering::Acquire) {
        return Ok(());
    }
    signal_group(group.pid, libc::SIGTERM)?;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
    while group_alive(group.pid)? && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    if group_alive(group.pid)? {
        signal_group(group.pid, libc::SIGKILL)?;
        // The leader is reaped by its supervisor. Orphan zombies can remain in a container
        // until its init reaps them, but SIGKILL guarantees no member can execute again.
    }
    group.cleaned.store(true, Ordering::Release);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn daemon_diagnostics_are_private_and_rotate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs/daemon.stderr.log");
        append_daemon_log(&path, b"runner started\n").await.unwrap();
        let file = tokio::fs::OpenOptions::new().write(true).open(&path).await.unwrap();
        file.set_len(MAX_DAEMON_LOG).await.unwrap();
        append_daemon_log(&path, b"runner disconnected\n").await.unwrap();
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"runner disconnected\n");
        assert_eq!(tokio::fs::metadata(path.with_extension("log.1")).await.unwrap().len(), MAX_DAEMON_LOG);
        assert_eq!(tokio::fs::metadata(&path).await.unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[tokio::test]
    async fn failed_diagnostics_still_drain_the_child_pipe() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("not-a-directory");
        tokio::fs::write(&blocked, b"blocked").await.unwrap();
        let (mut writer, reader) = tokio::io::duplex(32);
        let drain = tokio::spawn(drain(reader, blocked.join("daemon.log")));
        tokio::time::timeout(Duration::from_secs(2), async {
            writer.write_all(&vec![b'x'; 32 * 1024]).await.unwrap();
            drop(writer);
            drain.await.unwrap();
        })
        .await
        .unwrap();
    }

    fn controller(socket: PathBuf, cancel: CancellationToken) -> NativeController {
        NativeController::new(
            "/bin/false".into(),
            Device {
                platform: crate::identity::Platform::Ios,
                serial_number: "a".repeat(40),
                vendor_id: 1452,
                product_id: 4776,
                product_name: None,
            },
            "/tmp/unused".into(),
            socket,
            "127.0.0.1:1".into(),
            cancel,
        )
    }

    #[tokio::test]
    async fn request_adds_identity_and_reads_fragmented_response() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("ctl.sock");
        let server = UnixListener::bind(&socket).unwrap();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = server.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let byte = stream.read_u8().await.unwrap();
                if byte == b'\n' {
                    break;
                }
                request.push(byte);
            }
            let request: Value = serde_json::from_slice(&request).unwrap();
            assert_eq!(request["expect_udid"], "a".repeat(40));
            assert_eq!(request["cancel_on_close"], true);
            assert_eq!(request["lang"], "en");
            stream.write_all(b"{\"kind\":").await.unwrap();
            tokio::task::yield_now().await;
            stream.write_all(b"\"text\",\"text\":\"ok\"}\n").await.unwrap();
        });
        let response = controller(socket, CancellationToken::new())
            .request(json!({"op":"status"}), Duration::from_secs(1), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(response["text"], "ok");
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn setup_accepts_one_line_without_recording_it() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("native-fixture");
        tokio::fs::write(
            &script,
            "#!/bin/sh\nprintf 'fixture prompt\\n'\nIFS= read -r value\nprintf 'setup complete\\n'\n",
        )
        .await
        .unwrap();
        tokio::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).await.unwrap();
        let ctl = NativeController::new(
            script,
            Device {
                platform: crate::identity::Platform::Ios,
                serial_number: "a".repeat(40),
                vendor_id: 1452,
                product_id: 4776,
                product_name: None,
            },
            dir.path().join("data"),
            dir.path().join("ctl.sock"),
            "127.0.0.1:1".into(),
            CancellationToken::new(),
        );
        ctl.start_setup().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !ctl.snapshot().await["setup"]["transcript"].as_str().unwrap().contains("fixture prompt") {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(ctl.setup_input("bad\nline").await.is_err());
        ctl.setup_input("private input").await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while ctl.snapshot().await["setup"]["state"] == "running" {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let snapshot = ctl.snapshot().await;
        assert_eq!(snapshot["setup"]["state"], "complete");
        assert!(!snapshot.to_string().contains("private input"));
        ctl.close().await.unwrap();
    }

    #[tokio::test]
    async fn cleanup_escalates_to_kill_an_uncooperative_process_group() {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "trap '' TERM; sleep 30 & wait"])
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command.spawn().unwrap();
        let group = Group { pid: child.id().unwrap() as i32, cleanup: Mutex::new(()), cleaned: AtomicBool::new(false) };
        tokio::time::sleep(Duration::from_millis(40)).await;
        tokio::time::timeout(Duration::from_secs(3), stop_group(&group)).await.unwrap().unwrap();
        let status = tokio::time::timeout(Duration::from_secs(1), child.wait()).await.unwrap().unwrap();
        assert!(!status.success());
        assert!(group.cleaned.load(Ordering::Acquire));
        // Repeated cleanup is idempotent even when the group has already been reaped.
        stop_group(&group).await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_closes_sent_socket_without_replay() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("ctl.sock");
        let server = UnixListener::bind(&socket).unwrap();
        let cancel = CancellationToken::new();
        let request = tokio::spawn({
            let ctl = controller(socket, CancellationToken::new());
            let cancel = cancel.clone();
            async move { ctl.request(json!({"op":"tap","x":1,"y":1}), Duration::from_secs(20), cancel).await }
        });
        let (mut stream, _) = server.accept().await.unwrap();
        let mut chunk = [0; 4096];
        assert!(stream.read(&mut chunk).await.unwrap() > 0);
        cancel.cancel();
        assert!(request.await.unwrap().is_err());
        assert_eq!(stream.read(&mut chunk).await.unwrap(), 0);
        assert!(tokio::time::timeout(Duration::from_millis(20), server.accept()).await.is_err());
    }
}
