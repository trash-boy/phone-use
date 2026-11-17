use super::*;
use std::{future::Future, path::Path};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
};

fn cc_env(socket: &Path) -> HashMap<String, String> {
    HashMap::from([
        ("LLM_BACKEND".into(), "cc".into()),
        ("CC_SOCKET".into(), socket.to_str().unwrap().into()),
        ("AI_OBSERVATION".into(), "image".into()),
    ])
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), future).await.expect("mock bridge operation timed out")
}

struct Bridge {
    _directory: tempfile::TempDir,
    listener: UnixListener,
    model: Model,
}

impl Bridge {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("bridge.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let model = Model::new(&cc_env(&socket)).unwrap();
        Self { _directory: directory, listener, model }
    }

    async fn respond(&self, body: Value) -> (String, Value) {
        let (mut stream, _) = self.listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        let body = serde_json::to_vec(&body).unwrap();
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(headers.as_bytes()).await.unwrap();
        stream.write_all(&body).await.unwrap();
        stream.shutdown().await.unwrap();
        request
    }
}

// Read the entire request before responding or cancelling, so the cancellation
// test proves that an established, in-flight HTTP socket is actually closed.
async fn read_request(stream: &mut UnixStream) -> (String, Value) {
    let mut bytes = Vec::new();
    let header_end = loop {
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).await.unwrap();
        assert!(count > 0, "client disconnected before sending request headers");
        bytes.extend_from_slice(&chunk[..count]);
        assert!(bytes.len() < 128 * 1024, "request headers exceeded mock limit");
    };
    let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
    let length = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim().parse::<usize>().unwrap())
        .unwrap_or(0);
    assert!(length < 1024 * 1024, "request body exceeded mock limit");
    while bytes.len() < header_end + length {
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).await.unwrap();
        assert!(count > 0, "client disconnected before sending request body");
        bytes.extend_from_slice(&chunk[..count]);
    }
    let body = if length == 0 { Value::Null } else { serde_json::from_slice(&bytes[header_end..]).unwrap() };
    (headers, body)
}

#[test]
fn explicit_cc_ignores_legacy_llm_configuration_without_a_bridge() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("missing.sock");
    let mut env = cc_env(&socket);
    // These would fail OpenAI configuration validation if consulted.
    env.extend([
        ("LLM_BASE_URL".into(), "not a URL".into()),
        ("LLM_MODEL".into(), "legacy\nmodel".into()),
        ("LLM_API_KEY".into(), "legacy\r\nsecret".into()),
    ]);
    let model = Model::new(&env).unwrap();
    assert!(!socket.exists());
    assert!(model.cc, "explicit selection must not depend on socket discovery");
    assert!(model.ready(), "CC is configured even when its bridge is offline");
    assert_eq!(model.name.as_deref(), Some("sonnet"));
    assert_eq!(model.cc_models, ["opus", "sonnet", "haiku"]);
    assert_eq!(model.endpoint.as_ref().unwrap().as_str(), "http://localhost/plan");
    assert!(model.key.is_empty(), "legacy API credentials must not enter the CC backend");
}

#[tokio::test]
async fn configured_allowlist_and_task_selection_are_isolated() {
    let directory = tempfile::tempdir().unwrap();
    let mut env = cc_env(&directory.path().join("missing.sock"));
    env.insert("CC_MODELS".into(), " sonnet, opus,sonnet, custom-model ".into());
    env.insert("CC_MODEL".into(), " sonnet ".into());
    let model = Model::new(&env).unwrap();
    assert_eq!(
        model.model_names().await,
        (vec![String::from("sonnet"), String::from("opus"), String::from("custom-model")], false)
    );

    let opus = model.select(Some(&json!("opus"))).await.unwrap();
    let custom = model.select(Some(&json!("custom-model"))).await.unwrap();
    let default = model.select(None).await.unwrap();
    assert_eq!(model.name.as_deref(), Some("sonnet"));
    assert_eq!(default.name, model.name);
    assert_eq!(opus.name.as_deref(), Some("opus"));
    assert_eq!(custom.name.as_deref(), Some("custom-model"));
    for selected in [&opus, &custom, &default] {
        assert!(selected.cc, "task selection must retain the backend");
        assert_eq!(selected.endpoint, model.endpoint);
        assert_eq!(selected.cc_models, model.cc_models);
        assert!(selected.key.is_empty());
    }
    for invalid in [json!("haiku"), json!("unknown"), json!(" opus "), json!("opus\n"), json!(null), json!(42)] {
        let error = model.select(Some(&invalid)).await.err().expect("invalid model selection was accepted");
        assert_eq!(error.status, 400, "{invalid}");
        assert_eq!(model.name.as_deref(), Some("sonnet"));
        assert_eq!(opus.name.as_deref(), Some("opus"));
    }
}

#[test]
fn cc_configuration_rejects_invalid_allowlists_and_defaults() {
    let directory = tempfile::tempdir().unwrap();
    for (models, default) in [
        ("", "opus"),
        ("opus,", "opus"),
        ("opus,son\nnet", "opus"),
        ("sonnet,haiku", "opus"),
        ("opus,sonnet", ""),
        ("opus,sonnet", "opus\ninvalid"),
    ] {
        let mut env = cc_env(&directory.path().join("missing.sock"));
        env.insert("CC_MODELS".into(), models.into());
        env.insert("CC_MODEL".into(), default.into());
        assert!(Model::new(&env).is_err(), "accepted CC_MODELS={models:?}, CC_MODEL={default:?}");
    }
}

#[tokio::test]
async fn configuration_reports_missing_bridge_without_losing_configured_models() {
    let directory = tempfile::tempdir().unwrap();
    let model = Model::new(&cc_env(&directory.path().join("missing.sock"))).unwrap();
    let config = bounded(model.configuration()).await;
    assert_eq!(
        config,
        json!({
            "backend":"cc", "aiReady":false, "model":"sonnet", "models":["opus","sonnet","haiku"],
            "aiError":"CC bridge is unavailable"
        })
    );
    assert!(model.ready(), "health must not erase configured backend state");
    assert!(model.select(Some(&json!("sonnet"))).await.unwrap().cc);
}

#[tokio::test]
async fn configuration_uses_unix_bridge_health() {
    let bridge = Bridge::new();
    let (config, (headers, body)) =
        tokio::join!(bounded(bridge.model.configuration()), bounded(bridge.respond(json!({"ok":true}))));
    assert!(headers.starts_with("GET /health HTTP/1.1\r\n"));
    assert_eq!(body, Value::Null);
    assert_eq!(config, json!({"backend":"cc","aiReady":true,"model":"sonnet","models":["opus","sonnet","haiku"]}));
}

#[tokio::test]
async fn screenshot_plan_sends_selected_model_evidence_and_schema_over_unix_http() {
    let bridge = Bridge::new();
    let selected = bridge.model.select(Some(&json!("opus"))).await.unwrap();
    let mut jpeg = std::io::Cursor::new(Vec::new());
    image::RgbImage::new(2, 3).write_to(&mut jpeg, image::ImageFormat::Jpeg).unwrap();
    let screenshot = base64::engine::general_purpose::STANDARD.encode(jpeg.into_inner());
    let observation = json!({
        "observationId":"cc-screen-1", "caption":"2x3 phone points", "elementsText":"Button: Settings",
        "imageBase64":screenshot
    });
    let history = vec![json!({"tool":"iphone_list_apps","result":{"apps":["Settings"]}})];
    let reply = json!({"type":"action","tool":"iphone_screenshot","args":{},"reason":"Inspect the screen","requiresConfirmation":false});
    let (result, (headers, request)) = tokio::join!(
        bounded(selected.generate("Inspect Settings", observation, history.clone(), &[], CancellationToken::new())),
        bounded(bridge.respond(json!({"reply":reply})))
    );
    assert_eq!(result.unwrap(), reply);
    assert!(headers.starts_with("POST /plan HTTP/1.1\r\n"));
    assert!(!headers.to_ascii_lowercase().contains("authorization:"));
    assert_eq!(request["model"], "opus");
    assert_eq!(bridge.model.name.as_deref(), Some("sonnet"));
    let system = request["system"].as_str().unwrap();
    let (_, schema) = system.rsplit_once("Respond only with a JSON object matching this schema: ").unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(schema).unwrap(),
        serde_json::from_str::<Value>(include_str!("model-reply.schema.json")).unwrap()
    );
    let content = request["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["type"], "text");
    let context: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(context["instruction"], "Inspect Settings");
    assert_eq!(context["recentHistory"], json!(history));
    assert_eq!(context["observation"]["observationId"], "cc-screen-1");
    assert_eq!(context["observation"]["caption"], "2x3 phone points");
    assert_eq!(context["observation"]["elementsText"], "Button: Settings");
    assert_eq!(
        content[1],
        json!({"type":"image_url","image_url":{"url":format!("data:image/jpeg;base64,{screenshot}")}})
    );
}

#[tokio::test]
async fn bridge_replies_are_validated_before_becoming_device_actions() {
    for (reply, detail) in [
        (json!({"type":"action","tool":"unknown","args":{},"reason":"Inspect"}), "tool is not supported"),
        (json!({"type":"action","tool":"iphone_screenshot","args":{"x":1},"reason":"Inspect"}), "args:"),
        (json!({"type":"action","tool":"iphone_tap","args":{"x":"bad","y":20},"reason":"Open"}), "args:"),
        (json!({"type":"done","message":"OK","tool":"iphone_screenshot"}), "done only accepts"),
        (json!("{\"type\":\"done\",\"message\":\"OK\"}"), "expected a JSON object"),
    ] {
        let bridge = Bridge::new();
        // An invalid reply gets one correction attempt, like the OpenAI path.
        let replies = async {
            let first = bridge.respond(json!({"reply":reply})).await;
            let second = bridge.respond(json!({"reply":reply})).await;
            (first, second)
        };
        let (result, (_, (_, correction))) = tokio::join!(
            bounded(bridge.model.generate("Inspect", json!({"queryOnly":true}), vec![], &[], CancellationToken::new())),
            bounded(replies)
        );
        let last = correction["content"].as_array().unwrap().last().unwrap()["text"].as_str().unwrap().to_owned();
        assert!(last.starts_with("Your previous reply failed validation"), "{last}");
        assert!(correction.get("followUp").is_none(), "no follow-up without a bridge thread history");
        let error = result.unwrap_err();
        assert_eq!(error.status, 503, "{reply}");
        assert!(error.message.starts_with("Invalid model reply:"), "{}", error.message);
        assert!(error.message.contains(detail), "{}", error.message);
    }
}

#[tokio::test]
async fn cancelling_an_inflight_plan_closes_the_unix_socket() {
    let bridge = Bridge::new();
    let cancel = CancellationToken::new();
    let observe_disconnect = async {
        let (mut stream, _) = bridge.listener.accept().await.unwrap();
        let (headers, request) = read_request(&mut stream).await;
        assert!(headers.starts_with("POST /plan HTTP/1.1\r\n"));
        assert_eq!(request["model"], "sonnet");
        // Withhold the HTTP response: the client must disconnect while its
        // request is pending, even though Model retains its reqwest client.
        cancel.cancel();
        let mut byte = [0; 1];
        assert_eq!(stream.read(&mut byte).await.unwrap(), 0, "cancelled request socket remained open");
    };
    let (result, ()) = tokio::join!(
        bounded(bridge.model.generate("Inspect", json!({"queryOnly":true}), vec![], &[], cancel.clone())),
        bounded(observe_disconnect)
    );
    let error = result.unwrap_err();
    assert_eq!(error.status, 503);
    assert_eq!(error.message, "Model request cancelled");
    assert!(bridge.model.ready());
}

#[tokio::test]
async fn follow_ups_deadlines_and_corrections_reach_the_bridge() {
    let bridge = Bridge::new();
    let mut jpeg = std::io::Cursor::new(Vec::new());
    image::RgbImage::new(2, 3).write_to(&mut jpeg, image::ImageFormat::Jpeg).unwrap();
    let screenshot = base64::engine::general_purpose::STANDARD.encode(jpeg.into_inner());
    let observation = Observation::from_value(
        json!({"observationId":"step-2","caption":"2x3","elementsText":"Button: OK","imageBase64":screenshot}),
    );
    let follow_up = FollowUp {
        instruction: None,
        steps: vec![json!({"tool":"iphone_tap","state":"succeeded","result":{"kind":"text","text":"Tapped"}})],
    };
    let request = PlanRequest {
        instruction: "Play",
        observation: &observation,
        history: vec![json!({"tool":"iphone_tap","old":true})],
        follow_up: Some(follow_up),
        images: &[],
        session: Some("session-1"),
    };
    let valid = json!({"type":"done","message":"won"});
    let replies = async {
        let first = bridge.respond(json!({"reply":{"type":"action","tool":"unknown"}})).await;
        let second = bridge.respond(json!({"reply":valid})).await;
        (first.1, second.1)
    };
    let (result, (first, second)) =
        tokio::join!(bounded(bridge.model.plan(request, CancellationToken::new())), bounded(replies));
    assert_eq!(result.unwrap(), valid);
    assert_eq!(first["session"], "session-1");
    let deadline = first["deadlineMs"].as_u64().unwrap();
    assert!(deadline > 90_000 && deadline <= 95_000, "{deadline}");
    assert!(second["deadlineMs"].as_u64().unwrap() <= deadline);
    // Full content for a fresh thread still carries the whole history.
    let context: Value = serde_json::from_str(first["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(context["recentHistory"][0]["old"], true);
    // The follow-up is the delta: new observation, screenshot and the step since the last plan.
    let follow_up = first["followUp"].as_array().unwrap();
    assert_eq!(follow_up.len(), 2);
    let delta: Value = serde_json::from_str(follow_up[0]["text"].as_str().unwrap()).unwrap();
    assert!(delta.get("recentHistory").is_none() && delta.get("instruction").is_none());
    assert_eq!(delta["stepsSinceLastPlan"][0]["result"]["text"], "Tapped");
    assert_eq!(delta["observation"]["observationId"], "step-2");
    assert_eq!(delta["observation"]["elementsText"], "Button: OK");
    assert_eq!(follow_up[1]["type"], "image_url");
    // The correction follow-up only adds the rejected reply and the correction.
    let correction = second["followUp"].as_array().unwrap();
    assert_eq!(correction.len(), 2);
    assert!(correction[0]["text"].as_str().unwrap().starts_with("Your previous reply was:"));
    assert!(correction[1]["text"].as_str().unwrap().starts_with("Your previous reply failed validation"));
    assert_eq!(second["content"].as_array().unwrap().len(), first["content"].as_array().unwrap().len() + 2);
}

#[tokio::test]
async fn closing_a_session_tells_the_bridge_and_ignores_its_answer() {
    let bridge = Bridge::new();
    bridge.model.close_session("session-1");
    let (mut stream, _) = bounded(bridge.listener.accept()).await.unwrap();
    let (headers, body) = bounded(read_request(&mut stream)).await;
    assert!(headers.starts_with("POST /session/close HTTP/1.1\r\n"));
    assert_eq!(body, json!({"session":"session-1"}));
    stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await.unwrap();
}

#[tokio::test]
async fn configuration_probes_are_cached_for_concurrent_and_repeated_callers() {
    let bridge = Bridge::new();
    let (first, second, (headers, _)) = tokio::join!(
        bounded(bridge.model.configuration()),
        bounded(bridge.model.configuration()),
        bounded(bridge.respond(json!({"ok":true})))
    );
    assert!(headers.starts_with("GET /health"));
    assert_eq!(first, second);
    assert_eq!(bounded(bridge.model.configuration()).await["aiReady"], true, "no second probe within the TTL");
}
