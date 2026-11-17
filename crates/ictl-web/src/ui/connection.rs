use super::*;

pub(super) fn open_connection_settings() {
    if let Some(dialog) = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.get_element_by_id("connection-dialog"))
    {
        let _ = call(&dialog.into(), "showModal", &[]);
    }
}

#[component]
pub(super) fn ConnectionDialog(ui: Ui) -> impl IntoView {
    view! {
        <dialog id="connection-dialog" class="connection-dialog" aria-labelledby="connection-dialog-title">
            <div class="dialog-header">
                <div>
                    <span class="section-label">"USB 设备"</span>
                    <h2 id="connection-dialog-title">"连接设备"</h2>
                </div>
                <form method="dialog">
                    <button class="icon-button" aria-label="关闭连接设置" type="submit">
                        <Icon name="close" />
                    </button>
                </form>
            </div>
            <ConnectionPanel ui />
            <p class="dialog-note">
                <Icon name="shield" />
                "保持手机解锁，按提示完成信任与授权。关闭此设置窗口不会断开设备。"
            </p>
        </dialog>
    }
}

#[component]
pub(super) fn ConnectionPanel(ui: Ui) -> impl IntoView {
    let platform = RwSignal::new("ios".to_string());
    let token = RwSignal::new(String::new());
    let supported = truth(&get(&global(), "isSecureContext")) && truth(&get(&get(&global(), "navigator"), "usb"));
    let selected = move || {
        if ui.occupied() {
            ui.session.read()["platform"]
                .as_str()
                .or(ui.state.read()["device"]["platform"].as_str())
                .unwrap_or(&platform.get())
                .to_string()
        } else {
            platform.get()
        }
    };
    view! {
        <section class="connection-panel" aria-labelledby="connection-heading">
            <h3 class="sr-only" id="connection-heading">"连接参数"</h3>
            <label class="field-label" for="device-platform">
                "手机系统"
            </label>
            <select
                id="device-platform"
                prop:value=selected
                disabled=move || ui.occupied()
                on:change=move |e| platform.set(event_target_value(&e))
            >
                <option value="ios">"iPhone / iOS"</option>
                <option value="android">"Android"</option>
            </select>
            <p class="field-hint">
                {move || {
                    if selected() == "android" {
                        "Android 8+：开启开发者选项与 USB 调试，在手机上允许 RSA 授权。无需 Apple ID。请退出本地 adb/scrcpy，释放 USB 接口。"
                    } else {
                        "Mac Chrome：保持手机解锁，完成信任与 WDA 准备。"
                    }
                }}
            </p>
            <div class="connection-device">
                <div class="device-icon"><Icon name="phone" /></div>
                <div>
                    <strong>
                        {move || {
                            let name = ui.state.with(|s| string(&s["device"], "productName"));
                            if name.is_empty() {
                                if selected() == "android" {
                                    "你的 Android 手机".into()
                                } else {
                                    "你的 iPhone".into()
                                }
                            } else {
                                name
                            }
                        }}
                    </strong>
                    <span>
                        {move || {
                            if ui.occupied() {
                                "保持 USB 连接与页面打开"
                            } else {
                                "通过 Chrome 与 USB 连接"
                            }
                        }}
                    </span>
                </div>
            </div>
            <Show when=move || !ui.state.with(|s| string(&s["device"], "serialNumber")).is_empty()>
                <code class="device-id">
                    {move || ui.state.with(|s| string(&s["device"], "serialNumber"))}
                </code>
            </Show>
            <label class="field-label" for="server-url">
                "服务器地址"
            </label>
            <input
                id="server-url"
                type="url"
                placeholder="留空使用当前网站"
                prop:value=move || ui.base.get()
                disabled=move || ui.occupied()
                on:input=move |e| ui.base.set(event_target_value(&e))
                autocomplete="off"
                spellcheck="false"
            />
            <label class="field-label" for="server-token">
                "服务器访问令牌"
            </label>
            <input
                id="server-token"
                type="password"
                placeholder="由服务器管理员提供"
                prop:value=move || token.get()
                disabled=move || ui.occupied()
                on:input=move |e| token.set(event_target_value(&e))
                autocomplete="off"
                spellcheck="false"
            />
            <p class="field-hint">"令牌仅在当前页面内存中使用。"</p>
            <Show
                when=move || ui.occupied()
                fallback=move || {
                    view! {
                        <button
                            class="button primary full"
                            aria-label=move || {
                                if ui.state.read()["cleanupPending"] == true {
                                    "等待 USB 释放"
                                } else if selected() == "android" {
                                    "连接 Android"
                                } else {
                                    "连接 iPhone"
                                }
                            }
                            disabled=move || {
                                !supported || ui.state.read()["cleanupPending"] == true
                                    || token.get().trim().is_empty()
                            }
                            on:click=move |_| {
                                let value = token.get_untracked();
                                token.set(String::new());
                                ui.connect(value, platform.get_untracked());
                            }
                        >
                            <Icon name="plug" />
                            " "
                            {move || {
                                if ui.state.read()["cleanupPending"] == true {
                                    "等待 USB 释放"
                                } else if selected() == "android" {
                                    "连接 Android"
                                } else {
                                    "连接 iPhone"
                                }
                            }}
                        </button>
                    }
                }
            >
                <button class="button secondary full" on:click=move |_| ui.disconnect()>
                    {move || {
                        if ui.state.read()["phase"] == "connected" {
                            "断开连接"
                        } else {
                            "取消连接"
                        }
                    }}
                </button>
            </Show>
            <Show when=move || !ui.state.with(|s| string(s, "warning")).is_empty()>
                <p class="field-hint" role="status">
                    {move || ui.state.with(|s| string(s, "warning"))}
                </p>
            </Show>
            {(!supported)
                .then(|| {
                    view! {
                        <p class="inline-error">
                            "请用 Chrome，在 HTTPS 或 localhost 打开此页面。"
                        </p>
                    }
                })}
            <div class="connection-note">
                <span class="tiny-dot" />
                " "
                {move || {
                    let status = ui.state.with(|s| string(s, "transportStatus"));
                    if ui.occupied() && !status.is_empty() {
                        status
                    } else {
                        "USB 数据由此浏览器转发".into()
                    }
                }}
            </div>
        </section>
    }
}
