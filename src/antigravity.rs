//! Antigravity: Google's agent IDE, served by the private Cloud Code
//! (`v1internal`) API. Requests are Gemini `generateContent` bodies wrapped in
//! an envelope with the account's Cloud project; responses wrap each Gemini
//! chunk in `{"response": ...}`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use parking_lot::RwLock;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::accounts::{Account, Credential, write_oauth_file};
use crate::schema;
use crate::state::App;

pub const CLIENT_ID: &str = "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
pub const CLIENT_SECRET: &str = "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";
pub const PORT: u16 = 51121;
pub const REDIRECT: &str = "http://localhost:51121/oauth-callback";
pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v2/userinfo?alt=json";
pub const SCOPES: &str = "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile https://www.googleapis.com/auth/cclog https://www.googleapis.com/auth/experimentsandconfigs";

/// Inference goes to the daily channel, account setup to prod (as the IDE does).
pub const BASE_DAILY: &str = "https://daily-cloudcode-pa.googleapis.com";
pub const BASE_PROD: &str = "https://cloudcode-pa.googleapis.com";

const FALLBACK_VERSION: &str = "2.9.1";
const MANIFEST_URL: &str =
    "https://antigravity-hub-auto-updater-974169037036.us-central1.run.app/manifest/latest-arm64-mac.yml";
const NODE_CLIENT: &str = "google-api-nodejs-client/10.3.0";
const GOOG_API_CLIENT: &str = "gl-node/22.21.1";

static VERSION: RwLock<String> = RwLock::new(String::new());

pub fn version() -> String {
    let v = VERSION.read();
    if v.is_empty() { FALLBACK_VERSION.to_string() } else { v.clone() }
}

pub fn user_agent() -> String {
    format!("antigravity/hub/{} darwin/arm64", version())
}

/// Keeps the advertised IDE version current (the backend rejects stale clients).
pub async fn version_updater(app: Arc<App>) {
    loop {
        if app.pool.all().iter().any(|a| a.provider == crate::accounts::Provider::Antigravity) {
            match fetch_version(&app).await {
                Ok(v) => *VERSION.write() = v,
                Err(e) => tracing::debug!("antigravity version check failed: {e:#}"),
            }
        }
        tokio::time::sleep(Duration::from_secs(3 * 3600)).await;
    }
}

async fn fetch_version(app: &App) -> Result<String> {
    let text = app.http.client(None).get(MANIFEST_URL).timeout(Duration::from_secs(10)).send().await?.text().await?;
    text.lines()
        .find_map(|l| l.strip_prefix("version:"))
        .map(|v| v.trim().trim_matches(['"', '\'']).to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow!("no version in manifest"))
}

fn base(acct: &Account, default: &str) -> String {
    match &*acct.cred.read() {
        Credential::OAuth(o) => o.base_url.clone().unwrap_or_else(|| default.to_string()),
        _ => default.to_string(),
    }
    .trim_end_matches('/')
    .to_string()
}

pub fn request_base(acct: &Account) -> String {
    base(acct, BASE_DAILY)
}

fn token(acct: &Account) -> String {
    match &*acct.cred.read() {
        Credential::OAuth(o) => o.access_token.clone(),
        Credential::ApiKey { key, .. } => key.clone(),
    }
}

pub fn project(acct: &Account) -> Option<String> {
    match &*acct.cred.read() {
        Credential::OAuth(o) => o.project_id.clone(),
        _ => None,
    }
}

fn project_of(v: &Value) -> Option<String> {
    for k in ["cloudaicompanionProject", "projectId", "project"] {
        match &v[k] {
            Value::String(s) if !s.trim().is_empty() => return Some(s.trim().to_string()),
            Value::Object(o) => {
                if let Some(id) = o.get("id").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
                    return Some(id.trim().to_string());
                }
            }
            _ => {}
        }
    }
    None
}

fn default_tier(v: &Value) -> String {
    v["allowedTiers"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| t["isDefault"] == true)
        .and_then(|t| t["id"].as_str())
        .or_else(|| v["currentTier"]["id"].as_str())
        .unwrap_or("free-tier")
        .to_string()
}

/// Finds (or provisions) the Cloud project behind an Antigravity account.
pub async fn fetch_project(app: &App, http: &reqwest::Client, base: &str, token: &str) -> Result<String> {
    let resp = http
        .post(format!("{base}/v1internal:loadCodeAssist"))
        .bearer_auth(token)
        .header("user-agent", user_agent())
        .header("accept", "*/*")
        .json(&json!({ "metadata": { "ideType": "ANTIGRAVITY" } }))
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("loadCodeAssist returned {status}: {}", text.chars().take(300).collect::<String>());
    }
    let load: Value = serde_json::from_str(&text).context("invalid loadCodeAssist response")?;
    if let Some(p) = project_of(&load) {
        return Ok(p);
    }
    let tier = default_tier(&load);
    tracing::info!("antigravity: onboarding account on tier {tier}");
    let body = json!({
        "tier_id": tier,
        "metadata": { "ide_type": "ANTIGRAVITY", "ide_version": version(), "ide_name": "antigravity" },
    });
    for _ in 0..5 {
        let resp = http
            .post(format!("{BASE_DAILY}/v1internal:onboardUser"))
            .bearer_auth(token)
            .header("user-agent", format!("{} {NODE_CLIENT}", user_agent()))
            .header("x-goog-api-client", GOOG_API_CLIENT)
            .json(&body)
            .timeout(Duration::from_secs(30))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("onboardUser returned {status}: {}", text.chars().take(200).collect::<String>());
        }
        let v: Value = serde_json::from_str(&text).unwrap_or_default();
        if v["done"] == true {
            return project_of(&v["response"]).ok_or_else(|| anyhow!("onboarding finished without a project id"));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let _ = app;
    bail!("onboarding did not complete")
}

/// Makes sure the account knows its project (and models) before first use.
pub async fn ensure_ready(app: &App, acct: &Arc<Account>) -> Result<()> {
    if project(acct).is_none() {
        let _guard = acct.refresh_lock.lock().await;
        if project(acct).is_none() {
            let http = app.http.client(acct.proxy_url.as_deref());
            let id = fetch_project(app, &http, &base(acct, BASE_PROD), &token(acct)).await?;
            let snapshot = {
                let mut cred = acct.cred.write();
                let Credential::OAuth(o) = &mut *cred else { return Ok(()) };
                o.project_id = Some(id);
                o.clone()
            };
            if let Some(path) = &acct.path {
                app.suppress_reload();
                write_oauth_file(path, acct.provider, &snapshot, &[])?;
            }
        }
    }
    if acct.discovered.read().is_empty() {
        let app_http = app.http.client(acct.proxy_url.as_deref());
        if let Ok(models) = fetch_models(&app_http, &request_base(acct), &token(acct)).await
            && !models.is_empty()
        {
            *acct.discovered.write() = models;
        }
    }
    Ok(())
}

pub async fn fetch_models(http: &reqwest::Client, base: &str, token: &str) -> Result<Vec<String>> {
    let resp = http
        .post(format!("{base}/v1internal:fetchAvailableModels"))
        .bearer_auth(token)
        .header("user-agent", user_agent())
        .json(&json!({}))
        .timeout(Duration::from_secs(15))
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("fetchAvailableModels returned {}", resp.status());
    }
    let v: Value = resp.json().await?;
    let mut out: Vec<String> = match &v["models"] {
        Value::Object(m) => m.keys().cloned().collect(),
        Value::Array(a) => a.iter().filter_map(|x| x["id"].as_str().or(x["name"].as_str()).map(String::from)).collect(),
        _ => vec![],
    };
    out.retain(|m| !m.starts_with("chat_") && !m.starts_with("tab_") && !m.starts_with("gemini-2.5"));
    out.sort();
    Ok(out)
}

/// Stable per-conversation session id derived from the first user message.
fn session_id(req: &Value) -> String {
    let first = req["contents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["role"] == "user")
        .find_map(|c| c["parts"][0]["text"].as_str().filter(|t| !t.is_empty()));
    let n = match first {
        Some(t) => {
            let h = Sha256::digest(t.as_bytes());
            i64::from_be_bytes(h[..8].try_into().unwrap()) & i64::MAX
        }
        None => rand::random::<i64>() & i64::MAX,
    };
    format!("-{n}")
}

/// Wraps a Gemini request for the Cloud Code API.
pub fn envelope(mut req: Value, model: &str, project: Option<&str>) -> Value {
    let claude = model.contains("claude");
    let image = model.contains("image");
    if let Some(o) = req.as_object_mut() {
        o.remove("model");
        o.remove("safetySettings");
        o.remove("safety_settings");
    }
    // Function schemas: the backend only accepts its OpenAPI subset under `parameters`.
    let validated = claude || model.contains("gemini-3-pro") || model.contains("gemini-3.1-pro");
    for t in req.get_mut("tools").and_then(Value::as_array_mut).into_iter().flatten() {
        for key in ["functionDeclarations", "function_declarations"] {
            for f in t.get_mut(key).and_then(Value::as_array_mut).into_iter().flatten() {
                let Some(fo) = f.as_object_mut() else { continue };
                let schema = ["parametersJsonSchema", "parameters_json_schema", "parameters"]
                    .iter()
                    .find_map(|k| fo.remove(*k))
                    .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
                fo.insert("parameters".into(), schema::clean_tool(&schema, validated));
            }
        }
    }
    for key in ["generationConfig", "generation_config"] {
        let Some(gc) = req.get_mut(key).and_then(Value::as_object_mut) else { continue };
        for sk in ["responseSchema", "responseJsonSchema", "response_schema", "response_json_schema"] {
            if let Some(schema) = gc.get_mut(sk).filter(|v| v.is_object()) {
                *schema = schema::clean_response(schema);
            }
        }
        if !claude {
            gc.remove("maxOutputTokens");
        }
        // gpt-oss on Antigravity has no thinking controls.
        if model.starts_with("gpt-oss") {
            gc.remove("thinkingConfig");
        }
    }
    if claude && req["tools"].as_array().is_some_and(|t| !t.is_empty()) {
        req["toolConfig"]["functionCallingConfig"]["mode"] = "VALIDATED".into();
    }
    let mut env = json!({
        "model": model,
        "userAgent": "antigravity",
        "requestType": if image { "image_gen" } else { "agent" },
    });
    if let Some(p) = project {
        env["project"] = p.into();
    }
    if image {
        env["requestId"] =
            format!("image_gen/{}/{}/12", chrono::Utc::now().timestamp_millis(), uuid::Uuid::new_v4()).into();
    } else {
        env["requestId"] = format!("agent-{}", uuid::Uuid::new_v4()).into();
        if req["sessionId"].as_str().is_none_or(str::is_empty) {
            req["sessionId"] = session_id(&req).into();
        }
    }
    env["request"] = req;
    env
}

/// Cloud Code wraps each Gemini chunk as `{"response": {...}}`.
pub fn unwrap(v: Value) -> Value {
    match v {
        Value::Object(mut o) if o.get("response").is_some_and(Value::is_object) && !o.contains_key("candidates") => {
            o.remove("response").unwrap_or_default()
        }
        Value::Array(a) => Value::Array(a.into_iter().map(unwrap).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_wraps_and_cleans() {
        let req = json!({
            "model": "x",
            "contents": [{ "role": "user", "parts": [{ "text": "hello" }] }],
            "safetySettings": [],
            "tools": [{ "functionDeclarations": [{ "name": "f", "parametersJsonSchema": { "type": "object", "properties": {}, "$schema": "x" } }] }],
            "generationConfig": { "maxOutputTokens": 100 }
        });
        let env = envelope(req.clone(), "claude-sonnet-4-6", Some("proj-1"));
        assert_eq!(env["project"], "proj-1");
        assert_eq!(env["requestType"], "agent");
        assert!(env["requestId"].as_str().unwrap().starts_with("agent-"));
        let r = &env["request"];
        assert!(r.get("model").is_none() && r.get("safetySettings").is_none());
        let decl = &r["tools"][0]["functionDeclarations"][0];
        assert!(decl.get("parametersJsonSchema").is_none());
        assert_eq!(decl["parameters"]["required"], json!(["reason"]));
        assert_eq!(r["toolConfig"]["functionCallingConfig"]["mode"], "VALIDATED");
        assert_eq!(r["generationConfig"]["maxOutputTokens"], 100);
        // Same first message, same session.
        assert_eq!(r["sessionId"], envelope(req.clone(), "claude-sonnet-4-6", None)["request"]["sessionId"]);
        let gem = envelope(req, "gemini-3.8-flash-high", None);
        assert!(gem["request"]["generationConfig"].get("maxOutputTokens").is_none());
        assert!(gem["request"].get("generation_config").is_none());
        assert!(gem["request"]["tools"][0].get("function_declarations").is_none());
    }

    #[test]
    fn unwrap_strips_response_envelope() {
        let v = json!({ "response": { "candidates": [] }, "traceId": "t" });
        assert_eq!(unwrap(v), json!({ "candidates": [] }));
        let plain = json!({ "candidates": [], "response": { "x": 1 } });
        assert_eq!(unwrap(plain.clone()), plain);
    }
}
