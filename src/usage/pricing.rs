//! Versioned, integer-only price snapshots. Provider subscriptions are never prices.
use super::types::Observation;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Catalogue {
    pub version: String,
    pub verified_at: String,
    pub rates: Vec<Rate>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Rate {
    pub provider: String,
    pub models: Vec<String>,
    pub effective_from: Option<String>,
    pub effective_until: Option<String>,
    pub source_url: String,
    pub input_nanos_per_token: i64,
    pub cache_read_nanos_per_token: Option<i64>,
    #[serde(default)]
    pub cache_write_nanos_per_token: Option<i64>,
    pub cache_write_5m_nanos_per_token: Option<i64>,
    pub cache_write_1h_nanos_per_token: Option<i64>,
    pub output_nanos_per_token: i64,
    #[serde(default)]
    pub threshold_input_tokens: Option<u64>,
    #[serde(default)]
    pub above_threshold_input_nanos_per_token: Option<i64>,
    #[serde(default)]
    pub above_threshold_read_nanos_per_token: Option<i64>,
    #[serde(default)]
    pub above_threshold_write_nanos_per_token: Option<i64>,
    #[serde(default)]
    pub above_threshold_output_nanos_per_token: Option<i64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub cost_nanos: Option<i64>,
    pub catalogue_version: String,
    pub basis: String,
    pub rate: Option<Rate>,
    pub assumptions: Vec<String>,
    pub partial: bool,
}
impl Catalogue {
    pub fn load(overrides: Option<&Path>) -> Result<Self> {
        let raw = match overrides {
            Some(p) => std::fs::read_to_string(p).context("read pricing catalogue")?,
            None => include_str!("rates.json").to_owned(),
        };
        let catalogue: Self = serde_json::from_str(&raw).context("parse pricing catalogue")?;
        if catalogue.version.is_empty() || catalogue.version.len() > 128 {
            bail!("invalid catalogue version");
        }
        chrono::NaiveDate::parse_from_str(&catalogue.verified_at, "%Y-%m-%d")?;
        for rate in &catalogue.rates {
            let start = date_ms(rate.effective_from.as_deref().unwrap_or(&catalogue.verified_at))?;
            if rate.effective_until.as_deref().map(date_ms).transpose()?.is_some_and(|end| end <= start) {
                bail!("invalid pricing effective period");
            }
            if rate.models.is_empty() || rate.provider.is_empty() || !rate.source_url.starts_with("https://") {
                bail!("invalid pricing provenance");
            }
            for n in [
                Some(rate.input_nanos_per_token),
                Some(rate.output_nanos_per_token),
                rate.cache_read_nanos_per_token,
                rate.cache_write_nanos_per_token,
                rate.cache_write_5m_nanos_per_token,
                rate.cache_write_1h_nanos_per_token,
                rate.above_threshold_input_nanos_per_token,
                rate.above_threshold_read_nanos_per_token,
                rate.above_threshold_write_nanos_per_token,
                rate.above_threshold_output_nanos_per_token,
            ]
            .into_iter()
            .flatten()
            {
                if n < 0 {
                    bail!("negative pricing rate");
                }
            }
            if rate.threshold_input_tokens.is_some() != rate.above_threshold_input_nanos_per_token.is_some()
                || rate.threshold_input_tokens.is_some() != rate.above_threshold_output_nanos_per_token.is_some()
            {
                bail!("invalid price threshold");
            }
        }
        for (i, a) in catalogue.rates.iter().enumerate() {
            for b in &catalogue.rates[i + 1..] {
                if a.provider == b.provider
                    && a.models.iter().any(|m| b.models.contains(m))
                    && date_ms(a.effective_from.as_deref().unwrap_or(&catalogue.verified_at))?
                        < b.effective_until.as_deref().map(date_ms).transpose()?.unwrap_or(i64::MAX)
                    && date_ms(b.effective_from.as_deref().unwrap_or(&catalogue.verified_at))?
                        < a.effective_until.as_deref().map(date_ms).transpose()?.unwrap_or(i64::MAX)
                {
                    bail!("overlapping pricing periods");
                }
            }
        }
        Ok(catalogue)
    }
    pub fn price(&self, o: &Observation) -> Snapshot {
        let mut snapshot = Snapshot {
            cost_nanos: None,
            catalogue_version: self.version.clone(),
            basis: "unknown_model".into(),
            rate: None,
            assumptions: Vec::new(),
            partial: false,
        };
        let Some(model) = o.actual_model.as_ref().or(o.requested_model.as_ref()) else {
            return snapshot;
        };
        let mut rates = self.rates.iter().filter(|r| r.provider == o.provider && r.models.contains(model));
        let Some(rate) = rates.find(|r| {
            date_ms(r.effective_from.as_deref().unwrap_or(&self.verified_at)).is_ok_and(|s| s <= o.event_at_ms)
                && r.effective_until
                    .as_deref()
                    .map(date_ms)
                    .transpose()
                    .is_ok_and(|end| end.is_none_or(|e| o.event_at_ms < e))
        }) else {
            snapshot.basis = if self.rates.iter().any(|r| r.provider == o.provider && r.models.contains(model)) {
                "outside_effective_period"
            } else {
                "unknown_model"
            }
            .into();
            return snapshot;
        };
        snapshot.rate = Some(rate.clone());
        if o.service_tier.is_none() || o.service_tier.as_deref() == Some("auto") {
            snapshot.assumptions.push("standard_api_tier_assumed".into());
        }
        if o.inference_geo.is_none() {
            snapshot.assumptions.push("global_standard_region_assumed".into());
        }
        if o.actual_model.is_none() {
            snapshot.assumptions.push("requested_model_rate_assumed".into());
        }
        snapshot.partial = !snapshot.assumptions.is_empty();
        let regional = match o.inference_geo.as_deref() {
            None | Some("global") => false,
            Some("us" | "us-only") => true,
            Some(_) => {
                snapshot.basis = "unsupported_inference_region".into();
                return snapshot;
            }
        };
        let documented_regional_model = if o.provider == "anthropic" {
            model.split('-').nth(2) == Some("5")
                || ["claude-opus-4-6", "claude-opus-4-7", "claude-opus-4-8", "claude-sonnet-4-6"]
                    .contains(&model.as_str())
        } else {
            model.starts_with("gpt-6")
                || model.starts_with("gpt-5.6")
                || model == "gpt-5.5"
                || model.starts_with("gpt-5.4")
        };
        if regional && !documented_regional_model {
            snapshot.basis = "unsupported_regional_model".into();
            return snapshot;
        }
        if o.service_tier.as_deref().is_some_and(|t| !matches!(t, "default" | "standard" | "auto")) {
            snapshot.basis = "unsupported_service_tier".into();
            return snapshot;
        }
        if o.numeric_metadata.iter().any(|(k, v)| {
            *v > 0
                && matches!(k.as_str(), "audio_input_tokens" | "audio_output_tokens" | "image_tokens" | "tool_tokens")
        }) {
            snapshot.basis = "unsupported_modality_or_tools".into();
            return snapshot;
        }
        if o.tokens.validate().is_err() {
            snapshot.basis = "invalid_tokens".into();
            return snapshot;
        }
        let t = &o.tokens;
        let (Some(input), Some(read), Some(write), Some(output)) = (t.input, t.cache_read, t.cache_write, t.output)
        else {
            snapshot.basis = "missing_tokens".into();
            return snapshot;
        };
        let Some(total_input) = input.checked_add(read).and_then(|n| n.checked_add(write)) else {
            snapshot.basis = "overflow".into();
            return snapshot;
        };
        let above = rate.threshold_input_tokens.is_some_and(|n| total_input > n);
        let input_rate =
            if above { rate.above_threshold_input_nanos_per_token.unwrap() } else { rate.input_nanos_per_token };
        let output_rate =
            if above { rate.above_threshold_output_nanos_per_token.unwrap() } else { rate.output_nanos_per_token };
        let read_rate = if above { rate.above_threshold_read_nanos_per_token } else { rate.cache_read_nanos_per_token };
        let write_rate =
            if above { rate.above_threshold_write_nanos_per_token } else { rate.cache_write_nanos_per_token };
        let cost = (|| -> Option<i64> {
            let mul = |n: u64, r: i64| i64::try_from(n).ok()?.checked_mul(r);
            let mut sum = mul(input, input_rate)?.checked_add(mul(output, output_rate)?)?;
            if read > 0 {
                sum = sum.checked_add(mul(read, read_rate?)?)?;
            }
            if write > 0 && write_rate.is_some() {
                sum = sum.checked_add(mul(write, write_rate?)?)?;
            } else if write > 0 {
                let short = t.write_5m.unwrap_or(0);
                let long = t.write_1h.unwrap_or(0);
                if short.checked_add(long)? != write {
                    return None;
                }
                if short > 0 {
                    sum = sum.checked_add(mul(short, rate.cache_write_5m_nanos_per_token?)?)?;
                }
                if long > 0 {
                    sum = sum.checked_add(mul(long, rate.cache_write_1h_nanos_per_token?)?)?;
                }
            }
            if regional {
                sum = sum.checked_mul(11)?.checked_add(5)?.checked_div(10)?;
            }
            Some(sum)
        })();
        snapshot.cost_nanos = cost;
        snapshot.basis = if snapshot.cost_nanos.is_some() {
            if rate.effective_from.is_some() { "api_rate_estimate" } else { "current_rate_equivalent" }
        } else if write > 0
            && write_rate.is_none()
            && t.write_5m.unwrap_or(0).checked_add(t.write_1h.unwrap_or(0)) != Some(write)
        {
            "unknown_cache_write_ttl"
        } else {
            "unsupported_rate_or_overflow"
        }
        .into();
        snapshot
    }
}
pub fn date_ms(s: &str) -> Result<i64> {
    Ok(chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")?.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis())
}

/// Bundled provenance; configured override provenance is also returned by Store queries.
pub fn catalogue_info() -> serde_json::Value {
    match Catalogue::load(None) {
        Ok(c) => {
            serde_json::json!({"version":c.version,"verified_at":c.verified_at,"currency":"USD","unit":"integer nanodollars","rate_count":c.rates.len(),"effective_dates":"null means current-rate equivalent from verification day; older events unpriced","basis":"API list-price equivalent, standard tier/global region assumptions explicit; never subscription bill","sources":c.rates.iter().map(|r|r.source_url.clone()).collect::<std::collections::BTreeSet<_>>()})
        }
        Err(e) => serde_json::json!({"error":e.to_string()}),
    }
}
