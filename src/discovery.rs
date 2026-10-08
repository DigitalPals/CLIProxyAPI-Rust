//! Model lists from the providers themselves, so a new Claude or Codex model is
//! listed (and its thinking rules known) without waiting for a Fusebox release.
//!
//! Checks are rare. Each signed-in account asks once every 6 hours, spread by up
//! to an hour so accounts don't ask together; failures retry after 15 minutes,
//! doubling up to that interval. Lists survive restarts in
//! `<auth-dir>/.model-lists.state`, so a deploy asks nothing. Codex marks a changed
//! list with `x-models-etag` on ordinary responses; that brings the next check
//! forward, but no check of one account follows another within 10 minutes.
//!
//! Models added from the dashboard (`extra-models`) are removed from config.yaml
//! once this release or a provider list has them.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::accounts::{Account, Credential, Provider};
use crate::state::App;

const STATE_FILE: &str = ".model-lists.state";
const CLAUDE_MODELS: &str = "https://api.anthropic.com/v1/models?limit=1000";
/// Between successful checks of one account.
const EVERY: i64 = 6 * 3600;
/// Added to `EVERY` at random so accounts drift apart.
const SPREAD: i64 = 3600;
const FIRST_RETRY: i64 = 15 * 60;
/// The least time between two checks of one account, whatever asked for them.
const MIN_GAP: i64 = 10 * 60;
/// Dashboard notes about removed `extra-models` entries are kept this long.
const REMOVED_KEEP_DAYS: i64 = 30;

/// One model as a provider lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Listed {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Thinking can't be turned off (Claude says `thinking.types.disabled` is unsupported).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub thinking_always_on: bool,
    /// Effort levels the model takes, when the provider says.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Entry {
    models: Vec<Listed>,
    /// Last attempt, successful or not.
    checked_at: Option<DateTime<Utc>>,
    /// Last successful fetch.
    fetched_at: Option<DateTime<Utc>>,
    next_at: Option<DateTime<Utc>>,
    failures: u32,
    /// Codex's list version, compared with the `x-models-etag` responses carry.
    etag: Option<String>,
}

/// An `extra-models` entry dropped because Fusebox came to know the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Removed {
    pub provider: String,
    pub model: String,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Stored {
    #[serde(default)]
    accounts: BTreeMap<String, Entry>,
    #[serde(default)]
    removed: Vec<Removed>,
    /// Newest Claude Code and Codex releases seen from the user's clients (`clients`).
    #[serde(default)]
    clients: BTreeMap<String, String>,
}

pub struct Store {
    path: PathBuf,
    stored: Mutex<Stored>,
    /// The removals that last failed to save, so a read-only config.yaml warns once.
    unsaved: Mutex<Vec<(String, String)>>,
}

/// What the provider lists say about Claude models, for request building.
static CAPS: LazyLock<RwLock<HashMap<String, Listed>>> = LazyLock::new(Default::default);

/// The provider says thinking can't be turned off for `model`.
pub fn thinking_always_on(model: &str) -> bool {
    CAPS.read().get(&model.to_ascii_lowercase()).is_some_and(|m| m.thinking_always_on)
}

/// The provider lists `model`'s effort levels and `effort` isn't one of them.
pub fn lacks_effort(model: &str, effort: &str) -> bool {
    CAPS.read()
        .get(&model.to_ascii_lowercase())
        .is_some_and(|m| !m.efforts.is_empty() && !m.efforts.iter().any(|e| e == effort))
}

/// A name a client can send as a model: letters, digits and `.-_:`, no prefix slash.
pub fn valid_model_id(s: &str) -> bool {
    (1..=128).contains(&s.len())
        && s.starts_with(|c: char| c.is_ascii_alphanumeric())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'))
}

/// Signed-in Claude and Codex accounts on the official endpoints have a model list.
fn eligible(acct: &Account) -> bool {
    matches!(acct.provider, Provider::Claude | Provider::Codex)
        && matches!(&*acct.cred.read(), Credential::OAuth(o) if o.base_url.is_none())
        && !acct.state.lock().disabled
}

fn next_after_success(now: DateTime<Utc>) -> DateTime<Utc> {
    now + chrono::Duration::seconds(EVERY + (rand::random::<u64>() % SPREAD as u64) as i64)
}

fn next_after_failure(now: DateTime<Utc>, failures: u32) -> DateTime<Utc> {
    let wait = FIRST_RETRY.saturating_mul(1 << failures.saturating_sub(1).min(8)).min(EVERY);
    now + chrono::Duration::seconds(wait)
}

impl Store {
    pub fn load(auth_dir: &Path) -> Self {
        let path = auth_dir.join(STATE_FILE);
        let stored: Stored = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!("{} is unreadable ({e}); model lists are fetched again", path.display());
                Stored::default()
            }),
            Err(_) => Stored::default(),
        };
        crate::clients::restore(&stored.clients);
        let store = Self { path, stored: Mutex::new(stored), unsaved: Mutex::default() };
        store.publish_caps();
        store
    }

    fn save(&self, stored: &Stored) {
        let result = serde_json::to_vec(stored)
            .map_err(anyhow::Error::from)
            .and_then(|b| crate::files::write_private(&self.path, &b));
        if let Err(e) = result {
            tracing::warn!("could not save {}: {e}", self.path.display());
        }
    }

    /// Puts the stored lists on the accounts, e.g. after a restart or an account reload.
    pub fn apply(&self, pool: &crate::accounts::Pool) {
        let stored = self.stored.lock();
        for acct in pool.all() {
            if !matches!(acct.provider, Provider::Claude | Provider::Codex) {
                continue;
            }
            let ids: Vec<String> = stored
                .accounts
                .get(&acct.id)
                .map(|e| e.models.iter().map(|m| m.id.clone()).collect())
                .unwrap_or_default();
            let mut discovered = acct.discovered.write();
            if *discovered != ids {
                *discovered = ids;
            }
        }
    }

    /// Models only leave a list when they retire, so what was learned stays.
    fn publish_caps(&self) {
        let stored = self.stored.lock();
        let mut caps = CAPS.write();
        for m in stored.accounts.values().flat_map(|e| &e.models) {
            caps.insert(m.id.to_ascii_lowercase(), m.clone());
        }
    }

    /// The account's list is due: never fetched, its time has come, or Codex reported
    /// a different list version. Never within `MIN_GAP` of the last attempt.
    pub fn due(&self, acct: &Account, now: DateTime<Utc>) -> bool {
        if !eligible(acct) {
            return false;
        }
        let stored = self.stored.lock();
        let Some(e) = stored.accounts.get(&acct.id) else { return true };
        if e.checked_at.is_some_and(|t| (now - t).num_seconds() < MIN_GAP) {
            return false;
        }
        let hinted = acct.provider == Provider::Codex
            && e.etag.is_some()
            && acct.state.lock().models_etag.as_ref().is_some_and(|seen| Some(seen) != e.etag.as_ref());
        hinted || e.next_at.is_none_or(|t| now >= t)
    }

    /// Lets "Check for new models" ask now, still at most once per `MIN_GAP` per account.
    fn bring_forward(&self, acct: &Account, now: DateTime<Utc>) -> bool {
        if !eligible(acct) {
            return false;
        }
        let mut stored = self.stored.lock();
        match stored.accounts.get_mut(&acct.id) {
            Some(e) if e.checked_at.is_some_and(|t| (now - t).num_seconds() < MIN_GAP) => false,
            Some(e) => {
                e.next_at = Some(now);
                true
            }
            None => true,
        }
    }

    fn record(
        &self,
        acct: &Account,
        now: DateTime<Utc>,
        result: Result<(Vec<Listed>, Option<String>)>,
    ) -> Result<bool> {
        let mut stored = self.stored.lock();
        let entry = stored.accounts.entry(acct.id.clone()).or_default();
        entry.checked_at = Some(now);
        let outcome = match result {
            Ok((models, etag)) => {
                let changed = entry.models != models;
                entry.models = models;
                entry.fetched_at = Some(now);
                entry.failures = 0;
                entry.next_at = Some(next_after_success(now));
                // Without a header, keep the version the responses reported, so they stop hinting.
                entry.etag = etag.or_else(|| acct.state.lock().models_etag.clone());
                Ok(changed)
            }
            Err(e) => {
                entry.failures = entry.failures.saturating_add(1);
                entry.next_at = Some(next_after_failure(now, entry.failures));
                Err(e)
            }
        };
        // Accounts not checked for a month are gone; failing ones keep their backoff.
        stored.accounts.retain(|id, e| id == &acct.id || e.checked_at.is_some_and(|t| (now - t).num_days() < 30));
        self.save(&stored);
        outcome
    }

    /// Keeps the client releases `clients` has seen for the next start.
    pub fn save_clients(&self) {
        let mut stored = self.stored.lock();
        stored.clients = crate::clients::seen();
        self.save(&stored);
    }

    pub fn removed(&self) -> Vec<Removed> {
        self.stored.lock().removed.clone()
    }

    fn note_removed(&self, list: &[(String, String)], now: DateTime<Utc>) {
        let mut stored = self.stored.lock();
        stored.removed.retain(|r| (now - r.at).num_days() < REMOVED_KEEP_DAYS);
        for (provider, model) in list {
            stored.removed.push(Removed { provider: provider.clone(), model: model.clone(), at: now });
        }
        let excess = stored.removed.len().saturating_sub(20);
        stored.removed.drain(..excess);
        self.save(&stored);
    }
}

/// Fetches one account's model list and stores it. True when the list changed.
pub async fn refresh(app: &App, acct: &Arc<Account>) -> bool {
    let now = Utc::now();
    let result = async {
        crate::oauth::ensure_fresh(app, acct, chrono::Duration::minutes(5), false).await?;
        let (token, account_id) = match &*acct.cred.read() {
            Credential::OAuth(o) => (o.access_token.clone(), o.account_id.clone()),
            Credential::ApiKey { .. } => bail!("not a sign-in"),
        };
        let http = app.http.control_for_account(acct);
        match acct.provider {
            Provider::Claude => fetch_claude(&http, &token).await.map(|m| (m, None)),
            Provider::Codex => fetch_codex(&http, &token, account_id.as_deref()).await,
            _ => bail!("no model list"),
        }
    }
    .await;
    match app.models.record(acct, now, result) {
        Ok(changed) => {
            app.models.apply(&app.pool);
            app.models.publish_caps();
            if changed {
                tracing::info!(account = %acct.label, "model list updated");
            }
            changed
        }
        Err(e) => {
            tracing::debug!(account = %acct.label, "model list check failed: {e:#}");
            false
        }
    }
}

/// Checks every account whose list is due. True when a list changed.
pub async fn refresh_due(app: &Arc<App>) -> bool {
    let now = Utc::now();
    let due: Vec<_> = app.pool.all().into_iter().filter(|a| app.models.due(a, now)).collect();
    let mut changed = false;
    for acct in due {
        changed |= refresh(app, &acct).await;
    }
    changed
}

/// "Check for new models": asks every signed-in Claude and Codex account now, except
/// those checked in the last 10 minutes. Returns how many were asked and what is new.
pub async fn check_now(app: &Arc<App>) -> (usize, Vec<String>) {
    let now = Utc::now();
    let before: Vec<String> = app.pool.models().into_iter().map(|(m, _)| m).collect();
    let accounts: Vec<_> = app.pool.all().into_iter().filter(|a| app.models.bring_forward(a, now)).collect();
    let checks = accounts.iter().map(|a| refresh(app, a));
    futures::future::join_all(checks).await;
    prune(app);
    let new = app.pool.models().into_iter().map(|(m, _)| m).filter(|m| !before.contains(m)).collect();
    (accounts.len(), new)
}

async fn fetch_claude(http: &reqwest::Client, token: &str) -> Result<Vec<Listed>> {
    let mut models = Vec::new();
    let mut url = CLAUDE_MODELS.to_string();
    for _ in 0..5 {
        let page: Value = http
            .get(&url)
            .bearer_auth(token)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("user-agent", crate::clients::claude_user_agent())
            .timeout(Duration::from_secs(15))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        models.extend(parse_claude(&page, Utc::now()));
        match page["last_id"].as_str() {
            Some(last) if page["has_more"] == true => url = format!("{CLAUDE_MODELS}&after_id={last}"),
            _ => break,
        }
    }
    if models.is_empty() {
        bail!("empty model list");
    }
    Ok(models)
}

async fn fetch_codex(
    http: &reqwest::Client,
    token: &str,
    account_id: Option<&str>,
) -> Result<(Vec<Listed>, Option<String>)> {
    use crate::upstream::{CODEX_BACKEND, CODEX_ORIGINATOR};
    // The list depends on the client version, so ask as the newest Codex the user runs.
    let version = crate::clients::codex_version();
    let mut rb = http
        .get(format!("{CODEX_BACKEND}/models?client_version={version}"))
        .bearer_auth(token)
        .header("user-agent", crate::clients::codex_user_agent())
        .header("originator", CODEX_ORIGINATOR)
        .header("version", &version)
        .timeout(Duration::from_secs(15));
    if let Some(id) = account_id {
        rb = rb.header("chatgpt-account-id", id);
    }
    let resp = rb.send().await?.error_for_status()?;
    let etag = resp.headers().get("etag").and_then(|v| v.to_str().ok()).map(String::from);
    let models = parse_codex(&resp.json().await.context("model list")?);
    if models.is_empty() {
        bail!("empty model list");
    }
    Ok((models, etag))
}

/// Anthropic's `/v1/models` page: every model not yet retired.
fn parse_claude(page: &Value, now: DateTime<Utc>) -> Vec<Listed> {
    let retired = |m: &Value| {
        m["lifecycle"] == "retired"
            || m["retires_at"].as_str().and_then(|t| DateTime::parse_from_rfc3339(t).ok()).is_some_and(|t| t <= now)
    };
    let supported = |v: &Value| v["supported"].as_bool();
    page["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| !retired(m))
        .filter_map(|m| {
            let id = m["id"].as_str().filter(|id| valid_model_id(id))?;
            let caps = &m["capabilities"];
            let effort = &caps["effort"];
            let efforts = match effort.as_object() {
                Some(levels) if supported(effort) == Some(true) => {
                    levels.iter().filter(|(_, v)| supported(v) == Some(true)).map(|(k, _)| k.clone()).collect()
                }
                _ => vec![],
            };
            Some(Listed {
                id: id.to_string(),
                name: m["display_name"].as_str().map(String::from),
                thinking_always_on: supported(&caps["thinking"]["types"]["disabled"]) == Some(false),
                efforts,
            })
        })
        .collect()
}

/// The Codex backend's `/models`: models its picker shows (hidden ones are internal).
fn parse_codex(v: &Value) -> Vec<Listed> {
    v["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["visibility"].as_str().is_none_or(|v| v == "list"))
        .filter_map(|m| {
            let id = m["slug"].as_str().filter(|id| valid_model_id(id))?;
            let efforts = m["supported_reasoning_levels"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|l| l["effort"].as_str().map(String::from))
                .collect();
            Some(Listed {
                id: id.to_string(),
                name: m["display_name"].as_str().map(String::from),
                thinking_always_on: false,
                efforts,
            })
        })
        .collect()
}

/// `extra-models` entries Fusebox now knows, by config key and model.
fn redundant(app: &App) -> Vec<(String, String)> {
    let cfg = app.cfg();
    cfg.extra_models
        .iter()
        .filter_map(|(key, models)| Some((key, Provider::parse(key)?, models)))
        .flat_map(|(key, provider, models)| {
            models.iter().filter(move |m| app.pool.knows(provider, m)).map(move |m| (key.clone(), m.clone()))
        })
        .collect()
}

/// True when `model` was added from the dashboard and Fusebox doesn't know it yet.
pub fn added(app: &App, provider: Provider, model: &str) -> bool {
    !app.pool.knows(provider, model)
        && app
            .cfg()
            .extra_models
            .iter()
            .any(|(k, ms)| Provider::parse(k) == Some(provider) && ms.iter().any(|m| m.eq_ignore_ascii_case(model)))
}

/// Removes `extra-models` entries that this release or a provider list now has.
pub fn prune(app: &App) {
    let stale = redundant(app);
    if stale.is_empty() {
        app.models.unsaved.lock().clear();
        return;
    }
    let mut next = app.cfg().extra_models.clone();
    for (key, model) in &stale {
        if let Some(list) = next.get_mut(key) {
            list.retain(|m| m != model);
        }
    }
    next.retain(|_, list| !list.is_empty());
    let names = stale.iter().map(|(_, m)| m.as_str()).collect::<Vec<_>>().join(", ");
    match save_extra_models(app, next) {
        Ok(()) => {
            tracing::info!("Fusebox now knows {names}; removed from the added models");
            app.models.note_removed(&stale, Utc::now());
            app.models.unsaved.lock().clear();
            app.broadcast("accounts", Value::Null);
        }
        Err(e) => {
            let mut unsaved = app.models.unsaved.lock();
            if *unsaved != stale {
                tracing::warn!("Fusebox now knows {names}, but config.yaml could not be updated: {e:#}");
                *unsaved = stale;
            }
        }
    }
}

/// Writes `extra-models` through the config editor, keeping the file's comments.
pub fn save_extra_models(app: &App, models: BTreeMap<String, Vec<String>>) -> Result<()> {
    let _guard = app.config_write.lock();
    let text = std::fs::read_to_string(&app.cfg_path).context("read config")?;
    let (out, cfg, rewritten) = with_extra_models(&text, models)?;
    if out != text {
        if rewritten {
            crate::mgmt::keep_original(app, &text);
        }
        std::fs::write(&app.cfg_path, &out).context("save config")?;
        app.set_config(cfg);
    }
    Ok(())
}

/// `text` with `extra-models` set to `models`. A new section is appended in block
/// style (the editor writes new values inline); the last entry gone, so is the section.
fn with_extra_models(
    text: &str,
    models: BTreeMap<String, Vec<String>>,
) -> Result<(String, crate::config::Config, bool)> {
    let empty = models.is_empty();
    let changes = json!({ "extra-models": &models });
    // Validates as any settings change does, whichever way the result is written.
    let (out, cfg, rewritten) = crate::config_editor::apply(text, changes.as_object().unwrap())?;
    let before: serde_yaml::Value = serde_yaml::from_str(text)?;
    if !empty && before.get("extra-models").is_none() {
        let mut out = text.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&serde_yaml::to_string(&BTreeMap::from([("extra-models", models)]))?);
        let parsed = crate::config::Config::parse(&out)?;
        anyhow::ensure!(parsed.extra_models == cfg.extra_models, "Could not write the added models");
        return Ok((out, parsed, false));
    }
    if !empty {
        return Ok((out, cfg, rewritten));
    }
    let mut after: serde_yaml::Value = serde_yaml::from_str(&out)?;
    if let Some(root) = after.as_mapping_mut() {
        root.remove("extra-models");
    }
    let (out, also) = crate::config_editor::render(text, &before, &after)?;
    Ok((out.clone(), crate::config::Config::parse(&out)?, rewritten || also))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_lists_skip_retired_models_and_keep_thinking_rules() {
        let now = Utc::now();
        let page = json!({ "data": [
            { "id": "claude-haiku-5-5", "display_name": "Claude Haiku 5.5", "lifecycle": "active",
              "capabilities": { "thinking": { "supported": true, "types": { "adaptive": { "supported": true }, "disabled": { "supported": true } } },
                                "effort": { "supported": true, "low": { "supported": true }, "xhigh": { "supported": true } } } },
            { "id": "claude-opus-5-5", "capabilities": { "thinking": { "types": { "disabled": { "supported": false } } },
                                "effort": { "supported": true, "high": { "supported": true }, "xhigh": { "supported": false } } } },
            { "id": "claude-haiku-4-5-20251001", "capabilities": { "effort": { "supported": false, "low": { "supported": false } } } },
            { "id": "claude-3-old", "lifecycle": "retired" },
            { "id": "claude-sonnet-4-5", "retires_at": (now - chrono::Duration::days(1)).to_rfc3339() },
            { "id": "has spaces" }
        ], "has_more": false });
        let models = parse_claude(&page, now);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["claude-haiku-5-5", "claude-opus-5-5", "claude-haiku-4-5-20251001"]);
        assert_eq!(models[0].name.as_deref(), Some("Claude Haiku 5.5"));
        assert!(!models[0].thinking_always_on);
        assert_eq!(models[0].efforts, ["low", "xhigh"]);
        assert!(models[1].thinking_always_on);
        assert_eq!(models[1].efforts, ["high"]);
        assert!(models[2].efforts.is_empty());
    }

    #[test]
    fn codex_lists_keep_picker_models_only() {
        let v = json!({ "models": [
            { "slug": "gpt-6.2-sol", "display_name": "GPT-6.2 Sol", "visibility": "list",
              "supported_reasoning_levels": [{ "effort": "low" }, { "effort": "high" }] },
            { "slug": "codex-auto-review", "visibility": "hide" },
            { "slug": "gpt-6-luna" }
        ]});
        let models = parse_codex(&v);
        assert_eq!(models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["gpt-6.2-sol", "gpt-6-luna"]);
        assert_eq!(models[0].efforts, ["low", "high"]);
    }

    #[test]
    fn checks_are_spread_out_and_failures_back_off() {
        let now = Utc::now();
        for _ in 0..50 {
            let wait = (next_after_success(now) - now).num_seconds();
            assert!((EVERY..EVERY + SPREAD).contains(&wait), "{wait}");
        }
        let waits: Vec<i64> = (1..=8).map(|n| (next_after_failure(now, n) - now).num_seconds()).collect();
        assert_eq!(waits[..4], [900, 1800, 3600, 7200]);
        assert!(waits.iter().all(|w| *w <= EVERY));
        assert_eq!(*waits.last().unwrap(), EVERY);
    }

    #[test]
    fn a_new_section_is_written_in_block_style_and_edits_stay_clean() {
        let source = "# Keep me\nport: 8317";
        let one = |p: &str, m: &[&str]| (p.to_string(), m.iter().map(|m| m.to_string()).collect::<Vec<_>>());
        let (out, cfg, _) = with_extra_models(source, BTreeMap::from([one("claude", &["claude-opus-6"])])).unwrap();
        assert_eq!(out, "# Keep me\nport: 8317\nextra-models:\n  claude:\n  - claude-opus-6\n");
        assert_eq!(cfg.extra_models["claude"], ["claude-opus-6"]);
        let two = BTreeMap::from([one("claude", &["claude-opus-6", "claude-sonnet-6"]), one("codex", &["gpt-7"])]);
        let (out, _, _) = with_extra_models(&out, two).unwrap();
        let (out, cfg, _) = with_extra_models(&out, BTreeMap::from([one("claude", &["claude-opus-6"])])).unwrap();
        assert_eq!(cfg.extra_models.len(), 1);
        assert!(!out.contains(", ]") && !out.contains('{'), "{out}");
        assert!(with_extra_models(source, BTreeMap::from([one("claude", &["bad id"])])).is_err());
    }

    #[test]
    fn the_last_added_model_takes_its_section_along() {
        let source = "# Keep me\nport: 8317\nextra-models:\n  claude: [claude-opus-6]  # until 0.4\n  codex: [gpt-7]\n";
        let one = BTreeMap::from([("codex".to_string(), vec!["gpt-7".to_string()])]);
        let (out, cfg, _) = with_extra_models(source, one).unwrap();
        assert_eq!(cfg.extra_models.len(), 1);
        assert!(out.contains("# Keep me") && !out.contains("claude-opus-6"), "{out}");
        let (out, cfg, _) = with_extra_models(&out, BTreeMap::new()).unwrap();
        assert!(cfg.extra_models.is_empty());
        assert_eq!(out, "# Keep me\nport: 8317\n");
        let (out, _, _) = with_extra_models(&out, BTreeMap::new()).unwrap();
        assert_eq!(out, "# Keep me\nport: 8317\n");
    }

    #[test]
    fn each_account_is_asked_rarely_and_codex_hints_bring_it_forward() {
        let dir = std::env::temp_dir().join(format!("fusebox-models-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        for (file, kind) in [("claude-a@x.json", "claude"), ("codex-b@x.json", "codex")] {
            std::fs::write(dir.join(file), json!({ "type": kind, "access_token": "t", "email": file }).to_string())
                .unwrap();
        }
        let cfg = crate::config::Config { auth_dir: dir.display().to_string(), ..Default::default() };
        let pool = crate::accounts::Pool::default();
        pool.reload(&cfg);
        let by = |p| pool.all().into_iter().find(|a| a.provider == p).unwrap();
        let (claude, codex) = (by(Provider::Claude), by(Provider::Codex));
        let store = Store::load(&dir);
        let now = Utc::now();
        let at = |s: i64| now + chrono::Duration::seconds(s);
        let list = |id: &str| vec![Listed { id: id.into(), name: None, thinking_always_on: false, efforts: vec![] }];

        assert!(store.due(&claude, now), "never fetched");
        assert!(store.record(&claude, now, Ok((list("claude-haiku-9"), None))).unwrap());
        assert!(!store.due(&claude, at(MIN_GAP + 1)));
        assert!(!store.due(&claude, at(EVERY - 1)));
        assert!(store.due(&claude, at(EVERY + SPREAD)));
        assert!(!store.bring_forward(&claude, at(60)), "asked a minute ago");
        assert!(store.bring_forward(&claude, at(MIN_GAP + 1)) && store.due(&claude, at(MIN_GAP + 1)));

        assert!(store.record(&codex, now, Err(anyhow::anyhow!("503"))).is_err());
        store.record(&claude, at(1), Ok((list("claude-haiku-9"), None))).unwrap();
        assert!(!store.due(&codex, at(FIRST_RETRY - 1)), "another account's check keeps this backoff");
        assert!(store.due(&codex, at(FIRST_RETRY)));
        store.record(&codex, now, Ok((list("gpt-9"), Some("v1".into())))).unwrap();
        codex.state.lock().models_etag = Some("v1".into());
        assert!(!store.due(&codex, at(MIN_GAP + 1)), "same list version");
        codex.state.lock().models_etag = Some("v2".into());
        assert!(!store.due(&codex, at(60)), "too soon after the last check");
        assert!(store.due(&codex, at(MIN_GAP + 1)), "the list changed");

        // What the list says about thinking reaches request building.
        let rules = |id: &str, always_on, efforts: &[&str]| Listed {
            id: id.into(),
            name: None,
            thinking_always_on: always_on,
            efforts: efforts.iter().map(|e| e.to_string()).collect(),
        };
        let caps =
            vec![rules("claude-test-thinks", true, &["low", "high", "max"]), rules("claude-test-plain", false, &[])];
        store.record(&claude, at(EVERY * 2), Ok((caps, None))).unwrap();
        store.publish_caps();
        let mut body = json!({ "thinking": { "type": "disabled" } });
        crate::formats::claude::normalize_disabled_thinking(&mut body, "claude-test-thinks");
        assert_eq!(body, json!({ "thinking": { "type": "adaptive" }, "output_config": { "effort": "low" } }));
        let mut body = json!({ "thinking": { "type": "disabled" } });
        crate::formats::claude::normalize_disabled_thinking(&mut body, "claude-test-plain");
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(lacks_effort("CLAUDE-TEST-THINKS", "xhigh") && !lacks_effort("claude-test-thinks", "max"));
        assert!(!lacks_effort("claude-test-plain", "xhigh"), "no levels listed: no opinion");

        // Lists come back after a restart and reach the accounts.
        let reloaded = Store::load(&dir);
        reloaded.apply(&pool);
        assert_eq!(*claude.discovered.read(), ["claude-test-thinks", "claude-test-plain"]);
        assert!(pool.knows(Provider::Codex, "gpt-9"));
        claude.state.lock().disabled = true;
        assert!(!reloaded.due(&claude, at(EVERY * 2)), "disabled accounts are left alone");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn model_ids_are_plain_names() {
        for ok in ["claude-haiku-5-5", "gpt-6.2-sol", "gemini-4-pro:preview", "k3"] {
            assert!(valid_model_id(ok), "{ok}");
        }
        for bad in ["", "team/claude", "-x", "has space", "émoji", &"a".repeat(129)] {
            assert!(!valid_model_id(bad), "{bad}");
        }
    }
}
