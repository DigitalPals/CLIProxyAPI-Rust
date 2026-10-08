//! Management API used by the dashboard, plus OAuth login orchestration.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use axum::Json;
use axum::Router;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::accounts::{Credential, Only, Provider, set_file_disabled, write_oauth_file};
use crate::config::{Config, ModelAlias};
use crate::oauth;
use crate::state::App;

#[derive(Clone, Serialize)]
pub struct Login {
    pub provider: Provider,
    pub status: &'static str,
    pub message: Option<String>,
    pub url: String,
    pub callback: bool,
    /// "redirect" (browser OAuth) or "device" (enter `user_code` at `url`).
    pub kind: &'static str,
    pub user_code: Option<String>,
    #[serde(skip)]
    pub verifier: String,
    #[serde(skip)]
    pub created: Instant,
}

static CLAUDE_CB: AtomicBool = AtomicBool::new(false);
static CODEX_CB: AtomicBool = AtomicBool::new(false);
static ANTIGRAVITY_CB: AtomicBool = AtomicBool::new(false);

fn remember(app: &Arc<App>, state: &str, login: &Login) {
    let mut logins = app.logins.lock();
    logins.retain(|_, l| l.created.elapsed() < Duration::from_secs(1800));
    logins.insert(state.to_string(), login.clone());
}

fn settle(app: &Arc<App>, state: &str, result: &Result<String>) {
    if let Some(l) = app.logins.lock().get_mut(state) {
        match result {
            Ok(label) => {
                l.status = "done";
                l.message = Some(label.clone());
            }
            Err(e) => {
                l.status = "error";
                l.message = Some(format!("{e:#}"));
            }
        }
    }
    app.broadcast("login", json!({ "state": state }));
}

pub async fn start_login(app: &Arc<App>, provider: Provider) -> Result<(String, Login)> {
    let state = oauth::random_state();
    match provider {
        Provider::Claude | Provider::Codex | Provider::Antigravity => {
            let pkce = oauth::pkce();
            let url = oauth::auth_url(provider, &state, &pkce);
            let callback = ensure_callback_server(app, provider).await;
            let login = Login {
                provider,
                status: "pending",
                message: None,
                url,
                callback,
                kind: "redirect",
                user_code: None,
                verifier: pkce.verifier,
                created: Instant::now(),
            };
            remember(app, &state, &login);
            Ok((state, login))
        }
        Provider::Kimi | Provider::Xai | Provider::Meta => {
            let dev = crate::device::start(app, provider).await?;
            let login = Login {
                provider,
                status: "pending",
                message: None,
                url: dev.verification_uri.clone(),
                callback: true,
                kind: "device",
                user_code: Some(dev.user_code.clone()),
                verifier: String::new(),
                created: Instant::now(),
            };
            remember(app, &state, &login);
            let (app2, state2) = (app.clone(), state.clone());
            tokio::spawn(async move {
                let result = async {
                    let signed = crate::device::wait(&app2, provider, &dev).await?;
                    save_signed(&app2, provider, signed)
                }
                .await;
                settle(&app2, &state2, &result);
            });
            Ok((state, login))
        }
        Provider::Devin => {
            // Devin accepts any localhost redirect, so use a fresh port per login.
            let pkce = oauth::pkce();
            let (callback, redirect) = match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
                Ok(listener) => {
                    let port = listener.local_addr()?.port();
                    serve_callback(app, listener, "/callback", provider, None);
                    (true, format!("http://127.0.0.1:{port}/callback"))
                }
                Err(_) => (false, String::new()),
            };
            let login = Login {
                provider,
                status: "pending",
                message: None,
                url: crate::devin::auth_url(&redirect, &state, &pkce.challenge),
                callback,
                kind: "redirect",
                user_code: None,
                verifier: pkce.verifier,
                created: Instant::now(),
            };
            remember(app, &state, &login);
            Ok((state, login))
        }
        Provider::Vertex => Err(anyhow!("Vertex uses a service account key: import the JSON instead")),
        Provider::Gemini | Provider::Compat => Err(anyhow!("{} uses API keys", provider.as_str())),
    }
}

fn save_signed(app: &Arc<App>, provider: Provider, s: crate::device::Signed) -> Result<String> {
    let path = app.cfg().auth_dir().join(&s.file);
    write_oauth_file(&path, provider, &s.oauth, &s.extra)?;
    app.reload_accounts();
    Ok(s.oauth.email.unwrap_or(s.file))
}

/// Listens on the fixed OAuth redirect port while logins are pending.
async fn ensure_callback_server(app: &Arc<App>, provider: Provider) -> bool {
    let (flag, port, path) = match provider {
        Provider::Claude => (&CLAUDE_CB, oauth::claude::PORT, "/callback"),
        Provider::Antigravity => (&ANTIGRAVITY_CB, crate::antigravity::PORT, "/oauth-callback"),
        _ => (&CODEX_CB, oauth::codex::PORT, "/auth/callback"),
    };
    if flag.load(Ordering::SeqCst) {
        return true;
    }
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(
                "cannot listen on localhost:{port} for the OAuth callback ({e}); paste the redirect URL instead"
            );
            return false;
        }
    };
    flag.store(true, Ordering::SeqCst);
    serve_callback(app, listener, path, provider, Some(flag));
    true
}

/// Serves the OAuth redirect on `listener` until no login for `provider` is pending.
fn serve_callback(
    app: &Arc<App>,
    listener: tokio::net::TcpListener,
    path: &'static str,
    provider: Provider,
    flag: Option<&'static AtomicBool>,
) {
    let router = Router::new().route(path, get(callback)).with_state(app.clone());
    let app2 = app.clone();
    tokio::spawn(async move {
        let shutdown = async move {
            // Stay up while a login for this provider is pending (max 15 minutes).
            let started = Instant::now();
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let pending = app2.logins.lock().values().any(|l| l.provider == provider && l.status == "pending");
                if !pending || started.elapsed() > Duration::from_secs(900) {
                    break;
                }
            }
        };
        let _ = axum::serve(listener, router).with_graceful_shutdown(shutdown).await;
        if let Some(f) = flag {
            f.store(false, Ordering::SeqCst);
        }
    });
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

async fn callback(State(app): State<Arc<App>>, Query(q): Query<CallbackQuery>) -> Html<String> {
    let result = match (q.code, q.state, q.error) {
        (_, _, Some(e)) => Err(q.error_description.unwrap_or(e)),
        (Some(code), Some(state), _) => complete_login(&app, &state, &code).await.map_err(|e| format!("{e:#}")),
        _ => Err("missing code or state".to_string()),
    };
    let (title, body, dot) = match result {
        Ok(label) => (
            "Signed in",
            format!(
                "Connected <b style=\"color:#f2efe8;font-weight:500\">{}</b>. You can close this tab.",
                html_escape(&label)
            ),
            "#4ade80",
        ),
        Err(e) => ("Sign-in failed", html_escape(&e), "#fb7185"),
    };
    // Served from the OAuth callback port, so the page carries its own styles and no assets.
    Html(format!(
        r##"<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><meta name="color-scheme" content="dark"><meta name="theme-color" content="#0b0b0a"><title>{title} · Fusebox</title>
<body style="margin:0;min-height:100vh;display:grid;place-items:center;background:#0b0b0a;color:#f2efe8;font:14px/1.45 'IBM Plex Sans',system-ui,-apple-system,sans-serif">
<main style="box-sizing:border-box;width:min(420px,calc(100% - 32px));padding:22px 24px 24px;background:#121210;border:1px solid #24231f;border-radius:8px">
<div style="font:600 11px/1 'IBM Plex Sans Condensed','IBM Plex Sans',system-ui,sans-serif;letter-spacing:.12em;text-transform:uppercase;color:#857f74">Fusebox</div>
<h1 style="display:flex;align-items:center;gap:10px;font-size:17px;font-weight:600;margin:14px 0 6px"><span style="width:8px;height:8px;border-radius:50%;background:{dot}"></span>{title}</h1>
<p style="color:#b4afa4;margin:0;font-size:13px">{body}</p></main></body>"##
    ))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub async fn complete_login(app: &Arc<App>, state: &str, code: &str) -> Result<String> {
    let (provider, verifier) = {
        let logins = app.logins.lock();
        let l = logins.get(state).ok_or_else(|| anyhow!("unknown or expired login, start again"))?;
        if l.status != "pending" {
            return Err(anyhow!("this login was already completed"));
        }
        if l.kind == "device" {
            return Err(anyhow!("approve the code in your browser; there is nothing to paste"));
        }
        (l.provider, l.verifier.clone())
    };
    let result = async {
        if provider == Provider::Devin {
            let signed = crate::devin::complete_login(app, code.trim(), &verifier).await?;
            return save_signed(app, provider, signed);
        }
        let (cred, name, extra) = oauth::exchange(app, provider, code.trim(), state, &verifier).await?;
        let path = app.cfg().auth_dir().join(&name);
        write_oauth_file(&path, provider, &cred, &extra)?;
        app.reload_accounts();
        Ok::<_, anyhow::Error>(cred.email.unwrap_or(name))
    }
    .await;
    settle(app, state, &result);
    result
}

/// Accepts a pasted redirect URL, a `code#state` string or a bare code.
pub fn parse_pasted(input: &str) -> (String, Option<String>) {
    let input = input.trim();
    if let Some(q) = input.split_once('?').map(|(_, q)| q).filter(|q| q.contains("code=")) {
        let mut code = String::new();
        let mut state = None;
        for (k, v) in url::form_urlencoded::parse(q.split('#').next().unwrap_or(q).as_bytes()) {
            match k.as_ref() {
                "code" => code = v.into_owned(),
                "state" => state = Some(v.into_owned()),
                _ => {}
            }
        }
        return (code, state);
    }
    (input.to_string(), None)
}

// ---------------------------------------------------------------------- router

pub fn router(app: Arc<App>) -> Router<Arc<App>> {
    Router::new()
        .merge(crate::usage::api::router())
        .merge(crate::push::router())
        .route("/overview", get(overview))
        .route("/accounts", get(accounts))
        .route("/accounts/{id}", delete(delete_account))
        .route("/accounts/{id}/toggle", post(toggle_account))
        .route("/accounts/{id}/refresh", post(refresh_account))
        .route("/accounts/{id}/reset", post(reset_account))
        .route("/accounts/{id}/banked-resets", get(banked_resets).post(apply_banked_reset))
        .route("/accounts/{id}/quota/refresh", post(refresh_quota))
        .route("/accounts/{id}/activity", get(account_activity))
        .route("/routes", get(routes))
        .route("/keys", post(add_key))
        .route("/vertex", post(import_vertex))
        .route("/requests", get(requests))
        .route("/models", get(models))
        .route("/config", get(get_config).put(put_config))
        .route("/config/settings", get(get_settings).patch(patch_settings))
        .route("/login/{target}", post(login_start).get(login_status))
        .route("/login/{target}/code", post(login_code))
        .route("/live", get(live))
        .layer(middleware::from_fn_with_state(app, auth))
}

async fn auth(
    State(app): State<Arc<App>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let cfg = app.cfg();
    let collector_key =
        req.headers().get("authorization").and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("Bearer fbxc_"))
            || req.uri().query().is_some_and(|q| {
                url::form_urlencoded::parse(q.as_bytes()).any(|(k, v)| k == "key" && v.starts_with("fbxc_"))
            });
    if collector_key {
        return err(StatusCode::UNAUTHORIZED, "collector credentials are ingestion-only");
    }
    let key = cfg.management_key.clone();
    let loopback = addr.ip().to_canonical().is_loopback();
    let local = loopback && !proxied(req.headers());
    if key.is_empty() {
        if local {
            return next.run(req).await;
        }
        return err(
            StatusCode::FORBIDDEN,
            if loopback {
                "requests through a proxy (tailscale serve, nginx, Caddy) need management-key"
            } else {
                "the dashboard is only reachable from localhost until you set management-key"
            },
        );
    }
    if cfg.management_allow_remote == Some(false) && !local {
        return err(StatusCode::FORBIDDEN, "remote management is off (allow-remote: false)");
    }
    let bearer = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(String::from);
    let query = req
        .uri()
        .query()
        .and_then(|q| url::form_urlencoded::parse(q.as_bytes()).find(|(k, _)| k == "key").map(|(_, v)| v.into_owned()));
    if bearer.or(query).is_some_and(|k| management_key_matches(&k, &key)) {
        return next.run(req).await;
    }
    err(StatusCode::UNAUTHORIZED, "management key required")
}

/// A reverse proxy on the same host (tailscale serve, nginx, Caddy) connects from
/// loopback, but its forwarding headers show the request came from somewhere else.
/// Proxies that forward raw TCP add nothing, so behind those only a key protects.
fn proxied(headers: &axum::http::HeaderMap) -> bool {
    ["forwarded", "x-forwarded-for", "x-real-ip", "tailscale-user-login"].iter().any(|h| headers.contains_key(*h))
}

/// Plain keys compare in constant time; bcrypt hashes (CLIProxyAPI hashes
/// `secret-key` on first start) are verified once per key and remembered.
fn management_key_matches(provided: &str, configured: &str) -> bool {
    use sha2::{Digest, Sha256};
    static VERIFIED: parking_lot::Mutex<Vec<[u8; 32]>> = parking_lot::Mutex::new(Vec::new());
    if !["$2a$", "$2b$", "$2y$"].iter().any(|p| configured.starts_with(p)) {
        return constant_eq(provided, configured);
    }
    let id: [u8; 32] = Sha256::digest(format!("{configured}\0{provided}").as_bytes()).into();
    if VERIFIED.lock().contains(&id) {
        return true;
    }
    let ok = bcrypt::verify(provided, configured).unwrap_or(false);
    if ok {
        VERIFIED.lock().push(id);
    }
    ok
}

pub fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

fn ok() -> Response {
    Json(json!({ "ok": true })).into_response()
}

async fn overview(State(app): State<Arc<App>>) -> Json<Value> {
    let cfg = app.cfg();
    let accounts = app.pool.all();
    let (mut active, mut cooling, mut disabled) = (0, 0, 0);
    let mut providers = std::collections::BTreeMap::<&str, usize>::new();
    for a in &accounts {
        *providers.entry(a.provider.as_str()).or_default() += 1;
        let st = a.state.lock();
        if st.disabled {
            disabled += 1;
        } else if st.quota_refreshing
            || st.cooldowns.get("*").is_some_and(|t| *t > chrono::Utc::now())
            || st.quota_cooldowns.get("*").is_some_and(|t| *t > chrono::Utc::now())
            || st.quota.exhausted_until("").is_some()
        {
            cooling += 1;
        } else {
            active += 1;
        }
    }
    let host = if cfg.host == "0.0.0.0" || cfg.host.is_empty() { "127.0.0.1".to_string() } else { cfg.host.clone() };
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "started_at": app.started.to_rfc3339(),
        "uptime_secs": (chrono::Utc::now() - app.started).num_seconds(),
        "base_url": format!("http://{host}:{}", cfg.port),
        "client_keys": cfg.api_keys,
        "routing": cfg.routing,
        "banked_resets": cfg.banked_resets,
        "session_affinity": cfg.session_affinity,
        "request_retry": cfg.request_retry.max(1),
        "management_key": !cfg.management_key.is_empty(),
        "totals": *app.stats.totals.lock(),
        "active": app.stats.active.load(Ordering::Relaxed),
        "series": app.stats.series(),
        "accounts": { "total": accounts.len(), "active": active, "cooling": cooling, "disabled": disabled, "providers": providers },
        "models": app.pool.models().len(),
        "config_path": app.cfg_path.display().to_string(),
        "auth_dir": cfg.auth_dir().display().to_string(),
    }))
}

async fn accounts(State(app): State<Arc<App>>) -> Json<Value> {
    Json(Value::Array(app.pool.all().iter().map(|a| a.snapshot()).collect()))
}

async fn requests(State(app): State<Arc<App>>) -> Json<Value> {
    let recent = app.stats.recent.lock();
    Json(serde_json::to_value(recent.iter().rev().collect::<Vec<_>>()).unwrap_or_default())
}

/// One account's last hour, and the coding sessions pinned to it. Session details
/// (client, model, requests) come from the recent request log, so older ones show none.
async fn account_activity(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    if app.pool.get(&id).is_none() {
        return err(StatusCode::NOT_FOUND, "unknown account");
    }
    #[derive(Default)]
    struct Seen {
        session: String,
        since: Option<chrono::DateTime<chrono::Utc>>,
        client: &'static str,
        client_app: Option<&'static str>,
        model: String,
        requests: u64,
        cache_tokens: u64,
        usage_missing: u64,
        usage_partial: u64,
    }
    let mut seen: std::collections::HashMap<String, Seen> = Default::default();
    for log in app.stats.recent.lock().iter().filter(|l| l.account_id == id) {
        let Some(session) = &log.session_id else { continue };
        let s = seen.entry(crate::affinity::owner_of(session)).or_default();
        s.session.clone_from(session);
        s.since = Some(s.since.map_or(log.ts, |t| t.min(log.ts)));
        (s.client, s.client_app) = (log.client, log.client_app);
        s.model.clone_from(&log.model);
        s.requests += 1;
        s.cache_tokens += log.cache_tokens;
        s.usage_missing += u64::from(log.usage_completeness == "missing");
        s.usage_partial += u64::from(log.usage_completeness == "partial");
    }
    let sessions: Vec<Value> = app
        .sessions
        .pinned(&id, app.cfg().session_affinity_idle_seconds)
        .into_iter()
        .map(|p| {
            let s = seen.remove(&p.owner).unwrap_or_default();
            json!({
                "session": Some(s.session).filter(|s| !s.is_empty()),
                "last_seen": chrono::DateTime::from_timestamp(p.last_seen, 0).map(|t| t.to_rfc3339()),
                "active": p.active,
                "since": s.since.map(|t| t.to_rfc3339()),
                "client": Some(s.client).filter(|c| !c.is_empty()),
                "client_app": s.client_app,
                "model": Some(s.model).filter(|m| !m.is_empty()),
                "requests": s.requests,
                "cache_tokens": s.cache_tokens,
                "usage_missing": s.usage_missing,
                "usage_partial": s.usage_partial,
            })
        })
        .collect();
    Json(json!({ "series": app.stats.account_series(&id), "sessions": sessions })).into_response()
}

/// For every public model, the accounts new sessions would try, in order.
async fn routes(State(app): State<Arc<App>>) -> Json<Value> {
    let cfg = app.cfg();
    let load = app.sessions.account_load(&cfg);
    let models: serde_json::Map<String, Value> = app
        .pool
        .models()
        .into_iter()
        .map(|(id, _)| {
            let (only, model) = app.pool.route(&id);
            let model = app.pool.canonical(&model, only.as_ref());
            let steps = app.pool.route_order(&model, &cfg, only.as_ref(), &load);
            (id, serde_json::to_value(steps).unwrap_or_default())
        })
        .collect();
    Json(json!({
        "routing": cfg.routing,
        "session_affinity": cfg.session_affinity,
        "request_retry": cfg.request_retry.max(1),
        "models": models,
    }))
}

/// Public models; `prefix` marks ids that only reach the accounts carrying that prefix.
async fn models(State(app): State<Arc<App>>) -> Json<Value> {
    let models = app.pool.models().into_iter().map(|(m, p)| match app.pool.route(&m).0 {
        Some(Only::Prefix(prefix)) => json!({ "id": m, "provider": p, "prefix": prefix }),
        _ => json!({ "id": m, "provider": p }),
    });
    Json(Value::Array(models.collect()))
}

#[derive(Deserialize)]
struct ToggleBody {
    disabled: bool,
}

async fn toggle_account(State(app): State<Arc<App>>, Path(id): Path<String>, Json(b): Json<ToggleBody>) -> Response {
    let Some(acct) = app.pool.get(&id) else { return err(StatusCode::NOT_FOUND, "unknown account") };
    if let Some(path) = &acct.path
        && let Err(e) = set_file_disabled(path, b.disabled)
    {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    acct.state.lock().disabled = b.disabled;
    app.broadcast("accounts", Value::Null);
    ok()
}

async fn refresh_account(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let Some(acct) = app.pool.get(&id) else { return err(StatusCode::NOT_FOUND, "unknown account") };
    if !acct.is_oauth() {
        return err(StatusCode::BAD_REQUEST, "API keys don't need refreshing");
    }
    match oauth::ensure_fresh(&app, &acct, chrono::Duration::minutes(5), true).await {
        Ok(()) => {
            let _ = crate::quota::poll(&app, &acct).await;
            acct.state.lock().cooldowns.remove("*");
            app.broadcast("accounts", Value::Null);
            ok()
        }
        Err(e) => err(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

async fn reset_account(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let Some(acct) = app.pool.get(&id) else { return err(StatusCode::NOT_FOUND, "unknown account") };
    let mut st = acct.state.lock();
    st.cooldowns.clear();
    st.quota_cooldowns.clear();
    st.strikes = 0;
    st.last_error = None;
    drop(st);
    app.broadcast("accounts", Value::Null);
    ok()
}

const RESETS_OFF: &str = "Banked resets are turned off. Turn them on under Config, Connections.";

async fn banked_resets(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    if !app.cfg().banked_resets {
        return err(StatusCode::NOT_FOUND, RESETS_OFF);
    }
    let Some(acct) = app.pool.get(&id) else { return err(StatusCode::NOT_FOUND, "unknown account") };
    match crate::banked_resets::refresh(&app, &acct).await {
        Ok(view) => Json(view).into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, e.to_string()),
    }
}
async fn apply_banked_reset(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Json(body): Json<crate::banked_resets::Action>,
) -> Response {
    if !app.cfg().banked_resets {
        return err(StatusCode::NOT_FOUND, RESETS_OFF);
    }
    let Some(acct) = app.pool.get(&id) else { return err(StatusCode::NOT_FOUND, "unknown account") };
    match tokio::spawn(crate::banked_resets::apply(app, acct, body)).await {
        Ok(Ok(view)) => Json(view).into_response(),
        Ok(Err(e)) => err(StatusCode::CONFLICT, e.to_string()),
        Err(_) => {
            err(StatusCode::INTERNAL_SERVER_ERROR, "Reset operation interrupted; refresh its status before continuing")
        }
    }
}
async fn refresh_quota(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let Some(acct) = app.pool.get(&id) else { return err(StatusCode::NOT_FOUND, "unknown account") };
    if !matches!(acct.provider, Provider::Codex | Provider::Claude) || !acct.is_oauth() {
        return err(
            StatusCode::BAD_REQUEST,
            "Subscription quota is only available for Codex and Claude OAuth accounts",
        );
    }
    if oauth::ensure_fresh(&app, &acct, chrono::Duration::minutes(5), false).await.is_err() {
        return err(StatusCode::BAD_GATEWAY, "Could not refresh subscription credentials");
    }
    // Quota can still refresh if this subscription does not offer banked resets.
    acct.state.lock().quota_epoch += 1;
    if crate::quota::poll(&app, &acct).await.is_err() {
        return err(StatusCode::BAD_GATEWAY, "Could not refresh provider usage");
    }
    app.broadcast("accounts", Value::Null);
    if !app.cfg().banked_resets {
        return ok();
    }
    match crate::banked_resets::refresh(&app, &acct).await {
        Ok(view) => Json(view).into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, e.to_string()),
    }
}

async fn delete_account(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let Some(acct) = app.pool.get(&id) else { return err(StatusCode::NOT_FOUND, "unknown account") };
    if let Some(path) = &acct.path {
        if let Err(e) = std::fs::remove_file(path) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        }
        app.reload_accounts();
        return ok();
    }
    let key = match &*acct.cred.read() {
        Credential::ApiKey { key, .. } => key.clone(),
        _ => String::new(),
    };
    let group = if acct.provider == Provider::Compat { acct.group.clone() } else { None };
    edit_config(&app, |doc| crate::compat::remove_key(doc, &key, group.as_deref()))
}

#[derive(Deserialize)]
struct KeyBody {
    provider: String,
    api_key: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    models: String,
}

async fn add_key(State(app): State<Arc<App>>, Json(b): Json<KeyBody>) -> Response {
    let key = b.api_key.trim().to_string();
    let base = Some(b.base_url.trim().to_string()).filter(|s| !s.is_empty());
    let models: Vec<ModelAlias> = b
        .models
        .split([',', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|m| match m.split_once('=') {
            Some((alias, name)) => ModelAlias { name: name.trim().into(), alias: Some(alias.trim().into()) },
            None => ModelAlias { name: m.into(), alias: None },
        })
        .collect();
    let models: Vec<(String, Option<String>)> = models.into_iter().map(|m| (m.name, m.alias)).collect();
    let (group, name) = match Provider::parse(&b.provider) {
        Some(Provider::Compat) => {
            let Some(base) = &base else { return err(StatusCode::BAD_REQUEST, "base URL is required") };
            if models.is_empty() {
                return err(StatusCode::BAD_REQUEST, "list at least one model");
            }
            let name = if b.name.trim().is_empty() {
                url::Url::parse(base)
                    .ok()
                    .and_then(|u| u.host_str().map(String::from))
                    .unwrap_or_else(|| "provider".into())
            } else {
                b.name.trim().to_string()
            };
            ("openai-compatibility", Some(name))
        }
        Some(
            p @ (Provider::Claude
            | Provider::Codex
            | Provider::Gemini
            | Provider::Vertex
            | Provider::Kimi
            | Provider::Xai
            | Provider::Meta),
        ) => {
            if key.is_empty() {
                return err(StatusCode::BAD_REQUEST, "API key is required");
            }
            (p.as_str(), None)
        }
        Some(p) => return err(StatusCode::BAD_REQUEST, format!("{} does not take API keys", p.as_str())),
        None => return err(StatusCode::BAD_REQUEST, "unknown provider"),
    };
    let new = crate::compat::NewKey { group, api_key: &key, base_url: base.as_deref(), models, name: name.as_deref() };
    edit_config(&app, |doc| crate::compat::add_key(doc, &new))
}

#[derive(Deserialize)]
struct VertexBody {
    json: String,
    #[serde(default)]
    location: String,
}

async fn import_vertex(State(app): State<Arc<App>>, Json(b): Json<VertexBody>) -> Response {
    match crate::vertex::import(&app, &b.json, &b.location).await {
        Ok(label) => Json(json!({ "ok": true, "label": label })).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

/// Applies an edit to the config file's YAML tree, keeping every setting this
/// binary doesn't know about (so the file still works with CLIProxyAPI).
/// A rewrite drops comments, so the commented original is kept once as config.yaml.bak.
fn keep_original(app: &App, text: &str) {
    let backup = app.cfg_path.with_extension("yaml.bak");
    if text.contains('#') && !backup.exists() {
        let _ = std::fs::write(&backup, text);
    }
}

fn edit_config(app: &Arc<App>, edit: impl FnOnce(&mut serde_yaml::Value)) -> Response {
    let _guard = app.config_write.lock();
    let text = match std::fs::read_to_string(&app.cfg_path) {
        Ok(text) => text,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("Could not read config: {e}")),
    };
    let mut doc: serde_yaml::Value = match serde_yaml::from_str(&text) {
        Ok(serde_yaml::Value::Null) | Err(_) if text.trim().is_empty() => {
            serde_yaml::Value::Mapping(Default::default())
        }
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("config.yaml doesn't parse: {e}")),
    };
    let original = doc.clone();
    edit(&mut doc);
    let out = match crate::config_editor::render(&text, &original, &doc) {
        Ok((t, rewritten)) => {
            if rewritten {
                keep_original(app, &text);
            }
            t
        }
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let cfg = match Config::parse(&out) {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    if let Err(e) = std::fs::write(&app.cfg_path, out) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    app.set_config(cfg);
    ok()
}

async fn get_config(State(app): State<Arc<App>>) -> Json<Value> {
    let text = std::fs::read_to_string(&app.cfg_path).unwrap_or_default();
    Json(json!({ "text": text, "path": app.cfg_path.display().to_string() }))
}

#[derive(Deserialize)]
struct ConfigBody {
    text: String,
}

async fn put_config(State(app): State<Arc<App>>, Json(b): Json<ConfigBody>) -> Response {
    let _guard = app.config_write.lock();
    let cfg = match Config::parse(&b.text) {
        Ok(c) => c,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    };
    if let Err(e) = std::fs::write(&app.cfg_path, &b.text) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    let restart = !crate::config_editor::restart_fields(&app.startup_config, &cfg).is_empty();
    app.set_config(cfg);
    Json(json!({ "ok": true, "restart_required": restart })).into_response()
}

fn settings_response(app: &Arc<App>, text: &str) -> anyhow::Result<Value> {
    let cfg = Config::parse(text)?;
    let restart = crate::config_editor::restart_fields(&app.startup_config, &cfg);
    Ok(json!({
        "values": crate::config_editor::values(text)?,
        "defaults": crate::config_editor::values("")?,
        "revision": crate::config_editor::revision(text),
        "path": app.cfg_path.display().to_string(),
        "ignored": cfg.ignored,
        "restart_fields": restart,
        "restart_required": !restart.is_empty(),
    }))
}

async fn get_settings(State(app): State<Arc<App>>) -> Response {
    let _guard = app.config_write.lock();
    let text = match std::fs::read_to_string(&app.cfg_path) {
        Ok(text) => text,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("Could not read config: {e}")),
    };
    match settings_response(&app, &text) {
        Ok(body) => Json(body).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

#[derive(Deserialize)]
struct SettingsBody {
    revision: String,
    changes: serde_json::Map<String, Value>,
}

async fn patch_settings(State(app): State<Arc<App>>, Json(body): Json<SettingsBody>) -> Response {
    let _guard = app.config_write.lock();
    let text = match std::fs::read_to_string(&app.cfg_path) {
        Ok(text) => text,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("Could not read config: {e}")),
    };
    if body.revision != crate::config_editor::revision(&text) {
        return err(
            StatusCode::CONFLICT,
            "The config changed since you opened it. Reload the latest settings before saving.",
        );
    }
    let (out, cfg, rewritten) = match crate::config_editor::apply(&text, &body.changes) {
        Ok(result) => result,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    };
    let mut response = match settings_response(&app, &out) {
        Ok(body) => body,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    };
    response["rewritten"] = rewritten.into();
    // Also catch external file changes made while validation was running.
    if std::fs::read_to_string(&app.cfg_path).ok().as_deref() != Some(&text) {
        return err(StatusCode::CONFLICT, "The config changed while saving. Reload the latest settings before saving.");
    }
    if out != text {
        if rewritten {
            keep_original(&app, &text);
        }
        if let Err(e) = std::fs::write(&app.cfg_path, &out) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, format!("Could not save config: {e}"));
        }
        app.set_config(cfg);
    }
    Json(response).into_response()
}

async fn login_start(State(app): State<Arc<App>>, Path(target): Path<String>) -> Response {
    let Some(provider) = Provider::parse(&target) else { return err(StatusCode::BAD_REQUEST, "unknown provider") };
    match start_login(&app, provider).await {
        Ok((state, l)) => Json(json!({
            "state": state, "url": l.url, "callback": l.callback, "kind": l.kind, "user_code": l.user_code,
        }))
        .into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

async fn login_status(State(app): State<Arc<App>>, Path(target): Path<String>) -> Response {
    match app.logins.lock().get(&target) {
        Some(l) => Json(serde_json::to_value(l).unwrap_or_default()).into_response(),
        None => err(StatusCode::NOT_FOUND, "unknown login"),
    }
}

#[derive(Deserialize)]
struct CodeBody {
    input: String,
}

async fn login_code(State(app): State<Arc<App>>, Path(target): Path<String>, Json(b): Json<CodeBody>) -> Response {
    let (code, state) = parse_pasted(&b.input);
    if code.is_empty() {
        return err(StatusCode::BAD_REQUEST, "no authorization code found");
    }
    if state.as_deref().is_some_and(|s| s != target) {
        return err(StatusCode::BAD_REQUEST, "that URL belongs to a different login attempt");
    }
    match complete_login(&app, &target, &code).await {
        Ok(label) => Json(json!({ "ok": true, "label": label })).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

/// What each busy account is doing right now: requests in flight (including
/// streams that have not finished) and coding sessions seen in the last few minutes.
fn account_load(app: &App) -> Value {
    let sessions = app.sessions.recent_sessions(app.cfg().session_affinity_idle_seconds);
    let mut load = serde_json::Map::new();
    for a in app.pool.all() {
        let in_flight = a.state.lock().active_requests.load(Ordering::Relaxed);
        let sessions = sessions.get(&a.id).copied().unwrap_or(0);
        if in_flight > 0 || sessions > 0 {
            load.insert(a.id.clone(), json!({ "in_flight": in_flight, "sessions": sessions }));
        }
    }
    Value::Object(load)
}

async fn live(State(app): State<Arc<App>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |mut socket| async move {
        let mut rx = app.live.subscribe();
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        // Account load is checked every second but only sent when it changes.
        let mut load_tick = tokio::time::interval(Duration::from_secs(1));
        load_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut sent_load = String::new();
        loop {
            tokio::select! {
                _ = load_tick.tick() => {
                    let load = account_load(&app).to_string();
                    if load != sent_load {
                        let msg = format!(r#"{{"type":"load","data":{load}}}"#);
                        if socket.send(Message::Text(msg.into())).await.is_err() { break }
                        sent_load = load;
                    }
                }
                msg = rx.recv() => match msg {
                    Ok(m) => if socket.send(Message::Text(m.into())).await.is_err() { break },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                },
                _ = tick.tick() => {
                    let msg = json!({ "type": "tick", "data": {
                        "active": app.stats.active.load(Ordering::Relaxed),
                        "totals": *app.stats.totals.lock(),
                    }}).to_string();
                    if socket.send(Message::Text(msg.into())).await.is_err() { break }
                }
                incoming = socket.recv() => match incoming {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                },
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn requests_through_a_local_proxy_need_the_management_key() {
        let app = App::new(
            Config { auth_dir: "/nonexistent".into(), ..Default::default() },
            "/nonexistent/config.yaml".into(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let service = router(app.clone()).with_state(app.clone()).into_make_service_with_connect_info::<SocketAddr>();
        let server = tokio::spawn(async move { axum::serve(listener, service).await.unwrap() });
        let client = reqwest::Client::new();
        let status = |header: Option<(&'static str, &'static str)>, key: Option<&'static str>| {
            let mut req = client.get(format!("{origin}/requests"));
            if let Some((name, value)) = header {
                req = req.header(name, value);
            }
            if let Some(key) = key {
                req = req.bearer_auth(key);
            }
            async move { req.send().await.unwrap().status().as_u16() }
        };
        let tailscale = Some(("tailscale-user-login", "someone@example.com"));
        let nginx = Some(("x-forwarded-for", "100.64.0.7"));

        assert_eq!(status(None, None).await, 200, "localhost needs no key");
        assert_eq!(status(tailscale, None).await, 403);
        assert_eq!(status(nginx, None).await, 403);
        assert_eq!(status(Some(("forwarded", "for=192.0.2.60")), None).await, 403);

        let mut cfg = (*app.cfg()).clone();
        cfg.management_key = "secret".into();
        app.set_config(cfg.clone());
        assert_eq!(status(nginx, None).await, 401);
        assert_eq!(status(nginx, Some("secret")).await, 200);
        cfg.management_allow_remote = Some(false);
        app.set_config(cfg);
        assert_eq!(status(nginx, Some("secret")).await, 403, "a proxied request is remote");
        assert_eq!(status(None, Some("secret")).await, 200);
        server.abort();
    }

    #[tokio::test]
    async fn pinned_session_activity_preserves_missing_and_partial_usage() {
        let cfg = Config {
            auth_dir: "/nonexistent".into(),
            codex_api_key: vec![crate::config::KeyEntry { api_key: "mock-only".into(), ..Default::default() }],
            ..Default::default()
        };
        let mut app = App::new(cfg.clone(), "/nonexistent/config.yaml".into());
        Arc::get_mut(&mut app).unwrap().sessions = Arc::new(crate::affinity::Sessions::memory());
        let model = "gpt-6.1-sol";
        let mut account_id = String::new();
        for (session, cached) in [("mixed", 0), ("mixed", 40), ("missing", 0)] {
            let (account, _) = app.sessions.pick(&app.pool, &cfg, model, Some(session), &[], None).unwrap();
            account_id.clone_from(&account.id);
            let mut tracker = crate::proxy::Tracker::new(&app, crate::ir::Format::Responses, true, "http", model);
            tracker.session(Some(session), Some("thread-id"), &cfg);
            tracker.attempt(&account);
            tracker.finish(499, &crate::ir::Usage { cache_read: cached, ..Default::default() }, None);
        }
        let response = account_activity(State(app), Path(account_id)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let payload: Value = serde_json::from_slice(&body).unwrap();
        let sessions = payload["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 2);
        let mixed = sessions.iter().find(|s| s["session"] == "mixed").unwrap();
        assert_eq!((mixed["requests"].as_u64(), mixed["cache_tokens"].as_u64()), (Some(2), Some(40)));
        assert_eq!((mixed["usage_missing"].as_u64(), mixed["usage_partial"].as_u64()), (Some(1), Some(1)));
        let missing = sessions.iter().find(|s| s["session"] == "missing").unwrap();
        assert_eq!((missing["requests"].as_u64(), missing["cache_tokens"].as_u64()), (Some(1), Some(0)));
        assert_eq!((missing["usage_missing"].as_u64(), missing["usage_partial"].as_u64()), (Some(1), Some(0)));
    }

    #[tokio::test]
    async fn key_edits_fall_back_to_a_rewrite_when_formatting_cannot_be_kept() {
        let dir = std::env::temp_dir().join(format!("fusebox-edit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        let original = format!(
            "# indentless list\nauth-dir: {}\nclaude-api-key:\n- api-key: first\n  headers: {{X-Team: core}}\n",
            dir.display()
        );
        std::fs::write(&path, &original).unwrap();
        let app = App::new(Config::parse(&original).unwrap(), path.clone());
        let response = edit_config(&app, |doc| {
            crate::compat::add_key(
                doc,
                &crate::compat::NewKey {
                    group: "claude",
                    api_key: "second",
                    base_url: None,
                    models: vec![],
                    name: None,
                },
            )
        });
        assert_eq!(response.status(), StatusCode::OK);
        let cfg = Config::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.claude_api_key.len(), 2);
        assert_eq!(cfg.claude_api_key[0].headers["X-Team"], "core");
        assert_eq!(std::fs::read_to_string(dir.join("config.yaml.bak")).unwrap(), original);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn structured_saves_validate_and_reject_stale_edits() {
        let dir = std::env::temp_dir().join(format!("fusebox-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        let original = format!("# Keep this comment\nport: 8317 # port note\nauth-dir: {}\n", dir.display());
        std::fs::write(&path, &original).unwrap();
        let app = App::new(Config::parse(&original).unwrap(), path.clone());
        let version = crate::config_editor::revision(&original);
        let response = patch_settings(
            State(app.clone()),
            Json(SettingsBody {
                revision: version.clone(),
                changes: json!({"port": 70000}).as_object().unwrap().clone(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        let response = patch_settings(
            State(app.clone()),
            Json(SettingsBody {
                revision: version.clone(),
                changes: json!({"port": 9000}).as_object().unwrap().clone(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("# Keep this comment"));
        assert!(saved.contains("# port note"));
        let response = patch_settings(
            State(app.clone()),
            Json(SettingsBody { revision: version, changes: json!({"request-retry": 4}).as_object().unwrap().clone() }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);
        let response = patch_settings(
            State(app.clone()),
            Json(SettingsBody {
                revision: crate::config_editor::revision(&saved),
                changes: json!({"request-retry": 4}).as_object().unwrap().clone(),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let response: Value =
            serde_json::from_slice(&axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap();
        assert_eq!(response["restart_fields"], json!(["port"]));
        assert_eq!(response["values"]["request-retry"], json!(4));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bcrypt_management_keys() {
        let hash = bcrypt::hash("open sesame", 4).unwrap();
        assert!(management_key_matches("open sesame", &hash));
        assert!(management_key_matches("open sesame", &hash));
        assert!(!management_key_matches("wrong", &hash));
        assert!(management_key_matches("plain", "plain"));
        assert!(!management_key_matches("plain", "other"));
    }
}
