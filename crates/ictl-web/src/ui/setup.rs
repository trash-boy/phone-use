use super::*;
#[component]
pub(super) fn SetupPanel(ui: Ui) -> impl IntoView {
    let line = RwSignal::new(String::new());
    let transcript = NodeRef::<leptos::html::Pre>::new();
    let setup = move || ui.session.read()["setup"].clone();
    let running = move || setup()["state"] == "running";
    let transcript_text = Memo::new(move |_| string(&setup(), "transcript"));
    Effect::new(move |_| {
        let _ = transcript_text.get();
        if let Some(pre) = transcript.get() {
            pre.set_scroll_top(pre.scroll_height());
        }
    });
    view! {
        <section class="setup-card card" aria-labelledby="setup-heading">
            <div class="card-header">
                <div>
                    <span class="section-label">"设备准备"</span>
                    <h2 id="setup-heading">"授权与安装向导"</h2>
                </div>
                <span class=move || {
                    format!("badge {}", if running() { "pending" } else { "" })
                }>
                    {move || match setup()["state"].as_str().unwrap_or_default() {
                        "idle" => "尚未开始".into(),
                        "running" => "正在准备".into(),
                        "stopping" => "正在停止".into(),
                        "complete" => "向导已结束".into(),
                        "failed" => "准备失败".into(),
                        s => s.to_string(),
                    }}
                </span>
            </div>
            <p class="card-description">
                "先解锁手机并信任电脑，再按提示开启开发者模式。Apple ID 用于申请开发证书并给 WDA 控制组件签名，让 iOS 允许它运行。验证和签名由当前服务器处理，后续连接会复用有效签名。"
            </p>
            <pre
                class="setup-transcript"
                node_ref=transcript
                aria-label="设备准备输出"
                aria-live="polite"
            >
                {move || {
                    let text = transcript_text.get();
                    if text.is_empty() {
                        "等待设备连接和原生向导输出…".into()
                    } else {
                        text
                    }
                }}
            </pre>
            <Show when=move || !string(&setup(), "error").is_empty()>
                <p class="inline-error">{move || string(&setup(), "error")}</p>
            </Show>
            <Show
                when=running
                fallback=move || {
                    view! {
                        <button
                            class="button secondary"
                            disabled=move || {
                                ui.busy.get() || ui.running() || setup()["state"] == "stopping"
                            }
                            on:click=move |_| ui.run(|client| client.start_setup())
                        >
                            {move || {
                                if setup()["state"] == "idle" {
                                    "启动准备向导"
                                } else {
                                    "重新启动向导"
                                }
                            }}
                        </button>
                    }
                }
            >
                <form
                    class="setup-input"
                    on:submit=move |e| {
                        e.prevent_default();
                        if line.get_untracked().is_empty() || ui.busy.get_untracked()
                            || ui.running()
                        {
                            return;
                        }
                        let value = line.get_untracked();
                        line.set(String::new());
                        ui.run(move |client| client.setup_input(value));
                    }
                >
                    <label for="setup-line">"仅在向导提示输入时提交"</label>
                    <div class="text-control">
                        <input
                            id="setup-line"
                            type="password"
                            prop:value=move || line.get()
                            on:input=move |e| line.set(event_target_value(&e))
                            autocomplete="off"
                            spellcheck="false"
                            maxlength="4096"
                            placeholder="账号、密码或验证码（单行）"
                        />
                        <button
                            class="button secondary"
                            type="submit"
                            disabled=move || ui.busy.get() || ui.running() || line.get().is_empty()
                        >
                            "提交"
                        </button>
                    </div>
                    <p class="field-hint">
                        "提交后立即清空。页面不保存输入内容。"
                    </p>
                </form>
            </Show>
        </section>
    }
}
