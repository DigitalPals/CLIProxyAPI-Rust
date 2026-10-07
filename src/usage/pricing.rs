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
    #[serde(default)]
    pub local_override: bool,
    #[serde(default = "usd")]
    pub currency: String,
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
    pub tier_multipliers: std::collections::BTreeMap<String, [i64; 2]>,
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
    #[serde(default)]
    pub currency: String,
    #[serde(default)]
    pub service_tier: String,
    #[serde(default)]
    pub tier_multiplier: [i64; 2],
    #[serde(default)]
    pub local_override: bool,
    /// Priced through a rate without a start date for an event older than the catalogue.
    #[serde(default)]
    pub backdated: bool,
}
fn usd() -> String {
    "USD".into()
}
impl Catalogue {
    pub fn load(overrides: Option<&Path>) -> Result<Self> {
        let raw = match overrides {
            Some(p) => std::fs::read_to_string(p).context("read pricing catalogue")?,
            None => include_str!("rates.json").to_owned(),
        };
        let mut catalogue: Self = serde_json::from_str(&raw).context("parse pricing catalogue")?;
        if overrides.is_some() {
            use sha2::{Digest, Sha256};
            catalogue.local_override = true;
            catalogue.version =
                format!("local:{}:{}", catalogue.version, &hex::encode(Sha256::digest(raw.as_bytes()))[..16]);
        }
        if catalogue.currency != "USD" {
            bail!("only USD pricing is supported");
        }
        if catalogue.version.is_empty() || catalogue.version.len() > 128 {
            bail!("invalid catalogue version");
        }
        chrono::NaiveDate::parse_from_str(&catalogue.verified_at, "%Y-%m-%d")?;
        for rate in &catalogue.rates {
            let start = rate.start()?;
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
            if rate
                .tier_multipliers
                .iter()
                .any(|(name, [n, d])| name.is_empty() || *n <= 0 || *d <= 0 || *n > 100 || *d > 100)
            {
                bail!("invalid service tier multiplier");
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
                    && a.start()? < b.effective_until.as_deref().map(date_ms).transpose()?.unwrap_or(i64::MAX)
                    && b.start()? < a.effective_until.as_deref().map(date_ms).transpose()?.unwrap_or(i64::MAX)
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
            partial: o.completeness != "complete",
            currency: "USD".into(),
            service_tier: o.service_tier.clone().unwrap_or_else(|| "standard (assumed)".into()),
            tier_multiplier: [1, 1],
            local_override: self.local_override,
            backdated: false,
        };
        let Some(model) = o.actual_model.as_ref() else {
            return snapshot;
        };
        let mut rates = self.rates.iter().filter(|r| r.provider == o.provider && r.models.contains(model));
        let Some(rate) = rates.find(|r| {
            r.start().is_ok_and(|s| s <= o.event_at_ms)
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
        if o.source == "codex" && o.source_event_id.starts_with("counter:") {
            snapshot.basis = "cumulative_usage_not_per_request".into();
            snapshot.partial = true;
            return snapshot;
        }
        if o.service_tier.is_none() || o.service_tier.as_deref() == Some("auto") {
            snapshot.assumptions.push("standard_api_tier_assumed".into());
        }
        if o.inference_geo.is_none() {
            snapshot.assumptions.push("global_standard_region_assumed".into());
        }
        if o.actual_model.is_none() {
            snapshot.assumptions.push("requested_model_rate_assumed".into());
        }
        snapshot.partial |= !snapshot.assumptions.is_empty();
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
        let tier = o.service_tier.as_deref().unwrap_or("standard");
        let [tier_n, tier_d] = if matches!(tier, "default" | "standard" | "auto") {
            [1, 1]
        } else if let Some(m) = rate.tier_multipliers.get(tier) {
            *m
        } else {
            snapshot.basis = "unsupported_service_tier".into();
            return snapshot;
        };
        snapshot.tier_multiplier = [tier_n, tier_d];
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
            let numerator = tier_n.checked_mul(if regional { 11 } else { 1 })?;
            let denominator = tier_d.checked_mul(if regional { 10 } else { 1 })?;
            sum.checked_mul(numerator)?.checked_add(denominator / 2)?.checked_div(denominator)
        })();
        snapshot.cost_nanos = cost;
        // Today's rates applied to older usage: a labelled equivalent, never historical spend.
        snapshot.backdated = cost.is_some()
            && rate.effective_from.is_none()
            && date_ms(&self.verified_at).is_ok_and(|v| o.event_at_ms < v);
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
        if self.local_override {
            snapshot.basis = format!("local_override:{}", snapshot.basis);
        }
        snapshot
    }
}
impl Rate {
    /// A rate without `effective_from` has no start date and applies to any earlier event.
    fn start(&self) -> Result<i64> {
        self.effective_from.as_deref().map(date_ms).transpose().map(|s| s.unwrap_or(i64::MIN))
    }
}
pub fn date_ms(s: &str) -> Result<i64> {
    Ok(chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")?.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis())
}

/// Bundled provenance; configured override provenance is also returned by Store queries.
pub fn catalogue_info() -> serde_json::Value {
    match Catalogue::load(None) {
        Ok(c) => {
            serde_json::json!({"version":c.version,"verified_at":c.verified_at,"currency":"USD","unit":"integer nanodollars","rate_count":c.rates.len(),"effective_dates":"null effective_from means no start date: usage older than the catalogue is priced at today's published rates, labelled a current-rate equivalent (backdated), and is not what was actually paid at the time; rates with an explicit effective_from leave earlier events unpriced","basis":"API list-price equivalent, standard tier/global region assumptions explicit; never subscription bill","sources":c.rates.iter().map(|r|r.source_url.clone()).collect::<std::collections::BTreeSet<_>>()})
        }
        Err(e) => serde_json::json!({"error":e.to_string()}),
    }
}

#[cfg(test)]
mod tier_tests {
    use super::*;
    use crate::usage::types::Tokens;
    fn event(provider: &str, model: &str) -> Observation {
        let mut o = Observation::new("proxy", "pricing-test".into(), provider, date_ms("2026-10-07").unwrap());
        o.actual_model = Some(model.into());
        o.inference_geo = Some("global".into());
        o.completeness = "complete".into();
        o.tokens = Tokens {
            input: Some(1000),
            cache_read: Some(200),
            cache_write: Some(100),
            write_5m: Some(50),
            write_1h: Some(50),
            output: Some(100),
            reasoning: Some(60),
        };
        o
    }
    #[test]
    fn tier_context_and_region_are_exact() {
        let c = Catalogue::load(None).unwrap();
        let mut o = event("openai", "gpt-6.1-sol");
        o.service_tier = Some("standard".into());
        assert_eq!(c.price(&o).cost_nanos, Some(3_270_000));
        o.service_tier = Some("flex".into());
        assert_eq!(c.price(&o).cost_nanos, Some(1_635_000));
        o.service_tier = Some("priority".into());
        assert_eq!(c.price(&o).cost_nanos, Some(6_540_000));
        o.tokens.input = Some(272001);
        assert_eq!(c.price(&o).cost_nanos, Some(2_180_088_000));
        o.inference_geo = Some("us".into());
        assert_eq!(c.price(&o).cost_nanos, Some(2_398_096_800));
    }
    #[test]
    fn anthropic_batch_and_ttl_subsets() {
        let c = Catalogue::load(None).unwrap();
        let mut o = event("anthropic", "claude-sonnet-5-5");
        o.service_tier = Some("batch".into());
        assert_eq!(c.price(&o).cost_nanos, Some(1_682_500));
        o.service_tier = Some("priority".into());
        assert_eq!(c.price(&o).cost_nanos, None);
        o.actual_model = None;
        o.requested_model = Some("claude-sonnet-5-5".into());
        assert_eq!(c.price(&o).basis, "unknown_model");
    }
    #[test]
    fn override_is_distinct_and_historical_boundary_is_exclusive() {
        let mut c = Catalogue::load(None).unwrap();
        let rate = c.rates.iter_mut().find(|r| r.models.contains(&"gpt-6.1-sol".into())).unwrap();
        rate.effective_from = Some("2025-01-01".into());
        rate.effective_until = Some("2026-10-07".into());
        let path = std::env::temp_dir().join(format!("fusebox-rates-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&path, serde_json::to_vec(&c).unwrap()).unwrap();
        let c = Catalogue::load(Some(&path)).unwrap();
        std::fs::remove_file(path).unwrap();
        let mut o = event("openai", "gpt-6.1-sol");
        assert_eq!(c.price(&o).basis, "outside_effective_period");
        o.event_at_ms -= 1;
        let p = c.price(&o);
        assert!(p.cost_nanos.is_some());
        assert!(p.local_override);
        assert!(p.catalogue_version.starts_with("local:"));
        assert!(p.basis.starts_with("local_override:"));
    }
    #[test]
    fn no_start_rate_backdates_older_events_without_partial() {
        let c = Catalogue::load(None).unwrap();
        let mut o = event("anthropic", "claude-sonnet-4-6");
        o.service_tier = Some("standard".into());
        o.event_at_ms = date_ms("2025-01-01").unwrap();
        let p = c.price(&o);
        assert!(p.cost_nanos.is_some(), "{}", p.basis);
        assert_eq!(p.basis, "current_rate_equivalent");
        assert_eq!(serde_json::to_value(&p).unwrap()["backdated"], true);
        assert!(p.assumptions.is_empty() && !p.partial);
        o.event_at_ms = date_ms("2026-10-07").unwrap();
        let p = c.price(&o);
        assert_eq!((p.cost_nanos.is_some(), p.basis.as_str()), (true, "current_rate_equivalent"));
        assert_eq!(serde_json::to_value(&p).unwrap()["backdated"], false);
    }
    #[test]
    fn explicit_start_rate_keeps_older_events_unpriced() {
        let mut c = Catalogue::load(None).unwrap();
        let rate = c.rates.iter_mut().find(|r| r.models.contains(&"claude-sonnet-4-6".into())).unwrap();
        rate.effective_from = Some("2026-01-01".into());
        let mut o = event("anthropic", "claude-sonnet-4-6");
        o.service_tier = Some("standard".into());
        o.event_at_ms = date_ms("2026-01-01").unwrap() - 1;
        let p = c.price(&o);
        assert_eq!((p.cost_nanos, p.basis.as_str()), (None, "outside_effective_period"));
        o.event_at_ms += 1;
        let p = c.price(&o);
        assert_eq!(p.basis, "api_rate_estimate");
        assert_ne!(serde_json::to_value(&p).unwrap()["backdated"], true);
    }
}
