//! Devin (Windsurf / Codeium): `GetChatMessage` over Connect-RPC with
//! hand-rolled protobuf. The IR is turned into a list of prompts, and the
//! streamed frames back into IR events.

use std::collections::HashMap;
use std::io::Read;
use std::pin::Pin;

use anyhow::{Context, Result, anyhow, bail};
use bytes::{Buf, BytesMut};
use futures::{Stream, StreamExt};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::accounts::{Credential, OAuth};
use crate::device::Signed;
use crate::ir::{Event, Finish, Image, Part, Request, Role, Sig, Usage};
use crate::state::App;
use crate::upstream::{Prepared, Target};

pub const SERVER: &str = "https://server.codeium.com";
const CHAT_PATH: &str = "/exa.api_server_pb.ApiServerService/GetChatMessage";
const APP_BASE: &str = "https://app.devin.ai";
const API_BASE: &str = "https://api.devin.ai";
const CLIENT_NAME: &str = "chisel";
pub(crate) const CLIENT_VERSION: &str = "3000.10.21";
const FINGERPRINT_LEN: usize = 732;
const DEFAULT_MAX_TOKENS: u64 = 64_000;
const TOKEN_PREFIX: &str = "devin-session-token$";

/// Thinking levels each Devin model accepts (model UID = `<model>-<level>`).
const LEVELS: &[(&str, &[&str])] = &[
    ("claude-opus-5-5", &["low", "medium", "high", "xhigh", "max"]),
    ("claude-fable-5-1", &["low", "medium", "high", "xhigh", "max"]),
    ("claude-sonnet-5", &["low", "medium", "high", "xhigh", "max"]),
    ("claude-opus-5", &["low", "medium", "high", "xhigh", "max"]),
    ("claude-5-fable", &["low", "medium", "high", "xhigh", "max"]),
    ("claude-opus-4-8", &["low", "medium", "high", "xhigh", "max"]),
    ("claude-opus-4-7", &["low", "medium", "high", "xhigh", "max"]),
    ("gpt-6-astra", &["low", "medium", "high", "xhigh", "max"]),
    ("gpt-6-sol", &["none", "low", "medium", "high", "xhigh", "max"]),
    ("gpt-6-luna", &["none", "low", "medium", "high", "xhigh", "max"]),
    ("gpt-5-6-sol", &["none", "low", "medium", "high", "xhigh", "max"]),
    ("gpt-5-6-terra", &["none", "low", "medium", "high", "xhigh", "max"]),
    ("gpt-5-6-luna", &["none", "low", "medium", "high", "xhigh", "max"]),
    ("gpt-5-5", &["none", "low", "medium", "high", "xhigh"]),
    ("gpt-5-4", &["none", "low", "medium", "high", "xhigh"]),
    ("gpt-5-4-mini", &["low", "medium", "high", "xhigh"]),
    ("gpt-5-3-codex", &["low", "medium", "high", "xhigh"]),
    ("gemini-3-8-flash", &["low", "medium", "high"]),
    ("gemini-3-7-flash", &["low", "medium", "high"]),
    ("gemini-3-6-flash", &["minimal", "low", "medium", "high"]),
    ("gemini-3-5-flash", &["minimal", "low", "medium", "high"]),
    ("gemini-3-1-pro", &["low", "high"]),
    ("grok-4-7", &["low", "medium", "high", "xhigh"]),
    ("grok-4-6", &["low", "medium", "high", "xhigh"]),
    ("grok-4-5", &["low", "medium", "high"]),
    ("kimi-k3", &["low", "high", "max"]),
    ("glm-5-3", &["low", "high", "max"]),
    ("glm-5-3-flash", &["low", "high", "max"]),
    ("deepseek-v4-pro", &["high", "max"]),
    ("deepseek-v4-flash", &["high", "max"]),
    ("deepseek-v4-1-flash", &["high", "max"]),
    ("swe-2", &["medium", "high", "max"]),
    ("swe-1-7-lightning", &["medium"]),
    ("inkling", &["none", "low", "medium", "high", "xhigh", "max"]),
    ("nemotron-3-ultra", &["none", "medium", "high"]),
];

const ORDER: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max"];

fn has_effort_suffix(m: &str) -> bool {
    [
        "-none",
        "-low",
        "-medium",
        "-high",
        "-xhigh",
        "-max",
        "-fast",
        "-thinking",
        "-thinking-1m",
        "_low",
        "_medium",
        "_high",
        "_xhigh",
        "_minimal",
        "_none",
    ]
    .iter()
    .any(|s| m.ends_with(s))
}

fn clamp(requested: Option<&str>, allowed: &[&str], default: &str) -> String {
    let Some(req) = requested else { return default.into() };
    if let Some(a) = allowed.iter().find(|a| a.eq_ignore_ascii_case(req)) {
        return a.to_string();
    }
    let Some(ri) = ORDER.iter().position(|o| *o == req) else { return default.into() };
    allowed
        .iter()
        .filter_map(|a| ORDER.iter().position(|o| o == a).map(|i| (a, i)))
        .min_by_key(|(_, i)| (ri.abs_diff(*i), usize::MAX - i))
        .map(|(a, _)| a.to_string())
        .unwrap_or_else(|| default.into())
}

fn default_effort(model: &str, levels: &[&str]) -> String {
    let has = |l: &str| levels.contains(&l);
    if model.contains("swe-2") {
        return "high".into();
    }
    if has("none") && has("low") && model.starts_with("gpt-5") {
        return "low".into();
    }
    if has("high") && ["gemini", "grok", "glm", "deepseek", "kimi", "nemotron"].iter().any(|k| model.contains(k)) {
        return "high".into();
    }
    for l in ["medium", "high", "low"] {
        if has(l) {
            return l.into();
        }
    }
    levels.first().copied().unwrap_or("medium").into()
}

/// Maps a public model name plus requested effort to Devin's model UID.
pub fn model_uid(model: &str, effort: Option<&str>) -> String {
    let lower = model.trim().to_ascii_lowercase();
    if has_effort_suffix(&lower) || lower.starts_with("model_") {
        return model.trim().to_string();
    }
    let effort = effort.map(|e| match e {
        "off" | "disabled" => "none",
        "auto" | "adaptive" => "high",
        other => other,
    });
    let base = lower.replace('.', "-");
    let thinking = effort.is_some_and(|e| e != "none");
    match base.as_str() {
        "claude-haiku-4-5" => return "MODEL_PRIVATE_11".into(),
        "gpt-4-1" => return "MODEL_CHAT_GPT_4_1_2025_04_14".into(),
        b if b.contains("sonnet-4-5") => return if thinking { "MODEL_PRIVATE_3" } else { "MODEL_PRIVATE_2" }.into(),
        "claude-opus-4-6" | "claude-sonnet-4-6" => {
            return if thinking { format!("{base}-thinking") } else { base };
        }
        "swe-1-7" => return if effort == Some("medium") { "swe-1-7-medium" } else { "swe-1-7" }.into(),
        _ => {}
    }
    let base = if base == "gemini-3-flash" { "gemini-3-8-flash".to_string() } else { base };
    match LEVELS.iter().find(|(m, _)| *m == base) {
        Some((_, levels)) => {
            let def = default_effort(&base, levels);
            let level =
                if effort == Some("none") && !levels.contains(&"none") { def } else { clamp(effort, levels, &def) };
            format!("{base}-{level}")
        }
        None => base,
    }
}

// ------------------------------------------------------------------- protobuf

fn varint(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

fn tag(buf: &mut Vec<u8>, field: u32, wire: u8) {
    varint(buf, ((field as u64) << 3) | wire as u64);
}

fn put_bytes(buf: &mut Vec<u8>, field: u32, b: &[u8]) {
    tag(buf, field, 2);
    varint(buf, b.len() as u64);
    buf.extend_from_slice(b);
}

fn put_str(buf: &mut Vec<u8>, field: u32, s: &str) {
    put_bytes(buf, field, s.as_bytes());
}

fn put_uint(buf: &mut Vec<u8>, field: u32, v: u64) {
    tag(buf, field, 0);
    varint(buf, v);
}

fn put_f64(buf: &mut Vec<u8>, field: u32, v: f64) {
    tag(buf, field, 1);
    buf.extend_from_slice(&v.to_bits().to_le_bytes());
}

fn read_varint(b: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *b.get(*pos)?;
        *pos += 1;
        v |= ((byte & 0x7f) as u64) << shift;
        if byte < 0x80 {
            return Some(v);
        }
    }
    None
}

enum Field<'a> {
    Int(u64),
    Bytes(&'a [u8]),
    Fixed64,
}

/// Iterates the top-level fields of a protobuf message.
fn fields(b: &[u8]) -> Vec<(u32, Field<'_>)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < b.len() {
        let Some(key) = read_varint(b, &mut pos) else { break };
        let (num, wire) = ((key >> 3) as u32, (key & 7) as u8);
        match wire {
            0 => match read_varint(b, &mut pos) {
                Some(v) => out.push((num, Field::Int(v))),
                None => break,
            },
            1 => {
                let Some(chunk) = b.get(pos..pos + 8) else { break };
                let _ = chunk;
                out.push((num, Field::Fixed64));
                pos += 8;
            }
            2 => {
                let Some(len) = read_varint(b, &mut pos) else { break };
                let Some(chunk) = b.get(pos..pos + len as usize) else { break };
                out.push((num, Field::Bytes(chunk)));
                pos += len as usize;
            }
            5 => pos += 4,
            _ => break,
        }
    }
    out
}

fn fingerprint(seed: &str) -> String {
    let mut out = String::new();
    let mut i = 0;
    while out.len() < FINGERPRINT_LEN {
        out.push_str(&hex::encode(Sha256::digest(format!("{seed}-{i}").as_bytes())));
        i += 1;
    }
    out.truncate(FINGERPRINT_LEN);
    out
}

fn os_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

fn metadata(token: &str, seed: &str) -> Vec<u8> {
    let mut m = Vec::new();
    put_str(&mut m, 1, CLIENT_NAME);
    put_str(&mut m, 2, CLIENT_VERSION);
    put_str(&mut m, 3, token);
    put_str(&mut m, 4, "en");
    put_str(&mut m, 5, os_name());
    put_str(&mut m, 7, CLIENT_VERSION);
    put_str(&mut m, 12, CLIENT_NAME);
    put_str(&mut m, 31, &fingerprint(seed));
    m
}

/// Lines of other agents' system prompts Devin's backend refuses.
fn sanitize_system(prompt: &str) -> String {
    prompt
        .replace("\r\n", "\n")
        .lines()
        .filter(|l| {
            let t = l.trim();
            !(t.starts_with("You are Claude Code")
                || t.starts_with("x-anthropic-billing-header")
                || t.contains("authorized security testing")
                || t.contains("destructive techniques, DoS attacks")
                || t.contains("Claude Code is available as a CLI")
                || t.contains("Fast mode for Claude Code"))
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn split_sig(s: &str) -> (Vec<u8>, String) {
    match s.split_once(':') {
        Some((kind, sig)) if ["anthropic", "openai", "gemini", "sealed"].contains(&kind) => {
            (sig.as_bytes().to_vec(), kind.to_string())
        }
        _ => (s.as_bytes().to_vec(), "sealed".into()),
    }
}

/// IR -> an intermediate JSON description; `prepare` encodes it with the account's token.
pub fn build_request(req: &Request, model: &str) -> Value {
    let effort = req.reasoning.as_ref().and_then(|r| r.effort_level());
    let mut prompts: Vec<Value> = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    for m in &req.messages {
        match m.role {
            Role::User => {
                let mut text = Vec::new();
                let mut images = Vec::new();
                for p in &m.parts {
                    match p {
                        Part::Text(t) => text.push(t.clone()),
                        Part::Image(Image::Base64 { mime, data }) => images.push(json!([data, mime])),
                        Part::Image(Image::Url(u)) => text.push(format!("[image: {u}]")),
                        Part::ToolResult { id, content, .. } => {
                            let body = crate::ir::parts_text(content);
                            let imgs: Vec<Value> = content
                                .iter()
                                .filter_map(|c| match c {
                                    Part::Image(Image::Base64 { mime, data }) => Some(json!([data, mime])),
                                    _ => None,
                                })
                                .collect();
                            if let Some(i) = pending.iter().position(|p| p == id) {
                                pending.remove(i);
                                prompts
                                    .push(json!({ "source": 4, "content": body, "tool_call_id": id, "images": imgs }));
                            } else {
                                // A result without its call would be rejected; keep it as user text.
                                prompts.push(json!({ "source": 1, "content": format!("Tool result ({id}):\n{body}"), "images": imgs }));
                            }
                        }
                        _ => {}
                    }
                }
                if !text.is_empty() || !images.is_empty() {
                    prompts.push(json!({ "source": 1, "content": text.join("\n"), "images": images }));
                }
            }
            Role::Assistant => {
                let mut content = String::new();
                let mut thinking = String::new();
                let mut sig = None;
                let mut calls = Vec::new();
                for p in &m.parts {
                    match p {
                        Part::Text(t) => content.push_str(t),
                        Part::Reasoning { text, sig: s } => {
                            thinking.push_str(text);
                            if let Some(Sig::Devin(d)) = s {
                                sig = Some(d.clone());
                            }
                        }
                        Part::ToolCall { id, name, args, .. } => {
                            pending.push(id.clone());
                            calls.push(json!([id, name, if args.is_empty() { "{}" } else { args }]));
                        }
                        _ => {}
                    }
                }
                prompts.push(
                    json!({ "source": 2, "content": content, "thinking": thinking, "sig": sig, "tool_calls": calls }),
                );
            }
        }
    }
    let tools: Vec<Value> =
        req.tools.iter().map(|t| json!([t.name, t.description, t.parameters.to_string()])).collect();
    json!({
        "model_uid": model_uid(model, effort.as_deref()),
        "system": sanitize_system(&req.system.join("\n\n")),
        "prompts": prompts,
        "tools": tools,
        "temperature": req.temperature,
        "max_tokens": req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS).min(DEFAULT_MAX_TOKENS),
    })
}

fn encode(spec: &Value, token: &str, seed: &str) -> Vec<u8> {
    let mut r = Vec::new();
    put_bytes(&mut r, 1, &metadata(token, seed));
    if let Some(sys) = spec["system"].as_str().filter(|s| !s.is_empty()) {
        put_str(&mut r, 2, sys);
    }
    let prompts = spec["prompts"].as_array().cloned().unwrap_or_default();
    for p in &prompts {
        let mut b = Vec::new();
        put_str(&mut b, 1, &uuid::Uuid::new_v4().to_string());
        put_uint(&mut b, 2, p["source"].as_u64().unwrap_or(1));
        put_str(&mut b, 3, p["content"].as_str().unwrap_or_default());
        for c in p["tool_calls"].as_array().into_iter().flatten() {
            let mut tc = Vec::new();
            for (i, f) in [1u32, 2, 3].iter().enumerate() {
                if let Some(s) = c[i].as_str().filter(|s| !s.is_empty()) {
                    put_str(&mut tc, *f, s);
                }
            }
            put_bytes(&mut b, 6, &tc);
        }
        if let Some(id) = p["tool_call_id"].as_str() {
            put_str(&mut b, 7, id);
        }
        for img in p["images"].as_array().into_iter().flatten() {
            let mut ib = Vec::new();
            put_str(&mut ib, 1, img[0].as_str().unwrap_or_default());
            put_str(&mut ib, 2, img[1].as_str().unwrap_or("image/png"));
            put_bytes(&mut b, 10, &ib);
        }
        if let Some(t) = p["thinking"].as_str().filter(|t| !t.is_empty()) {
            put_str(&mut b, 11, t);
        }
        if let Some(s) = p["sig"].as_str() {
            let (bytes, kind) = split_sig(s);
            put_bytes(&mut b, 12, &bytes);
            put_str(&mut b, 18, &kind);
        }
        put_bytes(&mut r, 3, &b);
    }
    put_uint(&mut r, 7, 5);
    let mut cfg = Vec::new();
    put_uint(&mut cfg, 1, 1);
    put_uint(&mut cfg, 2, spec["max_tokens"].as_u64().unwrap_or(DEFAULT_MAX_TOKENS));
    put_uint(&mut cfg, 3, 400);
    put_f64(&mut cfg, 5, spec["temperature"].as_f64().unwrap_or(1.0));
    put_uint(&mut cfg, 7, 40);
    put_f64(&mut cfg, 8, 0.95f32 as f64);
    put_bytes(&mut r, 8, &cfg);
    for t in spec["tools"].as_array().into_iter().flatten() {
        let mut tb = Vec::new();
        put_str(&mut tb, 1, t[0].as_str().unwrap_or_default());
        if let Some(d) = t[1].as_str().filter(|d| !d.is_empty()) {
            put_str(&mut tb, 2, d);
        }
        put_str(&mut tb, 3, t[2].as_str().unwrap_or("{}"));
        put_bytes(&mut r, 10, &tb);
    }
    // Every request is a fresh session (turn 0); the full history is in the prompts.
    let session = uuid::Uuid::new_v4().to_string();
    let mut s = Vec::new();
    put_str(&mut s, 1, &session);
    put_uint(&mut s, 3, 4);
    if prompts.last().is_some_and(|p| p["source"] == 1) {
        put_uint(&mut s, 4, 14);
    }
    put_bytes(&mut r, 15, &s);
    put_str(&mut r, 16, &session);
    put_uint(&mut r, 20, 1);
    put_str(&mut r, 21, spec["model_uid"].as_str().unwrap_or("swe-2-high"));
    r
}

fn frame(flag: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 5);
    out.push(flag);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

fn sentry_trace() -> String {
    let b: [u8; 24] = rand::random();
    format!("{}-{}-1", hex::encode(&b[..16]), hex::encode(&b[16..]))
}

pub fn prepare(t: &Target, body: Value) -> Prepared {
    let (token, base, seed) = match &*t.acct.cred.read() {
        Credential::OAuth(o) => (
            o.access_token.clone(),
            o.base_url.clone(),
            o.field("device_seed").map(String::from).unwrap_or_else(|| t.acct.device_id.clone()),
        ),
        Credential::ApiKey { key, base_url } => (key.clone(), base_url.clone(), t.acct.device_id.clone()),
    };
    let base = base.filter(|b| !b.trim().is_empty()).unwrap_or_else(|| SERVER.into());
    let raw = frame(0, &encode(&body, &token, &seed));
    Prepared {
        url: format!("{}{CHAT_PATH}", base.trim_end_matches('/')),
        headers: vec![
            ("authorization".into(), format!("Basic {token}-{token}")),
            ("content-type".into(), "application/connect+proto".into()),
            ("connect-protocol-version".into(), "1".into()),
            ("accept".into(), "*/*".into()),
            ("sentry-trace".into(), sentry_trace()),
        ],
        body,
        raw: Some(raw),
    }
}

// --------------------------------------------------------------------- stream

/// Connect end-of-stream trailer: `{"error": {"code": "...", "message": "..."}}`.
pub fn trailer_error(payload: &[u8]) -> Option<(u16, String)> {
    let v: Value = serde_json::from_slice(payload).ok()?;
    let e = v.get("error")?;
    let code = e["code"].as_str().unwrap_or_default().to_ascii_lowercase();
    let msg = e["message"].as_str().unwrap_or("upstream error").to_string();
    let lower = msg.to_ascii_lowercase();
    let status = match code.as_str() {
        "invalid_argument" if !lower.contains("internal error") => 400,
        "unauthenticated" => 401,
        "permission_denied" if lower.contains("high demand") => 429,
        "permission_denied" => 403,
        "resource_exhausted" => 429,
        "failed_precondition" if lower.contains("quota") || lower.contains("credit") => 429,
        "unavailable" => 503,
        "deadline_exceeded" => 504,
        "not_found" => 404,
        _ => 502,
    };
    Some((status, msg))
}

#[derive(Default)]
struct Decoder {
    started: bool,
    tools: Vec<String>,
    current_tool: Option<usize>,
    sig: Vec<u8>,
    sig_kind: String,
    sig_sent: bool,
    thinking: bool,
    usage: Usage,
    stop: u64,
}

impl Decoder {
    fn flush_sig(&mut self, out: &mut Vec<Event>) {
        if self.thinking && !self.sig_sent && !self.sig.is_empty() {
            let kind = if self.sig_kind.is_empty() { "sealed" } else { &self.sig_kind };
            out.push(Event::ReasoningSig(Sig::Devin(format!("{kind}:{}", String::from_utf8_lossy(&self.sig)))));
            self.sig_sent = true;
        }
        self.thinking = false;
    }

    fn message(&mut self, payload: &[u8], names: &HashMap<String, String>, out: &mut Vec<Event>) {
        if !self.started {
            self.started = true;
            out.push(Event::Start { id: None, model: None });
        }
        let mut text = String::new();
        let mut thought = String::new();
        let mut calls: Vec<(String, String, String)> = Vec::new();
        for (num, f) in fields(payload) {
            match (num, f) {
                (3, Field::Bytes(b)) => text.push_str(&String::from_utf8_lossy(b)),
                (9, Field::Bytes(b)) => thought.push_str(&String::from_utf8_lossy(b)),
                (10, Field::Bytes(b)) => self.sig.extend_from_slice(b),
                (21, Field::Bytes(b)) => self.sig_kind = String::from_utf8_lossy(b).into_owned(),
                (5, Field::Int(v)) if v != 0 => self.stop = v,
                (6, Field::Bytes(b)) => {
                    let (mut id, mut name, mut args) = (String::new(), String::new(), String::new());
                    for (n, f) in fields(b) {
                        if let Field::Bytes(v) = f {
                            let s = String::from_utf8_lossy(v).into_owned();
                            match n {
                                1 => id = s,
                                2 => name = s,
                                3 => args = s,
                                4 if args.is_empty() => args = s,
                                _ => {}
                            }
                        }
                    }
                    calls.push((id, name, args));
                }
                (7, Field::Bytes(b)) => {
                    let mut u = Usage::default();
                    for (n, f) in fields(b) {
                        if let Field::Int(v) = f {
                            match n {
                                2 => u.input += v,
                                3 => u.output = v,
                                4 => u.cache_write += v,
                                5 => u.cache_read = v,
                                _ => {}
                            }
                        }
                    }
                    self.usage.merge(&u);
                }
                _ => {}
            }
        }
        if !thought.is_empty() {
            self.thinking = true;
            out.push(Event::Reasoning(thought));
        }
        for (id, name, args) in calls {
            self.flush_sig(out);
            let known = if id.is_empty() { self.current_tool } else { self.tools.iter().position(|t| *t == id) };
            let key = match known {
                Some(k) => k,
                None => {
                    let key = self.tools.len();
                    let id = if id.is_empty() { crate::ir::new_id("call_") } else { id };
                    self.tools.push(id.clone());
                    let name = names.get(&name).cloned().unwrap_or(name);
                    out.push(Event::ToolStart { key, id, name });
                    key
                }
            };
            self.current_tool = Some(key);
            if !args.is_empty() {
                out.push(Event::ToolArgs { key, delta: args });
            }
        }
        if !text.is_empty() {
            self.flush_sig(out);
            out.push(Event::Text(text));
        }
    }

    fn end(&mut self, out: &mut Vec<Event>) {
        self.flush_sig(out);
        if self.usage != Usage::default() {
            out.push(Event::Usage(self.usage.clone()));
        }
        out.push(Event::Finish(match self.stop {
            1 | 3 => Finish::Length,
            11 => Finish::Filter,
            _ if !self.tools.is_empty() => Finish::ToolCalls,
            _ => Finish::Stop,
        }));
    }
}

fn gunzip(b: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(b).take(64 << 20).read_to_end(&mut out).ok()?;
    Some(out)
}

pub fn event_stream(
    resp: reqwest::Response,
    names: HashMap<String, String>,
) -> Pin<Box<dyn Stream<Item = Event> + Send>> {
    Box::pin(async_stream::stream! {
        let mut body = resp.bytes_stream();
        let mut buf = BytesMut::new();
        let mut dec = Decoder::default();
        let mut out = Vec::new();
        let mut ended = false;
        'read: while let Some(chunk) = body.next().await {
            match chunk {
                Ok(c) => buf.extend_from_slice(&c),
                Err(e) => {
                    out.push(Event::Error { status: 502, message: format!("upstream stream error: {e}") });
                    break;
                }
            }
            while buf.len() >= 5 {
                let flag = buf[0];
                let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
                if buf.len() < 5 + len {
                    break;
                }
                buf.advance(5);
                let raw = buf.split_to(len);
                let payload = if flag & 1 != 0 { gunzip(&raw).unwrap_or_default() } else { raw.to_vec() };
                if flag & 2 != 0 {
                    match trailer_error(&payload) {
                        Some((status, message)) => out.push(Event::Error { status, message }),
                        None => dec.end(&mut out),
                    }
                    ended = true;
                    for ev in out.drain(..) { yield ev; }
                    break 'read;
                }
                dec.message(&payload, &names, &mut out);
            }
            for ev in out.drain(..) { yield ev; }
        }
        if !ended && out.iter().all(|e| !matches!(e, Event::Error { .. })) {
            out.push(Event::Error { status: 502, message: "Devin stream ended before its trailer".into() });
        }
        for ev in out.drain(..) { yield ev; }
    })
}

// ---------------------------------------------------------------------- login

/// Starts the PKCE sign-in: returns the URL and the verifier.
pub fn auth_url(redirect: &str, state: &str, challenge: &str) -> String {
    let q = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs([
            ("redirect_uri", redirect),
            ("state", state),
            ("prompt", "select_account"),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
        ])
        .finish();
    format!("{APP_BASE}/auth/cli/continue?{q}")
}

fn session_token(raw: &str) -> String {
    let t = raw.trim();
    if t.starts_with("eyJ") { format!("{TOKEN_PREFIX}{t}") } else { t.to_string() }
}

/// Finishes sign-in from an authorization code (or a pasted session token).
pub async fn complete_login(app: &App, input: &str, verifier: &str) -> Result<Signed> {
    let http = app.http.client(None);
    let input = input.trim();
    let token = if input.starts_with(TOKEN_PREFIX) || input.starts_with("eyJ") {
        session_token(input)
    } else {
        let resp = http
            .post(format!("{API_BASE}/auth/cli/token"))
            .header("accept", "application/json")
            .json(&json!({ "code": input, "code_verifier": verifier }))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("Devin token exchange returned {status}: {}", text.chars().take(300).collect::<String>());
        }
        let v: Value = serde_json::from_str(&text).context("invalid token response")?;
        session_token(v["token"].as_str().ok_or_else(|| anyhow!("no token in response"))?)
    };
    let me: Value = match http.get(format!("{API_BASE}/v3/self")).bearer_auth(&token).send().await {
        Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
        _ => Value::Null,
    };
    let s = |k: &str| me[k].as_str().filter(|s| !s.is_empty()).map(String::from);
    let who = s("user_name")
        .or_else(|| s("user_id"))
        .unwrap_or_else(|| format!("user-{}", &hex::encode(Sha256::digest(token.as_bytes()))[..16]));
    let mut raw = Map::new();
    for k in ["user_name", "user_id", "org_id"] {
        if let Some(v) = s(k) {
            raw.insert(k.into(), v.into());
        }
    }
    let oauth =
        OAuth { access_token: token, email: s("email").or(Some(who.clone())), raw: raw.clone(), ..Default::default() };
    let mut extra: Vec<(&'static str, Value)> = vec![("auth_kind", "oauth".into())];
    let seed: [u8; 16] = rand::random();
    extra.push(("device_seed", hex::encode(seed).into()));
    for (k, v) in raw {
        let key: &'static str = match k.as_str() {
            "user_name" => "user_name",
            "user_id" => "user_id",
            _ => "org_id",
        };
        extra.push((key, v));
    }
    Ok(Signed { oauth, file: format!("devin-{}.json", crate::oauth::file_safe(&who)), extra })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_uids_follow_devin_levels() {
        assert_eq!(model_uid("claude-opus-5-5", None), "claude-opus-5-5-medium");
        assert_eq!(model_uid("claude-opus-5-5", Some("max")), "claude-opus-5-5-max");
        assert_eq!(model_uid("gpt-6-sol", Some("none")), "gpt-6-sol-none");
        assert_eq!(model_uid("gpt-6-astra", Some("none")), "gpt-6-astra-medium");
        assert_eq!(model_uid("kimi-k3", Some("medium")), "kimi-k3-high");
        assert_eq!(model_uid("gemini-3.8-flash", None), "gemini-3-8-flash-high");
        assert_eq!(model_uid("claude-sonnet-4-6", Some("high")), "claude-sonnet-4-6-thinking");
        assert_eq!(model_uid("swe-2", None), "swe-2-high");
        assert_eq!(model_uid("swe-2-max", None), "swe-2-max");
    }

    #[test]
    fn request_round_trips_through_the_wire_format() {
        let req = Request {
            system: vec!["You are Claude Code, Anthropic's official CLI for Claude.\nBe brief.".into()],
            messages: vec![
                crate::ir::Message { role: Role::User, parts: vec![Part::Text("hi".into())] },
                crate::ir::Message {
                    role: Role::Assistant,
                    parts: vec![Part::ToolCall { id: "c1".into(), name: "f".into(), args: "{}".into(), sig: None }],
                },
                crate::ir::Message {
                    role: Role::User,
                    parts: vec![Part::ToolResult {
                        id: "c1".into(),
                        name: None,
                        content: vec![Part::Text("ok".into())],
                        is_error: false,
                    }],
                },
            ],
            ..Default::default()
        };
        let spec = build_request(&req, "swe-2");
        assert_eq!(spec["system"], "Be brief.");
        let bytes = encode(&spec, "tok", "seed");
        let top = fields(&bytes);
        let prompts: Vec<_> = top.iter().filter(|(n, _)| *n == 3).collect();
        assert_eq!(prompts.len(), 3);
        let Field::Bytes(last) = &prompts[2].1 else { panic!() };
        let f = fields(last);
        assert!(f.iter().any(|(n, v)| *n == 2 && matches!(v, Field::Int(4))));
        assert!(f.iter().any(|(n, v)| *n == 7 && matches!(v, Field::Bytes(b) if *b == b"c1")));
        assert!(top.iter().any(|(n, v)| *n == 21 && matches!(v, Field::Bytes(b) if *b == b"swe-2-high")));
    }

    #[test]
    fn frames_decode_to_events() {
        let mut msg = Vec::new();
        put_str(&mut msg, 9, "hmm");
        put_bytes(&mut msg, 10, b"SIG");
        put_str(&mut msg, 21, "anthropic");
        let mut tc = Vec::new();
        put_str(&mut tc, 1, "c1");
        put_str(&mut tc, 2, "f");
        put_str(&mut tc, 3, "{\"a\":1}");
        put_bytes(&mut msg, 6, &tc);
        let mut usage = Vec::new();
        put_uint(&mut usage, 2, 10);
        put_uint(&mut usage, 3, 5);
        put_bytes(&mut msg, 7, &usage);
        let mut dec = Decoder::default();
        let mut out = Vec::new();
        dec.message(&msg, &HashMap::new(), &mut out);
        dec.end(&mut out);
        let mut agg = crate::ir::Aggregate::default();
        out.iter().for_each(|e| agg.push(e));
        assert_eq!(agg.reasoning_text(), "hmm");
        assert!(matches!(&agg.parts[0], Part::Reasoning { sig: Some(Sig::Devin(s)), .. } if s == "anthropic:SIG"));
        assert!(matches!(&agg.parts[1], Part::ToolCall { args, .. } if args == "{\"a\":1}"));
        assert_eq!((agg.usage.input, agg.usage.output), (10, 5));
        assert_eq!(agg.finish_reason(), Finish::ToolCalls);
        assert_eq!(trailer_error(br#"{"error":{"code":"resource_exhausted","message":"slow down"}}"#).unwrap().0, 429);
        assert!(trailer_error(b"{}").is_none());
    }
}
