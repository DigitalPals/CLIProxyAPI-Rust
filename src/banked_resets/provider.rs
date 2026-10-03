//! Fixed subscription endpoints used by CLIProxyAPI's management center.
use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Result, anyhow, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::accounts::Provider;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Grant {
    pub id: String,
    pub label: String,
    pub remaining: u64,
    pub expires_at: Option<DateTime<Utc>>,
    pub starts_at: Option<DateTime<Utc>>,
    pub clears: Vec<String>,
    pub usable: bool,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Inventory {
    pub available: Option<u64>,
    pub applicable: Option<u64>,
    pub eligible: bool,
    pub grants: Vec<Grant>,
    pub reason: Option<String>,
    pub selected_grant: Option<String>,
}

fn timestamp(v: &Value) -> Result<Option<DateTime<Utc>>> {
    if v.is_null() {
        return Ok(None);
    }
    if let Some(s) = v.as_str() {
        return Ok(Some(
            DateTime::parse_from_rfc3339(s).map_err(|_| anyhow!("Invalid reset expiry"))?.with_timezone(&Utc),
        ));
    }
    if let Some(t) = v.as_i64() {
        return Ok(Some(DateTime::from_timestamp(t, 0).ok_or_else(|| anyhow!("Invalid reset expiry"))?));
    }
    Err(anyhow!("Invalid reset expiry"))
}

pub fn codex(usage: &Value, details: &Value, now: DateTime<Utc>) -> Result<Inventory> {
    ensure!(
        details.is_object() && (details.get("credits").is_some() || details.get("available_count").is_some()),
        "Reset inventory unavailable"
    );
    let counts = &usage["rate_limit_reset_credits"];
    let count = |v: &Value| -> Result<Option<u64>> {
        if v.is_null() {
            Ok(None)
        } else {
            Ok(Some(v.as_u64().filter(|v| *v <= 1_000_000).ok_or_else(|| anyhow!("Invalid reset count"))?))
        }
    };
    let available = count(&details["available_count"])?.or(count(&counts["available_count"])?);
    let applicable = count(&counts["applicable_available_count"])?.or(count(&details["applicable_available_count"])?);
    let mut grants = Vec::new();
    let mut seen = HashSet::new();
    if let Some(credits) = details.get("credits") {
        for credit in credits.as_array().filter(|v| v.len() <= 1000).ok_or_else(|| anyhow!("Invalid reset grants"))? {
            if credit["reset_type"] != "codex_rate_limits" || credit["status"] != "available" {
                continue;
            }
            let id = credit["id"].as_str().filter(|s| s.len() <= 200).unwrap_or("");
            ensure!(id.is_empty() || seen.insert(id), "Duplicate reset grant");
            let expires = timestamp(&credit["expires_at"])?;
            let usable = expires.is_none_or(|t| t > now);
            grants.push(Grant {
                id: id.into(),
                label: "Codex reset".into(),
                remaining: 1,
                expires_at: expires,
                starts_at: None,
                clears: vec!["subscription limits".into()],
                usable,
                reason: if usable { None } else { Some("Reset has expired".into()) },
            });
        }
    }
    let available = available.or_else(|| {
        details["credits"]
            .is_array()
            .then(|| grants.iter().filter(|g| g.expires_at.is_none_or(|t| t > now)).count() as u64)
    });
    // The usage endpoint's applicable count describes current limits, not whether
    // a saved credit can be manually redeemed. Match the upstream management
    // center: offer a manual reset when credits remain; the consume endpoint
    // decides whether to accept it. Known expired credits still cannot be used.
    let eligible = available.is_some_and(|n| n > 0) && (grants.is_empty() || grants.iter().any(|g| g.usable));
    let reason = if available == Some(0) {
        Some("No banked resets available".into())
    } else if !grants.is_empty() && !grants.iter().any(|g| g.usable) {
        Some("Available resets have expired".into())
    } else if available.is_none() {
        Some("Reset count unavailable".into())
    } else {
        None
    };
    Ok(Inventory { available, applicable, eligible, grants, reason, selected_grant: None })
}

pub fn claude(v: &Value, now: DateTime<Utc>) -> Result<Inventory> {
    let block = &v["cedar_ember"];
    let eligible = block["eligible"].as_bool().ok_or_else(|| anyhow!("Reset grants unavailable for this account"))?;
    let limited = if block["at_limit"].is_null() {
        false
    } else {
        block["at_limit"].as_bool().ok_or_else(|| anyhow!("Invalid reset eligibility"))?
    };
    let cooldown = timestamp(&block["cooldown_until"])?;
    let mut grants = Vec::new();
    let mut seen = HashSet::new();
    let empty = Vec::new();
    let items = if block["grants"].is_null() {
        &empty
    } else {
        block["grants"].as_array().filter(|v| v.len() <= 1000).ok_or_else(|| anyhow!("Invalid reset grants"))?
    };
    for raw in items {
        let id = raw["id"]
            .as_str()
            .filter(|s| {
                !s.is_empty()
                    && s.len() <= 40
                    && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
            })
            .ok_or_else(|| anyhow!("Invalid reset grant"))?;
        ensure!(seen.insert(id), "Duplicate reset grant");
        let total =
            raw["resets_total"].as_u64().filter(|n| *n <= 1_000_000).ok_or_else(|| anyhow!("Invalid reset count"))?;
        let remaining =
            raw["resets_left"].as_u64().filter(|n| *n <= total).ok_or_else(|| anyhow!("Invalid reset count"))?;
        let expires = timestamp(&raw["ends_at"])?;
        let starts = timestamp(&raw["starts_at"])?;
        let clears = if raw["clears"].is_null() {
            &empty
        } else {
            raw["clears"].as_array().ok_or_else(|| anyhow!("Reset scope unavailable"))?
        }
        .iter()
        .filter_map(|s| match s.as_str()? {
            "five_hour" => Some("5h".into()),
            "seven_day" => Some("week".into()),
            "seven_day_overage_included" => Some("week overage".into()),
            _ => None,
        })
        .collect::<Vec<String>>();
        let boolean = |name: &str, default| -> Result<bool> {
            if raw[name].is_null() {
                Ok(default)
            } else {
                raw[name].as_bool().ok_or_else(|| anyhow!("Invalid reset eligibility"))
            }
        };
        let paused = boolean("paused", false)?;
        let usable_now = boolean("usable_now", false)?;
        let requires_limit = boolean("use_requires_limit", true)?;
        let reason = if !eligible {
            Some("Account is not eligible")
        } else if remaining == 0 {
            Some("Grant is used up")
        } else if expires.is_some_and(|t| t <= now) {
            Some("Grant has expired")
        } else if starts.is_some_and(|t| t > now) {
            Some("Grant is not active yet")
        } else if cooldown.is_some_and(|t| t > now) {
            Some("Reset cooldown is active")
        } else if paused {
            Some("Grant is paused")
        } else if !usable_now {
            Some("Grant is not currently usable")
        } else if requires_limit && !limited {
            Some("Account must reach its limit first")
        } else if clears.is_empty() {
            Some("Reset scope is unsupported")
        } else {
            None
        };
        let label =
            raw["label"].as_str().unwrap_or("Claude reset").chars().filter(|c| !c.is_control()).take(120).collect();
        grants.push(Grant {
            id: id.into(),
            label,
            remaining,
            expires_at: expires,
            starts_at: starts,
            clears,
            usable: reason.is_none(),
            reason: reason.map(String::from),
        });
    }
    let selected = block["next_grant_id"]
        .as_str()
        .and_then(|id| grants.iter().find(|g| g.id == id && g.usable))
        .or_else(|| grants.iter().filter(|g| g.usable).min_by_key(|g| (g.expires_at.is_none(), g.expires_at, &g.id)))
        .map(|g| g.id.clone());
    let available = grants.iter().filter(|g| g.expires_at.is_none_or(|t| t > now)).map(|g| g.remaining).sum();
    let applicable = grants.iter().filter(|g| g.usable).map(|g| g.remaining).sum();
    let reason = if selected.is_some() {
        None
    } else {
        Some(
            if !eligible {
                "Account is not eligible"
            } else if available == 0 {
                "No banked resets available"
            } else {
                "No grant can be applied right now"
            }
            .into(),
        )
    };
    Ok(Inventory {
        available: Some(available),
        applicable: Some(applicable),
        eligible: selected.is_some(),
        grants,
        reason,
        selected_grant: selected,
    })
}

pub struct HttpProvider {
    pub provider: Provider,
    pub client: reqwest::Client,
    pub token: String,
    pub account_id: String,
    #[cfg(test)]
    pub origin: Option<String>,
}

pub trait Api {
    fn read(&self) -> impl std::future::Future<Output = Result<(Inventory, Value)>> + Send;
    fn identity(&self) -> impl std::future::Future<Output = Result<String>> + Send;
    fn redeem(
        &self,
        organization: &str,
        grant: &str,
        request: &str,
    ) -> impl std::future::Future<Output = Result<Outcome>> + Send;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Applied,
    AlreadyUsed,
    Refused(&'static str),
    Unknown,
}

impl HttpProvider {
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let origin =
            if self.provider == Provider::Claude { "https://api.anthropic.com" } else { "https://chatgpt.com" };
        #[cfg(test)]
        let origin = self.origin.as_deref().unwrap_or(origin);
        let mut rb = self
            .client
            .request(method, format!("{origin}{path}"))
            .bearer_auth(&self.token)
            .header("accept", "application/json")
            .timeout(Duration::from_secs(25));
        if self.provider == Provider::Claude {
            rb = rb.header("anthropic-beta", "oauth-2025-04-20").header("user-agent", crate::upstream::CC_USER_AGENT);
        } else {
            rb = rb
                .header("chatgpt-account-id", &self.account_id)
                .header("user-agent", crate::upstream::CODEX_USER_AGENT)
                .header("originator", crate::upstream::CODEX_ORIGINATOR)
                .header("openai-beta", "codex-1");
        }
        rb
    }
    async fn get(&self, path: &str) -> Result<Value> {
        let resp = self
            .request(reqwest::Method::GET, path)
            .send()
            .await
            .map_err(|_| anyhow!("Could not read provider reset status"))?;
        ensure!(resp.status().is_success(), "Provider reset status unavailable (HTTP {})", resp.status().as_u16());
        json_body(resp).await
    }
}

async fn json_body(mut response: reqwest::Response) -> Result<Value> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| anyhow!("Provider response interrupted"))? {
        ensure!(bytes.len() + chunk.len() <= 1 << 20, "Provider response too large");
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| anyhow!("Invalid provider response"))
}

impl Api for HttpProvider {
    async fn read(&self) -> Result<(Inventory, Value)> {
        match self.provider {
            Provider::Codex => {
                let usage = self.get("/backend-api/wham/usage").await?;
                let details = self.get("/backend-api/wham/rate-limit-reset-credits").await?;
                Ok((codex(&usage, &details, Utc::now())?, usage))
            }
            Provider::Claude => {
                let usage = self.get("/api/oauth/usage?cedar_ember=1&skip_spend=1").await?;
                Ok((claude(&usage, Utc::now())?, usage))
            }
            _ => Err(anyhow!("Banked resets are not supported by this provider")),
        }
    }
    async fn identity(&self) -> Result<String> {
        if self.provider == Provider::Codex {
            return Ok(self.account_id.clone());
        }
        let profile = self.get("/api/oauth/profile").await?;
        let id = profile["account"]["uuid"].as_str().ok_or_else(|| anyhow!("Provider account identity unavailable"))?;
        ensure!(id == self.account_id, "Provider account identity changed; reload accounts");
        let id = profile["organization"]["uuid"]
            .as_str()
            .and_then(|s| uuid::Uuid::parse_str(s).ok())
            .ok_or_else(|| anyhow!("Provider organization identity unavailable"))?;
        Ok(id.to_string())
    }
    async fn redeem(&self, organization: &str, grant: &str, request: &str) -> Result<Outcome> {
        let (path, body) = if self.provider == Provider::Codex {
            ("/backend-api/wham/rate-limit-reset-credits/consume".to_string(), json!({"redeem_request_id":request}))
        } else {
            ensure!(uuid::Uuid::parse_str(organization).is_ok(), "Invalid provider identity");
            (
                format!("/api/organizations/{organization}/reset_rate_limits"),
                json!({"program":"cedar_ember", "grant_id":grant, "request_id":request}),
            )
        };
        let response = match self.request(reqwest::Method::POST, &path).json(&body).send().await {
            Ok(r) => r,
            Err(_) => return Ok(Outcome::Unknown),
        };
        let status = response.status().as_u16();
        if matches!(status, 401 | 403) {
            return Ok(Outcome::Refused("Provider refused authorization"));
        }
        if status == 429 {
            return Ok(Outcome::Refused("Provider rate limited the reset"));
        }
        if !(200..300).contains(&status) {
            return Ok(Outcome::Unknown);
        }
        if self.provider == Provider::Codex {
            return Ok(Outcome::Applied);
        }
        let v = match json_body(response).await {
            Ok(v) => v,
            Err(_) => return Ok(Outcome::Unknown),
        };
        Ok(match v["result"].as_str() {
            Some("reset") => Outcome::Applied,
            Some("already_used") => Outcome::AlreadyUsed,
            Some("not_limited") => Outcome::Refused("Account is not currently limited"),
            Some("cooldown") => Outcome::Refused("Reset cooldown is active"),
            Some("ineligible") => Outcome::Refused("Account is not eligible"),
            Some("unavailable") => Outcome::Refused("Reset is unavailable"),
            _ => Outcome::Unknown,
        })
    }
}
