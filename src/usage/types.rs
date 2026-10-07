use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_TOKENS: u64 = 1_000_000_000_000;

/// Disjoint input categories; TTL write categories and reasoning are subsets.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Tokens {
    pub input: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub write_5m: Option<u64>,
    pub write_1h: Option<u64>,
    pub output: Option<u64>,
    pub reasoning: Option<u64>,
}
impl Tokens {
    pub fn input_total(&self) -> Option<u64> {
        self.input?.checked_add(self.cache_read?)?.checked_add(self.cache_write?)
    }
    pub fn total(&self) -> Option<u64> {
        self.input_total()?.checked_add(self.output?)
    }
    pub fn validate(&self) -> Result<(), String> {
        for v in
            [self.input, self.cache_read, self.cache_write, self.write_5m, self.write_1h, self.output, self.reasoning]
                .into_iter()
                .flatten()
        {
            if v > MAX_TOKENS {
                return Err("token bound exceeded".into());
            }
        }
        if let (Some(r), Some(o)) = (self.reasoning, self.output)
            && r > o
        {
            return Err("reasoning exceeds output".into());
        }
        if let Some(w) = self.cache_write
            && self.write_5m.unwrap_or(0).saturating_add(self.write_1h.unwrap_or(0)) > w
        {
            return Err("cache TTL subsets exceed writes".into());
        }
        Ok(())
    }
}

/// Only this metadata crosses collector boundaries or enters persistent analytics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub schema_version: u32,
    pub source: String,
    pub source_event_id: String,
    pub origin_id: String,
    pub parser_version: String,
    pub event_at_ms: i64,
    pub ingested_at_ms: i64,
    pub provider: String,
    pub requested_model: Option<String>,
    pub actual_model: Option<String>,
    pub account_id: Option<String>,
    pub auth_type: Option<String>,
    pub client_id: Option<String>,
    pub logical_request_id: Option<String>,
    pub attempt_id: Option<String>,
    pub provider_request_id: Option<String>,
    pub response_id: Option<String>,
    pub session_id: Option<String>,
    pub service_tier: Option<String>,
    #[serde(default)]
    pub inference_geo: Option<String>,
    pub tokens: Tokens,
    pub completeness: String,
    pub status: Option<u16>,
    pub logical_success: Option<bool>,
    #[serde(default)]
    pub numeric_metadata: BTreeMap<String, u64>,
}
impl Observation {
    pub fn new(source: &str, id: String, provider: &str, event_at_ms: i64) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            source: source.into(),
            source_event_id: id,
            origin_id: "local".into(),
            parser_version: "1".into(),
            event_at_ms,
            ingested_at_ms: chrono::Utc::now().timestamp_millis(),
            provider: provider.into(),
            requested_model: None,
            actual_model: None,
            account_id: None,
            auth_type: None,
            client_id: None,
            logical_request_id: None,
            attempt_id: None,
            provider_request_id: None,
            response_id: None,
            session_id: None,
            service_tier: None,
            inference_geo: None,
            tokens: Tokens::default(),
            completeness: "missing".into(),
            status: None,
            logical_success: None,
            numeric_metadata: BTreeMap::new(),
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != SCHEMA_VERSION {
            return Err("unsupported schema version".into());
        }
        if !matches!(self.source.as_str(), "proxy" | "claude_code" | "codex") {
            return Err("unsupported source".into());
        }
        if !matches!(self.completeness.as_str(), "complete" | "partial" | "missing") {
            return Err("invalid completeness".into());
        }
        if self.event_at_ms < 1_577_836_800_000 || self.event_at_ms > chrono::Utc::now().timestamp_millis() + 86_400_000
        {
            return Err("event timestamp out of bounds".into());
        }
        for s in [&self.source_event_id, &self.origin_id, &self.parser_version, &self.provider] {
            valid_label(s)?;
            if s.is_empty() {
                return Err("missing identity".into());
            }
        }
        for s in [
            &self.requested_model,
            &self.actual_model,
            &self.account_id,
            &self.auth_type,
            &self.client_id,
            &self.logical_request_id,
            &self.attempt_id,
            &self.provider_request_id,
            &self.response_id,
            &self.session_id,
            &self.service_tier,
            &self.inference_geo,
        ]
        .into_iter()
        .flatten()
        {
            valid_label(s)?;
        }
        if self.numeric_metadata.len() > 8
            || self.numeric_metadata.iter().any(|(k, v)| {
                !matches!(
                    k.as_str(),
                    "total_tokens"
                        | "input_total"
                        | "cached_tokens"
                        | "context_window"
                        | "audio_input_tokens"
                        | "audio_output_tokens"
                        | "image_tokens"
                        | "tool_tokens"
                ) || *v > MAX_TOKENS
            })
        {
            return Err("unsupported numeric metadata".into());
        }
        self.tokens.validate()
    }
}
pub fn valid_label(s: &str) -> Result<(), String> {
    if s.len() > 256 || s.chars().any(char::is_control) { Err("invalid metadata text".into()) } else { Ok(()) }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
pub struct UsageConfig {
    pub enabled: bool,
    pub database: Option<String>,
    pub retention_days: u32,
    pub queue_capacity: usize,
    pub pricing_overrides: Option<String>,
}
impl Default for UsageConfig {
    fn default() -> Self {
        Self { enabled: true, database: None, retention_days: 90, queue_capacity: 1024, pricing_overrides: None }
    }
}
