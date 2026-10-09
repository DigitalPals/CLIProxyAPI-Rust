//! Push notifications for faults. Devices subscribe from the dashboard (Config,
//! Notifications); a background check works out the faults every 30 seconds and sends
//! each new one, end-to-end encrypted, through the browser's push service.
//!
//! State lives in `<auth-dir>/.web-push.state` (0600): the VAPID key, the subscriptions
//! and the faults already sent, so a restart does not send them again.

mod crypto;
mod faults;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, ensure};
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::accounts::{Credential, Provider};
use crate::config::Notifications;
use crate::state::App;
use faults::{AccountView, Event, Fault};

const CHECK_EVERY: Duration = Duration::from_secs(30);
/// Checks in a row a fault must be seen (or gone) before it counts, so flapping
/// states don't notify.
const CONFIRM: u32 = 2;
const MAX_SUBSCRIPTIONS: usize = 32;
/// Undelivered notifications are dropped after this long (a device that is off).
const TTL_SECONDS: u32 = 12 * 3600;
/// Push services may contact whoever runs the sender; this identifies the software.
const SUBJECT: &str = "https://github.com/DigitalPals/Fusebox";
const STATE_FILE: &str = ".web-push.state";

#[derive(Default, Serialize, Deserialize)]
struct Stored {
    /// PKCS#8, base64url. Created the first time the dashboard asks for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    vapid: Option<String>,
    #[serde(default)]
    subscriptions: Vec<Subscription>,
    /// Faults that were sent (or seen when notifications were turned on).
    #[serde(default)]
    notified: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub id: String,
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    /// The dashboard's address as the device saw it (behind Tailscale or nginx the
    /// server can't know it), for links in notifications.
    pub origin: String,
    pub label: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Default)]
struct Tracker {
    /// Checks in a row each fault has been present.
    seen: HashMap<String, u32>,
    /// Checks in a row each sent fault has been absent.
    gone: HashMap<String, u32>,
}

pub struct Push {
    path: PathBuf,
    stored: Mutex<Stored>,
    vapid: Mutex<Option<Arc<crypto::Vapid>>>,
    tracker: Mutex<Tracker>,
}

impl Push {
    pub fn load(auth_dir: &Path) -> Self {
        let path = auth_dir.join(STATE_FILE);
        let stored = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!("{} is unreadable ({e}); push notifications start over", path.display());
                Stored::default()
            }),
            Err(_) => Stored::default(),
        };
        Self { path, stored: Mutex::new(stored), vapid: Mutex::new(None), tracker: Mutex::default() }
    }

    fn save(&self, stored: &Stored) {
        let result = serde_json::to_vec(stored)
            .map_err(anyhow::Error::from)
            .and_then(|b| crate::files::write_private(&self.path, &b));
        if let Err(e) = result {
            tracing::warn!("could not save {}: {e}", self.path.display());
        }
    }

    fn vapid(&self) -> Result<Arc<crypto::Vapid>> {
        let mut cached = self.vapid.lock();
        if let Some(v) = cached.as_ref() {
            return Ok(v.clone());
        }
        let mut stored = self.stored.lock();
        let pkcs8 = match stored.vapid.as_deref().map(crypto::decode) {
            Some(Ok(key)) => key,
            _ => {
                let key = crypto::Vapid::generate()?;
                stored.vapid = Some(URL_SAFE_NO_PAD.encode(&key));
                self.save(&stored);
                key
            }
        };
        let vapid = Arc::new(crypto::Vapid::from_pkcs8(&pkcs8)?);
        *cached = Some(vapid.clone());
        Ok(vapid)
    }

    pub fn subscriptions(&self) -> Vec<Subscription> {
        self.stored.lock().subscriptions.clone()
    }

    /// Adds or refreshes a device (by endpoint). Returns its id and whether it is the first.
    fn subscribe(&self, mut sub: Subscription) -> Result<(String, bool)> {
        let mut stored = self.stored.lock();
        let first = stored.subscriptions.is_empty();
        if let Some(existing) = stored.subscriptions.iter_mut().find(|s| s.endpoint == sub.endpoint) {
            sub.id = existing.id.clone();
            sub.created_at = existing.created_at;
            *existing = sub.clone();
        } else {
            ensure!(stored.subscriptions.len() < MAX_SUBSCRIPTIONS, "Too many devices; remove one first");
            stored.subscriptions.push(sub.clone());
        }
        self.save(&stored);
        Ok((sub.id, first))
    }

    fn remove(&self, matches: impl Fn(&Subscription) -> bool) -> bool {
        let mut stored = self.stored.lock();
        let before = stored.subscriptions.len();
        stored.subscriptions.retain(|s| !matches(s));
        let removed = stored.subscriptions.len() != before;
        if removed {
            self.save(&stored);
        }
        removed
    }

    /// Faults already present when the first device subscribes count as sent, so
    /// turning notifications on doesn't set off a burst.
    fn seed(&self, current: &[Fault]) {
        let mut stored = self.stored.lock();
        stored.notified = current.iter().filter(|f| f.event != Event::Quiet).map(|f| f.key.clone()).collect();
        self.save(&stored);
        *self.tracker.lock() = Tracker::default();
    }

    /// Takes one check's faults and returns what to send: faults seen twice in a row
    /// that were not sent yet, and providers that are back after running out.
    fn track(&self, current: &[Fault], accounts: &[AccountView], settings: Notifications) -> Vec<Fault> {
        let mut tracker = self.tracker.lock();
        let mut stored = self.stored.lock();
        let present: HashSet<&str> =
            current.iter().filter(|f| f.event != Event::Quiet).map(|f| f.key.as_str()).collect();
        tracker.seen.retain(|k, _| present.contains(k.as_str()));
        for key in &present {
            *tracker.seen.entry((*key).to_string()).or_default() += 1;
            tracker.gone.remove(*key);
        }

        let mut changed = false;
        let mut send = Vec::new();
        let new: Vec<&Fault> = current
            .iter()
            .filter(|f| f.event != Event::Quiet && !stored.notified.contains(&f.key))
            .filter(|f| tracker.seen.get(&f.key).is_some_and(|n| *n >= CONFIRM))
            .collect();
        // A provider running out says it for each of its accounts.
        let out: HashSet<&str> =
            new.iter().filter(|f| f.event == Event::ProviderExhausted).map(|f| f.provider.as_str()).collect();
        for f in new {
            stored.notified.insert(f.key.clone());
            changed = true;
            let covered = f.event == Event::AccountUsedUp && out.contains(f.provider.as_str());
            if enabled(settings, f.event) && !covered {
                send.push(f.clone());
            }
        }

        let absent: Vec<String> = stored.notified.iter().filter(|k| !present.contains(k.as_str())).cloned().collect();
        for key in absent {
            let n = tracker.gone.entry(key.clone()).or_default();
            *n += 1;
            if *n < CONFIRM {
                continue;
            }
            tracker.gone.remove(&key);
            stored.notified.remove(&key);
            changed = true;
            if let Some(provider) = key.strip_prefix("provider:")
                && settings.provider_exhausted
                && let Some(back) = faults::recovered(accounts, provider)
            {
                send.push(back);
            }
        }
        tracker.gone.retain(|k, _| stored.notified.contains(k));
        if changed {
            self.save(&stored);
        }
        send
    }
}

fn enabled(settings: Notifications, event: Event) -> bool {
    match event {
        Event::SignIn => settings.sign_in_expired,
        Event::ProviderExhausted => settings.provider_exhausted,
        Event::AccountUsedUp => settings.account_used_up,
        Event::AccountErrors => settings.account_errors,
        Event::Quiet => false,
    }
}

// ----------------------------------------------------------------------- check

pub async fn watcher(app: Arc<App>) {
    let mut tick = tokio::time::interval(CHECK_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;
    loop {
        tick.tick().await;
        check(&app).await;
    }
}

async fn check(app: &App) {
    if app.push.subscriptions().is_empty() {
        *app.push.tracker.lock() = Tracker::default();
        return;
    }
    let accounts = views(app);
    let current = faults::faults(&accounts, Utc::now());
    let send = app.push.track(&current, &accounts, app.cfg().notifications);
    for fault in send {
        notify(app, &fault, current.len()).await;
    }
}

/// Every current fault in the management API's form, errors first, as the faults
/// menu lists them.
pub fn current_faults(app: &App) -> Value {
    let mut current = faults::faults(&views(app), Utc::now());
    current.sort_by_key(|f| f.level != "err");
    Value::Array(current.iter().map(Fault::json).collect())
}

/// The rules' view of every account.
fn views(app: &App) -> Vec<AccountView> {
    let now = Utc::now();
    app.pool
        .all()
        .iter()
        .map(|a| {
            let api_key = matches!(*a.cred.read(), Credential::ApiKey { .. });
            let compat = a.group.clone().filter(|g| !g.is_empty()).unwrap_or_else(|| "Compatible".into());
            let group_name = match a.provider {
                Provider::Claude => "Claude",
                Provider::Codex => "Codex",
                Provider::Gemini => "Gemini",
                Provider::Vertex => "Vertex AI",
                Provider::Antigravity => "Antigravity",
                Provider::Kimi => "Kimi",
                Provider::Xai => "Grok",
                Provider::Meta => "Meta",
                Provider::Devin => "Devin",
                Provider::Compat => compat.as_str(),
            }
            .to_string();
            let provider_name = match a.provider {
                Provider::Xai if api_key => "xAI".to_string(),
                Provider::Codex if api_key => "OpenAI".to_string(),
                _ => group_name.clone(),
            };
            let provider = match a.provider {
                Provider::Compat => format!("openai-compat:{compat}"),
                p => p.as_str().to_string(),
            };
            let failures = app.stats.account_series(&a.id).iter().map(|b| b.failed).sum();
            let st = a.state.lock();
            AccountView {
                id: a.id.clone(),
                provider,
                provider_name,
                group_name,
                label: a.label.clone(),
                api_key,
                disabled: st.disabled,
                pauses: st.pauses(now),
                last_error: st.last_error.clone(),
                windows: st.quota.windows.clone(),
                failures,
            }
        })
        .collect()
}

/// Sends one fault to every subscribed device.
async fn notify(app: &App, fault: &Fault, badge: usize) {
    let urgency = if matches!(fault.event, Event::SignIn | Event::ProviderExhausted) { "high" } else { "normal" };
    for sub in app.push.subscriptions() {
        match deliver(app, &sub, fault, badge, urgency).await {
            Ok(status) if status.is_success() => tracing::info!(fault = %fault.key, device = %sub.label, "push sent"),
            Ok(StatusCode::NOT_FOUND | StatusCode::GONE) => {
                tracing::info!(device = %sub.label, "push subscription expired; removed");
                app.push.remove(|s| s.endpoint == sub.endpoint);
            }
            Ok(status) => tracing::warn!(device = %sub.label, %status, "push service refused a notification"),
            Err(e) => tracing::warn!(device = %sub.label, "push failed: {e:#}"),
        }
    }
}

/// One encrypted push. The payload uses the Declarative Web Push shape, which Safari
/// can show by itself; the dashboard's service worker reads the same fields elsewhere.
async fn deliver(app: &App, sub: &Subscription, fault: &Fault, badge: usize, urgency: &str) -> Result<StatusCode> {
    let payload = json!({
        "web_push": 8030,
        "notification": {
            "title": fault.title,
            "body": fault.body,
            "navigate": format!("{}/{}", sub.origin, fault.path),
            "tag": fault.key,
            "lang": "en",
            "app_badge": badge.to_string(),
        },
    });
    let body =
        crypto::encrypt(&crypto::decode(&sub.p256dh)?, &crypto::decode(&sub.auth)?, payload.to_string().as_bytes())?;
    let authorization = app.push.vapid()?.authorization(&sub.endpoint, SUBJECT, Utc::now().timestamp())?;
    let resp = app
        .http
        .control(None)
        .post(&sub.endpoint)
        .header("TTL", TTL_SECONDS.to_string())
        .header("Urgency", urgency)
        .header("Content-Type", "application/octet-stream")
        .header("Content-Encoding", "aes128gcm")
        .header("Authorization", authorization)
        .body(body)
        .send()
        .await
        .context("push service unreachable")?;
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        tracing::debug!(%status, "push service said: {}", text.chars().take(300).collect::<String>());
    }
    Ok(status)
}

// ---------------------------------------------------------------------- routes

pub fn router() -> Router<Arc<App>> {
    Router::new()
        .route("/push", get(status))
        .route("/push/subscriptions", post(subscribe))
        .route("/push/subscriptions/{id}", delete(unsubscribe))
        .route("/push/test", post(test))
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

async fn status(State(app): State<Arc<App>>) -> Response {
    let vapid = match app.push.vapid() {
        Ok(v) => v,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    let devices: Vec<Value> = app
        .push
        .subscriptions()
        .iter()
        .map(|s| json!({ "id": s.id, "label": s.label, "origin": s.origin, "endpoint": s.endpoint, "created_at": s.created_at }))
        .collect();
    Json(json!({ "public_key": vapid.public_key, "subscriptions": devices })).into_response()
}

#[derive(Deserialize)]
struct NewSubscription {
    endpoint: String,
    keys: Keys,
    origin: String,
    #[serde(default)]
    label: String,
}

#[derive(Deserialize)]
struct Keys {
    p256dh: String,
    auth: String,
}

fn validate(new: NewSubscription) -> Result<Subscription> {
    let endpoint = url::Url::parse(&new.endpoint).context("The push endpoint is not a URL")?;
    ensure!(
        endpoint.scheme() == "https" || (cfg!(test) && endpoint.scheme() == "http"),
        "The push endpoint must use HTTPS"
    );
    ensure!(new.endpoint.len() <= 2048, "The push endpoint is too long");
    let p256dh = crypto::decode(&new.keys.p256dh).map_err(|_| anyhow!("Invalid p256dh key"))?;
    ensure!(p256dh.len() == 65 && p256dh[0] == 4, "Invalid p256dh key");
    ensure!(crypto::decode(&new.keys.auth).is_ok_and(|a| a.len() == 16), "Invalid auth secret");
    let origin = url::Url::parse(&new.origin).context("Invalid dashboard origin")?;
    ensure!(matches!(origin.scheme(), "https" | "http"), "Invalid dashboard origin");
    let label: String = new.label.trim().chars().filter(|c| !c.is_control()).take(80).collect();
    Ok(Subscription {
        id: uuid::Uuid::new_v4().simple().to_string(),
        endpoint: new.endpoint,
        p256dh: new.keys.p256dh,
        auth: new.keys.auth,
        origin: origin.origin().ascii_serialization(),
        label: if label.is_empty() { "Browser".into() } else { label },
        created_at: Utc::now(),
    })
}

async fn subscribe(State(app): State<Arc<App>>, Json(new): Json<NewSubscription>) -> Response {
    let sub = match validate(new) {
        Ok(s) => s,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.to_string()),
    };
    match app.push.subscribe(sub) {
        Ok((id, first)) => {
            if first {
                app.push.seed(&faults::faults(&views(&app), Utc::now()));
            }
            Json(json!({ "id": id })).into_response()
        }
        Err(e) => error(StatusCode::CONFLICT, e.to_string()),
    }
}

async fn unsubscribe(State(app): State<Arc<App>>, UrlPath(id): UrlPath<String>) -> Response {
    if app.push.remove(|s| s.id == id) {
        Json(json!({ "ok": true })).into_response()
    } else {
        error(StatusCode::NOT_FOUND, "No such device")
    }
}

#[derive(Deserialize)]
struct TestRequest {
    id: String,
}

async fn test(State(app): State<Arc<App>>, Json(req): Json<TestRequest>) -> Response {
    let Some(sub) = app.push.subscriptions().into_iter().find(|s| s.id == req.id) else {
        return error(StatusCode::NOT_FOUND, "This device is not subscribed; turn notifications on again");
    };
    let fault = Fault {
        key: "test".into(),
        event: Event::Quiet,
        provider: String::new(),
        title: "Notifications are on".into(),
        body: format!("Fusebox will tell {} when something trips.", sub.label),
        path: "#/config/notifications".into(),
        ..Default::default()
    };
    let badge = faults::faults(&views(&app), Utc::now()).len();
    match deliver(&app, &sub, &fault, badge, "normal").await {
        Ok(status) if status.is_success() => Json(json!({ "ok": true })).into_response(),
        Ok(StatusCode::NOT_FOUND | StatusCode::GONE) => {
            app.push.remove(|s| s.id == sub.id);
            error(StatusCode::GONE, "The push service no longer knows this device; turn notifications on again")
        }
        Ok(status) => error(StatusCode::BAD_GATEWAY, format!("The push service answered {status}")),
        Err(e) => error(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests;
