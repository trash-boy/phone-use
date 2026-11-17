use crate::{
    identity::{android_uid, canonical_uid},
    js::*,
};
use js_sys::{Array, Promise};
use std::{cell::RefCell, collections::HashSet, rc::Rc};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::future_to_promise;
/// The synchronous result of calling `fetch`: the response promise or a thrown error.
type PendingFetch = Result<JsValue, JsValue>;
#[derive(Default)]
struct Attempt {
    id: u64,
    base: String,
    access_token: String,
    platform: String,
    sdk: JsValue,
    cancelled: bool,
    target: JsValue,
    physical_device: JsValue,
    session: JsValue,
    relay: JsValue,
    log: JsValue,
    detach: JsValue,
    diagnostics: JsValue,
}
struct Inner {
    sdk: JsValue,
    android_sdk: JsValue,
    fetch: JsValue,
    uuid: JsValue,
    usb_events: JsValue,
    cleanup_timeout: f64,
    view: JsValue,
    listeners: Vec<JsValue>,
    attempt: Option<Rc<RefCell<Attempt>>>,
    custody: HashSet<u64>,
    next_id: u64,
    refresh_sequence: u64,
}
#[wasm_bindgen]
#[derive(Clone)]
pub struct BrowserClient {
    inner: Rc<RefCell<Inner>>,
}
#[wasm_bindgen]
impl BrowserClient {
    #[wasm_bindgen(constructor)]
    pub fn new(options: JsValue) -> Self {
        let fetch = get(&options, "fetchImpl");
        let uuid = get(&options, "uuid");
        let events = get(&options, "usbEvents");
        Self {
            inner: Rc::new(RefCell::new(Inner {
                sdk: get(&options, "sdk"),
                android_sdk: get(&options, "androidSdk"),
                fetch: if fetch.is_undefined() { get(&global(), "fetch") } else { fetch },
                uuid,
                usb_events: if events.is_undefined() { get(&get(&global(), "navigator"), "usb") } else { events },
                cleanup_timeout: get(&options, "cleanupTimeoutMs").as_f64().unwrap_or(5000.0),
                view: json(
                    serde_json::json!({"phase":"offline","device":null,"session":null,"error":null,"warning":null,"transportStatus":"","cleanupPending":false}),
                ),
                listeners: vec![],
                attempt: None,
                custody: HashSet::new(),
                next_id: 0,
                refresh_sequence: 0,
            })),
        }
    }
    pub fn snapshot(&self) -> JsValue {
        self.inner.borrow().view.clone()
    }
    pub fn subscribe(&self, listener: JsValue) -> JsValue {
        self.inner.borrow_mut().listeners.push(listener.clone());
        let weak = Rc::downgrade(&self.inner);
        callback(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.borrow_mut().listeners.retain(|v| v != &listener);
            }
            JsValue::UNDEFINED
        })
    }
    pub fn update(&self, patch: JsValue) {
        let listeners = {
            let mut inner = self.inner.borrow_mut();
            let view = copy(&inner.view);
            let keys = js_sys::Object::keys(&js_sys::Object::from(patch.clone()));
            for key in keys.iter() {
                if let Some(key) = key.as_string() {
                    set(&view, &key, &get(&patch, &key));
                }
            }
            inner.view = view;
            inner.listeners.clone()
        };
        for listener in listeners {
            let _ = apply(&listener, &JsValue::UNDEFINED, &[]);
        }
    }
    #[wasm_bindgen(getter,js_name=fetchImpl)]
    pub fn fetch_impl(&self) -> JsValue {
        self.inner.borrow().fetch.clone()
    }
    #[wasm_bindgen(setter,js_name=fetchImpl)]
    pub fn set_fetch_impl(&self, value: JsValue) {
        self.inner.borrow_mut().fetch = value;
    }
    pub fn config(&self, api_base: Option<String>) -> Promise {
        let this = self.clone();
        future_to_promise(async move {
            this.request(
                api_base.unwrap_or_default().trim_end_matches('/'),
                "/api/config",
                "",
                "GET",
                JsValue::UNDEFINED,
                JsValue::UNDEFINED,
            )
            .await
        })
    }
    pub fn connect(&self, options: JsValue) -> Promise {
        let result = (|| -> Result<(Rc<RefCell<Attempt>>, JsValue), JsValue> {
            let platform = get(&options, "platform").as_string().unwrap_or("ios".into());
            if platform != "ios" && platform != "android" {
                return Err(error("不支持的设备系统"));
            }
            {
                let inner = self.inner.borrow();
                if inner.attempt.is_some() {
                    return Err(error("连接仍在处理中，请先断开"));
                }
                if !inner.custody.is_empty() {
                    return Err(error("上次 USB 配对或释放仍在处理中，请等待结束后再连接"));
                }
            }
            let mut base = text(&options, "apiBase").trim().trim_end_matches('/').to_string();
            if !base.is_empty() {
                let url = ctor("URL", &[base.clone().into()]).map_err(|_| error("请输入有效的服务器地址"))?;
                if !["http:", "https:"].contains(&text(&url, "protocol").as_str())
                    || !["username", "password", "search", "hash"].iter().all(|key| text(&url, key).is_empty())
                    || text(&url, "pathname") != "/"
                {
                    return Err(error("服务器地址必须为 HTTP(S) origin，不含账号或路径"));
                }
                base = text(&url, "origin");
            }
            let access_token = text(&options, "accessToken");
            if access_token.trim().is_empty() {
                return Err(error("请输入服务器访问令牌"));
            }
            let attempt = {
                let mut inner = self.inner.borrow_mut();
                inner.next_id += 1;
                let sdk = if platform == "android" { inner.android_sdk.clone() } else { inner.sdk.clone() };
                Rc::new(RefCell::new(Attempt {
                    id: inner.next_id,
                    base,
                    access_token,
                    platform,
                    sdk,
                    ..Default::default()
                }))
            };
            self.inner.borrow_mut().attempt = Some(attempt.clone());
            let weak = Rc::downgrade(&self.inner);
            let attempt_weak = Rc::downgrade(&attempt);
            let log = callback(move |value| {
                if let (Some(inner), Some(attempt)) = (weak.upgrade(), attempt_weak.upgrade()) {
                    let this = BrowserClient { inner };
                    if attempt.borrow().cancelled {
                        return JsValue::UNDEFINED;
                    }
                    let value = value.as_string().unwrap_or_default();
                    if let Some(label) = progress_label(&value) {
                        this.update(obj(&[("transportStatus", label.into())]));
                    }
                    if value == "Relay socket closed" {
                        this.connection_lost(
                            attempt,
                            "error",
                            "设备连接已断开。请重新连接；已经发出的操作可能已执行。",
                        );
                    }
                }
                JsValue::UNDEFINED
            });
            attempt.borrow_mut().log = log.clone();
            // The chooser is invoked before scheduling the asynchronous connection future.
            let chooser = call(&attempt.borrow().sdk, "requestUSBAccess", &[obj(&[("log", log)])])
                .unwrap_or_else(|e| Promise::reject(&e).into());
            self.update(json(
                serde_json::json!({"phase":"selecting","error":null,"warning":null,"device":null,"session":null}),
            ));
            Ok((attempt, chooser))
        })();
        match result {
            Ok((attempt, chooser)) => {
                let this = self.clone();
                future_to_promise(async move { this.complete_connection(attempt, chooser).await })
            }
            Err(e) => Promise::reject(&e),
        }
    }
    pub fn disconnect(&self) -> Promise {
        let attempt = self.inner.borrow_mut().attempt.take();
        if let Some(a) = &attempt {
            a.borrow_mut().cancelled = true;
        }
        self.inner.borrow_mut().refresh_sequence += 1;
        self.update(json(serde_json::json!({"phase":"offline","session":null,"device":null,"error":null,"warning":null,"transportStatus":""})));
        self.publish_custody();
        if let Some(attempt) = attempt { self.cleanup(attempt) } else { Promise::resolve(&JsValue::UNDEFINED) }
    }
    pub fn refresh(&self) -> Promise {
        let attempt = self.inner.borrow().attempt.clone();
        let sequence = {
            let mut inner = self.inner.borrow_mut();
            inner.refresh_sequence += 1;
            inner.refresh_sequence
        };
        let request = self.session_promise("", "GET", JsValue::UNDEFINED);
        let this = self.clone();
        future_to_promise(async move {
            let result = resolved(request.into()).await;
            if let Some(a) = attempt {
                let current = this.current(&a) && sequence == this.inner.borrow().refresh_sequence;
                if current {
                    match &result {
                        Ok(value) => this.update(obj(&[("session", value.clone())])),
                        Err(e) => {
                            let status = number(e, "status");
                            if status == 401.0 || status == 404.0 {
                                this.connection_lost(
                                    a,
                                    "error",
                                    "设备连接已断开。请重新连接；已经发出的操作可能已执行。",
                                );
                            } else {
                                let session = get(&this.snapshot(), "session");
                                if truth(&session) {
                                    let session = copy(&session);
                                    set(&session, "ready", &false.into());
                                    set(&session, "state", &"unavailable".into());
                                    this.update(obj(&[("session", session)]));
                                }
                            }
                        }
                    }
                }
            }
            result
        })
    }
    pub fn screen(&self) -> Promise {
        self.session_promise("/preview", "GET", JsValue::UNDEFINED)
    }
    /// Fetch the latest captured screen as JPEG bytes and return an object URL
    /// (the caller revokes it). Rejects with `status` set when the gateway has no
    /// `screen.jpg` route (older gateways) or the response is not an image.
    #[wasm_bindgen(js_name=screenImage)]
    pub fn screen_image(&self) -> Promise {
        let (_, response) = match self.session_fetch("/screen.jpg", "GET", JsValue::UNDEFINED) {
            Ok(started) => started,
            Err(e) => return Promise::reject(&e),
        };
        future_to_promise(async move {
            let response = resolved(response?).await?;
            let status = get(&response, "status");
            let kind = call(&get(&response, "headers"), "get", &["Content-Type".into()])
                .ok()
                .and_then(|v| v.as_string())
                .unwrap_or_default();
            if !truth(&get(&response, "ok")) || !kind.to_ascii_lowercase().starts_with("image/") {
                let e = error(format!("屏幕图片不可用（{}）", status.as_f64().unwrap_or(0.0)));
                set(&e, "status", &status);
                set(&e, "unsupported", &(truth(&get(&response, "ok")) || number(&response, "status") == 404.0).into());
                return Err(e);
            }
            let blob = invoke(&response, "blob", &[]).await?;
            call(&get(&global(), "URL"), "createObjectURL", &[blob])
        })
    }
    #[wasm_bindgen(js_name=stopPreview)]
    pub fn stop_preview(&self) -> Promise {
        let attempt = self.inner.borrow().attempt.clone();
        let pending = self.session_promise("/preview/stream", "DELETE", JsValue::UNDEFINED);
        let this = self.clone();
        future_to_promise(async move {
            let result = resolved(pending.into()).await?;
            if let Some(attempt) = attempt.filter(|a| this.current(a)) {
                let relay = attempt.borrow().relay.clone();
                set(&relay, "continuousPreviewEnabled", &false.into());
                set(&relay, "previewSuspendedUntil", &0.into());
            }
            Ok(result)
        })
    }
    #[wasm_bindgen(js_name=previewStream)]
    pub fn preview_stream(&self) -> Promise {
        let Some(attempt) = self.inner.borrow().attempt.clone() else {
            return Promise::reject(&error("请先连接手机"));
        };
        let base = attempt.borrow().base.clone();
        let id = text(&attempt.borrow().session, "id");
        let pending = self.session_promise("/preview/stream", "GET", JsValue::UNDEFINED);
        let this = self.clone();
        future_to_promise(async move {
            let descriptor = resolved(pending.into()).await?;
            if !this.current(&attempt) {
                return Err(error("会话已切换"));
            }
            let route = text(&descriptor, "url");
            if !route.starts_with(&format!("/api/sessions/{id}/preview/mjpeg/")) {
                return Err(error("无效的预览地址"));
            }
            set(&attempt.borrow().relay, "continuousPreviewEnabled", &true.into());
            Ok(format!("{base}{route}").into())
        })
    }
    pub fn command(&self, tool: String, args: JsValue) -> Promise {
        let request_id = {
            let function = self.inner.borrow().uuid.clone();
            if function.is_function() {
                apply(&function, &JsValue::UNDEFINED, &[]).unwrap_or_default()
            } else {
                uuid().into()
            }
        };
        let device_id =
            self.inner.borrow().attempt.as_ref().map(|a| get(&a.borrow().session, "deviceId")).unwrap_or_default();
        self.session_promise(
            "/command",
            "POST",
            obj(&[("requestId", request_id), ("deviceId", device_id), ("tool", tool.into()), ("args", args)]),
        )
    }
    #[wasm_bindgen(js_name=startSetup)]
    pub fn start_setup(&self) -> Promise {
        self.session_promise("/setup", "POST", object())
    }
    #[wasm_bindgen(js_name=setupInput)]
    pub fn setup_input(&self, line: String) -> Promise {
        self.session_promise("/setup/input", "POST", obj(&[("line", line.into())]))
    }
    #[wasm_bindgen(js_name=startTask)]
    pub fn start_task(
        &self,
        instruction: String,
        model: Option<String>,
        attachments: JsValue,
        auto_confirm: Option<bool>,
    ) -> Promise {
        let data = obj(&[("instruction", instruction.into())]);
        if auto_confirm == Some(true) {
            set(&data, "autoConfirm", &JsValue::TRUE);
        }
        if let Some(model) = model.filter(|m| !m.is_empty()) {
            set(&data, "model", &model.into());
        }
        if !attachments.is_null() && !attachments.is_undefined() {
            set(&data, "attachments", &attachments);
        }
        self.session_promise("/tasks", "POST", data)
    }
    #[wasm_bindgen(js_name=uploadAttachment)]
    pub fn upload_attachment(&self, name: String, mime_type: String, data_base64: String) -> Promise {
        self.session_promise(
            "/attachments",
            "POST",
            obj(&[("name", name.into()), ("mimeType", mime_type.into()), ("dataBase64", data_base64.into())]),
        )
    }
    #[wasm_bindgen(js_name=deleteAttachment)]
    pub fn delete_attachment(&self, id: String) -> Promise {
        self.session_promise(
            &format!("/attachments/{}", js_sys::encode_uri_component(&id)),
            "DELETE",
            JsValue::UNDEFINED,
        )
    }
    #[wasm_bindgen(js_name=confirmTask)]
    pub fn confirm_task(&self, id: String, action_id: String, approved: bool) -> Promise {
        self.session_promise(
            &format!("/tasks/{}/confirm", js_sys::encode_uri_component(&id)),
            "POST",
            obj(&[("actionId", action_id.into()), ("approved", approved.into())]),
        )
    }
    #[wasm_bindgen(js_name=steerTask)]
    pub fn steer_task(&self, id: String, instruction: String) -> Promise {
        self.session_promise(
            &format!("/tasks/{}/instruction", js_sys::encode_uri_component(&id)),
            "POST",
            obj(&[("instruction", instruction.into())]),
        )
    }
    #[wasm_bindgen(js_name=stopTask)]
    pub fn stop_task(&self, id: String) -> Promise {
        let attempt = self.inner.borrow().attempt.clone();
        self.inner.borrow_mut().refresh_sequence += 1;
        let request =
            self.session_promise(&format!("/tasks/{}/stop", js_sys::encode_uri_component(&id)), "POST", object());
        let this = self.clone();
        future_to_promise(async move {
            let value = resolved(request.into()).await?;
            if let Some(a) = attempt {
                let session = get(&this.snapshot(), "session");
                if this.current(&a) && text(&get(&session, "task"), "id") == id {
                    this.inner.borrow_mut().refresh_sequence += 1;
                    let next = copy(&session);
                    set(&next, "task", &value);
                    this.update(obj(&[("session", next)]));
                }
            }
            Ok(value)
        })
    }
}
impl BrowserClient {
    fn current(&self, a: &Rc<RefCell<Attempt>>) -> bool {
        !a.borrow().cancelled && self.inner.borrow().attempt.as_ref().is_some_and(|b| Rc::ptr_eq(a, b))
    }
    fn publish_custody(&self) {
        let pending = {
            let i = self.inner.borrow();
            !i.custody.is_empty() && i.attempt.is_none()
        };
        self.update(obj(&[
            ("cleanupPending", pending.into()),
            (
                "warning",
                if pending {
                    "上次 USB 配对或释放仍在处理中。请等待；长时间未结束时，可刷新页面后重试。".into()
                } else {
                    JsValue::NULL
                },
            ),
        ]));
    }
    fn hold(&self, id: u64) {
        self.inner.borrow_mut().custody.insert(id);
        self.publish_custody();
    }
    fn release(&self, id: u64) {
        if self.inner.borrow_mut().custody.remove(&id) {
            self.publish_custody();
        }
    }
    fn safe_error(&self, value: &JsValue, attempt: &Rc<RefCell<Attempt>>) -> String {
        let a = attempt.borrow();
        let mut safe_message = message(value);
        if safe_message == "Native signing state has an active owner" {
            safe_message =
                "设备已被另一个会话占用。请回到原工作台点击“断开连接”，或关闭重复页面，等待释放后重连。".into();
        }
        for secret in [&a.access_token, &text(&a.session, "token")] {
            if !secret.is_empty() {
                safe_message = safe_message.replace(secret, "[已隐藏]");
            }
        }
        safe_message.chars().take(1600).collect()
    }
    async fn request(
        &self,
        base: &str,
        route: &str,
        token: &str,
        method: &str,
        data: JsValue,
        signal: JsValue,
    ) -> Result<JsValue, JsValue> {
        let headers = object();
        if !token.is_empty() {
            set(&headers, "Authorization", &format!("Bearer {token}").into());
        }
        let options = obj(&[
            ("method", method.into()),
            ("headers", headers.clone()),
            ("cache", "no-store".into()),
            ("redirect", "error".into()),
        ]);
        if !data.is_undefined() {
            set(&headers, "Content-Type", &"application/json".into());
            set(&options, "body", &js_sys::JSON::stringify(&data)?.into());
        }
        if truth(&signal) {
            set(&options, "signal", &signal);
        }
        let fetch = self.inner.borrow().fetch.clone();
        let response = resolved(apply(&fetch, &global(), &[format!("{base}{route}").into(), options])?).await?;
        let value = invoke(&response, "json", &[]).await?;
        if !truth(&get(&response, "ok")) {
            let status = get(&response, "status");
            let m = text(&value, "error");
            let e =
                error(if m.is_empty() { format!("请求失败（{}）", status.as_f64().unwrap_or(0.0)) } else { m });
            set(&e, "status", &status);
            return Err(e);
        }
        Ok(value)
    }
    /// Start an authenticated request for a route of the current session.
    fn session_fetch(
        &self,
        route: &str,
        method: &str,
        data: JsValue,
    ) -> Result<(Rc<RefCell<Attempt>>, PendingFetch), JsValue> {
        let Some(a) = self.inner.borrow().attempt.clone() else {
            return Err(error("请先连接手机"));
        };
        let (base, session, cancelled) = {
            let a = a.borrow();
            (a.base.clone(), a.session.clone(), a.cancelled)
        };
        if cancelled || !truth(&session) {
            return Err(error("请先连接手机"));
        }
        let token = text(&session, "token");
        let headers = obj(&[("Authorization", format!("Bearer {token}").into())]);
        let options = obj(&[
            ("method", method.into()),
            ("headers", headers.clone()),
            ("cache", "no-store".into()),
            ("redirect", "error".into()),
        ]);
        if !data.is_undefined() {
            set(&headers, "Content-Type", &"application/json".into());
            set(&options, "body", &js_sys::JSON::stringify(&data).unwrap().into());
        }
        let url = format!("{base}/api/sessions/{}{route}", text(&session, "id"));
        let fetch = self.inner.borrow().fetch.clone();
        Ok((a, apply(&fetch, &global(), &[url.into(), options])))
    }
    fn session_promise(&self, route: &str, method: &str, data: JsValue) -> Promise {
        let (a, response) = match self.session_fetch(route, method, data) {
            Ok(started) => started,
            Err(e) => return Promise::reject(&e),
        };
        let this = self.clone();
        future_to_promise(async move {
            let result = async {
                let response = resolved(response?).await?;
                let value = invoke(&response, "json", &[]).await?;
                if !truth(&get(&response, "ok")) {
                    let e = error(if text(&value, "error").is_empty() {
                        format!("请求失败（{}）", number(&response, "status"))
                    } else {
                        text(&value, "error")
                    });
                    set(&e, "status", &get(&response, "status"));
                    return Err(e);
                }
                Ok(value)
            }
            .await;
            result.map_err(|e| {
                let safe = error(this.safe_error(&e, &a));
                set(&safe, "status", &get(&e, "status"));
                safe
            })
        })
    }
    fn watch_usb(&self, a: &Rc<RefCell<Attempt>>, uid: String) {
        let events = self.inner.borrow().usb_events.clone();
        if !get(&events, "addEventListener").is_function() || !get(&events, "removeEventListener").is_function() {
            return;
        }
        let device = get(&a.borrow().target, "device");
        let weak = Rc::downgrade(&self.inner);
        let attempt = Rc::downgrade(a);
        let handler = callback(move |event| {
            if let (Some(inner), Some(a)) = (weak.upgrade(), attempt.upgrade()) {
                let this = BrowserClient { inner };
                if !this.current(&a) {
                    return JsValue::UNDEFINED;
                }
                let unplugged = get(&event, "device");
                if !truth(&unplugged) {
                    return JsValue::UNDEFINED;
                }
                let serial = text(&unplugged, "serialNumber");
                let canonical =
                    if a.borrow().platform == "android" { android_uid(&serial) } else { canonical_uid(&serial) };
                if unplugged == device || canonical.is_ok_and(|s| s == uid) {
                    this.connection_lost(
                        a,
                        "offline",
                        "手机已断开 USB 连接。请重新连接设备；已经发出的操作可能已执行。",
                    );
                }
            }
            JsValue::UNDEFINED
        });
        let _ = call(&events, "addEventListener", &["disconnect".into(), handler.clone()]);
        a.borrow_mut().detach = callback(move |_| {
            let _ = call(&events, "removeEventListener", &["disconnect".into(), handler.clone()]);
            JsValue::UNDEFINED
        });
    }
    async fn complete_connection(&self, a: Rc<RefCell<Attempt>>, chooser: JsValue) -> Result<JsValue, JsValue> {
        let result = self.prepare_connection(a.clone(), chooser).await;
        let outcome = if let Err(e) = result {
            if !a.borrow().cancelled {
                let m = self.safe_error(&e, &a);
                a.borrow_mut().cancelled = true;
                self.update(obj(&[("phase", "error".into()), ("error", m.clone().into()), ("session", JsValue::NULL)]));
                Err(error(m))
            } else {
                Ok(JsValue::UNDEFINED)
            }
        } else {
            Ok(JsValue::UNDEFINED)
        };
        if a.borrow().cancelled {
            let _ = resolved(self.cleanup(a.clone()).into()).await;
            let mut i = self.inner.borrow_mut();
            if i.attempt.as_ref().is_some_and(|b| Rc::ptr_eq(&a, b)) {
                i.attempt = None;
            }
        }
        let id = a.borrow().id;
        self.release(id);
        outcome
    }
    async fn prepare_connection(&self, a: Rc<RefCell<Attempt>>, chooser: JsValue) -> Result<(), JsValue> {
        let target = resolved(chooser).await?;
        a.borrow_mut().target = target.clone();
        if a.borrow().cancelled {
            return Ok(());
        }
        let hello = get(&target, "hello");
        let serial = text(&hello, "serialNumber");
        let platform = a.borrow().platform.clone();
        let uid = if platform == "android" { android_uid(&serial) } else { canonical_uid(&serial) }.map_err(error)?;
        self.watch_usb(&a, uid.clone());
        // Keep the actual device separate from the packet-reassembly proxy.
        // Instrumenting that proxy can replace its underlying transferIn with
        // a wrapper that calls the proxy again, starving reads and UI events.
        a.borrow_mut().physical_device = get(&target, "device");
        if platform == "ios" && get(&get(&target, "device"), "transferIn").is_function() {
            set(&target, "device", &crate::packets::complete_usb_packet_reads(get(&target, "device"))?);
        }
        let device = copy(&hello);
        set(&device, "serialNumber", &uid.clone().into());
        self.update(obj(&[("phase", "pairing".into()), ("device", device)]));
        let sdk = a.borrow().sdk.clone();
        let stored = invoke(&sdk, "getPairRecord", &[uid.clone().into()]).await?;
        if a.borrow().cancelled {
            return Ok(());
        }
        let device = obj(&[
            ("serialNumber", uid.into()),
            ("vendorId", get(&hello, "vendorId")),
            ("productId", get(&hello, "productId")),
        ]);
        if platform == "android" {
            set(&device, "platform", &"android".into());
        }
        if truth(&get(&hello, "productName")) {
            set(&device, "productName", &get(&hello, "productName"));
        }
        let data = obj(&[("device", device)]);
        if truth(&stored) {
            let udid = canonical_uid(&text(&stored, "udid")).map_err(error)?;
            set(
                &data,
                "pairRecord",
                &obj(&[("udid", udid.into()), ("pairRecordBase64", get(&stored, "pairRecordBase64"))]),
            );
        }
        let (base, token) = {
            let a = a.borrow();
            (a.base.clone(), a.access_token.clone())
        };
        let session = self.request(&base, "/api/sessions", &token, "POST", data, JsValue::UNDEFINED).await?;
        a.borrow_mut().session = session.clone();
        a.borrow_mut().access_token.clear();
        if a.borrow().cancelled {
            return Ok(());
        }
        self.update(obj(&[(
            "session",
            obj(&[
                ("platform", platform.into()),
                ("id", get(&session, "id")),
                ("deviceId", get(&session, "deviceId")),
                ("state", "waiting_for_relay".into()),
                ("ready", false.into()),
                ("daemon", JsValue::NULL),
                ("setup", json(serde_json::json!({"state":"idle","transcript":"","exitCode":null}))),
                ("task", JsValue::NULL),
            ]),
        )]));
        let id = a.borrow().id;
        self.hold(id);
        let base = if base.is_empty() { text(&get(&global(), "location"), "origin") } else { base };
        let options = obj(&[
            ("registryApiUrl", base.into()),
            ("token", get(&session, "token")),
            ("target", target.clone()),
            ("log", a.borrow().log.clone()),
        ]);
        let result = invoke(&sdk, "pairDevice", &[options]).await;
        if !truth(&a.borrow().target) {
            a.borrow_mut().target = target;
        }
        let result = result?;
        a.borrow_mut().relay = get(&result, "relay");
        if truth(&a.borrow().relay) {
            set(&a.borrow().relay, "continuousPreviewEnabled", &false.into());
        }
        if a.borrow().cancelled {
            return Ok(());
        }
        if get(&sdk, "attachTransportDiagnostics").is_function() {
            let this = self.clone();
            let base = a.borrow().base.clone();
            let route = format!("/api/sessions/{}/diagnostics", text(&session, "id"));
            let token = text(&session, "token");
            let report = callback2(move |metrics, signal| {
                let this = this.clone();
                let base = base.clone();
                let route = route.clone();
                let token = token.clone();
                future_to_promise(async move { this.request(&base, &route, &token, "POST", metrics, signal).await })
                    .into()
            });
            let target = a.borrow().target.clone();
            let diagnostic_target =
                obj(&[("device", a.borrow().physical_device.clone()), ("session", get(&target, "session"))]);
            let diagnostics =
                call(&sdk, "attachTransportDiagnostics", &[diagnostic_target, get(&result, "relay"), report])
                    .unwrap_or_default();
            a.borrow_mut().diagnostics = diagnostics;
        }
        let record = get(&result, "pairRecord");
        if truth(&record) {
            let udid = canonical_uid(&text(&record, "udid")).map_err(error)?;
            invoke(
                &sdk,
                "putPairRecord",
                &[
                    obj(&[("udid", udid.into()), ("pairRecordBase64", get(&record, "pairRecordBase64"))]),
                    obj(&[("productName", get(&hello, "productName"))]),
                ],
            )
            .await?;
        }
        if !a.borrow().cancelled {
            self.update(obj(&[("phase", "connected".into())]));
        }
        Ok(())
    }
    fn connection_lost(&self, a: Rc<RefCell<Attempt>>, phase: &str, message: &str) {
        if !self.current(&a) {
            return;
        }
        a.borrow_mut().cancelled = true;
        {
            let mut inner = self.inner.borrow_mut();
            inner.attempt = None;
            inner.refresh_sequence += 1;
        }
        self.update(obj(&[
            ("phase", phase.into()),
            ("session", JsValue::NULL),
            ("device", JsValue::NULL),
            ("error", message.into()),
        ]));
        self.publish_custody();
        let _ = self.cleanup(a);
    }
    fn cleanup(&self, a: Rc<RefCell<Attempt>>) -> Promise {
        let diagnostics = std::mem::take(&mut a.borrow_mut().diagnostics);
        if diagnostics.is_function() {
            let _ = apply(&diagnostics, &JsValue::UNDEFINED, &[]);
        }
        let (relay, session, target, detach, sdk, base) = {
            let mut a = a.borrow_mut();
            (
                std::mem::take(&mut a.relay),
                std::mem::take(&mut a.session),
                std::mem::take(&mut a.target),
                std::mem::take(&mut a.detach),
                a.sdk.clone(),
                a.base.clone(),
            )
        };
        if detach.is_function() {
            let _ = apply(&detach, &JsValue::UNDEFINED, &[]);
        }
        if truth(&relay) {
            let _ = call(&relay, "close", &[]);
        }
        let abort = ctor("AbortController", &[]).unwrap_or_default();
        let timeout_ms = self.inner.borrow().cleanup_timeout;
        let abort_clone = abort.clone();
        let timer_slot = Rc::new(RefCell::new(JsValue::UNDEFINED));
        let timer_copy = timer_slot.clone();
        let timeout = Promise::new(&mut |resolve, _| {
            let resolve: JsValue = resolve.into();
            let abort_clone = abort_clone.clone();
            let callback = callback(move |_| {
                let _ = call(&abort_clone, "abort", &[]);
                let _ = apply(&resolve, &JsValue::UNDEFINED, &[]);
                JsValue::UNDEFINED
            });
            *timer_copy.borrow_mut() = global_call("setTimeout", &[callback, timeout_ms.into()]).unwrap_or_default();
        });
        let usb = if truth(&target) {
            let owner = {
                let mut i = self.inner.borrow_mut();
                i.next_id += 1;
                i.next_id
            };
            self.hold(owner);
            let request =
                call(&sdk, "closeDeviceRelayTarget", &[target]).unwrap_or_else(|e| Promise::reject(&e).into());
            let this = self.clone();
            future_to_promise(async move {
                let _ = resolved(request).await;
                this.release(owner);
                Ok(JsValue::UNDEFINED)
            })
        } else {
            Promise::resolve(&JsValue::UNDEFINED)
        };
        let deletion = if truth(&session) {
            let this = self.clone();
            let signal = get(&abort, "signal");
            future_to_promise(async move {
                this.request(
                    &base,
                    &format!("/api/sessions/{}", text(&session, "id")),
                    &text(&session, "token"),
                    "DELETE",
                    JsValue::UNDEFINED,
                    signal,
                )
                .await
            })
        } else {
            Promise::resolve(&JsValue::UNDEFINED)
        };
        let settled = Promise::all_settled(&Array::of2(&usb, &deletion));
        let race = Promise::race(&Array::of2(&settled, &timeout));
        future_to_promise(async move {
            let _ = resolved(race.into()).await;
            clear_timer(&timer_slot.borrow());
            Ok(JsValue::UNDEFINED)
        })
    }
}
fn progress_label(value: &str) -> Option<&'static str> {
    Some(match value {
        "Android authorization" => "请在手机上允许 USB 调试（RSA 指纹授权）",
        "Android control startup" => "正在准备 Android 控制通道",
        "Android ready" => "Android 已就绪",
        "Selecting USB device" => "等待 USB 授权",
        "Selected USB device" => "已选择设备",
        "Claiming usbmux interface" => "正在申请 USB 接口访问",
        "USB interface claim failed" => "此接口暂不可用，SDK 正在尝试其他接口",
        "Claimed usbmux interface" => "USB 接口已授权",
        "Created usbmux session" => "USB 数据通道已建立",
        "Connected WebSocket relay" => "已连接服务器",
        "Pairing requested" => "等待设备信任与配对",
        "Pair record received" => "设备配对记录已保存",
        _ => return None,
    })
}
