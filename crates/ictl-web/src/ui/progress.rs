use super::*;
fn tool_label(name: &str) -> String {
    match name {
        "iphone_tap" => "点按屏幕",
        "iphone_swipe" => "滑动屏幕",
        "iphone_long_press" => "长按",
        "iphone_press_button" => "按下设备按键",
        "iphone_type_text" => "输入文字",
        "iphone_tap_element" => "点按界面元素",
        "iphone_launch_app" => "打开应用",
        "iphone_open_url" => "打开链接",
        "iphone_terminate_app" => "关闭应用",
        "iphone_wait_for" => "等待界面",
        other => other,
    }
    .to_string()
}
fn state_label(state: &str) -> String {
    match state {
        "observing" => "正在查看屏幕",
        "thinking" => "正在规划下一步",
        "awaiting_confirmation" => "等待你的确认",
        "executing" => "正在操作",
        "recovering" => "正在等待设备恢复",
        "completed" => "任务已完成",
        "failed" => "任务失败",
        "stopped" => "任务已停止",
        other => other,
    }
    .to_string()
}
/// "观察 0.90 秒 · 模型 3.12 秒 · 操作 0.85 秒" for the stages a timing record contains.
fn timing_text(timings: &Value) -> String {
    [("观察", "observationMs"), ("模型", "modelMs"), ("操作", "actionMs")]
        .into_iter()
        .filter_map(|(label, key)| timings[key].as_f64().map(|ms| format!("{label} {:.2} 秒", ms / 1000.0)))
        .collect::<Vec<_>>()
        .join(" · ")
}
fn step_label(state: &str) -> String {
    match state {
        "pending" => "等待",
        "awaiting_confirmation" => "待确认",
        "executing" => "操作中",
        "succeeded" => "已执行",
        "missed" => "未命中",
        "failed" => "失败",
        "cancelled" => "已取消",
        "uncertain" => "结果不确定",
        other => other,
    }
    .to_string()
}
/// Instruction for a new task resuming a failed or stopped one. The gateway also
/// gives the model the previous task's summary as session history.
fn continue_instruction(task: &Value) -> String {
    let goal = string(task, "instruction");
    let reason = [string(task, "error"), string(task, "message")].into_iter().find(|s| !s.is_empty());
    let mut text = format!("继续完成上一个任务：{goal}\n");
    if let Some(reason) = reason {
        text.push_str(&format!("上次结束原因：{reason}\n"));
    }
    text.push_str("请先查看当前屏幕，从中断的地方接着做，不要重复已经完成的步骤。");
    text.chars().take(3900).collect()
}
#[component]
pub(super) fn TaskProgress(ui: Ui) -> impl IntoView {
    let goal = RwSignal::new(String::new());
    view! {
            <section class="task-progress card" aria-labelledby="execution-heading">
                <div class="progress-header">
                    <div class="progress-title"><Icon name="activity" /><h2 id="execution-heading">"执行记录"</h2></div>
                    <Show when=move || ui.running()>
                        <button
                            class="button danger small"
                            disabled=move || {
                                let s = ui.session.read();
                                ui.stop_key.get()
                                    == format!("{}/{}", string(&s, "id"), string(&s["task"], "id"))
                            }
                            on:click=move |_| ui.stop()
                        >
                            <Icon name="stop" />"停止任务"
                        </button>
                    </Show>
                </div>
                <Show
                    when=move || !ui.task.with(Value::is_null)
                    fallback=|| {
                        view! {
                            <div class="empty-progress">
                                <span class="progress-glyph"><Icon name="sparkles" /></span>
                                <div>
                                    <h3>"把想做的事，交给手机"</h3>
                                    <p>"描述一个目标，AI 会查看屏幕并一步步完成。"</p>
                                    <strong>"等待第一个任务"</strong>
                                </div>
                            </div>
                        }
                    }
                >
                    <div
                        class=move || format!("task-status {}", ui.task.with(|t| string(t, "state")))
                        role="status"
                    >
                        <span class="tiny-dot" />
                        {move || state_label(&ui.task.with(|t| string(t, "state")))}
                        <span class="step-count">
                            {move || {
                                format!("{} 步", ui.task.read()["steps"].as_array().map_or(0, Vec::len))
                            }}
                        </span>
                    </div>
                    <p class="current-instruction">{move || ui.task.with(|t| string(t, "instruction"))}</p>
                    <Show when=move || ui.task.read()["autoConfirm"] == true>
                        <p class="field-hint">"已开启自动确认：除付款、购买外不再询问"</p>
                    </Show>
                    <Show when=move || {
                        ui.task.read()["instructionUpdates"].as_array().is_some_and(|updates| !updates.is_empty())
                    }>
                        <ul class="field-hint" aria-label="目标修改记录">
                            {move || {
                                ui.task.read()["instructionUpdates"]
                                    .as_array()
                                    .cloned()
                                    .unwrap_or_default()
                                    .into_iter()
                                    .map(|update| {
                                        view! {
                                            <li>
                                                {format!(
                                                    "第 {} 步后更新了目标，原目标：{}",
                                                    update["afterStep"].as_u64().unwrap_or(0),
                                                    string(&update, "previous"),
                                                )}
                                            </li>
                                        }
                                    })
                                    .collect_view()
                            }}
                        </ul>
                    </Show>
                    <Show when=move || ui.running()>
                        <form
                            class="goal-update"
                            on:submit=move |e| {
                                e.prevent_default();
                                let text = goal.get_untracked();
                                if text.trim().is_empty() || ui.busy.get_untracked() {
                                    return;
                                }
                                goal.set(String::new());
                                ui.steer(text);
                            }
                        >
                            <label class="field-label" for="task-goal-update">
                                "更新任务目标"
                            </label>
                            <div class="text-control">
                                <input
                                    id="task-goal-update"
                                    maxlength="4000"
                                    placeholder="执行中想调整？输入新的目标，已执行的步骤会保留"
                                    prop:value=move || goal.get()
                                    on:input=move |e| goal.set(event_target_value(&e))
                                />
                                <button
                                    class="button secondary small"
                                    type="submit"
                                    disabled=move || ui.busy.get() || goal.get().trim().is_empty()
                                >
                                    "更新目标"
                                </button>
                            </div>
                        </form>
                    </Show>
                    <Show when=move || !ui.task.with(|t| string(t, "model")).is_empty()>
                        <p class="field-hint">
                            {move || {
                                format!("本次任务模型：{}", ui.task.with(|t| string(t, "model")))
                            }}
                        </p>
                    </Show>
                    <Show when=move || !ui.task.read()["timings"].is_null()>
                        <p class="card-description" aria-label="最近一轮耗时">
                            {move || timing_text(&ui.task.read()["timings"])}
                        </p>
                    </Show>
                    <Show when=move || ui.task.read()["plan"].as_array().is_some_and(|plan| !plan.is_empty())>
                        <p class="field-hint" aria-label="已规划的后续步骤">
                            {move || {
                                format!(
                                    "已规划、待执行：{}",
                                    ui.task.read()["plan"]
                                        .as_array()
                                        .map(|plan| {
                                            plan.iter()
                                                .map(|action| tool_label(action["tool"].as_str().unwrap_or_default()))
                                                .collect::<Vec<_>>()
                                                .join(" → ")
                                        })
                                        .unwrap_or_default(),
                                )
                            }}
                        </p>
                    </Show>
                    <Show when=move || !ui.task.read()["confirmation"].is_null()>
                        <div class="confirmation" role="alert">
                            <div class="confirmation-heading"><Icon name="shield" /><strong>"这一步需要你的确认"</strong><span class="badge pending">"待确认"</span></div>
                            <p>{move || string(&ui.task.read()["confirmation"], "reason")}</p>
                            <code>
                                {move || tool_label(&string(&ui.task.read()["confirmation"], "tool"))}
                            </code>
                            <pre>
                                {move || {
                                    serde_json::to_string_pretty(&ui.task.read()["confirmation"]["args"])
                                        .unwrap_or_default()
                                }}
                            </pre>
                            <div class="button-row">
                                <button
                                    class="button secondary"
                                    disabled=move || ui.busy.get()
                                    on:click=move |_| ui.confirm(false)
                                >
                                    "拒绝并停止"
                                </button>
                                <button
                                    class="button primary"
                                    disabled=move || ui.busy.get()
                                    on:click=move |_| ui.confirm(true)
                                >
                                    "确认这一步"
                                </button>
                            </div>
                        </div>
                    </Show>
                    <ol class="step-list">
                        {move || {
                            ui.task.read()["steps"]
                                .as_array()
                                .cloned()
                                .unwrap_or_default()
                                .into_iter()
                                .enumerate()
                                .map(|(i, step)| {
                                    view! {
                                        <li class=format!("step {}", string(&step, "state"))>
                                            <span class="step-number">{i + 1}</span>
                                            <div>
                                                <strong>
                                                    {tool_label(&string(&step, "tool"))}
                                                    {(step["planned"] == true).then_some(" · 连续执行")}
                                                </strong>
                                                <p>{string(&step, "reason")}</p>
                                                {
                                                    let timing = timing_text(&step["timings"]);
                                                    (!timing.is_empty())
                                                        .then(|| {
                                                            view! {
                                                                <small class="step-timing" aria-label="本步耗时">
                                                                    {timing}
                                                                </small>
                                                            }
                                                        })
                                                }
                                            </div>
                                            <span class=format!(
                                                "step-state {}",
                                                string(&step, "state"),
                                            )>{step_label(&string(&step, "state"))}</span>
                                        </li>
                                    }
                                })
                                .collect_view()
                        }}
                    </ol>
                    <Show when=move || {
                        !ui.task.with(|t| string(t, "message")).is_empty()
                            || !ui.task.with(|t| string(t, "error")).is_empty()
                    }>
                        <p class=move || {
                            if ui.task.with(|t| string(t, "error")).is_empty() {
                                "task-message"
                            } else {
                                "inline-error"
                            }
                        }>
                            {move || {
                                let err = ui.task.with(|t| string(t, "error"));
                                if err.is_empty() { ui.task.with(|t| string(t, "message")) } else { err }
                            }}
                        </p>
                    </Show>
                    <Show when=move || ui.task.read()["state"] == "stopped">
                        <p class="field-hint">
                            "后续步骤已停止。停止前已发出的操作可能仍已执行，请检查手机。"
                        </p>
                    </Show>
                    <Show when=move || {
                        matches!(ui.task.with(|t| string(t, "state")).as_str(), "failed" | "stopped") && ui.can_control()
                    }>
                        <div class="button-row">
                            <button
                                class="button primary small"
                                disabled=move || ui.busy.get() || ui.ai.get()["aiReady"] != true
                                on:click=move |_| {
                                    let task = ui.task.get_untracked();
                                    let text = continue_instruction(&task);
                                    let model = string(&task, "model");
                                    let auto = task["autoConfirm"] == true;
                                    ui.run(move |client| {
                                        client.start_task(
                                            text,
                                            if model.is_empty() { None } else { Some(model) },
                                            JsValue::NULL,
                                            Some(auto),
                                        )
                                    });
                                }
                            >
                                "继续任务"
                            </button>
                        </div>
                    </Show>
                </Show>
            </section>
    }
}
