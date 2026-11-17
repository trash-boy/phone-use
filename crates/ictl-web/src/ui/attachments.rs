use super::*;
use base64::Engine;
use web_sys::{FileList, HtmlInputElement};

#[derive(Clone)]
struct Image {
    id: String,
    name: String,
    preview: String,
    size: f64,
}
#[derive(Clone, Copy)]
pub(super) struct TaskImages {
    entries: RwSignal<Vec<Image>>,
    pub uploading: RwSignal<bool>,
    error: RwSignal<String>,
}
impl TaskImages {
    pub fn new(ui: Ui) -> Self {
        let images = Self {
            entries: RwSignal::new(vec![]),
            uploading: RwSignal::new(false),
            error: RwSignal::new(String::new()),
        };
        let session = Memo::new(move |_| ui.session.with(|s| string(s, "id")));
        Effect::new(move |_| {
            let _ = session.get();
            images.entries.set(vec![]);
            images.uploading.set(false);
            images.error.set(String::new());
        });
        images
    }
    pub fn ids(self) -> JsValue {
        let ids: Vec<_> = self.entries.get_untracked().into_iter().map(|i| i.id).collect();
        if ids.is_empty() { JsValue::UNDEFINED } else { json(value!(ids)) }
    }
    pub fn add(self, ui: Ui, files: FileList) {
        if self.uploading.get_untracked() || ui.running() {
            return;
        }
        let session = ui.session.with(|s| string(s, "id"));
        if session.is_empty() {
            self.error.set("请连接手机后添加图片".into());
            return;
        }
        let files: Vec<_> = (0..files.length()).filter_map(|i| files.item(i)).collect();
        if files.is_empty() {
            return;
        }
        self.uploading.set(true);
        self.error.set(String::new());
        spawn_local(async move {
            for file in files {
                if !ui.current_session(&session) {
                    return;
                }
                let result = async {
                    let mime = file.type_();
                    if !["image/jpeg", "image/png"].contains(&mime.as_str()) {
                        return Err(error("仅支持 JPEG、PNG 图片"));
                    }
                    let size = file.size();
                    let entries = self.entries.get_untracked();
                    if size <= 0.0 || size > 5.0 * 1024.0 * 1024.0 {
                        return Err(error("单张图片不能超过 5 MiB"));
                    }
                    if entries.len() >= 6 || entries.iter().map(|i| i.size).sum::<f64>() + size > 20.0 * 1024.0 * 1024.0
                    {
                        return Err(error("最多添加 6 张图片，总计不能超过 20 MiB"));
                    }
                    let bytes = resolved(file.array_buffer().into()).await?;
                    if !ui.current_session(&session) {
                        return Ok(());
                    }
                    let data =
                        base64::engine::general_purpose::STANDARD.encode(js_sys::Uint8Array::new(&bytes).to_vec());
                    let name = file.name();
                    let uploaded = resolved(
                        ui.client.get_value().upload_attachment(name.clone(), mime.clone(), data.clone()).into(),
                    )
                    .await?;
                    if ui.current_session(&session) {
                        self.entries.update(|entries| {
                            entries.push(Image {
                                id: text(&uploaded, "id"),
                                name,
                                preview: format!("data:{mime};base64,{data}"),
                                size,
                            })
                        });
                    }
                    Ok::<_, JsValue>(())
                }
                .await;
                if let Err(e) = result {
                    if ui.current_session(&session) {
                        self.error.set(message(&e));
                    }
                    break;
                }
            }
            if ui.current_session(&session) {
                self.uploading.set(false);
            }
        });
    }
    fn remove(self, ui: Ui, id: String) {
        if self.uploading.get_untracked() || ui.running() {
            return;
        }
        let session = ui.session.with(|s| string(s, "id"));
        self.uploading.set(true);
        self.error.set(String::new());
        let pending = ui.client.get_value().delete_attachment(id.clone());
        spawn_local(async move {
            let result = resolved(pending.into()).await;
            if !ui.current_session(&session) {
                return;
            }
            match result {
                Ok(_) => self.entries.update(|entries| entries.retain(|image| image.id != id)),
                Err(e) => self.error.set(message(&e)),
            }
            self.uploading.set(false);
        });
    }
}

#[component]
pub(super) fn ImageInputs(ui: Ui, images: TaskImages) -> impl IntoView {
    view! {
        <div class="task-images">
            <label class="image-picker">
                <Icon name="image" />
                "添加图片"
                <input type="file" accept="image/jpeg,image/png" multiple
                    aria-label="添加任务图片"
                    disabled=move || images.uploading.get() || ui.running() || ui.session.with(Value::is_null)
                    on:change=move |event| {
                        if let Some(input) = event.current_target().and_then(|v| v.dyn_into::<HtmlInputElement>().ok()) {
                            if let Some(files) = input.files() { images.add(ui, files); }
                            input.set_value("");
                        }
                    }
                />
            </label>
            <span class="field-hint">{move || if images.uploading.get() { "图片上传中…" } else { "可粘贴或拖入图片 · 最多 6 张 · 单张 5 MiB" }}</span>
            <div class="image-thumbnails">
                {move || images.entries.get().into_iter().map(|image| {
                    let id = image.id.clone();
                    view! {
                        <figure>
                            <img src=image.preview alt=image.name.clone() />
                            <figcaption>{image.name}</figcaption>
                            <button type="button" aria-label="移除图片" disabled=move || images.uploading.get() || ui.running()
                                on:click=move |_| images.remove(ui, id.clone())><Icon name="close" /></button>
                        </figure>
                    }
                }).collect_view()}
            </div>
            <Show when=move || !images.error.get().is_empty()><p class="inline-error" role="alert">{move || images.error.get()}</p></Show>
        </div>
    }
}
