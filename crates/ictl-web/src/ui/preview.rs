use super::*;
use crate::interaction::{Pointer, Rect, pointer_command, to_phone_point};
use web_sys::{HtmlImageElement, PointerEvent};
/// A continuous frame stops counting as live after this long without a new frame.
const LIVE_STALE_MS: u64 = 5000;
fn pointer(event: &PointerEvent, ui: Ui) -> Option<Pointer> {
    let image = event.current_target()?.dyn_into::<HtmlImageElement>().ok()?;
    let rect = image.get_bounding_client_rect();
    let (mut width, mut height) = (image.natural_width() as f64, image.natural_height() as f64);
    // MJPEG frames are scaled pixels, not phone points: map with the device screen size.
    if !ui.preview_url.get_untracked().is_empty() {
        let session = ui.session.read();
        let screen = &session["daemon"]["persisted"]["screen"];
        let (w, h) = (screen["width"].as_f64()?, screen["height"].as_f64()?);
        (width, height) = if width > height { (h.max(w), h.min(w)) } else { (h.min(w), h.max(w)) };
    }
    Some(Pointer {
        point: to_phone_point(
            event.client_x() as f64,
            event.client_y() as f64,
            Rect { left: rect.left(), top: rect.top(), width: rect.width(), height: rect.height() },
            width,
            height,
        ),
        x: event.client_x() as f64,
        y: event.client_y() as f64,
        time: now(),
    })
}
fn stop_live(ui: Ui) {
    ui.preview_url.set(String::new());
    let pending = ui.client.get_value().stop_preview();
    spawn_local(async move {
        let _ = resolved(pending.into()).await;
    });
}
fn start_live(ui: Ui, live_error: RwSignal<String>) {
    let session_id = ui.session.with(|s| string(s, "id"));
    live_error.set(String::new());
    let pending = ui.client.get_value().preview_stream();
    spawn_local(async move {
        match resolved(pending.into()).await {
            Ok(url) if ui.current_session(&session_id) => ui.preview_url.set(url.as_string().unwrap_or_default()),
            Ok(_) => {}
            Err(e) => live_error.set(message(&e)),
        }
    });
}
#[component]
pub(super) fn PhonePreview(ui: Ui) -> impl IntoView {
    let down = StoredValue::new_local(None::<Pointer>);
    let text = RwSignal::new(String::new());
    let position = RwSignal::new(String::new());
    let loaded = RwSignal::new(false);
    let landscape = RwSignal::new(false);
    let live_error = RwSignal::new(String::new());
    let stream_started = StoredValue::new_local((String::new(), 0.0));
    let image_ref = NodeRef::<leptos::html::Img>::new();
    let live = move || !ui.preview_url.get().is_empty();
    let live_fresh = move || ui.session.read()["preview"]["ageMs"].as_u64().is_some_and(|age| age <= LIVE_STALE_MS);
    // Chrome only emits load for the first MJPEG image: poll intrinsic dimensions with
    // session status so later rotations update the layout. A multipart response can stay
    // open across upstream retries without an error event, so a stream without frames for
    // 5 s is released (transport protection may have paused it) and one screenshot replaces it.
    Effect::new(move |_| {
        ui.state.track();
        let stream = ui.preview_url.get();
        if stream.is_empty() {
            stream_started.set_value((String::new(), 0.0));
            return;
        }
        let (previous, mut started) = stream_started.get_value();
        if previous != stream {
            started = now();
            stream_started.set_value((stream, started));
        }
        if now() - started >= LIVE_STALE_MS as f64 && !untrack(live_fresh) {
            live_error.set("实时画面暂停（链路拥塞或无新帧），已改回截图".into());
            loaded.set(false);
            stop_live(ui);
            if untrack(|| ui.can_control()) {
                ui.refresh_screen();
            }
            return;
        }
        if let Some(image) = image_ref.get()
            && image.natural_width() > 0
            && image.natural_height() > 0
        {
            loaded.set(true);
            landscape.set(image.natural_width() > image.natural_height());
        }
    });
    let disabled = move || !ui.can_control() || ui.busy.get() || (live() && !live_fresh());
    view! {
        <section class="phone-card card" aria-labelledby="preview-heading">
            <div class="card-header">
                <div>
                    <span class="section-label">"设备屏幕"</span>
                    <h2 id="preview-heading">{move || if live() { "实时画面" } else { "设备截图" }}</h2>
                </div>
                <Show when=move || !ui.android()>
                    <button
                        class="button secondary small live-toggle"
                        aria-pressed=move || live().to_string()
                        title="连续画面会持续占用 USB 链路；拥塞时自动暂停并改回截图"
                        disabled=move || !live() && !ui.ready()
                        on:click=move |_| {
                            if live() {
                                loaded.set(false);
                                stop_live(ui);
                            } else {
                                start_live(ui, live_error);
                            }
                        }
                    >
                        {move || if live() { "停止实时" } else { "实时画面" }}
                    </button>
                </Show>
                <button
                    class="icon-button"
                    aria-label="刷新屏幕"
                    title="刷新屏幕"
                    disabled=disabled
                    on:click=move |_| ui.refresh_screen()
                >
                    <Icon name="refresh" />
                </button>
            </div>
            <div class="phone-stage">
                <div class=move || {
                    format!(
                        "phone-frame {} {}",
                        if ui.screen.with(Value::is_null) && !live() { "" } else { "has-screen" },
                        if landscape.get() { "wide" } else { "" },
                    )
                }>
                    <Show
                        when=move || !ui.screen_src.with(String::is_empty) || live()
                        fallback=move || {
                            view! {
                                <div class="empty-phone">
                                    <div class="empty-usb"><Icon name="plug" /></div>
                                    <strong>{move || ui.status()}</strong>
                                    <p>
                                        {move || if ui.ready() {
                                            if ui.android() {
                                                "点击刷新屏幕查看设备。"
                                            } else {
                                                "点击刷新屏幕查看设备，或开启实时画面。"
                                            }
                                        } else {
                                            "连接并准备好设备后，即可查看和操作屏幕。"
                                        }}
                                    </p>
                                    <span class="empty-phone-line" />
                                </div>
                            }
                        }
                    >
                        <img
                            node_ref=image_ref
                            class="phone-screen"
                            src=move || {
                                let stream = ui.preview_url.get();
                                if !stream.is_empty() {
                                    return stream;
                                }
                                ui.screen_src.get()
                            }
                            alt="手机当前屏幕"
                            draggable="false"
                            on:load=move |e| {
                                if let Some(image) = e
                                    .current_target()
                                    .and_then(|v| v.dyn_into::<HtmlImageElement>().ok())
                                {
                                    loaded.set(true);
                                    landscape.set(image.natural_width() > image.natural_height());
                                }
                            }
                            on:error=move |_| {
                                if !ui.preview_url.get_untracked().is_empty() {
                                    live_error.set("实时画面中断，已改回截图".into());
                                    stop_live(ui);
                                }
                                loaded.set(false);
                                landscape.set(false);
                            }
                            on:pointerdown=move |e| {
                                if disabled() || !loaded.get_untracked() {
                                    return;
                                }
                                if let Some(image) = e
                                    .current_target()
                                    .and_then(|v| v.dyn_into::<HtmlImageElement>().ok())
                                {
                                    let _ = image.set_pointer_capture(e.pointer_id());
                                }
                                down.set_value(pointer(&e, ui));
                            }
                            on:pointermove=move |e| {
                                position
                                    .set(
                                        pointer(&e, ui)
                                            .and_then(|p| p.point)
                                            .map(|p| format!("{}, {}", p.x, p.y))
                                            .unwrap_or_default(),
                                    )
                            }
                            on:pointerup=move |e| {
                                let start = down.get_value();
                                down.set_value(None);
                                if disabled() {
                                    return;
                                }
                                if let Some(command) = start
                                    .zip(pointer(&e, ui))
                                    .and_then(|(s, e)| pointer_command(s, e))
                                {
                                    ui.manual(string(&command, "tool"), command["args"].clone());
                                }
                            }
                            on:pointercancel=move |_| down.set_value(None)
                        />
                    </Show>
                </div>
            </div>
            <div class="screen-meta">
                <span>
                    <i class=move || {
                        format!("tiny-dot {}", if ui.screen.with(Value::is_null) && !live() { "" } else { "live" })
                    } />
                    {move || {
                        if live() {
                            if live_fresh() { "实时画面" } else { "实时画面连接中" }
                        } else if ui.running() {
                            "任务最近一次观察"
                        } else if ui.screen.with(Value::is_null) {
                            "按需截图"
                        } else {
                            "最近一次截图（非实时）"
                        }
                    }}
                    {move || {
                        if live() {
                            return None;
                        }
                        ui.screen
                            .with(|s| s["timings"]["screenshotMs"].as_f64())
                            .filter(|ms| ms.is_finite())
                            .map(|ms| {
                                view! {
                                    <span title="采集设备画面的耗时，不含模型推理">
                                        {format!(" · 采图 {:.0} ms", ms)}
                                    </span>
                                }
                            })
                    }}
                </span>
                <code>
                    {move || {
                        if position.get().is_empty() {
                            "坐标随指针显示".into()
                        } else {
                            position.get()
                        }
                    }}
                </code>
            </div>
            <p class="screen-hint">
                {move || if ui.android() {
                    "点击刷新获取截图。任务运行时自动显示每一步的画面。点按、滑动可控制手机。"
                } else {
                    "点击刷新获取截图，或打开实时画面。任务运行时自动显示每一步的画面。点按、滑动可控制手机。"
                }}
            </p>
            <Show when=move || !live_error.get().is_empty()>
                <p class="field-hint">{move || live_error.get()}</p>
            </Show>
            <div class="hardware-buttons">
                <Show when=move || ui.android()>
                    <button
                        aria-label="返回"
                        disabled=disabled
                        on:click=move |_| {
                            ui.manual("iphone_press_button".into(), value!({"button":"back"}))
                        }
                    >
                        <Icon name="back" />
                        "返回"
                    </button>
                </Show>
                {[
                    ("home", "home", "主屏幕"),
                    ("lock", "lock", "锁屏"),
                    ("volume_down", "minus", "音量减"),
                    ("volume_up", "plus", "音量加"),
                ]
                    .into_iter()
                    .map(move |(button, icon, label)| {
                        view! {
                            <button
                                aria-label=label
                                disabled=disabled
                                on:click=move |_| {
                                    ui.manual(
                                        "iphone_press_button".into(),
                                        value!({"button":button}),
                                    )
                                }
                            >
                                <Icon name=icon />
                                {label}
                            </button>
                        }
                    })
                    .collect_view()}
            </div>
            <form
                class="text-control"
                on:submit=move |e| {
                    e.prevent_default();
                    if disabled() || text.get_untracked().trim().is_empty() {
                        return;
                    }
                    let value = text.get_untracked();
                    text.set(String::new());
                    ui.manual("iphone_type_text".into(), value!({"text":value,"submit":false}));
                }
            >
                <label class="sr-only" for="phone-text">
                    "输入文字到手机"
                </label>
                <input
                    id="phone-text"
                    placeholder="输入文字到手机…"
                    disabled=disabled
                    prop:value=move || text.get()
                    maxlength="10000"
                    on:input=move |e| text.set(event_target_value(&e))
                />
                <button
                    class="button secondary"
                    disabled=move || disabled() || text.get().trim().is_empty()
                    type="submit"
                >
                    "输入"
                </button>
            </form>
            <Show when=move || ui.android()>
                <p class="field-hint">
                    "请先点选手机上的输入框。中文输入通过粘贴完成，会覆盖手机剪贴板。"
                </p>
            </Show>
            <Show when=move || ui.screen.with(|s| !string(s, "elementsError").is_empty())>
                <p class="field-hint">"画面可用，界面元素暂时不可用。"</p>
            </Show>
        </section>
    }
}
