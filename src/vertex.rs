//! Vertex AI with a Google Cloud service account: signs an RS256 JWT and
//! trades it for an access token (the `jwt-bearer` grant).

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration, Utc};
use serde_json::{Map, Value, json};

use crate::accounts::{OAuth, Provider, write_oauth_file};
use crate::state::App;

const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";

pub fn base_url(location: &str) -> String {
    match location.trim() {
        "" => "https://us-central1-aiplatform.googleapis.com".into(),
        "global" => "https://aiplatform.googleapis.com".into(),
        loc => format!("https://{loc}-aiplatform.googleapis.com"),
    }
}

fn pem_der(pem: &str) -> Result<(Vec<u8>, bool)> {
    let pkcs8 = pem.contains("BEGIN PRIVATE KEY");
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).map(str::trim).collect();
    let der = STANDARD.decode(body).context("private_key is not valid PEM")?;
    Ok((der, pkcs8))
}

fn sign_jwt(sa: &Value, now: DateTime<Utc>) -> Result<String> {
    let key = sa["private_key"].as_str().ok_or_else(|| anyhow!("service account has no private_key"))?;
    let email = sa["client_email"].as_str().ok_or_else(|| anyhow!("service account has no client_email"))?;
    let aud = sa["token_uri"].as_str().unwrap_or(DEFAULT_TOKEN_URI);
    let mut header = json!({ "alg": "RS256", "typ": "JWT" });
    if let Some(kid) = sa["private_key_id"].as_str() {
        header["kid"] = kid.into();
    }
    let claims = json!({
        "iss": email, "scope": SCOPE, "aud": aud,
        "iat": now.timestamp(), "exp": (now + Duration::hours(1)).timestamp(),
    });
    let signing_input =
        format!("{}.{}", URL_SAFE_NO_PAD.encode(header.to_string()), URL_SAFE_NO_PAD.encode(claims.to_string()));
    let (der, pkcs8) = pem_der(key)?;
    let pair =
        if pkcs8 { ring::signature::RsaKeyPair::from_pkcs8(&der) } else { ring::signature::RsaKeyPair::from_der(&der) }
            .map_err(|e| anyhow!("unusable private_key: {e}"))?;
    let mut sig = vec![0u8; pair.public().modulus_len()];
    pair.sign(&ring::signature::RSA_PKCS1_SHA256, &ring::rand::SystemRandom::new(), signing_input.as_bytes(), &mut sig)
        .map_err(|_| anyhow!("signing failed"))?;
    Ok(format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(sig)))
}

/// Exchanges the service account for a one-hour access token.
pub async fn mint(http: &reqwest::Client, sa: &Value) -> Result<(String, DateTime<Utc>)> {
    let jwt = sign_jwt(sa, Utc::now())?;
    let uri = sa["token_uri"].as_str().unwrap_or(DEFAULT_TOKEN_URI);
    let resp = http
        .post(uri)
        .form(&[("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"), ("assertion", jwt.as_str())])
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("service account token exchange returned {status}: {}", text.chars().take(300).collect::<String>());
    }
    let v: Value = serde_json::from_str(&text).context("invalid token response")?;
    let token = v["access_token"].as_str().ok_or_else(|| anyhow!("missing access_token"))?.to_string();
    let expires = Utc::now() + Duration::seconds(v["expires_in"].as_i64().unwrap_or(3600));
    Ok((token, expires))
}

/// Validates a pasted service-account key and stores it in the auth dir.
pub async fn import(app: &App, text: &str, location: &str) -> Result<String> {
    let mut sa: Value = serde_json::from_str(text.trim()).context("that is not valid JSON")?;
    // Accept a CLIProxyAPI vertex credential file as well as a raw key.
    if sa["type"] == "vertex" && sa["service_account"].is_object() {
        sa = sa["service_account"].clone();
    }
    if sa["type"] != "service_account" {
        bail!("expected a service account key (\"type\": \"service_account\")");
    }
    let project = sa["project_id"].as_str().ok_or_else(|| anyhow!("service account has no project_id"))?.to_string();
    let email = sa["client_email"].as_str().unwrap_or_default().to_string();
    let (token, expires) = mint(&app.http.control(None), &sa).await?;
    let location = if location.trim().is_empty() { "us-central1" } else { location.trim() };
    let mut raw = Map::new();
    raw.insert("service_account".into(), sa);
    raw.insert("location".into(), location.into());
    let o = OAuth {
        access_token: token,
        expires_at: Some(expires),
        email: Some(email.clone()),
        project_id: Some(project.clone()),
        raw,
        ..Default::default()
    };
    let name = format!("vertex-{}.json", crate::oauth::file_safe(&project));
    let path = app.cfg().auth_dir().join(&name);
    let extra = [("service_account", o.raw["service_account"].clone()), ("location", location.into())];
    write_oauth_file(&path, Provider::Vertex, &o, &extra)?;
    app.reload_accounts();
    Ok(if email.is_empty() { project } else { email })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations_map_to_regional_hosts() {
        assert_eq!(base_url("global"), "https://aiplatform.googleapis.com");
        assert_eq!(base_url("europe-west4"), "https://europe-west4-aiplatform.googleapis.com");
        assert_eq!(base_url(""), "https://us-central1-aiplatform.googleapis.com");
    }

    #[test]
    fn rejects_missing_key() {
        assert!(sign_jwt(&json!({ "client_email": "a@b" }), Utc::now()).is_err());
    }
}
