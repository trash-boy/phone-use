use crate::{
    android::AndroidController,
    commands::command_request,
    controller::Controller,
    error::{ApiError, Result},
    identity::{Device, Platform, canonical_uid, device_uid, validate_device},
    model::Model,
    mux::{PairRecord, VirtualMux, read_pair_record},
    native::NativeController,
    relay::{Frame, MAX_PAYLOAD, Relay},
    tasks::TaskManager,
};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{
        FromRequestParts, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{Method, Request, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex, OnceCell, mpsc, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub struct Config {
    pub token: String,
    pub origins: HashSet<String>,
    pub public_url: Option<reqwest::Url>,
    pub runtime: PathBuf,
    pub native: PathBuf,
    pub web_dist: PathBuf,
}
pub struct Gateway {
    pub config: Config,
    pub model: Model,
    registry: Mutex<Registry>,
    pub cancel: CancellationToken,
}
#[derive(Default)]
struct Registry {
    sessions: HashMap<String, Arc<Session>>,
    owners: HashMap<String, String>,
}
/// A job result. Observations carry their screenshot decoded once.
#[derive(Clone, Debug)]
pub(crate) struct Output {
    pub value: Value,
    pub jpeg: Option<Arc<[u8]>>,
}
impl Output {
    fn value(value: Value) -> Self {
        Self { value, jpeg: None }
    }
}
type JobResult = watch::Sender<Option<Result<Output>>>;
struct CachedCommand {
    fingerprint: String,
    result: JobResult,
}
#[derive(Default)]
struct Commands {
    cached: HashMap<String, CachedCommand>,
    order: VecDeque<String>,
    used: HashSet<String>,
}
struct Job {
    request: JobRequest,
    result: JobResult,
    cancel: CancellationToken,
    queued_at: Instant,
    deadline: Option<ictl_protocol::ActionDeadline>,
    admission: Option<crate::control::Admission>,
}
enum JobRequest {
    Command(Value),
    Observe { include_elements: bool },
    Preview,
}
struct CapturedScreen {
    at: Instant,
    fresh: bool,
    image: Value,
    jpeg: Option<Arc<[u8]>>,
}
pub struct Session {
    pub id: String,
    pub device: Device,
    namespace: String,
    token: String,
    gateway: Weak<Gateway>,
    pub cancel: CancellationToken,
    hello: CancellationToken,
    state: Mutex<String>,
    attached: AtomicBool,
    pub controller: OnceCell<Controller>,
    mux: Mutex<Option<VirtualMux>>,
    relay: OnceCell<Relay>,
    pair: Mutex<Option<PairRecord>>,
    socket_dir: PathBuf,
    data_dir: PathBuf,
    commands: Mutex<Commands>,
    pub(crate) control: crate::control::Control,
    jobs: mpsc::Sender<Job>,
    latest_screen: Mutex<Option<CapturedScreen>>,
    preview_lock: Mutex<()>,
    mjpeg: crate::mjpeg::Preview,
    preview_ticket: String,
    attachments: crate::attachments::Attachments,
    close_lock: Mutex<bool>,
    close_reason: crate::heartbeat::CloseReason,
    pub tasks: TaskManager,
}

fn id() -> String {
    Uuid::new_v4().to_string()
}
fn equal(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}
fn native_error(error: anyhow::Error) -> ApiError {
    if crate::error::recoverable_wda_error(&error.to_string()) {
        return ApiError::unavailable("Device response unavailable; action result is uncertain");
    }
    ApiError::unavailable("Native operation failed; sent actions are not retried")
}
async fn private_dir(path: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::create_dir_all(path).await?;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await?;
    Ok(())
}
fn reply(status: u16, value: Value) -> Response {
    let mut response = (StatusCode::from_u16(status).unwrap(), Json(value)).into_response();
    response.headers_mut().insert("cache-control", "no-store".parse().unwrap());
    response
}
impl Gateway {
    pub async fn new(config: Config, model: Model) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(!config.token.trim().is_empty(), "CONTROL_ACCESS_TOKEN required");
        anyhow::ensure!(!config.origins.is_empty(), "Exact ALLOWED_ORIGINS required");
        for origin in &config.origins {
            let u = reqwest::Url::parse(origin)?;
            anyhow::ensure!(
                ["http", "https"].contains(&u.scheme()) && u.origin().ascii_serialization() == *origin,
                "Exact ALLOWED_ORIGINS required"
            );
        }
        for p in [&config.runtime, &config.runtime.join("data"), &config.runtime.join("record-map")] {
            private_dir(p).await?;
        }
        Ok(Arc::new(Self {
            config,
            model,
            registry: Mutex::new(Registry::default()),
            cancel: CancellationToken::new(),
        }))
    }
    pub fn router(self: &Arc<Self>) -> Router {
        Router::new().fallback(handle).with_state(self.clone())
    }
    fn record_path(&self, record: &PairRecord) -> PathBuf {
        let mut hash = Sha256::new();
        hash.update(self.config.token.as_bytes());
        hash.update(record.pair_record_base64.as_bytes());
        self.config.runtime.join("record-map").join(format!("{}.json", hex::encode(hash.finalize())))
    }
    async fn saved_namespace(&self, record: &PairRecord) -> Result<Option<String>> {
        match tokio::fs::read(self.record_path(record)).await {
            Ok(bytes) => {
                let parsed: Value = serde_json::from_slice(&bytes)
                    .map_err(|_| ApiError::new(500, "Could not read native resume state"))?;
                let namespace = parsed["namespace"]
                    .as_str()
                    .filter(|s| Uuid::parse_str(s).is_ok_and(|u| u.to_string() == *s))
                    .ok_or_else(|| ApiError::new(500, "Could not read native resume state"))?;
                Ok(Some(namespace.to_owned()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(ApiError::new(500, "Could not read native resume state")),
        }
    }
    async fn save_mapping(&self, record: &PairRecord, namespace: &str, registry: &Registry) -> Result<()> {
        if let Some(previous) = self.saved_namespace(record).await?
            && previous != namespace
            && registry.owners.contains_key(&previous)
        {
            return Err(ApiError::new(409, "Native signing state has an active owner"));
        }
        let target = self.record_path(record);
        let temporary = target.with_extension(format!("{}.tmp", id()));
        let bytes = serde_json::to_vec(&json!({"namespace":namespace})).unwrap();
        let write = async {
            use tokio::io::AsyncWriteExt;
            let mut file =
                tokio::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary).await?;
            file.write_all(&bytes).await?;
            file.sync_all().await?;
            drop(file);
            tokio::fs::rename(&temporary, &target).await
        }
        .await;
        if write.is_err() {
            let _ = tokio::fs::remove_file(&temporary).await;
        }
        write.map_err(|_| ApiError::new(500, "Could not save native resume state"))
    }
    async fn create_session(self: &Arc<Self>, device: Device, mut pair: Option<PairRecord>) -> Result<Arc<Session>> {
        let mut registry = self.registry.lock().await;
        if self.cancel.is_cancelled() {
            return Err(ApiError::new(503, "Gateway closing"));
        }
        if registry.sessions.len() >= 32 {
            return Err(ApiError::new(429, "Session limit reached"));
        }
        let mut namespace = if device.platform == Platform::Android {
            format!("android-{}", hex::encode(Sha256::digest(device.serial_number.as_bytes())))
        } else {
            id()
        };
        if device.platform == Platform::Android && pair.is_some() {
            return Err(ApiError::new(400, "Android does not use Apple pairing records"));
        }
        if let Some(ref mut record) = pair {
            read_pair_record(record, &device.serial_number).map_err(|_| ApiError::new(400, "Invalid pair record"))?;
            record.udid = canonical_uid(&record.udid).map_err(|_| ApiError::new(400, "Invalid pair record"))?;
            if let Some(saved) = self.saved_namespace(record).await? {
                namespace = saved;
            }
        }
        if registry.owners.contains_key(&namespace) {
            return Err(ApiError::new(409, "Native signing state has an active owner"));
        }
        if let Some(record) = &pair {
            self.save_mapping(record, &namespace, &registry).await?;
        }
        let sid = id();
        let socket_dir = std::env::temp_dir().join(format!("ictl-{}", &Uuid::new_v4().simple().to_string()[..12]));
        tokio::fs::create_dir(&socket_dir)
            .await
            .map_err(|_| ApiError::unavailable("Could not create control directory"))?;
        private_dir(&socket_dir).await.map_err(|_| ApiError::unavailable("Could not secure control directory"))?;
        let (jobs, receiver) = mpsc::channel(256);
        let session = Arc::new(Session {
            id: sid.clone(),
            device,
            namespace: namespace.clone(),
            token: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
            gateway: Arc::downgrade(self),
            cancel: self.cancel.child_token(),
            hello: CancellationToken::new(),
            state: Mutex::new("waiting_for_relay".into()),
            attached: AtomicBool::new(false),
            controller: OnceCell::new(),
            mux: Mutex::new(None),
            relay: OnceCell::new(),
            pair: Mutex::new(pair),
            attachments: crate::attachments::Attachments::new(socket_dir.clone()),
            socket_dir,
            data_dir: self.config.runtime.join("data").join(&namespace),
            commands: Mutex::new(Commands::default()),
            control: crate::control::Control::default(),
            jobs,
            latest_screen: Mutex::new(None),
            preview_lock: Mutex::new(()),
            mjpeg: crate::mjpeg::Preview::default(),
            preview_ticket: format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
            close_lock: Mutex::new(false),
            close_reason: crate::heartbeat::CloseReason::default(),
            tasks: TaskManager::new(),
        });
        registry.owners.insert(namespace, sid.clone());
        registry.sessions.insert(sid, session.clone());
        tokio::spawn(session.clone().work(receiver));
        let waiting = session.clone();
        tokio::spawn(async move {
            tokio::select! {_=waiting.cancel.cancelled()=>{},_=waiting.hello.cancelled()=>return,_=tokio::time::sleep(Duration::from_secs(300))=>{}}
            let reason = if waiting.cancel.is_cancelled() { "cancel" } else { "hello_timeout" };
            waiting.close_with_reason(reason).await;
        });
        Ok(session)
    }
    pub async fn close(&self) {
        let sessions: Vec<_> = self.registry.lock().await.sessions.values().cloned().collect();
        for session in &sessions {
            session.close_reason.record("server_shutdown");
        }
        self.cancel.cancel();
        futures_util::future::join_all(sessions.iter().map(|s| s.close_with_reason("server_shutdown"))).await;
    }
}
impl Session {
    pub async fn close(&self) {
        self.close_with_reason("unattributed").await;
    }
    pub async fn close_with_reason(&self, reason: &'static str) {
        self.close_reason.record(reason);
        self.control.close();
        self.cancel.cancel();
        let mut done = self.close_lock.lock().await;
        if *done {
            return;
        }
        eprintln!(
            "{} event=session_close_requested session={} namespace={} reason={}",
            chrono::Utc::now().to_rfc3339(),
            self.id,
            self.namespace,
            self.close_reason.get()
        );
        *self.state.lock().await = "closed".into();
        self.tasks.disconnect().await;
        if let Some(relay) = self.relay.get() {
            relay.close().await;
        }
        if let Some(native) = self.controller.get() {
            let _ = native.close().await;
        }
        if let Some(mux) = self.mux.lock().await.take() {
            mux.close().await;
        }
        let _ = tokio::fs::remove_dir_all(&self.socket_dir).await;
        if let Some(g) = self.gateway.upgrade() {
            g.model.close_session(&self.id);
            let mut registry = g.registry.lock().await;
            registry.sessions.remove(&self.id);
            registry.owners.remove(&self.namespace);
        }
        self.commands.lock().await.cached.clear();
        *self.latest_screen.lock().await = None;
        *self.pair.lock().await = None;
        *done = true;
    }
    pub async fn ready(&self) -> Result<&Controller> {
        if self.cancel.is_cancelled() {
            return Err(ApiError::new(410, "Session closed"));
        }
        let controller = self.controller.get().ok_or_else(|| ApiError::new(409, "Device relay is not connected"))?;
        let probe = async {
            loop {
                let snapshot = controller.snapshot().await;
                if snapshot["ready"] == true {
                    return Ok(controller);
                }
                if snapshot["daemon"] == "failed" || snapshot["daemon"] == "closed" {
                    return Err(ApiError::unavailable("Native control transport is not ready"));
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        };
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(ApiError::new(410, "Session closed")),
            result = tokio::time::timeout(Duration::from_secs(30), probe) => {
                result.unwrap_or_else(|_| Err(ApiError::unavailable("Native control transport is not ready")))
            }
        }
    }
    async fn wait_result(
        &self,
        mut result: watch::Receiver<Option<Result<Output>>>,
        cancel: CancellationToken,
    ) -> Result<Output> {
        loop {
            if let Some(value) = result.borrow().clone() {
                return value;
            }
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Err(ApiError::new(410, "Session closed")),
                _ = cancel.cancelled() => return Err(ApiError::new(409, "Request cancelled; sent action is not retried")),
                changed = result.changed() => {
                    if changed.is_err() { return Err(ApiError::unavailable("Operation unavailable")); }
                }
            }
        }
    }
    pub async fn run_command(&self, input: Value, cancel: CancellationToken) -> Result<Value> {
        self.run_command_at(
            input,
            cancel,
            Instant::now(),
            ictl_protocol::ActionDeadline::admit().map_err(|_| ApiError::unavailable("Action clock unavailable"))?,
        )
        .await
    }
    async fn run_command_at(
        &self,
        input: Value,
        cancel: CancellationToken,
        entered: Instant,
        deadline: ictl_protocol::ActionDeadline,
    ) -> Result<Value> {
        self.run_command_admitted(input, cancel, entered, deadline, None).await
    }
    pub(crate) async fn run_task_command(
        &self,
        input: Value,
        cancel: CancellationToken,
        lease: &crate::control::TaskLease,
    ) -> Result<Value> {
        let entered = Instant::now();
        let deadline =
            ictl_protocol::ActionDeadline::admit().map_err(|_| ApiError::unavailable("Action clock unavailable"))?;
        let _pause = if input["tool"] == "iphone_screenshot" { Some(self.mjpeg.pause().await) } else { None };
        self.run_command_admitted(input, cancel, entered, deadline, Some(lease)).await
    }
    fn manual_admission(&self) -> Result<crate::control::Admission> {
        self.control.manual().inspect_err(|_| {
            eprintln!(
                "{} event=control_rejected session={} namespace={} category=control_busy stage=manual_admission",
                chrono::Utc::now().to_rfc3339(),
                self.id,
                self.namespace
            );
        })
    }
    pub(crate) fn claim_task(&self) -> Result<crate::control::TaskLease> {
        self.control.task().inspect_err(|_| {
            eprintln!(
                "{} event=control_rejected session={} namespace={} category=control_busy stage=task_admission",
                chrono::Utc::now().to_rfc3339(),
                self.id,
                self.namespace
            );
        })
    }
    async fn run_command_admitted(
        &self,
        input: Value,
        cancel: CancellationToken,
        entered: Instant,
        deadline: ictl_protocol::ActionDeadline,
        task: Option<&crate::control::TaskLease>,
    ) -> Result<Value> {
        if device_uid(self.device.platform, input["deviceId"].as_str().unwrap_or("")).ok().as_deref()
            != Some(self.device.serial_number.as_str())
        {
            return Err(ApiError::new(409, "Device identity mismatch"));
        }
        let request_id = input["requestId"]
            .as_str()
            .filter(|s| {
                !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
            })
            .ok_or_else(|| ApiError::new(400, "Invalid requestId"))?;
        let request = command_request(input["tool"].as_str().unwrap_or(""), input.get("args").unwrap_or(&json!({})))?;
        if self.device.platform == Platform::Ios && request["op"] == "press_button" && request["button"] == "back" {
            return Err(ApiError::new(400, "Back is supported on Android only"));
        }
        let typed: ictl_protocol::Request =
            serde_json::from_value(request.clone()).map_err(|_| ApiError::new(400, "Invalid command"))?;
        let deadline = typed.requires_control().then_some(deadline);
        let fingerprint = request.to_string();
        let mut commands = self.commands.lock().await;
        let receiver = if let Some(entry) = commands.cached.get(request_id) {
            if entry.fingerprint != fingerprint {
                return Err(ApiError::new(409, "requestId already used for another command"));
            }
            entry.result.subscribe()
        } else {
            if commands.used.contains(request_id) {
                return Err(ApiError::new(409, "requestId result expired; command cannot be replayed"));
            }
            if commands.used.len() >= 10000 {
                return Err(ApiError::new(429, "Session command-ID limit reached"));
            }
            if commands.cached.len() >= 256 {
                let expired = commands
                    .order
                    .iter()
                    .find(|key| commands.cached.get(*key).is_some_and(|v| v.result.borrow().is_some()))
                    .cloned()
                    .ok_or_else(|| ApiError::new(429, "Command queue full"))?;
                commands.cached.remove(&expired);
                commands.order.retain(|k| k != &expired);
            }
            let admission = if let Some(task) = task {
                Some(self.control.task_job(task)?)
            } else if typed.requires_control() {
                Some(self.manual_admission()?)
            } else {
                None
            };
            let (sender, receiver) = watch::channel(None);
            self.jobs
                .try_send(Job {
                    request: JobRequest::Command(request),
                    result: sender.clone(),
                    cancel: cancel.clone(),
                    queued_at: entered,
                    deadline,
                    admission,
                })
                .map_err(|_| ApiError::new(429, "Command queue full"))?;
            commands.used.insert(request_id.to_owned());
            commands.order.push_back(request_id.to_owned());
            commands.cached.insert(request_id.to_owned(), CachedCommand { fingerprint, result: sender });
            receiver
        };
        drop(commands);
        self.wait_result(receiver, cancel).await.map(|output| output.value)
    }
    pub async fn screen(&self, cancel: CancellationToken) -> Result<Value> {
        let entered = Instant::now();
        let deadline =
            ictl_protocol::ActionDeadline::admit().map_err(|_| ApiError::unavailable("Action clock unavailable"))?;
        self.screen_admitted(cancel, None, entered, deadline, true).await.map(|output| output.value)
    }
    pub(crate) async fn task_screen(
        &self,
        cancel: CancellationToken,
        lease: &crate::control::TaskLease,
        include_elements: bool,
    ) -> Result<Output> {
        let entered = Instant::now();
        let deadline =
            ictl_protocol::ActionDeadline::admit().map_err(|_| ApiError::unavailable("Action clock unavailable"))?;
        // An opt-in live view would hold WDA's single-client MJPEG server and refuse this capture.
        let _pause = self.mjpeg.pause().await;
        self.screen_admitted(cancel, Some(lease), entered, deadline, include_elements).await
    }
    async fn screen_admitted(
        &self,
        cancel: CancellationToken,
        task: Option<&crate::control::TaskLease>,
        queued_at: Instant,
        deadline: ictl_protocol::ActionDeadline,
        include_elements: bool,
    ) -> Result<Output> {
        let admission = Some(match task {
            Some(task) => self.control.task_job(task)?,
            None => self.manual_admission()?,
        });
        let (result, receiver) = watch::channel(None);
        self.jobs
            .try_send(Job {
                request: JobRequest::Observe { include_elements },
                result,
                cancel: cancel.clone(),
                queued_at,
                deadline: Some(deadline),
                admission,
            })
            .map_err(|_| ApiError::new(429, "Command queue full"))?;
        self.wait_result(receiver, cancel).await
    }
    // During a task, display its latest real observation without touching the device.
    // Null means the first task observation has not arrived yet.
    pub async fn preview(&self) -> Result<Value> {
        let entered = Instant::now();
        let deadline =
            ictl_protocol::ActionDeadline::admit().map_err(|_| ApiError::unavailable("Action clock unavailable"))?;
        self.preview_at(entered, deadline).await
    }
    async fn preview_at(&self, queued_at: Instant, deadline: ictl_protocol::ActionDeadline) -> Result<Value> {
        if self.control.task_owned() {
            return Ok(self.cached_preview().await);
        }
        let _guard = self.preview_lock.lock().await;
        if self.control.task_owned() {
            return Ok(self.cached_preview().await);
        }
        if let Some(frame) = self.latest_screen.lock().await.as_ref()
            && frame.fresh
            && frame.at.elapsed() < Duration::from_millis(250)
        {
            return Ok(frame.image.clone());
        }
        let admission = match self.manual_admission() {
            Ok(admission) => Some(admission),
            Err(error) if error.status == 409 => return Ok(self.cached_preview().await),
            Err(error) => return Err(error),
        };
        let (result, receiver) = watch::channel(None);
        self.jobs
            .try_send(Job {
                request: JobRequest::Preview,
                result,
                cancel: self.cancel.clone(),
                queued_at,
                deadline: Some(deadline),
                admission,
            })
            .map_err(|_| ApiError::new(429, "Command queue full"))?;
        self.wait_result(receiver, self.cancel.clone()).await.map(|output| output.value)
    }
    /// The latest captured frame's image bytes, capturing first under the same
    /// rules as the JSON preview (never during a task, which reuses its frames).
    async fn screen_bytes(&self, queued_at: Instant, deadline: ictl_protocol::ActionDeadline) -> Result<Arc<[u8]>> {
        self.preview_at(queued_at, deadline).await?;
        self.latest_screen
            .lock()
            .await
            .as_ref()
            .and_then(|frame| frame.jpeg.clone())
            .ok_or_else(|| ApiError::new(404, "No captured screen"))
    }
    async fn cached_preview(&self) -> Value {
        self.latest_screen.lock().await.as_ref().map(|s| s.image.clone()).unwrap_or(Value::Null)
    }
    async fn work(self: Arc<Self>, mut jobs: mpsc::Receiver<Job>) {
        loop {
            let job = tokio::select! {biased;_=self.cancel.cancelled()=>break,job=jobs.recv()=>match job{Some(j)=>j,None=>break}};
            let queue_ms = job.queued_at.elapsed().as_millis() as u64;
            let mut result =
                if job.cancel.is_cancelled() || job.admission.as_ref().is_some_and(|a| !a.valid(&self.control)) {
                    Err(ApiError::new(409, "Request cancelled; sent action is not retried"))
                } else {
                    self.execute(job.request, job.cancel, job.deadline, job.queued_at).await
                };
            if let Ok(Output { value, .. }) = &mut result
                && value.is_object()
            {
                value["timings"]["queueMs"] = json!(queue_ms);
            }
            job.result.send_replace(Some(result));
        }
        while let Ok(job) = jobs.try_recv() {
            job.result.send_replace(Some(Err(ApiError::new(410, "Session closed"))));
        }
    }
    fn check_deadline(
        &self,
        deadline: Option<ictl_protocol::ActionDeadline>,
        entered: Instant,
        stage: &str,
    ) -> Result<()> {
        if deadline.is_some_and(|deadline| {
            entered.elapsed() >= ictl_protocol::ACTION_LIFETIME || deadline.expired().unwrap_or(true)
        }) {
            eprintln!(
                "{} event=action_rejected session={} namespace={} category=expired stage={stage} queue_age_ms={} budget_ms=30000",
                chrono::Utc::now().to_rfc3339(),
                self.id,
                self.namespace,
                entered.elapsed().as_millis()
            );
            return Err(ApiError::new(409, "Action expired before dispatch; request was not sent"));
        }
        Ok(())
    }
    async fn execute(
        &self,
        request: JobRequest,
        cancel: CancellationToken,
        deadline: Option<ictl_protocol::ActionDeadline>,
        entered: Instant,
    ) -> Result<Output> {
        self.check_deadline(deadline, entered, "gateway_queue")?;
        let observe = matches!(request, JobRequest::Observe { .. });
        let include_elements = matches!(request, JobRequest::Observe { include_elements: true });
        if matches!(request, JobRequest::Preview) && self.control.task_owned() {
            return Ok(Output::value(self.cached_preview().await));
        }
        let controller = self.ready().await;
        self.check_deadline(deadline, entered, "gateway_ready")?;
        let controller = controller?;
        let native = |r| controller.request_with_deadline(r, Duration::from_secs(45), cancel.clone(), deadline);
        if let JobRequest::Command(request) = request {
            if let Some(frame) = self.latest_screen.lock().await.as_mut() {
                frame.fresh = false;
            }
            return native(request).await.map(Output::value).map_err(native_error);
        }
        let capture = Instant::now();
        let mut result = if observe {
            let response = native(json!({"op":"observe","limit":if include_elements { 80 } else { 0 },"snapshot_timeout_ms":if include_elements { 1000 } else { 0 }}))
                .await
                .map_err(|_| ApiError::unavailable("WDA observation unavailable"))?;
            if response["kind"] != "text" {
                if crate::error::recoverable_wda_error(response["message"].as_str().unwrap_or("")) {
                    return Err(ApiError::unavailable("WDA observation unavailable"));
                }
                return Err(ApiError::unavailable("Observation unavailable"));
            }
            let value: Value = serde_json::from_str(response["text"].as_str().unwrap_or(""))
                .map_err(|_| ApiError::unavailable("Invalid native observation"))?;
            if value["deviceId"] != self.device.serial_number
                || !value["imageBase64"].is_string()
                || !value["caption"].is_string()
            {
                return Err(ApiError::unavailable("Invalid native observation identity or image"));
            }
            value
        } else {
            let image = native(json!({"op":"screenshot","region":null}))
                .await
                .map_err(|_| ApiError::unavailable("暂时无法读取设备截图，请稍后刷新画面。"))?;
            if image["kind"] != "image" {
                return Err(ApiError::unavailable("Screenshot unavailable"));
            }
            json!({"imageBase64":image["jpeg_base64"],"caption":image["caption"],"coordinateMapping":image["coordinateMapping"],"elementsText":"","deviceId":self.device.serial_number,
                "timings":{"screenshotMs":capture.elapsed().as_millis() as u64}})
        };
        result["capturedAt"] = json!(chrono::Utc::now().to_rfc3339());
        // Decoded once here; the task loop, OCR and the binary screen route reuse these bytes.
        let jpeg = crate::model::decode_image(&result);
        let mut preview = result.clone();
        preview["elementsText"] = json!("");
        preview.as_object_mut().unwrap().remove("elementsError");
        *self.latest_screen.lock().await =
            Some(CapturedScreen { at: Instant::now(), fresh: true, image: preview, jpeg: jpeg.clone() });
        Ok(Output { value: result, jpeg })
    }
    async fn snapshot(&self) -> Value {
        let native = match self.controller.get() {
            Some(c) => c.snapshot().await,
            None => json!({"ready":false,"setup":{"state":"idle","transcript":"","exitCode":null}}),
        };
        let daemon = if native["ready"] == true {
            match self
                .controller
                .get()
                .unwrap()
                .request(json!({"op":"daemon"}), Duration::from_secs(3), self.cancel.clone())
                .await
            {
                Ok(v) if v["kind"] == "text" => {
                    serde_json::from_str::<Value>(v["text"].as_str().unwrap_or("")).unwrap_or(Value::Null)
                }
                _ => Value::Null,
            }
        } else {
            Value::Null
        };
        json!({
            "id": self.id,
            "deviceId": self.device.serial_number,
            "platform": self.device.platform,
            "state": *self.state.lock().await,
            "ready": native["ready"],
            "daemon": daemon,
            "setup": native["setup"],
            "task": self.tasks.snapshot().await,
            "preview": self.mjpeg.status().await
        })
    }
    async fn hello(self: &Arc<Self>, frame: Frame, outbound: mpsc::Sender<Frame>) -> anyhow::Result<()> {
        anyhow::ensure!(frame.kind == 1 && frame.stream_id == 0, "DeviceHello required first");
        let device = validate_device(&serde_json::from_slice(&frame.payload)?)?;
        anyhow::ensure!(
            device.serial_number == self.device.serial_number
                && device.product_id == self.device.product_id
                && device.vendor_id == self.device.vendor_id,
            "DeviceHello identity mismatch"
        );
        self.hello.cancel();
        *self.state.lock().await = "connecting".into();
        if self.device.platform == Platform::Android {
            anyhow::ensure!(device.platform == Platform::Android, "Device platform mismatch");
            let controller =
                AndroidController::new(self.device.serial_number.clone(), outbound.clone(), self.cancel.clone());
            self.controller
                .set(Controller::Android(controller))
                .map_err(|_| anyhow::anyhow!("Android controller already started"))?;
            *self.state.lock().await = "connected".into();
            outbound
                .send(Frame::json(17, 0, 0, &json!({"ready":true,"deviceId":self.device.serial_number})))
                .await
                .map_err(|_| anyhow::anyhow!("Android relay closed"))?;
            return Ok(());
        }
        anyhow::ensure!(device.platform == Platform::Ios, "Device platform mismatch");
        let relay = Relay::new(outbound.clone(), self.cancel.clone(), self.id.clone());
        self.relay.set(relay.clone()).map_err(|_| anyhow::anyhow!("Duplicate DeviceHello"))?;
        let (pair_sender, mut records) = mpsc::channel::<PairRecord>(8);
        let mux = VirtualMux::start(
            self.device.clone(),
            self.pair.lock().await.clone(),
            relay,
            pair_sender,
            self.cancel.clone(),
            &self.socket_dir.join("mux.sock"),
        )
        .await?;
        let address = mux.address();
        *self.mux.lock().await = Some(mux);
        let gateway = self.gateway.upgrade().ok_or_else(|| anyhow::anyhow!("Gateway closed"))?;
        let controller = NativeController::new(
            gateway.config.native.clone(),
            self.device.clone(),
            self.data_dir.clone(),
            self.socket_dir.join("ctl.sock"),
            address,
            self.cancel.clone(),
        )
        .with_signing_runtime(gateway.config.runtime.clone());
        self.controller
            .set(Controller::Ios(controller.clone()))
            .map_err(|_| anyhow::anyhow!("Native already started"))?;
        let session = self.clone();
        tokio::spawn(async move {
            let result = controller.start().await;
            if !session.cancel.is_cancelled() {
                if result.is_ok() {
                    *session.state.lock().await = "connected".into();
                } else {
                    eprintln!(
                        "{} event=session_close_requested session={} reason=native_start_failed",
                        chrono::Utc::now().to_rfc3339(),
                        session.id
                    );
                    session.close_with_reason("native_start_failed").await;
                }
            }
        });
        let session = self.clone();
        tokio::spawn(async move {
            loop {
                let record = tokio::select! {biased;_=session.cancel.cancelled()=>break,r=records.recv()=>match r{Some(r)=>r,None=>break}};
                let Some(g) = session.gateway.upgrade() else {
                    break;
                };
                let registry = g.registry.lock().await;
                let result = g.save_mapping(&record, &session.namespace, &registry).await;
                drop(registry);
                if result.is_err() {
                    eprintln!(
                        "{} event=session_close_requested session={} reason=pair_mapping_save_failed",
                        chrono::Utc::now().to_rfc3339(),
                        session.id
                    );
                    session.close_with_reason("pair_mapping_save_failed").await;
                    break;
                }
                *session.pair.lock().await = Some(record.clone());
                if outbound.send(Frame::json(12, 0, 0, &serde_json::to_value(record).unwrap())).await.is_err() {
                    break;
                }
            }
        });
        Ok(())
    }
    async fn websocket(self: Arc<Self>, socket: WebSocket) {
        let (sink, mut source) = socket.split();
        let (outbound, frames) = mpsc::channel::<Frame>(64);
        let alive = Arc::new(AtomicBool::new(true));
        let writer_alive = alive.clone();
        let cancel = self.cancel.clone();
        let writer = tokio::spawn(crate::heartbeat::write_frames(
            sink,
            frames,
            writer_alive,
            cancel,
            self.id.clone(),
            self.close_reason.clone(),
        ));
        let deadline = tokio::time::sleep(Duration::from_secs(10));
        tokio::pin!(deadline);
        let mut hello = false;
        let started = Instant::now();
        let mut frames_received = 0u64;
        let mut bytes_received = 0u64;
        eprintln!(
            "{} event=ws_opened session={} namespace={}",
            chrono::Utc::now().to_rfc3339(),
            self.id,
            self.namespace
        );
        let reason = loop {
            let next = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => break "cancel",
                _ = &mut deadline, if !hello => break "hello_timeout",
                next = source.next() => next,
            };
            if let Some(Ok(Message::Binary(bytes))) = &next {
                frames_received += 1;
                bytes_received += bytes.len() as u64;
            }
            let result = match next {
                Some(Ok(Message::Binary(bytes))) => match Frame::decode(&bytes) {
                    Ok(frame) if !hello => {
                        hello = true;
                        self.hello(frame, outbound.clone()).await
                    }
                    Ok(frame) => match frame.kind {
                        2..=5 if frame.kind != 2 => match self.relay.get() {
                            Some(relay) => relay.handle(frame).await,
                            None => Err(anyhow::anyhow!("USB mux frames are invalid for Android")),
                        },
                        15 => match self.controller.get() {
                            Some(Controller::Android(c)) => c.handle(frame).await,
                            _ => Err(anyhow::anyhow!("Android result on non-Android session")),
                        },
                        8 => outbound
                            .send(Frame::json(9, frame.request_id, 0, &Value::Null))
                            .await
                            .map_err(|_| anyhow::anyhow!("Relay closed")),
                        9 => Ok(()),
                        10 if self.device.platform == Platform::Ios => {
                            let entered = Instant::now();
                            let deadline = ictl_protocol::ActionDeadline::admit();
                            if let Some(record) = self.pair.lock().await.clone() {
                                outbound
                                    .send(Frame::json(12, 0, 0, &serde_json::to_value(record).unwrap()))
                                    .await
                                    .map_err(|_| anyhow::anyhow!("Relay closed"))
                            } else {
                                let admitted = self.manual_admission().and_then(|admission| {
                                    deadline
                                        .map(|deadline| (admission, deadline))
                                        .map_err(|_| ApiError::unavailable("Control admission unavailable"))
                                });
                                match admitted {
                                    Err(_) => outbound.send(Frame::json(7, frame.request_id, 0, &json!({"error":"Session control is busy or unavailable; stop the task before setup"}))).await
                                        .map_err(|_| anyhow::anyhow!("Relay closed")),
                                    Ok((admission, deadline)) => {
                                        let session = self.clone(); let response = outbound.clone(); let request_id = frame.request_id;
                                        tokio::spawn(async move {
                                            let result = async {
                                                let controller = session.ready().await?;
                                                session.check_deadline(Some(deadline), entered, "pairing_ready")?;
                                                controller.start_setup_admitted(admission, deadline).await.map_err(native_error)
                                            }.await;
                                            if result.is_err() {
                                                eprintln!("{} event=control_rejected session={} namespace={} category=setup_unavailable stage=pairing_setup", chrono::Utc::now().to_rfc3339(), session.id, session.namespace);
                                                if response.try_send(Frame::json(7, request_id, 0, &json!({"error":"Native setup unavailable; request was not retried"}))).is_err() {
                                                    eprintln!("{} event=control_response_dropped session={} category=relay_unavailable", chrono::Utc::now().to_rfc3339(), session.id);
                                                }
                                            }
                                        });
                                        Ok(())
                                    }
                                }
                            }
                        }
                        11 => outbound
                            .send(Frame::json(
                                7,
                                frame.request_id,
                                0,
                                &json!({"error":"Asset installation is unsupported; use native setup for WDA"}),
                            ))
                            .await
                            .map_err(|_| anyhow::anyhow!("Relay closed")),
                        _ => Err(anyhow::anyhow!("Invalid relay message")),
                    },
                    Err(e) => Err(e),
                },
                Some(Ok(Message::Pong(_))) => {
                    alive.store(true, Ordering::SeqCst);
                    Ok(())
                }
                Some(Ok(Message::Ping(_))) => Ok(()),
                Some(Ok(Message::Close(_))) => break "browser_close",
                Some(Err(_)) => break "socket_error",
                None => break "eof",
                _ => break "unsupported_frame",
            };
            if let Err(error) = result {
                // Only fixed categories leave this boundary: SDK/device error strings
                // may contain caller-supplied content or access-bearing URLs.
                let category = match error.to_string().as_str() {
                    "Unknown or unopened stream" | "Unknown relay stream" => "unknown_stream",
                    "Invalid relay header" | "Invalid relay length" => "invalid_frame",
                    "Invalid OpenResult" | "Unexpected OpenResult" => "invalid_open_result",
                    "Relay closed" => "relay_closed",
                    _ => "invalid_protocol_or_hello",
                };
                eprintln!(
                    "{} event=ws_protocol_error session={} category={category}",
                    chrono::Utc::now().to_rfc3339(),
                    self.id
                );
                break "protocol_error";
            }
        };
        self.close_reason.record(reason);
        let reason = self.close_reason.get();
        eprintln!(
            "{} event=ws_closed session={} namespace={} reason={reason} duration_ms={} frames_received={frames_received} bytes_received={bytes_received}",
            chrono::Utc::now().to_rfc3339(),
            self.id,
            self.namespace,
            started.elapsed().as_millis()
        );
        self.close_with_reason(reason).await;
        let _ = writer.await;
    }
}

/// Screens are JPEG; sniff the rare PNG/WebP frame instead of mislabelling it.
fn image_type(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "image/jpeg"
    }
}
async fn body(request: Request<Body>) -> Result<Value> {
    body_with_limit(request, MAX_PAYLOAD).await
}
async fn body_with_limit(request: Request<Body>, limit: usize) -> Result<Value> {
    let bytes = tokio::time::timeout(Duration::from_secs(15), to_bytes(request.into_body(), limit))
        .await
        .map_err(|_| ApiError::new(408, "Request body timeout"))?
        .map_err(|_| ApiError::new(413, "Request body too large"))?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| ApiError::new(400, "Invalid JSON body"))?;
    if !value.is_object() {
        return Err(ApiError::new(400, "Invalid JSON body"));
    }
    Ok(value)
}
/// WebSocket subprotocol a relay client offers next to `ictl.token.<token>`;
/// the gateway selects it so browsers accept the upgrade.
const RELAY_PROTOCOL: &str = "ictl.relay";
/// The relay session token, preferring places that stay out of URLs and access
/// logs: an `Authorization: Bearer` header (non-browser clients), then an
/// `ictl.token.<token>` subprotocol (browsers cannot set WebSocket headers),
/// then the legacy `token` query parameter that `relayUrl` still carries.
/// The flag says whether the subprotocol form was used.
fn relay_token(headers: &axum::http::HeaderMap, url: &reqwest::Url) -> (String, bool) {
    if let Some(token) = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|v| !v.is_empty())
    {
        return (token.to_owned(), false);
    }
    let offered = headers.get_all("sec-websocket-protocol").iter().filter_map(|v| v.to_str().ok());
    if let Some(token) =
        offered.flat_map(|v| v.split(',')).find_map(|p| p.trim().strip_prefix("ictl.token.")).filter(|t| !t.is_empty())
    {
        return (token.to_owned(), true);
    }
    (url.query_pairs().find(|(k, _)| k == "token").map(|(_, v)| v.into_owned()).unwrap_or_default(), false)
}
async fn handle(State(gateway): State<Arc<Gateway>>, request: Request<Body>) -> Response {
    let origin = request.headers().get("origin").and_then(|h| h.to_str().ok()).map(str::to_owned);
    if origin.as_ref().is_some_and(|o| !gateway.config.origins.contains(o)) {
        return ApiError::new(403, "Origin denied").into_response();
    }
    let mut response = match dispatch(gateway, request).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    };
    if let Some(origin) = origin {
        response.headers_mut().insert("access-control-allow-origin", origin.parse().unwrap());
        response.headers_mut().insert("vary", "Origin".parse().unwrap());
    }
    response
}
async fn dispatch(g: Arc<Gateway>, request: Request<Body>) -> Result<Response> {
    let entered = Instant::now();
    let control_path = request.method() == Method::POST
        && (request.uri().path().ends_with("/command")
            || request.uri().path().ends_with("/setup")
            || request.uri().path().ends_with("/setup/input"))
        || request.method() == Method::GET
            && (request.uri().path().ends_with("/screen")
                || request.uri().path().ends_with("/screen.jpg")
                || request.uri().path().ends_with("/preview"));
    let control_deadline = control_path.then(ictl_protocol::ActionDeadline::admit);
    let path = request.uri().path().to_owned();
    let method = request.method().clone();
    if path == "/ios/device/ws" || path == "/device/ws" {
        let origin = request.headers().get("origin").and_then(|v| v.to_str().ok()).unwrap_or("");
        if !g.config.origins.contains(origin) {
            return Err(ApiError::new(403, "Origin denied"));
        }
        let query = reqwest::Url::parse(&format!("http://gateway{}", request.uri()))
            .map_err(|_| ApiError::new(400, "Invalid relay URL"))?;
        let (token, subprotocol) = relay_token(request.headers(), &query);
        let registry = g.registry.lock().await;
        let session = registry
            .sessions
            .values()
            .find(|s| !s.cancel.is_cancelled() && equal(&token, &s.token))
            .cloned()
            .ok_or_else(|| ApiError::new(401, "Authentication required"))?;
        drop(registry);
        let (mut parts, _) = request.into_parts();
        let ws = WebSocketUpgrade::from_request_parts(&mut parts, &())
            .await
            .map_err(|_| ApiError::new(400, "Invalid WebSocket upgrade"))?;
        if session.attached.swap(true, Ordering::SeqCst) {
            return Err(ApiError::new(409, "Relay already connected"));
        }
        let ws = if subprotocol { ws.protocols([RELAY_PROTOCOL]) } else { ws };
        return Ok(ws
            .max_message_size(MAX_PAYLOAD + 16)
            .max_frame_size(MAX_PAYLOAD + 16)
            .on_upgrade(move |socket| session.websocket(socket))
            .into_response());
    }
    if method == Method::OPTIONS {
        let mut response = StatusCode::NO_CONTENT.into_response();
        response.headers_mut().insert("access-control-allow-headers", "Authorization, Content-Type".parse().unwrap());
        response.headers_mut().insert("access-control-allow-methods", "GET,POST,DELETE,OPTIONS".parse().unwrap());
        return Ok(response);
    }
    if method == Method::GET && path == "/api/config" {
        return Ok(reply(200, g.model.configuration().await));
    }
    if let Some(response) = crate::static_files::serve(&g.config.web_dist, &method, &path).await {
        return Ok(response);
    }
    let token = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_owned();
    if method == Method::POST && path == "/api/sessions" {
        if !equal(&token, &g.config.token) {
            return Err(ApiError::new(401, "Authentication required"));
        }
        let host = request.headers().get("host").and_then(|v| v.to_str().ok()).unwrap_or("127.0.0.1").to_owned();
        let input = body(request).await?;
        let device = validate_device(&input["device"]).map_err(|_| ApiError::new(400, "Invalid device identity"))?;
        let pair = match input.get("pairRecord").filter(|v| !v.is_null()) {
            Some(v) => Some(serde_json::from_value(v.clone()).map_err(|_| ApiError::new(400, "Invalid pair record"))?),
            None => None,
        };
        let base = g
            .config
            .public_url
            .clone()
            .map(Ok)
            .unwrap_or_else(|| reqwest::Url::parse(&format!("http://{host}")))
            .map_err(|_| ApiError::new(400, "Invalid host"))?;
        let mut relay = base
            .join(if device.platform == Platform::Android { "/device/ws" } else { "/ios/device/ws" })
            .map_err(|_| ApiError::new(400, "Invalid host"))?;
        relay
            .set_scheme(if base.scheme() == "https" { "wss" } else { "ws" })
            .map_err(|_| ApiError::new(400, "Invalid host"))?;
        let s = g.create_session(device, pair).await?;
        relay.query_pairs_mut().append_pair("token", &s.token);
        return Ok(reply(
            201,
            json!({"id":s.id,"deviceId":s.device.serial_number,"token":s.token,"relayUrl":relay.as_str(),"state":"waiting_for_relay"}),
        ));
    }
    let parts: Vec<_> = path.trim_start_matches('/').split('/').collect();
    if parts.len() < 3 || parts[0] != "api" || parts[1] != "sessions" || Uuid::parse_str(parts[2]).is_err() {
        return Err(ApiError::new(404, "Route not found"));
    }
    let s = g
        .registry
        .lock()
        .await
        .sessions
        .get(parts[2])
        .cloned()
        .ok_or_else(|| ApiError::new(404, "Session not found"))?;
    let route = &parts[3..];
    if let (&Method::GET, ["media", key]) = (&method, route) {
        if s.cancel.is_cancelled() {
            return Err(ApiError::new(410, "Session closed"));
        }
        return s.attachments.serve(key).await;
    }
    let preview_access =
        method == Method::GET && matches!(route, ["preview", "mjpeg", ticket] if equal(ticket, &s.preview_ticket));
    if s.cancel.is_cancelled() || (!preview_access && !equal(&token, &s.token)) {
        return Err(ApiError::new(401, "Authentication required"));
    }
    match (method, route) {
        (Method::POST, ["diagnostics"]) => {
            // Allow all 64 streams with the expanded, strictly numeric window metrics.
            let input = body_with_limit(request, 64 * 1024).await?;
            crate::transport_diagnostics::record(&s.id, input)?;
            Ok(reply(200, json!({"recorded":true})))
        }
        (Method::POST, ["attachments"]) => {
            if s.tasks.active().await {
                return Err(ApiError::new(409, "任务运行中，请结束后再修改附件"));
            }
            let input = body_with_limit(request, crate::attachments::MAX_UPLOAD_BODY).await?;
            Ok(reply(201, s.attachments.add(input).await?))
        }
        (Method::DELETE, ["attachments", attachment]) => {
            if s.tasks.active().await {
                return Err(ApiError::new(409, "任务运行中，请结束后再修改附件"));
            }
            s.attachments.delete(attachment).await?;
            Ok(reply(200, json!({"deleted":true})))
        }
        (Method::GET, ["preview", "stream"]) => {
            if s.device.platform != Platform::Ios {
                return Err(ApiError::new(400, "MJPEG preview requires iPhone"));
            }
            s.ready().await?;
            Ok(reply(200, json!({"url":format!("/api/sessions/{}/preview/mjpeg/{}", s.id, s.preview_ticket)})))
        }
        (Method::DELETE, ["preview", "stream"]) => {
            s.mjpeg.stop().await;
            eprintln!(
                "{} event=preview_stopped session={} reason=client_request",
                chrono::Utc::now().to_rfc3339(),
                s.id
            );
            Ok(reply(200, json!({"stopped":true})))
        }
        (Method::GET, ["preview", "mjpeg", _]) => {
            if !preview_access {
                return Err(ApiError::new(401, "Authentication required"));
            }
            s.ready().await?;
            let address = s
                .mux
                .lock()
                .await
                .as_ref()
                .map(VirtualMux::address)
                .ok_or_else(|| ApiError::new(409, "Device relay is not ready"))?;
            let mut receiver = s.mjpeg.subscribe(address, s.cancel.clone()).await;
            let first = tokio::select! {
                biased;
                _ = s.cancel.cancelled() => return Err(ApiError::new(410, "Session closed")),
                result = tokio::time::timeout(Duration::from_secs(15), receiver.wait_for(|frame| frame.is_some())) => result,
            };
            if !matches!(first, Ok(Ok(_))) {
                return Err(ApiError::unavailable("MJPEG unavailable; use screenshot preview"));
            }
            drop(first);
            receiver.mark_changed();
            Ok(crate::mjpeg::response(receiver, s.cancel.clone()))
        }
        (Method::GET, []) => Ok(reply(200, s.snapshot().await)),
        (Method::DELETE, []) => {
            eprintln!(
                "{} event=session_close_requested session={} reason=api_delete",
                chrono::Utc::now().to_rfc3339(),
                s.id
            );
            s.close_with_reason("api_deleted").await;
            Ok(reply(200, json!({"state":"closed"})))
        }
        (Method::POST, ["command"]) => {
            let deadline = control_deadline
                .expect("control route captured admission")
                .map_err(|_| ApiError::unavailable("Action clock unavailable"))?;
            let input = body(request).await?;
            Ok(reply(200, s.run_command_at(input, s.cancel.clone(), entered, deadline).await?))
        }
        (Method::GET, ["screen"]) => {
            let deadline = control_deadline
                .expect("control route captured admission")
                .map_err(|_| ApiError::unavailable("Action clock unavailable"))?;
            Ok(reply(200, s.screen_admitted(s.cancel.clone(), None, entered, deadline, true).await?.value))
        }
        (Method::GET, ["screen.jpg"]) => {
            let deadline = control_deadline
                .expect("control route captured admission")
                .map_err(|_| ApiError::unavailable("Action clock unavailable"))?;
            let bytes = s.screen_bytes(entered, deadline).await?;
            let mut response = Response::new(Body::from(bytes.to_vec()));
            response.headers_mut().insert("content-type", image_type(&bytes).parse().unwrap());
            response.headers_mut().insert("cache-control", "no-store".parse().unwrap());
            Ok(response)
        }
        (Method::GET, ["preview"]) => {
            let deadline = control_deadline
                .expect("control route captured admission")
                .map_err(|_| ApiError::unavailable("Action clock unavailable"))?;
            Ok(reply(200, s.preview_at(entered, deadline).await?))
        }
        (Method::POST, ["setup"]) => {
            let deadline = control_deadline
                .expect("control route captured admission")
                .map_err(|_| ApiError::unavailable("Action clock unavailable"))?;
            let admission = s.manual_admission()?;
            body(request).await?;
            let c = s.ready().await?;
            s.check_deadline(Some(deadline), entered, "setup_ready")?;
            c.start_setup_admitted(admission, deadline).await.map_err(native_error)?;
            Ok(reply(200, c.snapshot().await["setup"].clone()))
        }
        (Method::POST, ["setup", "input"]) => {
            let deadline = control_deadline
                .expect("control route captured admission")
                .map_err(|_| ApiError::unavailable("Action clock unavailable"))?;
            let _admission = s.manual_admission()?;
            let input = body(request).await?;
            let line = input["line"]
                .as_str()
                .filter(|s| s.chars().count() <= 4096 && !s.contains(['\r', '\n', '\0']))
                .ok_or_else(|| ApiError::new(400, "Invalid setup input"))?;
            let c = s.ready().await?;
            if c.snapshot().await["setup"]["state"] != "running" {
                return Err(ApiError::new(409, "Setup is not running"));
            }
            s.check_deadline(Some(deadline), entered, "setup_input_ready")?;
            c.setup_input_with_deadline(line, deadline).await.map_err(native_error)?;
            Ok(reply(200, json!({"accepted":true})))
        }
        (Method::POST, ["tasks"]) => {
            let base = g.config.public_url.as_ref().map(|u| u.origin().ascii_serialization()).unwrap_or_else(|| {
                format!("http://{}", request.headers().get("host").and_then(|v| v.to_str().ok()).unwrap_or("127.0.0.1"))
            });
            let input = body(request).await?;
            if input
                .as_object()
                .unwrap()
                .keys()
                .any(|k| !["instruction", "model", "attachments", "autoConfirm"].contains(&k.as_str()))
                || input.get("autoConfirm").is_some_and(|v| !v.is_boolean())
            {
                return Err(ApiError::new(400, "Invalid task request"));
            }
            let model = g.model.select(input.get("model")).await?;
            let images = s.attachments.references(input.get("attachments"), &base, &s.id).await?;
            s.ready().await?;
            Ok(reply(
                201,
                s.tasks
                    .start(
                        s.clone(),
                        input["instruction"].as_str().unwrap_or(""),
                        model,
                        images,
                        input["autoConfirm"] == true,
                    )
                    .await?,
            ))
        }
        (Method::POST, ["tasks", task, "stop"]) => {
            if !body(request).await?.as_object().unwrap().is_empty() {
                return Err(ApiError::new(400, "Invalid stop request"));
            }
            Ok(reply(200, s.tasks.stop(task).await?))
        }
        (Method::POST, ["tasks", task, "instruction"]) => {
            let input = body(request).await?;
            if input.as_object().unwrap().keys().any(|k| !["instruction"].contains(&k.as_str()))
                || !input["instruction"].is_string()
            {
                return Err(ApiError::new(400, "Invalid instruction update"));
            }
            Ok(reply(200, s.tasks.steer(task, input["instruction"].as_str().unwrap()).await?))
        }
        (Method::POST, ["tasks", task, "confirm"]) => {
            let input = body(request).await?;
            if input.as_object().unwrap().keys().any(|k| !["actionId", "approved"].contains(&k.as_str()))
                || !input["actionId"].is_string()
                || !input["approved"].is_boolean()
            {
                return Err(ApiError::new(400, "Invalid confirmation request"));
            }
            Ok(reply(
                200,
                s.tasks
                    .confirm(task, input["actionId"].as_str().unwrap(), input["approved"].as_bool().unwrap())
                    .await?,
            ))
        }
        _ => Err(ApiError::new(404, "Route not found")),
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    use crate::android::AndroidController;
    use std::sync::atomic::AtomicUsize;

    async fn fixture() -> (Arc<Gateway>, Arc<Session>, Arc<AtomicUsize>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let gateway = Gateway::new(
            Config {
                token: "fixture".into(),
                origins: HashSet::from(["http://localhost".into()]),
                public_url: None,
                runtime: dir.path().into(),
                native: "/bin/false".into(),
                web_dist: dir.path().into(),
            },
            Model::new(&HashMap::new()).unwrap(),
        )
        .await
        .unwrap();
        let device = Device {
            platform: Platform::Android,
            serial_number: "android:fixture".into(),
            vendor_id: 1,
            product_id: 1,
            product_name: None,
        };
        let session = gateway.create_session(device, None).await.unwrap();
        let (outbound, mut frames) = mpsc::channel(10);
        let controller = AndroidController::new(session.device.serial_number.clone(), outbound, session.cancel.clone());
        assert!(session.controller.set(Controller::Android(controller.clone())).is_ok());
        let sent = Arc::new(AtomicUsize::new(0));
        let counter = sent.clone();
        tokio::spawn(async move {
            while let Some(frame) = frames.recv().await {
                if frame.kind == 14 {
                    counter.fetch_add(1, Ordering::SeqCst);
                    controller
                        .handle(crate::relay::Frame::json(
                            15,
                            frame.request_id,
                            0,
                            &json!({"deviceId":"android:fixture","result":{"kind":"text","text":"dispatched"}}),
                        ))
                        .await
                        .unwrap();
                }
            }
        });
        (gateway, session, sent, dir)
    }
    #[tokio::test]
    async fn deadline_is_rechecked_after_readiness_wait_without_reset() {
        let (gateway, _manual_session, _, _dir) = fixture().await;
        let device = Device {
            platform: Platform::Ios,
            serial_number: "b".repeat(40),
            vendor_id: 1452,
            product_id: 4776,
            product_name: None,
        };
        let session = gateway.create_session(device.clone(), None).await.unwrap();
        let native = NativeController::new(
            "/bin/false".into(),
            device,
            "/tmp/unused-boundary".into(),
            session.socket_dir.join("unused.sock"),
            "127.0.0.1:1".into(),
            session.cancel.clone(),
        );
        assert!(session.controller.set(Controller::Ios(native)).is_ok());
        let deadline =
            ictl_protocol::ActionDeadline::parse(json!(ictl_protocol::monotonic_ns().unwrap() + 40_000_000)).unwrap();
        tokio::time::pause();
        let running = session.clone();
        let operation = tokio::spawn(async move {
            running
                .execute(
                    JobRequest::Command(json!({"op":"tap","x":1,"y":2})),
                    running.cancel.clone(),
                    Some(deadline),
                    Instant::now(),
                )
                .await
        });
        tokio::task::yield_now().await;
        // Only a short monotonic deadline elapses; the 30s readiness timer is virtual.
        std::thread::sleep(Duration::from_millis(60));
        tokio::time::advance(Duration::from_secs(30)).await;
        let result = operation.await.unwrap();
        assert!(matches!(&result, Err(e) if e.status == 409 && e.message.contains("expired")), "{result:?}");
        gateway.cancel.cancel();
    }

    fn tap(id: &str) -> Value {
        json!({"deviceId":"android:fixture","requestId":id,"tool":"iphone_tap","args":{"x":1,"y":2}})
    }

    #[tokio::test]
    async fn expired_gateway_admission_never_dispatches_or_renews_duplicate() {
        let (_gateway, session, sent, _dir) = fixture().await;
        let deadline =
            ictl_protocol::ActionDeadline::admit_at(ictl_protocol::monotonic_ns().unwrap() - 31_000_000_000).unwrap();
        let entered = Instant::now() - Duration::from_secs(31);
        let first = session.run_command_at(tap("expired"), session.cancel.clone(), entered, deadline).await;
        assert!(matches!(&first, Err(error) if error.status == 409 && error.message.contains("expired")), "{first:?}");
        let retry = session.run_command(tap("expired"), session.cancel.clone()).await;
        assert!(matches!(&retry, Err(error) if error.message.contains("expired")), "{retry:?}");
        assert_eq!(sent.load(Ordering::SeqCst), 0);
        let fresh = session.run_command(tap("fresh"), session.cancel.clone()).await.unwrap();
        assert_eq!(session.run_command(tap("fresh"), session.cancel.clone()).await.unwrap(), fresh);
        assert_eq!(sent.load(Ordering::SeqCst), 1);
        session.cancel.cancel();
    }
}
