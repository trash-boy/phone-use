use crate::android_text;
use crate::js::*;
use js_sys::{Array, Promise};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::future_to_promise;
#[wasm_bindgen(js_name=quote)]
pub fn quote(value: JsValue) -> String {
    let value = match global_call("String", &[value]) {
        Ok(value) => value.as_string().unwrap_or_default(),
        Err(error) => wasm_bindgen::throw_val(error),
    };
    format!("'{}'", value.replace('\'', "'\\''"))
}
#[wasm_bindgen(js_name=androidUID)]
pub fn android_uid(value: JsValue) -> Result<String, JsValue> {
    crate::identity::android_uid(&value.as_string().ok_or_else(|| error("Invalid Android device serial"))?)
        .map_err(error)
}
#[wasm_bindgen(js_name=parseHierarchy)]
pub fn parse_hierarchy(xml: String) -> Result<JsValue, JsValue> {
    if !xml.contains("<hierarchy") || !xml.contains("</hierarchy>") {
        return Err(error("Android UI hierarchy unavailable"));
    }
    let nodes = Array::new();
    for body in android_text::node_bodies(&xml) {
        let attrs = object();
        for (name, value) in android_text::attributes(body) {
            set(&attrs, name, &android_text::decode_xml(value).into());
        }
        if text(&attrs, "visible-to-user") == "false" {
            continue;
        }
        let Some(values) = android_text::bounds(&text(&attrs, "bounds")) else {
            continue;
        };
        if values[2] <= values[0] || values[3] <= values[1] {
            continue;
        }
        let short = |key: &str, max: usize| text(&attrs, key).chars().take(max).collect::<String>();
        nodes.push(&obj(&[
            ("text", short("text", 1024).into()),
            ("label", short("content-desc", 1024).into()),
            ("id", short("resource-id", 512).into()),
            ("type", short("class", 256).into()),
            ("enabled", (text(&attrs, "enabled") != "false").into()),
            ("clickable", (text(&attrs, "clickable") == "true").into()),
            ("focusable", (text(&attrs, "focusable") == "true").into()),
            ("x", values[0].into()),
            ("y", values[1].into()),
            ("right", values[2].into()),
            ("bottom", values[3].into()),
        ]));
        if nodes.length() >= 5000 {
            break;
        }
    }
    Ok(nodes.into())
}
#[wasm_bindgen(js_name=inputViewport)]
pub fn input_viewport(input: String) -> JsValue {
    if let Some((width, height, orientation)) = android_text::input_viewport(&input) {
        return json(serde_json::json!({"width":width,"height":height,"orientation":orientation}));
    }
    JsValue::NULL
}
#[wasm_bindgen(js_name=mapPoint)]
pub fn map_point(view: JsValue, x: f64, y: f64) -> Result<JsValue, JsValue> {
    if !truth(&view)
        || !x.is_finite()
        || !y.is_finite()
        || x < 0.0
        || y < 0.0
        || x >= number(&view, "width")
        || y >= number(&view, "height")
    {
        return Err(error("Coordinates outside the latest Android screenshot; observe again"));
    }
    Ok(Array::of2(
        &((x * number(&view, "nativeWidth") / number(&view, "width")).round().min(number(&view, "nativeWidth") - 1.0))
            .into(),
        &((y * number(&view, "nativeHeight") / number(&view, "height"))
            .round()
            .min(number(&view, "nativeHeight") - 1.0))
        .into(),
    )
    .into())
}
fn text_result(value: JsValue) -> JsValue {
    obj(&[
        ("kind", "text".into()),
        ("text", if value.is_string() { value } else { js_sys::JSON::stringify(&value).unwrap().into() }),
    ])
}
#[wasm_bindgen]
#[derive(Clone)]
pub struct AndroidCommands {
    options: JsValue,
    view: Rc<RefCell<JsValue>>,
    sequence: Rc<Cell<u64>>,
}
#[wasm_bindgen]
impl AndroidCommands {
    #[wasm_bindgen(constructor)]
    pub fn new(options: JsValue) -> Self {
        Self { options, view: Rc::new(RefCell::new(JsValue::NULL)), sequence: Rc::new(Cell::new(1)) }
    }
    #[wasm_bindgen(getter)]
    pub fn view(&self) -> JsValue {
        self.view.borrow().clone()
    }
    #[wasm_bindgen(setter)]
    pub fn set_view(&self, value: JsValue) {
        *self.view.borrow_mut() = value;
    }
    #[wasm_bindgen(getter)]
    pub fn shell(&self) -> JsValue {
        get(&self.options, "shell")
    }
    #[wasm_bindgen(setter)]
    pub fn set_shell(&self, value: JsValue) {
        set(&self.options, "shell", &value);
    }
    #[wasm_bindgen(getter)]
    pub fn capture(&self) -> JsValue {
        get(&self.options, "capture")
    }
    #[wasm_bindgen(setter)]
    pub fn set_capture(&self, value: JsValue) {
        set(&self.options, "capture", &value);
    }
    #[wasm_bindgen(getter)]
    pub fn controller(&self) -> JsValue {
        get(&self.options, "controller")
    }
    #[wasm_bindgen(setter)]
    pub fn set_controller(&self, value: JsValue) {
        set(&self.options, "controller", &value);
    }
    pub fn screenshot(&self, signal: JsValue, region: JsValue) -> Promise {
        let this = self.clone();
        future_to_promise(async move { this.take_screenshot(signal, region).await })
    }
    pub fn nodes(&self, signal: JsValue, budget: Option<f64>) -> Promise {
        let this = self.clone();
        future_to_promise(async move { this.read_nodes(&signal, budget.unwrap_or(1800.0)).await })
    }
    #[wasm_bindgen(js_name=formatNodes)]
    pub fn format_nodes(&self, nodes: JsValue, limit: Option<u32>, filter: Option<String>) -> String {
        let view = self.view();
        let filter = filter.unwrap_or_default();
        Array::from(&nodes)
            .iter()
            .filter(|n| {
                (!text(n, "text").is_empty()
                    || !text(n, "label").is_empty()
                    || !text(n, "id").is_empty()
                    || truth(&get(n, "clickable"))
                    || truth(&get(n, "focusable")))
                    && format!("{} {} {} {}", text(n, "text"), text(n, "label"), text(n, "id"), text(n, "type"))
                        .contains(&filter)
            })
            .take(limit.unwrap_or(80) as usize)
            .map(|n| {
                let x = number(&n, "x");
                let y = number(&n, "y");
                let mut bounds = [x, y, number(&n, "right") - x, number(&n, "bottom") - y];
                if truth(&view) {
                    for (i, v) in bounds.iter_mut().enumerate() {
                        *v = (*v * number(&view, if i % 2 == 0 { "width" } else { "height" })
                            / number(&view, if i % 2 == 0 { "nativeWidth" } else { "nativeHeight" }))
                        .round();
                    }
                }
                let result = obj(&[
                    ("text", get(&n, "text")),
                    ("label", get(&n, "label")),
                    ("id", get(&n, "id")),
                    ("type", get(&n, "type")),
                    ("enabled", get(&n, "enabled")),
                    ("bounds", to_value(&bounds)),
                ]);
                js_sys::JSON::stringify(&result).unwrap().as_string().unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .take(48000)
            .collect()
    }
    pub fn geometry(&self, signal: JsValue) -> Promise {
        let this = self.clone();
        future_to_promise(async move {
            this.check_geometry(&signal).await?;
            Ok(JsValue::UNDEFINED)
        })
    }
    pub fn key(&self, code: u32, signal: JsValue) -> Promise {
        let this = self.clone();
        future_to_promise(async move {
            this.press_key(code, &signal).await?;
            Ok(JsValue::UNDEFINED)
        })
    }
    pub fn execute(&self, request: JsValue, signal: JsValue) -> Promise {
        let this = self.clone();
        future_to_promise(async move { this.execute_inner(request, signal).await })
    }
}
impl AndroidCommands {
    fn clock(&self) -> f64 {
        let f = get(&self.options, "now");
        if f.is_function() {
            apply(&f, &JsValue::UNDEFINED, &[]).ok().and_then(|v| v.as_f64()).unwrap_or(0.0)
        } else {
            now()
        }
    }
    async fn shell_command(&self, command: String, signal: &JsValue, budget: Option<f64>) -> Result<JsValue, JsValue> {
        let args = if let Some(b) = budget {
            vec![command.into(), signal.clone(), b.into()]
        } else {
            vec![command.into(), signal.clone()]
        };
        invoke(&self.options, "shell", &args).await
    }
    async fn delay(&self, ms: f64, signal: &JsValue) -> Result<(), JsValue> {
        abort_check(signal)?;
        if get(&self.options, "delay").is_function() {
            invoke(&self.options, "delay", &[ms.into(), signal.clone()]).await?;
        } else {
            resolved(timeout(ms).into()).await?;
        }
        abort_check(signal)
    }
    async fn take_screenshot(&self, signal: JsValue, region: JsValue) -> Result<JsValue, JsValue> {
        let value = invoke(&self.options, "capture", &[signal, region.clone(), self.view()]).await?;
        if !truth(&region) {
            self.set_view(get(&value, "view"));
        }
        Ok(value)
    }
    async fn read_nodes(&self, signal: &JsValue, budget: f64) -> Result<JsValue, JsValue> {
        let path = format!("/data/local/tmp/ictl-ui-{}.xml", uuid());
        let path = quote(path.into());
        let xml = self
            .shell_command(
                format!("uiautomator dump --compressed {path} >/dev/null && cat {path}; rm -f {path}"),
                signal,
                Some(budget),
            )
            .await?;
        parse_hierarchy(xml.as_string().unwrap_or_default())
    }
    async fn check_geometry(&self, signal: &JsValue) -> Result<(), JsValue> {
        let view = self.view();
        if !truth(&view) {
            return Err(error("Take a full screenshot before coordinate actions"));
        }
        let input = self.shell_command("dumpsys input".into(), signal, Some(3000.0)).await?;
        let viewport = input_viewport(input.as_string().unwrap_or_default());
        if !truth(&viewport)
            || number(&viewport, "width") != number(&view, "nativeWidth")
            || number(&viewport, "height") != number(&view, "nativeHeight")
            || get(&viewport, "orientation") != get(&view, "orientation")
        {
            return Err(error(
                "Android display size changed or cannot be verified; refresh the screenshot before acting",
            ));
        }
        Ok(())
    }
    async fn press_key(&self, code: u32, signal: &JsValue) -> Result<(), JsValue> {
        abort_check(signal)?;
        let controller = get(&self.options, "controller");
        if get(&controller, "injectKeyCode").is_function() {
            let key = json(serde_json::json!({"keyCode":code,"repeat":0,"metaState":0,"action":0}));
            let result = invoke(&controller, "injectKeyCode", std::slice::from_ref(&key))
                .await
                .and_then(|_| abort_check(signal).map(|_| JsValue::UNDEFINED));
            let release = copy(&key);
            set(&release, "action", &1.into());
            invoke(&controller, "injectKeyCode", &[release]).await?;
            result?;
            abort_check(signal)?;
        } else {
            self.shell_command(format!("input keyevent {code}"), signal, None).await?;
        }
        Ok(())
    }
    async fn execute_inner(&self, request: JsValue, signal: JsValue) -> Result<JsValue, JsValue> {
        abort_check(&signal)?;
        let op = text(&request, "op");
        match op.as_str() {
            "screenshot" => {
                let image = self.take_screenshot(signal, get(&request, "region")).await?;
                return Ok(obj(&[
                    ("kind", "image".into()),
                    ("jpeg_base64", get(&image, "imageBase64")),
                    ("caption", get(&image, "caption")),
                    ("coordinateMapping", get(&image, "coordinateMapping")),
                ]));
            }
            "observe" => {
                let start = self.clock();
                let this = self.clone();
                let sig = signal.clone();
                let budget = number(&request, "snapshot_timeout_ms");
                let tree = if get(&request, "limit").as_f64().unwrap_or(80.0) > 0.0 {
                    future_to_promise(async move {
                        let result =
                            this.read_nodes(&sig, if budget > 0.0 { budget.min(1800.0) } else { 1000.0 }).await;
                        let ms = (this.clock() - start).round();
                        Ok(match result {
                            Ok(nodes) => obj(&[("nodes", nodes), ("elementsMs", ms.into())]),
                            Err(_) => obj(&[
                                ("error", "UI hierarchy unavailable; use screenshot coordinates".into()),
                                ("elementsMs", ms.into()),
                            ]),
                        })
                    })
                } else {
                    Promise::resolve(&obj(&[("elementsMs", 0.into())]))
                };
                let image = self.take_screenshot(signal, JsValue::UNDEFINED).await?;
                let capture = (self.clock() - start).round();
                let tree = resolved(tree.into()).await?;
                let nodes = get(&tree, "nodes");
                let value = obj(&[
                    ("deviceId", get(&self.options, "deviceId")),
                    ("imageBase64", get(&image, "imageBase64")),
                    ("caption", get(&image, "caption")),
                    ("coordinateMapping", get(&image, "coordinateMapping")),
                    (
                        "elementsText",
                        if truth(&nodes) {
                            self.format_nodes(nodes, get(&request, "limit").as_f64().map(|v| v as u32), None).into()
                        } else {
                            "".into()
                        },
                    ),
                    (
                        "timings",
                        obj(&[
                            ("screenshotMs", capture.into()),
                            ("elementsMs", get(&tree, "elementsMs")),
                            ("sourceBytes", get(&image, "sourceBytes")),
                            ("sourceFormat", get(&image, "sourceFormat").as_string().unwrap_or("png".into()).into()),
                            ("uploadedBytes", get(&image, "uploadedBytes")),
                        ]),
                    ),
                ]);
                if truth(&get(&tree, "error")) {
                    set(&value, "elementsError", &get(&tree, "error"));
                }
                return Ok(text_result(value));
            }
            "status" => {
                return Ok(text_result(obj(&[
                    ("platform", "android".into()),
                    ("deviceId", get(&self.options, "deviceId")),
                    ("screen", self.view()),
                ])));
            }
            "list_devices" => {
                return Ok(text_result(
                    Array::of1(&obj(&[("platform", "android".into()), ("deviceId", get(&self.options, "deviceId"))]))
                        .into(),
                ));
            }
            "list_elements" => {
                let nodes = self.read_nodes(&signal, 5000.0).await?;
                return Ok(text_result(
                    self.format_nodes(
                        nodes,
                        get(&request, "limit").as_f64().map(|v| v as u32),
                        Some(text(&request, "filter")),
                    )
                    .into(),
                ));
            }
            "tap_element" | "wait_for" => {
                let target = text(&request, "text");
                if target.trim().is_empty() {
                    return Err(error("Element text must not be empty"));
                }
                let deadline = now() + get(&request, "timeout_ms").as_f64().unwrap_or(5000.0);
                loop {
                    let nodes = self.read_nodes(&signal, 5000.0).await?;
                    let found = Array::from(&nodes)
                        .iter()
                        .filter(|n| {
                            truth(&get(n, "enabled"))
                                && ["text", "label", "id"].iter().any(|k| text(n, k).contains(&target))
                        })
                        .nth(number(&request, "index") as usize);
                    if let Some(found) = found {
                        if op == "wait_for" {
                            return Ok(text_result(obj(&[
                                ("found", true.into()),
                                ("text", get(&found, "text")),
                                ("label", get(&found, "label")),
                            ])));
                        }
                        self.shell_command(
                            format!(
                                "input tap {} {}",
                                ((number(&found, "x") + number(&found, "right")) / 2.0).round(),
                                ((number(&found, "y") + number(&found, "bottom")) / 2.0).round()
                            ),
                            &signal,
                            None,
                        )
                        .await?;
                        self.delay(180.0, &signal).await?;
                        return Ok(text_result(obj(&[("tapped", true.into()), ("text", get(&found, "text"))])));
                    }
                    if op == "tap_element" {
                        let result =
                            text_result(json(serde_json::json!({"tapped":false,"reason":"No matching element"})));
                        set(&result, "outcome", &json(serde_json::json!({"tapped_nothing":true})));
                        return Ok(result);
                    }
                    if now() >= deadline {
                        break;
                    }
                    self.delay(250.0, &signal).await?;
                }
                return Ok(text_result(json(serde_json::json!({"found":false}))));
            }
            "tap" | "long_press" | "swipe" => {
                self.check_geometry(&signal).await?;
                let point = if op == "swipe" {
                    Array::from(&get(&request, "from"))
                } else {
                    Array::of2(&get(&request, "x"), &get(&request, "y"))
                };
                let from = Array::from(&map_point(
                    self.view(),
                    point.get(0).as_f64().unwrap_or(f64::NAN),
                    point.get(1).as_f64().unwrap_or(f64::NAN),
                )?);
                if op == "tap" {
                    self.shell_command(
                        format!("input tap {} {}", from.get(0).as_f64().unwrap(), from.get(1).as_f64().unwrap()),
                        &signal,
                        None,
                    )
                    .await?;
                } else {
                    let to = if op == "swipe" {
                        let point = Array::from(&get(&request, "to"));
                        Array::from(&map_point(
                            self.view(),
                            point.get(0).as_f64().unwrap_or(f64::NAN),
                            point.get(1).as_f64().unwrap_or(f64::NAN),
                        )?)
                    } else {
                        from.clone()
                    };
                    let ms = (get(&request, "seconds").as_f64().unwrap_or(if op == "swipe" { 0.3 } else { 1.0 })
                        * 1000.0)
                        .round()
                        .max(1.0);
                    self.shell_command(
                        format!(
                            "input swipe {} {} {} {} {ms}",
                            from.get(0).as_f64().unwrap(),
                            from.get(1).as_f64().unwrap(),
                            to.get(0).as_f64().unwrap(),
                            to.get(1).as_f64().unwrap()
                        ),
                        &signal,
                        Some(ms + 3000.0),
                    )
                    .await?;
                }
            }
            "type_text" => {
                let controller = get(&self.options, "controller");
                if !truth(&controller) {
                    return Err(error("Android Unicode input is not ready"));
                }
                abort_check(&signal)?;
                let seq = self.sequence.get();
                self.sequence.set(seq + 1);
                invoke(
                    &controller,
                    "setClipboard",
                    &[obj(&[
                        ("sequence", js_sys::BigInt::from(seq).into()),
                        ("content", get(&request, "text")),
                        ("paste", true.into()),
                    ])],
                )
                .await?;
                abort_check(&signal)?;
                if truth(&get(&request, "submit")) {
                    self.press_key(66, &signal).await?;
                }
            }
            "press_button" => {
                let code = match text(&request, "button").as_str() {
                    "home" => 3,
                    "back" => 4,
                    "volume_up" => 24,
                    "volume_down" => 25,
                    "lock" => 223,
                    _ => return Err(error("Unsupported Android button")),
                };
                self.press_key(code, &signal).await?;
            }
            "list_apps" => {
                return Ok(text_result(
                    self.shell_command(
                        format!("pm list packages {}", if truth(&get(&request, "include_system")) { "" } else { "-3" }),
                        &signal,
                        Some(5000.0),
                    )
                    .await?,
                ));
            }
            "launch_app" => {
                let app = text(&request, "app");
                if !android_text::package_id(&app) {
                    return Err(error("Use an installed Android package ID from list_apps"));
                }
                let resolve_command = format!(
                    "cmd package resolve-activity --brief -a android.intent.action.MAIN -c android.intent.category.LAUNCHER {}",
                    quote(app.into())
                );
                let resolved = self.shell_command(resolve_command, &signal, Some(5000.0)).await?;
                let resolved = resolved.as_string().unwrap_or_default();
                let component = resolved.trim().lines().last().unwrap_or("");
                if !android_text::component(component) {
                    return Err(error("App has no launchable activity"));
                }
                let result = self
                    .shell_command(format!("am start -W -n {}", quote(component.into())), &signal, Some(10000.0))
                    .await?;
                if launch_failed(&result.as_string().unwrap_or_default()) {
                    return Err(error("Android could not launch this app"));
                }
            }
            "terminate_app" => {
                let app = text(&request, "app");
                if !android_text::loose_package(&app) {
                    return Err(error("Invalid Android package ID"));
                }
                self.shell_command(format!("am force-stop {}", quote(app.into())), &signal, None).await?;
            }
            "open_url" => {
                let url = text(&request, "url");
                let scheme = url.split_once(':').map(|(scheme, _)| scheme.to_ascii_lowercase());
                if !scheme.as_deref().is_some_and(|scheme| ["http", "https", "tel", "sms", "mailto"].contains(&scheme))
                {
                    return Err(error("Unsupported URL scheme"));
                }
                let result = self
                    .shell_command(
                        format!("am start -W -a android.intent.action.VIEW -d {}", quote(url.into())),
                        &signal,
                        Some(10000.0),
                    )
                    .await?;
                if android_text::reports_error(&result.as_string().unwrap_or_default()) {
                    return Err(error("Android could not open this URL"));
                }
            }
            _ => return Err(error(format!("Unsupported Android operation: {op}"))),
        }
        self.delay(180.0, &signal).await?;
        Ok(text_result(json(serde_json::json!({"ok":true}))))
    }
}
fn launch_failed(output: &str) -> bool {
    android_text::reports_error(output)
        || output.lines().filter_map(|l| l.trim().strip_prefix("Status:")).any(|s| s.trim() != "ok")
}
