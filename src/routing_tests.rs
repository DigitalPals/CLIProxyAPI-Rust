//! Exercise actual HTTP and WebSocket routes against token-free local providers.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use axum::Json;
use axum::extract::{
    State,
    ws::{Message, WebSocketUpgrade},
};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};

use crate::config::{Config, KeyEntry, Routing};
use crate::state::{App, RequestLog};

#[derive(Default)]
struct Mock {
    mode: AtomicU8,
    sequence: AtomicU64,
    calls: Mutex<Vec<(String, Value, &'static str)>>,
}

fn account(headers: &HeaderMap) -> String {
    headers
        .get("authorization")
        .or_else(|| headers.get("x-api-key"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .trim_start_matches("Bearer ")
        .into()
}

fn completed(mock: &Mock, account: &str, body: &Value) -> Value {
    json!({"id":format!("resp_{}", mock.sequence.fetch_add(1, Ordering::Relaxed)), "object":"response", "status":"completed", "model":body["model"], "output":[{"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":account}]}], "usage":{"input_tokens":100, "input_tokens_details":{"cached_tokens":80}, "output_tokens":1}})
}

fn events(response: &Value, omit_output: bool) -> Vec<Value> {
    let mut terminal = response.clone();
    if omit_output {
        terminal["output"] = json!([]);
    }
    vec![
        json!({"type":"response.created", "response":{"id":response["id"], "model":response["model"]}}),
        json!({"type":"response.output_text.delta", "delta":response["output"][0]["content"][0]["text"], "output_index":0, "content_index":0}),
        json!({"type":"response.completed", "response":terminal}),
    ]
}

async fn mock_http(State(mock): State<Arc<Mock>>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let account = account(&headers);
    mock.calls.lock().push((account.clone(), body.clone(), "http"));
    let mode = mock.mode.load(Ordering::Relaxed);
    if account == "a" && ((1..=3).contains(&mode) || mode == 6) {
        let (status, code) = match mode {
            1 => (429, "usage_limit_reached"),
            2 => (429, "rate_limit_exceeded"),
            6 => (401, "invalid_api_key"),
            _ => (503, "server_is_overloaded"),
        };
        return (
            StatusCode::from_u16(status).unwrap(),
            [("retry-after", "120")],
            Json(json!({"error":{"code":code, "message":"mock error", "resets_in_seconds":3600}})),
        )
            .into_response();
    }
    if account == "a" && mode == 4 {
        let event = json!({"type":"response.failed", "response":{"error":{"code":"usage_limit_reached", "message":"mock error"}}});
        return ([("content-type", "text/event-stream")], format!("data: {event}\n\n")).into_response();
    }
    if body["messages"].is_array() {
        return Json(json!({"id":"message", "type":"message", "role":"assistant", "model":body["model"], "content":[{"type":"text", "text":account}], "stop_reason":"end_turn", "usage":{"input_tokens":100,"output_tokens":1}})).into_response();
    }
    let response = completed(&mock, &account, &body);
    let mut out = if body["stream"] == true {
        let sse: String = events(&response, true).into_iter().map(|v| format!("data: {v}\n\n")).collect();
        ([("content-type", "text/event-stream")], sse).into_response()
    } else {
        Json(response).into_response()
    };
    if account == "a" && mode == 5 {
        for (k, v) in [
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-used-percent", "100"),
            ("x-codex-primary-reset-after-seconds", "3600"),
        ] {
            out.headers_mut().insert(k, v.parse().unwrap());
        }
    }
    out
}

async fn mock_chat(State(mock): State<Arc<Mock>>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let account = account(&headers);
    mock.calls.lock().push((account.clone(), body.clone(), "chat"));
    Json(json!({
        "id":"chat-response", "object":"chat.completion", "model":body["model"],
        "choices":[{"index":0,"message":{"role":"assistant","content":account},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":100,"completion_tokens":1,"prompt_tokens_details":{"cached_tokens":80}}
    }))
    .into_response()
}

async fn mock_ws(State(mock): State<Arc<Mock>>, headers: HeaderMap, upgrade: WebSocketUpgrade) -> Response {
    let account = account(&headers);
    upgrade.on_upgrade(move |mut socket| async move {
        let mut known = std::collections::HashSet::<String>::new();
        while let Some(Ok(Message::Text(text))) = socket.next().await {
            let body: Value = serde_json::from_str(&text).unwrap();
            mock.calls.lock().push((account.clone(), body.clone(), "ws"));
            if account == "a" && mock.mode.load(Ordering::Relaxed) == 1 {
                let error = json!({"type":"error", "status":429, "error":{"code":"usage_limit_reached", "message":"mock error", "resets_in_seconds":3600}});
                if socket.send(Message::Text(error.to_string().into())).await.is_err() { break; }
                continue;
            }
            if let Some(prev) = body["previous_response_id"].as_str() && !known.contains(prev) {
                let error = json!({"type":"error", "status":400, "error":{"code":"previous_response_not_found", "message":"unknown on this socket"}});
                if socket.send(Message::Text(error.to_string().into())).await.is_err() { break; }
                continue;
            }
            let response = completed(&mock, &account, &body);
            known.insert(response["id"].as_str().unwrap().to_string());
            for event in events(&response, true) {
                if socket.send(Message::Text(event.to_string().into())).await.is_err() { return; }
            }
        }
    }).into_response()
}

struct Server {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

async fn serve(router: axum::Router) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await.unwrap();
    });
    Server { url, task }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Fixture {
    directory: PathBuf,
    cfg: Config,
    app: Arc<App>,
    proxy: Server,
    provider: Server,
    mock: Arc<Mock>,
}

impl Fixture {
    async fn new(routing: Routing, native: bool) -> Self {
        let mock = Arc::new(Mock::default());
        let provider = serve(
            axum::Router::new()
                .route("/v1/responses", post(mock_http).get(mock_ws))
                .route("/v1/responses/compact", post(mock_http))
                .route("/v1/messages", post(mock_http))
                .route("/v1/chat/completions", post(mock_chat))
                .with_state(mock.clone()),
        )
        .await;
        let directory = std::env::temp_dir().join(format!("cliproxy-routing-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut cfg = Config {
            auth_dir: directory.to_string_lossy().into(),
            api_keys: vec!["client-one".into(), "client-two".into()],
            routing,
            codex_websockets: native,
            ..Default::default()
        };
        if native {
            for account in ["a", "b"] {
                std::fs::write(directory.join(format!("{account}.json")), json!({"type":"codex", "access_token":account, "email":account, "base_url":format!("{}/v1", provider.url)}).to_string()).unwrap();
            }
        } else {
            cfg.codex_api_key = ["a", "b"]
                .map(|account| KeyEntry {
                    api_key: account.into(),
                    base_url: Some(format!("{}/v1", provider.url)),
                    label: Some(account.into()),
                    ..Default::default()
                })
                .to_vec();
        }
        let app = App::new(cfg.clone(), directory.join("config.yaml"));
        let proxy = serve(crate::server::router(app.clone())).await;
        Self { directory, cfg, app, proxy, provider, mock }
    }

    async fn request(&self, task: Option<&str>, body: Value) -> (u16, Value) {
        let mut request = reqwest::Client::new()
            .post(format!("{}/v1/responses", self.proxy.url))
            .bearer_auth("client-one")
            .json(&body);
        if let Some(task) = task {
            request = request.header("thread-id", task);
        }
        let response = request.send().await.unwrap();
        (response.status().as_u16(), response.json().await.unwrap())
    }

    async fn socket(
        &self,
        task: &str,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
        self.socket_with_task(Some(task)).await
    }

    async fn socket_with_task(
        &self,
        task: Option<&str>,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
        let mut request =
            format!("{}/v1/responses", self.proxy.url.replacen("http://", "ws://", 1)).into_client_request().unwrap();
        request.headers_mut().insert("authorization", "Bearer client-one".parse().unwrap());
        if let Some(task) = task {
            request.headers_mut().insert("thread-id", task.parse().unwrap());
        }
        tokio_tungstenite::connect_async(request).await.unwrap().0
    }

    async fn logs(&self, count: usize) -> Vec<RequestLog> {
        // A WebSocket's terminal event can reach the client just before its tracker finishes.
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let mut logs: Vec<_> = self.app.stats.recent.lock().iter().cloned().collect();
                if logs.len() >= count {
                    logs.sort_by_key(|log| log.id);
                    return logs;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("request diagnostics were not recorded")
    }

    fn recover(&self) {
        self.mock.mode.store(0, Ordering::Relaxed);
        for account in self.app.pool.all() {
            let mut state = account.state.lock();
            state.cooldowns.clear();
            state.quota_cooldowns.clear();
            state.quota = Default::default();
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.proxy.task.abort();
        self.provider.task.abort();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn prompt() -> Value {
    json!({"model":"gpt-6.1-sol","input":"question"})
}
fn answer(response: &Value) -> &str {
    response["output"][0]["content"][0]["text"].as_str().unwrap()
}

async fn turn(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    mut body: Value,
) -> Value {
    body["type"] = "response.create".into();
    socket.send(tungstenite::Message::Text(body.to_string().into())).await.unwrap();
    loop {
        let frame =
            tokio::time::timeout(std::time::Duration::from_secs(10), socket.next()).await.unwrap().unwrap().unwrap();
        if let tungstenite::Message::Text(text) = frame {
            let event: Value = serde_json::from_str(&text).unwrap();
            assert_ne!(event["type"], "error", "{event}");
            if event["type"] == "response.completed" {
                return event["response"].clone();
            }
        }
    }
}

#[tokio::test]
async fn smart_balancing_uses_reserve_and_session_load_over_http_and_websockets() {
    for native in [false, true] {
        let fixture = Fixture::new(Routing::SmartQuota, native).await;
        let accounts = fixture.app.pool.all();
        let now = chrono::Utc::now();
        for (account, days) in accounts.iter().zip([3, 5]) {
            account.state.lock().quota = crate::quota::Quota {
                windows: vec![
                    crate::quota::Window {
                        name: "5h".into(),
                        used: 0.0,
                        resets_at: Some(now + chrono::Duration::hours(4)),
                        model: None,
                    },
                    crate::quota::Window {
                        name: "week".into(),
                        used: 20.0,
                        resets_at: Some(now + chrono::Duration::days(days)),
                        model: None,
                    },
                ],
                updated_at: Some(now),
                ..Default::default()
            };
        }
        let first = fixture.request(Some("http-first"), prompt()).await;
        assert_eq!(first.0, 200);
        assert_eq!(answer(&first.1), "a");
        let mut socket = fixture.socket("ws-second").await;
        turn(&mut socket, prompt()).await;
        assert_eq!(fixture.mock.calls.lock().last().unwrap().0, "b"); // new load counts before quota moves
        assert_eq!(answer(&fixture.request(Some("http-first"), prompt()).await.1), "a");

        // Change the reserve at runtime without disturbing either existing assignment.
        let mut cfg = fixture.cfg.clone();
        cfg.five_hour_reserve_percent = 60;
        fixture.app.set_config(cfg);
        accounts[0].state.lock().quota.windows[0].used = 41.0;
        assert_eq!(answer(&fixture.request(Some("new-after-reserve"), prompt()).await.1), "b");
        assert_eq!(answer(&fixture.request(Some("http-first"), prompt()).await.1), "a");
        turn(&mut socket, prompt()).await;
        assert_eq!(fixture.mock.calls.lock().last().unwrap().0, "b");

        let logs = fixture.logs(6).await;
        assert!(logs.iter().all(|log| log.routing_strategy == Routing::SmartQuota));
        assert!(accounts.iter().all(|a| a.state.lock().active_requests.load(Ordering::Relaxed) == 0));
        socket.close(None).await.unwrap();
    }
}

#[tokio::test]
async fn smart_balancing_uses_weekly_headroom_without_inventing_a_five_hour_reserve() {
    for native in [false, true] {
        let fixture = Fixture::new(Routing::SmartQuota, native).await;
        let accounts = fixture.app.pool.all();
        let reset = chrono::Utc::now() + chrono::Duration::days(3);
        for (account, used) in accounts.iter().zip([80.0, 20.0]) {
            crate::quota::authoritative(
                &mut account.state.lock(),
                vec![crate::quota::Window { name: "week".into(), used, resets_at: Some(reset), model: None }],
                None,
            );
        }
        // Identical renewals: known weekly headroom beats the old equal 50% fallback.
        assert_eq!(answer(&fixture.request(Some("weekly-http"), prompt()).await.1), "b");
        let mut socket = fixture.socket("weekly-ws").await;
        turn(&mut socket, prompt()).await;
        assert_eq!(fixture.mock.calls.lock().last().unwrap().0, "b");

        // A real 5-hour window at the reserve takes precedence over weekly-only headroom.
        accounts[0].state.lock().quota.windows.push(crate::quota::Window {
            name: "5h".into(),
            used: 70.0,
            resets_at: Some(reset),
            model: None,
        });
        assert_eq!(answer(&fixture.request(Some("real-reserve"), prompt()).await.1), "a");
        assert_eq!(answer(&fixture.request(Some("weekly-http"), prompt()).await.1), "b");
        turn(&mut socket, prompt()).await;
        assert_eq!(fixture.mock.calls.lock().last().unwrap().0, "b"); // existing assignment stays pinned
        socket.close(None).await.unwrap();
    }
}

#[tokio::test]
async fn request_load_releases_on_retry_finish_cancel_and_drop() {
    let fixture = Fixture::new(Routing::SmartQuota, false).await;
    let accounts = fixture.app.pool.all();
    let count = |i: usize| accounts[i].state.lock().active_requests.load(Ordering::Relaxed);
    let new_tracker =
        || crate::proxy::Tracker::new(&fixture.app, crate::ir::Format::Responses, true, "http", "gpt-6.1-sol");
    let mut tracker = new_tracker();
    tracker.attempt(&accounts[0]);
    assert_eq!(count(0), 1);
    tracker.attempt(&accounts[0]); // retry on same account must not accumulate
    assert_eq!(count(0), 1);
    assert_eq!(answer(&fixture.request(None, prompt()).await.1), "b"); // unbound requests see in-flight work
    tracker.attempt(&accounts[1]);
    assert_eq!((count(0), count(1)), (0, 1));
    tracker.finish(200, &crate::ir::Usage::default(), None);
    assert_eq!((count(0), count(1)), (0, 0));
    drop(tracker);
    let mut tracker = new_tracker();
    tracker.attempt(&accounts[0]);
    tracker.cancel();
    tracker.cancel();
    assert_eq!(count(0), 0);
    drop(tracker);
    let mut tracker = new_tracker();
    tracker.attempt(&accounts[0]);
    drop(tracker); // client disconnects mid-stream
    assert_eq!(count(0), 0);
}

#[tokio::test]
async fn http_tasks_stay_pinned_and_migrate_only_when_quota_runs_out() {
    for routing in [Routing::LeastUsed, Routing::SmartQuota, Routing::RoundRobin, Routing::FillFirst] {
        let fixture = Fixture::new(routing, false).await;
        let task = "private-coding-thread-identifier";
        for _ in 0..3 {
            let (status, response) = fixture.request(Some(task), prompt()).await;
            assert_eq!(status, 200);
            assert_eq!(answer(&response), "a");
        }
        let initial = fixture.logs(3).await;
        let session = initial[0].session_id.as_deref().unwrap();
        assert_eq!(session.len(), 64);
        assert!(session.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(initial[0].routing_reason, Some("new_session"));
        for log in &initial[1..] {
            assert_eq!(log.routing_reason, Some("session_reused"));
        }
        fixture.mock.mode.store(1, Ordering::Relaxed);
        let (status, response) = fixture.request(Some(task), prompt()).await;
        assert_eq!(status, 200);
        assert_eq!(answer(&response), "b");
        let migrated = fixture.logs(4).await.pop().unwrap();
        assert_eq!(migrated.routing_reason, Some("quota_exhausted"));
        assert_eq!(migrated.routing_attempts.len(), 2);
        assert_eq!(migrated.routing_attempts[0].reason, "session_reused");
        assert_eq!(migrated.routing_attempts[1].reason, "quota_exhausted");
        assert_eq!(
            migrated.routing_attempts[1].previous_account.as_deref(),
            Some(initial[0].routing_attempts[0].account_id.as_str())
        );
        assert_ne!(migrated.routing_attempts[0].account_id, migrated.routing_attempts[1].account_id);
        fixture.recover();
        for _ in 0..3 {
            assert_eq!(answer(&fixture.request(Some(task), prompt()).await.1), "b");
        }
        let logs = fixture.logs(7).await;
        for log in &logs {
            assert_eq!(log.session_id.as_deref(), Some(session));
            assert_eq!(log.session_source, Some("thread-id"));
            assert_eq!(log.routing_strategy, routing);
            assert_eq!(log.routing_warning, None);
        }
        for log in &logs[4..] {
            assert_eq!(log.routing_reason, Some("session_reused"));
        }
        let serialized = serde_json::to_string(&logs).unwrap();
        assert!(!serialized.contains(task));
        assert!(!serialized.contains("client-one"));
        let restarted = App::new(fixture.cfg.clone(), fixture.directory.join("config.yaml"));
        let proxy = serve(crate::server::router(restarted)).await;
        let response: Value = reqwest::Client::new()
            .post(format!("{}/v1/responses", proxy.url))
            .bearer_auth("client-one")
            .header("thread-id", task)
            .json(&prompt())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(answer(&response), "b");
    }
}

#[tokio::test]
async fn temporary_failures_detour_and_the_session_returns_to_its_subscription() {
    for mode in [2, 3, 6] {
        let fixture = Fixture::new(Routing::RoundRobin, false).await;
        assert_eq!(answer(&fixture.request(Some("task"), prompt()).await.1), "a");
        fixture.mock.mode.store(mode, Ordering::Relaxed);
        // The client still gets an answer, from the other subscription...
        let (status, body) = fixture.request(Some("task"), prompt()).await;
        assert_eq!(status, 200, "mode {mode}");
        assert_eq!(answer(&body), "b");
        assert_eq!(fixture.logs(2).await.pop().unwrap().routing_reason, Some("temporary_detour"));
        // ...and the session goes back to its own once that recovers.
        fixture.recover();
        assert_eq!(answer(&fixture.request(Some("task"), prompt()).await.1), "a");
    }
}

#[tokio::test]
async fn quota_headers_and_stream_errors_trigger_migration_on_the_next_call() {
    for mode in [4, 5] {
        let fixture = Fixture::new(Routing::RoundRobin, false).await;
        fixture.mock.mode.store(mode, Ordering::Relaxed);
        assert_eq!(fixture.request(Some("task"), prompt()).await.0, if mode == 4 { 429 } else { 200 });
        fixture.mock.mode.store(0, Ordering::Relaxed);
        assert_eq!(answer(&fixture.request(Some("task"), prompt()).await.1), "b");
    }
}

#[tokio::test]
async fn response_ids_continue_the_task_without_session_headers_and_replay_full_input() {
    let fixture = Fixture::new(Routing::RoundRobin, false).await;
    let first = fixture.request(None, prompt()).await.1;
    let first_log = fixture.logs(1).await.pop().unwrap();
    assert_eq!(first_log.session_source, Some("generated_response"));
    assert_eq!(first_log.routing_warning, Some("missing_session_id"));
    fixture.mock.mode.store(1, Ordering::Relaxed);
    let body = json!({"model":"gpt-6.1-sol","previous_response_id":first["id"],"input":"next"});
    let (status, next) = fixture.request(None, body).await;
    assert_eq!(status, 200);
    assert_eq!(answer(&next), "b");
    let next_log = fixture.logs(2).await.pop().unwrap();
    assert_eq!(next_log.session_id, first_log.session_id);
    assert_eq!(next_log.session_source, Some("previous_response_id"));
    assert_eq!(next_log.routing_warning, Some("response_id_only"));
    // The first turn had no session id, so the continuation makes the first assignment.
    assert_eq!(next_log.routing_reason, Some("new_session"));
    {
        let calls = fixture.mock.calls.lock();
        let (_, sent, _) = calls.last().unwrap();
        assert!(sent["previous_response_id"].is_null());
        assert_eq!(sent["input"].as_array().unwrap().len(), 3);
    }
    assert_eq!(
        fixture.request(None, json!({"model":"gpt-6.1-sol","previous_response_id":"external", "input":"next"})).await.0,
        200
    );
}

#[tokio::test]
async fn websocket_fallback_and_reconnect_share_http_assignments_and_history() {
    let fixture = Fixture::new(Routing::RoundRobin, false).await;
    let first = fixture.request(Some("task"), prompt()).await.1;
    let mut socket = fixture.socket("task").await;
    let next =
        turn(&mut socket, json!({"model":"gpt-6.1-sol","previous_response_id":first["id"],"input":"next"})).await;
    socket.close(None).await.unwrap();
    let mut socket = fixture.socket("task").await;
    turn(&mut socket, json!({"model":"gpt-6.1-sol","previous_response_id":next["id"],"input":"last"})).await;
    assert!(fixture.mock.calls.lock().iter().all(|(account, _, _)| account == "a"));
    assert_eq!(fixture.mock.calls.lock().last().unwrap().1["input"].as_array().unwrap().len(), 5);
    let logs = fixture.logs(3).await;
    for log in &logs {
        assert_eq!(log.session_id, logs[0].session_id);
        assert!(log.session_id.is_some());
        assert_eq!(log.session_source, Some("thread-id"));
        assert_eq!(log.routing_warning, None);
    }
    for log in &logs[1..] {
        assert_eq!(log.transport, "ws");
        assert_eq!(log.routing_reason, Some("session_reused"));
    }
}

#[tokio::test]
async fn native_websocket_reconnect_replays_history_and_quota_failover_stays_on_replacement() {
    for routing in [Routing::LeastUsed, Routing::SmartQuota, Routing::RoundRobin, Routing::FillFirst] {
        let fixture = Fixture::new(routing, true).await;
        let mut socket = fixture.socket("task").await;
        let first = turn(&mut socket, prompt()).await;
        let second =
            turn(&mut socket, json!({"model":"gpt-6.1-sol","previous_response_id":first["id"],"input":"second"})).await;
        assert_eq!(fixture.mock.calls.lock().last().unwrap().1["previous_response_id"], first["id"]);
        socket.close(None).await.unwrap();
        let mut socket = fixture.socket("task").await;
        let third =
            turn(&mut socket, json!({"model":"gpt-6.1-sol","previous_response_id":second["id"],"input":"third"})).await;
        assert!(fixture.mock.calls.lock().last().unwrap().1["previous_response_id"].is_null());
        assert_eq!(fixture.mock.calls.lock().last().unwrap().1["input"].as_array().unwrap().len(), 5);
        fixture.mock.mode.store(1, Ordering::Relaxed);
        let fourth =
            turn(&mut socket, json!({"model":"gpt-6.1-sol","previous_response_id":third["id"],"input":"fourth"})).await;
        assert_eq!(fixture.mock.calls.lock().last().unwrap().0, "b");
        fixture.recover();
        turn(&mut socket, json!({"model":"gpt-6.1-sol","previous_response_id":fourth["id"],"input":"last"})).await;
        {
            let calls = fixture.mock.calls.lock();
            assert_eq!(calls.last().unwrap().0, "b");
            assert_eq!(calls.last().unwrap().2, "ws");
            assert_eq!(calls.last().unwrap().1["input"].as_array().unwrap().len(), 9);
        }
        // Quota exhaustion records the failed native turn and its successful HTTP fallback.
        let logs = fixture.logs(6).await;
        for log in &logs {
            assert_eq!(log.session_id, logs[0].session_id);
            assert!(log.session_id.is_some());
            assert_eq!(log.session_source, Some("thread-id"));
            assert_eq!(log.routing_strategy, routing);
            assert_eq!(log.routing_warning, None);
            assert_eq!(log.transport, "ws");
        }
        assert_eq!(logs[0].routing_reason, Some("new_session"));
        let migration = logs.iter().find(|log| log.routing_reason == Some("quota_exhausted")).unwrap();
        assert_eq!(migration.status, 200);
        assert_eq!(
            migration.routing_attempts[0].previous_account,
            Some(logs[0].routing_attempts[0].account_id.clone())
        );
        assert_eq!(logs.last().unwrap().routing_reason, Some("session_reused"));
    }
}

#[tokio::test]
async fn authentication_scopes_prevent_cross_client_pins_and_accept_query_keys() {
    let fixture = Fixture::new(Routing::RoundRobin, false).await;
    assert_eq!(answer(&fixture.request(Some("task"), prompt()).await.1), "a");
    let response: Value = reqwest::Client::new()
        .post(format!("{}/v1/responses?key=client-one", fixture.proxy.url))
        .header("thread-id", "task")
        .json(&prompt())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(answer(&response), "a");
    let response: Value = reqwest::Client::new()
        .post(format!("{}/v1/responses", fixture.proxy.url))
        .bearer_auth("client-two")
        .header("thread-id", "task")
        .header("x-cliproxy-client-scope", crate::affinity::scope_for_key(Some("client-one")))
        .json(&prompt())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(answer(&response), "b");
}

#[tokio::test]
async fn compaction_uses_the_tasks_account_and_configuration_changes_do_not_move_it() {
    let fixture = Fixture::new(Routing::RoundRobin, false).await;
    let first = fixture.request(Some("task"), prompt()).await.1;
    let mut cfg = fixture.cfg.clone();
    cfg.routing = Routing::LeastUsed;
    fixture.app.set_config(cfg);
    let response: Value = reqwest::Client::new()
        .post(format!("{}/v1/responses/compact", fixture.proxy.url))
        .bearer_auth("client-one")
        .header("thread-id", "task")
        .json(&prompt())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(answer(&response), answer(&first));
    let logs = fixture.logs(2).await;
    assert_eq!(logs[1].session_id, logs[0].session_id);
    assert!(logs[1].session_id.is_some());
    assert_eq!(logs[1].session_source, Some("thread-id"));
    assert_eq!(logs[1].routing_strategy, Routing::LeastUsed);
    assert_eq!(logs[1].routing_reason, Some("session_reused"));
    assert_eq!(logs[1].routing_warning, None);
}

#[tokio::test]
async fn claude_metadata_pins_the_session_before_request_translation() {
    let fixture = Fixture::new(Routing::RoundRobin, false).await;
    let mut cfg = fixture.cfg.clone();
    cfg.claude_api_key = cfg.codex_api_key.clone();
    cfg.codex_api_key.clear();
    for key in &mut cfg.claude_api_key {
        key.base_url = Some(fixture.provider.url.clone());
    }
    fixture.app.set_config(cfg);
    for _ in 0..4 {
        let body = json!({"model":"claude-sonnet-4-6","messages":[{"role":"user","content":"question"}],"max_tokens":32,"metadata":{"user_id":json!({"device_id":"device","session_id":"claude-task"}).to_string()}});
        let response: Value = reqwest::Client::new()
            .post(format!("{}/v1/messages", fixture.proxy.url))
            .header("x-api-key", "client-one")
            .json(&body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(response["content"][0]["text"], "a");
    }
    assert!(fixture.mock.calls.lock().iter().all(|(account, _, _)| account == "a"));
}

#[tokio::test]
async fn ending_a_task_releases_its_assignment_and_missing_codex_history_is_explicit() {
    let fixture = Fixture::new(Routing::RoundRobin, false).await;
    assert_eq!(answer(&fixture.request(Some("task"), prompt()).await.1), "a");
    let response: Value = reqwest::Client::new()
        .post(format!("{}/v1/responses", fixture.proxy.url))
        .bearer_auth("client-one")
        .header("thread-id", "task")
        .header("x-cliproxy-session-end", "true")
        .json(&prompt())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(answer(&response), "a");
    assert_eq!(answer(&fixture.request(Some("task"), prompt()).await.1), "b");
    let fixture = Fixture::new(Routing::RoundRobin, true).await;
    let (status, response) = fixture
        .request(
            Some("task"),
            json!({"model":"gpt-6.1-sol","input":"next","previous_response_id":"from-before-restart"}),
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(response["error"]["code"], "previous_response_not_found");
    assert!(fixture.mock.calls.lock().is_empty());
}

#[tokio::test]
async fn diagnostics_distinguish_agent_threads_and_missing_or_disabled_affinity() {
    let fixture = Fixture::new(Routing::RoundRobin, false).await;
    let threads = ["private-parent-thread", "private-agent-one-thread", "private-agent-two-thread"];
    for thread in threads.into_iter().chain([threads[0]]) {
        assert_eq!(fixture.request(Some(thread), prompt()).await.0, 200);
    }
    let logs = fixture.logs(4).await;
    let sessions: std::collections::HashSet<_> = logs[..3].iter().map(|log| log.session_id.clone()).collect();
    assert_eq!(sessions.len(), 3);
    assert!(!sessions.contains(&None));
    assert_eq!(logs[0].session_id, logs[3].session_id);
    assert_eq!(logs[0].account, logs[3].account);
    assert_eq!(logs[3].routing_reason, Some("session_reused"));
    let serialized = serde_json::to_string(&logs).unwrap();
    assert!(threads.iter().all(|thread| !serialized.contains(*thread)));
    assert!(!serialized.contains("client-one"));

    for _ in 0..2 {
        assert_eq!(fixture.request(None, prompt()).await.0, 200);
    }
    let logs = fixture.logs(6).await;
    assert_ne!(logs[4].session_id, logs[5].session_id);
    assert_ne!(logs[4].account, logs[5].account);
    for log in &logs[4..] {
        assert_eq!(log.session_source, Some("generated_response"));
        assert_eq!(log.routing_reason, Some("missing_session"));
        assert_eq!(log.routing_warning, Some("missing_session_id"));
    }

    let mut cfg = fixture.cfg.clone();
    cfg.session_affinity = false;
    fixture.app.set_config(cfg);
    for _ in 0..2 {
        assert_eq!(fixture.request(Some(threads[0]), prompt()).await.0, 200);
    }
    let logs = fixture.logs(8).await;
    assert_ne!(logs[6].account, logs[7].account);
    for log in &logs[6..] {
        assert_eq!(log.session_id, logs[0].session_id);
        assert_eq!(log.session_source, Some("thread-id"));
        assert_eq!(log.routing_reason, Some("affinity_disabled"));
        assert_eq!(log.routing_warning, Some("affinity_disabled"));
        assert_eq!(log.routing_strategy, Routing::RoundRobin);
    }
}

#[tokio::test]
async fn websocket_without_client_identity_warns_that_affinity_is_connection_only() {
    for native in [false, true] {
        let fixture = Fixture::new(Routing::RoundRobin, native).await;
        let mut socket = fixture.socket_with_task(None).await;
        turn(&mut socket, prompt()).await;
        turn(&mut socket, prompt()).await;
        socket.close(None).await.unwrap();
        let mut socket = fixture.socket_with_task(None).await;
        turn(&mut socket, prompt()).await;
        let logs = fixture.logs(3).await;
        assert_eq!(logs[0].session_id, logs[1].session_id);
        assert_eq!(logs[0].account, logs[1].account);
        assert_ne!(logs[0].session_id, logs[2].session_id);
        assert_ne!(logs[0].account, logs[2].account);
        for log in &logs {
            assert!(log.session_id.is_some());
            assert_eq!(log.session_source, Some("websocket_connection"));
            assert_eq!(log.routing_warning, Some("connection_only"));
        }
        assert_eq!(logs[0].routing_reason, Some("new_session"));
        assert_eq!(logs[1].routing_reason, Some("session_reused"));
        assert_eq!(logs[2].routing_reason, Some("new_session"));
    }
}

#[tokio::test]
async fn websocket_native_selection_preserves_decision_when_api_keys_require_http_fallback() {
    let fixture = Fixture::new(Routing::RoundRobin, false).await;
    let mut cfg = fixture.cfg.clone();
    cfg.codex_websockets = true;
    fixture.app.set_config(cfg);
    let mut socket = fixture.socket("fallback-task").await;
    turn(&mut socket, prompt()).await;
    turn(&mut socket, prompt()).await;
    let logs = fixture.logs(2).await;
    assert_eq!(logs[0].routing_reason, Some("new_session"));
    assert_eq!(logs[1].routing_reason, Some("session_reused"));
    let previous = logs[0].routing_attempts[0].account_id.clone();
    fixture.app.pool.get(&previous).unwrap().exhaust(
        "gpt-6.1-sol",
        chrono::Utc::now() + chrono::Duration::minutes(5),
        "mock observed quota exhaustion",
    );
    turn(&mut socket, prompt()).await;
    turn(&mut socket, prompt()).await;
    let logs = fixture.logs(4).await;
    assert_eq!(logs[2].routing_reason, Some("quota_exhausted"));
    assert_eq!(logs[2].routing_attempts[0].previous_account.as_deref(), Some(previous.as_str()));
    assert_ne!(logs[2].routing_attempts[0].account_id, previous);
    assert_eq!(logs[3].routing_reason, Some("session_reused"));
    assert!(logs[3].routing_attempts[0].previous_account.is_none());
    for log in &logs {
        assert_eq!(log.session_id, logs[0].session_id);
        assert_eq!(log.attempts, 1);
        assert_eq!(log.routing_attempts.len(), 1);
    }
    let calls = fixture.mock.calls.lock();
    assert_eq!(calls.len(), 4);
    assert!(calls.iter().all(|(_, _, transport)| *transport == "http"));
}

#[tokio::test]
async fn websocket_turn_without_its_original_body_identifier_reports_connection_only() {
    for native in [false, true] {
        let fixture = Fixture::new(Routing::RoundRobin, native).await;
        let mut socket = fixture.socket_with_task(None).await;
        let mut first = prompt();
        first["prompt_cache_key"] = "private-first-frame-session-key".into();
        turn(&mut socket, first).await;
        turn(&mut socket, prompt()).await;
        let logs = fixture.logs(2).await;
        assert_eq!(logs[0].session_source, Some("prompt_cache_key"));
        assert_eq!(logs[0].routing_warning, None);
        assert_eq!(logs[1].session_source, Some("websocket_connection"));
        assert_eq!(logs[1].routing_warning, Some("connection_only"));
        assert_eq!(logs[1].routing_reason, Some("session_reused"));
        assert_eq!(logs[0].session_id, logs[1].session_id);
        assert_eq!(logs[0].account, logs[1].account);
        assert!(!serde_json::to_string(&logs).unwrap().contains("private-first-frame-session-key"));
    }
}

#[tokio::test]
async fn translated_responses_preserve_cache_controls_and_reject_unsupported_prewarming() {
    let fixture = Fixture::new(Routing::RoundRobin, false).await;
    let mut cfg = fixture.cfg.clone();
    cfg.codex_api_key.clear();
    cfg.openai_compatibility = vec![crate::config::CompatEntry {
        name: "local-chat".into(),
        base_url: format!("{}/v1", fixture.provider.url),
        api_keys: vec!["a".into()],
        models: vec![crate::config::ModelAlias { name: "cache-compat-model".into(), alias: None }],
        ..Default::default()
    }];
    fixture.app.set_config(cfg);
    let mut body = json!({
        "model":"cache-compat-model",
        "prompt_cache_key":"private-cache-key",
        "prompt_cache_retention":"24h",
        "prompt_cache_options":{"mode":"explicit","ttl":"30m"},
        "input":[
            {"role":"system","content":[
                {"type":"input_text","text":"stable instructions","prompt_cache_breakpoint":{"mode":"explicit"}},
                {"type":"input_text","text":"variable instructions"}
            ]},
            {"role":"user","content":[
                {"type":"input_text","text":"stable context","prompt_cache_breakpoint":{"mode":"explicit"}},
                {"type":"input_text","text":"changing question"}
            ]}
        ]
    });
    let (status, response) = fixture.request(Some("cache-task"), body.clone()).await;
    assert_eq!(status, 200, "{response}");
    assert_eq!(answer(&response), "a");
    {
        let calls = fixture.mock.calls.lock();
        assert_eq!(calls.len(), 1);
        let (_, sent, transport) = &calls[0];
        assert_eq!(*transport, "chat");
        for field in ["prompt_cache_key", "prompt_cache_options", "prompt_cache_retention"] {
            assert_eq!(sent[field], body[field]);
        }
        let messages = sent["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        for (message, original) in messages.iter().zip(body["input"].as_array().unwrap()) {
            assert_eq!(message["role"], original["role"]);
            assert_eq!(message["content"][0]["text"], original["content"][0]["text"]);
            assert_eq!(message["content"][0]["prompt_cache_breakpoint"], json!({"mode":"explicit"}));
            assert_eq!(message["content"][1]["text"], original["content"][1]["text"]);
            assert!(message["content"][1]["prompt_cache_breakpoint"].is_null());
        }
    }
    body["prompt_cache_options"]["prewarm"] = true.into();
    let (status, response) = fixture.request(Some("cache-task"), body).await;
    assert_eq!(status, 400, "{response}");
    assert!(response["error"]["message"].as_str().unwrap().contains("prewarming"));
    assert_eq!(fixture.mock.calls.lock().len(), 1, "unsupported cache controls reached the upstream");
}
