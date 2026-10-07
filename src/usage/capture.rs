//! Allowlisted native usage tap. Never retains a body, prompt, tool or transcript.
use super::{
    store::Store,
    types::{Observation, Tokens},
};
use crate::{accounts::Account, ir::Usage};
use parking_lot::Mutex;
use serde_json::Value;
use std::sync::Arc;

pub const REQUEST_HEADER: &str = "x-fusebox-usage-request";
pub const CLIENT_HEADER: &str = "x-fusebox-usage-client";
#[derive(Clone)]
pub struct Capture(Arc<Mutex<Observation>>);
impl Capture {
    pub fn wire(&self, value: &Value) {
        observe(&mut self.0.lock(), value);
    }
    pub fn text(&self, text: &str) {
        if let Ok(v) = serde_json::from_str(text) {
            self.wire(&v);
        }
    }
    pub fn headers(&self, headers: &axum::http::HeaderMap, status: u16) {
        let mut o = self.0.lock();
        o.status = Some(status);
        for key in ["request-id", "x-request-id"] {
            if let Some(v) = headers.get(key).and_then(|v| v.to_str().ok()).and_then(safe) {
                o.provider_request_id = Some(v);
                break;
            }
        }
    }
    pub fn model(&self, model: &str) {
        self.0.lock().actual_model = safe(model);
    }
}

pub struct RequestUsage {
    store: Store,
    logical_id: String,
    requested_model: Option<String>,
    client: Option<String>,
    session: Option<String>,
    current: Option<Capture>,
}
impl RequestUsage {
    pub fn new(store: Store, model: &str) -> Self {
        Self {
            store,
            logical_id: uuid::Uuid::new_v4().to_string(),
            requested_model: safe(model),
            client: None,
            session: None,
            current: None,
        }
    }
    pub fn client(&mut self, h: &axum::http::HeaderMap) {
        self.client = h.get(CLIENT_HEADER).and_then(|v| v.to_str().ok()).and_then(safe);
        if let Some(id) = h.get(REQUEST_HEADER).and_then(|v| v.to_str().ok()).and_then(safe) {
            self.logical_id = id;
        }
    }
    pub fn session(&mut self, id: Option<&str>) {
        self.session = id.and_then(safe);
    }
    pub fn attempt(&mut self, acct: &Account) {
        self.finish(None, None, false);
        let id = uuid::Uuid::new_v4().to_string();
        let provider = match acct.provider {
            crate::accounts::Provider::Claude => "anthropic",
            crate::accounts::Provider::Codex => "openai",
            p => p.as_str(),
        };
        let mut o = Observation::new("proxy", id.clone(), provider, chrono::Utc::now().timestamp_millis());
        o.parser_version = "proxy-native-v1".into();
        o.requested_model = self.requested_model.clone();
        o.logical_request_id = Some(self.logical_id.clone());
        o.attempt_id = Some(id);
        o.client_id = self.client.clone();
        o.session_id = self.session.clone();
        o.account_id = Some(acct.id.clone());
        o.auth_type = Some(if acct.is_oauth() { "subscription" } else { "api_key" }.into());
        self.current = Some(Capture(Arc::new(Mutex::new(o))));
    }
    pub fn tap(&self) -> Option<Capture> {
        self.current.clone()
    }
    pub fn finish(&mut self, status: Option<u16>, fallback: Option<&Usage>, logical_final: bool) {
        let Some(c) = self.current.take() else {
            return;
        };
        let mut o = c.0.lock().clone();
        if let Some(s) = status {
            o.status = Some(s);
        }
        if logical_final {
            o.logical_success = Some(status.is_some_and(|s| s < 400));
        }
        // IR fallback cannot prove absent fields zero; native tap is preferred.
        if o.completeness == "missing"
            && let Some(u) = fallback.filter(|u| u.input > 0 || u.output > 0 || u.cache_read > 0 || u.cache_write > 0)
        {
            o.tokens.input = (u.input > 0).then_some(u.input);
            o.tokens.output = (u.output > 0).then_some(u.output);
            o.tokens.cache_read = (u.cache_read > 0).then_some(u.cache_read);
            o.tokens.cache_write = (u.cache_write > 0).then_some(u.cache_write);
            o.tokens.reasoning = (u.reasoning > 0).then_some(u.reasoning);
            o.completeness = "partial".into();
        }
        if o.status.is_none_or(|s| s >= 400) && o.completeness == "complete" {
            o.completeness = "partial".into();
        }
        self.store.enqueue(o);
    }
}
fn safe(s: &str) -> Option<String> {
    (!s.is_empty() && super::types::valid_label(s).is_ok()).then(|| s.to_string())
}
fn number(v: &Value) -> Option<u64> {
    v.as_u64().filter(|n| *n <= super::types::MAX_TOKENS)
}
fn assign(dst: &mut Option<u64>, src: Option<u64>) {
    if src.is_some() {
        *dst = src;
    }
}

/// Usage snapshots replace reported fields; repeated final snapshots never add.
pub fn observe(o: &mut Observation, v: &Value) {
    let mut v = v;
    if v["response"].is_object() {
        v = &v["response"];
    }
    if v["message"].is_object() {
        v = &v["message"];
    }
    if let Some(s) = v["model"].as_str().or_else(|| v["modelVersion"].as_str()).and_then(safe) {
        o.actual_model = Some(s);
    }
    if let Some(s) = v["id"].as_str().or_else(|| v["responseId"].as_str()).and_then(safe) {
        o.response_id = Some(s);
    }
    if let Some(s) = v["service_tier"].as_str().and_then(safe) {
        o.service_tier = Some(s);
    }
    let u = &v["usage"];
    if let Some(s) = u["service_tier"].as_str().and_then(safe) {
        o.service_tier = Some(s);
    }
    if let Some(s) = u["inference_geo"].as_str().or_else(|| v["inference_geo"].as_str()).and_then(safe) {
        o.inference_geo = Some(s);
    }
    let mut invalid = false;
    if u.is_object() {
        for (paths, field) in [
            (&["/input_tokens", "/prompt_tokens"][..], "input"),
            (&["/output_tokens", "/completion_tokens"][..], "output"),
            (
                &[
                    "/cache_read_input_tokens",
                    "/input_tokens_details/cached_tokens",
                    "/prompt_tokens_details/cached_tokens",
                ][..],
                "read",
            ),
            (
                &[
                    "/cache_creation_input_tokens",
                    "/input_tokens_details/cache_creation_tokens",
                    "/input_tokens_details/cache_write_tokens",
                    "/prompt_tokens_details/cache_creation_tokens",
                    "/prompt_tokens_details/cache_write_tokens",
                ][..],
                "write",
            ),
            (&["/cache_creation/ephemeral_5m_input_tokens"][..], "5m"),
            (&["/cache_creation/ephemeral_1h_input_tokens"][..], "1h"),
            (
                &["/output_tokens_details/reasoning_tokens", "/completion_tokens_details/reasoning_tokens"][..],
                "reasoning",
            ),
        ] {
            if paths.iter().any(|p| u.pointer(p).is_some_and(|v| number(v).is_none())) {
                invalid = true;
                match field {
                    "input" => o.tokens.input = None,
                    "output" => o.tokens.output = None,
                    "read" => {
                        o.tokens.cache_read = None;
                        o.tokens.input = None
                    }
                    "write" => {
                        o.tokens.cache_write = None;
                        o.tokens.input = None
                    }
                    "5m" => o.tokens.write_5m = None,
                    "1h" => o.tokens.write_1h = None,
                    _ => o.tokens.reasoning = None,
                }
            }
        }
    }
    let mut t = Tokens::default();
    if u.is_object() {
        if o.provider == "anthropic"
            || u.get("cache_creation_input_tokens").is_some()
            || u.get("cache_read_input_tokens").is_some()
        {
            t.input = number(&u["input_tokens"]);
            t.output = number(&u["output_tokens"]);
            // Anthropic optional caching fields default to zero when input is reported.
            t.cache_read = u.get("cache_read_input_tokens").map(number).unwrap_or_else(|| t.input.map(|_| 0));
            t.cache_write = u.get("cache_creation_input_tokens").map(number).unwrap_or_else(|| t.input.map(|_| 0));
            t.write_5m = number(&u["cache_creation"]["ephemeral_5m_input_tokens"]);
            t.write_1h = number(&u["cache_creation"]["ephemeral_1h_input_tokens"]);
            if t.cache_write == Some(0) {
                if u["cache_creation"].get("ephemeral_5m_input_tokens").is_none() {
                    t.write_5m = Some(0);
                }
                if u["cache_creation"].get("ephemeral_1h_input_tokens").is_none() {
                    t.write_1h = Some(0);
                }
            }
        } else {
            let input = number(&u["input_tokens"]).or_else(|| number(&u["prompt_tokens"]));
            let d = if u["input_tokens_details"].is_object() {
                &u["input_tokens_details"]
            } else {
                &u["prompt_tokens_details"]
            };
            t.cache_read = number(&d["cached_tokens"]);
            t.cache_write = number(&d["cache_creation_tokens"])
                .or_else(|| number(&d["cache_write_tokens"]))
                .or_else(|| number(&u["cache_creation_input_tokens"]));
            t.input = input.and_then(|n| n.checked_sub(t.cache_read?).and_then(|n| n.checked_sub(t.cache_write?)));
            if input.is_some() && t.input.is_none() {
                o.tokens.input = None;
            }
            t.output = number(&u["output_tokens"]).or_else(|| number(&u["completion_tokens"]));
            t.reasoning = number(&u["output_tokens_details"]["reasoning_tokens"])
                .or_else(|| number(&u["completion_tokens_details"]["reasoning_tokens"]));
            if let Some(n) = input {
                o.numeric_metadata.insert("input_total".into(), n);
            }
            for (key, path) in [
                ("audio_input_tokens", d.get("audio_tokens")),
                ("audio_output_tokens", u.get("output_tokens_details").and_then(|d| d.get("audio_tokens"))),
            ] {
                if let Some(n) = path.and_then(number) {
                    o.numeric_metadata.insert(key.into(), n);
                }
            }
        }
        if let Some(n) = number(&u["total_tokens"]) {
            o.numeric_metadata.insert("total_tokens".into(), n);
        }
    } else if v["usageMetadata"].is_object() {
        let u = &v["usageMetadata"];
        let input = number(&u["promptTokenCount"]);
        for (key, field) in [
            ("promptTokenCount", "input"),
            ("cachedContentTokenCount", "read"),
            ("candidatesTokenCount", "output"),
            ("thoughtsTokenCount", "reasoning"),
        ] {
            if u.get(key).is_some_and(|v| number(v).is_none()) {
                invalid = true;
                match field {
                    "input" => o.tokens.input = None,
                    "read" => {
                        o.tokens.cache_read = None;
                        o.tokens.input = None
                    }
                    "output" => o.tokens.output = None,
                    _ => {
                        o.tokens.reasoning = None;
                        o.tokens.output = None
                    }
                }
            }
        }
        t.cache_read = u.get("cachedContentTokenCount").map(number).unwrap_or_else(|| input.map(|_| 0));
        t.cache_write = input.map(|_| 0);
        t.input = input.and_then(|n| n.checked_sub(t.cache_read?));
        t.reasoning = number(&u["thoughtsTokenCount"]);
        // Gemini candidates exclude thoughts, unlike OpenAI output.
        t.output = number(&u["candidatesTokenCount"])
            .and_then(|n| n.checked_add(if u.get("thoughtsTokenCount").is_some() { t.reasoning? } else { 0 }));
        if let Some(n) = number(&u["totalTokenCount"]) {
            o.numeric_metadata.insert("total_tokens".into(), n);
        }
    } else {
        return;
    }
    assign(&mut o.tokens.input, t.input);
    assign(&mut o.tokens.output, t.output);
    assign(&mut o.tokens.cache_read, t.cache_read);
    assign(&mut o.tokens.cache_write, t.cache_write);
    assign(&mut o.tokens.write_5m, t.write_5m);
    assign(&mut o.tokens.write_1h, t.write_1h);
    assign(&mut o.tokens.reasoning, t.reasoning);
    if o.tokens.reasoning.zip(o.tokens.output).is_some_and(|(r, out)| r > out) {
        o.tokens.reasoning = None;
        invalid = true;
    }
    if o.tokens
        .cache_write
        .is_some_and(|w| o.tokens.write_5m.unwrap_or(0).saturating_add(o.tokens.write_1h.unwrap_or(0)) > w)
    {
        o.tokens.write_5m = None;
        o.tokens.write_1h = None;
        invalid = true;
    }
    o.completeness =
        if !invalid && o.tokens.total().is_some() && o.tokens.validate().is_ok() { "complete" } else { "partial" }
            .into();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn snapshots_and_subsets() {
        let mut o = Observation::new("proxy", "id".into(), "openai", chrono::Utc::now().timestamp_millis());
        let v = json!({"response":{"id":"resp_1","model":"actual","usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":20,"cache_creation_tokens":10},"output_tokens":40,"output_tokens_details":{"reasoning_tokens":30}}}});
        observe(&mut o, &v);
        observe(&mut o, &v);
        assert_eq!(o.tokens.total(), Some(140));
        assert_eq!(o.tokens.input, Some(70));
        assert_eq!(o.tokens.reasoning, Some(30));
        assert_eq!(o.actual_model.as_deref(), Some("actual"));
    }
    #[test]
    fn claude_delta_does_not_erase_input() {
        let mut o = Observation::new("proxy", "id".into(), "anthropic", chrono::Utc::now().timestamp_millis());
        observe(
            &mut o,
            &json!({"message":{"id":"msg_1","usage":{"input_tokens":100,"output_tokens":0,"cache_creation_input_tokens":20,"cache_read_input_tokens":30,"cache_creation":{"ephemeral_5m_input_tokens":5,"ephemeral_1h_input_tokens":15}}}}),
        );
        observe(&mut o, &json!({"usage":{"output_tokens":50}}));
        assert_eq!(o.tokens.total(), Some(200));
        assert_eq!(o.tokens.write_1h, Some(15));
    }
    #[test]
    fn absent_cache_details_are_not_zero() {
        let mut o = Observation::new("proxy", "id".into(), "openai-compat", chrono::Utc::now().timestamp_millis());
        observe(&mut o, &json!({"usage":{"input_tokens":100,"output_tokens":20}}));
        assert_eq!(o.tokens.cache_read, None);
        assert_eq!(o.tokens.cache_write, None);
        assert_eq!(o.tokens.input, None);
        assert_eq!(o.numeric_metadata["input_total"], 100);
        assert_eq!(o.completeness, "partial");
    }
    #[test]
    fn malformed_snapshot_never_becomes_zero_or_a_stale_complete_value() {
        let mut o = Observation::new("proxy", "id".into(), "anthropic", chrono::Utc::now().timestamp_millis());
        observe(&mut o, &json!({"usage":{"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":-1}}));
        assert_eq!(o.tokens.cache_read, None);
        assert_eq!(o.completeness, "partial");
        observe(&mut o, &json!({"usage":{"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":0}}));
        assert_eq!(o.completeness, "complete");
        observe(&mut o, &json!({"usage":{"output_tokens":-1}}));
        assert_eq!(o.tokens.output, None);
        assert_eq!(o.tokens.input, Some(100));
        assert_eq!(o.completeness, "partial");
    }
    #[test]
    fn gemini_invalid_details_stay_unknown() {
        let mut o = Observation::new("proxy", "id".into(), "gemini", chrono::Utc::now().timestamp_millis());
        observe(
            &mut o,
            &json!({"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":20,"cachedContentTokenCount":-1}}),
        );
        assert_eq!(o.tokens.cache_read, None);
        assert_eq!(o.completeness, "partial");
        observe(
            &mut o,
            &json!({"usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":20,"cachedContentTokenCount":0}}),
        );
        observe(&mut o, &json!({"usageMetadata":{"candidatesTokenCount":-1}}));
        assert_eq!(o.tokens.output, None);
        assert_eq!(o.completeness, "partial");
    }
    #[test]
    fn contradictory_cache_ttl_is_partial() {
        let mut o = Observation::new("proxy", "id".into(), "anthropic", chrono::Utc::now().timestamp_millis());
        observe(
            &mut o,
            &json!({"usage":{"input_tokens":100,"output_tokens":20,"cache_creation_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":5}}}),
        );
        assert_eq!(o.completeness, "partial");
        assert_eq!(o.tokens.write_5m, None);
    }
    #[test]
    fn absent_usage_is_unknown() {
        let mut o = Observation::new("proxy", "id".into(), "openai", chrono::Utc::now().timestamp_millis());
        observe(&mut o, &json!({"id":"resp_1","output":[{"text":"PRIVATE_MARKER"}]}));
        assert_eq!(o.tokens.total(), None);
        assert!(!serde_json::to_string(&o).unwrap().contains("PRIVATE_MARKER"));
    }
}
