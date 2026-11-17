mod attachments;
mod connection;
mod icons;
mod preview;
mod progress;
mod setup;
mod task;

use crate::{BrowserClient, actions::create_task_actions, js::*, polling::start_session_polling};
use icons::Icon;
use js_sys::Promise;
use leptos::prelude::*;
use serde_json::{Value, json as value};
use wasm_bindgen::{JsCast, prelude::*};
use wasm_bindgen_futures::{future_to_promise, spawn_local};

#[derive(Clone, Copy)]
pub(super) struct Ui {
    client: StoredValue<BrowserClient, LocalStorage>,
    actions: StoredValue<JsValue, LocalStorage>,
    pub state: RwSignal<Value>,
    /// `state.session` and `session.task`, memoised so reactive reads borrow
    /// (`.with`/`.read`) instead of deep-cloning the whole client state every poll.
    pub session: Memo<Value>,
    pub task: Memo<Value>,
    pub ai: RwSignal<Value>,
    /// Latest screenshot metadata (timings, deviceId, ...), without the image bytes.
    pub screen: RwSignal<Value>,
    /// Image source for `screen`: an object URL for `screen.jpg`, or a data URL
    /// fallback for gateways without that route. Empty until the image is ready.
    pub screen_src: RwSignal<String>,
    /// Discards image loads that finish after a newer screen or a session change.
    screen_generation: StoredValue<u64, LocalStorage>,
    /// Session whose gateway has no `screen.jpg` route; skip straight to base64.
    screen_jpg_unsupported: StoredValue<String, LocalStorage>,
    /// Opt-in continuous MJPEG preview URL; empty while showing single screenshots.
    pub preview_url: RwSignal<String>,
    pub base: RwSignal<String>,
    pub error: RwSignal<String>,
    pub config_error: RwSignal<String>,
    pub config_revision: RwSignal<u64>,
    pub busy: RwSignal<bool>,
    pub stop_key: RwSignal<String>,
}
/// Release an object URL created for a screenshot; data URLs need no cleanup.
fn revoke(source: &str) {
    if source.starts_with("blob:") {
        let _ = call(&get(&global(), "URL"), "revokeObjectURL", &[source.into()]);
    }
}
fn value_from_js(value: JsValue) -> Value {
    serde_wasm_bindgen::from_value(value).unwrap_or(Value::Null)
}
pub(super) fn string(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_string()
}
impl Ui {
    pub fn running(self) -> bool {
        self.task.with(|task| {
            !task.is_null() && !["completed", "failed", "stopped"].contains(&task["state"].as_str().unwrap_or_default())
        })
    }
    pub fn ready(self) -> bool {
        self.session.with(|session| session["ready"] == true && session["daemon"]["state"]["state"] == "ready")
    }
    pub fn android(self) -> bool {
        self.session.with(|s| s["platform"] == "android") || self.state.with(|s| s["device"]["platform"] == "android")
    }
    pub fn can_control(self) -> bool {
        self.ready() && !self.running() && self.state.with(|s| s["phase"] == "connected")
    }
    pub fn occupied(self) -> bool {
        !self.session.with(Value::is_null)
            || self
                .state
                .with(|s| ["selecting", "pairing", "connected"].contains(&s["phase"].as_str().unwrap_or_default()))
    }
    pub fn status(self) -> String {
        let (error, phase, connected, native_ready) = self.state.with(|state| {
            let session = &state["session"];
            (
                !state["error"].is_null(),
                string(state, "phase"),
                (!session.is_null()).then(|| session["state"] == "connected"),
                session["ready"] == true,
            )
        });
        if error {
            return "连接失败".into();
        }
        match phase.as_str() {
            "selecting" => "等待 USB 授权".into(),
            "pairing" => "正在配对与准备".into(),
            _ if connected.is_none() => "未连接设备".into(),
            _ if connected == Some(false) => "设备离线".into(),
            _ if !native_ready => "原生服务启动中".into(),
            _ if !self.ready() => if self.android() { "Android 控制准备中" } else { "WDA 准备中" }.into(),
            _ => "可以控制".into(),
        }
    }
    pub fn session_id(self) -> String {
        self.session.with_untracked(|s| string(s, "id"))
    }
    pub fn run(self, operation: impl FnOnce(BrowserClient) -> Promise + 'static) {
        let mut operation = Some(operation);
        let action = callback(move |_| {
            operation.take().map(|operation| operation(self.client.get_value()).into()).unwrap_or(JsValue::UNDEFINED)
        });
        let _ = call(&self.actions.get_value(), "run", &[action]);
    }
    fn current_session(self, session_id: &str) -> bool {
        !session_id.is_empty() && self.state.with_untracked(|s| s["session"]["id"] == session_id)
    }
    /// Drop the shown screenshot and any image load still in flight.
    pub fn clear_screen(self) {
        self.screen_generation.update_value(|g| *g += 1);
        self.screen.set(Value::Null);
        revoke(&self.screen_src.get_untracked());
        self.screen_src.set(String::new());
    }
    pub fn accept_screen(self, session_id: &str, screen: JsValue) {
        if !self.current_session(session_id) || screen.is_null() || screen.is_undefined() {
            return;
        }
        // Keep the base64 payload as a JS handle: it is only copied into WASM for
        // the fallback data URL. The displayed image comes from `screen.jpg`.
        let base64 = get(&screen, "imageBase64");
        let metadata = copy(&screen);
        let _ = js_sys::Reflect::delete_property(metadata.unchecked_ref::<js_sys::Object>(), &"imageBase64".into());
        let image = value_from_js(metadata);
        if image.is_null() || image["deviceId"] != self.session.with_untracked(|s| s["deviceId"].clone()) {
            return;
        }
        self.screen_generation.update_value(|g| *g += 1);
        let generation = self.screen_generation.get_value();
        let session_id = session_id.to_string();
        let blob =
            (self.screen_jpg_unsupported.get_value() != session_id).then(|| self.client.get_value().screen_image());
        spawn_local(async move {
            let source = match blob {
                Some(pending) => match resolved(pending.into()).await {
                    Ok(url) => url.as_string(),
                    Err(e) => {
                        // Older gateways: remember for this session and use the JSON image.
                        if truth(&get(&e, "unsupported")) {
                            self.screen_jpg_unsupported.set_value(session_id.clone());
                        }
                        None
                    }
                },
                None => None,
            };
            let source =
                source.unwrap_or_else(|| format!("data:image/jpeg;base64,{}", base64.as_string().unwrap_or_default()));
            if self.screen_generation.get_value() != generation || !self.current_session(&session_id) {
                revoke(&source);
                return;
            }
            revoke(&self.screen_src.get_untracked());
            self.screen.set(image);
            self.screen_src.set(source);
        });
    }
    pub fn manual(self, tool: String, args: Value) {
        if !self.can_control() || self.busy.get_untracked() {
            return;
        }
        let session_id = self.session_id();
        self.run(move |client| {
            if !self.current_session(&session_id) {
                return Promise::resolve(&JsValue::UNDEFINED);
            }
            let pending = client.command(tool, json(args));
            future_to_promise(async move {
                let result = resolved(pending.into()).await?;
                if !self.current_session(&session_id) {
                    return Ok(JsValue::UNDEFINED);
                }
                if text(&result, "kind") == "error" {
                    return Err(error(if text(&result, "message").is_empty() {
                        "设备操作失败。已经发出的操作不会自动重试。".into()
                    } else {
                        text(&result, "message")
                    }));
                }
                self.accept_screen(&session_id, resolved(client.screen().into()).await?);
                Ok(JsValue::UNDEFINED)
            })
        });
    }
    pub fn refresh_screen(self) {
        let session_id = self.session_id();
        self.run(move |client| {
            if !self.current_session(&session_id) {
                return Promise::resolve(&JsValue::UNDEFINED);
            }
            let pending = client.screen();
            future_to_promise(async move {
                self.accept_screen(&session_id, resolved(pending.into()).await?);
                Ok(JsValue::UNDEFINED)
            })
        });
    }
    pub fn disconnect(self) {
        self.clear_screen();
        self.error.set(String::new());
        let _ = self.client.get_value().disconnect();
    }
    pub fn connect(self, token: String, platform: String) {
        self.error.set(String::new());
        self.clear_screen();
        // Dispatch the chooser within the click event, before spawning any future.
        let pending = self
            .client
            .get_value()
            .connect(json(value!({"apiBase":self.base.get_untracked(),"accessToken":token,"platform":platform})));
        spawn_local(async move {
            if let Err(e) = resolved(pending.into()).await {
                self.error.set(message(&e));
            }
        });
    }
    pub fn stop(self) {
        let (session, task) = self.session.with_untracked(|s| (s["id"].clone(), s["task"]["id"].clone()));
        let _ = call(&self.actions.get_value(), "stop", &[json(session), json(task)]);
    }
    pub fn steer(self, instruction: String) {
        let task = self.task.get_untracked();
        if task.is_null() || !self.running() {
            return;
        }
        self.run(move |client| client.steer_task(string(&task, "id"), instruction));
    }
    pub fn confirm(self, approved: bool) {
        let task = self.task.get_untracked();
        if task["confirmation"].is_null() {
            return;
        }
        self.run(move |client| {
            client.confirm_task(string(&task, "id"), string(&task["confirmation"], "actionId"), approved)
        });
    }
}

#[wasm_bindgen]
pub fn mount(options: JsValue) -> Result<(), JsValue> {
    let root = web_sys::window()
        .ok_or_else(|| error("Browser window unavailable"))?
        .document()
        .ok_or_else(|| error("Document unavailable"))?
        .get_element_by_id("root")
        .ok_or_else(|| error("Missing #root"))?
        .dyn_into::<web_sys::HtmlElement>()?;
    leptos::mount::mount_to(root, move || view! { <App options=options.clone() /> }).forget();
    Ok(())
}

#[component]
fn App(options: JsValue) -> impl IntoView {
    let client = BrowserClient::new(options);
    let state = RwSignal::new(value_from_js(client.snapshot()));
    let session = Memo::new(move |_| state.with(|s| s["session"].clone()));
    let ui = Ui {
        client: StoredValue::new_local(client.clone()),
        actions: StoredValue::new_local(JsValue::NULL),
        state,
        session,
        task: Memo::new(move |_| session.with(|s| s["task"].clone())),
        ai: RwSignal::new(Value::Null),
        screen: RwSignal::new(Value::Null),
        screen_src: RwSignal::new(String::new()),
        screen_generation: StoredValue::new_local(0),
        screen_jpg_unsupported: StoredValue::new_local(String::new()),
        preview_url: RwSignal::new(String::new()),
        base: RwSignal::new(String::new()),
        error: RwSignal::new(String::new()),
        config_error: RwSignal::new(String::new()),
        config_revision: RwSignal::new(0),
        busy: RwSignal::new(false),
        stop_key: RwSignal::new(String::new()),
    };
    let subscription_client = client.clone();
    let _unsubscribe = client.subscribe(callback(move |_| {
        state.set(value_from_js(subscription_client.snapshot()));
        JsValue::UNDEFINED
    }));
    let actions = create_task_actions(
        client.clone().into(),
        obj(&[
            (
                "onBusy",
                callback(move |v| {
                    ui.busy.set(truth(&v));
                    JsValue::UNDEFINED
                }),
            ),
            (
                "onError",
                callback(move |v| {
                    ui.error.set(v.as_string().unwrap_or_default());
                    JsValue::UNDEFINED
                }),
            ),
            (
                "onStopBusy",
                callback(move |v| {
                    ui.stop_key.set(v.as_string().unwrap_or_default());
                    JsValue::UNDEFINED
                }),
            ),
        ]),
    );
    ui.actions.set_value(actions);
    let config_epoch = RwSignal::new(0u64);
    Effect::new(move |_| {
        let base = ui.base.get();
        let _ = ui.config_revision.get();
        let epoch = config_epoch.get_untracked() + 1;
        config_epoch.set(epoch);
        ui.ai.set(Value::Null);
        ui.config_error.set(String::new());
        spawn_local(async move {
            let _ = resolved(timeout(350.0).into()).await;
            if config_epoch.get_untracked() != epoch {
                return;
            }
            let result = resolved(ui.client.get_value().config(Some(base)).into()).await;
            if config_epoch.get_untracked() != epoch {
                return;
            }
            match result {
                Ok(value) => ui.ai.set(value_from_js(value)),
                Err(_) => ui.config_error.set("无法读取服务器配置，请检查地址和服务状态。".into()),
            }
        });
    });
    let session_id = Memo::new(move |_| ui.session.with(|s| string(s, "id")));
    // The continuous preview belongs to one session and is never restarted automatically.
    Effect::new(move |_| {
        let _ = session_id.get();
        ui.preview_url.set(String::new());
    });
    // Show every task observation as the model receives it. While a task owns the
    // device, `/preview` returns the gateway's cached observation without a capture.
    // When the task ends, one on-demand frame shows the final result.
    let task_frame = Memo::new(move |_| {
        let ended = !ui.task.with(Value::is_null) && !ui.running();
        ui.task.with(|task| {
            let observation = string(task, "observationId");
            let captured = ended || (!observation.is_empty() && task["state"] != "observing");
            (session_id.get(), string(task, "id"), observation, ended, captured)
        })
    });
    Effect::new(move |_| {
        let (session, task, _, _, captured) = task_frame.get();
        // Read device state untracked: only a new observation (or the task end) captures.
        if session.is_empty() || task.is_empty() || !captured || untrack(|| ui.android() || !ui.ready()) {
            return;
        }
        let pending = ui.client.get_value().screen();
        spawn_local(async move {
            if let Ok(image) = resolved(pending.into()).await {
                ui.accept_screen(&session, image);
            }
        });
    });
    let poll_stop = StoredValue::new_local(JsValue::NULL);
    Effect::new(move |_| {
        let id = session_id.get();
        ui.busy.set(false); // An old session's pending capture cannot block this one.
        let previous = poll_stop.get_value();
        if previous.is_function() {
            let _ = apply(&previous, &JsValue::UNDEFINED, &[]);
        }
        poll_stop.set_value(JsValue::NULL);
        if id.is_empty() {
            ui.clear_screen();
            return;
        }
        let origin_session = id;
        let stop = start_session_polling(
            ui.client.get_value().into(),
            obj(&[
                // iPhone captures are explicit or task-driven; idle status polling
                // must not create MJPEG/screenshot traffic on the shared USB link.
                ("screenshots", ui.session.with_untracked(|s| s["platform"] == "android").into()),
                (
                    "onScreen",
                    callback(move |image| {
                        if image.is_null() {
                            ui.clear_screen();
                        } else {
                            ui.accept_screen(&origin_session, image);
                        }
                        JsValue::UNDEFINED
                    }),
                ),
                (
                    "onError",
                    callback(move |e| {
                        ui.error.set(e.as_string().unwrap_or_default());
                        JsValue::UNDEFINED
                    }),
                ),
            ]),
        );
        poll_stop.set_value(stop);
    });
    let close = callback(move |_| {
        ui.disconnect();
        let stop = poll_stop.get_value();
        if stop.is_function() {
            let _ = apply(&stop, &JsValue::UNDEFINED, &[]);
        }
        JsValue::UNDEFINED
    });
    let _ = call(&global(), "addEventListener", &["pagehide".into(), close]);
    view! {
        <div class="app-shell">
            <a class="skip-link" href="#workspace">"跳到工作台"</a>
            <header class="topbar">
                <div class="topbar-brand">
                    <a class="brand" href="/" aria-label="iPhone Control 首页">
                        <span class="brand-mark"><Icon name="phone" /></span>
                        "iPhone Control"
                    </a>
                    <span class="brand-separator" aria-hidden="true" />
                    <h1>"设备工作台"</h1>
                </div>
                <div class="topbar-actions">
                    <span class="server-summary">
                        {move || if ui.base.get().is_empty() { "当前网站服务器" } else { "自定义服务器" }}
                    </span>
                    <button class="connection-switch"
                        aria-label=move || if ui.occupied() { "连接设置" } else { "连接设备" }
                        on:click=move |_| connection::open_connection_settings()>
                        <span class=move || format!("connection-status {}", if ui.ready() { "ready" } else { "" }) role="status">
                            <span class="tiny-dot" aria-hidden="true" />
                            {move || ui.status()}
                        </span>
                        <span class="connection-switch-label">{move || if ui.occupied() { "设备设置" } else { "连接设备" }}</span>
                        <Icon name="settings" />
                    </button>
                </div>
            </header>
            <connection::ConnectionDialog ui />
            <main class="workspace" id="workspace" tabindex="-1">
                <div class="workspace-content">
                    <Show when=move || {
                        !ui.state.with(|s| string(s, "error")).is_empty() || !ui.error.get().is_empty()
                            || !ui.config_error.get().is_empty()
                    }>
                        <div class="error-banner" role="alert">
                            <Icon name="info" />
                            <div>
                                <strong>
                                    {move || {
                                        let error = ui.state.with(|s| string(s, "error"));
                                        if !error.is_empty() {
                                            error
                                        } else if !ui.error.get().is_empty() {
                                            ui.error.get()
                                        } else {
                                            ui.config_error.get()
                                        }
                                    }}
                                </strong>
                                <Show when=move || {
                                    let e = format!(
                                        "{} {}",
                                        ui.state.with(|s| string(s, "error")),
                                        ui.error.get(),
                                    )
                                        .to_lowercase();
                                    e.contains("claim") || e.contains("exclusive usb")
                                }>
                                    <p>
                                        "请保持手机解锁，关闭占用设备的其他浏览器标签页，然后重新连接。USB 接口访问成功后，请按对应系统完成手机授权。"
                                    </p>
                                </Show>
                            </div>
                            <button
                                class="icon-button"
                                aria-label="清除操作提示"
                                on:click=move |_| {
                                    ui.error.set(String::new());
                                    ui.config_error.set(String::new());
                                }
                            >
                                <Icon name="close" />
                            </button>
                        </div>
                    </Show>
                    <div class="console-grid">
                        <div class="workflow-column">
                            <progress::TaskProgress ui />
                            <task::TaskPanel ui />
                        </div>
                        <preview::PhonePreview ui />
                    </div>
                    <Show when=move || {
                        !ui.session.with(Value::is_null) && !ui.android()
                            && (!ui.ready() || ui.session.with(|s| !string(&s["setup"], "transcript").is_empty()))
                    }>
                        <div class="setup-area"><setup::SetupPanel ui /></div>
                    </Show>
                    <footer class="workspace-footer">
                        <span class="workflow-note">
                            <Icon name="shield" />
                            {move || if ui.running() {
                                "任务执行中，手动操作已暂停。你可以随时停止。"
                            } else {
                                "每一步都来自真实设备。连接后即可开始。"
                            }}
                        </span>
                        <span>"关闭页面将断开设备连接"</span>
                    </footer>
                </div>
            </main>
        </div>
    }
}
