//! Real Axum + mocked upstream acceptance. No provider credentials or personal histories.
use super::*;
use crate::{
    config::{Config, KeyEntry, NamedClient},
    state::App,
};
use axum::{
    Json, Router,
    extract::{
        State,
        ws::{Message, WebSocketUpgrade},
    },
    http::StatusCode,
    response::IntoResponse,
    routing::post,
};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

struct Server {
    url: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn serve(router: Router) -> Server {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(l, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await.unwrap();
    });
    Server { url, task }
}
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("fusebox-usage-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn complete(id: String) -> Value {
    json!({"id":id,"object":"response","status":"completed","model":"gpt-6.1-sol","service_tier":"default","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"safe answer"}]}],"usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":20,"cache_write_tokens":10},"output_tokens":40,"output_tokens_details":{"reasoning_tokens":30}}})
}
async fn mock(State(count): State<Arc<AtomicUsize>>, Json(body): Json<Value>) -> axum::response::Response {
    let n = count.fetch_add(1, Ordering::SeqCst);
    let v = complete(format!("resp_{n}"));
    if body["metadata"]["retry"] == true && n == 0 {
        return (StatusCode::SERVICE_UNAVAILABLE,Json(json!({"error":{"message":"synthetic unavailable"},"model":"gpt-6.1-sol","service_tier":"default","usage":{"input_tokens":50,"output_tokens":10}}))).into_response();
    }
    if body["stream"] == true {
        let event = json!({"type":"response.completed","response":v});
        let text = format!("data: {event}\n\ndata: {event}\n\n");
        return ([("content-type", "text/event-stream")], text).into_response();
    }
    Json(v).into_response()
}
async fn mock_ws(ws: WebSocketUpgrade) -> axum::response::Response {
    ws.on_upgrade(|mut socket| async move {
        while let Some(Ok(Message::Text(_))) = socket.recv().await {
            let event =
                json!({"type":"response.completed","response":complete(format!("resp_ws_{}",uuid::Uuid::new_v4()))});
            if socket.send(Message::Text(event.to_string().into())).await.is_err() {
                break;
            }
        }
    })
}
fn config(dir: &Directory, url: &str, native: bool) -> Config {
    let mut cfg = Config {
        auth_dir: dir.0.join("auth").to_string_lossy().into(),
        management_key: "synthetic-management".into(),
        named_clients: vec![NamedClient {
            id: "desktop".into(),
            label: "Desktop <safe>".into(),
            key: "synthetic-inference".into(),
        }],
        proxy_url: "direct".into(),
        codex_websockets: native,
        ..Default::default()
    };
    std::fs::create_dir_all(cfg.auth_dir()).unwrap();
    cfg.usage.database = Some(dir.0.join("usage.sqlite3").to_string_lossy().into());
    if native {
        cfg.proxy_url.clear();
        std::fs::write(cfg.auth_dir().join("mock.json"),json!({"type":"codex","access_token":"synthetic-upstream","email":"test@example.invalid","base_url":format!("{url}/v1")}).to_string()).unwrap();
    } else {
        cfg.codex_api_key = vec![KeyEntry {
            api_key: "synthetic-upstream".into(),
            base_url: Some(format!("{url}/v1")),
            ..Default::default()
        }];
    }
    cfg
}
async fn summary(app: &App) -> Value {
    app.usage.as_ref().unwrap().flush().await.unwrap();
    app.usage.as_ref().unwrap().reference_summary(store::Query::default()).await.unwrap()
}

#[tokio::test]
async fn retired_summary_needs_management_auth_but_not_an_analytics_store() {
    let dir = Directory::new();
    let mut cfg = config(&dir, "http://127.0.0.1:9", false);
    cfg.usage.enabled = false;
    let app = App::new(cfg, dir.0.join("config.yaml"));
    assert!(app.usage.is_none());
    let proxy = serve(crate::server::router(app)).await;
    let client = reqwest::Client::new();
    for suffix in ["", "?timezone=invalid&start=not-a-date"] {
        let response = client
            .get(format!("{}/api/usage/summary{suffix}", proxy.url))
            .bearer_auth("synthetic-management")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::GONE);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["code"], "usage_summary_retired");
        assert_eq!(body["replacement"], "/api/usage/dashboard");
        assert!(body.get("combined").is_none());
    }
    let response =
        client.get(format!("{}/api/usage/summary", proxy.url)).bearer_auth("synthetic-inference").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = client
        .get(format!("{}/api/usage/dashboard", proxy.url))
        .bearer_auth("synthetic-management")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(!dir.0.join("usage.sqlite3").exists());
}

#[tokio::test]
async fn historical_proxy_exceeds_ring_survives_restart_and_streams_do_not_double_count() {
    let dir = Directory::new();
    let count = Arc::new(AtomicUsize::new(0));
    let upstream = serve(
        Router::new().route("/v1/responses", post(mock)).route("/v1/responses/compact", post(mock)).with_state(count),
    )
    .await;
    let cfg = config(&dir, &upstream.url, false);
    let app = App::new(cfg.clone(), dir.0.join("config.yaml"));
    let proxy = serve(crate::server::router(app.clone())).await;
    let client = reqwest::Client::new();
    for n in 0..305 {
        let r = client
            .post(format!("{}/v1/responses", proxy.url))
            .bearer_auth("synthetic-inference")
            .json(&json!({"model":"gpt-6.1-sol","input":"PRIVATE_TRANSCRIPT_MARKER","stream":n%2==0}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        r.bytes().await.unwrap();
    }
    let r = client
        .post(format!("{}/v1/responses/compact", proxy.url))
        .bearer_auth("synthetic-inference")
        .json(&json!({"model":"gpt-6.1-sol","input":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let s = summary(&app).await;
    assert_eq!(s["proxy"]["logical_requests"], 306);
    assert_eq!(s["proxy"]["attempts"], 306);
    assert_eq!(s["proxy"]["tokens"]["input"], 306 * 70);
    assert_eq!(s["proxy"]["tokens"]["output"], 306 * 40);
    assert!(app.stats.recent.lock().len() <= 300);
    assert!(!serde_json::to_string(&s).unwrap().contains("PRIVATE_TRANSCRIPT_MARKER"));
    let exported = client
        .get(format!("{}/api/usage/export?format=json&limit=50&offset=0&timezone=UTC", proxy.url))
        .bearer_auth("synthetic-management")
        .send()
        .await
        .unwrap();
    assert_eq!(exported.status(), StatusCode::OK);
    let exported = exported.text().await.unwrap();
    assert!(!exported.contains("PRIVATE_TRANSCRIPT_MARKER"));
    assert!(!exported.contains("synthetic-inference"));
    drop(proxy);
    drop(app);
    let restarted = App::new(cfg, dir.0.join("config.yaml"));
    assert_eq!(summary(&restarted).await["proxy"]["attempts"], 306);
}
#[tokio::test]
async fn retries_keep_failed_usage_one_logical_request() {
    let dir = Directory::new();
    let upstream =
        serve(Router::new().route("/v1/responses", post(mock)).with_state(Arc::new(AtomicUsize::new(0)))).await;
    let app = App::new(config(&dir, &upstream.url, false), dir.0.join("config.yaml"));
    let proxy = serve(crate::server::router(app.clone())).await;
    let r = reqwest::Client::new()
        .post(format!("{}/v1/responses", proxy.url))
        .bearer_auth("synthetic-inference")
        .json(&json!({"model":"gpt-6.1-sol","input":"safe","metadata":{"retry":true}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    r.bytes().await.unwrap();
    let s = summary(&app).await;
    assert_eq!(s["proxy"]["logical_requests"], 1);
    assert_eq!(s["proxy"]["attempts"], 2);
    assert_eq!(s["proxy"]["tokens"]["output"], 50);
    assert_eq!(s["proxy"]["partial"], 1);
}
#[tokio::test]
async fn native_websocket_and_translated_http_capture_usage() {
    let dir = Directory::new();
    let upstream =
        serve(Router::new().route("/v1/responses", post(mock).get(mock_ws)).with_state(Arc::new(AtomicUsize::new(0))))
            .await;
    let app = App::new(config(&dir, &upstream.url, true), dir.0.join("config.yaml"));
    let proxy = serve(crate::server::router(app.clone())).await;
    use tokio_tungstenite::tungstenite::{Message as WsMessage, client::IntoClientRequest};
    let mut req = format!("{}/v1/responses", proxy.url.replace("http:", "ws:")).into_client_request().unwrap();
    req.headers_mut().insert("authorization", "Bearer synthetic-inference".parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws.send(WsMessage::Text(json!({"type":"response.create","model":"gpt-6.1-sol","input":[]}).to_string().into()))
        .await
        .unwrap();
    let text = ws.next().await.unwrap().unwrap().into_text().unwrap();
    assert!(text.contains("response.completed"));
    ws.close(None).await.unwrap();
    let r=reqwest::Client::new().post(format!("{}/v1/messages",proxy.url)).bearer_auth("synthetic-inference").json(&json!({"model":"gpt-6.1-sol","max_tokens":100,"messages":[{"role":"user","content":"PRIVATE_TRANSCRIPT_MARKER"}]})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    r.bytes().await.unwrap();
    let s = summary(&app).await;
    assert_eq!(s["proxy"]["attempts"], 2);
    assert_eq!(s["proxy"]["tokens"]["cache_write"], 20);
    assert_eq!(s["proxy"]["tokens"]["reasoning"], 60);
}
#[tokio::test]
async fn cancelled_interrupted_and_missing_usage_remain_explicit() {
    let dir = Directory::new();
    let upstream = serve(Router::new().route("/v1/responses", post(|Json(body): Json<Value>| async move {
        if body["metadata"]["case"] == "missing" {
            let mut response = complete("resp_missing".into());
            response.as_object_mut().unwrap().remove("usage");
            return Json(response).into_response();
        }
        let cancel = body["metadata"]["case"] == "cancel";
        let event = json!({"type":"response.in_progress","response":complete(if cancel {"resp_cancel"} else {"resp_interrupted"}.into())});
        let stream = async_stream::stream! {
            yield Ok::<_, std::convert::Infallible>(format!("data: {event}\n\n"));
            if cancel {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    yield Ok(": heartbeat\n\n".to_owned());
                }
            }
        };
        ([("content-type", "text/event-stream")], axum::body::Body::from_stream(stream)).into_response()
    }))).await;
    let app = App::new(config(&dir, &upstream.url, false), dir.0.join("config.yaml"));
    let proxy = serve(crate::server::router(app.clone())).await;
    let client = reqwest::Client::new();
    for case in ["missing", "interrupted", "cancel"] {
        let response = client
            .post(format!("{}/v1/responses", proxy.url))
            .bearer_auth("synthetic-inference")
            .json(&json!({"model":"gpt-6.1-sol","input":[],"stream":case!="missing","metadata":{"case":case}}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "case {case}");
        if case == "cancel" {
            let mut stream = response.bytes_stream();
            assert!(stream.next().await.unwrap().is_ok());
            drop(stream);
        } else {
            response.bytes().await.unwrap();
        }
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if summary(&app).await["proxy"]["attempts"] == 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let s = summary(&app).await;
    assert_eq!(s["proxy"]["logical_requests"], 3);
    assert_eq!(s["proxy"]["missing_usage"], 1);
    assert_eq!(s["proxy"]["partial"], 2);
    assert_eq!(s["proxy"]["tokens"]["output"], 80);
    let details = app.usage.as_ref().unwrap().details(store::Query::default()).await.unwrap();
    let serialized = details.to_string();
    assert!(serialized.contains("499"));
    assert!(serialized.contains("502"));
}
#[tokio::test]
async fn analytics_authentication_and_collector_scope() {
    let dir = Directory::new();
    let mut cfg = config(&dir, "http://127.0.0.1:1", false);
    cfg.codex_api_key.clear();
    let app = App::new(cfg, dir.0.join("config.yaml"));
    let proxy = serve(crate::server::router(app.clone())).await;
    let client = reqwest::Client::new();
    for key in ["", "synthetic-inference", "fbxc_invalid"] {
        for endpoint in ["summary", "dashboard"] {
            let r = client.get(format!("{}/api/usage/{endpoint}", proxy.url)).bearer_auth(key).send().await.unwrap();
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        }
    }
    let r = client
        .get(format!("{}/api/usage/dashboard", proxy.url))
        .bearer_auth("synthetic-management")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let v: Value = r.json().await.unwrap();
    assert!(v["combined"]["trends"]["model"].is_array());
    let r = client
        .post(format!("{}/api/usage/collectors", proxy.url))
        .bearer_auth("synthetic-management")
        .json(&json!({"label":"<script>test</script>"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let v: Value = r.json().await.unwrap();
    let credential = v["credential"].as_str().unwrap();
    let r = client
        .post(format!("{}/v1/responses", proxy.url))
        .bearer_auth(credential)
        .json(&json!({"model":"gpt-6.1-sol","input":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = client.get(format!("{}/api/accounts", proxy.url)).bearer_auth(credential).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = client
        .post(format!("{}/api/usage-ingest", proxy.url))
        .bearer_auth(credential)
        .body("x".repeat(600 * 1024))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let r =
        client.get(format!("{}/api/usage/status", proxy.url)).bearer_auth("synthetic-management").send().await.unwrap();
    let text = r.text().await.unwrap();
    assert!(!text.contains(credential));
    assert!(text.contains("<script>test</script>"));
    let mut cfg = app.cfg().as_ref().clone();
    cfg.management_key.clear();
    cfg.named_clients.clear();
    app.set_config(cfg);
    let r = client.get(format!("{}/api/usage/status", proxy.url)).bearer_auth(credential).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = client.get(format!("{}/v1/models", proxy.url)).bearer_auth(credential).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}
