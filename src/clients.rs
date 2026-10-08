//! The Claude Code and Codex releases Fusebox speaks as.
//!
//! Requests Fusebox builds for other clients (translated requests, cloaking) match one
//! release of each: the constants in `upstream`, listed in `client-versions.json` and
//! compared with the latest releases by the daily CI check. Real clients passing
//! through keep their own identity. Fusebox's own small requests (usage, model lists,
//! limit resets) claim the newest release seen from the user's own clients, so a
//! model list matches what those clients are offered.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use axum::http::HeaderMap;
use parking_lot::RwLock;

use crate::upstream::{CC_USER_AGENT, CC_VERSION, CODEX_USER_AGENT, CODEX_VERSION};

pub const CLAUDE_CODE: &str = "claude-code";
pub const CODEX: &str = "codex";

/// Newest version seen per client, never older than the built-in one.
static SEEN: LazyLock<RwLock<BTreeMap<&'static str, String>>> = LazyLock::new(Default::default);

fn parse(v: &str) -> Option<(u32, u32, u32)> {
    let mut it = v.trim().splitn(3, '.');
    let n = |s: Option<&str>| s?.parse::<u32>().ok();
    let (a, b) = (n(it.next())?, n(it.next())?);
    // `0.161.0`, but not `0.161.0-alpha.2`: pre-releases are nobody's default.
    let c = n(it.next())?;
    Some((a, b, c))
}

/// Newer than the built-in release and close enough to it to be real: same major
/// version, at most 20 minor versions ahead.
fn plausible(builtin: &str, v: &str) -> bool {
    match (parse(builtin), parse(v)) {
        (Some(b), Some(v)) => v > b && v.0 == b.0 && v.1 <= b.1 + 20,
        _ => false,
    }
}

fn builtin(client: &str) -> &'static str {
    if client == CODEX { CODEX_VERSION } else { CC_VERSION }
}

/// The version of a request's client, when it is Claude Code or Codex.
fn version_of(headers: &HeaderMap) -> Option<(&'static str, String)> {
    let get = |n: &str| headers.get(n).and_then(|v| v.to_str().ok()).unwrap_or_default();
    let agent = get("user-agent");
    let (name, rest) = agent.split_once('/')?;
    let version = rest.split([' ', '(', ';']).next().unwrap_or_default().to_string();
    match name.to_ascii_lowercase().as_str() {
        "claude-cli" => Some((CLAUDE_CODE, version)),
        n if n.starts_with("codex") && get("originator").starts_with("codex") => Some((CODEX, version)),
        _ => None,
    }
}

/// Remembers a newer Claude Code or Codex release from a client's request.
/// True when that changed what Fusebox claims (worth saving).
pub fn observe(headers: &HeaderMap) -> bool {
    let Some((client, version)) = version_of(headers) else { return false };
    if !plausible(builtin(client), &version) {
        return false;
    }
    if SEEN.read().get(client).is_some_and(|seen| parse(seen) >= parse(&version)) {
        return false;
    }
    tracing::info!("{client} {version} seen; Fusebox's own requests now say so");
    SEEN.write().insert(client, version);
    true
}

/// What to save, so a restart keeps claiming the newest release.
pub fn seen() -> BTreeMap<String, String> {
    SEEN.read().iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
}

pub fn restore(saved: &BTreeMap<String, String>) {
    let mut seen = SEEN.write();
    for client in [CLAUDE_CODE, CODEX] {
        if let Some(v) = saved.get(client).filter(|v| plausible(builtin(client), v))
            && seen.get(client).is_none_or(|old| parse(old) < parse(v))
        {
            seen.insert(client, v.clone());
        }
    }
}

fn current(client: &'static str) -> String {
    SEEN.read().get(client).cloned().unwrap_or_else(|| builtin(client).to_string())
}

/// For Fusebox's own requests to Anthropic.
pub fn claude_user_agent() -> String {
    CC_USER_AGENT.replace(CC_VERSION, &current(CLAUDE_CODE))
}

/// For Fusebox's own requests to the Codex backend: `client_version` and `version`.
pub fn codex_version() -> String {
    current(CODEX)
}

pub fn codex_user_agent() -> String {
    CODEX_USER_AGENT.replace(CODEX_VERSION, &current(CODEX))
}

/// A real Codex client: its own user agent and originator go upstream unchanged.
pub fn is_codex(headers: &HeaderMap) -> bool {
    matches!(version_of(headers), Some((CODEX, _)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `.github/client-versions.json` is what the daily check compares; the code must agree.
    #[test]
    fn the_version_list_matches_the_code() {
        let list: serde_json::Value = serde_json::from_str(include_str!("../.github/client-versions.json")).unwrap();
        let mut ids = Vec::new();
        for client in list["clients"].as_array().unwrap() {
            let (id, v) = (client["id"].as_str().unwrap(), client["version"].as_str().unwrap());
            ids.push(id);
            let in_code = match id {
                "claude-code" => CC_VERSION == v && CC_USER_AGENT.contains(&format!("/{v} ")),
                "codex" => CODEX_VERSION == v && CODEX_USER_AGENT.matches(v).count() == 2,
                "grok-build" => crate::device::xai::CLIENT_VERSION == v,
                "muse-code" => crate::device::meta::API_UA.starts_with(&format!("muse-build/{v} ")),
                "devin-cli" => crate::devin::CLIENT_VERSION == v,
                "antigravity" => crate::antigravity::FALLBACK_VERSION == v,
                other => panic!("{other} has no check here"),
            };
            assert!(in_code, "{id} {v} in client-versions.json differs from the code");
        }
        assert_eq!(ids.len(), 6);
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn clients_are_recognised_by_their_own_headers() {
        let cc = headers(&[("user-agent", "claude-cli/2.9.1 (external, cli)")]);
        assert_eq!(version_of(&cc), Some((CLAUDE_CODE, "2.9.1".into())));
        let codex = headers(&[
            ("user-agent", "codex_cli_rs/0.170.2 (Mac OS 26.5.2; arm64) iTerm.app/3.6.11"),
            ("originator", "codex_cli_rs"),
        ]);
        assert_eq!(version_of(&codex), Some((CODEX, "0.170.2".into())));
        assert!(is_codex(&codex));
        // A Codex-looking agent without Codex's originator is some other program.
        assert!(!is_codex(&headers(&[("user-agent", "codex_cli_rs/0.170.2")])));
        assert_eq!(version_of(&headers(&[("user-agent", "curl/8.9.0")])), None);
    }

    #[test]
    fn only_plausible_newer_releases_count() {
        let (major, minor, _) = parse(CODEX_VERSION).unwrap();
        assert!(plausible(CODEX_VERSION, &format!("{major}.{}.0", minor + 1)));
        assert!(plausible(CODEX_VERSION, &format!("{major}.{}.3", minor + 20)));
        for no in [
            CODEX_VERSION.to_string(),
            format!("{major}.{}.0", minor - 1),
            format!("{major}.{}.0", minor + 21),
            format!("{}.0.0", major + 1),
            format!("{major}.{}.0-alpha.1", minor + 1),
            "garbage".into(),
        ] {
            assert!(!plausible(CODEX_VERSION, &no), "{no}");
        }
    }

    #[test]
    fn the_newest_client_release_is_claimed_for_fusebox_requests() {
        let (major, minor, _) = parse(CC_VERSION).unwrap();
        let newer = format!("{major}.{minor}.9999");
        assert!(!observe(&headers(&[("user-agent", &format!("claude-cli/{major}.{}.0 (x)", minor + 21))])));
        assert!(observe(&headers(&[("user-agent", &format!("claude-cli/{newer} (external, cli)"))])));
        assert!(!observe(&headers(&[("user-agent", &format!("claude-cli/{newer} (external, cli)"))])), "already known");
        assert_eq!(claude_user_agent(), CC_USER_AGENT.replace(CC_VERSION, &newer));
        assert_eq!(seen().get(CLAUDE_CODE), Some(&newer));
        restore(&BTreeMap::from([(CLAUDE_CODE.to_string(), CC_VERSION.to_string())]));
        assert_eq!(seen().get(CLAUDE_CODE), Some(&newer), "an older saved release changes nothing");
    }
}
