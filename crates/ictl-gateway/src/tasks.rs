use crate::{
    commands::command_request,
    error::{ApiError, Result, recoverable_observation_error, recoverable_wda_error},
    gateway::Session,
    identity::Platform,
    model::{FollowUp, Model, Observation, PlanRequest, validate_reply},
};
use regex::Regex;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, oneshot};
use tokio_util::sync::CancellationToken;

const RECOVERY_WAIT: Duration = Duration::from_secs(120);
const RECOVERY_INTERVAL: Duration = Duration::from_secs(2);
const MAX_RECOVERIES: u32 = 2;
/// A missed element tap (nothing was tapped, no side effect) is reported back
/// to the model instead of halting, at most this many times in a row. Labels
/// often differ from visible or OCR text in every observation mode.
const MAX_ELEMENT_MISSES: u32 = 2;
/// A done reply that asks the user for confirmation would end the task with no
/// way to answer; it is returned to the model once to re-plan as a confirmable action.
const MAX_DONE_QUESTIONS: u32 = 1;
/// Actions the gateway rejects before dispatch (unknown tool, invalid
/// arguments, Back on iOS) never reach the device. Report them to the model
/// and re-plan, at most this many times in a row.
const MAX_REJECTED_ACTIONS: u32 = 2;
/// A timed-out iphone_wait_for is read-only, so it is reported back to the
/// model instead of halting, at most this many times in a row.
const MAX_WAIT_MISSES: u32 = 3;
/// Consecutive successful actions that left the screen unchanged before the
/// model is warned, and before the task stops instead of repeating itself.
/// Games reject invalid moves without any change, so the stop limit leaves room to try others.
const WARN_NO_EFFECT: u32 = 3;
const MAX_NO_EFFECT: u32 = 8;
/// Screens are compared as 32x32 grayscale thumbnails: a pixel differing by more than
/// `PIXEL_DELTA` counts as changed, and more than `CHANGED_PIXELS` (about 1%) is a new screen.
/// A 16x16 average hash missed real moves in a match-3 game, whose busy board looks alike when coarse.
const THUMBNAIL_SIDE: u32 = 32;
const PIXEL_DELTA: u8 = 24;
const CHANGED_PIXELS: usize = 10;
/// Latest history entries sent with full detail; older steps are summarized in a step log.
const DETAILED_HISTORY: usize = 8;
/// Most earlier steps repeated to the model in the step log.
const STEP_LOG_ENTRIES: usize = 20;
/// Longest iphone_wait_for a task may request; long waits for text that never
/// appears were the slowest steps in on-device testing.
const MAX_TASK_WAIT_SECONDS: f64 = 10.0;
/// Goal updates accepted while one task runs.
const MAX_INSTRUCTION_UPDATES: usize = 20;
/// Earlier goals repeated to the model after the user changes the goal.
const EARLIER_GOALS: usize = 3;
/// Coordinates come from the observation the model saw; only a plan's first action may use them.
const COORDINATE_TOOLS: [&str; 3] = ["iphone_tap", "iphone_swipe", "iphone_long_press"];
/// Read-only results become the next observation, so they end a plan.
const QUERY_TOOLS: [&str; 5] =
    ["iphone_screenshot", "iphone_list_elements", "iphone_status", "iphone_list_apps", "iphone_list_devices"];

struct TaskStage<'a> {
    session_id: &'a str,
    task_id: &'a str,
    observation_id: &'a str,
    stage: &'static str,
    started: Instant,
    outcome: &'static str,
}
impl<'a> TaskStage<'a> {
    fn start(session_id: &'a str, task_id: &'a str, observation_id: &'a str, stage: &'static str) -> Self {
        let stage =
            Self { session_id, task_id, observation_id, stage, started: Instant::now(), outcome: "interrupted" };
        eprintln!("{}", stage.event("start", "started"));
        stage
    }
    fn event(&self, phase: &str, status: &str) -> Value {
        json!({
            "at":now(),"event":"task_stage","session_id":self.session_id,"task_id":self.task_id,
            "observation_id":self.observation_id,"stage":self.stage,"phase":phase,"status":status,
            "elapsed_ms":self.started.elapsed().as_millis() as u64
        })
    }
    fn finish(mut self, outcome: &'static str) {
        self.outcome = outcome;
    }
}
impl Drop for TaskStage<'_> {
    fn drop(&mut self) {
        eprintln!("{}", self.event("end", self.outcome));
    }
}

/// Steps kept in the polled task snapshot. Long tasks (games) run for hundreds of
/// steps; older entries drop off the front while `totalSteps` keeps the count.
const SNAPSHOT_STEPS: usize = 200;

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskPhase {
    Observing,
    Thinking,
    AwaitingConfirmation,
    Executing,
    Recovering,
    Completed,
    Failed,
    Stopped,
}
impl TaskPhase {
    fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Stopped)
    }
}
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StepState {
    Pending,
    AwaitingConfirmation,
    Executing,
    Succeeded,
    Failed,
    Missed,
    Rejected,
    Uncertain,
    Cancelled,
}
impl StepState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::AwaitingConfirmation => "awaiting_confirmation",
            Self::Executing => "executing",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Missed => "missed",
            Self::Rejected => "rejected",
            Self::Uncertain => "uncertain",
            Self::Cancelled => "cancelled",
        }
    }
}
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct StepView {
    id: String,
    /// 1-based position in the whole task, stable after older steps leave the snapshot.
    number: usize,
    tool: Value,
    args: Value,
    reason: Value,
    state: StepState,
    started_at: String,
    finished_at: Option<String>,
    result: Option<Value>,
    planned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    timings: Option<Value>,
}
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct ConfirmationView {
    action_id: String,
    tool: Value,
    args: Value,
    reason: Value,
}
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct InstructionUpdate {
    at: String,
    instruction: String,
    previous: String,
    after_step: usize,
}
#[derive(Serialize, Clone, Debug)]
struct PlanView {
    tool: Value,
    reason: Value,
}
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct Recovery {
    attempt: u32,
    max_attempts: u32,
    max_wait_seconds: u64,
    started_at: String,
    finished_at: Option<String>,
    elapsed_ms: u64,
}
/// The task as the browser polls it. Serialized only when a snapshot is read;
/// the JSON shape is the gateway's public task wire format.
#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct TaskState {
    id: String,
    instruction: String,
    model: Option<String>,
    state: TaskPhase,
    /// The latest `SNAPSHOT_STEPS` steps, oldest first.
    steps: VecDeque<StepView>,
    total_steps: usize,
    confirmation: Option<ConfirmationView>,
    started_at: String,
    finished_at: Option<String>,
    message: Option<String>,
    error: Option<String>,
    timings: Value,
    observation_id: Option<String>,
    attachments: Vec<String>,
    instruction_updates: Vec<InstructionUpdate>,
    plan: Vec<PlanView>,
    discarded_plan_actions: u64,
    auto_confirm: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery: Option<Recovery>,
}
impl TaskState {
    fn new(id: String, instruction: &str, model: Option<String>, attachments: Vec<String>, auto_confirm: bool) -> Self {
        Self {
            id,
            instruction: instruction.to_owned(),
            model,
            state: TaskPhase::Observing,
            steps: VecDeque::new(),
            total_steps: 0,
            confirmation: None,
            started_at: now(),
            finished_at: None,
            message: None,
            error: None,
            timings: json!({}),
            observation_id: None,
            attachments,
            instruction_updates: Vec::new(),
            plan: Vec::new(),
            discarded_plan_actions: 0,
            auto_confirm,
            recovery: None,
        }
    }
    fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("task state serializes")
    }
    fn push_step(&mut self, mut step: StepView) {
        self.total_steps += 1;
        step.number = self.total_steps;
        self.steps.push_back(step);
        while self.steps.len() > SNAPSHOT_STEPS {
            self.steps.pop_front();
        }
    }
    fn current_step(&mut self) -> Option<&mut StepView> {
        self.steps.back_mut()
    }
}

struct Task {
    session_id: String,
    started: Instant,
    state: TaskState,
    cancel: CancellationToken,
    confirmation: Option<oneshot::Sender<()>>,
    lease: Option<crate::control::TaskLease>,
    /// Armed only while a goal update may safely abandon the current phase:
    /// model planning, a confirmation wait or a read-only iphone_wait_for.
    interrupt: Option<CancellationToken>,
}
pub struct TaskManager {
    task: Mutex<Option<Task>>,
    /// Summaries of this session's finished tasks, oldest first, so a new task
    /// knows what the user asked and what happened before it.
    previous: Mutex<Vec<Value>>,
}
/// Finished tasks of a session repeated to the model for context.
const PREVIOUS_TASKS: usize = 3;
fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
impl Default for TaskManager {
    fn default() -> Self {
        Self::new()
    }
}
impl TaskManager {
    pub fn new() -> Self {
        Self { task: Mutex::new(None), previous: Mutex::new(Vec::new()) }
    }
    pub async fn snapshot(&self) -> Value {
        self.task.lock().await.as_ref().map(|t| t.state.to_value()).unwrap_or(Value::Null)
    }
    pub async fn active(&self) -> bool {
        self.task.lock().await.as_ref().is_some_and(|t| !t.state.state.terminal())
    }
    /// Registers a new running task. Fails while another task of the session runs.
    async fn install(
        &self,
        session_id: &str,
        state: TaskState,
        cancel: CancellationToken,
        lease: Option<crate::control::TaskLease>,
    ) -> Result<Value> {
        let mut guard = self.task.lock().await;
        if guard.as_ref().is_some_and(|t| !t.state.state.terminal()) {
            return Err(ApiError::new(409, "Session already has an active task"));
        }
        let snapshot = state.to_value();
        *guard = Some(Task {
            session_id: session_id.to_owned(),
            started: Instant::now(),
            state,
            cancel,
            confirmation: None,
            lease,
            interrupt: None,
        });
        Ok(snapshot)
    }
    pub async fn start(
        &self,
        session: Arc<Session>,
        instruction: &str,
        model: Model,
        images: Vec<crate::attachments::ReferenceImage>,
        auto_confirm: bool,
    ) -> Result<Value> {
        let instruction = valid_instruction(instruction)?;
        if self.active().await {
            return Err(ApiError::new(409, "Session already has an active task"));
        }
        if session.cancel.is_cancelled() {
            return Err(ApiError::new(410, "Session closed"));
        }
        let lease = session.claim_task()?;
        let id = uuid::Uuid::new_v4().to_string();
        let cancel = session.cancel.child_token();
        let state = TaskState::new(
            id.clone(),
            instruction,
            model.name.clone(),
            images.iter().map(|i| i.name.clone()).collect(),
            auto_confirm,
        );
        let snapshot = match self.install(&session.id, state, cancel.clone(), Some(lease.clone())).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                lease.cancel();
                return Err(error);
            }
        };
        let previous = self.previous.lock().await.clone();
        tokio::spawn(async move {
            let device = SessionDevice { session: session.clone(), lease };
            let task = TaskLoop::new(&session.tasks, &session.id, &id, device, model, images, previous, cancel.clone());
            tokio::select! {biased;
                _=session.cancel.cancelled()=>session.tasks.finish(&id,TaskPhase::Stopped,Some("Session disconnected. Already sent actions cannot be undone.".into()),None).await,
                _=cancel.cancelled()=>{},
                _=tokio::time::sleep(Duration::from_secs(1800))=>session.tasks.finish(&id,TaskPhase::Failed,None,Some("任务已达到 30 分钟时限".into())).await,
                result=task.run()=>match result{
                    Ok(message)=>session.tasks.finish(&id,TaskPhase::Completed,Some(message),None).await,
                    Err(error)=>session.tasks.finish(&id,TaskPhase::Failed,None,Some(error.message)).await,
                }
            }
        });
        Ok(snapshot)
    }
    async fn finish(&self, id: &str, phase: TaskPhase, message: Option<String>, error: Option<String>) {
        let mut guard = self.task.lock().await;
        let Some(task) = guard.as_mut().filter(|t| t.state.id == id && !t.state.state.terminal()) else {
            return;
        };
        let mut uncertain = false;
        for step in task.state.steps.iter_mut() {
            if step.state == StepState::Executing {
                step.state = StepState::Uncertain;
                step.finished_at = Some(now());
                uncertain = true;
            }
            if matches!(step.state, StepState::Pending | StepState::AwaitingConfirmation) {
                step.state = StepState::Cancelled;
                step.finished_at = Some(now());
            }
        }
        eprintln!(
            "{}",
            json!({
                "at":now(),"event":"task_stage","session_id":task.session_id,"task_id":id,
                "observation_id":task.state.observation_id,"stage":"finish","phase":"end","status":phase,
                "elapsed_ms":task.started.elapsed().as_millis() as u64
            })
        );
        let state = &mut task.state;
        state.state = phase;
        state.message = message
            .or_else(|| uncertain.then(|| "A device action may already have been sent and cannot be undone.".into()));
        state.error = error;
        state.finished_at = Some(now());
        state.confirmation = None;
        state.plan.clear();
        let summary = task_summary(&state.to_value());
        {
            let mut previous = self.previous.lock().await;
            previous.push(summary);
            if previous.len() > PREVIOUS_TASKS {
                previous.remove(0);
            }
        }
        task.confirmation = None;
        task.interrupt = None;
        task.cancel.cancel();
        if let Some(lease) = task.lease.take() {
            lease.cancel();
        }
    }
    pub async fn disconnect(&self) {
        let id = self.task.lock().await.as_ref().map(|t| t.state.id.clone());
        if let Some(id) = id {
            self.finish(
                &id,
                TaskPhase::Stopped,
                Some("Session disconnected. Already sent actions cannot be undone.".into()),
                None,
            )
            .await;
        }
    }
    async fn is_current(&self, id: &str) -> bool {
        self.task.lock().await.as_ref().is_some_and(|t| t.state.id == id)
    }
    pub async fn stop(&self, id: &str) -> Result<Value> {
        if !self.is_current(id).await {
            return Err(ApiError::new(404, "Task not found"));
        }
        self.finish(id, TaskPhase::Stopped, Some("Task stopped. Already sent actions cannot be undone.".into()), None)
            .await;
        Ok(self.snapshot().await)
    }
    pub async fn confirm(&self, id: &str, action_id: &str, approved: bool) -> Result<Value> {
        let mut guard = self.task.lock().await;
        let task = guard
            .as_mut()
            .filter(|t| {
                t.state.id == id
                    && t.state.state == TaskPhase::AwaitingConfirmation
                    && t.state.confirmation.as_ref().is_some_and(|c| c.action_id == action_id)
                    && !t.cancel.is_cancelled()
            })
            .ok_or_else(|| ApiError::new(409, "Invalid or stale confirmation"))?;
        let sender = task.confirmation.take().ok_or_else(|| ApiError::new(409, "Invalid or stale confirmation"))?;
        task.state.confirmation = None;
        if approved {
            task.state.state = TaskPhase::Executing;
            let _ = sender.send(());
            Ok(task.state.to_value())
        } else {
            drop(guard);
            self.finish(id, TaskPhase::Stopped, Some("Action confirmation declined.".into()), None).await;
            Ok(self.snapshot().await)
        }
    }
    /// Replaces the goal of a running task. Already sent actions are kept; the
    /// next plan uses the new goal, and phases that sent nothing to the phone
    /// (planning, a pending confirmation, a read-only wait) are abandoned now.
    pub async fn steer(&self, id: &str, instruction: &str) -> Result<Value> {
        let instruction = valid_instruction(instruction)?;
        let mut guard = self.task.lock().await;
        let task = guard.as_mut().filter(|t| t.state.id == id).ok_or_else(|| ApiError::new(404, "Task not found"))?;
        if task.state.state.terminal() || task.cancel.is_cancelled() {
            return Err(ApiError::new(409, "Task is not running"));
        }
        if task.state.instruction_updates.len() >= MAX_INSTRUCTION_UPDATES {
            return Err(ApiError::new(429, "本次任务的目标更新次数已达上限，请停止后重新开始。"));
        }
        let previous = std::mem::replace(&mut task.state.instruction, instruction.to_owned());
        let after_step = task.state.total_steps;
        task.state.instruction_updates.push(InstructionUpdate {
            at: now(),
            instruction: instruction.to_owned(),
            previous,
            after_step,
        });
        if let Some(interrupt) = task.interrupt.take() {
            interrupt.cancel();
        }
        Ok(task.state.to_value())
    }
    /// Arms a goal-update interrupt for a phase that has sent nothing to the phone.
    /// A goal update that arrived after `seen_updates` was read, but before
    /// arming, interrupts at once instead of being missed.
    async fn arm_interrupt(
        &self,
        id: &str,
        cancel: &CancellationToken,
        seen_updates: usize,
    ) -> Result<CancellationToken> {
        let interrupt = cancel.child_token();
        let armed = interrupt.clone();
        self.update(id, move |t| {
            if t.state.instruction_updates.len() == seen_updates {
                t.interrupt = Some(armed);
            } else {
                armed.cancel();
            }
        })
        .await?;
        Ok(interrupt)
    }
    async fn disarm_interrupt(&self, id: &str) {
        let _ = self.update(id, |t| t.interrupt = None).await;
    }
    /// Runs `operation` on the running task `id`; fails once it ended or was cancelled.
    async fn update<R>(&self, id: &str, operation: impl FnOnce(&mut Task) -> R) -> Result<R> {
        let mut guard = self.task.lock().await;
        let task = guard
            .as_mut()
            .filter(|t| t.state.id == id && !t.state.state.terminal() && !t.cancel.is_cancelled())
            .ok_or_else(|| ApiError::new(409, "Task cancelled"))?;
        Ok(operation(task))
    }
    async fn state<R>(&self, id: &str, read: impl FnOnce(&TaskState) -> R) -> Result<R> {
        self.update(id, |t| read(&t.state)).await
    }
}

/// How strongly an action needs the user's approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Gate {
    None,
    /// Outgoing, destructive or system-level: asks unless the user pre-approved the task.
    Sensitive,
    /// Payments and purchases: always asks, even with autoConfirm.
    Payment,
}
static PAYMENT_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(pay|payment|purchase|buy|checkout|subscribe|top[ -]?up|place order)\b|支付|付款|购买|下单|转账|订阅|充值|买单|结账",
    )
    .unwrap()
});
static SENSITIVE_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(send|submit|pay|purchase|delete|erase|remove|transfer)\b|发送|提交|支付|购买|删除|清空|转账|移除|抹掉",
    )
    .unwrap()
});
/// Submitting a search needs no approval; sending, posting or replying does.
static OUTGOING_WORDS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(post|publish|reply|comment)\b|发布|评论|回复|留言").unwrap());
/// A price on the targeted control (¥12, $4.99) means the tap may buy something.
static PRICE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[¥￥$€£]\s?\d|\d\s?(元|円)|\b(USD|CNY|EUR)\s?\d").unwrap());
/// Account- or device-wide changes inside a settings app.
static SETTINGS_WORDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(reset|erase|sign out|log out|passcode|apple id|restore)\b|还原|抹掉|退出登录|密码|抹除|重置")
        .unwrap()
});
/// One element of `elementsText`: `Kind "label" … @(cx,cy) wxh`.
static ELEMENT_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?P<text>.*) @\((?P<x>-?\d+),(?P<y>-?\d+)\) (?P<w>\d+)x(?P<h>\d+)$").unwrap());
/// Foreground app reported by iphone_status or app launches.
static FOREGROUND: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)(?:Foreground app|前台 App|前台)[:：]?\s*(?P<app>[A-Za-z0-9._-]+)").unwrap());

/// Apps in which every tap or submit can move money.
const PAYMENT_APPS: [&str; 10] = [
    "com.apple.passbook",
    "com.alipay.iphoneclient",
    "com.eg.android.alipaygphone",
    "com.paypal.ppclient",
    "com.paypal.android.p2pmobile",
    "com.unionpay.chsp",
    "com.unionpay",
    "wallet",
    "钱包",
    "支付宝",
];
/// Apps where submitting typed text sends a message.
const MESSAGING_APPS: [&str; 12] = [
    "com.apple.mobilesms",
    "com.apple.mobilemail",
    "com.tencent.xin",
    "com.tencent.mm",
    "com.tencent.mqq",
    "com.tencent.mobileqq",
    "net.whatsapp.whatsapp",
    "com.whatsapp",
    "ph.telegra.telegraph",
    "org.telegram.messenger",
    "com.google.android.apps.messaging",
    "com.google.android.gm",
];
const SETTINGS_APPS: [&str; 3] = ["com.apple.preferences", "com.android.settings", "设置"];
fn app_in(app: Option<&str>, list: &[&str]) -> bool {
    app.is_some_and(|app| {
        let app = app.to_lowercase();
        list.iter().any(|known| app == *known || (known.contains('.') && app.starts_with(&format!("{known}."))))
    })
}

/// What the gateway itself knows about an action's target, independent of the
/// model's own description: the on-screen labels at the tapped point or matching
/// the tapped element text, and the foreground app.
#[derive(Default, Debug, Clone)]
struct GateContext {
    /// `elementsText` of the observation that planned the action.
    elements: String,
    /// OCR lines (text, phone-point bounding box) of that observation.
    ocr: Vec<(String, [f64; 4])>,
    foreground_app: Option<String>,
}
impl GateContext {
    fn observe(&mut self, observation: &Observation) {
        self.elements = observation.value["elementsText"].as_str().unwrap_or("").to_owned();
        self.ocr = observation
            .ocr
            .as_ref()
            .and_then(|ocr| ocr.as_ref().ok())
            .and_then(|ocr| ocr["lines"].as_array())
            .map(|lines| {
                lines
                    .iter()
                    .filter_map(|line| {
                        let text = line["text"].as_str()?.to_owned();
                        let points = line["phonePolygon"].as_array()?;
                        let xs = points.iter().filter_map(|p| p[0].as_f64());
                        let ys = points.iter().filter_map(|p| p[1].as_f64());
                        let (x0, x1) = xs.fold((f64::MAX, f64::MIN), |(a, b), x| (a.min(x), b.max(x)));
                        let (y0, y1) = ys.fold((f64::MAX, f64::MIN), |(a, b), y| (a.min(y), b.max(y)));
                        (x0 <= x1 && y0 <= y1).then_some((text, [x0, y0, x1, y1]))
                    })
                    .collect()
            })
            .unwrap_or_default();
        for key in ["foregroundApp", "bundleId"] {
            if let Some(app) = observation.value[key].as_str().filter(|app| !app.is_empty()) {
                self.foreground_app = Some(app.to_owned());
            }
        }
    }
    /// Labels of what an action touches, from the screen rather than the model.
    fn targets(&self, request: &Value) -> Vec<String> {
        let mut targets = Vec::new();
        match request["op"].as_str() {
            Some("tap" | "long_press") => {
                let (Some(x), Some(y)) = (request["x"].as_f64(), request["y"].as_f64()) else { return targets };
                // The smallest element containing the point is the control being tapped.
                let mut hit: Option<(f64, String)> = None;
                for line in self.elements.lines() {
                    let Some(c) = ELEMENT_LINE.captures(line.trim()) else { continue };
                    let n = |name: &str| c[name].parse::<f64>().unwrap_or(0.0);
                    let (cx, cy, w, h) = (n("x"), n("y"), n("w"), n("h"));
                    if (x - cx).abs() <= w / 2.0 + 2.0
                        && (y - cy).abs() <= h / 2.0 + 2.0
                        && hit.as_ref().is_none_or(|(area, _)| w * h < *area)
                    {
                        hit = Some((w * h, c["text"].to_owned()));
                    }
                }
                targets.extend(hit.map(|(_, text)| text));
                for (text, [x0, y0, x1, y1]) in &self.ocr {
                    if x >= x0 - 4.0 && x <= x1 + 4.0 && y >= y0 - 4.0 && y <= y1 + 4.0 {
                        targets.push(text.clone());
                    }
                }
            }
            Some("tap_element") => {
                let text = request["text"].as_str().unwrap_or("");
                targets.push(text.to_owned());
                let needle = text.to_lowercase();
                if !needle.is_empty() {
                    targets.extend(
                        self.elements
                            .lines()
                            .filter(|line| line.to_lowercase().contains(&needle))
                            .take(3)
                            .map(str::to_owned),
                    );
                }
            }
            Some("type_text") => targets.push(request["text"].as_str().unwrap_or("").to_owned()),
            _ => {}
        }
        targets
    }
    /// Tracks the foreground app from actions the gateway itself dispatched.
    fn after_action(&mut self, request: &Value, result: &Value) {
        match request["op"].as_str() {
            Some("launch_app") if result["kind"] != "error" => {
                self.foreground_app = request["app"].as_str().map(str::to_owned);
            }
            Some("press_button") if request["button"] == "home" => self.foreground_app = None,
            Some("open_url" | "terminate_app") => self.foreground_app = None,
            _ => {}
        }
        if let Some(app) = result["text"].as_str().and_then(|text| FOREGROUND.captures(text)) {
            self.foreground_app = Some(app["app"].to_owned());
        }
    }
}

/// The approval class of an action. The model's flag and description can only
/// add confirmation: the gateway also derives it from the action, the target's
/// on-screen label and the foreground app, so text on the screen that steers the
/// model's description cannot remove it.
fn gate(action: &Value, request: &Value, context: &GateContext) -> Gate {
    let op = request["op"].as_str().unwrap_or("");
    let described =
        format!("{} {}", action["reason"].as_str().unwrap_or(""), action["args"]["text"].as_str().unwrap_or(""));
    let targets = context.targets(request).join("\n");
    let app = context.foreground_app.as_deref();
    let touches = ["tap", "tap_element", "long_press"].contains(&op);
    let submits = op == "type_text" && request["submit"] == true;
    if PAYMENT_WORDS.is_match(&described)
        || ((touches || submits)
            && (PAYMENT_WORDS.is_match(&targets) || PRICE.is_match(&targets) || app_in(app, &PAYMENT_APPS)))
    {
        return Gate::Payment;
    }
    let sensitive_text = |text: &str| SENSITIVE_WORDS.is_match(text) || OUTGOING_WORDS.is_match(text);
    if action["requiresConfirmation"] == true
        || op == "terminate_app"
        || (op == "open_url"
            && reqwest::Url::parse(request["url"].as_str().unwrap_or(""))
                .is_ok_and(|u| !["http", "https"].contains(&u.scheme())))
        || (submits && (sensitive_text(&described) || sensitive_text(&targets) || app_in(app, &MESSAGING_APPS)))
        || (touches
            && (SENSITIVE_WORDS.is_match(&described)
                || sensitive_text(&targets)
                || (app_in(app, &SETTINGS_APPS) && SETTINGS_WORDS.is_match(&targets))))
    {
        return Gate::Sensitive;
    }
    Gate::None
}
/// Whether an action must wait for the user. With `auto_confirm` the user
/// pre-approved the task, and only payments and purchases still stop.
fn confirmation_required(action: &Value, request: &Value, context: &GateContext, auto_confirm: bool) -> bool {
    match gate(action, request, context) {
        Gate::Payment => true,
        Gate::Sensitive => !auto_confirm,
        Gate::None => false,
    }
}

/// The device side of a task: observation, dispatch and readiness. A seam so the
/// loop can run against fakes in unit tests.
pub(crate) trait TaskDevice {
    fn device_id(&self) -> &str;
    fn platform(&self) -> Platform;
    async fn observe(&self, elements: bool, cancel: CancellationToken) -> Result<Observation>;
    /// Whether the native daemon reports WDA ready (recovery polling).
    async fn ready(&self, cancel: CancellationToken) -> Result<bool>;
    async fn execute(&self, command: Value, cancel: CancellationToken) -> Result<Value>;
}
/// The planning side of a task.
pub(crate) trait TaskPlanner {
    fn wants_elements(&self) -> bool;
    async fn recognize(&self, observation: &Observation, cancel: CancellationToken) -> Option<Result<Value>>;
    async fn plan(&self, request: PlanRequest<'_>, cancel: CancellationToken) -> Result<Value>;
}
struct SessionDevice {
    session: Arc<Session>,
    lease: crate::control::TaskLease,
}
impl TaskDevice for SessionDevice {
    fn device_id(&self) -> &str {
        &self.session.device.serial_number
    }
    fn platform(&self) -> Platform {
        self.session.device.platform
    }
    async fn observe(&self, elements: bool, cancel: CancellationToken) -> Result<Observation> {
        let output = self.session.task_screen(cancel, &self.lease, elements).await?;
        Ok(Observation::new(output.value, output.jpeg))
    }
    async fn ready(&self, cancel: CancellationToken) -> Result<bool> {
        let controller = self
            .session
            .controller
            .get()
            .ok_or_else(|| ApiError::unavailable("Native control transport is not ready"))?;
        let daemon = controller.request(json!({"op":"daemon"}), Duration::from_secs(3), cancel).await;
        Ok(daemon
            .ok()
            .filter(|response| response["kind"] == "text")
            .and_then(|response| serde_json::from_str::<Value>(response["text"].as_str()?).ok())
            .is_some_and(|state| state["state"]["state"] == "ready"))
    }
    async fn execute(&self, command: Value, cancel: CancellationToken) -> Result<Value> {
        self.session.run_task_command(command, cancel, &self.lease).await
    }
}
impl TaskPlanner for Model {
    fn wants_elements(&self) -> bool {
        Model::wants_elements(self)
    }
    async fn recognize(&self, observation: &Observation, cancel: CancellationToken) -> Option<Result<Value>> {
        Model::recognize(self, observation, cancel).await
    }
    async fn plan(&self, request: PlanRequest<'_>, cancel: CancellationToken) -> Result<Value> {
        Model::plan(self, request, cancel).await
    }
}

/// Re-plan budgets. Each counts consecutive occurrences and resets after a
/// successful step, so a long task (a game) is not ended by scattered misses.
#[derive(Default, Debug)]
struct Counters {
    element_misses: u32,
    rejected_actions: u32,
    wait_misses: u32,
    /// Consecutive successful actions that left the screen unchanged.
    no_effect: u32,
    /// Per task: one focus recovery and one done-question re-plan.
    focus_recoveries: u32,
    done_questions: u32,
}
impl Counters {
    fn succeeded(&mut self) {
        self.element_misses = 0;
        self.rejected_actions = 0;
        self.wait_misses = 0;
    }
    /// Spends one gateway-rejection re-plan; false once the budget is used up.
    fn reject(&mut self) -> bool {
        if self.rejected_actions >= MAX_REJECTED_ACTIONS {
            return false;
        }
        self.rejected_actions += 1;
        true
    }
}

/// One history entry sent to the model, with the step-log number it records.
#[derive(Clone, Debug)]
struct HistoryEntry {
    step: Option<usize>,
    value: Value,
}
/// The detailed model history plus what a bridge thread has not seen yet.
#[derive(Default)]
struct History {
    entries: Vec<HistoryEntry>,
    /// Entries added since the last successful plan, for bridge follow-ups.
    unsent: Vec<Value>,
}
impl History {
    fn push(&mut self, value: Value, step: Option<usize>) {
        self.unsent.push(value.clone());
        if self.unsent.len() > DETAILED_HISTORY * 2 {
            self.unsent.remove(0);
        }
        self.entries.push(HistoryEntry { step, value });
    }
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    /// Bounds what is sent: fewer entries, shorter older results, total size cap.
    fn trim(&mut self) {
        if let Some((_, older)) = self.entries.split_last_mut() {
            for entry in older {
                compact_entry(&mut entry.value);
            }
        }
        while self.entries.len() > DETAILED_HISTORY
            || self.entries.iter().map(|e| e.value.to_string().len()).sum::<usize>() > HISTORY_CHARS
        {
            self.entries.remove(0);
        }
    }
    fn mark_last_succeeded(&mut self, changed: bool) {
        if let Some(entry) = self.entries.iter_mut().rev().find(|e| e.value["state"] == "succeeded") {
            entry.value["screenChanged"] = json!(changed);
        }
    }
}

enum Next {
    Action { action: Value, observation_id: String, timings: Value, planned: bool },
    Again,
    Done(String),
}

/// One task run: observe, plan, gate and execute until done or halted.
struct TaskLoop<'a, D, P> {
    tasks: &'a TaskManager,
    session_id: &'a str,
    id: &'a str,
    device: D,
    planner: P,
    images: Vec<crate::attachments::ReferenceImage>,
    previous_tasks: Vec<Value>,
    cancel: CancellationToken,
    history: History,
    /// Every dispatched action, so the model keeps track of the whole task.
    step_log: Vec<Value>,
    next_observation: Option<Observation>,
    counters: Counters,
    /// Set when the goal changes; cleared once an action is dispatched for the new goal.
    steered: bool,
    /// The screen that planned the latest actions, and whether a mutating action ran since.
    last_screen: Option<ScreenFingerprint>,
    awaiting_effect: bool,
    typing_blocked: bool,
    input_pending: bool,
    recoveries: u32,
    /// Remaining actions of a model plan, dispatched without re-observing.
    queue: VecDeque<Value>,
    plan_observation_id: String,
    seen_updates: usize,
    /// The goal a bridge thread last received, so follow-ups repeat it only after a change.
    sent_instruction: Option<String>,
    gate: GateContext,
}
impl<'a, D: TaskDevice, P: TaskPlanner> TaskLoop<'a, D, P> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        tasks: &'a TaskManager,
        session_id: &'a str,
        id: &'a str,
        device: D,
        planner: P,
        images: Vec<crate::attachments::ReferenceImage>,
        previous_tasks: Vec<Value>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            tasks,
            session_id,
            id,
            device,
            planner,
            images,
            previous_tasks,
            cancel,
            history: History::default(),
            step_log: Vec::new(),
            next_observation: None,
            counters: Counters::default(),
            steered: false,
            last_screen: None,
            awaiting_effect: false,
            typing_blocked: false,
            input_pending: false,
            recoveries: 0,
            queue: VecDeque::new(),
            plan_observation_id: String::new(),
            seen_updates: 0,
            sent_instruction: None,
            gate: GateContext::default(),
        }
    }

    async fn run(mut self) -> Result<String> {
        loop {
            let (action, observation_id, timings, planned) = match self.next_action().await? {
                Next::Action { action, observation_id, timings, planned } => (action, observation_id, timings, planned),
                Next::Again => continue,
                Next::Done(message) => return Ok(message),
            };
            self.dispatch(action, &observation_id, timings, planned).await?;
        }
    }

    async fn update<R>(&self, operation: impl FnOnce(&mut Task) -> R) -> Result<R> {
        self.tasks.update(self.id, operation).await
    }

    /// Drops the planned actions that were not dispatched; nothing reached the phone.
    async fn discard_plan(&mut self) -> Result<()> {
        if self.queue.is_empty() {
            return Ok(());
        }
        let discarded = self.queue.len() as u64;
        self.queue.clear();
        self.update(move |t| {
            t.state.discarded_plan_actions += discarded;
            t.state.plan.clear();
        })
        .await
    }
    async fn show_plan(&self) -> Result<()> {
        let remaining = plan_view(&self.queue);
        self.update(move |t| t.state.plan = remaining).await
    }

    async fn observe(&mut self) -> Result<Observation> {
        if let Some(observation) = self.next_observation.take() {
            return Ok(observation);
        }
        match self.device.observe(self.planner.wants_elements(), self.cancel.clone()).await {
            Ok(observation) => Ok(observation),
            Err(error) if self.device.platform() == Platform::Ios && recoverable_observation_error(&error.message) => {
                self.recover().await
            }
            Err(error) => Err(error),
        }
    }

    async fn recover(&mut self) -> Result<Observation> {
        self.recoveries += 1;
        let episode = self.recoveries;
        if episode > MAX_RECOVERIES {
            return Err(ApiError::unavailable("任务已达到 2 次恢复次数上限，请检查设备后重新开始。"));
        }
        self.update(|task| {
            task.state.state = TaskPhase::Recovering;
            task.state.message =
                Some("正在等待 WDA 恢复，最多 120 秒。恢复后会重新查看屏幕，核对上一步的结果。".into());
            task.state.recovery = Some(Recovery {
                attempt: episode,
                max_attempts: MAX_RECOVERIES,
                max_wait_seconds: RECOVERY_WAIT.as_secs(),
                started_at: now(),
                finished_at: None,
                elapsed_ms: 0,
            });
        })
        .await?;
        let started = Instant::now();
        let elements = self.planner.wants_elements();
        let recovering = async {
            loop {
                let attempt = self.cancel.child_token();
                // Cancel queued observations too if the recovery deadline drops this future.
                let _guard = attempt.clone().drop_guard();
                if self.device.ready(attempt.clone()).await? {
                    match self.device.observe(elements, attempt).await {
                        Ok(observation) => return Ok(observation),
                        Err(error) if recoverable_observation_error(&error.message) => {}
                        Err(error) => return Err(error),
                    }
                }
                self.update(|task| {
                    if let Some(recovery) = task.state.recovery.as_mut() {
                        recovery.elapsed_ms = started.elapsed().as_millis() as u64;
                    }
                })
                .await?;
                tokio::time::sleep(RECOVERY_INTERVAL).await;
            }
        };
        let result = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(ApiError::new(409,"Task cancelled")),
            result = tokio::time::timeout(RECOVERY_WAIT, recovering) => result.unwrap_or_else(|_| {
                Err(ApiError::unavailable("等待 WDA 恢复已超过 120 秒，任务已停止。请检查手机连接和 WDA 状态。"))
            }),
        };
        self.update(|task| {
            if let Some(recovery) = task.state.recovery.as_mut() {
                recovery.finished_at = Some(now());
                recovery.elapsed_ms = started.elapsed().as_millis() as u64;
            }
            task.state.message = None;
        })
        .await?;
        result
    }

    /// The next action: from the current plan, or from a fresh observation and model call.
    async fn next_action(&mut self) -> Result<Next> {
        let (updates, instruction, total_steps) =
            self.tasks.state(self.id, |t| (t.instruction_updates.len(), t.instruction.clone(), t.total_steps)).await?;
        if updates != self.seen_updates {
            // The user changed the goal: actions planned for the old goal are void.
            self.seen_updates = updates;
            self.steered = true;
            self.counters.no_effect = 0;
            self.discard_plan().await?;
        }
        if let Some(action) = self.queue.pop_front() {
            self.show_plan().await?;
            return Ok(Next::Action {
                action,
                observation_id: self.plan_observation_id.clone(),
                timings: json!({"planned":true}),
                planned: true,
            });
        }
        let observation_id = uuid::Uuid::new_v4().to_string();
        self.update(|task| {
            task.state.state = TaskPhase::Observing;
            task.state.observation_id = Some(observation_id.clone());
        })
        .await?;
        let observing = Instant::now();
        let observation_stage = TaskStage::start(self.session_id, self.id, &observation_id, "observe");
        let direct = if self.images.is_empty() && self.history.is_empty() && total_steps == 0 {
            direct_command_for(&instruction, self.device.platform())
        } else {
            None
        };
        let observation_result = if direct.is_some() {
            Ok(Observation::from_value(json!({"deviceId":self.device.device_id(),"caption":"Explicit user command"})))
        } else {
            self.observe().await
        };
        observation_stage.finish(if observation_result.is_ok() { "ok" } else { "error" });
        let mut observation = observation_result?;
        if observation.value["deviceId"] != self.device.device_id() {
            return Err(ApiError::new(409, "Observation device identity mismatch"));
        }
        observation.value["observationId"] = json!(observation_id);
        // The screen fingerprint and OCR both read the decoded frame: run them together.
        let (screen, ocr) = if direct.is_some() || observation.ocr.is_some() {
            (ScreenFingerprint::of(&observation).await, None)
        } else {
            tokio::join!(ScreenFingerprint::of(&observation), self.planner.recognize(&observation, self.cancel.clone()))
        };
        if ocr.is_some() {
            observation.ocr = ocr;
        }
        if self.cancel.is_cancelled() {
            return Err(ApiError::new(409, "Task cancelled"));
        }
        if let Some(screen) = screen {
            self.track_effect(&screen)?;
            self.last_screen = Some(screen);
        }
        self.gate.observe(&observation);
        let query_only = observation.value["queryOnly"] == true;
        let mut timings =
            json!({"observationMs":observing.elapsed().as_millis() as u64,"capture":observation.value["timings"]});
        let (instruction, goal) = self
            .update(|t| {
                t.state.state = TaskPhase::Thinking;
                t.state.timings = timings.clone();
                (model_instruction(&t.state.instruction, &t.state.instruction_updates), t.state.instruction.clone())
            })
            .await?;
        self.history.trim();
        let thinking = Instant::now();
        let model_stage = TaskStage::start(self.session_id, self.id, &observation_id, "model");
        let action_result = if let Some(direct) = direct {
            timings["direct"] = json!(true);
            Ok(direct)
        } else {
            let interrupt = self.tasks.arm_interrupt(self.id, &self.cancel, self.seen_updates).await?;
            let follow_up = FollowUp {
                instruction: (self.sent_instruction.as_deref() != Some(instruction.as_str()))
                    .then(|| instruction.clone()),
                steps: self.history.unsent.clone(),
            };
            let request = PlanRequest {
                instruction: &instruction,
                observation: &observation,
                history: model_history(&self.previous_tasks, &self.step_log, &self.history.entries),
                follow_up: Some(follow_up),
                images: &self.images,
                // A Codex bridge keeps one agent thread per connection; other backends ignore this.
                session: Some(self.session_id),
            };
            let result = self.planner.plan(request, interrupt).await.and_then(validate_reply);
            self.tasks.disarm_interrupt(self.id).await;
            if result.is_ok() {
                self.sent_instruction = Some(instruction.clone());
                self.history.unsent.clear();
            }
            result
        };
        model_stage.finish(if action_result.is_ok() { "ok" } else { "error" });
        if self.cancel.is_cancelled() {
            return Err(ApiError::new(409, "Task cancelled"));
        }
        // Kept to re-plan from the same screen when the goal changes during planning.
        let reusable = (timings["direct"] != true).then_some(observation);
        if self.tasks.state(self.id, |t| t.instruction_updates.len()).await? != self.seen_updates {
            // Nothing was dispatched for the old goal: plan again for the new one.
            self.next_observation = reusable;
            return Ok(Next::Again);
        }
        let action = action_result?;
        timings["modelMs"] = json!(thinking.elapsed().as_millis() as u64);
        let shown = timings.clone();
        self.update(|t| t.state.timings = shown).await?;
        if action["type"] == "done" {
            if self.input_pending {
                return Err(ApiError::unavailable("Text input is still pending; task halted"));
            }
            let message = action["message"].as_str().unwrap_or("");
            if self.counters.done_questions < MAX_DONE_QUESTIONS && asks_for_confirmation(message) {
                // Nothing was dispatched: re-plan from the same screen as a confirmable action.
                self.counters.done_questions += 1;
                self.history.push(
                    json!({"tool":"done","args":{"message":message},"state":"rejected",
                        "result":{"kind":"error","message":DONE_IS_NOT_CONFIRMATION}}),
                    None,
                );
                self.next_observation = reusable;
                return Ok(Next::Again);
            }
            if self.steered && !stops_task(&goal) {
                // Models read a replaced goal as a refusal and stop; re-plan once per update.
                self.steered = false;
                self.history.push(
                    json!({"tool":"done","args":{"message":message},"state":"rejected",
                        "result":{"kind":"error","message":DONE_AFTER_GOAL_UPDATE}}),
                    None,
                );
                self.next_observation = reusable;
                return Ok(Next::Again);
            }
            return Ok(Next::Done(message.to_owned()));
        }
        let action = if action["type"] == "plan" {
            let mut actions = plan_prefix(action["actions"].as_array().map(Vec::as_slice).unwrap_or(&[]));
            if actions.is_empty() {
                return Err(ApiError::unavailable("Invalid model reply: empty plan"));
            }
            let first = actions.remove(0);
            self.queue.extend(actions);
            self.plan_observation_id = observation_id.clone();
            timings["plannedActions"] = json!(self.queue.len() + 1);
            self.show_plan().await?;
            first
        } else {
            action
        };
        // A text-only query result is sufficient for app lookup/completion, but
        // never authorizes invented screen coordinates. Re-observe and re-plan first.
        if query_only && action["tool"].as_str().is_some_and(|tool| COORDINATE_TOOLS.contains(&tool)) {
            self.discard_plan().await?;
            return Ok(Next::Again);
        }
        Ok(Next::Action { action, observation_id, timings, planned: false })
    }

    /// Compares the new screen with the one before the last mutating action.
    fn track_effect(&mut self, screen: &ScreenFingerprint) -> Result<()> {
        if !self.awaiting_effect {
            return Ok(());
        }
        let Some(changed) = self.last_screen.as_ref().and_then(|before| before.changed(screen)) else {
            return Ok(());
        };
        self.awaiting_effect = false;
        self.history.mark_last_succeeded(changed);
        if let Some(entry) = self.step_log.last_mut() {
            entry["screenChanged"] = json!(changed);
        }
        self.counters.no_effect = if changed { 0 } else { self.counters.no_effect + 1 };
        if self.counters.no_effect >= MAX_NO_EFFECT {
            let last = self.step_log.last().and_then(|s| s["reason"].as_str()).unwrap_or("").to_owned();
            return Err(ApiError::unavailable(format!(
                "连续 {MAX_NO_EFFECT} 步操作后屏幕都没有变化，任务已停止，避免继续重复操作。最后一步：{last}。请检查手机画面是否被弹窗挡住或页面没有响应。"
            )));
        }
        if self.counters.no_effect == WARN_NO_EFFECT {
            self.history.push(
                json!({"tool":"gateway","state":"warning","result":{"kind":"text","text":NO_EFFECT_WARNING}}),
                None,
            );
        }
        Ok(())
    }

    /// Records a gateway rejection (nothing reached the device) for the model to re-plan.
    async fn reject(&mut self, action: &Value, error: &ApiError, reason: &str) -> Result<()> {
        log_rejection(self.session_id, self.id, action, reason);
        self.history.push(rejected_history(action, error), None);
        self.discard_plan().await
    }

    async fn set_step(&self, state: StepState, timings: Option<Value>, result: Option<Value>) -> Result<()> {
        self.update(move |t| {
            if let Some(timings) = &timings {
                t.state.timings = timings.clone();
            }
            if let Some(step) = t.state.current_step() {
                step.state = state;
                step.finished_at = Some(now());
                if timings.is_some() {
                    step.timings = timings;
                }
                if result.is_some() {
                    step.result = result;
                }
            }
        })
        .await
    }

    /// Gates and dispatches one action, then classifies its result.
    async fn dispatch(
        &mut self,
        mut action: Value,
        observation_id: &str,
        mut timings: Value,
        planned: bool,
    ) -> Result<()> {
        if action["tool"] == "iphone_wait_for"
            && action["args"]["timeout"].as_f64().is_some_and(|timeout| timeout > MAX_TASK_WAIT_SECONDS)
        {
            action["args"]["timeout"] = json!(MAX_TASK_WAIT_SECONDS);
        }
        let request = match command_request(action["tool"].as_str().unwrap_or(""), &action["args"]) {
            Ok(request) => request,
            Err(error) if error.status == 400 && self.counters.reject() => {
                return self.reject(&action, &error, &error.message.clone()).await;
            }
            Err(error) => return Err(error),
        };
        if self.typing_blocked && request["op"] == "type_text" {
            // Nothing is sent: return the action to the model to tap the field first,
            // within the same re-plan budget as other gateway rejections.
            self.discard_plan().await?;
            if self.counters.reject() {
                return self.reject(&action, &ApiError::new(409, FOCUS_FIRST), "focus_first").await;
            }
            return Err(ApiError::unavailable("Focus an editable field before typing again; task halted"));
        }
        let step_id = uuid::Uuid::new_v4().to_string();
        let auto_confirm = self.tasks.state(self.id, |t| t.auto_confirm).await?;
        let needs_confirmation = confirmation_required(&action, &request, &self.gate, auto_confirm);
        let step = StepView {
            id: step_id.clone(),
            number: 0,
            tool: action["tool"].clone(),
            args: action["args"].clone(),
            reason: action["reason"].clone(),
            state: if needs_confirmation { StepState::AwaitingConfirmation } else { StepState::Pending },
            started_at: now(),
            finished_at: None,
            result: None,
            planned,
            timings: None,
        };
        let (sender, receiver) = oneshot::channel();
        let confirmation = needs_confirmation.then(|| ConfirmationView {
            action_id: step_id.clone(),
            tool: action["tool"].clone(),
            args: action["args"].clone(),
            reason: action["reason"].clone(),
        });
        self.update(|task| {
            task.state.push_step(step);
            if let Some(confirmation) = confirmation {
                task.state.state = TaskPhase::AwaitingConfirmation;
                task.state.confirmation = Some(confirmation);
                task.confirmation = Some(sender);
            }
        })
        .await?;
        if needs_confirmation {
            let interrupt = self.tasks.arm_interrupt(self.id, &self.cancel, self.seen_updates).await?;
            let confirmed = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return Err(ApiError::new(409, "Task cancelled")),
                r = receiver => {
                    r.map_err(|_| ApiError::new(409, "Task cancelled"))?;
                    true
                }
                _ = interrupt.cancelled() => false,
            };
            self.tasks.disarm_interrupt(self.id).await;
            if !confirmed {
                // The goal changed before the user confirmed: this action was never sent.
                self.update(|t| {
                    t.confirmation = None;
                    t.state.confirmation = None;
                    t.state.state = TaskPhase::Thinking;
                    if let Some(step) = t.state.current_step() {
                        step.state = StepState::Cancelled;
                        step.finished_at = Some(now());
                    }
                })
                .await?;
                self.history.push(not_dispatched_history(&action, GOAL_CHANGED_BEFORE_CONFIRMATION), None);
                return self.discard_plan().await;
            }
        }
        self.update(|t| {
            t.state.state = TaskPhase::Executing;
            if let Some(step) = t.state.current_step() {
                step.state = StepState::Executing;
            }
        })
        .await?;
        let command = json!({
            "requestId": format!("task:{}:{step_id}", self.id), "deviceId": self.device.device_id(),
            "tool": action["tool"], "args": action["args"]
        });
        let executing = Instant::now();
        let action_stage = TaskStage::start(self.session_id, self.id, observation_id, "action");
        // A wait only reads the screen, so a goal update may abandon it safely.
        let interruptible = action["tool"] == "iphone_wait_for";
        let action_cancel = if interruptible {
            self.tasks.arm_interrupt(self.id, &self.cancel, self.seen_updates).await?
        } else {
            self.cancel.clone()
        };
        let action_result = self.device.execute(command, action_cancel.clone()).await;
        self.steered = false;
        if interruptible {
            self.tasks.disarm_interrupt(self.id).await;
        }
        action_stage.finish(if action_result.as_ref().is_ok_and(|result| result["kind"] != "error") {
            "ok"
        } else {
            "error"
        });
        timings["actionMs"] = json!(executing.elapsed().as_millis() as u64);
        if interruptible && action_cancel.is_cancelled() && !self.cancel.is_cancelled() {
            self.history.push(not_dispatched_history(&action, WAIT_INTERRUPTED), None);
            self.set_step(StepState::Cancelled, Some(timings), None).await?;
            return self.discard_plan().await;
        }
        let ios = self.device.platform() == Platform::Ios;
        let mut rejected = false;
        let result = match action_result {
            Ok(result) => result,
            Err(error) if error.status == 400 && self.counters.reject() => {
                rejected = true;
                log_rejection(self.session_id, self.id, &action, &error.message);
                json!({"kind":"error","message":rejected_message(&error)})
            }
            Err(error) if ios && recoverable_wda_error(&error.message) => {
                json!({"kind":"error","message":error.message})
            }
            Err(error) => return Err(error),
        };
        let missed = if result.get("outcome").is_some() {
            result["outcome"]["tapped_nothing"] == true
        } else {
            ["没找到", "只匹配到", "被挡住", "Not found", "Only matched", "Covered"]
                .iter()
                .any(|p| result["text"].as_str().unwrap_or("").starts_with(p))
        };
        let failed = result["kind"] == "error";
        let recoverable = failed && ios && recoverable_wda_error(result["message"].as_str().unwrap_or(""));
        let missed = action["tool"] == "iphone_tap_element" && missed;
        let state = if recoverable {
            StepState::Uncertain
        } else if rejected {
            StepState::Rejected
        } else if failed {
            StepState::Failed
        } else if missed {
            StepState::Missed
        } else {
            StepState::Succeeded
        };
        self.step_log.push(step_summary(self.step_log.len() + 1, &action, state.as_str()));
        self.history.push(
            json!({"tool":action["tool"],"args":action["args"],"reason":action["reason"],"state":state,"result":inference_result(&result)}),
            Some(self.step_log.len()),
        );
        self.gate.after_action(&request, &result);
        let tool = action["tool"].as_str().unwrap_or("");
        if state == StepState::Succeeded {
            self.counters.succeeded();
            if !QUERY_TOOLS.contains(&tool) && tool != "iphone_wait_for" {
                self.awaiting_effect = true;
            }
        }
        if result["kind"] == "image" {
            // The just-captured full frame/crop IS the next observation; do not
            // fetch another full image and send two pictures for the same query.
            self.next_observation = Some(Observation::from_value(
                json!({"deviceId":self.device.device_id(),"imageBase64":result["jpeg_base64"],"caption":result["caption"],"coordinateMapping":result["coordinateMapping"],"elementsText":""}),
            ));
        } else if matches!(tool, "iphone_status" | "iphone_list_apps" | "iphone_list_devices" | "iphone_list_elements")
            && !failed
        {
            self.next_observation =
                Some(Observation::from_value(json!({"deviceId":self.device.device_id(),"queryOnly":true,
                "caption":"Fresh read-only tool result in recentHistory; no current image or coordinate evidence.",
                "elementsText":if tool == "iphone_list_elements" { result["text"].clone() } else { Value::Null }})));
        }
        self.set_step(state, Some(timings), Some(json!({"kind":result["kind"]}))).await?;
        // The rest of a plan assumed this action succeeded and needed no review.
        if state != StepState::Succeeded || needs_confirmation || self.next_observation.is_some() {
            self.discard_plan().await?;
        }
        if rejected {
            // Nothing reached the device; observe again and let the model re-plan.
            return Ok(());
        }
        if failed && !recoverable && tool == "iphone_wait_for" && self.counters.wait_misses < MAX_WAIT_MISSES {
            // A wait that timed out only read the screen; let the model look again.
            self.counters.wait_misses += 1;
            return Ok(());
        }
        if failed {
            if recoverable {
                self.next_observation = Some(self.recover().await?);
                if needs_confirmation || request["op"] == "type_text" {
                    return Err(ApiError::unavailable(
                        "设备已恢复，但上一步操作结果不确定。请先核对手机上的实际结果，再重新开始任务。",
                    ));
                }
                return Ok(());
            }
            // These exact native preflight errors guarantee that no text was sent.
            // Re-observe and re-plan once, requiring a successful tap before typing.
            let no_input_sent = ios
                && request["op"] == "type_text"
                && matches!(
                    result["message"].as_str(),
                    Some(
                        "No keyboard on screen (no text field has focus), nothing typed; tap a text field first and check with a screenshot that the keyboard is up"
                            | "屏幕上没有键盘(没有输入框获得焦点),什么都没输;先点一下输入框,截图确认键盘出来了再输入"
                    )
                );
            if no_input_sent && self.counters.focus_recoveries == 0 {
                self.counters.focus_recoveries += 1;
                self.typing_blocked = true;
                self.input_pending = true;
                self.next_observation = None;
                return Ok(());
            }
            let cause = result["message"]
                .as_str()
                .filter(|message| !message.trim().is_empty())
                .map(|message| message.chars().take(2000).collect::<String>())
                .unwrap_or_else(|| "Native action failed".into());
            return Err(ApiError::unavailable(cause));
        }
        if missed {
            self.counters.element_misses += 1;
            if !continue_after_element_miss(self.counters.element_misses) {
                return Err(ApiError::new(409, "Element tap missed; task halted"));
            }
        }
        // A missed element tap changed nothing, so it cannot have focused a field.
        if !missed && matches!(request["op"].as_str(), Some("tap" | "tap_element")) {
            self.typing_blocked = false;
        }
        if request["op"] == "type_text" {
            self.input_pending = false;
        }
        Ok(())
    }
}

const NO_EFFECT_WARNING: &str = "Warning from the gateway: the last actions did not change the screen (screenChanged:false). Do not repeat them unchanged. Try a different target, iphone_tap_element with an exact label from iphone_list_elements, a swipe or scroll, dismissing a dialog or keyboard, or reply done explaining what blocks the task.";

/// A screen identity: a grayscale thumbnail of the screenshot and a hash of the UI element text.
struct ScreenFingerprint {
    image: Option<Vec<u8>>,
    elements: Option<u64>,
}
impl ScreenFingerprint {
    async fn of(observation: &Observation) -> Option<Self> {
        let image = match observation.jpeg.clone() {
            Some(bytes) => tokio::task::spawn_blocking(move || thumbnail(&bytes)).await.ok().flatten(),
            None => None,
        };
        let elements = observation.value["elementsText"].as_str().filter(|text| !text.is_empty()).map(|text| {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            text.hash(&mut hasher);
            hasher.finish()
        });
        (image.is_some() || elements.is_some()).then_some(Self { image, elements })
    }
    /// Whether the screen changed; unknown when the two observations share no evidence.
    fn changed(&self, after: &Self) -> Option<bool> {
        let same_image = self
            .image
            .as_ref()
            .zip(after.image.as_ref())
            .map(|(a, b)| a.iter().zip(b).filter(|(x, y)| x.abs_diff(**y) > PIXEL_DELTA).count() <= CHANGED_PIXELS);
        let same_elements = self.elements.zip(after.elements).map(|(a, b)| a == b);
        // Unchanged only when every available signal agrees: a scrolled list keeps a
        // similar thumbnail but shows different elements.
        match (same_image, same_elements) {
            (None, None) => None,
            (Some(same), None) | (None, Some(same)) => Some(!same),
            (Some(image), Some(elements)) => Some(!(image && elements)),
        }
    }
}
/// Grayscale thumbnail of a decoded screenshot, `THUMBNAIL_SIDE` pixels square.
fn thumbnail(bytes: &[u8]) -> Option<Vec<u8>> {
    let image = image::load_from_memory(bytes).ok()?;
    let pixels = image.thumbnail_exact(THUMBNAIL_SIDE, THUMBNAIL_SIDE).to_luma8().into_raw();
    (pixels.len() == (THUMBNAIL_SIDE * THUMBNAIL_SIDE) as usize).then_some(pixels)
}

/// One line of the task step log sent to the model.
fn step_summary(number: usize, action: &Value, state: &str) -> Value {
    let mut args = action["args"].clone();
    if let Some(text) = args.get("text").and_then(Value::as_str).map(|t| t.chars().take(40).collect::<String>()) {
        args["text"] = json!(text);
    }
    json!({"step":number,"tool":action["tool"],"args":args,"state":state,
        "reason":action["reason"].as_str().unwrap_or("").chars().take(80).collect::<String>()})
}

/// History sent to the model: when older entries were trimmed, a step log of the
/// steps that the retained history no longer details comes first, so the model
/// does not redo completed work. The cut is where the retained history actually
/// starts: it is trimmed by size too and holds entries (warnings, rejections)
/// that are not steps, so a fixed count would drop some steps from both.
fn with_step_log(step_log: &[Value], history: &[HistoryEntry]) -> Vec<Value> {
    let detailed = history.iter().find_map(|entry| entry.step).map_or(step_log.len(), |step| step - 1);
    let earlier = &step_log[..detailed.min(step_log.len())];
    let history = history.iter().map(|entry| entry.value.clone());
    if earlier.is_empty() {
        return history.collect();
    }
    let shown = &earlier[earlier.len().saturating_sub(STEP_LOG_ENTRIES)..];
    let mut out = vec![json!({
        "stepLog": shown, "omittedSteps": earlier.len() - shown.len(),
        "note": "Earlier steps of this task, oldest first. The entries after this one detail the latest steps. Do not repeat completed steps."
    })];
    out.extend(history);
    out
}

/// History sent to the model: earlier tasks of this session, then this task's
/// step log and detailed history.
fn model_history(previous_tasks: &[Value], step_log: &[Value], history: &[HistoryEntry]) -> Vec<Value> {
    let mut out = with_step_log(step_log, history);
    if !previous_tasks.is_empty() {
        out.insert(0, json!({
            "previousTasks": previous_tasks,
            "note": "Earlier tasks in this session, oldest first. The current instruction may refer to them (for example continue, again, the same, undo). Use them as context; do not redo them unless the current instruction asks to."
        }));
    }
    out
}

/// What a finished task asked for and how it ended, for the next task of the session.
fn task_summary(snapshot: &Value) -> Value {
    let bounded = |value: &Value, max: usize| value.as_str().map(|s| s.chars().take(max).collect::<String>());
    let steps = snapshot["steps"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let last_steps: Vec<Value> = steps[steps.len().saturating_sub(5)..]
        .iter()
        .map(|step| json!({"tool":step["tool"],"state":step["state"],"reason":bounded(&step["reason"], 80)}))
        .collect();
    let updates: Vec<Value> = snapshot["instructionUpdates"]
        .as_array()
        .map(|updates| updates.iter().map(|update| json!(bounded(&update["instruction"], 300))).collect())
        .unwrap_or_default();
    json!({
        "instruction": bounded(&snapshot["instruction"], 500), "goalUpdates": updates, "state": snapshot["state"],
        "result": bounded(&snapshot["message"], 500).or_else(|| bounded(&snapshot["error"], 500)),
        "steps": snapshot["totalSteps"].as_u64().map_or(steps.len(), |total| total as usize), "lastSteps": last_steps,
    })
}

/// Content-free record of a gateway rejection; the message is gateway-generated.
fn log_rejection(session_id: &str, task_id: &str, action: &Value, reason: &str) {
    eprintln!(
        "{}",
        json!({"at":now(),"event":"task_action_rejected","session_id":session_id,"task_id":task_id,
            "tool":action["tool"],"reason":reason.chars().take(160).collect::<String>()})
    );
}

/// The goal sent to the model. After the user changes the goal mid-task, the
/// model also sees the most recent earlier goals so it can build on (or undo)
/// steps already taken for them without pursuing them further.
fn model_instruction(current: &str, updates: &[InstructionUpdate]) -> String {
    let earlier: Vec<&str> = updates.iter().map(|update| update.previous.as_str()).collect();
    if earlier.is_empty() {
        return current.to_owned();
    }
    let mut text = format!(
        "CURRENT GOAL (the user changed the goal while this task was running; it replaces every earlier goal): {current}\nEarlier goals, oldest first, for context only:"
    );
    for goal in &earlier[earlier.len().saturating_sub(EARLIER_GOALS)..] {
        text.push_str("\n- ");
        text.push_str(&goal.chars().take(1000).collect::<String>());
    }
    text.push_str("\nSteps in recentHistory may have served an earlier goal. Build on them when they help the current goal, undo them only when the current goal requires it, and never repeat completed steps.");
    text
}

/// Keeps the longest prefix of a model plan that may run without re-observing.
/// The first action is always kept; it has the same checks as a single action.
fn plan_prefix(actions: &[Value]) -> Vec<Value> {
    let mut kept: Vec<Value> = Vec::new();
    for action in actions.iter().take(crate::model::MAX_PLAN_ACTIONS) {
        if let Some(previous) = kept.last() {
            let tool = action["tool"].as_str().unwrap_or("");
            let previous_tool = previous["tool"].as_str().unwrap_or("");
            if COORDINATE_TOOLS.contains(&tool)
                || QUERY_TOOLS.contains(&previous_tool)
                || previous["requiresConfirmation"] == true
                || (tool == "iphone_type_text" && !["iphone_tap", "iphone_tap_element"].contains(&previous_tool))
            {
                break;
            }
        }
        kept.push(action.clone());
    }
    kept
}

fn plan_view(queue: &VecDeque<Value>) -> Vec<PlanView> {
    queue.iter().map(|action| PlanView { tool: action["tool"].clone(), reason: action["reason"].clone() }).collect()
}

const GOAL_CHANGED_BEFORE_CONFIRMATION: &str = "Not sent to the device and not refused: the user updated the goal while this action awaited confirmation. Continue with the CURRENT GOAL in the instruction.";
const DONE_AFTER_GOAL_UPDATE: &str = "Not finished: the user just updated the goal (CURRENT GOAL in the instruction). Earlier actions were replaced, not refused. Plan the next action for the current goal; reply done only if the current goal is already achieved, and say how.";
/// Whether an updated goal tells the task to stop rather than to do something else.
fn stops_task(instruction: &str) -> bool {
    static STOP: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)停止|别做了|不用了|不要做了|取消任务|算了|结束任务|\bstop\b|\bcancel\b|never ?mind").unwrap()
    });
    STOP.is_match(instruction)
}
const WAIT_INTERRUPTED: &str = "Interrupted by a goal update; this read-only wait changed nothing on the device.";
fn not_dispatched_history(action: &Value, message: &str) -> Value {
    json!({"tool":action["tool"],"args":action["args"],"reason":action["reason"],"state":"cancelled",
        "result":{"kind":"text","text":message}})
}
fn valid_instruction(instruction: &str) -> Result<&str> {
    if instruction.trim().is_empty() || instruction.chars().count() > 4000 || instruction.contains('\0') {
        return Err(ApiError::new(400, "Invalid task instruction"));
    }
    Ok(instruction.trim())
}
fn direct_command_for(instruction: &str, platform: Platform) -> Option<Value> {
    let mut action = direct_command(instruction)?;
    if platform == Platform::Android {
        match action["args"]["app"].as_str() {
            Some("com.apple.Preferences") => action["args"]["app"] = json!("com.android.settings"),
            Some(_) => return None, // Clock package IDs vary between Android vendors; let the model look up apps.
            None => {}
        }
    }
    Some(action)
}
fn direct_command(instruction: &str) -> Option<Value> {
    let text = instruction.trim().trim_end_matches(['。', '.', '!', '！']).to_lowercase();
    let (tool, args) = match text.as_str() {
        "回到主屏幕" | "返回主屏幕" | "回到桌面" | "go home" | "go to home screen" => {
            ("iphone_press_button", json!({"button":"home"}))
        }
        "打开设置" | "open settings" => ("iphone_launch_app", json!({"app":"com.apple.Preferences"})),
        "打开时钟" | "open clock" => ("iphone_launch_app", json!({"app":"com.apple.mobiletimer"})),
        _ => return None,
    };
    Some(
        json!({"type":"action","tool":tool,"args":args,"reason":"执行用户明确指定的单步操作，随后观察并验证结果","requiresConfirmation":false}),
    )
}
const FOCUS_FIRST: &str = "No editable field has focus: the previous typing found no keyboard. Tap the intended text field first and confirm the keyboard in the next observation before typing.";
fn rejected_message(error: &ApiError) -> String {
    format!("Rejected by the gateway before reaching the device, nothing was done: {}", error.message)
}
fn rejected_history(action: &Value, error: &ApiError) -> Value {
    json!({"tool":action["tool"],"args":action["args"],"reason":action["reason"],"state":"rejected",
        "result":{"kind":"error","message":rejected_message(error)}})
}

fn continue_after_element_miss(misses: u32) -> bool {
    misses <= MAX_ELEMENT_MISSES
}

const DONE_IS_NOT_CONFIRMATION: &str = "Not finished: your done message asked the user for confirmation, but done ends the task and the user cannot answer it. Return the next action itself with requiresConfirmation:true so the user can approve it in the interface, or continue the task if no confirmation is needed.";
/// Whether a done message asks the user to confirm or choose instead of reporting a result.
fn asks_for_confirmation(message: &str) -> bool {
    static ASKS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)请确认|是否(继续|要|需要|确认|同意)|要不要|需要你(的)?确认|确认(后|一下)?(我|再)?(继续|执行)|\bconfirm\b|\bproceed\?|\bcontinue\?|would you like|shall i|should i",)
        .unwrap()
    });
    ASKS.is_match(message)
}

/// Total history sent to the model. Input tokens drive time to first token, so
/// only the newest result keeps its full text; older results keep a short summary.
const HISTORY_CHARS: usize = 16000;
const OLDER_RESULT_CHARS: usize = 1500;

/// Shortens an older history entry's result; only the newest keeps its full text.
fn compact_entry(entry: &mut Value) {
    for key in ["text", "caption", "message"] {
        if let Some(text) = entry["result"][key].as_str()
            && text.chars().count() > OLDER_RESULT_CHARS
        {
            entry["result"][key] = json!(text.chars().take(OLDER_RESULT_CHARS).collect::<String>());
            entry["result"]["truncated"] = json!(true);
        }
    }
}

fn inference_result(result: &Value) -> Value {
    let mut summary = json!({"kind":result["kind"]});
    for key in ["text", "caption", "message"] {
        if let Some(text) = result[key].as_str() {
            let bounded: String = text.chars().take(6000).collect();
            summary[key] = json!(bounded);
            if text.chars().count() > 6000 {
                summary["truncated"] = json!(true);
            }
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn task_stage_events_include_only_trace_and_timing_metadata() {
        let stage = TaskStage::start("session-fixture", "task-fixture", "observation-fixture", "model");
        let event = stage.event("end", "error");
        assert_eq!(event["session_id"], "session-fixture");
        assert_eq!(event["task_id"], "task-fixture");
        assert_eq!(event["observation_id"], "observation-fixture");
        assert_eq!(event["stage"], "model");
        assert_eq!(event["phase"], "end");
        assert_eq!(event["status"], "error");
        let mut keys: Vec<_> = event.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["at", "elapsed_ms", "event", "observation_id", "phase", "session_id", "stage", "status", "task_id"]
        );
        stage.finish("error");
    }

    #[test]
    fn only_the_newest_history_result_keeps_its_full_text() {
        let long = "x".repeat(5000);
        let mut history = History::default();
        history.push(json!({"tool":"iphone_list_apps","result":{"kind":"text","text":long}}), None);
        history.push(json!({"tool":"iphone_tap","result":{"kind":"text","text":"Tapped"}}), Some(1));
        history.push(json!({"tool":"iphone_list_elements","result":{"kind":"text","text":long}}), None);
        history.trim();
        let history: Vec<Value> = history.entries.into_iter().map(|e| e.value).collect();
        assert_eq!(history[0]["result"]["text"].as_str().unwrap().len(), OLDER_RESULT_CHARS);
        assert_eq!(history[0]["result"]["truncated"], true);
        assert_eq!(history[1]["result"]["text"], "Tapped");
        assert!(history[1]["result"].get("truncated").is_none());
        assert_eq!(history[2]["result"]["text"].as_str().unwrap().len(), 5000);
        History::default().trim();
    }

    #[test]
    fn rejected_actions_are_reported_as_not_dispatched() {
        let action = json!({"tool":"iphone_press_button","args":{"button":"back"},"reason":"Go back"});
        let entry = rejected_history(&action, &ApiError::new(400, "Back is supported on Android only"));
        assert_eq!(entry["state"], "rejected");
        assert_eq!(entry["tool"], "iphone_press_button");
        let message = entry["result"]["message"].as_str().unwrap();
        assert!(
            message.contains("before reaching the device") && message.ends_with("Back is supported on Android only")
        );
        assert!(command_request("iphone_unknown", &json!({})).unwrap_err().status == 400);
    }

    #[test]
    fn typing_before_a_focus_tap_is_returned_to_the_model_without_dispatch() {
        let action = json!({"tool":"iphone_type_text","args":{"text":"bilibili"},"reason":"Search"});
        let entry = rejected_history(&action, &ApiError::new(409, FOCUS_FIRST));
        assert_eq!(entry["state"], "rejected");
        let message = entry["result"]["message"].as_str().unwrap();
        assert!(
            message.contains("before reaching the device") && message.contains("Tap the intended text field first")
        );
    }

    #[test]
    fn tasks_continue_after_a_bounded_number_of_element_misses() {
        assert!(continue_after_element_miss(1));
        assert!(continue_after_element_miss(MAX_ELEMENT_MISSES));
        assert!(!continue_after_element_miss(MAX_ELEMENT_MISSES + 1));
    }

    #[test]
    fn confirmation_is_limited_to_outgoing_or_destructive_actions_and_auto_confirm_keeps_payments() {
        let action = |tool: &str, args: Value, reason: &str| json!({"tool":tool,"args":args,"reason":reason});
        let needs = |action: &Value, auto: bool| {
            let request = command_request(action["tool"].as_str().unwrap(), &action["args"]).unwrap();
            confirmation_required(action, &request, &GateContext::default(), auto)
        };
        let search = action("iphone_type_text", json!({"text":"天气","submit":true}), "搜索天气");
        let message = action("iphone_type_text", json!({"text":"你好","submit":true}), "发送消息");
        let delete = action("iphone_tap", json!({"x":1,"y":2}), "点击删除");
        let pay = action("iphone_tap", json!({"x":1,"y":2}), "点击立即支付");
        let open = action("iphone_tap", json!({"x":1,"y":2}), "打开设置");
        assert!(!needs(&search, false));
        assert!(needs(&message, false));
        assert!(needs(&delete, false));
        assert!(!needs(&open, false));
        assert!(!needs(&message, true) && !needs(&delete, true));
        assert!(needs(&pay, true) && needs(&pay, false));
    }

    fn encode(image: &image::RgbImage) -> String {
        use base64::Engine;
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        base64::engine::general_purpose::STANDARD.encode(bytes.into_inner())
    }
    async fn fingerprint_of(value: Value) -> Option<ScreenFingerprint> {
        ScreenFingerprint::of(&Observation::from_value(value)).await
    }

    fn png(shade: u8) -> String {
        encode(&image::RgbImage::from_fn(32, 64, |x, _| {
            if x < 16 { image::Rgb([shade, shade, shade]) } else { image::Rgb([255 - shade, 0, 0]) }
        }))
    }

    /// A 64x64 board with a white square of `side` pixels at the top left.
    fn board(side: u32) -> String {
        encode(&image::RgbImage::from_fn(64, 64, |x, y| {
            if x < side && y < side { image::Rgb([255, 255, 255]) } else { image::Rgb([40, 90, 160]) }
        }))
    }

    #[tokio::test]
    async fn a_single_cleared_tile_counts_as_a_change_but_noise_does_not() {
        let fingerprint = |image: String| async move { fingerprint_of(json!({"imageBase64":image})).await.unwrap() };
        let before = fingerprint(board(0)).await;
        assert_eq!(before.changed(&fingerprint(board(0)).await), Some(false));
        assert_eq!(before.changed(&fingerprint(board(16)).await), Some(true), "one tile of a 4x4 board");
        assert_eq!(before.changed(&fingerprint(board(2)).await), Some(false), "a tiny difference is noise");
    }

    #[tokio::test]
    async fn screen_fingerprints_detect_unchanged_and_changed_screens() {
        let a = fingerprint_of(json!({"imageBase64":png(0),"elementsText":"Button: A"})).await.unwrap();
        let same = fingerprint_of(json!({"imageBase64":png(0),"elementsText":"Button: A"})).await.unwrap();
        let scrolled = fingerprint_of(json!({"imageBase64":png(0),"elementsText":"Button: B"})).await.unwrap();
        let other = fingerprint_of(json!({"imageBase64":png(255),"elementsText":"Button: A"})).await.unwrap();
        assert_eq!(a.changed(&same), Some(false));
        assert_eq!(a.changed(&scrolled), Some(true), "a similar thumbnail with new elements is a change");
        assert_eq!(a.changed(&other), Some(true));
        let tree = fingerprint_of(json!({"elementsText":"Button: A"})).await.unwrap();
        assert_eq!(a.changed(&tree), Some(false), "same tree without an image");
        assert!(fingerprint_of(json!({"queryOnly":true})).await.is_none());
    }

    #[test]
    fn step_log_precedes_detailed_history_only_after_it_was_trimmed() {
        let action = json!({"tool":"iphone_type_text","args":{"text":"x".repeat(100)},"reason":"r".repeat(200)});
        let entry = step_summary(1, &action, "succeeded");
        assert_eq!(entry["args"]["text"].as_str().unwrap().len(), 40);
        assert_eq!(entry["reason"].as_str().unwrap().len(), 80);
        let entry = |step: Option<usize>| HistoryEntry { step, value: json!({"tool":"iphone_tap","step":step}) };
        let short: Vec<Value> = (1..=3).map(|n| step_summary(n, &action, "succeeded")).collect();
        let history: Vec<HistoryEntry> = (1..=3).map(|n| entry(Some(n))).collect();
        assert_eq!(with_step_log(&short, &history).len(), 3, "every step is still detailed");
        let long: Vec<Value> = (1..=60).map(|n| step_summary(n, &action, "succeeded")).collect();
        let sent = with_step_log(&long, &[entry(Some(60))]);
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0]["stepLog"].as_array().unwrap().len(), STEP_LOG_ENTRIES);
        assert_eq!(sent[0]["omittedSteps"], 59 - STEP_LOG_ENTRIES);
        assert_eq!(sent[0]["stepLog"][STEP_LOG_ENTRIES - 1]["step"], 59);
    }

    #[test]
    fn step_log_covers_exactly_the_steps_the_trimmed_history_no_longer_details() {
        let action = json!({"tool":"iphone_tap","args":{"x":1,"y":2},"reason":"r"});
        let log: Vec<Value> = (1..=12).map(|n| step_summary(n, &action, "succeeded")).collect();
        let entry = |step: Option<usize>| HistoryEntry { step, value: json!({"tool":"gateway","step":step}) };
        // Warnings and rejections are history entries without steps; the size cap
        // left only steps 11 and 12 although fewer than DETAILED_HISTORY entries remain.
        let history = vec![entry(None), entry(Some(11)), entry(None), entry(Some(12))];
        let sent = with_step_log(&log, &history);
        let shown: Vec<u64> =
            sent[0]["stepLog"].as_array().unwrap().iter().map(|s| s["step"].as_u64().unwrap()).collect();
        assert_eq!(shown, (1..=10).collect::<Vec<_>>());
        assert_eq!(sent[0]["omittedSteps"], 0);
        assert_eq!(sent.len(), 5);
        // Only non-step entries were retained: every step is summarized.
        let sent = with_step_log(&log, &[entry(None)]);
        assert_eq!(sent[0]["stepLog"].as_array().unwrap().len(), 12);
        assert_eq!(with_step_log(&[], &[entry(None)]).len(), 1);
    }

    #[test]
    fn previous_tasks_lead_the_model_history_with_bounded_summaries() {
        let snapshot = json!({
            "instruction": "打开时钟", "state": "completed", "message": "已打开时钟", "error": null,
            "instructionUpdates": [{"instruction": "改为打开秒表"}],
            "steps": (0..12).map(|n| json!({"tool":"iphone_tap","state":"succeeded","reason":format!("step {n}")})).collect::<Vec<_>>(),
        });
        let summary = task_summary(&snapshot);
        assert_eq!(summary["result"], "已打开时钟");
        assert_eq!(summary["goalUpdates"], json!(["改为打开秒表"]));
        assert_eq!(summary["steps"], 12);
        assert_eq!(summary["lastSteps"].as_array().unwrap().len(), 5);
        assert_eq!(summary["lastSteps"][0]["reason"], "step 7");
        let history = vec![HistoryEntry { step: None, value: json!({"tool":"iphone_tap"}) }];
        assert_eq!(model_history(&[], &[], &history), vec![history[0].value.clone()]);
        let sent = model_history(std::slice::from_ref(&summary), &[], &history);
        assert_eq!(sent[0]["previousTasks"], json!([summary]));
        assert_eq!(sent[1], history[0].value);
        let capped = task_summary(&json!({"steps":[], "totalSteps": 250}));
        assert_eq!(capped["steps"], 250, "a capped snapshot still reports every step");
    }

    #[test]
    fn only_goal_updates_that_say_stop_may_finish_without_a_replan() {
        assert!(stops_task("算了，不用了"));
        assert!(stops_task("Stop here"));
        assert!(!stops_task("改为打开时钟"));
        assert!(!stops_task("Open the Settings app instead"));
    }

    #[test]
    fn done_messages_that_ask_for_confirmation_are_recognized() {
        for message in [
            "切换 App Store 账号会更改账户设置。请确认是否继续退出当前账号并登录你提供的账号。",
            "是否继续删除这条消息？",
            "Should I proceed? Please confirm the purchase.",
            "Would you like me to send it?",
        ] {
            assert!(asks_for_confirmation(message), "{message}");
        }
        for message in
            ["已打开设置，系统版本为 iOS 26.3.1。", "Alarm list shows 3 alarms.", "无法继续：未找到登录入口。"]
        {
            assert!(!asks_for_confirmation(message), "{message}");
        }
    }

    #[test]
    fn direct_commands_are_exact_allowlisted_and_do_not_match_compound_or_negative_requests() {
        assert_eq!(direct_command(" 回到主屏幕。 ").unwrap()["args"]["button"], "home");
        assert_eq!(direct_command("OPEN SETTINGS").unwrap()["args"]["app"], "com.apple.Preferences");
        for instruction in
            ["不要回到主屏幕", "打开设置并关闭密码", "打开时钟并删除闹钟", "pay", "send message", "回到主屏幕然后支付"]
        {
            assert!(direct_command(instruction).is_none(), "{instruction}");
        }
    }
    #[test]
    fn android_shortcuts_use_android_packages_and_clock_falls_back_to_lookup() {
        use crate::identity::Platform;
        assert_eq!(direct_command_for("打开设置", Platform::Android).unwrap()["args"]["app"], "com.android.settings");
        assert!(direct_command_for("打开时钟", Platform::Android).is_none());
        assert_eq!(direct_command_for("打开设置", Platform::Ios).unwrap()["args"]["app"], "com.apple.Preferences");
    }
    #[test]
    fn inference_preserves_query_text_without_copying_image_data() {
        let result = json!({"kind":"text","text":"App: com.example.fixture"});
        assert_eq!(inference_result(&result)["text"], result["text"]);
        let result = json!({"kind":"image","caption":"crop at x=10 y=20","jpeg_base64":"private-image"});
        assert_eq!(inference_result(&result)["caption"], result["caption"]);
        assert!(inference_result(&result).get("jpeg_base64").is_none());
        let result = inference_result(&json!({"kind":"text","text":"中".repeat(7000)}));
        assert_eq!(result["text"].as_str().unwrap().chars().count(), 6000);
        assert_eq!(result["truncated"], true);
    }
    #[test]
    fn plans_keep_only_actions_that_need_no_fresh_observation() {
        let a =
            |tool: &str| json!({"type":"action","tool":tool,"args":{},"reason":"fixture","requiresConfirmation":false});
        let tools = |plan: Vec<Value>| {
            plan_prefix(&plan).iter().map(|v| v["tool"].as_str().unwrap().to_owned()).collect::<Vec<_>>()
        };
        assert_eq!(
            tools(vec![a("iphone_tap"), a("iphone_type_text"), a("iphone_wait_for"), a("iphone_press_button")]),
            ["iphone_tap", "iphone_type_text", "iphone_wait_for", "iphone_press_button"]
        );
        // Later coordinates would come from a stale observation.
        assert_eq!(tools(vec![a("iphone_tap"), a("iphone_swipe")]), ["iphone_tap"]);
        // Typing must directly follow the tap that focuses its field.
        assert_eq!(tools(vec![a("iphone_launch_app"), a("iphone_type_text")]), ["iphone_launch_app"]);
        // A read-only result becomes the next observation.
        assert_eq!(tools(vec![a("iphone_screenshot"), a("iphone_press_button")]), ["iphone_screenshot"]);
        let mut sensitive = a("iphone_tap_element");
        sensitive["requiresConfirmation"] = json!(true);
        assert_eq!(tools(vec![sensitive, a("iphone_press_button")]), ["iphone_tap_element"]);
        assert_eq!(tools(vec![a("iphone_press_button"); 6]).len(), crate::model::MAX_PLAN_ACTIONS);
        assert!(plan_prefix(&[]).is_empty());
    }

    #[test]
    fn validated_plans_normalize_actions_and_reject_unsafe_shapes() {
        let plan = validate_reply(json!({"type":"plan","actions":[
            {"tool":"iphone_tap","args":{"x":1,"y":2},"reason":"Focus"},
            {"tool":"iphone_type_text","args":{"text":"hi","submit":false},"reason":"Type"}
        ]}))
        .unwrap();
        assert_eq!(plan["type"], "plan");
        assert_eq!(plan["actions"][0]["type"], "action");
        assert_eq!(plan["actions"][1]["requiresConfirmation"], false);
        let single = validate_reply(json!({"type":"plan","actions":[
            {"tool":"iphone_press_button","args":{"button":"home"},"reason":"Home"}
        ]}))
        .unwrap();
        assert_eq!(single["type"], "action");
        let five = Value::Array(vec![json!({"tool":"iphone_status","args":{},"reason":"Status"}); 5]);
        for (reply, detail) in [
            (json!({"type":"plan","actions":[]}), "nonempty"),
            (
                json!({"type":"plan","actions":[{"tool":"iphone_status","args":{},"reason":"r"}],"reason":"x"}),
                "only accepts",
            ),
            (json!({"type":"plan","actions":[{"type":"done","message":"ok"}]}), "must be an action"),
            (json!({"type":"plan","actions":[{"type":"plan","actions":[]}]}), "must be an action"),
            (
                json!({"type":"plan","actions":[{"tool":"iphone_tap","args":{"x":"bad","y":1},"reason":"r"}]}),
                "plan action 1: args:",
            ),
            (json!({"type":"plan","actions":five}), "at most"),
        ] {
            let error = validate_reply(reply).unwrap_err();
            assert!(error.message.contains(detail), "{}", error.message);
        }
    }

    #[test]
    fn goal_updates_reach_the_model_with_bounded_earlier_goals() {
        assert_eq!(model_instruction("Open Clock", &[]), "Open Clock");
        let updates: Vec<InstructionUpdate> = (1..=5)
            .map(|i| InstructionUpdate {
                at: now(),
                instruction: "x".into(),
                previous: format!("goal {i}"),
                after_step: 0,
            })
            .collect();
        let text = model_instruction("Open Clock", &updates);
        assert!(text.starts_with("CURRENT GOAL"), "{text}");
        assert!(text.contains("Open Clock"));
        assert!(!text.contains("goal 2") && text.contains("goal 3") && text.contains("goal 5"), "{text}");
        assert!(valid_instruction(" ").is_err());
        assert!(valid_instruction(&"x".repeat(4001)).is_err());
        assert!(valid_instruction("a\0").is_err());
        assert_eq!(valid_instruction("  New goal ").unwrap(), "New goal");
    }

    // ---- Task loop with fake device and planner ----

    use std::sync::Mutex as StdMutex;

    #[derive(Default)]
    struct FakeDeviceState {
        elements: Option<String>,
        results: StdMutex<VecDeque<Value>>,
        executed: StdMutex<Vec<Value>>,
    }
    #[derive(Clone, Default)]
    struct FakeDevice(Arc<FakeDeviceState>);
    impl TaskDevice for FakeDevice {
        fn device_id(&self) -> &str {
            "fake-device"
        }
        fn platform(&self) -> Platform {
            Platform::Ios
        }
        async fn observe(&self, _elements: bool, _cancel: CancellationToken) -> Result<Observation> {
            let mut value = json!({"deviceId":"fake-device","caption":"fake 390x844 points"});
            if let Some(elements) = &self.0.elements {
                value["elementsText"] = json!(elements);
            }
            Ok(Observation::from_value(value))
        }
        async fn ready(&self, _cancel: CancellationToken) -> Result<bool> {
            Ok(true)
        }
        async fn execute(&self, command: Value, _cancel: CancellationToken) -> Result<Value> {
            self.0.executed.lock().unwrap().push(command);
            Ok(self.0.results.lock().unwrap().pop_front().unwrap_or_else(|| json!({"kind":"text","text":"ok"})))
        }
    }
    #[derive(Default)]
    struct FakePlannerState {
        replies: StdMutex<VecDeque<Value>>,
        follow_ups: StdMutex<Vec<FollowUp>>,
        histories: StdMutex<Vec<Vec<Value>>>,
    }
    #[derive(Clone, Default)]
    struct FakePlanner(Arc<FakePlannerState>);
    impl TaskPlanner for FakePlanner {
        fn wants_elements(&self) -> bool {
            true
        }
        async fn recognize(&self, _observation: &Observation, _cancel: CancellationToken) -> Option<Result<Value>> {
            None
        }
        async fn plan(&self, request: PlanRequest<'_>, _cancel: CancellationToken) -> Result<Value> {
            self.0.follow_ups.lock().unwrap().push(request.follow_up.clone().unwrap_or_default());
            self.0.histories.lock().unwrap().push(request.history.clone());
            let reply = self.0.replies.lock().unwrap().pop_front();
            Ok(reply.unwrap_or_else(|| json!({"type":"done","message":"out of replies"})))
        }
    }
    fn act(tool: &str, args: Value, reason: &str) -> Value {
        json!({"type":"action","tool":tool,"args":args,"reason":reason,"requiresConfirmation":false})
    }
    fn missed_element(text: &str) -> Value {
        act("iphone_tap_element", json!({"text":text}), "Tap the tile")
    }
    const NOT_FOUND: &str = "Not found: no element matches";
    struct Fixture {
        tasks: TaskManager,
        device: FakeDevice,
        planner: FakePlanner,
        cancel: CancellationToken,
    }
    async fn fixture(replies: Vec<Value>, results: Vec<Value>, elements: Option<&str>, auto_confirm: bool) -> Fixture {
        let tasks = TaskManager::new();
        let cancel = CancellationToken::new();
        let state = TaskState::new("task".into(), "Play the game", Some("fake".into()), vec![], auto_confirm);
        tasks.install("session", state, cancel.clone(), None).await.unwrap();
        let device = FakeDevice(Arc::new(FakeDeviceState {
            elements: elements.map(str::to_owned),
            results: StdMutex::new(results.into()),
            executed: StdMutex::default(),
        }));
        let planner =
            FakePlanner(Arc::new(FakePlannerState { replies: StdMutex::new(replies.into()), ..Default::default() }));
        Fixture { tasks, device, planner, cancel }
    }
    impl Fixture {
        fn task(&self) -> TaskLoop<'_, FakeDevice, FakePlanner> {
            TaskLoop::new(
                &self.tasks,
                "session",
                "task",
                self.device.clone(),
                self.planner.clone(),
                vec![],
                vec![],
                self.cancel.clone(),
            )
        }
    }

    #[tokio::test]
    async fn re_plan_budgets_count_consecutive_misses_and_reset_after_a_success() {
        let not_found = || json!({"kind":"text","text":NOT_FOUND});
        let replies = vec![
            missed_element("A"),
            missed_element("B"),
            act("iphone_press_button", json!({"button":"home"}), "Reset the view"),
            missed_element("C"),
            missed_element("D"),
            json!({"type":"done","message":"finished"}),
        ];
        let results =
            vec![not_found(), not_found(), json!({"kind":"text","text":"Pressed home"}), not_found(), not_found()];
        let f = fixture(replies, results, None, false).await;
        // Four misses in total: a whole-task count of MAX_ELEMENT_MISSES would have halted.
        assert_eq!(f.task().run().await.unwrap(), "finished");
        assert_eq!(f.device.0.executed.lock().unwrap().len(), 5);
        let steps = f.tasks.snapshot().await["steps"].clone();
        let states: Vec<&str> = steps.as_array().unwrap().iter().map(|s| s["state"].as_str().unwrap()).collect();
        assert_eq!(states, ["missed", "missed", "succeeded", "missed", "missed"]);

        let replies = vec![missed_element("A"), missed_element("B"), missed_element("C")];
        let f = fixture(replies, vec![not_found(), not_found(), not_found()], None, false).await;
        let error = f.task().run().await.unwrap_err();
        assert_eq!((error.status, error.message.as_str()), (409, "Element tap missed; task halted"));
    }

    #[tokio::test]
    async fn rejected_actions_are_consecutive_too() {
        let invalid = || act("iphone_press_button", json!({"button":"back"}), "Go back");
        let replies = vec![
            invalid(),
            invalid(),
            act("iphone_press_button", json!({"button":"home"}), "Home"),
            invalid(),
            invalid(),
            json!({"type":"done","message":"finished"}),
        ];
        // The fake reports rejections like the gateway: an HTTP 400 that reached no device.
        struct Rejecting(FakeDevice);
        impl TaskDevice for Rejecting {
            fn device_id(&self) -> &str {
                self.0.device_id()
            }
            fn platform(&self) -> Platform {
                Platform::Ios
            }
            async fn observe(&self, elements: bool, cancel: CancellationToken) -> Result<Observation> {
                self.0.observe(elements, cancel).await
            }
            async fn ready(&self, _cancel: CancellationToken) -> Result<bool> {
                Ok(true)
            }
            async fn execute(&self, command: Value, cancel: CancellationToken) -> Result<Value> {
                if command["args"]["button"] == "back" {
                    return Err(ApiError::new(400, "Back is supported on Android only"));
                }
                self.0.execute(command, cancel).await
            }
        }
        let f = fixture(replies, vec![], None, false).await;
        let task = TaskLoop::new(
            &f.tasks,
            "session",
            "task",
            Rejecting(f.device.clone()),
            f.planner.clone(),
            vec![],
            vec![],
            f.cancel.clone(),
        );
        assert_eq!(task.run().await.unwrap(), "finished");
    }

    #[tokio::test]
    async fn bridge_follow_ups_carry_only_what_happened_since_the_previous_plan() {
        let replies = vec![
            act("iphone_press_button", json!({"button":"home"}), "Home"),
            act("iphone_launch_app", json!({"app":"com.apple.Preferences"}), "Open Settings"),
            json!({"type":"done","message":"finished"}),
        ];
        let f = fixture(replies, vec![], None, false).await;
        f.task().run().await.unwrap();
        let follow_ups = f.planner.0.follow_ups.lock().unwrap().clone();
        assert_eq!(follow_ups.len(), 3);
        assert_eq!(follow_ups[0].instruction.as_deref(), Some("Play the game"));
        assert!(follow_ups[0].steps.is_empty());
        assert_eq!(follow_ups[1].instruction, None, "the thread already has the goal");
        assert_eq!(follow_ups[1].steps.len(), 1);
        assert_eq!(follow_ups[1].steps[0]["tool"], "iphone_press_button");
        assert_eq!(follow_ups[2].steps.len(), 1);
        assert_eq!(follow_ups[2].steps[0]["tool"], "iphone_launch_app");
    }

    #[tokio::test]
    async fn model_history_never_loses_a_step_between_the_step_log_and_detailed_entries() {
        let count = 15;
        let mut replies: Vec<Value> =
            (0..count).map(|_| act("iphone_press_button", json!({"button":"home"}), "Home again")).collect();
        replies.push(json!({"type":"done","message":"finished"}));
        // Long results make the size cap, not the entry count, decide what history keeps.
        let results = (0..count).map(|_| json!({"kind":"text","text":"x".repeat(5000)})).collect();
        let f = fixture(replies, results, None, false).await;
        f.task().run().await.unwrap();
        for (done, history) in f.planner.0.histories.lock().unwrap().iter().enumerate() {
            let (logged, omitted) =
                history.first().filter(|first| first.get("stepLog").is_some()).map_or((0, 0), |first| {
                    (first["stepLog"].as_array().unwrap().len(), first["omittedSteps"].as_u64().unwrap() as usize)
                });
            let detailed = history.iter().filter(|entry| entry["tool"] == "iphone_press_button").count();
            assert_eq!(logged + omitted + detailed, done, "plan {done}: {history:?}");
        }
    }

    #[tokio::test]
    async fn the_gate_asks_for_a_tap_on_a_payment_control_whatever_the_model_says() {
        let elements = "Button \"Details\" @(100,400) 80x40\nButton \"Pay now\" @(100,200) 80x40";
        let replies = vec![
            act("iphone_tap", json!({"x":100,"y":400}), "Open details"),
            act("iphone_tap", json!({"x":100,"y":200}), "Open details"),
            json!({"type":"done","message":"finished"}),
        ];
        // autoConfirm pre-approves sensitive steps, never payments.
        let f = fixture(replies, vec![], Some(elements), true).await;
        let approve = async {
            loop {
                let snapshot = f.tasks.snapshot().await;
                if snapshot["state"] == "awaiting_confirmation" {
                    assert_eq!(snapshot["confirmation"]["args"], json!({"x":100,"y":200}));
                    assert_eq!(snapshot["steps"].as_array().unwrap().len(), 2, "the first tap ran unasked");
                    let action = snapshot["confirmation"]["actionId"].as_str().unwrap().to_owned();
                    f.tasks.confirm("task", &action, true).await.unwrap();
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        let (result, ()) = tokio::join!(f.task().run(), approve);
        assert_eq!(result.unwrap(), "finished");
        assert_eq!(f.device.0.executed.lock().unwrap().len(), 2);
    }

    #[test]
    fn the_gate_derives_confirmation_from_the_target_and_app_not_only_the_model() {
        let context = |elements: &str, app: Option<&str>| GateContext {
            elements: elements.into(),
            ocr: vec![("确认付款".into(), [10.0, 10.0, 60.0, 30.0])],
            foreground_app: app.map(str::to_owned),
        };
        let class = |action: &Value, context: &GateContext| {
            let request = command_request(action["tool"].as_str().unwrap(), &action["args"]).unwrap();
            gate(action, &request, context)
        };
        let none = context("", None);
        let elements =
            "Cell \"Inbox\" @(200,500) 390x600\nButton \"Delete\" @(200,520) 80x40\nButton \"Get ¥6\" @(300,100) 60x30";
        let screen = context(elements, None);
        // Innocuous model descriptions do not hide what the tapped control is.
        assert_eq!(class(&act("iphone_tap", json!({"x":200,"y":520}), "Open the message"), &screen), Gate::Sensitive);
        assert_eq!(class(&act("iphone_tap", json!({"x":200,"y":700}), "Open the message"), &screen), Gate::None);
        assert_eq!(class(&act("iphone_tap", json!({"x":300,"y":100}), "Open app page"), &screen), Gate::Payment);
        assert_eq!(class(&act("iphone_tap", json!({"x":30,"y":20}), "Continue"), &screen), Gate::Payment, "OCR label");
        assert_eq!(class(&act("iphone_tap_element", json!({"text":"Delete"}), "Continue"), &screen), Gate::Sensitive);
        assert_eq!(class(&act("iphone_tap_element", json!({"text":"Get"}), "Open"), &screen), Gate::Payment);
        // Foreground apps: anything that touches a wallet pays; a submit in a messenger sends.
        let wallet = context("", Some("com.apple.Passbook"));
        assert_eq!(class(&act("iphone_tap", json!({"x":5,"y":5}), "Open card"), &wallet), Gate::Payment);
        let messages = context("", Some("com.apple.MobileSMS"));
        let submit = act("iphone_type_text", json!({"text":"hello","submit":true}), "Type the text");
        assert_eq!(class(&submit, &messages), Gate::Sensitive);
        assert_eq!(class(&submit, &none), Gate::None);
        let settings = context("Button \"Reset All Settings\" @(100,100) 300x40", Some("com.apple.Preferences"));
        assert_eq!(class(&act("iphone_tap", json!({"x":100,"y":100}), "Open"), &settings), Gate::Sensitive);
        // The model's flag only adds confirmation; it cannot lower a derived class.
        let mut flagged = act("iphone_tap", json!({"x":200,"y":700}), "Open");
        flagged["requiresConfirmation"] = json!(true);
        assert_eq!(class(&flagged, &screen), Gate::Sensitive);
        let mut unflagged = act("iphone_tap", json!({"x":300,"y":100}), "Open");
        unflagged["requiresConfirmation"] = json!(false);
        assert_eq!(class(&unflagged, &screen), Gate::Payment);
        let request = command_request("iphone_tap", &json!({"x":300,"y":100})).unwrap();
        assert!(confirmation_required(&unflagged, &request, &screen, true), "payments ask even with autoConfirm");
        let request = command_request("iphone_tap", &json!({"x":200,"y":520})).unwrap();
        assert!(!confirmation_required(&unflagged, &request, &screen, true));
        assert!(confirmation_required(&unflagged, &request, &screen, false));
    }

    #[test]
    fn the_foreground_app_follows_launches_home_and_status_results() {
        let mut context = GateContext::default();
        let launch = command_request("iphone_launch_app", &json!({"app":"com.tencent.xin"})).unwrap();
        context.after_action(&launch, &json!({"kind":"text","text":"Launched"}));
        assert_eq!(context.foreground_app.as_deref(), Some("com.tencent.xin"));
        let home = command_request("iphone_press_button", &json!({"button":"home"})).unwrap();
        context.after_action(&home, &json!({"kind":"text","text":"Pressed home"}));
        assert_eq!(context.foreground_app, None);
        let status = command_request("iphone_status", &json!({})).unwrap();
        context.after_action(
            &status,
            &json!({"kind":"text","text":"WDA ready: yes\nForeground app: com.apple.Passbook\n"}),
        );
        assert_eq!(context.foreground_app.as_deref(), Some("com.apple.Passbook"));
        assert!(app_in(context.foreground_app.as_deref(), &PAYMENT_APPS));
    }

    #[test]
    fn snapshots_keep_a_bounded_tail_of_steps_with_their_task_numbers() {
        let mut state = TaskState::new("t".into(), "goal", None, vec![], false);
        for _ in 0..SNAPSHOT_STEPS + 50 {
            state.push_step(StepView {
                id: "s".into(),
                number: 0,
                tool: json!("iphone_tap"),
                args: json!({}),
                reason: json!("r"),
                state: StepState::Succeeded,
                started_at: now(),
                finished_at: None,
                result: None,
                planned: false,
                timings: None,
            });
        }
        let value = state.to_value();
        assert_eq!(value["steps"].as_array().unwrap().len(), SNAPSHOT_STEPS);
        assert_eq!(value["totalSteps"], SNAPSHOT_STEPS + 50);
        assert_eq!(value["steps"][0]["number"], 51);
        assert!(value["steps"][0].get("timings").is_none());
    }

    #[test]
    fn the_typed_snapshot_keeps_the_task_wire_format() {
        let value = TaskState::new("id".into(), "goal", Some("model".into()), vec!["a.png".into()], true).to_value();
        let mut keys: Vec<_> = value.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "attachments",
                "autoConfirm",
                "confirmation",
                "discardedPlanActions",
                "error",
                "finishedAt",
                "id",
                "instruction",
                "instructionUpdates",
                "message",
                "model",
                "observationId",
                "plan",
                "startedAt",
                "state",
                "steps",
                "timings",
                "totalSteps"
            ]
        );
        assert_eq!(value["state"], "observing");
        assert_eq!(value["timings"], json!({}));
        assert_eq!(value["confirmation"], Value::Null);
        assert_eq!(value["attachments"], json!(["a.png"]));
    }
}
