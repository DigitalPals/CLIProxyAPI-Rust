//! OAuth device-code sign-in (RFC 8628) for Kimi, xAI (Grok) and Meta (Muse):
//! show a code, the user approves it in a browser, we poll for the token.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use serde_json::{Value, json};

use crate::accounts::{OAuth, Provider};
use crate::oauth::{file_safe, jwt_claims};
use crate::state::App;

pub mod kimi {
    pub const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
    pub const DEVICE_URL: &str = "https://auth.kimi.com/api/oauth/device_authorization";
    pub const TOKEN_URL: &str = "https://auth.kimi.com/api/oauth/token";
    pub const API_BASE: &str = "https://api.kimi.com/coding";
}

pub mod xai {
    pub const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
    pub const DISCOVERY: &str = "https://auth.x.ai/.well-known/openid-configuration";
    pub const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
    pub const API_BASE: &str = "https://api.x.ai/v1";
    /// Grok Build subscriptions are served by the CLI chat proxy.
    pub const CLI_BASE: &str = "https://cli-chat-proxy.grok.com/v1";
    pub const CLIENT_VERSION: &str = "1.0.46";
}

pub mod meta {
    pub const CLIENT_ID: &str = "1031625952748946";
    pub const DEVICE_URL: &str = "https://auth.meta.com/oidc/device/authorization/";
    pub const TOKEN_URL: &str = "https://auth.meta.com/oidc/device/token/";
    pub const MINT_URL: &str = "https://api.meta.ai/muse-code/key";
    pub const API_BASE: &str = "https://api.meta.ai/v1";
    pub const AUTH_UA: &str = "muse-code/1.0.2";
    /// Muse Code 1.4.3 for macOS arm64; the build hash is the commit compiled into that release.
    pub const API_UA: &str =
        "muse-build/1.4.3 (interactive; macos-aarch64; build 5c1bccecf3f125ff8fdc53f63558f6527ee70e56)";
}

const GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

#[derive(Clone, Debug)]
pub struct Device {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
    pub expires_in: u64,
    pub token_endpoint: String,
    /// Kimi binds tokens to a device id sent in headers.
    pub device_id: String,
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(|| "localhost".into())
}

fn device_model() -> String {
    let os = match std::env::consts::OS {
        "macos" => "macOS",
        "windows" => "Windows",
        "linux" => "Linux",
        other => other,
    };
    format!("{os} {}", std::env::consts::ARCH)
}

/// Headers Kimi's auth and API servers expect from a Kimi Code client.
pub fn kimi_headers(device_id: &str) -> Vec<(String, String)> {
    vec![
        ("X-Msh-Platform".into(), "CLIProxyAPI".into()),
        ("X-Msh-Version".into(), env!("CARGO_PKG_VERSION").into()),
        ("X-Msh-Device-Name".into(), hostname()),
        ("X-Msh-Device-Model".into(), device_model()),
        ("X-Msh-Device-Id".into(), device_id.into()),
    ]
}

async fn xai_endpoints(http: &reqwest::Client) -> Result<(String, String)> {
    let v: Value = http.get(xai::DISCOVERY).header("accept", "application/json").send().await?.json().await?;
    let ok = |k: &str| -> Result<String> {
        let u = v[k].as_str().ok_or_else(|| anyhow!("xAI discovery has no {k}"))?;
        let host = url::Url::parse(u).ok().and_then(|p| p.host_str().map(String::from)).unwrap_or_default();
        if !u.starts_with("https://") || !(host == "x.ai" || host.ends_with(".x.ai")) {
            bail!("unexpected xAI endpoint {u}");
        }
        Ok(u.to_string())
    };
    Ok((ok("device_authorization_endpoint")?, ok("token_endpoint")?))
}

pub async fn xai_token_endpoint(http: &reqwest::Client) -> Result<String> {
    Ok(xai_endpoints(http).await?.1)
}

pub async fn start(app: &App, provider: Provider) -> Result<Device> {
    let http = app.http.control(None);
    let device_id = uuid::Uuid::new_v4().to_string();
    type Headers = Vec<(String, String)>;
    let (url, token_endpoint, form, headers): (String, String, Vec<(&str, &str)>, Headers) = match provider {
        Provider::Kimi => (
            kimi::DEVICE_URL.into(),
            kimi::TOKEN_URL.into(),
            vec![("client_id", kimi::CLIENT_ID)],
            kimi_headers(&device_id),
        ),
        Provider::Xai => {
            let (device, token) = xai_endpoints(&http).await?;
            (device, token, vec![("client_id", xai::CLIENT_ID), ("scope", xai::SCOPE)], vec![])
        }
        Provider::Meta => (
            meta::DEVICE_URL.into(),
            meta::TOKEN_URL.into(),
            vec![("client_id", meta::CLIENT_ID)],
            vec![("user-agent".into(), meta::AUTH_UA.into())],
        ),
        other => bail!("{} does not use device sign-in", other.as_str()),
    };
    let mut rb = http.post(&url).header("accept", "application/json").form(&form);
    for (k, v) in headers {
        rb = rb.header(k, v);
    }
    let resp = rb.send().await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("device authorization returned {status}: {}", text.chars().take(300).collect::<String>());
    }
    let v: Value = serde_json::from_str(&text).context("invalid device authorization response")?;
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    let verification =
        Some(s("verification_uri_complete")).filter(|u| !u.is_empty()).unwrap_or_else(|| s("verification_uri"));
    if s("device_code").is_empty() || s("user_code").is_empty() || verification.is_empty() {
        bail!("device authorization response is missing fields");
    }
    Ok(Device {
        device_code: s("device_code"),
        user_code: s("user_code"),
        verification_uri: verification,
        interval: v["interval"].as_u64().unwrap_or(5).max(1),
        expires_in: v["expires_in"].as_u64().unwrap_or(900),
        token_endpoint,
        device_id,
    })
}

/// One finished sign-in, ready to be written to the auth dir.
pub struct Signed {
    pub oauth: OAuth,
    pub file: String,
    pub extra: Vec<(&'static str, Value)>,
}

/// Polls until the user approves (or the code expires).
pub async fn wait(app: &App, provider: Provider, d: &Device) -> Result<Signed> {
    let http = app.http.control(None);
    let deadline = std::time::Instant::now() + Duration::from_secs(d.expires_in.min(1800));
    let mut interval = d.interval;
    loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        if std::time::Instant::now() > deadline {
            bail!("the code expired before it was approved");
        }
        let client_id = match provider {
            Provider::Kimi => kimi::CLIENT_ID,
            Provider::Xai => xai::CLIENT_ID,
            _ => meta::CLIENT_ID,
        };
        let mut rb = http.post(&d.token_endpoint).header("accept", "application/json").form(&[
            ("grant_type", GRANT),
            ("device_code", d.device_code.as_str()),
            ("client_id", client_id),
        ]);
        match provider {
            Provider::Kimi => {
                for (k, v) in kimi_headers(&d.device_id) {
                    rb = rb.header(k, v);
                }
            }
            Provider::Meta => rb = rb.header("user-agent", meta::AUTH_UA),
            _ => {}
        }
        let resp = match rb.send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("device token poll failed: {e}");
                continue;
            }
        };
        let v: Value = resp.json().await.unwrap_or_default();
        match v["error"].as_str() {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                interval += 5;
                continue;
            }
            Some("access_denied") => bail!("the sign-in was denied"),
            Some("expired_token") => bail!("the code expired before it was approved"),
            Some(e) => bail!("{e}: {}", v["error_description"].as_str().unwrap_or_default()),
            None => {}
        }
        let Some(access) = v["access_token"].as_str().filter(|t| !t.is_empty()) else { continue };
        return finish(app, provider, d, access, &v).await;
    }
}

async fn finish(app: &App, provider: Provider, d: &Device, access: &str, v: &Value) -> Result<Signed> {
    let expires_at = v["expires_in"].as_f64().map(|s| Utc::now() + chrono::Duration::seconds(s as i64));
    let refresh = v["refresh_token"].as_str().unwrap_or_default().to_string();
    let mut o = OAuth { access_token: access.into(), refresh_token: refresh, expires_at, ..Default::default() };
    let mut extra: Vec<(&'static str, Value)> = vec![("auth_kind", "oauth".into())];
    let file = match provider {
        Provider::Kimi => {
            o.base_url = Some(kimi::API_BASE.into());
            extra.push(("device_id", d.device_id.clone().into()));
            extra.push(("domain", "kimi.com".into()));
            format!("kimi-{}.json", Utc::now().timestamp_millis())
        }
        Provider::Xai => {
            let id_token = v["id_token"].as_str().unwrap_or_default();
            let claims = jwt_claims(id_token);
            o.email = claims["email"].as_str().map(String::from);
            let sub = claims["sub"].as_str().unwrap_or_default().to_string();
            extra.push(("id_token", id_token.into()));
            extra.push(("token_endpoint", d.token_endpoint.clone().into()));
            extra.push(("sub", sub.clone().into()));
            let who = o.email.clone().unwrap_or(sub);
            format!("xai-{}.json", file_safe(if who.is_empty() { "account" } else { &who }))
        }
        _ => {
            let minted = meta_mint(&app.http.control(None), access).await?;
            o.access_token = minted["api_key"].as_str().unwrap_or_default().to_string();
            o.base_url = minted["base_url"].as_str().filter(|s| !s.is_empty()).map(String::from);
            o.email = minted["user_email"].as_str().filter(|s| !s.is_empty()).map(String::from);
            // The minted key has no fixed expiry; the device token does.
            o.expires_at = None;
            extra.push(("dca_token", access.into()));
            if let Some(t) = expires_at {
                extra.push(("dca_expired", t.to_rfc3339().into()));
            }
            for k in ["user_full_name", "subs_tier_name"] {
                if let Some(s) = minted[k].as_str() {
                    extra.push((if k == "user_full_name" { "name" } else { "subs_tier_name" }, s.into()));
                }
            }
            format!("meta-{}.json", file_safe(o.email.as_deref().unwrap_or("account")))
        }
    };
    Ok(Signed { oauth: o, file, extra })
}

/// Trades Meta's device token for an API key.
pub async fn meta_mint(http: &reqwest::Client, dca_token: &str) -> Result<Value> {
    let resp = http
        .post(meta::MINT_URL)
        .bearer_auth(dca_token)
        .header("user-agent", meta::AUTH_UA)
        .header("accept", "application/json")
        .json(&json!({ "dca_token": dca_token }))
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("minting a Meta API key returned {status}: {}", text.chars().take(300).collect::<String>());
    }
    let v: Value = serde_json::from_str(&text).context("invalid mint response")?;
    if v["api_key"].as_str().is_none_or(str::is_empty) {
        bail!("mint response has no api_key");
    }
    Ok(v)
}
