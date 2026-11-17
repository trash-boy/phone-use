use crate::{
    commands::command_request,
    error::{ApiError, Result},
};
use base64::Engine;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// What the planner sees for each phone observation (`AI_OBSERVATION`, default `image`).
/// Measured on the 2-core VPS, image steps were faster than OCR steps (8.8 s vs 10.4 s).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservationMode {
    /// Screenshot, UI element text and reference images, without OCR.
    Image,
    /// Image mode plus PaddleOCR text and phone-point polygons. OCR failures
    /// degrade to image mode for that observation.
    Hybrid,
    /// Text-only OCR evidence. No image or UI tree leaves the gateway.
    Ocr,
}
impl ObservationMode {
    fn parse(value: &str) -> anyhow::Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "image" => Ok(Self::Image),
            "hybrid" => Ok(Self::Hybrid),
            "ocr" => Ok(Self::Ocr),
            _ => anyhow::bail!("AI_OBSERVATION must be image, hybrid or ocr"),
        }
    }
    fn system_prompt(self) -> String {
        let lead = match self {
            Self::Image => include_str!("model-prompt-image.txt").to_owned(),
            Self::Hybrid => {
                format!("{}{}", include_str!("model-prompt-image.txt"), include_str!("model-prompt-hybrid.txt"))
            }
            Self::Ocr => include_str!("model-prompt-ocr.txt").to_owned(),
        };
        lead + include_str!("model-prompt-common.txt")
    }
}

#[derive(Clone)]
pub struct Model {
    client: reqwest::Client,
    endpoint: Option<reqwest::Url>,
    pub name: Option<String>,
    key: String,
    /// Pinned at construction and retained by task-local model selections.
    cc: bool,
    /// Which host bridge serves `cc` requests: `cc` (Claude Code) or `codex` (Codex agent).
    bridge: &'static str,
    cc_models: Vec<String>,
    catalog: Arc<Mutex<Catalog>>,
    format_refused: Arc<Mutex<HashSet<String>>>,
    /// `AI_REASONING_EFFORT` (default `low`, `off` to omit): shorter hidden reasoning
    /// means faster steps on reasoning models.
    reasoning_effort: Option<String>,
    reasoning_refused: Arc<Mutex<HashSet<String>>>,
    ocr: Arc<crate::ocr::Ocr>,
    observation_mode: ObservationMode,
    /// `/api/config` answer, shared by concurrent callers (single flight) for `CONFIG_TTL`.
    config: Arc<Mutex<Option<(Instant, Value)>>>,
}
/// `/api/config` is public (the page reads it before connecting), so a burst of
/// requests reuses one bridge health probe or provider catalog fetch.
const CONFIG_TTL: Duration = Duration::from_secs(10);
#[derive(Default)]
struct Catalog {
    fetched: Option<Instant>,
    names: Vec<String>,
    failed: bool,
}
struct RequestDiagnostics<'a> {
    observation_id: &'a str,
    attempt: u8,
    provider_attempt: u8,
    started: Instant,
    request_bytes: usize,
    response_bytes: usize,
    response_headers_ms: Option<u128>,
    first_body_chunk_ms: Option<u128>,
    http_status: Option<u16>,
    finish_reason: Option<&'static str>,
    status: &'static str,
}
impl RequestDiagnostics<'_> {
    fn snapshot(&self) -> Value {
        json!({"observation_id":self.observation_id,"attempt":self.attempt,"provider_attempt":self.provider_attempt,
            "request_bytes":self.request_bytes,"response_bytes":self.response_bytes,
            "response_headers_ms":self.response_headers_ms,"first_body_chunk_ms":self.first_body_chunk_ms,
            "total_ms":self.started.elapsed().as_millis(),"http_status":self.http_status,
            "finish_reason":self.finish_reason,"status":self.status})
    }
}
impl Drop for RequestDiagnostics<'_> {
    fn drop(&mut self) {
        eprintln!("event=model_completion metrics={}", self.snapshot());
    }
}

impl Model {
    pub fn new(env: &HashMap<String, String>) -> anyhow::Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let get = |k: &str| env.get(k).cloned().unwrap_or_default().trim().to_owned();
        let runtime = PathBuf::from(env.get("RUNTIME_DIR").map(String::as_str).unwrap_or(".runtime"));
        let backend = get("LLM_BACKEND");
        let codex = backend == "codex";
        let socket_override = get(if codex { "CODEX_SOCKET" } else { "CC_SOCKET" });
        let socket = if socket_override.is_empty() {
            runtime.join(if codex { "codex/bridge.sock" } else { "cc/bridge.sock" })
        } else {
            let path = PathBuf::from(&socket_override);
            anyhow::ensure!(
                path.is_absolute() && !socket_override.chars().any(char::is_control),
                "CC_SOCKET and CODEX_SOCKET must be absolute socket paths"
            );
            path
        };
        let cc = match backend.as_str() {
            "" => socket.exists(),
            "cc" | "codex" => true,
            "openai" => false,
            _ => anyhow::bail!("LLM_BACKEND must be openai, cc or codex"),
        };
        let mut cc_models = Vec::new();
        let (name, key, base) = if codex {
            // `default` lets the Codex CLI use its own configured model and provider.
            for name in env.get("CODEX_MODELS").map(String::as_str).unwrap_or("default").split(',') {
                let name = name.trim();
                anyhow::ensure!(valid_model_id(name), "Invalid CODEX_MODELS configuration");
                if !cc_models.iter().any(|existing| existing == name) {
                    cc_models.push(name.to_owned());
                }
            }
            let name =
                env.get("CODEX_MODEL").map(|name| name.trim().to_owned()).unwrap_or_else(|| cc_models[0].clone());
            anyhow::ensure!(valid_model_id(&name) && cc_models.contains(&name), "CODEX_MODEL must be in CODEX_MODELS");
            (name, String::new(), String::new())
        } else if cc {
            for name in env.get("CC_MODELS").map(String::as_str).unwrap_or("opus,sonnet,haiku").split(',') {
                let name = name.trim();
                anyhow::ensure!(valid_model_id(name), "Invalid CC_MODELS configuration");
                if !cc_models.iter().any(|existing| existing == name) {
                    cc_models.push(name.to_owned());
                }
            }
            // One action per step: a faster default model shortens every step of a task.
            let name = env.get("CC_MODEL").map(|name| name.trim()).unwrap_or("sonnet").to_owned();
            anyhow::ensure!(valid_model_id(&name) && cc_models.contains(&name), "CC_MODEL must be in CC_MODELS");
            (name, String::new(), String::new())
        } else {
            (get("LLM_MODEL"), get("LLM_API_KEY"), get("LLM_BASE_URL"))
        };
        let ready = cc || (!base.is_empty() && !name.is_empty() && !key.is_empty());
        let endpoint = if cc {
            Some(reqwest::Url::parse("http://localhost/plan")?)
        } else if ready {
            let url = reqwest::Url::parse(&(base.trim_end_matches('/').to_owned() + "/chat/completions"))?;
            anyhow::ensure!(
                ["http", "https"].contains(&url.scheme())
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && valid_model_id(&name)
                    && !key.contains(['\r', '\n', '\0']),
                "Invalid model configuration"
            );
            Some(url)
        } else {
            None
        };
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(if cc { 95 } else { 60 }));
        let client = if cc { client.unix_socket(socket).no_proxy() } else { client }.build()?;
        Ok(Self {
            client,
            endpoint,
            name: ready.then_some(name),
            key,
            cc,
            bridge: if codex { "codex" } else { "cc" },
            cc_models,
            catalog: Arc::default(),
            format_refused: Arc::default(),
            reasoning_effort: match get("AI_REASONING_EFFORT").to_ascii_lowercase().as_str() {
                "" => Some("low".to_owned()),
                "off" | "none" => None,
                effort if effort.len() <= 16 && effort.bytes().all(|b| b.is_ascii_lowercase()) => {
                    Some(effort.to_owned())
                }
                _ => anyhow::bail!("AI_REASONING_EFFORT must be a lowercase effort name or off"),
            },
            reasoning_refused: Arc::default(),
            ocr: Arc::new(crate::ocr::Ocr::new(env)),
            observation_mode: ObservationMode::parse(&get("AI_OBSERVATION"))?,
            config: Arc::default(),
        })
    }
    pub fn observation_mode(&self) -> ObservationMode {
        self.observation_mode
    }
    /// Text-only OCR planning deliberately omits the UI tree; other modes use it.
    pub fn wants_elements(&self) -> bool {
        self.observation_mode != ObservationMode::Ocr
    }
    /// Loads the OCR model in the background so the first task step does not
    /// pay the cold start. Requires a Tokio runtime.
    pub fn warm_ocr(&self) {
        if self.ready() && self.observation_mode != ObservationMode::Image {
            let ocr = self.ocr.clone();
            tokio::spawn(async move { ocr.warm().await });
        }
    }
    pub fn ready(&self) -> bool {
        self.endpoint.is_some()
    }
    async fn model_names(&self) -> (Vec<String>, bool) {
        if self.cc {
            return (self.cc_models.clone(), false);
        }
        let mut catalog = self.catalog.lock().await;
        let ttl = if catalog.failed { 15 } else { 60 };
        if catalog.fetched.is_none_or(|at| at.elapsed() >= Duration::from_secs(ttl)) {
            let fetch = async {
                let endpoint = self.endpoint.as_ref()?.as_str().strip_suffix("chat/completions")?;
                let mut response =
                    self.client.get(format!("{endpoint}models")).bearer_auth(&self.key).send().await.ok()?;
                if !response.status().is_success() || response.content_length().is_some_and(|n| n > 512 * 1024) {
                    return None;
                }
                let mut bytes = Vec::new();
                while let Some(chunk) = response.chunk().await.ok()? {
                    if bytes.len() + chunk.len() > 512 * 1024 {
                        return None;
                    }
                    bytes.extend_from_slice(&chunk);
                }
                let value: Value = serde_json::from_slice(&bytes).ok()?;
                let data = value["data"].as_array()?;
                if data.len() > 4096 {
                    return None;
                }
                let mut names: Vec<_> = data
                    .iter()
                    .filter_map(|v| v["id"].as_str())
                    .filter(|id| valid_model_id(id))
                    .map(str::to_owned)
                    .collect();
                names.sort();
                names.dedup();
                Some(names)
            };
            match tokio::time::timeout(Duration::from_secs(5), fetch).await {
                Ok(Some(names)) => {
                    catalog.names = names;
                    catalog.failed = false;
                }
                _ => catalog.failed = true,
            }
            catalog.fetched = Some(Instant::now());
        }
        let mut names = catalog.names.clone();
        if let Some(default) = &self.name
            && !names.contains(default)
        {
            names.insert(0, default.clone());
        }
        (names, catalog.failed)
    }
    pub async fn configuration(&self) -> Value {
        // Holding the lock while probing makes concurrent callers wait for one probe.
        let mut cached = self.config.lock().await;
        if let Some((at, config)) = cached.as_ref()
            && at.elapsed() < CONFIG_TTL
        {
            return config.clone();
        }
        let config = self.probe_configuration().await;
        *cached = Some((Instant::now(), config.clone()));
        config
    }
    async fn probe_configuration(&self) -> Value {
        if self.cc {
            let health = async {
                let response = self
                    .client
                    .get("http://localhost/health")
                    .send()
                    .await
                    .map_err(|_| ApiError::unavailable("CC bridge is unavailable"))?;
                let bytes = bounded_cc_body(response, 4096).await?;
                let value: Value = serde_json::from_slice(&bytes)
                    .map_err(|_| ApiError::unavailable("CC bridge returned invalid health"))?;
                Ok::<_, ApiError>(value["ok"] == true)
            };
            let ready = matches!(tokio::time::timeout(Duration::from_secs(2), health).await, Ok(Ok(true)));
            let mut config = json!({"backend":self.bridge,"aiReady":ready,"model":self.name,"models":self.cc_models});
            if !ready {
                config["aiError"] = json!(if self.bridge == "codex" {
                    "Codex bridge is unavailable"
                } else {
                    "CC bridge is unavailable"
                });
            }
            return config;
        }
        if !self.ready() {
            return json!({"backend":"openai","aiReady":false,"model":null});
        }
        let (models, failed) = self.model_names().await;
        let mut config = json!({"backend":"openai","aiReady":true,"model":self.name,"models":models});
        if failed {
            config["modelsError"] = json!("模型列表暂时无法更新，显示默认模型或上次列表。");
        }
        config
    }
    pub async fn select(&self, requested: Option<&Value>) -> Result<Self> {
        if !self.ready() {
            return Err(ApiError::unavailable("AI is not configured"));
        }
        let Some(requested) = requested else {
            return Ok(self.clone());
        };
        let name = requested
            .as_str()
            .filter(|s| valid_model_id(s))
            .ok_or_else(|| ApiError::new(400, "Invalid model selection"))?;
        if self.name.as_deref() != Some(name) && !self.model_names().await.0.iter().any(|s| s == name) {
            return Err(ApiError::new(400, "Model is unavailable; refresh the model list"));
        }
        let mut selected = self.clone();
        selected.name = Some(name.to_owned());
        Ok(selected)
    }
    /// Convenience entry point that plans from an undecoded observation with no
    /// bridge follow-up (tests and simple callers).
    pub async fn generate(
        &self,
        instruction: &str,
        observation: Value,
        history: Vec<Value>,
        images: &[crate::attachments::ReferenceImage],
        cancel: CancellationToken,
    ) -> Result<Value> {
        let observation = Observation::from_value(observation);
        let request =
            PlanRequest { instruction, observation: &observation, history, follow_up: None, images, session: None };
        self.plan(request, cancel).await
    }

    /// OCR evidence for an observation in hybrid and OCR modes, `None` in image
    /// mode or without a phone image. Runs before planning on its own OCR
    /// deadline, so a cold OCR start never consumes the model's time budget.
    pub async fn recognize(&self, observation: &Observation, cancel: CancellationToken) -> Option<Result<Value>> {
        if self.observation_mode == ObservationMode::Image {
            return None;
        }
        let image = observation.value["imageBase64"].as_str()?;
        Some(self.ocr.recognize(image, observation.jpeg.as_deref(), Some(&observation.value), cancel).await)
    }

    /// Time one planning call may take once its evidence is ready, including the
    /// single correction retry. The bridge receives what is left as `deadlineMs`.
    fn model_budget(&self) -> Duration {
        if self.cc { CC_MODEL_BUDGET } else { OPENAI_MODEL_BUDGET }
    }

    pub async fn plan(&self, request: PlanRequest<'_>, cancel: CancellationToken) -> Result<Value> {
        let endpoint = self.endpoint.clone().ok_or_else(|| ApiError::unavailable("AI is not configured"))?;
        let observation = request.observation;
        let observation_id = observation.value["observationId"].as_str().unwrap_or("").to_owned();
        // Evidence first (OCR has its own deadline); the model budget starts afterwards.
        let evidence = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ApiError::unavailable("Model request cancelled")),
            evidence = self.evidence(observation, &observation_id, request.images, cancel.clone()) => evidence?,
        };
        let deadline = Instant::now() + self.model_budget();
        let content = evidence.content(request.instruction, &request.history);
        let operation = async {
            if self.cc {
                let follow_up = request.follow_up.as_ref().map(|follow_up| evidence.follow_up(follow_up));
                self.bridge_plan(endpoint, content, follow_up, request.session, deadline, &observation_id).await
            } else {
                self.openai_plan(&endpoint, content, &observation_id).await
            }
        };
        // Dropping the operation closes an in-flight bridge request; the bridge
        // observes the disconnect and terminates its planner.
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ApiError::unavailable("Model request cancelled")),
            result = tokio::time::timeout_at(deadline.into(), operation) => {
                result.unwrap_or_else(|_| Err(ApiError::unavailable("Model request timed out")))
            }
        }
    }

    async fn openai_plan(&self, endpoint: &reqwest::Url, content: Vec<Value>, observation_id: &str) -> Result<Value> {
        let system_prompt = self.observation_mode.system_prompt();
        let messages = Mutex::new(vec![
            json!({"role":"system","content":system_prompt}),
            json!({"role":"user","content":content}),
        ]);
        with_correction(observation_id, |correction| {
            let messages = &messages;
            async move {
                let mut messages = messages.lock().await;
                let (max_tokens, attempt) = match correction {
                    Some(correction) => {
                        if let Some(text) = correction.previous {
                            messages.push(json!({"role":"assistant","content":text}));
                        }
                        messages.push(json!({"role":"user","content":correction.message}));
                        (2048, 2)
                    }
                    None => (1024, 1),
                };
                self.completion(endpoint, &messages, max_tokens, attempt, observation_id).await.map(RawReply::Choice)
            }
        })
        .await
    }

    async fn bridge_plan(
        &self,
        endpoint: reqwest::Url,
        content: Vec<Value>,
        follow_up: Option<Vec<Value>>,
        session: Option<&str>,
        deadline: Instant,
        observation_id: &str,
    ) -> Result<Value> {
        let schema: Value = serde_json::from_str(include_str!("model-reply.schema.json"))
            .map_err(|_| ApiError::unavailable("Invalid model reply schema"))?;
        let system = format!(
            "{}\nRespond only with a JSON object matching this schema: {schema}",
            self.observation_mode.system_prompt()
        );
        with_correction(observation_id, |correction| {
            let (endpoint, system) = (endpoint.clone(), &system);
            let (mut content, mut follow_up) = (content.clone(), follow_up.clone());
            async move {
                if let Some(correction) = correction {
                    // A fresh thread needs the whole step plus the rejected reply; an
                    // existing thread already holds both and only needs the correction.
                    let mut blocks = Vec::new();
                    if let Some(previous) = &correction.previous {
                        blocks.push(json!({"type":"text","text":format!("Your previous reply was:\n{previous}")}));
                    }
                    blocks.push(json!({"type":"text","text":correction.message}));
                    content.extend(blocks.iter().cloned());
                    follow_up = follow_up.map(|_| blocks);
                }
                let mut body = json!({"model":self.name,"system":system,"content":content,"session":session,
                    "deadlineMs":deadline.saturating_duration_since(Instant::now()).as_millis() as u64});
                if let Some(follow_up) = follow_up {
                    body["followUp"] = json!(follow_up);
                }
                let response = self
                    .client
                    .post(endpoint)
                    .json(&body)
                    .send()
                    .await
                    .map_err(|_| ApiError::unavailable("CC bridge request failed"))?;
                let bytes = bounded_cc_body(response, 1024 * 1024).await?;
                let envelope: Value = serde_json::from_slice(&bytes)
                    .map_err(|_| ApiError::unavailable("CC bridge returned an invalid JSON envelope"))?;
                envelope
                    .get("reply")
                    .cloned()
                    .map(RawReply::Bridge)
                    .ok_or_else(|| ApiError::unavailable("CC bridge response is missing reply"))
            }
        })
        .await
    }

    /// Tells the bridge a gateway session ended so it can drop that session's
    /// agent thread. Best effort: detached, short timeout, errors and 404 ignored.
    pub fn close_session(&self, session: &str) {
        if !self.cc {
            return;
        }
        let (client, body) = (self.client.clone(), json!({"session":session}));
        tokio::spawn(async move {
            let request = client.post("http://localhost/session/close").json(&body).send();
            let _ = tokio::time::timeout(Duration::from_secs(2), request).await;
        });
    }

    /// The observation evidence shared by full content and bridge follow-ups.
    async fn evidence(
        &self,
        observation: &Observation,
        observation_id: &str,
        images: &[crate::attachments::ReferenceImage],
        cancel: CancellationToken,
    ) -> Result<Evidence> {
        let value = &observation.value;
        let mut evidence = json!({
            "observationId":observation_id,
            "caption":truncate(value["caption"].as_str().unwrap_or(""),1000),
        });
        if self.observation_mode != ObservationMode::Ocr {
            evidence["elementsText"] = json!(truncate(value["elementsText"].as_str().unwrap_or(""), 16384));
        }
        evidence["queryOnly"] = json!(value["queryOnly"] == true);
        let image = value["imageBase64"].as_str();
        let ocr = match &observation.ocr {
            Some(ocr) => Some(ocr.clone()),
            None => self.recognize(observation, cancel.clone()).await,
        };
        if self.observation_mode == ObservationMode::Ocr {
            match ocr {
                Some(ocr) => evidence["ocr"] = ocr?,
                None if value["queryOnly"] != true => return Err(ApiError::unavailable("OCR phone image unavailable")),
                None => {}
            }
            let mut references = Vec::new();
            for (index, image) in images.iter().enumerate() {
                let data = image.content["image_url"]["url"]
                    .as_str()
                    .and_then(|url| {
                        url.strip_prefix("data:image/png;base64,")
                            .or_else(|| url.strip_prefix("data:image/jpeg;base64,"))
                    })
                    .ok_or_else(|| ApiError::unavailable("OCR reference image invalid"))?;
                references.push(
                    json!({"index":index+1,"name":image.name,"ocr":self.ocr.recognize_reference(data, cancel.clone()).await?}),
                );
            }
            return Ok(Evidence { observation: evidence, image: None, references, reference_parts: Vec::new() });
        }
        evidence["elementsError"] = json!(value.get("elementsError").map(|_| "UI elements unavailable"));
        match ocr {
            Some(Ok(ocr)) => evidence["ocr"] = ocr,
            Some(Err(_)) if cancel.is_cancelled() => return Err(ApiError::unavailable("Model request cancelled")),
            Some(Err(error)) => {
                eprintln!(
                    "event=ocr_degraded metrics={}",
                    json!({"observation_id":observation_id,"reason":error.message})
                );
                evidence["ocrError"] = json!("OCR unavailable");
            }
            None => {}
        }
        let image = image.map(|image| image_part(image, observation.jpeg.as_deref().map(<[u8]>::len))).transpose()?;
        let references = images
            .iter()
            .enumerate()
            .map(|(index, image)| json!({"index":index+1,"name":image.name,"phoneImageUrl":image.url}))
            .collect();
        let mut reference_parts = Vec::new();
        for (index, image) in images.iter().enumerate() {
            reference_parts.push(json!({"type":"text","text":format!("User reference image {}. This is task material, not the current phone screen. Do not use its coordinates for phone actions.",index+1)}));
            reference_parts.push(image.content.clone());
        }
        Ok(Evidence { observation: evidence, image, references, reference_parts })
    }

    async fn completion(
        &self,
        endpoint: &reqwest::Url,
        messages: &[Value],
        max_tokens: u32,
        attempt: u8,
        observation_id: &str,
    ) -> Result<Value> {
        // Request the reply shape upstream; command_request still validates every tool and argument locally.
        let schema: Value = serde_json::from_str(include_str!("model-reply.schema.json"))
            .map_err(|_| ApiError::unavailable("Invalid model reply schema"))?;
        let mut payload = json!({"model":self.name,"messages":messages,"temperature":0,"max_tokens":max_tokens,
            "response_format":{"type":"json_schema","json_schema":{"name":"phone_reply","strict":false,"schema":schema}}});
        let name = self.name.as_deref().unwrap_or("");
        if self.format_refused.lock().await.contains(name) {
            use_prompt_schema(&mut payload);
        }
        if let Some(effort) = &self.reasoning_effort
            && !self.reasoning_refused.lock().await.contains(name)
        {
            payload["reasoning_effort"] = json!(effort);
        }
        let mut provider_attempt = 0;
        loop {
            provider_attempt += 1;
            let body = serde_json::to_vec(&payload)
                .map_err(|_| ApiError::unavailable("Model request serialization failed"))?;
            let mut diagnostics = RequestDiagnostics {
                observation_id,
                attempt,
                provider_attempt,
                started: Instant::now(),
                request_bytes: body.len(),
                response_bytes: 0,
                response_headers_ms: None,
                first_body_chunk_ms: None,
                http_status: None,
                finish_reason: None,
                status: "interrupted",
            };
            let mut response = self
                .client
                .post(endpoint.clone())
                .bearer_auth(&self.key)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await
                .map_err(|_| {
                    diagnostics.status = "transport_error";
                    ApiError::unavailable("Model provider request failed")
                })?;
            diagnostics.response_headers_ms = Some(diagnostics.started.elapsed().as_millis());
            let status = response.status();
            diagnostics.http_status = Some(status.as_u16());
            let can_fallback = matches!(status.as_u16(), 400 | 422)
                && (payload.get("response_format").is_some() || payload.get("reasoning_effort").is_some());
            if !status.is_success() && !can_fallback {
                diagnostics.status = "http_error";
                return Err(ApiError::unavailable(format!(
                    "模型服务返回 HTTP {}，请检查所选模型的权限、额度和可用性。",
                    status.as_u16()
                )));
            }
            if response.content_length().is_some_and(|n| n > 65536) {
                diagnostics.status = "response_too_large";
                return Err(ApiError::unavailable("Model response too large"));
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| {
                diagnostics.status = "body_error";
                ApiError::unavailable("Model provider request failed")
            })? {
                diagnostics.response_bytes += chunk.len();
                if !chunk.is_empty() && diagnostics.first_body_chunk_ms.is_none() {
                    diagnostics.first_body_chunk_ms = Some(diagnostics.started.elapsed().as_millis());
                }
                if diagnostics.response_bytes > 65536 {
                    diagnostics.status = "response_too_large";
                    return Err(ApiError::unavailable("Model response too large"));
                }
                bytes.extend_from_slice(&chunk);
            }
            if !status.is_success() {
                let body = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
                if payload.get("response_format").is_some()
                    && (body.contains("response_format") || body.contains("json_schema"))
                {
                    self.format_refused.lock().await.insert(name.to_owned());
                    use_prompt_schema(&mut payload);
                    diagnostics.status = "format_refused";
                    continue;
                }
                // A provider or model without reasoning controls: retry without the field.
                if body.contains("reasoning") && payload.as_object_mut().unwrap().remove("reasoning_effort").is_some() {
                    self.reasoning_refused.lock().await.insert(name.to_owned());
                    diagnostics.status = "reasoning_refused";
                    continue;
                }
                diagnostics.status = "http_error";
                return Err(ApiError::unavailable(format!(
                    "模型服务返回 HTTP {}，请检查所选模型的权限、额度和可用性。",
                    status.as_u16()
                )));
            }
            let envelope: Value = serde_json::from_slice(&bytes).map_err(|_| {
                diagnostics.status = "envelope_error";
                ApiError::unavailable("Model provider returned an invalid JSON envelope")
            })?;
            let choice = envelope["choices"].as_array().and_then(|choices| choices.first()).ok_or_else(|| {
                diagnostics.status = "missing_choice";
                ApiError::unavailable("Model provider response is missing choices[0]")
            })?;
            diagnostics.finish_reason = Some(match choice["finish_reason"].as_str() {
                Some("stop") => "stop",
                Some("length") => "length",
                Some("content_filter") => "content_filter",
                Some("tool_calls") => "tool_calls",
                Some("function_call") => "function_call",
                Some(_) => "unknown",
                None => "missing",
            });
            if choice["finish_reason"] == "content_filter"
                || choice["message"]["refusal"].as_str().is_some_and(|s| !s.is_empty())
            {
                diagnostics.status = "provider_refusal";
                return Err(ApiError::unavailable("模型拒绝生成本次操作，请调整任务内容。"));
            }
            diagnostics.status = "complete";
            return Ok(choice.clone());
        }
    }
}

/// Never expose bridge response bodies or transport details to API consumers.
async fn bounded_cc_body(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    if !response.status().is_success() {
        return Err(ApiError::unavailable(format!("CC bridge returned HTTP {}", response.status().as_u16())));
    }
    if response.content_length().is_some_and(|length| length > limit as u64) {
        return Err(ApiError::unavailable("CC bridge response too large"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ApiError::unavailable("CC bridge request failed"))? {
        if chunk.len() > limit - bytes.len() {
            return Err(ApiError::unavailable("CC bridge response too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

// Deliberately names no concrete tool. A concrete example (formerly
// iphone_screenshot) was copied by models on retry, producing screenshot loops.
fn correction_message(error: &str) -> String {
    format!("Your previous reply failed validation: {error}. {CORRECTION_INSTRUCTIONS}")
}
const CORRECTION_INSTRUCTIONS: &str = "Return exactly one complete JSON object using the response schema and nothing after it. If your previous reply contained several separate objects, return only the first action you intended, or combine at most 4 predictable actions into one plan object. Keep the tool and arguments you intended when they were valid; otherwise choose the actual tool and arguments from the original tool catalog. An action has type \"action\", a string tool name from the catalog, object args, a string reason and a boolean requiresConfirmation. A plan has type \"plan\" and an actions array of 2 to 4 such actions without type fields. For completion or inability to proceed, use type \"done\" with a string message. Preserve the original task, safety and confirmation requirements. Do not include markdown or extra text.";

fn decode_logged_reply(choice: &Value, attempt: u8, observation_id: &str) -> Result<Value> {
    let mut decoded = DecodeDiagnostics::default();
    let result = decode_reply_with(choice, &mut decoded);
    let category = protocol_decode_category(&result);
    // Content-free metrics only: reply text may contain phone or task data.
    eprintln!(
        "event=model_protocol metrics={}",
        json!({"observation_id":observation_id,"attempt":attempt,"decode_category":category,
            "reply_chars":decoded.reply_chars,"json_error":decoded.json_error,
            "trailing_kind":decoded.trailing_kind,"trailing_bytes":decoded.trailing_bytes,
            "leading":decoded.leading,"fenced":decoded.fenced,"repair":decoded.repair})
    );
    result
}
fn protocol_decode_category(result: &Result<Value>) -> &'static str {
    match result {
        Ok(action) if action["type"] == "done" => "done",
        Ok(action) if action["type"] == "plan" => "plan",
        Ok(_) => "action",
        Err(error) if error.message.contains("response was truncated") => "truncated",
        Err(error) if error.message.contains("message.content") => "content_shape",
        Err(error) if error.message.contains("invalid JSON") || error.message.contains("code fence") => "json_syntax",
        Err(_) => "action_schema",
    }
}

fn use_prompt_schema(payload: &mut Value) {
    let format = payload.as_object_mut().unwrap().remove("response_format").unwrap();
    payload["messages"][0]["content"] = json!(format!(
        "{}\nRespond only with a JSON object matching this schema: {}",
        payload["messages"][0]["content"].as_str().unwrap(),
        format["json_schema"]["schema"]
    ));
}
fn valid_model_id(name: &str) -> bool {
    !name.is_empty() && name.len() <= 256 && name.trim() == name && !name.chars().any(char::is_control)
}
/// Base64 text of `image` decodes to this many bytes (without decoding it).
fn decoded_len(image: &str) -> usize {
    let padding = image.bytes().rev().take(2).filter(|b| *b == b'=').count();
    (image.len() / 4 * 3 + [0, 0, 1, 2][image.len() % 4]).saturating_sub(padding)
}
/// `decoded` is the size of the already decoded frame; without it (the frame did
/// not decode) the image is rejected rather than sent as an unreadable payload.
fn image_part(image: &str, decoded: Option<usize>) -> Result<Value> {
    let decoded = decoded.unwrap_or_else(|| {
        if base64::engine::general_purpose::STANDARD.decode(image).is_ok() { decoded_len(image) } else { usize::MAX }
    });
    if image.len() > 8 * 1024 * 1024 || decoded > 6 * 1024 * 1024 {
        return Err(ApiError::unavailable("Phone image exceeds model limit"));
    }
    Ok(json!({"type":"image_url","image_url":{"url":format!("data:image/jpeg;base64,{image}")}}))
}

/// Model-backend reply before decoding: an OpenAI-compatible choice, or the
/// `reply` value of a host bridge.
enum RawReply {
    Choice(Value),
    Bridge(Value),
}
/// What a correction attempt tells the model about its rejected first reply.
struct Correction {
    previous: Option<String>,
    message: String,
}
impl RawReply {
    fn decode(&self, attempt: u8, observation_id: &str) -> Result<Value> {
        match self {
            Self::Choice(choice) => decode_logged_reply(choice, attempt, observation_id),
            Self::Bridge(reply) => {
                let result = validate_reply(reply.clone());
                eprintln!(
                    "event=model_protocol metrics={}",
                    json!({"observation_id":observation_id,"attempt":attempt,"backend":"bridge",
                        "decode_category":protocol_decode_category(&result)})
                );
                result
            }
        }
    }
    fn text(&self) -> Option<String> {
        let text = match self {
            Self::Choice(choice) => reply_text(choice).ok()?,
            Self::Bridge(Value::String(text)) => text.clone(),
            Self::Bridge(reply) => reply.to_string(),
        };
        Some(truncate(&text, 8192))
    }
}
/// Backend-agnostic decoding: an invalid first reply gets exactly one correction
/// attempt from the same observation. No device action has been dispatched yet,
/// so re-planning is safe; the original task and confirmation rules still apply.
async fn with_correction<F, Fut>(observation_id: &str, mut call: F) -> Result<Value>
where
    F: FnMut(Option<Correction>) -> Fut,
    Fut: std::future::Future<Output = Result<RawReply>>,
{
    let first = call(None).await?;
    match first.decode(1, observation_id) {
        Ok(action) => Ok(action),
        Err(error) => {
            let correction = Correction { previous: first.text(), message: correction_message(&error.message) };
            call(Some(correction)).await?.decode(2, observation_id)
        }
    }
}

/// Planning time once evidence is ready. A host bridge runs an agent CLI, so it gets longer.
const CC_MODEL_BUDGET: Duration = Duration::from_secs(95);
const OPENAI_MODEL_BUDGET: Duration = Duration::from_secs(60);

/// A phone observation with its screenshot decoded once, shared by the change
/// fingerprint, the model size check, OCR and the binary screen route.
#[derive(Clone, Default)]
pub struct Observation {
    pub value: Value,
    pub jpeg: Option<Arc<[u8]>>,
    /// OCR evidence computed before planning (hybrid and OCR modes).
    pub ocr: Option<Result<Value>>,
}
impl Observation {
    pub fn new(value: Value, jpeg: Option<Arc<[u8]>>) -> Self {
        let jpeg = jpeg.or_else(|| decode_image(&value));
        Self { value, jpeg, ocr: None }
    }
    pub fn from_value(value: Value) -> Self {
        Self::new(value, None)
    }
}
/// Decodes an observation's `imageBase64`, if any.
pub fn decode_image(value: &Value) -> Option<Arc<[u8]>> {
    let data = value["imageBase64"].as_str()?;
    base64::engine::general_purpose::STANDARD.decode(data).ok().map(Arc::from)
}

/// What a bridge thread that already holds the earlier steps still needs.
#[derive(Clone, Debug, Default)]
pub struct FollowUp {
    /// The goal, only when the thread has not seen it yet (new task or goal update).
    pub instruction: Option<String>,
    /// History entries added since the previous successful plan.
    pub steps: Vec<Value>,
}
pub struct PlanRequest<'a> {
    pub instruction: &'a str,
    pub observation: &'a Observation,
    pub history: Vec<Value>,
    /// Sent to host bridges as `followUp`; other backends ignore it.
    pub follow_up: Option<FollowUp>,
    pub images: &'a [crate::attachments::ReferenceImage],
    /// Bridge thread key (the gateway session id).
    pub session: Option<&'a str>,
}
struct Evidence {
    observation: Value,
    image: Option<Value>,
    references: Vec<Value>,
    reference_parts: Vec<Value>,
}
impl Evidence {
    /// Full planning content: everything a fresh model context needs.
    fn content(&self, instruction: &str, history: &[Value]) -> Vec<Value> {
        let mut content = vec![json!({"type":"text","text":json!({
            "instruction":instruction,"recentHistory":history,"observation":self.observation,
            "referenceImages":self.references,
        }).to_string()})];
        content.extend(self.image.iter().cloned());
        content.extend(self.reference_parts.iter().cloned());
        content
    }
    /// Compact delta for a thread that saw the earlier steps: the new
    /// observation, its screenshot and what happened since the previous plan.
    fn follow_up(&self, follow_up: &FollowUp) -> Vec<Value> {
        let mut text = json!({
            "followUp":true,"stepsSinceLastPlan":follow_up.steps,"observation":self.observation,
            "note":"Continue the same task. stepsSinceLastPlan lists what happened since your previous reply; the observation is the current screen."
        });
        if let Some(instruction) = &follow_up.instruction {
            text["instruction"] = json!(instruction);
        }
        let mut blocks = vec![json!({"type":"text","text":text.to_string()})];
        blocks.extend(self.image.iter().cloned());
        blocks
    }
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn invalid_reply(detail: &str) -> ApiError {
    ApiError::unavailable(format!("Invalid model reply: {detail}"))
}

fn reply_text(choice: &Value) -> Result<String> {
    let content = &choice["message"]["content"];
    if let Some(text) = content.as_str() {
        return Ok(text.to_owned());
    }
    let parts = content.as_array().ok_or_else(|| invalid_reply("message.content must be text"))?;
    parts
        .iter()
        .map(|part| match part["type"].as_str() {
            Some("text" | "output_text") => part["text"].as_str(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .map(|parts| parts.join(""))
        .ok_or_else(|| invalid_reply("message.content must contain text parts"))
}

#[derive(Default)]
struct DecodeDiagnostics {
    reply_chars: Option<usize>,
    json_error: Option<&'static str>,
    trailing_kind: Option<&'static str>,
    trailing_bytes: usize,
    /// Content-free shape of the reply: what it starts with and whether it has a code fence.
    leading: Option<&'static str>,
    fenced: bool,
    /// Which tolerant repair produced the accepted object, if any.
    repair: Option<&'static str>,
}

fn leading_class(text: &str) -> &'static str {
    match text.chars().next() {
        None => "empty",
        Some('{') => "object",
        Some('[') => "array",
        Some('`') => "fence",
        Some('<') => "tag",
        Some(c) if c.is_alphabetic() => "text",
        Some(_) => "other",
    }
}

/// The first fenced block's body, tolerating prose around it and a missing language tag.
fn fenced_block(text: &str) -> Option<&str> {
    let start = text.find("```")? + 3;
    let body = &text[start..];
    let body = body.split_once('\n').map_or(body, |(language, rest)| {
        if language.trim().is_empty() || language.trim().eq_ignore_ascii_case("json") { rest } else { body }
    });
    Some(body.split("```").next().unwrap_or(body).trim())
}

/// Escapes raw control characters inside JSON string literals (models sometimes
/// write a literal newline in `reason`). Text outside strings is unchanged.
fn escape_string_controls(json: &str) -> Option<String> {
    let mut out = String::with_capacity(json.len() + 8);
    let (mut in_string, mut escaped, mut changed) = (false, false, false);
    for c in json.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            } else if c.is_control() {
                changed = true;
                match c {
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    _ => out.push_str(&format!("\\u{:04x}", c as u32)),
                }
                continue;
            }
        } else if c == '"' {
            in_string = true;
        }
        out.push(c);
    }
    changed.then_some(out)
}

/// Candidate JSON texts in order of preference, each labelled with its repair.
fn json_candidates(text: &str) -> Vec<(&'static str, String)> {
    let mut candidates = vec![("none", text.to_owned())];
    if let Some(block) = fenced_block(text)
        && block != text
    {
        candidates.push(("fence", block.to_owned()));
    }
    if let Some(start) = text.find('{')
        && start > 0
    {
        candidates.push(("prefix", text[start..].to_owned()));
    }
    let escaped: Vec<_> = candidates
        .iter()
        .filter_map(|(repair, json)| escape_string_controls(json).map(|json| (*repair, json)))
        .map(|(repair, json)| (if repair == "none" { "controls" } else { "controls+" }, json))
        .collect();
    candidates.extend(escaped);
    candidates
}

#[cfg(test)]
fn decode_reply(choice: &Value) -> Result<Value> {
    decode_reply_with(choice, &mut DecodeDiagnostics::default())
}

/// Parses the first JSON value and reports what, if anything, followed it.
/// Only one device action ever runs per step and the next observation verifies
/// it, so a valid first object followed by more objects or prose is accepted.
fn first_json_value(json: &str, diagnostics: &mut DecodeDiagnostics) -> Result<Value> {
    let mut stream = serde_json::Deserializer::from_str(json).into_iter::<Value>();
    let value = match stream.next() {
        Some(Ok(value)) => value,
        Some(Err(error)) => {
            diagnostics.json_error = Some(match error.classify() {
                serde_json::error::Category::Eof => "eof",
                serde_json::error::Category::Syntax => "syntax",
                serde_json::error::Category::Data => "data",
                serde_json::error::Category::Io => "io",
            });
            return Err(invalid_reply(&format!("invalid JSON at line {}, column {}", error.line(), error.column())));
        }
        None => {
            diagnostics.json_error = Some("empty");
            return Err(invalid_reply("invalid JSON: empty reply"));
        }
    };
    let rest = json[stream.byte_offset()..].trim();
    diagnostics.trailing_bytes = rest.len();
    diagnostics.trailing_kind = Some(if rest.is_empty() {
        "none"
    } else if serde_json::Deserializer::from_str(rest).into_iter::<Value>().next().is_some_and(|v| v.is_ok()) {
        "json_value"
    } else {
        "text"
    });
    Ok(value)
}

fn decode_reply_with(choice: &Value, diagnostics: &mut DecodeDiagnostics) -> Result<Value> {
    if choice["finish_reason"] == "length" {
        return Err(invalid_reply("response was truncated (finish_reason=length)"));
    }
    let text = reply_text(choice)?;
    diagnostics.reply_chars = Some(text.chars().count());
    let text = text.trim().trim_start_matches('\u{feff}').trim();
    diagnostics.leading = Some(leading_class(text));
    diagnostics.fenced = text.contains("```");
    // Try the reply as-is first, then tolerant repairs. The first candidate that
    // parses AND validates wins; otherwise report the original reply's error.
    let mut first_error = None;
    for (repair, json) in json_candidates(text) {
        let mut attempt = DecodeDiagnostics::default();
        match first_json_value(json.trim(), &mut attempt).and_then(validate_reply) {
            Ok(action) => {
                diagnostics.trailing_kind = attempt.trailing_kind;
                diagnostics.trailing_bytes = attempt.trailing_bytes;
                diagnostics.repair = Some(repair);
                return Ok(action);
            }
            Err(error) if first_error.is_none() => {
                diagnostics.json_error = attempt.json_error;
                first_error = Some(error);
            }
            Err(_) => {}
        }
    }
    Err(first_error.unwrap_or_else(|| invalid_reply("invalid JSON: empty reply")))
}

/// Most actions one model reply may plan. The gateway runs them without new
/// observations, so it may stop earlier (see `tasks::plan_prefix`).
pub const MAX_PLAN_ACTIONS: usize = 4;

pub fn validate_reply(mut value: Value) -> Result<Value> {
    let obj = value.as_object().ok_or_else(|| invalid_reply("expected a JSON object"))?;
    if obj.get("type").and_then(Value::as_str) == Some("plan") {
        if obj.keys().any(|k| !["type", "actions"].contains(&k.as_str())) {
            return Err(invalid_reply("plan only accepts type and actions fields"));
        }
        let actions = value["actions"]
            .as_array()
            .filter(|actions| !actions.is_empty())
            .ok_or_else(|| invalid_reply("plan actions must be a nonempty array"))?;
        if actions.len() > MAX_PLAN_ACTIONS {
            return Err(invalid_reply(&format!("plan accepts at most {MAX_PLAN_ACTIONS} actions")));
        }
        let mut validated = Vec::with_capacity(actions.len());
        for (index, action) in actions.iter().enumerate() {
            let mut action = action.clone();
            if let Some(fields) = action.as_object_mut() {
                fields.entry("type").or_insert_with(|| json!("action"));
            }
            // Plans never nest and never contain completion replies.
            if action["type"] != "action" {
                return Err(invalid_reply(&format!("plan action {} must be an action", index + 1)));
            }
            let action = validate_reply(action).map_err(|error| {
                let detail = error.message.strip_prefix("Invalid model reply: ").unwrap_or(&error.message).to_owned();
                invalid_reply(&format!("plan action {}: {detail}", index + 1))
            })?;
            validated.push(action);
        }
        if validated.len() == 1 {
            return Ok(validated.remove(0));
        }
        return Ok(json!({"type":"plan","actions":validated}));
    }
    if obj.get("type").and_then(Value::as_str) == Some("done") {
        if obj.keys().any(|k| !["type", "message"].contains(&k.as_str())) {
            return Err(invalid_reply("done only accepts type and message fields"));
        }
        if !valid_string(&value["message"], 4000) {
            return Err(invalid_reply("message must be nonempty text of at most 4000 characters"));
        }
        return Ok(value);
    }
    if value["type"] != "action" {
        return Err(invalid_reply("type must be action or done"));
    }
    if obj.keys().any(|k| !["type", "tool", "args", "reason", "requiresConfirmation"].contains(&k.as_str())) {
        return Err(invalid_reply("action contains unsupported fields"));
    }
    if !valid_string(&value["reason"], 2000) {
        return Err(invalid_reply("reason must be nonempty text of at most 2000 characters"));
    }
    if value.get("requiresConfirmation").is_some_and(|v| !v.is_boolean()) {
        return Err(invalid_reply("requiresConfirmation must be a boolean"));
    }
    let tool = value["tool"].as_str().ok_or_else(|| invalid_reply("tool must be text"))?;
    let args = value.get("args").cloned().unwrap_or(json!({}));
    command_request(tool, &args).map_err(|error| {
        if error.message.starts_with("Unknown or unsupported tool") {
            invalid_reply("tool is not supported")
        } else {
            invalid_reply(&format!("args: {}", error.message))
        }
    })?;
    value["args"] = args;
    if value.get("requiresConfirmation").is_none() {
        value["requiresConfirmation"] = json!(false);
    }
    Ok(value)
}
fn valid_string(v: &Value, max: usize) -> bool {
    v.as_str().is_some_and(|s| !s.trim().is_empty() && s.chars().count() <= max)
}

#[cfg(test)]
mod ocr_tests {
    use super::*;
    use axum::{Json, Router, extract::State, routing::post};
    use base64::Engine;
    use std::io::Cursor;

    fn png() -> String {
        let mut bytes = Cursor::new(Vec::new());
        image::RgbImage::new(20, 10).write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        base64::engine::general_purpose::STANDARD.encode(bytes.into_inner())
    }

    #[tokio::test]
    async fn initial_and_correction_payloads_contain_only_ocr_for_phone_and_reference() {
        let directory = tempfile::tempdir().unwrap();
        let worker = directory.path().join("worker.py");
        std::fs::write(&worker, r#"import sys,json,pathlib
for line in sys.stdin:
    request=json.loads(line)
    with open(pathlib.Path(__file__).with_suffix('.calls'),'a') as calls: calls.write('request\n')
    print(json.dumps({'id':request['id'],'width':20,'height':10,'lines':[{'text':'OCR recognized only','confidence':0.95,'polygon':[[1,2],[10,2],[10,8],[1,8]]}],'truncated':False}),flush=True)
"#).unwrap();
        let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
        async fn completion(State(requests): State<Arc<Mutex<Vec<Value>>>>, Json(payload): Json<Value>) -> Json<Value> {
            let mut requests = requests.lock().await;
            requests.push(payload);
            let content =
                if requests.len() == 1 { "invalid reply" } else { r#"{"type":"done","message":"OCR inspected"}"# };
            Json(json!({"choices":[{"message":{"content":content},"finish_reason":"stop"}]}))
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = listener.local_addr().unwrap();
        let app = Router::new().route("/chat/completions", post(completion)).with_state(requests.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let model = Model::new(&HashMap::from([
            ("LLM_BASE_URL".into(), format!("http://{endpoint}")),
            ("LLM_MODEL".into(), "fixture".into()),
            ("LLM_API_KEY".into(), "secret".into()),
            ("OCR_WORKER".into(), worker.to_string_lossy().into_owned()),
            ("AI_OBSERVATION".into(), "ocr".into()),
        ]))
        .unwrap();
        let image = png();
        let reference = crate::attachments::ReferenceImage {
            name: "reference.png".into(),
            url: "https://private.example/image-capability".into(),
            content: json!({"type":"image_url","image_url":{"url":format!("data:image/png;base64,{image}")}}),
        };
        let result = model.generate("Inspect text", json!({"observationId":"fixture-observation","imageBase64":image,"caption":"20x10 points, image pixels are point coordinates","elementsText":"PRIVATE WDA TREE"}), vec![], &[reference], CancellationToken::new()).await.unwrap();
        assert_eq!(result["message"], "OCR inspected");
        let renamed_reference = crate::attachments::ReferenceImage {
            name: "different-name.png".into(),
            url: "https://private.example/another-image-capability".into(),
            content: json!({"type":"image_url","image_url":{"url":format!("data:image/png;base64,{image}")}}),
        };
        model
            .clone()
            .generate(
                "Inspect text",
                json!({"imageBase64":image,"caption":"20x10 points, image pixels are point coordinates"}),
                vec![],
                &[renamed_reference],
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(worker.with_extension("calls")).unwrap().lines().count(),
            3,
            "phone frames must be fresh, identical reference pixels must share OCR across Model clones"
        );
        let requests = requests.lock().await;
        assert_eq!(requests.len(), 3);
        for request in requests.iter() {
            let payload = request.to_string();
            assert!(!payload.contains("image_url"), "image payload sent to model");
            assert!(!payload.contains("base64"));
            assert!(!payload.contains("image-capability"));
            assert!(!payload.contains("PRIVATE WDA TREE"));
            assert!(payload.contains("OCR recognized only"));
            let context: Value =
                serde_json::from_str(request["messages"][1]["content"][0]["text"].as_str().unwrap()).unwrap();
            assert!(context["observation"].get("observationId").is_some());
            assert_eq!(context["referenceImages"][0]["ocr"]["coordinateSpace"], "referenceImagePixels");
            assert!(context["referenceImages"][0]["ocr"]["lines"][0].get("phonePolygon").is_none());
        }
        assert!(
            requests[1]["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["role"] == "assistant" && message["content"] == "invalid reply")
        );
        server.abort();
    }
    #[tokio::test]
    async fn ocr_failure_stops_before_model_http_without_image_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let worker = dir.path().join("worker.py");
        std::fs::write(
            &worker,
            r#"import sys,json
for line in sys.stdin:
    request=json.loads(line)
    print(json.dumps({'id':request['id'],'error':'PRIVATE WORKER DETAIL'}),flush=True)
"#,
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let model = Model::new(&HashMap::from([
            ("LLM_BASE_URL".into(), format!("http://{}", listener.local_addr().unwrap())),
            ("LLM_MODEL".into(), "fixture".into()),
            ("LLM_API_KEY".into(), "secret".into()),
            ("OCR_WORKER".into(), worker.to_string_lossy().into_owned()),
            ("AI_OBSERVATION".into(), "ocr".into()),
        ]))
        .unwrap();
        let error = model
            .generate("Inspect", json!({"imageBase64":png()}), vec![], &[], CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.message, "OCR recognition failed");
        assert!(tokio::time::timeout(Duration::from_millis(25), listener.accept()).await.is_err());
    }

    #[tokio::test]
    async fn query_only_tool_results_do_not_require_an_ocr_worker() {
        async fn completion(Json(payload): Json<Value>) -> Json<Value> {
            let context: Value =
                serde_json::from_str(payload["messages"][1]["content"][0]["text"].as_str().unwrap()).unwrap();
            assert_eq!(context["observation"]["queryOnly"], true);
            assert!(context["observation"].get("ocr").is_none());
            assert_eq!(context["recentHistory"][0]["result"]["text"], "Installed app result");
            Json(
                json!({"choices":[{"message":{"content":r#"{"type":"done","message":"Read query"}"#},"finish_reason":"stop"}]}),
            )
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/chat/completions", post(completion))).await.unwrap()
        });
        let model = Model::new(&HashMap::from([
            ("LLM_BASE_URL".into(), format!("http://{endpoint}")),
            ("LLM_MODEL".into(), "fixture".into()),
            ("LLM_API_KEY".into(), "secret".into()),
            ("OCR_PYTHON".into(), "/missing-ocr-interpreter".into()),
        ]))
        .unwrap();
        let result = model
            .generate(
                "Read query",
                json!({"queryOnly":true}),
                vec![json!({"result":{"text":"Installed app result"}})],
                &[],
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result["message"], "Read query");
        server.abort();
    }
    async fn capture_server() -> (String, Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
        async fn completion(State(requests): State<Arc<Mutex<Vec<Value>>>>, Json(payload): Json<Value>) -> Json<Value> {
            requests.lock().await.push(payload);
            Json(
                json!({"choices":[{"message":{"content":r#"{"type":"done","message":"ok"}"#},"finish_reason":"stop"}]}),
            )
        }
        let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route("/chat/completions", post(completion)).with_state(requests.clone());
        (endpoint, requests, tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }))
    }
    fn mode_model(endpoint: &str, mode: &str, worker: &std::path::Path) -> Model {
        Model::new(&HashMap::from([
            ("LLM_BASE_URL".into(), endpoint.to_owned()),
            ("LLM_MODEL".into(), "fixture".into()),
            ("LLM_API_KEY".into(), "secret".into()),
            ("AI_OBSERVATION".into(), mode.into()),
            ("OCR_WORKER".into(), worker.to_string_lossy().into_owned()),
        ]))
        .unwrap()
    }
    fn reference() -> crate::attachments::ReferenceImage {
        crate::attachments::ReferenceImage {
            name: "reference.png".into(),
            url: "https://private.example/image-capability".into(),
            content: json!({"type":"image_url","image_url":{"url":format!("data:image/png;base64,{}", png())}}),
        }
    }
    const OK_WORKER: &str = r#"import sys,json
for line in sys.stdin:
    request=json.loads(line)
    print(json.dumps({'id':request['id'],'width':20,'height':10,'lines':[{'text':'OCR line','confidence':0.9,'polygon':[[1,2],[10,2],[10,8],[1,8]]}],'truncated':False}),flush=True)
"#;

    #[test]
    fn observation_mode_defaults_to_image_and_rejects_unknown_values() {
        assert_eq!(ObservationMode::parse("").unwrap(), ObservationMode::Image);
        assert_eq!(ObservationMode::parse("ocr").unwrap(), ObservationMode::Ocr);
        assert_eq!(ObservationMode::parse(" Hybrid ").unwrap(), ObservationMode::Hybrid);
        assert_eq!(ObservationMode::parse("image").unwrap(), ObservationMode::Image);
        assert!(ObservationMode::parse("vision").is_err());
        let ocr = ObservationMode::Ocr.system_prompt();
        let image = ObservationMode::Image.system_prompt();
        let hybrid = ObservationMode::Hybrid.system_prompt();
        assert!(ocr.contains("You receive no visual image") && !ocr.contains("phoneImageUrl"));
        // OCR text is not an accessibility label: steer taps to OCR coordinates.
        assert!(ocr.contains("iphone_tap at the center of its phonePolygon"));
        assert!(ocr.contains("exact label taken from a fresh iphone_list_elements result"));
        assert!(image.contains("phoneImageUrl") && !image.contains("phonePolygon"));
        assert!(hybrid.contains("phoneImageUrl") && hybrid.contains("phonePolygon"));
        for prompt in [ocr, image, hybrid] {
            assert!(
                prompt.contains("Return only one JSON object")
                    && prompt.contains("Tools (all unknown fields rejected)")
            );
        }
    }

    #[tokio::test]
    async fn hybrid_sends_image_elements_reference_url_and_ocr() {
        let dir = tempfile::tempdir().unwrap();
        let worker = dir.path().join("worker.py");
        std::fs::write(&worker, OK_WORKER).unwrap();
        let (endpoint, requests, server) = capture_server().await;
        let model = mode_model(&endpoint, "hybrid", &worker);
        assert!(model.wants_elements());
        let observation = json!({"imageBase64":png(),"caption":"20x10 points","elementsText":"Button: Send"});
        model.generate("Inspect", observation, vec![], &[reference()], CancellationToken::new()).await.unwrap();
        let request = requests.lock().await[0].clone();
        let parts = request["messages"][1]["content"].as_array().unwrap();
        let context: Value = serde_json::from_str(parts[0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(context["observation"]["elementsText"], "Button: Send");
        assert_eq!(context["observation"]["ocr"]["lines"][0]["text"], "OCR line");
        assert_eq!(context["referenceImages"][0]["phoneImageUrl"], "https://private.example/image-capability");
        assert_eq!(parts.iter().filter(|part| part["type"] == "image_url").count(), 2, "phone and reference images");
        server.abort();
    }

    #[tokio::test]
    async fn hybrid_ocr_failure_degrades_to_the_image_step() {
        let (endpoint, requests, server) = capture_server().await;
        let model = mode_model(&endpoint, "hybrid", std::path::Path::new("/missing-ocr-worker.py"));
        model
            .generate("Inspect", json!({"imageBase64":png(),"caption":"20x10"}), vec![], &[], CancellationToken::new())
            .await
            .unwrap();
        let request = requests.lock().await[0].clone();
        let context: Value =
            serde_json::from_str(request["messages"][1]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(context["observation"]["ocrError"], "OCR unavailable");
        assert!(context["observation"].get("ocr").is_none());
        assert_eq!(request["messages"][1]["content"][1]["type"], "image_url");
        server.abort();
    }

    #[tokio::test]
    async fn image_mode_never_starts_the_ocr_worker() {
        let dir = tempfile::tempdir().unwrap();
        let worker = dir.path().join("worker.py");
        std::fs::write(&worker, "open(__file__ + '.started', 'w').close()\n").unwrap();
        let (endpoint, requests, server) = capture_server().await;
        let model = mode_model(&endpoint, "image", &worker);
        model.warm_ocr();
        model
            .generate("Inspect", json!({"imageBase64":png(),"caption":"20x10"}), vec![], &[], CancellationToken::new())
            .await
            .unwrap();
        assert!(!dir.path().join("worker.py.started").exists());
        let request = requests.lock().await[0].clone();
        assert!(!request.to_string().contains("\"ocr\""));
        server.abort();
    }

    #[tokio::test]
    async fn ocr_mode_omits_elements_and_warmup_loads_the_worker() {
        let dir = tempfile::tempdir().unwrap();
        let worker = dir.path().join("worker.py");
        std::fs::write(
            &worker,
            format!("import pathlib\npathlib.Path(__file__).with_suffix('.started').touch()\n{OK_WORKER}"),
        )
        .unwrap();
        let (endpoint, _requests, server) = capture_server().await;
        let model = mode_model(&endpoint, "ocr", &worker);
        assert!(!model.wants_elements());
        model.warm_ocr();
        tokio::time::timeout(Duration::from_secs(3), async {
            while !worker.with_extension("started").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("warm-up starts the OCR worker");
        server.abort();
    }

    fn reply(content: &str) -> Value {
        json!({"message":{"content":content},"finish_reason":"stop"})
    }

    #[test]
    fn concatenated_replies_run_only_the_first_valid_object() {
        let first = r#"{"type":"action","tool":"iphone_tap","args":{"x":10,"y":20},"reason":"Open","requiresConfirmation":false}"#;
        let second = r#"{"type":"action","tool":"iphone_type_text","args":{"text":"hi"},"reason":"Type","requiresConfirmation":false}"#;
        for (content, kind) in [
            (format!("{first}{second}"), "json_value"),
            (format!("{first}\n{second}"), "json_value"),
            (format!("{first}\nI will then type the text."), "text"),
            (format!("{first}\n"), "none"),
        ] {
            let mut diagnostics = DecodeDiagnostics::default();
            let action = decode_reply_with(&reply(&content), &mut diagnostics).unwrap();
            assert_eq!(action["tool"], "iphone_tap", "{content}");
            assert_eq!(diagnostics.trailing_kind, Some(kind), "{content}");
            assert_eq!(diagnostics.trailing_bytes > 0, kind != "none");
        }
        // A valid trailing object never rescues an invalid first object.
        let invalid_first = format!(
            r#"{{"type":"action","tool":"unknown","args":{{}},"reason":"x","requiresConfirmation":false}}{second}"#
        );
        assert!(decode_reply(&reply(&invalid_first)).is_err());
    }

    #[test]
    fn tolerant_repairs_accept_prose_fences_and_raw_newlines_but_not_invalid_actions() {
        let action = r#"{"type":"action","tool":"iphone_tap","args":{"x":10,"y":20},"reason":"Open","requiresConfirmation":false}"#;
        let newline = "{\"type\":\"action\",\"tool\":\"iphone_tap\",\"args\":{\"x\":10,\"y\":20},\"reason\":\"Open\nthe list\",\"requiresConfirmation\":false}";
        for (content, repair, leading) in [
            (action.to_owned(), "none", "object"),
            (format!("Sure, here is the next action:\n{action}"), "prefix", "text"),
            (format!("```json\n{action}\n```\nThis opens the list."), "fence", "fence"),
            (format!("```\n{action}\n```"), "fence", "fence"),
            (newline.to_owned(), "controls", "object"),
            (format!("Next:\n{newline}"), "controls+", "text"),
        ] {
            let mut diagnostics = DecodeDiagnostics::default();
            let decoded =
                decode_reply_with(&reply(&content), &mut diagnostics).unwrap_or_else(|e| panic!("{content}: {e}"));
            assert_eq!(decoded["tool"], "iphone_tap", "{content}");
            assert_eq!(diagnostics.repair, Some(repair), "{content}");
            assert_eq!(diagnostics.leading, Some(leading), "{content}");
        }
        // Repairs never turn invalid content into an action.
        for content in [
            "{type:action}".to_owned(),
            format!("```python\n{action}\n```"),
            r#"Here: {"type":"action","tool":"unknown","args":{},"reason":"x"}"#.to_owned(),
            "No JSON here".to_owned(),
        ] {
            let mut diagnostics = DecodeDiagnostics::default();
            if content.starts_with("```python") {
                // The python fence is skipped as a fence, but the JSON object after it still parses as a prefix repair.
                assert_eq!(decode_reply_with(&reply(&content), &mut diagnostics).unwrap()["tool"], "iphone_tap");
                assert_eq!(diagnostics.repair, Some("prefix"));
                continue;
            }
            assert!(decode_reply_with(&reply(&content), &mut diagnostics).is_err(), "{content}");
            assert!(diagnostics.repair.is_none());
        }
        assert_eq!(escape_string_controls("{\"a\":\"x\\\"\ny\"}").unwrap(), "{\"a\":\"x\\\"\\ny\"}");
        assert!(escape_string_controls("{\n\"a\": 1\n}").is_none(), "whitespace outside strings is valid JSON");
    }

    #[test]
    fn json_errors_are_classified_without_reply_content() {
        for (content, category) in [("{\"type\":\"action\"", "eof"), ("{type:action}", "syntax"), ("   ", "empty")] {
            let mut diagnostics = DecodeDiagnostics::default();
            let error = decode_reply_with(&reply(content), &mut diagnostics).unwrap_err();
            assert_eq!(diagnostics.json_error, Some(category), "{content}");
            assert_eq!(protocol_decode_category(&Err(error)), "json_syntax");
        }
    }

    #[test]
    fn correction_message_names_no_concrete_tool() {
        let message = correction_message("Invalid model reply: invalid JSON at line 1, column 9");
        assert!(message.starts_with("Your previous reply failed validation: Invalid model reply: invalid JSON"));
        assert!(!message.contains("iphone_"), "a concrete tool example is copied by models on retry");
        assert!(message.contains("only the first action"));
    }

    #[test]
    fn diagnostic_fields_have_explicit_timing_and_protocol_categories() {
        let diagnostics = RequestDiagnostics {
            observation_id: "fixture-observation",
            attempt: 2,
            provider_attempt: 1,
            started: Instant::now(),
            request_bytes: 123,
            response_bytes: 45,
            response_headers_ms: Some(10),
            first_body_chunk_ms: Some(20),
            http_status: Some(200),
            finish_reason: Some("stop"),
            status: "complete",
        };
        let metrics = diagnostics.snapshot();
        assert_eq!(metrics["request_bytes"], 123);
        assert_eq!(metrics["response_bytes"], 45);
        assert_eq!(metrics["response_headers_ms"], 10);
        assert_eq!(metrics["first_body_chunk_ms"], 20);
        assert_eq!(metrics["observation_id"], "fixture-observation");
        assert_eq!(protocol_decode_category(&decode_reply(&json!({"message":{"content":"invalid"}}))), "json_syntax");
        assert_eq!(
            protocol_decode_category(&decode_reply(&json!({"finish_reason":"length","message":{"content":"invalid"}}))),
            "truncated"
        );
        assert_eq!(protocol_decode_category(&decode_reply(&json!({"message":{"content":null}}))), "content_shape");
        assert_eq!(
            protocol_decode_category(&decode_reply(
                &json!({"message":{"content":r#"{"type":"action","tool":"unknown","reason":"inspect"}"#}})
            )),
            "action_schema"
        );
        assert_eq!(
            protocol_decode_category(&decode_reply(
                &json!({"message":{"content":r#"{"type":"done","message":"finished"}"#}})
            )),
            "done"
        );
    }
}

#[cfg(test)]
#[path = "model_cc_tests.rs"]
mod cc_tests;
