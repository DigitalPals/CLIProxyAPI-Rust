//! Builds the HTTP request for each upstream provider.

use axum::http::HeaderMap;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::accounts::{Account, Credential, Provider};
use crate::config::Config;
use crate::device;
use crate::ir::Format;

pub const CLAUDE_API: &str = "https://api.anthropic.com";
pub const CODEX_BACKEND: &str = "https://chatgpt.com/backend-api/codex";
pub const OPENAI_API: &str = "https://api.openai.com/v1";
pub const GEMINI_API: &str = "https://generativelanguage.googleapis.com";

pub const CC_VERSION: &str = "2.1.296";
pub const CC_USER_AGENT: &str = "claude-cli/2.1.296 (external, cli)";
const CC_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const CC_FINGERPRINT_SALT: &str = "59cf53e54c78";
pub const CODEX_USER_AGENT: &str = "codex-tui/0.162.1 (Mac OS 26.5.2; arm64) iTerm.app/3.6.11 (codex-tui; 0.162.1)";
pub const CODEX_ORIGINATOR: &str = "codex-tui";
/// The Codex release `CODEX_USER_AGENT` claims; its model list depends on it.
pub const CODEX_VERSION: &str = "0.162.1";
pub const CODEX_WS_BETA: &str = "responses_websockets=2026-02-06";

pub struct Prepared {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Value,
    /// Pre-encoded body (Devin's protobuf); `body` is then only for logging.
    pub raw: Option<Vec<u8>>,
}

pub struct Target<'a> {
    pub acct: &'a Account,
    pub cfg: &'a Config,
    pub client_headers: &'a HeaderMap,
    pub model: &'a str,
    /// Dialect of `body` (one of the provider's wires).
    pub wire: Format,
    pub passthrough: bool,
    pub stream: bool,
    pub count_tokens: bool,
    /// A stable per-session key for providers that route their prompt cache by one.
    pub cache_key: Option<&'a str>,
}

/// Headers never copied from the client to an upstream.
const HOP: &[&str] = &[
    "host",
    "authorization",
    "x-api-key",
    "x-goog-api-key",
    "content-length",
    "connection",
    "accept-encoding",
    "transfer-encoding",
    "cookie",
    "upgrade",
    "te",
    "keep-alive",
    "proxy-authorization",
    "proxy-connection",
    "x-fusebox-usage-request",
    "x-fusebox-usage-client",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-real-ip",
    "origin",
    "referer",
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-extensions",
    "sec-websocket-protocol",
];

fn header(h: &HeaderMap, name: &str) -> Option<String> {
    h.get(name).and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub fn is_claude_code(h: &HeaderMap) -> bool {
    header(h, "user-agent").is_some_and(|ua| ua.to_ascii_lowercase().starts_with("claude-cli/"))
}

fn creds(acct: &Account) -> (String, Option<String>, bool, Option<String>) {
    match &*acct.cred.read() {
        Credential::OAuth(o) => (
            o.access_token.clone(),
            o.base_url.as_ref().map(|b| b.trim_end_matches('/').to_string()),
            true,
            o.account_id.clone(),
        ),
        Credential::ApiKey { key, base_url } => (
            key.clone(),
            base_url.clone().filter(|b| !b.trim().is_empty()).map(|b| b.trim_end_matches('/').to_string()),
            false,
            None,
        ),
    }
}

fn push_custom(acct: &Account, headers: &mut Vec<(String, String)>) {
    for (k, v) in &acct.headers {
        headers.retain(|(n, _)| !n.eq_ignore_ascii_case(k));
        headers.push((k.clone(), v.clone()));
    }
}

pub fn prepare(t: &Target, body: Value) -> Prepared {
    use crate::accounts::Provider::*;
    let mut p = match t.acct.provider {
        Claude => claude(t, body),
        Codex => codex(t, body),
        Gemini => gemini(t, body),
        Vertex => vertex(t, body),
        Antigravity => antigravity(t, body),
        Kimi => kimi(t, body),
        Xai | Meta => responses_api(t, body),
        Devin => crate::devin::prepare(t, body),
        Compat => compat(t, body),
    };
    push_custom(t.acct, &mut p.headers);
    p
}

// ---------------------------------------------------------------------- claude

fn claude(t: &Target, mut body: Value) -> Prepared {
    let (token, base, oauth, account_uuid) = creds(t.acct);
    let base = base.unwrap_or_else(|| CLAUDE_API.into());
    let path = if t.count_tokens { "/v1/messages/count_tokens" } else { "/v1/messages" };
    let url = if oauth { format!("{base}{path}?beta=true") } else { format!("{base}{path}") };
    body["model"] = t.model.into();
    strip_foreign_thinking(&mut body);
    crate::formats::claude::normalize_disabled_thinking(&mut body, t.model);

    let native_cc = t.passthrough && is_claude_code(t.client_headers);
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut betas: Vec<String> = Vec::new();
    if let Some(b) = header(t.client_headers, "anthropic-beta") {
        betas.extend(b.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    }

    if native_cc {
        // Claude Code already speaks the right dialect: forward it untouched.
        for (k, v) in t.client_headers {
            let name = k.as_str();
            if HOP.contains(&name) || name == "anthropic-beta" {
                continue;
            }
            if let Ok(v) = v.to_str() {
                headers.push((name.to_string(), v.to_string()));
            }
        }
    } else if oauth && t.cfg.claude_cloak {
        let mut cc = vec![
            "claude-code-20250219",
            "oauth-2025-04-20",
            "interleaved-thinking-2025-05-14",
            "context-management-2025-06-27",
            "prompt-caching-scope-2026-01-05",
        ];
        if body["output_config"]["effort"].is_string() {
            cc.push("effort-2025-11-24");
        }
        if body["output_config"]["format"].is_object() {
            cc.push("structured-outputs-2025-12-15");
        }
        let caller = std::mem::take(&mut betas);
        betas = cc.into_iter().map(String::from).collect();
        betas.extend(caller);
        cloak_body(&mut body, t.acct, account_uuid.as_deref(), t.count_tokens);
        headers.extend(
            [
                ("anthropic-version", "2023-06-01"),
                ("anthropic-dangerous-direct-browser-access", "true"),
                ("x-app", "cli"),
                ("user-agent", CC_USER_AGENT),
                ("x-stainless-lang", "js"),
                ("x-stainless-package-version", "0.128.0"),
                ("x-stainless-os", "MacOS"),
                ("x-stainless-arch", "arm64"),
                ("x-stainless-runtime", "node"),
                ("x-stainless-runtime-version", "v26.3.0"),
                ("x-stainless-retry-count", "0"),
                ("x-stainless-timeout", "600"),
                ("x-claude-code-session-id", t.acct.session_id.as_str()),
            ]
            .map(|(k, v)| (k.to_string(), v.to_string())),
        );
    } else {
        let version = header(t.client_headers, "anthropic-version").unwrap_or_else(|| "2023-06-01".into());
        headers.push(("anthropic-version".into(), version));
        headers.push(("user-agent".into(), format!("Fusebox/{}", env!("CARGO_PKG_VERSION"))));
        if body["output_config"]["effort"].is_string() && !betas.iter().any(|b| b.starts_with("effort-")) {
            betas.push("effort-2025-11-24".into());
        }
    }
    if oauth && !betas.iter().any(|b| b == "oauth-2025-04-20") {
        betas.insert(0, "oauth-2025-04-20".into());
    }
    let mut seen = std::collections::HashSet::new();
    betas.retain(|b| seen.insert(b.clone()));
    if !betas.is_empty() {
        headers.push(("anthropic-beta".into(), betas.join(",")));
    }
    if oauth {
        headers.push(("authorization".into(), format!("Bearer {token}")));
    } else {
        headers.push(("x-api-key".into(), token));
    }
    headers.retain(|(k, _)| k != "content-type" && k != "accept");
    headers.push(("content-type".into(), "application/json".into()));
    headers.push(("accept".into(), if t.stream { "text/event-stream" } else { "application/json" }.into()));
    Prepared { url, headers, body, raw: None }
}

/// Thinking blocks that carried another provider's reasoning through a Claude
/// client would fail Anthropic's signature check.
fn strip_foreign_thinking(body: &mut Value) {
    for m in body["messages"].as_array_mut().into_iter().flatten() {
        if let Some(blocks) = m["content"].as_array_mut() {
            blocks.retain(|b| {
                !(b["type"] == "thinking" && b["signature"].as_str().is_some_and(|s| s.starts_with("cpx-")))
            });
        }
    }
}

/// JavaScript-compatible 3-char build fingerprint Claude Code puts in its billing header.
fn cc_fingerprint(message: &str) -> String {
    let units: Vec<u16> = message.encode_utf16().collect();
    let sampled: Vec<u16> = [4usize, 7, 20].iter().map(|&i| units.get(i).copied().unwrap_or(b'0' as u16)).collect();
    let input = format!("{CC_FINGERPRINT_SALT}{}{CC_VERSION}", String::from_utf16_lossy(&sampled));
    hex::encode(Sha256::digest(input.as_bytes()))[..3].to_string()
}

fn first_user_text(body: &Value) -> (Option<usize>, String) {
    let Some(msgs) = body["messages"].as_array() else { return (None, String::new()) };
    let Some(idx) = msgs.iter().position(|m| m["role"] == "user") else { return (None, String::new()) };
    let text = match &msgs[idx]["content"] {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .find(|t| !t.starts_with("<system-reminder>"))
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    };
    (Some(idx), text)
}

/// Makes a third-party request look like Claude Code: Claude Code system
/// prompt on top, caller instructions moved into the first user turn, and a
/// Claude-Code-shaped metadata.user_id.
fn cloak_body(body: &mut Value, acct: &Account, account_uuid: Option<&str>, count_tokens: bool) {
    let configured_cache = has_claude_cache_control(body);
    let original: Vec<Value> = match &body["system"] {
        Value::String(s) => vec![json!({"type": "text", "text": s})],
        Value::Array(blocks) => blocks.clone(),
        _ => vec![],
    };
    let caller: Vec<Value> = original
        .iter()
        .filter_map(|block| {
            let text = block["text"].as_str()?;
            if text.trim().is_empty() || text == CC_IDENTITY || text.starts_with("x-anthropic-billing-header") {
                return None;
            }
            let mut reminder =
                json!({"type": "text", "text": format!("<system-reminder>\n{}\n</system-reminder>", text.trim_end())});
            if !block["cache_control"].is_null() {
                reminder["cache_control"] = block["cache_control"].clone();
            }
            Some(reminder)
        })
        .collect();

    let (first_user, text) = first_user_text(body);
    let billing =
        format!("x-anthropic-billing-header: cc_version={CC_VERSION}.{}; cc_entrypoint=cli;", cc_fingerprint(&text));
    body["system"] = json!([
        { "type": "text", "text": billing },
        { "type": "text", "text": CC_IDENTITY }
    ]);
    for block in &original {
        let text = block["text"].as_str().unwrap_or_default();
        let index = if text == CC_IDENTITY {
            Some(1)
        } else if text.starts_with("x-anthropic-billing-header") {
            Some(0)
        } else {
            None
        };
        if let Some(index) = index
            && !block["cache_control"].is_null()
        {
            body["system"][index]["cache_control"] = block["cache_control"].clone();
        }
    }
    // An extra 5m breakpoint could exceed Anthropic's four-breakpoint limit or
    // precede the caller's 1h breakpoint. Respect explicit and automatic caching.
    // Without any, cache the shared tools and system prompt, and let automatic caching
    // follow the growing conversation (which now holds the caller's instructions too).
    if !configured_cache {
        body["system"][1]["cache_control"] = json!({"type": "ephemeral"});
        if !count_tokens {
            body["cache_control"] = json!({"type": "ephemeral"});
        }
    }

    if let Some(idx) = first_user.filter(|_| !caller.is_empty()) {
        let has_long_cache = caller.iter().any(|b| b["cache_control"]["ttl"] == "1h");
        let short_cache = |block: &Value| !block["cache_control"].is_null() && block["cache_control"]["ttl"] != "1h";
        let earlier_short_cache = body["system"].as_array().unwrap().iter().any(short_cache)
            || body["messages"].as_array().unwrap()[..idx]
                .iter()
                .any(|message| message["content"].as_array().into_iter().flatten().any(short_cache));
        let content = &mut body["messages"][idx]["content"];
        let mut blocks = match content.take() {
            Value::String(s) => vec![json!({ "type": "text", "text": s })],
            Value::Array(a) => a,
            _ => vec![],
        };
        let at = blocks.iter().take_while(|b| b["type"] == "tool_result").count();
        let ttl_conflict = has_long_cache && (earlier_short_cache || blocks[..at].iter().any(short_cache));
        if ttl_conflict {
            // Keep the original system-cache order before any earlier assistant
            // or leading tool-result block with a shorter cache lifetime.
            body["system"] = cloaked_system_in_original_order(&original, &body["system"], caller);
        } else {
            blocks.splice(at..at, caller);
        }
        body["messages"][idx]["content"] = Value::Array(blocks);
    } else if first_user.is_none() {
        body["system"] = cloaked_system_in_original_order(&original, &body["system"], caller);
    }

    if count_tokens {
        if let Some(o) = body.as_object_mut() {
            o.remove("metadata");
        }
        return;
    }
    let valid = body["metadata"]["user_id"]
        .as_str()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .is_some_and(|v| v["device_id"].as_str().is_some_and(|d| d.len() == 64));
    if !valid {
        let user_id = json!({
            "device_id": acct.device_id,
            "account_uuid": account_uuid.unwrap_or_default(),
            "session_id": acct.session_id,
        })
        .to_string();
        if !body["metadata"].is_object() {
            body["metadata"] = json!({});
        }
        body["metadata"]["user_id"] = user_id.into();
    }
}

fn cloaked_system_in_original_order(original: &[Value], identity: &Value, caller: Vec<Value>) -> Value {
    let mut system = Vec::new();
    for index in 0..2 {
        let present = original.iter().any(|block| {
            let text = block["text"].as_str().unwrap_or_default();
            if index == 0 { text.starts_with("x-anthropic-billing-header") } else { text == CC_IDENTITY }
        });
        if !present {
            system.push(identity[index].clone());
        }
    }
    let mut reminders = caller.into_iter();
    for block in original {
        let text = block["text"].as_str().unwrap_or_default();
        if text == CC_IDENTITY {
            system.push(identity[1].clone());
        } else if text.starts_with("x-anthropic-billing-header") {
            system.push(identity[0].clone());
        } else if !text.trim().is_empty()
            && let Some(reminder) = reminders.next()
        {
            system.push(reminder);
        }
    }
    Value::Array(system)
}

/// Any cache marker, at any depth (tool results can carry them on their inner blocks):
/// adding ours on top could exceed Anthropic's four breakpoints.
fn has_claude_cache_control(body: &Value) -> bool {
    match body {
        Value::Object(object) => {
            object.get("cache_control").is_some_and(|c| !c.is_null()) || object.values().any(has_claude_cache_control)
        }
        Value::Array(items) => items.iter().any(has_claude_cache_control),
        _ => false,
    }
}

// ----------------------------------------------------------------------- codex

/// Fields the ChatGPT Codex backend rejects (`generate` is fine on websockets).
const CODEX_STRIP: &[&str] = &[
    "max_output_tokens",
    "max_completion_tokens",
    "temperature",
    "top_p",
    "safety_identifier",
    "prompt_cache_retention",
    "generate",
    "user",
    "truncation",
    "stream_options",
    "background",
];

/// `websocket`: the request is a native websocket turn, which keeps `previous_response_id`
/// and `generate` (Codex warms a connection up with `generate: false`).
pub fn sanitize_codex_body(body: &mut Value, model: &str, websocket: bool) {
    body["model"] = model.into();
    body["store"] = false.into();
    if let Some(o) = body.as_object_mut() {
        for k in CODEX_STRIP.iter().filter(|k| !(websocket && **k == "generate")) {
            o.remove(*k);
        }
        // The ChatGPT backend only takes a list ("Input must be a list").
        if let Some(Value::String(text)) = o.get("input").cloned() {
            o.insert(
                "input".into(),
                json!([{ "type": "message", "role": "user", "content": [{ "type": "input_text", "text": text }] }]),
            );
        }
        // Codex 0.162 sends base instructions as developer input, on HTTP and websockets.
        if let Some(Value::String(text)) = o.remove("instructions")
            && !text.is_empty()
        {
            let input = o.entry("input").or_insert_with(|| json!([]));
            if input.is_null() {
                *input = json!([]);
            }
            if let Some(items) = input.as_array_mut() {
                items.insert(
                    0,
                    json!({ "type": "message", "role": "developer", "content": [{ "type": "input_text", "text": text }] }),
                );
            }
        }
        if !websocket {
            o.remove("previous_response_id");
        }
        if let Some(Value::Array(items)) = o.get_mut("input") {
            // Reasoning tunnelled from another provider can't be decrypted by OpenAI.
            items.retain(|it| {
                !(it["type"] == "reasoning" && it["encrypted_content"].as_str().is_some_and(|e| e.starts_with("cpx-")))
            });
            // Item ids reference server-side state that store=false never kept.
            for it in items.iter_mut() {
                if let Some(io) = it.as_object_mut()
                    && io.get("type").and_then(Value::as_str) != Some("item_reference")
                {
                    io.remove("id");
                }
            }
        }
    }
}

/// Codex 0.162 sends routing fields first; other keys keep their order after these.
const CODEX_FIELD_ORDER: &[&str] = &[
    "type",
    "model",
    "stream",
    "service_tier",
    "previous_response_id",
    "input",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "reasoning",
    "store",
    "stream_options",
    "include",
    "prompt_cache_key",
    "text",
    "generate",
    "client_metadata",
    "access_programs",
];

/// Puts a Codex request body's fields in the order Codex itself writes them.
pub fn order_codex_body(body: &mut Value) {
    let Some(o) = body.as_object_mut() else { return };
    let mut rest = std::mem::take(o);
    for k in CODEX_FIELD_ORDER {
        if let Some(v) = rest.remove(*k) {
            o.insert((*k).to_string(), v);
        }
    }
    o.extend(rest);
}

pub fn codex_headers(client: &HeaderMap, token: &str, account_id: Option<&str>, oauth: bool) -> Vec<(String, String)> {
    let mut h: Vec<(String, String)> = vec![("authorization".into(), format!("Bearer {token}"))];
    for name in [
        "version",
        "session_id",
        "session-id",
        "thread-id",
        "x-codex-turn-metadata",
        "x-codex-turn-state",
        "x-codex-beta-features",
        "x-client-request-id",
        "x-codex-window-id",
        "x-openai-internal-codex-responses-lite",
    ] {
        if let Some(v) = header(client, name) {
            h.push((name.into(), v));
        }
    }
    if oauth && crate::clients::is_codex(client) {
        // A real Codex client keeps its own identity, as Claude Code does.
        for name in ["user-agent", "originator"] {
            if let Some(v) = header(client, name) {
                h.push((name.into(), v));
            }
        }
        if let Some(a) = account_id {
            h.push(("chatgpt-account-id".into(), a.into()));
        }
    } else if oauth {
        if !h.iter().any(|(k, _)| k == "version") {
            h.push(("version".into(), CODEX_VERSION.into()));
        }
        h.push(("user-agent".into(), CODEX_USER_AGENT.into()));
        h.push(("originator".into(), CODEX_ORIGINATOR.into()));
        if let Some(a) = account_id {
            h.push(("chatgpt-account-id".into(), a.into()));
        }
    } else {
        h.push(("user-agent".into(), format!("Fusebox/{}", env!("CARGO_PKG_VERSION"))));
    }
    h
}

fn codex(t: &Target, mut body: Value) -> Prepared {
    let (token, base, oauth, account_id) = creds(t.acct);
    let base = base.unwrap_or_else(|| if oauth { CODEX_BACKEND.into() } else { OPENAI_API.into() });
    if oauth {
        sanitize_codex_body(&mut body, t.model, false);
        body["stream"] = true.into();
    } else {
        body["model"] = t.model.into();
        body["stream"] = t.stream.into();
    }
    // OpenAI routes cache lookups by prefix and this key. Codex always sends one; other
    // clients (and translated requests) get the session's. Custom endpoints may reject it.
    let official = oauth || base.trim_end_matches('/') == OPENAI_API;
    if official
        && body["prompt_cache_key"].is_null()
        && let Some(key) = t.cache_key
    {
        body["prompt_cache_key"] = key.into();
    }
    if oauth {
        order_codex_body(&mut body);
    }
    let mut headers = codex_headers(t.client_headers, &token, account_id.as_deref(), oauth);
    if let Some(key) = body["prompt_cache_key"].as_str().filter(|_| oauth)
        && !headers.iter().any(|(k, _)| k == "session_id" || k == "session-id")
    {
        headers.push(("session_id".into(), key.to_string()));
    }
    headers.push(("content-type".into(), "application/json".into()));
    headers.push(("accept".into(), if oauth || t.stream { "text/event-stream" } else { "application/json" }.into()));
    Prepared { url: format!("{base}/responses"), headers, body, raw: None }
}

pub fn codex_ws_url(acct: &Account, client_headers: &HeaderMap) -> (String, Vec<(String, String)>) {
    let (token, base, oauth, account_id) = creds(acct);
    let base = base.unwrap_or_else(|| if oauth { CODEX_BACKEND.into() } else { OPENAI_API.into() });
    let ws = base.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
    let mut h = codex_headers(client_headers, &token, account_id.as_deref(), oauth);
    h.push(("openai-beta".into(), CODEX_WS_BETA.into()));
    (format!("{ws}/responses"), h)
}

// ---------------------------------------------------------------------- gemini

fn gemini(t: &Target, mut body: Value) -> Prepared {
    let (token, base, _, _) = creds(t.acct);
    let base = base.unwrap_or_else(|| GEMINI_API.into());
    let action = gemini_action(t);
    if let Some(o) = body.as_object_mut() {
        o.remove("model");
    }
    let headers = vec![
        ("x-goog-api-key".into(), token),
        ("content-type".into(), "application/json".into()),
        ("user-agent".into(), format!("Fusebox/{}", env!("CARGO_PKG_VERSION"))),
    ];
    Prepared { url: format!("{base}/v1beta/models/{}:{action}", t.model), headers, body, raw: None }
}

// ---------------------------------------------------------------------- vertex

fn gemini_action(t: &Target) -> &'static str {
    if t.count_tokens {
        "countTokens"
    } else if t.stream {
        "streamGenerateContent?alt=sse"
    } else {
        "generateContent"
    }
}

fn vertex(t: &Target, mut body: Value) -> Prepared {
    let (token, base, oauth, _) = creds(t.acct);
    if let Some(o) = body.as_object_mut() {
        o.remove("model");
    }
    let action = gemini_action(t);
    let mut headers = vec![
        ("content-type".into(), "application/json".into()),
        ("user-agent".into(), format!("Fusebox/{}", env!("CARGO_PKG_VERSION"))),
    ];
    let url = if oauth {
        let (project, location) = match &*t.acct.cred.read() {
            Credential::OAuth(o) => {
                (o.project_id.clone().unwrap_or_default(), o.field("location").unwrap_or("us-central1").to_string())
            }
            _ => Default::default(),
        };
        let base = base.unwrap_or_else(|| crate::vertex::base_url(&location));
        headers.push(("authorization".into(), format!("Bearer {token}")));
        format!("{base}/v1/projects/{project}/locations/{location}/publishers/google/models/{}:{action}", t.model)
    } else {
        // Express mode: API key, no project.
        let base = base.unwrap_or_else(|| "https://aiplatform.googleapis.com".into());
        headers.push(("x-goog-api-key".into(), token));
        format!("{base}/v1/publishers/google/models/{}:{action}", t.model)
    };
    Prepared { url, headers, body, raw: None }
}

// ----------------------------------------------------------------- antigravity

fn antigravity(t: &Target, body: Value) -> Prepared {
    let (token, _, _, _) = creds(t.acct);
    let project = crate::antigravity::project(t.acct);
    let base = crate::antigravity::request_base(t.acct);
    let path = if t.stream { "streamGenerateContent?alt=sse" } else { "generateContent" };
    Prepared {
        url: format!("{base}/v1internal:{path}"),
        headers: vec![
            ("authorization".into(), format!("Bearer {token}")),
            ("content-type".into(), "application/json".into()),
            ("user-agent".into(), crate::antigravity::user_agent()),
            ("accept".into(), if t.stream { "text/event-stream" } else { "application/json" }.into()),
        ],
        body: crate::antigravity::envelope(body, &crate::antigravity::current_id(t.model), project.as_deref()),
        raw: None,
    }
}

// ------------------------------------------------------------------------ kimi

/// Inlines local `$ref`s and makes sure every tool schema is an object (Moonshot is strict).
fn normalize_chat_tools(body: &mut Value) {
    for tool in body["tools"].as_array_mut().into_iter().flatten() {
        let params = &mut tool["function"]["parameters"];
        if params.is_object() {
            let mut p = crate::schema::inline_only(params);
            if p.get("type").is_none() {
                p["type"] = "object".into();
            }
            *params = p;
        }
    }
}

fn kimi(t: &Target, mut body: Value) -> Prepared {
    let (token, base, oauth, _) = creds(t.acct);
    let base = base.unwrap_or_else(|| device::kimi::API_BASE.into());
    let root = base.trim_end_matches("/v1").to_string();
    let v1 = format!("{root}/v1");
    body["model"] = t.model.into();
    let mut headers = vec![
        ("authorization".into(), format!("Bearer {token}")),
        ("content-type".into(), "application/json".into()),
        ("user-agent".into(), format!("Fusebox/{}", env!("CARGO_PKG_VERSION"))),
        ("accept".into(), if t.stream { "text/event-stream" } else { "application/json" }.into()),
    ];
    if oauth {
        let device_id = match &*t.acct.cred.read() {
            Credential::OAuth(o) => o.field("device_id").map(String::from),
            _ => None,
        };
        headers.extend(device::kimi_headers(&device_id.unwrap_or_else(|| t.acct.device_id.clone())));
    }
    let url = match t.wire {
        Format::Claude => {
            headers.push(("x-api-key".into(), token));
            let version = header(t.client_headers, "anthropic-version").unwrap_or_else(|| "2023-06-01".into());
            headers.push(("anthropic-version".into(), version));
            if let Some(b) = header(t.client_headers, "anthropic-beta") {
                headers.push(("anthropic-beta".into(), b));
            }
            strip_foreign_thinking(&mut body);
            format!("{root}/v1/messages")
        }
        Format::Responses => {
            body["stream"] = t.stream.into();
            strip_responses_extras(&mut body);
            format!("{v1}/responses")
        }
        _ => {
            body["stream"] = t.stream.into();
            if t.stream {
                body["stream_options"] = json!({ "include_usage": true });
            }
            normalize_chat_tools(&mut body);
            // Kimi only accepts its fixed temperatures; let it choose.
            let thinking_off = body["thinking"]["type"] == "disabled";
            let temp = body["temperature"].as_f64();
            if temp.is_some_and(|x| (thinking_off && x != 0.6) || (!thinking_off && x != 1.0))
                && let Some(o) = body.as_object_mut()
            {
                o.remove("temperature");
            }
            format!("{v1}/chat/completions")
        }
    };
    Prepared { url, headers, body, raw: None }
}

// ------------------------------------------------------------------ xai / meta

/// Fields the stateless Responses backends (xAI, Meta) reject.
fn strip_responses_extras(body: &mut Value) {
    if let Some(o) = body.as_object_mut() {
        for k in ["previous_response_id", "prompt_cache_retention", "safety_identifier", "stream_options", "stop"] {
            o.remove(k);
        }
        if let Some(Value::Array(items)) = o.get_mut("input") {
            items.retain(|it| {
                !(it["type"] == "reasoning" && it["encrypted_content"].as_str().is_some_and(|e| e.starts_with("cpx-")))
            });
        }
    }
}

fn responses_api(t: &Target, mut body: Value) -> Prepared {
    let (token, base, oauth, _) = creds(t.acct);
    body["model"] = t.model.into();
    body["stream"] = t.stream.into();
    strip_responses_extras(&mut body);
    let mut headers = vec![
        ("authorization".into(), format!("Bearer {token}")),
        ("content-type".into(), "application/json".into()),
        ("accept".into(), if t.stream { "text/event-stream" } else { "application/json" }.into()),
    ];
    let base = if t.acct.provider == Provider::Meta {
        headers.push(("user-agent".into(), device::meta::API_UA.into()));
        headers.push(("x-client-id".into(), "tbh:tui".into()));
        base.unwrap_or_else(|| device::meta::API_BASE.into())
    } else {
        // xAI routes its prompt cache by conversation id.
        if let Some(key) = body["prompt_cache_key"].as_str().or(t.cache_key) {
            headers.push(("x-grok-conv-id".into(), key.to_string()));
        }
        let official = base.as_deref().is_none_or(|b| b.trim_end_matches('/') == device::xai::API_BASE);
        if oauth && official {
            // Grok subscriptions (SuperGrok / X Premium) go through the CLI chat proxy.
            headers.extend(
                [
                    ("x-xai-token-auth", "xai-grok-cli".to_string()),
                    ("x-grok-client-version", device::xai::CLIENT_VERSION.to_string()),
                    // The interactive CLI: its pager names itself and the shell it runs.
                    (
                        "user-agent",
                        format!("grok-pager/{v} grok-shell/{v} (macos; aarch64)", v = device::xai::CLIENT_VERSION),
                    ),
                    ("x-grok-client-identifier", "grok-shell".to_string()),
                    ("x-grok-client-mode", "interactive".to_string()),
                    ("x-authenticateresponse", "authenticate-response".to_string()),
                ]
                .map(|(k, v)| (k.to_string(), v)),
            );
            device::xai::CLI_BASE.into()
        } else {
            headers.push(("user-agent".into(), format!("Fusebox/{}", env!("CARGO_PKG_VERSION"))));
            base.unwrap_or_else(|| device::xai::API_BASE.into())
        }
    };
    Prepared { url: format!("{}/responses", base.trim_end_matches('/')), headers, body, raw: None }
}

// ---------------------------------------------------------------------- compat

fn compat(t: &Target, mut body: Value) -> Prepared {
    let (token, base, _, _) = creds(t.acct);
    let base = base.unwrap_or_else(|| OPENAI_API.into());
    body["model"] = t.model.into();
    if !t.passthrough {
        body["stream"] = true.into();
    }
    let mut headers = vec![
        ("content-type".into(), "application/json".into()),
        ("user-agent".into(), format!("Fusebox/{}", env!("CARGO_PKG_VERSION"))),
    ];
    if !token.is_empty() {
        headers.push(("authorization".into(), format!("Bearer {token}")));
    }
    Prepared { url: format!("{base}/chat/completions"), headers, body, raw: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grok_subscription_requests_match_1_0_50_identity() {
        let cfg = Config {
            auth_dir: "/nonexistent".into(),
            xai_api_key: vec![crate::config::KeyEntry { api_key: "test".into(), ..Default::default() }],
            ..Default::default()
        };
        let pool = crate::accounts::Pool::default();
        pool.reload(&cfg);
        let acct = pool.all().remove(0);
        *acct.cred.write() =
            Credential::OAuth(crate::accounts::OAuth { access_token: "test".into(), ..Default::default() });
        let headers = HeaderMap::new();
        let prepared = prepare(
            &Target {
                acct: &acct,
                cfg: &cfg,
                client_headers: &headers,
                model: "grok-4",
                wire: Format::Responses,
                passthrough: false,
                stream: true,
                count_tokens: false,
                cache_key: Some("conversation"),
            },
            json!({"input":"Reply OK"}),
        );
        assert_eq!(prepared.url, "https://cli-chat-proxy.grok.com/v1/responses");
        for (name, expected) in [
            ("x-grok-client-version", "1.0.50"),
            ("user-agent", "grok-pager/1.0.50 grok-shell/1.0.50 (macos; aarch64)"),
            ("x-grok-client-identifier", "grok-shell"),
            ("x-grok-client-mode", "interactive"),
            ("x-xai-token-auth", "xai-grok-cli"),
            ("x-authenticateresponse", "authenticate-response"),
            ("x-grok-conv-id", "conversation"),
        ] {
            assert_eq!(prepared.headers.iter().find(|(key, _)| key == name).unwrap().1, expected, "{name}");
        }
        assert_eq!(prepared.body, json!({"input":"Reply OK","model":"grok-4","stream":true}));
    }

    fn claude_account() -> std::sync::Arc<Account> {
        let cfg = Config {
            auth_dir: "/nonexistent".into(),
            claude_api_key: vec![crate::config::KeyEntry { api_key: "test".into(), ..Default::default() }],
            ..Default::default()
        };
        let pool = crate::accounts::Pool::default();
        pool.reload(&cfg);
        pool.all().remove(0)
    }

    #[test]
    fn claude_generated_requests_match_2_1_296_identity() {
        let acct = claude_account();
        *acct.cred.write() = Credential::OAuth(crate::accounts::OAuth {
            access_token: "test".into(),
            refresh_token: String::new(),
            expires_at: None,
            email: None,
            account_id: None,
            base_url: None,
            project_id: None,
            raw: Default::default(),
        });
        let cfg = Config::default();
        let headers = HeaderMap::new();
        let prepared = prepare(
            &Target {
                acct: &acct,
                cfg: &cfg,
                client_headers: &headers,
                model: "claude-sonnet-4-6",
                wire: Format::Claude,
                passthrough: false,
                stream: true,
                count_tokens: false,
                cache_key: None,
            },
            json!({"messages":[{"role":"user","content":"Reply OK"}]}),
        );
        assert_eq!(
            prepared.headers.iter().find(|(name, _)| name == "user-agent").unwrap().1,
            "claude-cli/2.1.296 (external, cli)"
        );
        // 9f1 is the fingerprint captured from the published 2.1.296 client for "Reply OK".
        assert_eq!(
            prepared.body["system"][0]["text"],
            "x-anthropic-billing-header: cc_version=2.1.296.9f1; cc_entrypoint=cli;"
        );
    }

    #[test]
    fn claude_cloak_preserves_four_caller_breakpoints_and_ttl_order() {
        let acct = claude_account();
        let mut body = json!({
            "tools":[{"name":"lookup","input_schema":{"type":"object"},"cache_control":{"type":"ephemeral","ttl":"1h"}}],
            "system":[
                {"type":"text","text":"stable instructions","cache_control":{"type":"ephemeral","ttl":"1h"}},
                {"type":"text","text":"brief instructions","cache_control":{"type":"ephemeral","ttl":"5m"}}
            ],
            "messages":[{"role":"user","content":[{"type":"text","text":"question","cache_control":{"type":"ephemeral"}}]}]
        });
        cloak_body(&mut body, &acct, None, false);
        assert!(body["system"][1]["cache_control"].is_null());
        let blocks = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["text"], "<system-reminder>\nstable instructions\n</system-reminder>");
        assert_eq!(blocks[0]["cache_control"], json!({"type":"ephemeral","ttl":"1h"}));
        assert_eq!(blocks[1]["cache_control"], json!({"type":"ephemeral","ttl":"5m"}));
        assert_eq!(blocks[2]["cache_control"], json!({"type":"ephemeral"}));
        assert_eq!(body["tools"][0]["cache_control"], json!({"type":"ephemeral","ttl":"1h"}));
    }

    #[test]
    fn claude_cloak_respects_automatic_caching_and_retains_default_when_unconfigured() {
        let acct = claude_account();
        let request = json!({"system":"instructions","messages":[{"role":"user","content":"question"}]});
        let mut automatic = request.clone();
        automatic["cache_control"] = json!({"type":"ephemeral","ttl":"1h"});
        cloak_body(&mut automatic, &acct, None, false);
        assert!(automatic["system"][1]["cache_control"].is_null());
        assert_eq!(automatic["cache_control"], json!({"type":"ephemeral","ttl":"1h"}));
        let mut default = request.clone();
        cloak_body(&mut default, &acct, None, false);
        assert_eq!(default["system"][1]["cache_control"], json!({"type":"ephemeral"}));
        // The conversation, which now holds the caller's instructions, is cached too.
        assert_eq!(default["cache_control"], json!({"type":"ephemeral"}));
        assert_eq!(default["messages"][0]["content"][0]["text"], "<system-reminder>\ninstructions\n</system-reminder>");
        assert!(default["messages"][0]["content"][0]["cache_control"].is_null());
        // Token counting has nothing to cache.
        let mut counted = request;
        cloak_body(&mut counted, &acct, None, true);
        assert!(counted["cache_control"].is_null());
    }

    #[test]
    fn translated_automatic_cache_covers_instructions_after_cloaking() {
        let acct = claude_account();
        let original = json!({"instructions":"stable instructions","input":"question"});
        let req = crate::formats::responses::parse_request(&original).unwrap();
        let mut body = crate::formats::claude::build_request(&req, "claude-sonnet-5-5");
        crate::formats::claude::apply_translated_cache_policy(&original, &mut body);
        cloak_body(&mut body, &acct, None, false);
        assert_eq!(body["cache_control"], json!({"type":"ephemeral"}));
        assert!(body["system"][1]["cache_control"].is_null());
        assert_eq!(
            body["messages"][0]["content"][0]["text"],
            "<system-reminder>\nstable instructions\n</system-reminder>"
        );
        assert_eq!(body["messages"][0]["content"][1]["text"], "question");
    }

    #[test]
    fn claude_cloak_does_not_move_long_ttl_after_short_cached_tool_result() {
        let acct = claude_account();
        let mut body = json!({
            "system":[{"type":"text","text":"instructions","cache_control":{"type":"ephemeral","ttl":"1h"}}],
            "messages":[
                {"role":"assistant","content":[{"type":"tool_use","id":"tool_1","name":"lookup","input":{}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"tool_1","content":"result","cache_control":{"type":"ephemeral"}}]}
            ]
        });
        let messages = body["messages"].clone();
        cloak_body(&mut body, &acct, None, false);
        assert_eq!(body["messages"], messages);
        assert_eq!(body["system"][2]["cache_control"], json!({"type":"ephemeral","ttl":"1h"}));
        assert_eq!(body["system"][2]["text"], "<system-reminder>\ninstructions\n</system-reminder>");
    }

    #[test]
    fn claude_cloak_preserves_ttl_order_before_assistant_and_identity_markers() {
        let acct = claude_account();
        let mut body = json!({
            "system":[{"type":"text","text":"instructions","cache_control":{"type":"ephemeral","ttl":"1h"}}],
            "messages":[
                {"role":"assistant","content":[{"type":"tool_use","id":"tool_1","name":"lookup","input":{},"cache_control":{"type":"ephemeral"}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"tool_1","content":"result"}]}
            ]
        });
        let messages = body["messages"].clone();
        cloak_body(&mut body, &acct, None, false);
        assert_eq!(body["messages"], messages);
        assert_eq!(body["system"][2]["cache_control"]["ttl"], "1h");

        let mut body = json!({
            "system":[
                {"type":"text","text":"instructions","cache_control":{"type":"ephemeral","ttl":"1h"}},
                {"type":"text","text":CC_IDENTITY,"cache_control":{"type":"ephemeral"}}
            ],
            "messages":[{"role":"user","content":"question"}]
        });
        cloak_body(&mut body, &acct, None, false);
        assert_eq!(body["system"][1]["cache_control"]["ttl"], "1h");
        assert_eq!(body["system"][2]["text"], CC_IDENTITY);
        assert_eq!(body["system"][2]["cache_control"], json!({"type":"ephemeral"}));
        assert_eq!(body["messages"][0]["content"], json!([{"type":"text","text":"question"}]));
    }

    #[test]
    fn claude_native_code_preserves_cache_controls_without_cloaking() {
        let acct = claude_account();
        *acct.cred.write() = Credential::OAuth(crate::accounts::OAuth {
            access_token: "test".into(),
            refresh_token: String::new(),
            expires_at: None,
            email: None,
            account_id: None,
            base_url: None,
            project_id: None,
            raw: Default::default(),
        });
        let mut headers = HeaderMap::new();
        headers.insert("user-agent", CC_USER_AGENT.parse().unwrap());
        let cfg = Config::default();
        let body = json!({
            "model":"claude-sonnet-4-6",
            "cache_control":{"type":"ephemeral","ttl":"1h"},
            "system":[{"type":"text","text":"instructions","cache_control":{"type":"ephemeral","ttl":"1h"}}],
            "messages":[{"role":"user","content":"question"}]
        });
        let prepared = prepare(
            &Target {
                acct: &acct,
                cfg: &cfg,
                client_headers: &headers,
                model: "claude-sonnet-4-6",
                wire: Format::Claude,
                passthrough: true,
                stream: false,
                count_tokens: false,
                cache_key: None,
            },
            body.clone(),
        );
        assert_eq!(prepared.body, body);
    }

    #[test]
    fn claude_cloak_adds_no_breakpoints_when_the_caller_marked_a_nested_block() {
        let acct = claude_account();
        let mut body = json!({"system":"instructions","messages":[
            {"role":"assistant","content":[{"type":"tool_use","id":"t","name":"lookup","input":{}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":[
                {"type":"text","text":"result","cache_control":{"type":"ephemeral"}}
            ]}]}
        ]});
        cloak_body(&mut body, &acct, None, false);
        assert!(body["cache_control"].is_null());
        assert!(body["system"][1]["cache_control"].is_null());
    }

    #[test]
    fn claude_cloak_keeps_instructions_without_a_user_turn() {
        let acct = claude_account();
        let mut body = json!({"system":[{"type":"text","text":"instructions","cache_control":{"type":"ephemeral","ttl":"1h"}}],"messages":[]});
        cloak_body(&mut body, &acct, None, false);
        assert_eq!(body["system"][2]["text"], "<system-reminder>\ninstructions\n</system-reminder>");
        assert_eq!(body["system"][2]["cache_control"], json!({"type":"ephemeral","ttl":"1h"}));
    }

    #[test]
    fn codex_bodies_follow_codex_field_order_and_keep_websocket_fields() {
        let mut body = json!({"input": [], "instructions": "", "prompt_cache_key": "k", "x-extra": 1, "stream": true,
            "model": "m", "type": "response.create", "store": false, "generate": false, "previous_response_id": "r"});
        sanitize_codex_body(&mut body, "gpt-6.1-sol", true);
        order_codex_body(&mut body);
        let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "type",
                "model",
                "stream",
                "previous_response_id",
                "input",
                "store",
                "prompt_cache_key",
                "generate",
                "x-extra"
            ]
        );
        let mut http = json!({"generate": false, "previous_response_id": "r", "input": []});
        sanitize_codex_body(&mut http, "gpt-6.1-sol", false);
        assert!(http.get("generate").is_none() && http.get("previous_response_id").is_none());
    }

    #[test]
    fn codex_instructions_are_developer_input_on_both_transports() {
        for websocket in [false, true] {
            let mut body = json!({"instructions": "Follow these rules.", "input": "question"});
            sanitize_codex_body(&mut body, "gpt-6.1-sol", websocket);
            assert!(body.get("instructions").is_none());
            assert_eq!(
                body["input"],
                json!([
                    {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "Follow these rules."}]},
                    {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "question"}]}
                ])
            );
            let once = body.clone();
            sanitize_codex_body(&mut body, "gpt-6.1-sol", websocket);
            assert_eq!(body, once, "sanitizing again must not duplicate instructions");
        }
    }

    #[test]
    fn codex_native_developer_input_does_not_gain_an_instructions_field() {
        let input = json!([
            {"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "native instructions"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "question"}]}
        ]);
        for websocket in [false, true] {
            for instructions in [None, Some(Value::Null), Some(json!(""))] {
                let mut body = json!({"input": input});
                if let Some(instructions) = instructions {
                    body["instructions"] = instructions;
                }
                sanitize_codex_body(&mut body, "gpt-6.1-sol", websocket);
                assert!(body.get("instructions").is_none());
                assert_eq!(body["input"], input);
            }
        }
    }

    #[test]
    fn codex_identity_is_the_clients_own_or_the_built_in_release() {
        let get = |h: &[(String, String)], k: &str| h.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let mut real = HeaderMap::new();
        real.insert("user-agent", "codex-tui/0.170.0 (Linux 6.12; x86_64) xterm (codex-tui; 0.170.0)".parse().unwrap());
        real.insert("originator", "codex-tui".parse().unwrap());
        real.insert("version", "0.170.0".parse().unwrap());
        let h = codex_headers(&real, "t", Some("acct"), true);
        assert_eq!(get(&h, "user-agent").unwrap(), "codex-tui/0.170.0 (Linux 6.12; x86_64) xterm (codex-tui; 0.170.0)");
        assert_eq!(get(&h, "version").unwrap(), "0.170.0");
        assert_eq!(get(&h, "chatgpt-account-id").unwrap(), "acct");
        let other = HeaderMap::new();
        let h = codex_headers(&other, "t", None, true);
        assert_eq!(get(&h, "user-agent").unwrap(), CODEX_USER_AGENT);
        assert_eq!(get(&h, "version").unwrap(), CODEX_VERSION);
        assert_eq!(get(&h, "originator").unwrap(), CODEX_ORIGINATOR);
        assert_eq!(h.iter().filter(|(k, _)| k == "user-agent").count(), 1);
    }

    #[test]
    fn codex_input_strings_become_lists() {
        let mut body = json!({ "input": "hi", "max_output_tokens": 5 });
        sanitize_codex_body(&mut body, "gpt-6-astra", false);
        assert_eq!(body["input"][0]["content"][0]["text"], "hi");
        assert!(body.get("max_output_tokens").is_none());
        assert_eq!(body["store"], false);
    }

    #[test]
    fn fingerprint_is_three_hex_chars() {
        let f = cc_fingerprint("hello world, this is a test");
        assert_eq!(f.len(), 3);
        assert!(f.chars().all(|c| c.is_ascii_hexdigit()));
        // Short messages fall back to '0' for missing positions.
        assert_eq!(cc_fingerprint(""), cc_fingerprint("abc"));
    }
}
