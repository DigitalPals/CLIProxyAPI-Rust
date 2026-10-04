//! Keep valid thinking configuration across tool turns and unsupported disable requests.

use super::*;
use crate::config::{Config, KeyEntry};
use crate::state::App;
use axum::Json;
use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use parking_lot::Mutex;
use std::sync::Arc;

fn chat_request(effort: &str, tool_turn: bool) -> Value {
    let mut body = json!({
        "model":"claude-sonnet-5-5", "reasoning_effort":effort, "max_tokens":4096,
        "messages":[{"role":"user","content":"Read example.rs."}],
        "tools":[{"type":"function","function":{"name":"read_file","parameters":{
            "type":"object","properties":{"path":{"type":"string"}}
        }}}]
    });
    if tool_turn {
        body["messages"].as_array_mut().unwrap().extend([
            json!({"role":"assistant","tool_calls":[{"id":"call_1","type":"function",
                "function":{"name":"read_file","arguments":"{\"path\":\"example.rs\"}"}}]}),
            json!({"role":"tool","tool_call_id":"call_1","content":"fn main() {}"}),
        ]);
    }
    body
}

#[test]
fn adaptive_tool_turns_preserve_thinking_effort_and_prefix() {
    for model in ["claude-sonnet-5-5", "claude-opus-5-5", "claude-fable-5-1", "claude-opus-4-8", "claude-sonnet-4-6"] {
        for effort in ["low", "medium", "high", "xhigh", "max"] {
            let fresh = build_request(&super::super::chat::parse_request(&chat_request(effort, false)).unwrap(), model);
            let replay = build_request(&super::super::chat::parse_request(&chat_request(effort, true)).unwrap(), model);
            assert_eq!(fresh["thinking"]["type"], "adaptive");
            assert_eq!(replay["thinking"], fresh["thinking"], "{model} {effort}");
            assert_eq!(replay["output_config"], fresh["output_config"], "{model} {effort}");
            assert_eq!(replay["messages"][0], fresh["messages"][0]);
            assert_eq!(replay["tools"], fresh["tools"]);
            assert_eq!(replay["messages"][1]["content"][0]["type"], "tool_use");
            assert_eq!(replay["messages"][2]["content"][0]["type"], "tool_result");
        }
    }
}

#[test]
fn explicit_none_uses_valid_thinking_and_preserves_structured_output() {
    for model in [
        "claude-sonnet-5-5",
        "claude-opus-5-5",
        "claude-fable-5-1",
        "claude-fable-5",
        "claude-mythos-5-1",
        "claude-mythos-preview",
    ] {
        let mut body = chat_request("none", true);
        body["response_format"] = json!({"type":"json_schema","json_schema":{
            "name":"answer","schema":{"type":"object","properties":{"ok":{"type":"boolean"}}}
        }});
        let req = super::super::chat::parse_request(&body).unwrap();
        let out = build_request(&req, model);
        if model == "claude-sonnet-5-5" {
            assert_eq!(out["thinking"]["type"], "between_tools");
        } else {
            assert_eq!(out["thinking"]["type"], "adaptive", "{model}");
            assert_eq!(out["output_config"]["effort"], "low", "{model}");
        }
        assert_eq!(out["output_config"]["format"]["type"], "json_schema");
        assert_eq!(out["output_config"]["format"]["schema"], body["response_format"]["json_schema"]["schema"]);
    }
}

#[test]
fn legacy_models_keep_explicit_disabled_thinking() {
    let req = super::super::chat::parse_request(&chat_request("none", true)).unwrap();
    for model in [
        "claude-sonnet-5",
        "claude-opus-5",
        "claude-opus-4-8",
        "claude-sonnet-4-6",
        "claude-sonnet-4-5",
        "claude-haiku-4-5",
    ] {
        let out = build_request(&req, model);
        assert_eq!(out["thinking"]["type"], "disabled", "{model}");
        assert!(out.get("output_config").is_none());
    }
}

#[test]
fn manual_thinking_still_requires_signed_tool_turns() {
    for model in ["claude-sonnet-4-5", "claude-haiku-4-5", "claude-opus-4-1"] {
        let mut req = super::super::chat::parse_request(&chat_request("high", true)).unwrap();
        assert_eq!(build_request(&req, model)["thinking"]["type"], "disabled");
        req.messages[1].parts.insert(
            0,
            Part::Reasoning { text: "Checking the file.".into(), sig: Some(Sig::Claude("signed-thinking".into())) },
        );
        let signed = build_request(&req, model);
        assert_eq!(signed["thinking"]["type"], "enabled");
        assert_eq!(signed["messages"][1]["content"][0]["signature"], "signed-thinking");
        req.messages[1].parts[0] = Part::RedactedReasoning("redacted-thinking".into());
        let redacted = build_request(&req, model);
        assert_eq!(redacted["thinking"]["type"], "enabled");
        assert_eq!(redacted["messages"][1]["content"][0]["data"], "redacted-thinking");
    }
}

#[test]
fn responses_replay_preserves_adaptive_thinking_and_signatures() {
    let body = json!({"reasoning":{"effort":"high"},"input":[
        {"role":"user","content":"Read example.rs."},
        {"type":"reasoning","summary":[{"type":"summary_text","text":"Checking the file."}],"encrypted_content":"cpx-claude:signed-thinking"},
        {"type":"function_call","call_id":"call_1","name":"read_file","arguments":"{\"path\":\"example.rs\"}"},
        {"type":"function_call_output","call_id":"call_1","output":"fn main() {}"}
    ]});
    let req = super::super::responses::parse_request(&body).unwrap();
    let out = build_request(&req, "claude-opus-5-5");
    assert_eq!(out["thinking"]["type"], "adaptive");
    assert_eq!(out["output_config"]["effort"], "high");
    assert_eq!(out["messages"][1]["content"][0]["signature"], "signed-thinking");
}

struct Server {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(router: axum::Router) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await.unwrap();
    });
    Server { url, task }
}

struct Fixture {
    proxy: Server,
    _provider: Server,
    calls: Arc<Mutex<Vec<Value>>>,
    directory: std::path::PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

impl Fixture {
    async fn new() -> Self {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let captured = calls.clone();
        let provider = serve(axum::Router::new().route("/v1/messages", post(move |Json(body): Json<Value>| {
            let calls = captured.clone();
            async move {
                calls.lock().push(body.clone());
                let mode = body["thinking"]["type"].as_str();
                let sonnet = body["model"].as_str().unwrap().starts_with("claude-sonnet-5-5");
                if mode == Some("disabled") || mode == Some("enabled")
                    || mode == Some("between_tools") && (!sonnet || matches!(body["output_config"]["effort"].as_str(), Some("xhigh" | "max"))) {
                    return (StatusCode::BAD_REQUEST, Json(json!({"error":{"message":"unsupported thinking configuration"}}))).into_response();
                }
                let message = json!({"id":"msg_mock","type":"message","role":"assistant","model":body["model"],
                    "content":[{"type":"text","text":"OK"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":1}});
                if body["stream"] != true { return Json(message).into_response(); }
                let events = [
                    json!({"type":"message_start","message":{"id":"msg_mock","model":body["model"],"usage":{"input_tokens":3}}}),
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"OK"}}),
                    json!({"type":"content_block_stop","index":0}),
                    json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
                    json!({"type":"message_stop"}),
                ];
                let data: String = events.iter().map(|v| format!("data: {v}\n\n")).collect();
                Response::builder().header(header::CONTENT_TYPE, "text/event-stream").body(Body::from(data)).unwrap()
            }
        }))).await;
        let directory = std::env::temp_dir().join(format!("cliproxy-claude-thinking-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let cfg = Config {
            auth_dir: directory.to_string_lossy().into(),
            claude_api_key: vec![KeyEntry {
                api_key: "mock-only".into(),
                base_url: Some(provider.url.clone()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let app = App::new(cfg, directory.join("config.yaml"));
        let proxy = serve(crate::server::router(app)).await;
        Self { proxy, _provider: provider, calls, directory }
    }

    async fn request(&self, path: &str, body: Value) -> Value {
        let response = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap()
            .post(format!("{}{path}", self.proxy.url))
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let reply: Value = response.json().await.unwrap();
        assert_eq!(status, StatusCode::OK, "{reply}");
        self.calls.lock().last().unwrap().clone()
    }
}

#[tokio::test]
async fn http_tool_replays_keep_adaptive_thinking_and_effort() {
    let fixture = Fixture::new().await;
    for model in ["claude-sonnet-5-5", "claude-opus-5-5", "claude-fable-5-1"] {
        for effort in ["high", "xhigh", "max"] {
            let mut body = chat_request(effort, true);
            body["model"] = model.into();
            let out = fixture.request("/v1/chat/completions", body).await;
            assert_eq!(out["thinking"]["type"], "adaptive");
            assert_eq!(out["output_config"]["effort"], effort);
            let body = json!({"model":model,"reasoning":{"effort":effort},"input":[
                {"role":"user","content":"Read example.rs."},
                {"type":"function_call","call_id":"call_1","name":"read_file","arguments":"{\"path\":\"example.rs\"}"},
                {"type":"function_call_output","call_id":"call_1","output":"fn main() {}"}
            ]});
            let out = fixture.request("/v1/responses", body).await;
            assert_eq!(out["thinking"]["type"], "adaptive");
            assert_eq!(out["output_config"]["effort"], effort);
        }
    }
}

#[tokio::test]
async fn http_explicit_disable_and_native_suffix_use_supported_modes() {
    let fixture = Fixture::new().await;
    for model in ["claude-sonnet-5-5", "claude-opus-5-5", "claude-fable-5-1"] {
        for native in [false, true] {
            let mut body = if native {
                json!({"model":model,"max_tokens":64,"thinking":{"type":"disabled"},
                "output_config":{"effort":"max","format":{"type":"json_schema","schema":{"type":"object"}}},
                "messages":[{"role":"user","content":"Reply OK."}]})
            } else {
                chat_request("none", true)
            };
            body["model"] = model.into();
            let out = fixture.request(if native { "/v1/messages" } else { "/v1/chat/completions" }, body).await;
            if model == "claude-sonnet-5-5" {
                assert_eq!(out["thinking"]["type"], "between_tools");
                if native {
                    assert_eq!(out["output_config"]["effort"], "high");
                }
            } else {
                assert_eq!(out["thinking"]["type"], "adaptive");
                assert_eq!(out["output_config"]["effort"], "low");
            }
            if native {
                assert_eq!(out["output_config"]["format"]["schema"], json!({"type":"object"}));
            }
        }
        let out = fixture
            .request(
                "/v1/messages",
                json!({"model":format!("{model}(none)"),"max_tokens":64,
            "messages":[{"role":"user","content":"Reply OK."}]}),
            )
            .await;
        assert_ne!(out["thinking"]["type"], "disabled");
    }
}
