//! Serialized, persistent PaddleOCR subprocess. Images remain inside the OCR boundary.
use crate::error::{ApiError, Result};
use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    io::Cursor,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;

const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_REPLY_BYTES: usize = 1024 * 1024;
const MAX_LINES: usize = 512;
const MAX_CACHED_REFERENCES: usize = 6;
const MAX_REFERENCE_CACHE_BYTES: usize = 512 * 1024;

pub struct Ocr {
    python: String,
    worker: String,
    // Owned by a spawned exchange while a request is in flight, so a caller's
    // cancellation never kills a worker that is mid-recognition.
    process: Arc<Mutex<Option<Worker>>>,
    references: Mutex<ReferenceCache>,
    timeout: Duration,
}
#[derive(Default)]
struct ReferenceCache {
    entries: VecDeque<CachedReference>,
    bytes: usize,
}
struct CachedReference {
    hash: [u8; 32],
    evidence: Value,
    bytes: usize,
}
impl ReferenceCache {
    fn get(&mut self, hash: &[u8; 32]) -> Option<Value> {
        let index = self.entries.iter().position(|entry| &entry.hash == hash)?;
        let entry = self.entries.remove(index)?;
        let evidence = entry.evidence.clone();
        self.entries.push_back(entry);
        Some(evidence)
    }
    fn insert(&mut self, hash: [u8; 32], evidence: Value) {
        let bytes = evidence.to_string().len();
        if bytes > MAX_REFERENCE_CACHE_BYTES {
            return;
        }
        while self.entries.len() >= MAX_CACHED_REFERENCES || self.bytes + bytes > MAX_REFERENCE_CACHE_BYTES {
            if let Some(oldest) = self.entries.pop_front() {
                self.bytes -= oldest.bytes;
            }
        }
        self.bytes += bytes;
        self.entries.push_back(CachedReference { hash, evidence, bytes });
    }
}
struct Worker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    id: String,
    width: u32,
    height: u32,
    lines: Vec<Line>,
    truncated: bool,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Line {
    text: String,
    confidence: f64,
    polygon: Vec<[f64; 2]>,
}
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Mapping {
    offset_x: f64,
    offset_y: f64,
    scale_x: f64,
    scale_y: f64,
}

fn default_worker(directory: &std::path::Path) -> &'static str {
    if directory.join("browser-control/ocr/worker.py").is_file() {
        "browser-control/ocr/worker.py"
    } else {
        "ocr/worker.py"
    }
}

impl Ocr {
    pub fn new(env: &HashMap<String, String>) -> Self {
        let setting = |key: &str, fallback: &str| {
            env.get(key).filter(|s| !s.trim().is_empty()).cloned().unwrap_or_else(|| fallback.to_owned())
        };
        Self {
            python: setting("OCR_PYTHON", "python3"),
            worker: setting("OCR_WORKER", default_worker(&std::env::current_dir().unwrap_or_default())),
            process: Arc::new(Mutex::new(None)),
            references: Mutex::new(ReferenceCache::default()),
            // Includes queueing and cold model loading. Model HTTP retains its own deadline.
            timeout: Duration::from_secs(90),
        }
    }

    pub async fn recognize_reference(&self, image: &str, cancel: CancellationToken) -> Result<Value> {
        if image.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
            return Err(ApiError::unavailable("OCR image exceeds limit"));
        }
        if cancel.is_cancelled() {
            return Err(ApiError::unavailable("OCR request cancelled"));
        }
        let hash: [u8; 32] = Sha256::digest(image.as_bytes()).into();
        let operation = async {
            // Keep this lock across a miss so concurrent identical references share
            // one OCR result. Phone observations always bypass this cache.
            let mut cache = self.references.lock().await;
            if let Some(evidence) = cache.get(&hash) {
                eprintln!("event=ocr_reference_cache status=hit entries={} bytes={}", cache.entries.len(), cache.bytes);
                return Ok(evidence);
            }
            let evidence = self.recognize(image, None, None, cancel.clone()).await?;
            cache.insert(hash, evidence.clone());
            eprintln!("event=ocr_reference_cache status=miss entries={} bytes={}", cache.entries.len(), cache.bytes);
            Ok(evidence)
        };
        // `recognize` bounds the worker exchange; the cache lock is only held by
        // requests that are themselves bounded, so no second deadline is needed.
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ApiError::unavailable("OCR request cancelled")),
            result = operation => result,
        }
    }

    /// Loads the OCR model before the first task step. Failures only log.
    pub async fn warm(&self) {
        let mut bytes = Cursor::new(Vec::new());
        if image::RgbImage::new(32, 32).write_to(&mut bytes, image::ImageFormat::Png).is_err() {
            return;
        }
        let image = base64::engine::general_purpose::STANDARD.encode(bytes.into_inner());
        let started = Instant::now();
        let result = self.recognize(&image, None, None, CancellationToken::new()).await;
        eprintln!(
            "event=ocr_warmup elapsed_ms={} status={}",
            started.elapsed().as_millis(),
            if result.is_ok() { "ok" } else { "error" }
        );
    }

    /// `decoded` is the already decoded `image`, so a phone frame is not decoded again here.
    pub async fn recognize(
        &self,
        image: &str,
        decoded: Option<&[u8]>,
        observation: Option<&Value>,
        cancel: CancellationToken,
    ) -> Result<Value> {
        let started = Instant::now();
        let dimensions = image_dimensions(image, decoded)?;
        if cancel.is_cancelled() {
            return Err(ApiError::unavailable("OCR request cancelled"));
        }
        let (process, spawner, timeout) = (self.process.clone(), self.spawner(), self.timeout);
        let (image, queued) = (image.to_owned(), cancel.clone());
        // The exchange runs detached from the caller. A cancelled caller stops
        // waiting immediately; the worker finishes its current reply, which is
        // matched by id and discarded, and stays warm for the next request.
        // Only a timeout or protocol failure drops (and so kills) the worker.
        let exchange = tokio::spawn(async move {
            let operation = async {
                let mut slot = process.lock_owned().await;
                if queued.is_cancelled() {
                    return Err(ApiError::unavailable("OCR request cancelled"));
                }
                let mut worker = match slot.take() {
                    Some(worker) => worker,
                    None => spawner.spawn()?,
                };
                let id = uuid::Uuid::new_v4().to_string();
                let reply = worker.exchange(&id, &image).await;
                match reply {
                    Ok(reply) => {
                        let parsed = parse_response(&reply, &id, dimensions);
                        if parsed.is_ok() {
                            *slot = Some(worker);
                        } else {
                            let _ = worker.child.kill().await;
                        }
                        parsed
                    }
                    Err(error) => {
                        let _ = worker.child.kill().await;
                        Err(error)
                    }
                }
            };
            tokio::time::timeout(timeout, operation)
                .await
                .unwrap_or_else(|_| Err(ApiError::unavailable("OCR request timed out")))
        });
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ApiError::unavailable("OCR request cancelled")),
            result = exchange => result.unwrap_or_else(|_| Err(ApiError::unavailable("OCR worker task failed"))),
        };
        eprintln!(
            "event=ocr_recognition elapsed_ms={} status={}",
            started.elapsed().as_millis(),
            if result.is_ok() { "ok" } else { "error" }
        );
        observation_text(result?, observation)
    }

    fn spawner(&self) -> Spawner {
        Spawner { python: self.python.clone(), worker: self.worker.clone() }
    }
}
struct Spawner {
    python: String,
    worker: String,
}
impl Spawner {
    fn spawn(&self) -> Result<Worker> {
        let mut child = Command::new(&self.python)
            .arg(&self.worker)
            .env("PYTHONUNBUFFERED", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Worker stderr can contain library diagnostics and user text. Keep it
            // outside both the API and gateway logs.
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| ApiError::unavailable("OCR worker could not start"))?;
        Ok(Worker {
            stdin: child.stdin.take().ok_or_else(|| ApiError::unavailable("OCR worker input unavailable"))?,
            stdout: BufReader::new(
                child.stdout.take().ok_or_else(|| ApiError::unavailable("OCR worker output unavailable"))?,
            ),
            child,
        })
    }
}
impl Worker {
    async fn exchange(&mut self, id: &str, image: &str) -> Result<Vec<u8>> {
        let mut input = serde_json::to_vec(&json!({"id":id,"imageBase64":image}))
            .map_err(|_| ApiError::unavailable("OCR input invalid"))?;
        input.push(b'\n');
        self.stdin.write_all(&input).await.map_err(|_| ApiError::unavailable("OCR worker input failed"))?;
        self.stdin.flush().await.map_err(|_| ApiError::unavailable("OCR worker input failed"))?;
        let mut reply = Vec::new();
        loop {
            let available =
                self.stdout.fill_buf().await.map_err(|_| ApiError::unavailable("OCR worker output failed"))?;
            if available.is_empty() {
                return Err(ApiError::unavailable("OCR worker stopped before replying"));
            }
            let end = available.iter().position(|b| *b == b'\n').map(|i| i + 1);
            let count = end.unwrap_or(available.len());
            if reply.len() + count > MAX_REPLY_BYTES {
                return Err(ApiError::unavailable("OCR worker response exceeds limit"));
            }
            reply.extend_from_slice(&available[..count]);
            self.stdout.consume(count);
            if end.is_some() {
                return Ok(reply);
            }
        }
    }
}
fn image_dimensions(image: &str, decoded: Option<&[u8]>) -> Result<(u32, u32)> {
    if image.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
        return Err(ApiError::unavailable("OCR image exceeds limit"));
    }
    let owned;
    let bytes = match decoded {
        Some(bytes) => bytes,
        None => {
            owned = base64::engine::general_purpose::STANDARD
                .decode(image)
                .map_err(|_| ApiError::unavailable("OCR image encoding invalid"))?;
            &owned
        }
    };
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
        return Err(ApiError::unavailable("OCR image exceeds limit"));
    }
    let reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| ApiError::unavailable("OCR image format invalid"))?;
    if !matches!(reader.format(), Some(image::ImageFormat::Png | image::ImageFormat::Jpeg)) {
        return Err(ApiError::unavailable("OCR image format invalid"));
    }
    let (width, height) =
        reader.into_dimensions().map_err(|_| ApiError::unavailable("OCR image dimensions invalid"))?;
    if width == 0 || height == 0 || width > 8192 || height > 8192 || u64::from(width) * u64::from(height) > 24_000_000 {
        return Err(ApiError::unavailable("OCR image dimensions exceed limit"));
    }
    Ok((width, height))
}
fn parse_response(bytes: &[u8], id: &str, dimensions: (u32, u32)) -> Result<Response> {
    let invalid = || ApiError::unavailable("OCR worker response invalid");
    // Error details are intentionally generic, regardless of subprocess output.
    let value: Value = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    if value.get("error").is_some() {
        return Err(ApiError::unavailable("OCR recognition failed"));
    }
    let response: Response = serde_json::from_value(value).map_err(|_| invalid())?;
    if response.id != id || (response.width, response.height) != dimensions || response.lines.len() > MAX_LINES {
        return Err(invalid());
    }
    for line in &response.lines {
        if line.text.trim().is_empty()
            || line.text.chars().count() > 2048
            || line.text.chars().any(|c| c.is_control() && !['\n', '\r', '\t'].contains(&c))
            || !line.confidence.is_finite()
            || !(0.0..=1.0).contains(&line.confidence)
            || line.polygon.len() != 4
            || line.polygon.iter().any(|[x, y]| {
                !x.is_finite()
                    || !y.is_finite()
                    || *x < 0.0
                    || *y < 0.0
                    || *x > f64::from(response.width)
                    || *y > f64::from(response.height)
            })
        {
            return Err(invalid());
        }
    }
    Ok(response)
}
fn observation_text(response: Response, observation: Option<&Value>) -> Result<Value> {
    let mapping = if let Some(observation) = observation {
        match observation.get("coordinateMapping").filter(|v| !v.is_null()) {
            Some(value) => serde_json::from_value::<Mapping>(value.clone())
                .map_err(|_| ApiError::unavailable("OCR coordinate mapping invalid"))?,
            None => {
                let caption = observation["caption"].as_str().unwrap_or("");
                if caption.contains("Region from")
                    || caption.contains("区域 (")
                    || caption.contains("image pixel (u,v)")
                    || caption.contains("Android crop at")
                {
                    return Err(ApiError::unavailable("OCR crop coordinate mapping unavailable"));
                }
                Mapping { offset_x: 0.0, offset_y: 0.0, scale_x: 1.0, scale_y: 1.0 }
            }
        }
    } else {
        Mapping { offset_x: 0.0, offset_y: 0.0, scale_x: 1.0, scale_y: 1.0 }
    };
    if ![mapping.offset_x, mapping.offset_y, mapping.scale_x, mapping.scale_y].iter().all(|n| n.is_finite())
        || mapping.offset_x < 0.0
        || mapping.offset_y < 0.0
        || mapping.scale_x <= 0.0
        || mapping.scale_y <= 0.0
        || mapping.offset_x + f64::from(response.width) * mapping.scale_x > 65536.0
        || mapping.offset_y + f64::from(response.height) * mapping.scale_y > 65536.0
    {
        return Err(ApiError::unavailable("OCR coordinate mapping invalid"));
    }
    let mut text_bytes = 0;
    let mut truncated = response.truncated;
    let mut lines = Vec::new();
    for line in response.lines {
        let value = if observation.is_some() {
            let points: Vec<_> = line
                .polygon
                .iter()
                .map(|[x, y]| [mapping.offset_x + x * mapping.scale_x, mapping.offset_y + y * mapping.scale_y])
                .collect();
            json!({"text":line.text,"confidence":line.confidence,"imagePolygon":line.polygon,"phonePolygon":points})
        } else {
            json!({"text":line.text,"confidence":line.confidence,"imagePolygon":line.polygon})
        };
        let size = value.to_string().len();
        if text_bytes + size > 96 * 1024 {
            truncated = true;
            break;
        }
        text_bytes += size;
        lines.push(value);
    }
    Ok(json!({"width":response.width,"height":response.height,
        "coordinateSpace":if observation.is_some() {"phonePoints"} else {"referenceImagePixels"},
        "lines":lines,"truncated":truncated}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png() -> String {
        let mut bytes = Cursor::new(Vec::new());
        image::RgbImage::new(20, 10).write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        base64::engine::general_purpose::STANDARD.encode(bytes.into_inner())
    }
    fn response() -> Value {
        json!({"id":"request","width":20,"height":10,"lines":[{"text":"Settings","confidence":0.9,"polygon":[[1,2],[10,2],[10,8],[1,8]]}],"truncated":false})
    }
    fn parse(value: Value) -> Result<Response> {
        parse_response(&serde_json::to_vec(&value).unwrap(), "request", (20, 10))
    }
    fn fixture(timeout: Duration) -> (tempfile::TempDir, Ocr) {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("worker.py");
        std::fs::write(&script, r#"import sys,json,os,time,pathlib
root=pathlib.Path(__file__).parent
with open(root/'events','a') as events:
    events.write(str(os.getpid())+' start\n')
for line in sys.stdin:
    request=json.loads(line)
    with open(root/'events','a') as events:
        events.write(str(os.getpid())+' request\n')
    if (root/'delay').exists(): time.sleep(2)
    reply={'id':request['id'],'width':20,'height':10,'lines':[{'text':'Settings','confidence':0.9,'polygon':[[1,2],[10,2],[10,8],[1,8]]}],'truncated':False}
    if (root/'invalid').exists(): reply['id']='wrong'
    if (root/'oversized').exists():
        print('x'*(1024*1024+1),flush=True)
    else: print(json.dumps(reply),flush=True)
"#).unwrap();
        let mut ocr = Ocr::new(&HashMap::from([("OCR_WORKER".into(), script.to_string_lossy().into_owned())]));
        ocr.timeout = timeout;
        (dir, ocr)
    }
    async fn wait_for_requests(dir: &std::path::Path, count: usize) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let events = std::fs::read_to_string(dir.join("events")).unwrap_or_default();
                if events.lines().filter(|line| line.ends_with(" request")).count() >= count {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn validates_worker_shape_dimensions_and_coordinate_ranges() {
        assert!(parse(response()).is_ok());
        let mut invalid = Vec::new();
        for (key, value) in
            [("id", json!("wrong")), ("width", json!(21)), ("height", json!(0)), ("truncated", json!("false"))]
        {
            let mut v = response();
            v[key] = value;
            invalid.push(v);
        }
        for (key, value) in [
            ("text", json!("")),
            ("text", json!("x".repeat(2049))),
            ("confidence", json!(1.1)),
            ("polygon", json!([[1, 2], [21, 2], [10, 8], [1, 8]])),
            ("polygon", json!([[1, 2], [10, 2]])),
        ] {
            let mut v = response();
            v["lines"][0][key] = value;
            invalid.push(v);
        }
        let mut many = response();
        many["lines"] = json!(vec![many["lines"][0].clone(); 513]);
        invalid.push(many);
        for v in invalid {
            assert!(parse(v).is_err());
        }
        assert_eq!(parse(json!({"id":"request","error":"SECRET"})).unwrap_err().message, "OCR recognition failed");
        assert!(parse_response(b"not JSON", "request", (20, 10)).is_err());
    }

    #[test]
    fn crops_use_exact_affine_mapping_and_reference_coordinates_remain_separate() {
        let observation = json!({"caption":"Region from (10,20), 10x5 points, zoomed 2x", "coordinateMapping":{"offsetX":10.125,"offsetY":20.25,"scaleX":0.5,"scaleY":0.625}});
        let evidence = observation_text(parse(response()).unwrap(), Some(&observation)).unwrap();
        assert_eq!(evidence["lines"][0]["phonePolygon"][0], json!([10.625, 21.5]));
        assert_eq!(evidence["lines"][0]["imagePolygon"][0], json!([1.0, 2.0]));
        let reference = observation_text(parse(response()).unwrap(), None).unwrap();
        assert_eq!(reference["coordinateSpace"], "referenceImagePixels");
        assert!(reference["lines"][0].get("phonePolygon").is_none());
        assert!(observation_text(parse(response()).unwrap(), Some(&json!({"caption":"Region from (10,20)"}))).is_err());
    }

    #[test]
    fn empty_ocr_succeeds_and_model_evidence_is_bounded() {
        let mut empty = response();
        empty["lines"] = json!([]);
        assert_eq!(observation_text(parse(empty).unwrap(), None).unwrap()["lines"], json!([]));
        let mut large = response();
        let mut line = large["lines"][0].clone();
        line["text"] = json!("字".repeat(2048));
        large["lines"] = json!(vec![line; 128]);
        let bounded = observation_text(parse(large).unwrap(), None).unwrap();
        assert_eq!(bounded["truncated"], true);
        assert!(bounded.to_string().len() < 100 * 1024);
    }

    #[tokio::test]
    async fn persistent_worker_reuses_process_and_restarts_after_invalid_output() {
        let (dir, ocr) = fixture(Duration::from_secs(3));
        let image = png();
        for _ in 0..2 {
            ocr.recognize(&image, None, None, CancellationToken::new()).await.unwrap();
        }
        let events = std::fs::read_to_string(dir.path().join("events")).unwrap();
        assert_eq!(events.lines().filter(|line| line.ends_with(" start")).count(), 1);
        std::fs::write(dir.path().join("invalid"), "").unwrap();
        assert!(ocr.recognize(&image, None, None, CancellationToken::new()).await.is_err());
        std::fs::remove_file(dir.path().join("invalid")).unwrap();
        ocr.recognize(&image, None, None, CancellationToken::new()).await.unwrap();
        let events = std::fs::read_to_string(dir.path().join("events")).unwrap();
        assert_eq!(events.lines().filter(|line| line.ends_with(" start")).count(), 2);
    }

    #[tokio::test]
    async fn timeout_kills_worker_and_next_request_recovers() {
        // Leave Python start-up well inside the deadline under parallel test load;
        // the fixture's delayed reply (2 s) still exceeds it.
        let (dir, ocr) = fixture(Duration::from_millis(1000));
        std::fs::write(dir.path().join("delay"), "").unwrap();
        assert_eq!(
            ocr.recognize(&png(), None, None, CancellationToken::new()).await.unwrap_err().message,
            "OCR request timed out"
        );
        std::fs::remove_file(dir.path().join("delay")).unwrap();
        ocr.recognize(&png(), None, None, CancellationToken::new()).await.unwrap();
        let events = std::fs::read_to_string(dir.path().join("events")).unwrap();
        assert_eq!(events.lines().filter(|line| line.ends_with(" start")).count(), 2);
    }

    #[tokio::test]
    async fn cancellation_keeps_the_active_worker_without_a_stale_reply() {
        let (dir, ocr) = fixture(Duration::from_secs(3));
        let ocr = std::sync::Arc::new(ocr);
        std::fs::write(dir.path().join("delay"), "").unwrap();
        let cancel = CancellationToken::new();
        let request = {
            let ocr = ocr.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move { ocr.recognize(&png(), None, None, cancel).await })
        };
        wait_for_requests(dir.path(), 1).await;
        cancel.cancel();
        assert_eq!(request.await.unwrap().unwrap_err().message, "OCR request cancelled");
        std::fs::remove_file(dir.path().join("delay")).unwrap();
        // The cancelled exchange completes in the background; its reply is
        // consumed there, and the same warm worker serves the next request.
        ocr.recognize(&png(), None, None, CancellationToken::new()).await.unwrap();
        let events = std::fs::read_to_string(dir.path().join("events")).unwrap();
        assert_eq!(events.lines().filter(|line| line.ends_with(" start")).count(), 1);
        assert_eq!(events.lines().filter(|line| line.ends_with(" request")).count(), 2);
    }

    #[tokio::test]
    async fn queued_cancellation_preserves_the_inflight_worker() {
        let (dir, ocr) = fixture(Duration::from_secs(3));
        let ocr = std::sync::Arc::new(ocr);
        std::fs::write(dir.path().join("delay"), "").unwrap();
        let active = {
            let ocr = ocr.clone();
            tokio::spawn(async move { ocr.recognize(&png(), None, None, CancellationToken::new()).await })
        };
        wait_for_requests(dir.path(), 1).await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(ocr.recognize(&png(), None, None, cancel).await.unwrap_err().message, "OCR request cancelled");
        std::fs::remove_file(dir.path().join("delay")).unwrap();
        active.await.unwrap().unwrap();
        ocr.recognize(&png(), None, None, CancellationToken::new()).await.unwrap();
        let events = std::fs::read_to_string(dir.path().join("events")).unwrap();
        assert_eq!(events.lines().filter(|line| line.ends_with(" start")).count(), 1);
    }

    #[tokio::test]
    async fn oversized_worker_output_stops_at_the_bound_and_recovers() {
        let (dir, ocr) = fixture(Duration::from_secs(3));
        std::fs::write(dir.path().join("oversized"), "").unwrap();
        assert_eq!(
            ocr.recognize(&png(), None, None, CancellationToken::new()).await.unwrap_err().message,
            "OCR worker response exceeds limit"
        );
        std::fs::remove_file(dir.path().join("oversized")).unwrap();
        ocr.recognize(&png(), None, None, CancellationToken::new()).await.unwrap();
    }
    #[test]
    fn default_worker_supports_repo_root_and_browser_control_entrypoints() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("browser-control/ocr")).unwrap();
        std::fs::write(directory.path().join("browser-control/ocr/worker.py"), "").unwrap();
        assert_eq!(default_worker(directory.path()), "browser-control/ocr/worker.py");
        assert_eq!(default_worker(&directory.path().join("browser-control")), "ocr/worker.py");
    }

    #[test]
    fn reference_cache_evicts_by_lru_count_and_serialized_bytes() {
        let mut cache = ReferenceCache::default();
        for index in 0..6 {
            cache.insert([index; 32], json!({"text":"cached"}));
        }
        assert!(cache.get(&[0; 32]).is_some());
        cache.insert([6; 32], json!({"text":"new"}));
        assert_eq!(cache.entries.len(), 6);
        assert!(cache.get(&[1; 32]).is_none());
        assert!(cache.get(&[0; 32]).is_some());
        for index in 7..10 {
            cache.insert([index; 32], json!({"text":"x".repeat(200*1024)}));
        }
        assert!(cache.bytes <= MAX_REFERENCE_CACHE_BYTES);
        assert_eq!(cache.entries.len(), 2);
        assert!(cache.get(&[7; 32]).is_none());
        cache.insert([99; 32], json!({"text":"x".repeat(MAX_REFERENCE_CACHE_BYTES)}));
        assert!(cache.get(&[99; 32]).is_none());
    }

    #[tokio::test]
    async fn cached_reference_cancellation_and_queue_cancellation_leave_cache_usable() {
        let (dir, ocr) = fixture(Duration::from_secs(3));
        let ocr = std::sync::Arc::new(ocr);
        let image = png();
        ocr.recognize_reference(&image, CancellationToken::new()).await.unwrap();
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert_eq!(ocr.recognize_reference(&image, cancelled).await.unwrap_err().message, "OCR request cancelled");
        let cache_guard = ocr.references.lock().await;
        let cancel = CancellationToken::new();
        let queued = {
            let ocr = ocr.clone();
            let image = image.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move { ocr.recognize_reference(&image, cancel).await })
        };
        tokio::task::yield_now().await;
        cancel.cancel();
        assert_eq!(queued.await.unwrap().unwrap_err().message, "OCR request cancelled");
        drop(cache_guard);
        ocr.recognize_reference(&image, CancellationToken::new()).await.unwrap();
        let events = std::fs::read_to_string(dir.path().join("events")).unwrap();
        assert_eq!(events.lines().filter(|line| line.ends_with(" request")).count(), 1);
    }

    #[tokio::test]
    async fn concurrent_reference_requests_share_one_worker_result() {
        let (dir, ocr) = fixture(Duration::from_secs(3));
        let ocr = std::sync::Arc::new(ocr);
        let first = {
            let ocr = ocr.clone();
            tokio::spawn(async move { ocr.recognize_reference(&png(), CancellationToken::new()).await })
        };
        let second = {
            let ocr = ocr.clone();
            tokio::spawn(async move { ocr.recognize_reference(&png(), CancellationToken::new()).await })
        };
        assert_eq!(first.await.unwrap().unwrap(), second.await.unwrap().unwrap());
        let events = std::fs::read_to_string(dir.path().join("events")).unwrap();
        assert_eq!(events.lines().filter(|line| line.ends_with(" request")).count(), 1);
    }
    #[test]
    fn legacy_android_crop_requires_explicit_coordinate_mapping() {
        let response = Response { id: "fixture".into(), width: 20, height: 10, lines: vec![], truncated: false };
        let observation = json!({"caption":"Android crop at x=100, y=200, width=20, height=10 in full screenshot 390x844. Add this origin to crop pixel coordinates."});
        assert!(observation_text(response, Some(&observation)).is_err());
    }
}
