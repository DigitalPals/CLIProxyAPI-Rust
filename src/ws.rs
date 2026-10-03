//! Responses API over websocket (`GET /v1/responses`), the transport Codex uses.
//!
//! Each `response.create` message is one turn. Codex OAuth accounts get a
//! native upstream websocket (server-side `previous_response_id` works as-is);
//! every other provider is served through the normal pipeline, with
//! `previous_response_id` expanded from a small local history.

use std::collections::HashSet;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use axum::http::HeaderMap;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite;

use crate::accounts::{Account, Provider};
use crate::formats::{StreamParser, responses};
use crate::ir::{self, Event, Format, Usage};
use crate::proxy::{self, Call, Reply, Tracker};
use crate::sse::SseEvent;
use crate::state::App;

type Upstream = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type ClientTx = futures::stream::SplitSink<WebSocket, Message>;

struct Session {
    upstream: Option<(Arc<Account>, Upstream)>,
    /// Response ids known to the current upstream socket.
    upstream_responses: HashSet<String>,
    key: Option<String>,
    source: Option<&'static str>,
    pending_selection: Option<crate::affinity::Selected>,
    connection_id: String,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            upstream: None,
            upstream_responses: HashSet::new(),
            key: None,
            source: None,
            pending_selection: None,
            connection_id: uuid::Uuid::new_v4().to_string(),
        }
    }
}

struct ClientGone;

async fn send(tx: &mut ClientTx, text: String) -> Result<(), ClientGone> {
    tx.send(Message::Text(text.into())).await.map_err(|_| ClientGone)
}

fn error_event(status: u16, body: &Value) -> String {
    let err = if body["error"].is_object() {
        body["error"].clone()
    } else {
        json!({ "message": proxy::error_message(&body.to_string()) })
    };
    json!({ "type": "error", "status": status, "error": err }).to_string()
}

fn input_items(body: &Value) -> Vec<Value> {
    crate::affinity::input_items(body)
}

pub async fn handle(app: Arc<App>, headers: HeaderMap, socket: WebSocket) {
    let (mut tx, mut rx) = socket.split();
    let mut sess = Session::default();
    while let Some(msg) = rx.next().await {
        let text = match msg {
            Ok(Message::Text(t)) => t.to_string(),
            Ok(Message::Binary(b)) => String::from_utf8_lossy(&b).into_owned(),
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => continue,
        };
        let Ok(mut body) = serde_json::from_str::<Value>(&text) else {
            if send(&mut tx, error_event(400, &json!({ "error": { "message": "invalid JSON" } }))).await.is_err() {
                break;
            }
            continue;
        };
        if body["type"] != "response.create" {
            let msg = format!("unsupported message type `{}`", body["type"].as_str().unwrap_or_default());
            if send(&mut tx, error_event(400, &json!({ "error": { "message": msg, "type": "invalid_request_error" } })))
                .await
                .is_err()
            {
                break;
            }
            continue;
        }
        if let Some(o) = body.as_object_mut() {
            o.remove("type");
        }
        if turn(&app, &headers, &mut sess, body, &mut tx).await.is_err() {
            break;
        }
    }
    if let Some((_, mut up)) = sess.upstream.take() {
        let _ = up.close(None).await;
    }
}

async fn turn(
    app: &Arc<App>,
    headers: &HeaderMap,
    sess: &mut Session,
    mut body: Value,
    tx: &mut ClientTx,
) -> Result<(), ClientGone> {
    sess.pending_selection = None;
    // Full conversation for local history (and for providers without server state).
    let prev = body["previous_response_id"].as_str().map(String::from);
    let requested = crate::affinity::session_identity(headers, &body);
    let previous =
        prev.as_deref().and_then(|id| app.sessions.previous(headers, id, requested.as_ref().map(|s| s.key.as_str())));
    let (key, source) = if let Some(identity) = requested {
        (identity.key, identity.source)
    } else if let Some((key, _)) = &previous {
        (key.clone(), "previous_response_id")
    } else if let Some(key) = &sess.key {
        (key.clone(), "websocket_connection")
    } else {
        (crate::affinity::connection_key(headers, &sess.connection_id), "websocket_connection")
    };
    if sess.key.as_ref().is_some_and(|old| old != &key) {
        if let Some((_, mut up)) = sess.upstream.take() {
            let _ = up.close(None).await;
        }
        sess.upstream_responses.clear();
    }
    sess.key = Some(key);
    sess.source = Some(source);
    let _lease = app.sessions.hold(sess.key.as_deref().unwrap(), app.cfg().session_affinity_idle_seconds);
    let mut full = if prev.is_some() { previous.map(|(_, items)| items) } else { Some(vec![]) };
    if let Some(items) = &mut full {
        items.extend(input_items(&body));
    }

    let cfg = app.cfg();
    if cfg.codex_websockets {
        match native_turn(app, headers, sess, &body, full.as_deref(), tx).await {
            Native::Done => {
                sess.pending_selection = None;
                if headers.get("x-cliproxy-session-end").is_some_and(|v| v == "true") {
                    app.sessions.end(sess.key.as_deref().unwrap());
                }
                return Ok(());
            }
            Native::Gone => {
                sess.pending_selection = None;
                return Err(ClientGone);
            }
            Native::Fallback => {}
        }
    }

    if prev.is_some() {
        if full.is_none() {
            let err = json!({ "error": {
                "message": "Previous response is not available on this websocket; resend the full conversation input without previous_response_id",
                "type": "invalid_request_error", "code": "previous_response_not_found", "param": "previous_response_id"
            }});
            return send(tx, error_event(400, &err)).await;
        }
        body["input"] = Value::Array(full.clone().unwrap());
        body.as_object_mut().unwrap().remove("previous_response_id");
    }

    let call = Call {
        format: Format::Responses,
        body,
        headers: headers.clone(),
        stream: true,
        transport: "ws",
        path_model: None,
        session: sess.key.clone(),
        session_source: sess.source,
        routing_selection: sess.pending_selection.take(),
    };
    match proxy::execute(app.clone(), call).await {
        Reply::Stream { mut frames, .. } => {
            while let Some(f) = frames.next().await {
                send(tx, f.data).await?;
            }
            Ok(())
        }
        Reply::Json(v) => send(tx, json!({ "type": "response.completed", "response": v }).to_string()).await,
        Reply::Error(status, body) => send(tx, error_event(status, &body)).await,
    }
}

fn capture(
    app: &App,
    headers: &HeaderMap,
    sess: &mut Session,
    data: &str,
    full: Option<&[Value]>,
    aggregate: &ir::Aggregate,
) {
    let Ok(v) = serde_json::from_str::<Value>(data) else { return };
    let r = &v["response"];
    let Some(id) = r["id"].as_str() else { return };
    // Only the most recent responses need connection-local continuation state.
    if sess.upstream_responses.len() >= 100 {
        sess.upstream_responses.clear();
    }
    sess.upstream_responses.insert(id.to_string());
    if let (Some(key), Some(full)) = (&sess.key, full) {
        let mut response = r.clone();
        crate::affinity::complete_output(&mut response, aggregate);
        app.sessions.remember(headers, key, &response, full);
    }
}

enum Native {
    Done,
    Gone,
    Fallback,
}

async fn connect(acct: &Arc<Account>, client_headers: &HeaderMap) -> Result<Upstream, String> {
    let (url, headers) = crate::upstream::codex_ws_url(acct, client_headers);
    let mut req =
        tungstenite::client::IntoClientRequest::into_client_request(url.as_str()).map_err(|e| e.to_string())?;
    for (k, v) in headers {
        if let (Ok(name), Ok(val)) =
            (tungstenite::http::HeaderName::from_bytes(k.as_bytes()), tungstenite::http::HeaderValue::from_str(&v))
        {
            req.headers_mut().insert(name, val);
        }
    }
    let (ws, _) = tokio::time::timeout(std::time::Duration::from_secs(20), tokio_tungstenite::connect_async(req))
        .await
        .map_err(|_| "websocket handshake timed out".to_string())?
        .map_err(|e| match e {
            tungstenite::Error::Http(resp) => format!("websocket handshake rejected: {}", resp.status()),
            other => other.to_string(),
        })?;
    Ok(ws)
}

async fn native_turn(
    app: &Arc<App>,
    headers: &HeaderMap,
    sess: &mut Session,
    body: &Value,
    full: Option<&[Value]>,
    tx: &mut ClientTx,
) -> Native {
    let cfg = app.cfg();
    // Native websockets don't go through HTTP proxies.
    if !cfg.proxy_url.is_empty() {
        return Native::Fallback;
    }
    let (model, suffix) = ir::split_model_suffix(body["model"].as_str().unwrap_or_default());
    if model.is_empty() {
        return Native::Fallback;
    }

    let (only, model) = app.pool.route(&model);
    let model = app.pool.canonical(&model, only.as_ref());
    if only.as_ref().is_some_and(|o| *o != crate::accounts::Only::Provider(Provider::Codex)) {
        return Native::Fallback;
    }

    // Refresh the shared assignment every turn, including on an existing socket.
    let selected = match app.sessions.pick_with_reason(&app.pool, &cfg, &model, sess.key.as_deref(), &[], only.as_ref())
    {
        Ok(pair) => pair,
        Err((status, message)) => {
            let result = send(tx, error_event(status, &json!({"error":{"message":message}}))).await;
            return if result.is_ok() { Native::Done } else { Native::Gone };
        }
    };
    sess.pending_selection = Some(selected.clone());
    let acct = selected.account.clone();
    let upstream_model = selected.model.clone();
    if acct.provider != Provider::Codex || !acct.is_oauth() || acct.proxy_url.is_some() {
        return Native::Fallback;
    }
    let reuse = sess.upstream.as_ref().is_some_and(|(a, _)| a.id == acct.id);
    if !reuse {
        if let Some((_, mut up)) = sess.upstream.take() {
            let _ = up.close(None).await;
        }
        sess.upstream_responses.clear();
        if crate::oauth::ensure_fresh(app, &acct, chrono::Duration::minutes(5), false).await.is_err() {
            return Native::Fallback;
        }
        let mut upstream_headers = headers.clone();
        if !upstream_headers.contains_key("session_id")
            && !upstream_headers.contains_key("session-id")
            && let Some(key) = body["prompt_cache_key"].as_str()
            && let Ok(value) = key.parse()
        {
            upstream_headers.insert("session_id", value);
        }
        match connect(&acct, &upstream_headers).await {
            Ok(ws) => {
                sess.upstream = Some((acct, ws));
            }
            Err(e) => {
                tracing::warn!(account = %acct.label, "codex websocket unavailable, using HTTP: {e}");
                return Native::Fallback;
            }
        }
    }
    let (acct, mut up) = sess.upstream.take().unwrap();
    let mut payload = body.clone();
    if let Some(prev) = body["previous_response_id"].as_str()
        && !sess.upstream_responses.contains(prev)
    {
        let Some(full) = full else {
            sess.upstream = Some((acct, up));
            let err = json!({"error":{"message":"Previous response is unavailable on this connection; resend full conversation input without previous_response_id", "code":"previous_response_not_found"}});
            return if send(tx, error_event(400, &err)).await.is_ok() { Native::Done } else { Native::Gone };
        };
        payload["input"] = Value::Array(full.to_vec());
        payload.as_object_mut().unwrap().remove("previous_response_id");
    }
    crate::upstream::sanitize_codex_body(&mut payload, &upstream_model, true);
    if let Some(r) = &suffix
        && let Some(e) = r.effort_level()
    {
        payload["reasoning"]["effort"] = e.into();
    }
    payload["type"] = "response.create".into();

    let quota_epoch = acct.quota_epoch();
    let mut tracker = Tracker::new(app, Format::Responses, true, "ws", &model);
    tracker.session(sess.key.as_deref(), sess.source, &cfg);
    tracker.selected(&selected);
    if up.send(tungstenite::Message::Text(payload.to_string().into())).await.is_err() {
        tracker.cancel();
        return Native::Fallback;
    }

    let mut parser = responses::Parser::default();
    let mut aggregate = ir::Aggregate::default();
    let mut usage = Usage::default();
    let mut evs = Vec::new();
    let mut error: Option<(u16, String)> = None;
    let mut forwarded = false;
    let mut terminal = false;
    loop {
        let next = tokio::time::timeout(std::time::Duration::from_secs(600), up.next()).await;
        let text = match next {
            Ok(Some(Ok(tungstenite::Message::Text(t)))) => t.to_string(),
            Ok(Some(Ok(tungstenite::Message::Binary(b)))) => String::from_utf8_lossy(&b).into_owned(),
            Ok(Some(Ok(tungstenite::Message::Close(_)))) | Ok(None) => {
                error = Some((502, "codex websocket closed".into()));
                break;
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => {
                error = Some((502, format!("codex websocket error: {e}")));
                break;
            }
            Err(_) => {
                error = Some((504, "codex websocket idle timeout".into()));
                break;
            }
        };
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let kind = v["type"].as_str().unwrap_or_default().to_string();
        crate::quota::observe_codex_event(&acct, &v, quota_epoch);
        parser.feed(&SseEvent { event: None, data: text.clone() }, &mut evs);
        for ev in evs.drain(..) {
            aggregate.push(&ev);
            match ev {
                Event::Usage(u) => usage.merge(&u),
                Event::Error { status, message } => error = Some((status, message)),
                Event::Text(_) | Event::Reasoning(_) | Event::ToolStart { .. } => tracker.first_token(),
                _ => {}
            }
        }
        if matches!(kind.as_str(), "error" | "response.failed")
            && let Some((status, msg)) = error.clone()
        {
            if proxy::quota_exhausted(&acct, &model, status, &text) {
                proxy::mark_quota_exhausted(&acct, &model, &reqwest::header::HeaderMap::new(), &text);
                app.broadcast("accounts", Value::Null);
                if !forwarded {
                    let _ = up.close(None).await;
                    sess.upstream_responses.clear();
                    tracker.finish(status, &usage, Some(msg));
                    return Native::Fallback;
                }
            } else if !forwarded && matches!(status, 401 | 403) {
                // HTTP fallback refreshes credentials on the same assigned account.
                let _ = up.close(None).await;
                sess.upstream_responses.clear();
                tracker.cancel();
                return Native::Fallback;
            } else if matches!(status, 401 | 403) {
                // An error after response.created also invalidates this socket.
                // Refresh the same subscription before the client's next turn.
                if let Err(e) = crate::oauth::ensure_fresh(app, &acct, chrono::Duration::minutes(5), true).await {
                    acct.cool(
                        None,
                        chrono::Utc::now() + chrono::Duration::seconds(60),
                        &format!("token refresh failed: {e}"),
                    );
                }
            }
        }
        if kind == "response.completed" || kind == "response.incomplete" {
            capture(app, headers, sess, &text, full, &aggregate);
        }
        terminal = matches!(kind.as_str(), "response.completed" | "response.incomplete" | "response.failed" | "error");
        forwarded = true;
        if send(tx, text).await.is_err() {
            tracker.finish(499, &usage, Some("client disconnected".into()));
            return Native::Gone;
        }
        if terminal {
            break;
        }
    }
    if terminal && !error.as_ref().is_some_and(|(status, _)| matches!(status, 401 | 403)) {
        sess.upstream = Some((acct.clone(), up));
    } else if !terminal {
        // The upstream socket died mid-turn: tell the client and reconnect next turn.
        let (status, msg) = error.clone().unwrap_or((502, "codex websocket closed".into()));
        let body = json!({ "error": { "message": msg, "type": "upstream_error" } });
        if send(tx, error_event(status, &body)).await.is_err() {
            tracker.finish(status, &usage, Some(msg));
            return Native::Gone;
        }
    }
    match error {
        Some((s, m)) => tracker.finish(s, &usage, Some(m)),
        None => {
            acct.record_ok();
            tracker.finish(200, &usage, None)
        }
    }
    Native::Done
}
