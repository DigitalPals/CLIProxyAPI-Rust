//! Browser OAuth login (Claude, Codex, Antigravity) and token refresh for
//! every provider with expiring credentials.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::Utc;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::accounts::{Account, Credential, OAuth, Provider, write_oauth_file};
use crate::antigravity;
use crate::device;
use crate::state::App;

pub mod claude {
    pub const AUTH_URL: &str = "https://claude.ai/oauth/authorize";
    pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
    pub const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";
    pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
    pub const REDIRECT: &str = "http://localhost:54545/callback";
    pub const PORT: u16 = 54545;
    pub const SCOPE: &str = "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
}

pub mod codex {
    pub const AUTH_URL: &str = "https://auth.openai.com/oauth/authorize";
    pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
    pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
    pub const REDIRECT: &str = "http://localhost:1455/auth/callback";
    pub const PORT: u16 = 1455;
}

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

pub fn pkce() -> Pkce {
    let bytes: Vec<u8> = (0..96).map(|_| rand::random::<u8>()).collect();
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Pkce { verifier, challenge }
}

pub fn random_state() -> String {
    let bytes: Vec<u8> = (0..24).map(|_| rand::random::<u8>()).collect();
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn auth_url(provider: Provider, state: &str, p: &Pkce) -> String {
    let (base, params): (&str, Vec<(&str, &str)>) = match provider {
        Provider::Claude => (
            claude::AUTH_URL,
            vec![
                ("code", "true"),
                ("client_id", claude::CLIENT_ID),
                ("response_type", "code"),
                ("redirect_uri", claude::REDIRECT),
                ("scope", claude::SCOPE),
                ("code_challenge", &p.challenge),
                ("code_challenge_method", "S256"),
                ("state", state),
            ],
        ),
        Provider::Antigravity => (
            antigravity::AUTH_URL,
            vec![
                ("access_type", "offline"),
                ("client_id", antigravity::CLIENT_ID),
                ("prompt", "consent"),
                ("redirect_uri", antigravity::REDIRECT),
                ("response_type", "code"),
                ("scope", antigravity::SCOPES),
                ("state", state),
            ],
        ),
        _ => (
            codex::AUTH_URL,
            vec![
                ("client_id", codex::CLIENT_ID),
                ("response_type", "code"),
                ("redirect_uri", codex::REDIRECT),
                ("scope", "openid email profile offline_access"),
                ("state", state),
                ("code_challenge", &p.challenge),
                ("code_challenge_method", "S256"),
                ("prompt", "login"),
                ("id_token_add_organizations", "true"),
                ("codex_cli_simplified_flow", "true"),
            ],
        ),
    };
    let q = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(params).finish();
    format!("{base}?{q}")
}

fn claude_headers(rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    rb.header("Accept", "application/json, text/plain, */*")
        .header("Content-Type", "application/json")
        .header("User-Agent", "axios/1.15.2")
}

/// Decodes the payload of a JWT without verifying it.
pub fn jwt_claims(token: &str) -> Value {
    token
        .split('.')
        .nth(1)
        .and_then(|p| URL_SAFE_NO_PAD.decode(p.trim_end_matches('=')).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}

async fn read_json(resp: reqwest::Response) -> Result<Value> {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("token endpoint returned {status}: {}", text.chars().take(300).collect::<String>());
    }
    serde_json::from_str(&text).context("invalid token response")
}

fn expiry(v: &Value) -> Option<chrono::DateTime<Utc>> {
    v["expires_in"].as_i64().map(|s| Utc::now() + chrono::Duration::seconds(s))
}

/// Exchanges an authorization code. Returns the stored credential and file name.
pub async fn exchange(
    app: &App,
    provider: Provider,
    code: &str,
    state: &str,
    verifier: &str,
) -> Result<(OAuth, String, Vec<(&'static str, Value)>)> {
    let http = app.http.client(None);
    match provider {
        Provider::Claude => {
            // The hosted callback page shows `code#state`.
            let (code, code_state) = code.split_once('#').unwrap_or((code, state));
            let body = json!({
                "grant_type": "authorization_code",
                "code": code,
                "redirect_uri": claude::REDIRECT,
                "client_id": claude::CLIENT_ID,
                "code_verifier": verifier,
                "state": code_state,
            });
            let v = read_json(claude_headers(http.post(claude::TOKEN_URL)).json(&body).send().await?).await?;
            let mut o = OAuth {
                access_token: v["access_token"].as_str().unwrap_or_default().to_string(),
                refresh_token: v["refresh_token"].as_str().unwrap_or_default().to_string(),
                expires_at: expiry(&v),
                email: v["account"]["email_address"].as_str().map(String::from),
                account_id: v["account"]["uuid"].as_str().map(String::from),
                ..Default::default()
            };
            if o.email.is_none()
                && let Ok(p) = claude_profile(app, &o.access_token).await
            {
                o.email = p["account"]["email"].as_str().map(String::from);
                o.account_id = o.account_id.or_else(|| p["account"]["uuid"].as_str().map(String::from));
            }
            let name = format!("claude-{}.json", file_safe(o.email.as_deref().unwrap_or("account")));
            let device: String = (0..32).map(|_| format!("{:02x}", rand::random::<u8>())).collect();
            Ok((o, name, vec![("claude_device_ids", json!([device]))]))
        }
        Provider::Antigravity => {
            let form = [
                ("code", code),
                ("client_id", antigravity::CLIENT_ID),
                ("client_secret", antigravity::CLIENT_SECRET),
                ("redirect_uri", antigravity::REDIRECT),
                ("grant_type", "authorization_code"),
            ];
            let v = read_json(http.post(antigravity::TOKEN_URL).form(&form).send().await?).await?;
            let access = v["access_token"].as_str().unwrap_or_default().to_string();
            let info = read_json(
                http.get(antigravity::USERINFO_URL)
                    .bearer_auth(&access)
                    .header("user-agent", antigravity::user_agent())
                    .send()
                    .await?,
            )
            .await
            .unwrap_or_default();
            let email = info["email"].as_str().map(String::from);
            let project = match antigravity::fetch_project(app, &http, antigravity::BASE_PROD, &access).await {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!("antigravity: could not resolve the Cloud project yet: {e:#}");
                    None
                }
            };
            let o = OAuth {
                access_token: access,
                refresh_token: v["refresh_token"].as_str().unwrap_or_default().to_string(),
                expires_at: expiry(&v),
                email,
                project_id: project,
                ..Default::default()
            };
            let name = match &o.email {
                Some(e) => format!("antigravity-{}.json", file_safe(e)),
                None => "antigravity.json".into(),
            };
            let extra =
                vec![("expires_in", v["expires_in"].clone()), ("timestamp", Utc::now().timestamp_millis().into())];
            Ok((o, name, extra))
        }
        _ => {
            let form = [
                ("grant_type", "authorization_code"),
                ("client_id", codex::CLIENT_ID),
                ("code", code),
                ("redirect_uri", codex::REDIRECT),
                ("code_verifier", verifier),
            ];
            let v =
                read_json(http.post(codex::TOKEN_URL).header("Accept", "application/json").form(&form).send().await?)
                    .await?;
            let id_token = v["id_token"].as_str().unwrap_or_default().to_string();
            let claims = jwt_claims(&id_token);
            let auth = &claims["https://api.openai.com/auth"];
            let o = OAuth {
                access_token: v["access_token"].as_str().unwrap_or_default().to_string(),
                refresh_token: v["refresh_token"].as_str().unwrap_or_default().to_string(),
                expires_at: expiry(&v),
                email: claims["email"].as_str().map(String::from),
                account_id: auth["chatgpt_account_id"].as_str().map(String::from),
                ..Default::default()
            };
            let plan = auth["chatgpt_plan_type"].as_str().unwrap_or("free").to_string();
            let name =
                format!("codex-{}-{}.json", file_safe(o.email.as_deref().unwrap_or("account")), file_safe(&plan));
            Ok((o, name, vec![("id_token", id_token.into()), ("plan_type", plan.into())]))
        }
    }
}

async fn claude_profile(app: &App, token: &str) -> Result<Value> {
    let resp = claude_headers(app.http.client(None).get(claude::PROFILE_URL)).bearer_auth(token).send().await?;
    read_json(resp).await
}

pub fn file_safe(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || "@._-".contains(c) { c } else { '_' }).collect()
}

/// Refreshes an account's access token if it expires within `margin`.
pub async fn ensure_fresh(app: &App, acct: &Arc<Account>, margin: chrono::Duration, force: bool) -> Result<()> {
    let needs = |acct: &Account| match &*acct.cred.read() {
        Credential::OAuth(o) => {
            force || o.access_token.is_empty() || o.expires_at.is_some_and(|t| t - Utc::now() < margin)
        }
        _ => false,
    };
    if !needs(acct) {
        return Ok(());
    }
    let token_before = match &*acct.cred.read() {
        Credential::OAuth(o) => o.access_token.clone(),
        _ => return Ok(()),
    };
    let _guard = acct.refresh_lock.lock().await;
    let old = match &*acct.cred.read() {
        Credential::OAuth(o) => o.clone(),
        _ => return Ok(()),
    };
    // Another task refreshed while we waited for the lock.
    if old.access_token != token_before {
        return Ok(());
    }
    let self_minted = matches!(acct.provider, Provider::Vertex | Provider::Meta);
    if old.refresh_token.is_empty() && !self_minted {
        bail!("no refresh token");
    }
    let http = app.http.client(acct.proxy_url.as_deref().or(Some(&app.cfg().proxy_url)));
    let mut extra: Vec<(&str, Value)> = vec![];
    let keep = |access: String, v: &Value| OAuth {
        access_token: access,
        refresh_token: v["refresh_token"].as_str().map(String::from).unwrap_or(old.refresh_token.clone()),
        expires_at: expiry(v),
        ..old.clone()
    };
    let new = match acct.provider {
        Provider::Claude => {
            let body = json!({
                "client_id": claude::CLIENT_ID,
                "grant_type": "refresh_token",
                "refresh_token": old.refresh_token,
                "scope": claude::SCOPE,
            });
            let v = read_json(claude_headers(http.post(claude::TOKEN_URL)).json(&body).send().await?).await?;
            OAuth {
                email: v["account"]["email_address"].as_str().map(String::from).or(old.email.clone()),
                account_id: v["account"]["uuid"].as_str().map(String::from).or(old.account_id.clone()),
                ..keep(access_of(&v)?, &v)
            }
        }
        Provider::Codex => {
            let form = [
                ("client_id", codex::CLIENT_ID),
                ("grant_type", "refresh_token"),
                ("refresh_token", old.refresh_token.as_str()),
                ("scope", "openid profile email"),
            ];
            let v =
                read_json(http.post(codex::TOKEN_URL).header("Accept", "application/json").form(&form).send().await?)
                    .await?;
            let id_token = v["id_token"].as_str().map(String::from);
            let claims = id_token.as_deref().map(jwt_claims).unwrap_or(Value::Null);
            if let Some(t) = id_token {
                extra.push(("id_token", t.into()));
            }
            OAuth {
                email: claims["email"].as_str().map(String::from).or(old.email.clone()),
                account_id: claims["https://api.openai.com/auth"]["chatgpt_account_id"]
                    .as_str()
                    .map(String::from)
                    .or(old.account_id.clone()),
                ..keep(access_of(&v)?, &v)
            }
        }
        Provider::Antigravity => {
            let form = [
                ("client_id", antigravity::CLIENT_ID),
                ("client_secret", antigravity::CLIENT_SECRET),
                ("grant_type", "refresh_token"),
                ("refresh_token", old.refresh_token.as_str()),
            ];
            let v = read_json(http.post(antigravity::TOKEN_URL).form(&form).send().await?).await?;
            extra.push(("expires_in", v["expires_in"].clone()));
            extra.push(("timestamp", Utc::now().timestamp_millis().into()));
            keep(access_of(&v)?, &v)
        }
        Provider::Kimi => {
            let device_id = old.field("device_id").map(String::from).unwrap_or_else(|| acct.device_id.clone());
            let form = [
                ("client_id", device::kimi::CLIENT_ID),
                ("grant_type", "refresh_token"),
                ("refresh_token", old.refresh_token.as_str()),
            ];
            let mut rb = http.post(device::kimi::TOKEN_URL).header("accept", "application/json").form(&form);
            for (k, v) in device::kimi_headers(&device_id) {
                rb = rb.header(k, v);
            }
            let v = read_json(rb.send().await?).await?;
            keep(access_of(&v)?, &v)
        }
        Provider::Xai => {
            let endpoint = match old.field("token_endpoint") {
                Some(e) => e.to_string(),
                None => device::xai_token_endpoint(&http).await?,
            };
            let form = [
                ("grant_type", "refresh_token"),
                ("client_id", device::xai::CLIENT_ID),
                ("refresh_token", old.refresh_token.as_str()),
            ];
            let v =
                read_json(http.post(&endpoint).header("accept", "application/json").form(&form).send().await?).await?;
            if let Some(t) = v["id_token"].as_str() {
                extra.push(("id_token", t.into()));
            }
            keep(access_of(&v)?, &v)
        }
        Provider::Meta => {
            let dca =
                old.field("dca_token").ok_or_else(|| anyhow!("no device token to mint a new key; sign in again"))?;
            let minted = device::meta_mint(&http, dca).await?;
            OAuth {
                access_token: minted["api_key"].as_str().unwrap_or_default().to_string(),
                base_url: minted["base_url"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .or(old.base_url.clone()),
                expires_at: None,
                ..old.clone()
            }
        }
        Provider::Vertex => {
            let sa = old.raw.get("service_account").cloned().ok_or_else(|| anyhow!("no service account"))?;
            let (token, expires) = crate::vertex::mint(&http, &sa).await?;
            OAuth { access_token: token, expires_at: Some(expires), ..old.clone() }
        }
        _ => return Ok(()),
    };
    if let Some(path) = &acct.path {
        app.suppress_reload();
        write_oauth_file(path, acct.provider, &new, &extra).context("saving refreshed token")?;
    }
    tracing::info!(account = %acct.label, "refreshed {} token", acct.provider.as_str());
    *acct.cred.write() = Credential::OAuth(new);
    Ok(())
}

fn access_of(v: &Value) -> Result<String> {
    Ok(v["access_token"].as_str().ok_or_else(|| anyhow!("missing access_token"))?.to_string())
}

/// Fresh credentials plus any per-provider setup (Antigravity's Cloud project).
pub async fn ensure_ready(app: &App, acct: &Arc<Account>) -> Result<()> {
    ensure_fresh(app, acct, chrono::Duration::minutes(5), false).await?;
    if acct.provider == Provider::Antigravity && acct.is_oauth() {
        antigravity::ensure_ready(app, acct).await?;
    }
    Ok(())
}

/// Periodically refreshes tokens that are about to expire.
pub async fn refresher(app: Arc<App>) {
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        for acct in app.pool.all() {
            if !acct.is_oauth() || acct.state.lock().disabled {
                continue;
            }
            if let Err(e) = ensure_fresh(&app, &acct, chrono::Duration::minutes(10), false).await {
                tracing::warn!(account = %acct.label, "token refresh failed: {e:#}");
                acct.state.lock().last_error = Some(format!("refresh failed: {e}"));
            }
        }
    }
}
