use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{DefaultBodyLimit, FromRequest, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::StreamExt;
use serde_json::{Value, json};
use tower_http::cors::CorsLayer;

use crate::formats::{self, Frame};
use crate::ir::Format;
use crate::proxy::{self, Call, FrameStream, Reply};
use crate::state::App;

mod request_body;

pub fn router(app: Arc<App>) -> Router {
    let api = Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/responses", post(responses).get(responses_ws))
        .route("/backend-api/codex/responses", post(responses).get(responses_ws))
        .route("/v1/completions", post(completions))
        .route("/v1/responses/compact", post(compact))
        .route("/v1/images/generations", post(image_generations))
        .route("/v1/images/edits", post(image_edits))
        .route("/v1/videos/{kind}", post(video_create).get(video_status))
        .route("/v1/models", get(models))
        .route("/v1beta/models", get(gemini_models))
        .route("/v1beta/models/{*rest}", post(gemini))
        .layer(middleware::from_fn_with_state(app.clone(), request_body::decoded_body))
        .layer(middleware::from_fn_with_state(app.clone(), request_body::encoded_body))
        .layer(middleware::from_fn_with_state(app.clone(), client_auth))
        .layer(DefaultBodyLimit::max(request_body::MAX_BODY_SIZE))
        .layer(CorsLayer::permissive());

    Router::new()
        .merge(api)
        .route("/api/usage-ingest", post(crate::usage::api::ingest).layer(DefaultBodyLimit::max(512 << 10)))
        .nest("/api", crate::mgmt::router(app.clone()))
        .route("/", get(ui_index))
        .route("/ui/{file}", get(ui_asset))
        .route("/ui/fonts/{file}", get(ui_font))
        .route("/favicon.ico", get(favicon))
        .route("/manifest.webmanifest", get(manifest))
        .route("/sw.js", get(service_worker))
        .route("/ui/icons/{file}", get(ui_icon))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(app)
}

// ------------------------------------------------------------------------ auth

async fn client_auth(State(app): State<Arc<App>>, mut req: Request, next: Next) -> Response {
    let cfg = app.cfg();
    let h = req.headers();
    let get = |n: &str| h.get(n).and_then(|v| v.to_str().ok()).map(str::trim).map(String::from);
    let provided = get("authorization")
        .and_then(|v| v.strip_prefix("Bearer ").map(|s| s.trim().to_string()))
        .or_else(|| get("x-api-key"))
        .or_else(|| get("x-goog-api-key"))
        .or_else(|| {
            req.uri().query().and_then(|q| {
                url::form_urlencoded::parse(q.as_bytes()).find(|(k, _)| k == "key").map(|(_, v)| v.into_owned())
            })
        });
    // Collector credentials are ingestion-only even on an otherwise open proxy.
    let collector_key = provided.as_deref().is_some_and(|k| k.starts_with("fbxc_"));
    let named = provided.as_ref().and_then(|k| cfg.named_clients.iter().find(|c| crate::mgmt::constant_eq(&c.key, k)));
    if !collector_key
        && ((cfg.api_keys.is_empty() && cfg.named_clients.is_empty())
            || named.is_some()
            || provided.as_ref().is_some_and(|k| cfg.api_keys.iter().any(|a| crate::mgmt::constant_eq(a, k))))
    {
        // Never trust an incoming internal scope header. Query-string credentials
        // and header credentials get the same namespace without forwarding keys.
        let scope = crate::affinity::scope_for_key(provided.as_deref());
        req.headers_mut().remove(crate::usage::capture::REQUEST_HEADER);
        req.headers_mut().remove(crate::usage::capture::CLIENT_HEADER);
        let identity = named
            .map(|c| c.id.clone())
            .or_else(|| provided.as_ref().filter(|_| !cfg.api_keys.is_empty()).map(|_| scope.clone()));
        if let Some(identity) = identity.and_then(|v| HeaderValue::from_str(&v).ok()) {
            req.headers_mut().insert(crate::usage::capture::CLIENT_HEADER, identity);
        }
        req.headers_mut().remove(crate::affinity::LEGACY_SCOPE_HEADER);
        req.headers_mut().insert(crate::affinity::SCOPE_HEADER, HeaderValue::from_str(&scope).unwrap());
        return next.run(req).await;
    }
    let format = format_for_path(req.uri().path());
    (StatusCode::UNAUTHORIZED, axum::Json(formats::error_body(format, 401, "invalid or missing API key")))
        .into_response()
}

fn format_for_path(path: &str) -> Format {
    if path.starts_with("/v1/messages") {
        Format::Claude
    } else if path.starts_with("/v1beta") {
        Format::Gemini
    } else if path.contains("/responses") {
        Format::Responses
    } else {
        Format::Chat
    }
}

// -------------------------------------------------------------------- handlers

fn parse_body(app: &Arc<App>, format: Format, headers: &HeaderMap, body: &Bytes) -> Result<Value, Box<Response>> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .filter(Value::is_object)
        .ok_or_else(|| Box::new(body_error(app, format, headers, 400, "request body must be a JSON object")))
}

/// Record rejected input without assigning a provider or recording a provider attempt.
fn body_error(app: &Arc<App>, format: Format, headers: &HeaderMap, status: u16, message: &str) -> Response {
    let mut tracker = proxy::Tracker::new(app, format, false, "http", "");
    tracker.client_app(headers);
    tracker.finish(status, &crate::ir::Usage::default(), Some(message.into()));
    reply(format, Reply::Error(status, formats::error_body(format, status, message)), false)
}

async fn run(app: Arc<App>, format: Format, headers: HeaderMap, body: Bytes) -> Response {
    let body = match parse_body(&app, format, &headers, &body) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let stream = body["stream"].as_bool().unwrap_or(false);
    let call = Call {
        format,
        body,
        headers,
        stream,
        transport: "http",
        path_model: None,
        session: None,
        session_source: None,
        routing_selection: None,
    };
    reply(format, proxy::execute(app, call).await, false)
}

async fn chat(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Response {
    run(app, Format::Chat, headers, body).await
}

async fn messages(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Response {
    run(app, Format::Claude, headers, body).await
}

async fn responses(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Response {
    run(app, Format::Responses, headers, body).await
}

async fn responses_ws(State(app): State<Arc<App>>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    ws.max_message_size(256 << 20)
        .max_frame_size(256 << 20)
        .on_upgrade(move |socket| crate::ws::handle(app, headers, socket))
}

fn outcome(o: crate::media::Outcome) -> Response {
    match o {
        Ok(v) => axum::Json(v).into_response(),
        Err((status, body)) => {
            (StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY), axum::Json(body)).into_response()
        }
    }
}

async fn image_generations(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Response {
    match parse_body(&app, Format::Chat, &headers, &body) {
        Ok(v) => outcome(crate::media::images(app, headers, v, false).await),
        Err(r) => *r,
    }
}

async fn image_edits(State(app): State<Arc<App>>, req: Request) -> Response {
    let headers = req.headers().clone();
    let multipart =
        headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|c| c.starts_with("multipart/"));
    let body = if multipart {
        let form = match axum::extract::Multipart::from_request(req, &app).await {
            Ok(f) => f,
            Err(e) => return outcome(Err((400, formats::error_body(Format::Chat, 400, &e.body_text())))),
        };
        match crate::media::multipart_to_json(form).await {
            Ok(v) => v,
            Err(e) => return outcome(Err((400, formats::error_body(Format::Chat, 400, &e)))),
        }
    } else {
        let bytes = match axum::body::to_bytes(req.into_body(), request_body::MAX_BODY_SIZE).await {
            Ok(b) => b,
            Err(e) => return outcome(Err((400, formats::error_body(Format::Chat, 400, &e.to_string())))),
        };
        match parse_body(&app, Format::Chat, &headers, &bytes) {
            Ok(v) => v,
            Err(r) => return *r,
        }
    };
    outcome(crate::media::images(app, headers, body, true).await)
}

async fn video_create(
    State(app): State<Arc<App>>,
    Path(kind): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !matches!(kind.as_str(), "generations" | "edits" | "extensions") {
        return outcome(Err((404, formats::error_body(Format::Chat, 404, "unknown video endpoint"))));
    }
    match parse_body(&app, Format::Chat, &headers, &body) {
        Ok(v) => outcome(crate::media::video_create(app, headers, v, &kind).await),
        Err(r) => *r,
    }
}

async fn video_status(State(app): State<Arc<App>>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    outcome(crate::media::video_status(app, headers, id).await)
}

async fn compact(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Response {
    match parse_body(&app, Format::Responses, &headers, &body) {
        Ok(v) => outcome(crate::media::compact(app, headers, v).await),
        Err(r) => *r,
    }
}

/// Legacy `/v1/completions`, served through the chat pipeline.
async fn completions(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Response {
    let body = match parse_body(&app, Format::Chat, &headers, &body) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let prompt = match &body["prompt"] {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    };
    let mut chat = json!({ "model": body["model"], "messages": [{ "role": "user", "content": prompt }] });
    for k in [
        "max_tokens",
        "temperature",
        "top_p",
        "stop",
        "stream",
        "stream_options",
        "user",
        "metadata",
        "prompt_cache_key",
    ] {
        if !body[k].is_null() {
            chat[k] = body[k].clone();
        }
    }
    let stream = body["stream"].as_bool().unwrap_or(false);
    let call = Call {
        format: Format::Chat,
        body: chat,
        headers,
        stream,
        transport: "http",
        path_model: None,
        session: None,
        session_source: None,
        routing_selection: None,
    };
    match proxy::execute(app, call).await {
        Reply::Json(v) => {
            let choice = &v["choices"][0];
            axum::Json(json!({
                "id": v["id"].as_str().map(|s| s.replace("chatcmpl-", "cmpl-")),
                "object": "text_completion", "created": v["created"], "model": v["model"],
                "choices": [{
                    "text": choice["message"]["content"].as_str().unwrap_or_default(), "index": 0,
                    "logprobs": null, "finish_reason": choice["finish_reason"],
                }],
                "usage": v["usage"],
            }))
            .into_response()
        }
        Reply::Stream { frames, account } => {
            let frames: FrameStream = Box::pin(frames.filter_map(|f| async move {
                let Ok(v) = serde_json::from_str::<Value>(&f.data) else { return Some(f) };
                if v["error"].is_object() {
                    return Some(f);
                }
                let choice = &v["choices"][0];
                let text = choice["delta"]["content"].as_str().unwrap_or_default();
                if text.is_empty() && choice["finish_reason"].is_null() && v["usage"].is_null() {
                    return None;
                }
                let mut out = json!({
                    "id": v["id"].as_str().map(|s| s.replace("chatcmpl-", "cmpl-")), "object": "text_completion", "created": v["created"], "model": v["model"],
                    "choices": if choice.is_null() { json!([]) } else { json!([{
                        "text": text, "index": 0, "logprobs": null, "finish_reason": choice["finish_reason"],
                    }]) },
                });
                if !v["usage"].is_null() {
                    out["usage"] = v["usage"].clone();
                }
                Some(Frame::data(out.to_string()))
            }));
            reply(Format::Chat, Reply::Stream { frames, account }, false)
        }
        other => reply(Format::Chat, other, false),
    }
}

async fn count_tokens(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Response {
    match parse_body(&app, Format::Claude, &headers, &body) {
        Ok(v) => {
            let (count, estimated) = proxy::count_tokens(app, headers, v).await;
            token_count_response(count, estimated)
        }
        Err(r) => *r,
    }
}

fn token_count_response(body: Value, estimated: bool) -> Response {
    ([("x-fusebox-token-count-estimated", if estimated { "true" } else { "false" })], axum::Json(body)).into_response()
}

async fn gemini(
    State(app): State<Arc<App>>,
    Path(rest): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some((model, action)) = rest.rsplit_once(':') else {
        return reply(
            Format::Gemini,
            Reply::Error(404, formats::error_body(Format::Gemini, 404, "unknown method")),
            false,
        );
    };
    let body = match parse_body(&app, Format::Gemini, &headers, &body) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let stream = match action {
        "generateContent" => false,
        "streamGenerateContent" => true,
        "countTokens" => return token_count_response(json!({ "totalTokens": proxy::estimate_tokens(&body) }), true),
        _ => {
            return reply(
                Format::Gemini,
                Reply::Error(404, formats::error_body(Format::Gemini, 404, "unknown method")),
                false,
            );
        }
    };
    let json_array = stream && q.get("alt").map(String::as_str) != Some("sse");
    let call = Call {
        format: Format::Gemini,
        body,
        headers,
        stream,
        transport: "http",
        path_model: Some(model.trim_start_matches("models/").to_string()),
        session: None,
        session_source: None,
        routing_selection: None,
    };
    reply(Format::Gemini, proxy::execute(app, call).await, json_array)
}

async fn models(State(app): State<Arc<App>>) -> Response {
    let created = app.started.timestamp();
    let data: Vec<Value> = app
        .pool
        .models()
        .into_iter()
        .map(|(id, provider)| {
            json!({
                "id": id, "object": "model", "created": created, "owned_by": provider.as_str(),
                "type": "model", "display_name": id, "created_at": app.started.to_rfc3339(),
            })
        })
        .collect();
    let first = data.first().and_then(|m| m["id"].as_str()).map(String::from);
    let last = data.last().and_then(|m| m["id"].as_str()).map(String::from);
    axum::Json(json!({ "object": "list", "data": data, "has_more": false, "first_id": first, "last_id": last }))
        .into_response()
}

async fn gemini_models(State(app): State<Arc<App>>) -> Response {
    let models: Vec<Value> = app
        .pool
        .models()
        .into_iter()
        .map(|(id, _)| {
            json!({
                "name": format!("models/{id}"), "displayName": id, "version": "001",
                "supportedGenerationMethods": ["generateContent", "streamGenerateContent", "countTokens"],
            })
        })
        .collect();
    axum::Json(json!({ "models": models })).into_response()
}

// -------------------------------------------------------------------- replies

fn reply(_format: Format, r: Reply, json_array: bool) -> Response {
    match r {
        Reply::Json(v) => axum::Json(v).into_response(),
        Reply::Error(status, v) => {
            let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            (code, axum::Json(v)).into_response()
        }
        Reply::Stream { frames, account } => {
            let (body, ctype) = if json_array {
                (json_array_body(frames), "application/json")
            } else {
                (sse_body(frames), "text/event-stream")
            };
            let mut resp = Response::new(body);
            let h = resp.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            h.insert("x-accel-buffering", HeaderValue::from_static("no"));
            if let Ok(v) = HeaderValue::from_str(&account) {
                h.insert("x-fusebox-account", v);
            }
            resp
        }
    }
}

const KEEPALIVE: Duration = Duration::from_secs(15);

fn sse_body(mut frames: FrameStream) -> Body {
    Body::from_stream(async_stream::stream! {
        loop {
            match tokio::time::timeout(KEEPALIVE, frames.next()).await {
                Ok(Some(f)) => yield Ok::<_, Infallible>(Bytes::from(f.to_sse())),
                Ok(None) => break,
                Err(_) => yield Ok(Bytes::from_static(b": keepalive\n\n")),
            }
        }
    })
}

/// Gemini without `alt=sse` streams a JSON array.
fn json_array_body(mut frames: FrameStream) -> Body {
    Body::from_stream(async_stream::stream! {
        let mut first = true;
        yield Ok::<_, Infallible>(Bytes::from_static(b"["));
        loop {
            match tokio::time::timeout(KEEPALIVE, frames.next()).await {
                Ok(Some(Frame { data, .. })) => {
                    let sep = if first { "" } else { ",\r\n" };
                    first = false;
                    yield Ok(Bytes::from(format!("{sep}{data}")));
                }
                Ok(None) => break,
                Err(_) => yield Ok(Bytes::from_static(b"\n")),
            }
        }
        yield Ok(Bytes::from_static(b"]"));
    })
}

// ------------------------------------------------------------------------- ui

const INDEX: &str = include_str!("../ui/index.html");
const LOGOS: &str = include_str!("../ui/logos.svg");
const APP_JS: &str = include_str!("../ui/app.js");
const STYLE: &str = include_str!("../ui/style.css");
const ICON: &str = include_str!("../ui/icon.svg");
const FAVICON: &[u8] = include_bytes!("../ui/favicon.ico");

// The logo sprite is inlined so its gradients resolve from every <use>.
static PAGE: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| INDEX.replace("<!-- logos -->", LOGOS));

async fn ui_index() -> Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-cache")], PAGE.as_str())
        .into_response()
}

async fn ui_asset(Path(file): Path<String>) -> Response {
    let (body, ctype) = match file.as_str() {
        "app.js" => (APP_JS, "text/javascript; charset=utf-8"),
        "usage.js" => (include_str!("../ui/usage.js"), "text/javascript; charset=utf-8"),
        "config.js" => (include_str!("../ui/config.js"), "text/javascript; charset=utf-8"),
        "style.css" => (STYLE, "text/css; charset=utf-8"),
        "icon.svg" => (ICON, "image/svg+xml"),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    ([(header::CONTENT_TYPE, ctype), (header::CACHE_CONTROL, "no-cache")], body).into_response()
}

// Self-hosted fonts (latin subset, SIL OFL 1.1; licences in ui/fonts). Their names
// change with their contents, so they can be cached for a long time.
const FONTS: &[(&str, &[u8])] = &[
    ("ibm-plex-sans.woff2", include_bytes!("../ui/fonts/ibm-plex-sans.woff2")),
    ("ibm-plex-sans-condensed-500.woff2", include_bytes!("../ui/fonts/ibm-plex-sans-condensed-500.woff2")),
    ("ibm-plex-sans-condensed-600.woff2", include_bytes!("../ui/fonts/ibm-plex-sans-condensed-600.woff2")),
    ("dm-mono-400.woff2", include_bytes!("../ui/fonts/dm-mono-400.woff2")),
    ("dm-mono-500.woff2", include_bytes!("../ui/fonts/dm-mono-500.woff2")),
];

async fn ui_font(Path(file): Path<String>) -> Response {
    match FONTS.iter().find(|(name, _)| *name == file) {
        Some((_, body)) => (
            [(header::CONTENT_TYPE, "font/woff2"), (header::CACHE_CONTROL, "public, max-age=31536000, immutable")],
            *body,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// Installing the dashboard as an app: the manifest, its icons, and the service worker,
// served from the root so its scope covers the dashboard at `/`.
const MANIFEST: &str = include_str!("../ui/manifest.webmanifest");
const SERVICE_WORKER: &str = include_str!("../ui/sw.js");
const ICONS: &[(&str, &[u8])] = &[
    ("icon-192.png", include_bytes!("../ui/icons/icon-192.png")),
    ("icon-512.png", include_bytes!("../ui/icons/icon-512.png")),
    ("maskable-512.png", include_bytes!("../ui/icons/maskable-512.png")),
    ("apple-touch-icon.png", include_bytes!("../ui/icons/apple-touch-icon.png")),
];

async fn manifest() -> Response {
    ([(header::CONTENT_TYPE, "application/manifest+json"), (header::CACHE_CONTROL, "no-cache")], MANIFEST)
        .into_response()
}

async fn service_worker() -> Response {
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8"), (header::CACHE_CONTROL, "no-cache")], SERVICE_WORKER)
        .into_response()
}

async fn ui_icon(Path(file): Path<String>) -> Response {
    match ICONS.iter().find(|(name, _)| *name == file) {
        Some((_, body)) => {
            ([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "public, max-age=86400")], *body)
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn favicon() -> Response {
    ([(header::CONTENT_TYPE, "image/x-icon"), (header::CACHE_CONTROL, "public, max-age=86400")], FAVICON)
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    #[tokio::test]
    async fn serves_the_installable_app() {
        let cfg = crate::config::Config { auth_dir: "/nonexistent".into(), ..Default::default() };
        let app = App::new(cfg, "/nonexistent/config.yaml".into());
        for (path, ctype) in [
            ("/manifest.webmanifest", "application/manifest+json"),
            ("/sw.js", "text/javascript; charset=utf-8"),
            ("/ui/icons/icon-192.png", "image/png"),
            ("/ui/icons/maskable-512.png", "image/png"),
            ("/ui/icons/apple-touch-icon.png", "image/png"),
        ] {
            let resp = router(app.clone()).oneshot(Request::get(path).body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{path}");
            assert_eq!(resp.headers()[header::CONTENT_TYPE], ctype, "{path}");
        }
        let missing = router(app.clone()).oneshot(Request::get("/ui/icons/nope.png").body(Body::empty()).unwrap());
        assert_eq!(missing.await.unwrap().status(), StatusCode::NOT_FOUND);

        // Every icon the manifest names is served.
        let resp = router(app.clone()).oneshot(Request::get("/manifest.webmanifest").body(Body::empty()).unwrap());
        let manifest: Value =
            serde_json::from_slice(&to_bytes(resp.await.unwrap().into_body(), 1 << 16).await.unwrap()).unwrap();
        assert_eq!(manifest["scope"], "/");
        for icon in manifest["icons"].as_array().unwrap() {
            let src = icon["src"].as_str().unwrap();
            let resp = router(app.clone()).oneshot(Request::get(src).body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{src}");
        }
    }
}
