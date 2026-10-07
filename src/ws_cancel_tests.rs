use super::*;
use crate::accounts::{Credential, OAuth};
use crate::config::{Config, KeyEntry};
use axum::body::{Body, Bytes};
use axum::routing::post;
use std::convert::Infallible;
use std::sync::atomic::Ordering;
use tokio::sync::{Notify, mpsc};

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

fn app(base: &str, native: bool) -> Arc<App> {
    let key = KeyEntry { api_key: "loopback-only".into(), base_url: Some(base.into()), ..Default::default() };
    let cfg = Config {
        auth_dir: "/nonexistent".into(),
        codex_api_key: if native { vec![key.clone()] } else { vec![] },
        claude_api_key: if native { vec![] } else { vec![key] },
        ..Default::default()
    };
    let app = App::new(cfg, "/nonexistent/config.yaml".into());
    if native {
        *app.pool.all()[0].cred.write() = Credential::OAuth(OAuth {
            access_token: "loopback-only".into(),
            base_url: Some(base.into()),
            ..Default::default()
        });
    }
    app
}

async fn client(server: &Server) -> Upstream {
    tokio_tungstenite::connect_async(format!("{}/v1/responses", server.url.replace("http://", "ws://")))
        .await
        .unwrap()
        .0
}

async fn create(client: &mut Upstream, native: bool, input: &str) {
    client
        .send(tungstenite::Message::Text(
            json!({
                "type":"response.create", "model":if native {"gpt-6.1-sol"} else {"claude-sonnet-4-6"}, "input":input
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
}

async fn until_event(client: &mut Upstream, kind: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let message = client.next().await.unwrap().unwrap();
            if let tungstenite::Message::Text(text) = message {
                let value: Value = serde_json::from_str(&text).unwrap();
                if value["type"] == kind {
                    return value;
                }
            }
        }
    })
    .await
    .expect("expected downstream event")
}

async fn cancelled(app: &App, input: u64, cache: u64) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(log) = app.stats.recent.lock().front().cloned() {
                assert_eq!(log.status, 499);
                assert!(matches!(log.failure_kind, Some("downstream_disconnect" | "downstream_read_error")));
                assert_eq!(log.usage_completeness, if input > 0 { "partial" } else { "missing" });
                assert_eq!((log.input_tokens, log.cache_tokens), (input, cache));
                assert_eq!(app.stats.active.load(Ordering::Relaxed), 0);
                assert_eq!(app.pool.all()[0].state.lock().active_requests.load(Ordering::Relaxed), 0);
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("disconnected client must cancel a silent turn promptly");
}

#[tokio::test]
async fn downstream_disconnect_cancels_silent_native_generation() {
    for graceful in [true, false] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let disconnected = Arc::new(Notify::new());
        let observed = disconnected.clone();
        let task = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
            socket.next().await.unwrap().unwrap();
            socket
                .send(tungstenite::Message::Text(
                    json!({
                        "type":"response.created", "response":{"id":"native-pending","status":"in_progress"}
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            loop {
                match socket.next().await {
                    Some(Ok(tungstenite::Message::Close(_))) | Some(Err(_)) | None => break,
                    _ => {}
                }
            }
            observed.notify_one();
        });
        let _upstream = Server { url: base.clone(), task };
        let app = app(&base, true);
        let proxy = serve(crate::server::router(app.clone())).await;
        let mut socket = client(&proxy).await;
        create(&mut socket, true, "first").await;
        until_event(&mut socket, "response.created").await;
        if graceful {
            socket.close(None).await.unwrap();
        }
        drop(socket);
        cancelled(&app, 0, 0).await;
        tokio::time::timeout(Duration::from_secs(2), disconnected.notified())
            .await
            .expect("the provider socket must be released on client cancellation");
    }
}

#[tokio::test]
async fn downstream_disconnect_cancels_http_generation_and_keeps_usage() {
    let upstream = serve(axum::Router::new().route("/v1/messages", post(|| async {
        let body = Body::from_stream(async_stream::stream! {
            for value in [
                json!({"type":"message_start","message":{"id":"msg_pending","usage":{"input_tokens":20,"cache_read_input_tokens":80}}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}),
            ] {
                yield Ok::<_, Infallible>(Bytes::from(format!("data: {value}\n\n")));
            }
            std::future::pending::<()>().await;
        });
        ([("content-type", "text/event-stream")], body)
    }))).await;
    let app = app(&upstream.url, false);
    let proxy = serve(crate::server::router(app.clone())).await;
    let mut socket = client(&proxy).await;
    create(&mut socket, false, "first").await;
    until_event(&mut socket, "response.output_text.delta").await;
    socket.close(None).await.unwrap();
    drop(socket);
    cancelled(&app, 20, 80).await;
}

#[tokio::test]
async fn pipelined_creates_wait_for_prior_completion_and_keep_their_order() {
    let released = Arc::new(Notify::new());
    let (calls, mut received) = mpsc::unbounded_channel();
    let upstream = serve(axum::Router::new().route("/v1/messages", post({
        let released = released.clone();
        move |axum::Json(body): axum::Json<Value>| {
            let released = released.clone();
            let calls = calls.clone();
            async move {
                let prompt = body["messages"][0]["content"][0]["text"].as_str().unwrap().to_string();
                calls.send(prompt.clone()).unwrap();
                let body = Body::from_stream(async_stream::stream! {
                    let start = json!({"type":"message_start","message":{"id":format!("msg_{prompt}"),"usage":{"input_tokens":1}}});
                    yield Ok::<_, Infallible>(Bytes::from(format!("data: {start}\n\n")));
                    if prompt == "first" { released.notified().await; }
                    let text = json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":prompt}});
                    yield Ok(Bytes::from(format!("data: {text}\n\n")));
                    let stop = json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}});
                    yield Ok(Bytes::from(format!("data: {stop}\n\ndata: {{\"type\":\"message_stop\"}}\n\n")));
                });
                ([("content-type", "text/event-stream")], body)
            }
        }
    }))).await;
    let app = app(&upstream.url, false);
    let proxy = serve(crate::server::router(app.clone())).await;
    let mut socket = client(&proxy).await;
    create(&mut socket, false, "first").await;
    assert_eq!(received.recv().await.unwrap(), "first");
    for prompt in ["second", "third"] {
        create(&mut socket, false, prompt).await;
    }
    assert!(tokio::time::timeout(Duration::from_millis(50), received.recv()).await.is_err());
    released.notify_one();
    for prompt in ["first", "second", "third"] {
        let completed = until_event(&mut socket, "response.completed").await;
        assert_eq!(completed["response"]["output"][0]["content"][0]["text"], prompt);
    }
    assert_eq!(received.recv().await.unwrap(), "second");
    assert_eq!(received.recv().await.unwrap(), "third");
}

#[tokio::test]
async fn downstream_write_deadline_releases_a_blocked_sink() {
    let (socket, _peer) = tokio::io::duplex(1);
    let mut sender =
        tokio_tungstenite::WebSocketStream::from_raw_socket(socket, tungstenite::protocol::Role::Client, None).await;
    let started = tokio::time::Instant::now();
    let downstream = crate::diagnostics::Downstream::default();
    assert!(
        crate::diagnostics::downstream_scope(
            downstream.clone(),
            client_write(sender.send(tungstenite::Message::Text("blocked response".into())))
        )
        .await
        .is_err()
    );
    assert_eq!(downstream.kind(), "downstream_write_timeout");
    assert!(started.elapsed() >= CLIENT_WRITE_TIMEOUT);
    assert!(started.elapsed() < CLIENT_WRITE_TIMEOUT + Duration::from_secs(2));
}

#[tokio::test]
async fn downstream_socket_error_is_distinct_from_write_timeout() {
    let downstream = crate::diagnostics::Downstream::default();
    assert!(
        crate::diagnostics::downstream_scope(
            downstream.clone(),
            client_write(async { Err::<(), _>("private socket error") })
        )
        .await
        .is_err()
    );
    assert_eq!(downstream.kind(), "downstream_write_error");
}

#[tokio::test]
async fn native_completion_reports_full_usage_without_durable_analytics() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
        socket.next().await.unwrap().unwrap();
        socket.send(tungstenite::Message::Text(json!({
            "type":"response.completed", "response":{"id":"native-complete","status":"completed","output":[],
                "usage":{"input_tokens":12,"output_tokens":2,"input_tokens_details":{"cached_tokens":4,"cache_creation_tokens":0}}}
        }).to_string().into())).await.unwrap();
        while socket.next().await.is_some() {}
    });
    let _upstream = Server { url: base.clone(), task };
    let app = app(&base, true);
    assert!(app.usage.is_none());
    let proxy = serve(crate::server::router(app.clone())).await;
    let mut socket = client(&proxy).await;
    create(&mut socket, true, "first").await;
    until_event(&mut socket, "response.completed").await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(log) = app.stats.recent.lock().front().cloned() {
                assert_eq!(log.status, 200);
                assert_eq!(log.transport, "ws");
                assert_eq!(log.usage_completeness, "complete");
                assert_eq!(log.failure_kind, None);
                assert_eq!((log.input_tokens, log.output_tokens, log.cache_tokens), (8, 2, 4));
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
