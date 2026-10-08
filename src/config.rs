use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use serde_yaml::Value as Yaml;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct Config {
    /// Interface to bind. Use 0.0.0.0 to expose on the network (set api-keys first).
    pub host: String,
    pub port: u16,
    /// Directory holding OAuth credential files. Compatible with CLIProxyAPI.
    pub auth_dir: String,
    /// Keys clients must send (Authorization: Bearer, x-api-key or x-goog-api-key).
    /// Empty means no client authentication.
    pub api_keys: Vec<String>,
    pub named_clients: Vec<NamedClient>,
    pub usage: crate::usage::types::UsageConfig,
    /// Protects the dashboard and management API. Empty means localhost-only access.
    /// A bcrypt hash (as CLIProxyAPI stores it) works too.
    pub management_key: String,
    /// With a management key set, allow the dashboard from other machines (default true).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub management_allow_remote: Option<bool>,
    /// Optional upstream proxy (http://, https://, socks5://).
    pub proxy_url: String,
    /// How many different accounts to try before giving up on a request.
    #[serde(deserialize_with = "lenient_u32")]
    pub request_retry: u32,
    /// How new sessions choose an account.
    pub routing: Routing,
    /// Smart routing reserves this 5-hour remaining percentage for active/recent sessions.
    #[serde(deserialize_with = "percentage")]
    pub five_hour_reserve_percent: u8,
    /// Keep each coding session on its account until subscription quota is exhausted.
    pub session_affinity: bool,
    /// Forget inactive sessions after this many seconds (default one day).
    pub session_affinity_idle_seconds: u64,
    /// Keep an upstream websocket open to Codex when clients connect over websocket.
    pub codex_websockets: bool,
    /// Rewrite non-Claude-Code requests on Claude OAuth accounts so they look like Claude Code.
    pub claude_cloak: bool,
    /// Check Claude and ChatGPT subscriptions for banked rate-limit resets and allow
    /// spending them from the dashboard. Off by default: it uses unofficial endpoints.
    pub banked_resets: bool,
    pub debug: bool,
    /// Serve HTTPS with this certificate.
    #[serde(skip_serializing_if = "Tls::is_off")]
    pub tls: Tls,
    /// Which faults send a push notification to the devices that turned them on.
    #[serde(skip_serializing_if = "Notifications::is_default")]
    pub notifications: Notifications,
    /// Only route unprefixed model names to accounts without a `prefix`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub force_model_prefix: bool,
    /// Per-provider renames for OAuth accounts (`claude: [{name, alias, fork}]`).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub oauth_model_alias: BTreeMap<String, Vec<OAuthAlias>>,
    /// Per-provider model patterns OAuth accounts must not serve (`*` wildcards).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub oauth_excluded_models: BTreeMap<String, Vec<String>>,
    /// Models added from the dashboard before Fusebox knows them (`claude: [claude-opus-6]`).
    /// Each is dropped once a release lists it or the provider's model list includes it.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_models: BTreeMap<String, Vec<String>>,
    /// CLIProxyAPI settings found in the file that have no effect here.
    #[serde(skip)]
    pub ignored: Vec<String>,
    /// The file names no auth-dir and only the pre-Fusebox directory exists, so it is used.
    #[serde(skip)]
    pub legacy_auth_dir: bool,
    pub claude_api_key: Vec<KeyEntry>,
    pub codex_api_key: Vec<KeyEntry>,
    pub gemini_api_key: Vec<KeyEntry>,
    /// Vertex AI express-mode API keys (service accounts go in the auth dir).
    pub vertex_api_key: Vec<KeyEntry>,
    /// Kimi Code (api.kimi.com/coding) or Moonshot platform keys.
    pub kimi_api_key: Vec<KeyEntry>,
    pub xai_api_key: Vec<KeyEntry>,
    pub meta_api_key: Vec<KeyEntry>,
    pub openai_compatibility: Vec<CompatEntry>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Routing {
    /// The account with the most subscription quota left (falls back to round-robin).
    #[default]
    LeastUsed,
    /// Drain the earliest eligible weekly renewal; quota and load break close ties.
    SmartQuota,
    RoundRobin,
    FillFirst,
}

/// Accepts `routing: fill-first` and CLIProxyAPI's `routing: { strategy: fill-first }`.
/// Strategies this crate doesn't have (weighted-round-robin) fall back to round-robin.
impl<'de> Deserialize<'de> for Routing {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Yaml::deserialize(d)?;
        let s = match &v {
            Yaml::String(s) => s.as_str(),
            Yaml::Mapping(m) => m.get("strategy").and_then(Yaml::as_str).unwrap_or_default(),
            _ => "",
        };
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "smart-quota" | "soonest-reset" => Routing::SmartQuota,
            "fill-first" => Routing::FillFirst,
            "round-robin" | "weighted-round-robin" => Routing::RoundRobin,
            _ => Routing::LeastUsed,
        })
    }
}

fn lenient_u32<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    let v = Yaml::deserialize(d)?;
    Ok(v.as_i64().map(|n| n.clamp(0, u32::MAX as i64) as u32).unwrap_or(3))
}

fn percentage<'de, D: Deserializer<'de>>(d: D) -> Result<u8, D::Error> {
    let n = u8::deserialize(d)?;
    if n > 100 {
        return Err(serde::de::Error::custom("5-hour reserve must be a whole percentage between 0 and 100"));
    }
    Ok(n)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedClient {
    pub id: String,
    pub label: String,
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case", default)]
pub struct Tls {
    pub enable: bool,
    pub cert: String,
    pub key: String,
}

impl Tls {
    fn is_off(&self) -> bool {
        !self.enable && self.cert.is_empty() && self.key.is_empty()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct Notifications {
    /// A signed-in account needs signing in again.
    pub sign_in_expired: bool,
    /// Every account of a provider is used up or unavailable, and when one is back.
    pub provider_exhausted: bool,
    /// One account used up its 5-hour or weekly limit.
    pub account_used_up: bool,
    /// An account error, or three or more failed requests in an hour.
    pub account_errors: bool,
}

impl Default for Notifications {
    fn default() -> Self {
        Self { sign_in_expired: true, provider_exhausted: true, account_used_up: false, account_errors: false }
    }
}

impl Notifications {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "kebab-case", default)]
pub struct OAuthAlias {
    /// Upstream model name.
    pub name: String,
    /// Name clients use.
    pub alias: String,
    /// Keep serving `name` as well (otherwise the alias replaces it).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub fork: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case", default)]
pub struct KeyEntry {
    pub api_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Restrict (and optionally rename) models served by this key.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelAlias>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Clients must call `prefix/model` to reach this key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// Model patterns this key must not serve (`*` wildcards).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub excluded_models: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case", default)]
pub struct CompatEntry {
    pub name: String,
    pub base_url: String,
    pub api_keys: Vec<String>,
    pub models: Vec<ModelAlias>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub excluded_models: Vec<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub disabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case", default)]
pub struct ModelAlias {
    /// Model name sent upstream.
    pub name: String,
    /// Name clients use. Defaults to `name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

impl ModelAlias {
    pub fn public(&self) -> &str {
        self.alias.as_deref().filter(|a| !a.is_empty()).unwrap_or(&self.name)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: 8317,
            auth_dir: DEFAULT_AUTH_DIR.into(),
            api_keys: vec![],
            named_clients: vec![],
            usage: Default::default(),
            management_key: String::new(),
            management_allow_remote: None,
            proxy_url: String::new(),
            request_retry: 3,
            routing: Routing::LeastUsed,
            five_hour_reserve_percent: 30,
            session_affinity: true,
            session_affinity_idle_seconds: 86_400,
            codex_websockets: true,
            claude_cloak: true,
            banked_resets: false,
            debug: false,
            tls: Tls::default(),
            notifications: Notifications::default(),
            force_model_prefix: false,
            oauth_model_alias: BTreeMap::new(),
            oauth_excluded_models: BTreeMap::new(),
            extra_models: BTreeMap::new(),
            ignored: vec![],
            legacy_auth_dir: false,
            claude_api_key: vec![],
            codex_api_key: vec![],
            gemini_api_key: vec![],
            vertex_api_key: vec![],
            kimi_api_key: vec![],
            xai_api_key: vec![],
            meta_api_key: vec![],
            openai_compatibility: vec![],
        }
    }
}

pub const DEFAULT_AUTH_DIR: &str = "~/.fusebox";
/// Where sign-ins lived before the rename (and where CLIProxyAPI keeps them).
pub const LEGACY_AUTH_DIR: &str = "~/.cli-proxy-api";

const TEMPLATE: &str = r#"# Fusebox configuration. Changes are picked up automatically.

host: "127.0.0.1"          # use 0.0.0.0 to expose on your network (set api-keys first!)
port: 8317
auth-dir: "~/.fusebox"      # OAuth credentials (CLIProxyAPI's files work too)

# Keys your clients must send. Leave empty to allow anyone who can reach the port.
api-keys: []

# Protects the dashboard + management API. Empty = only reachable from localhost.
management-key: ""

proxy-url: ""               # optional upstream proxy, e.g. socks5://127.0.0.1:1080
request-retry: 3            # accounts to try before failing a request
routing: least-used         # new sessions: least-used | smart-quota | round-robin | fill-first
five-hour-reserve-percent: 30 # smart-quota: keep this 5-hour share for sessions already on an account
session-affinity: true      # keep a session on its subscription until quota is exhausted
session-affinity-idle-seconds: 86400 # expire assignments after a day without requests
codex-websockets: true      # native upstream websocket for Codex websocket clients
claude-cloak: true          # make non-Claude-Code clients look like Claude Code on OAuth accounts
banked-resets: false        # show and spend banked Claude/ChatGPT limit resets (unofficial endpoints)
debug: false

# Persistent proxy usage; imports/collectors are opt-in. Storage changes need a restart.
usage:
  enabled: true
  retention-days: 90
  queue-capacity: 1024
  # database: /path/to/usage.sqlite3  # default: beside config.yaml
  # pricing-overrides: /path/to/rates.json

# Push notifications for devices that turn them on in the dashboard (needs HTTPS).
# notifications:
#   sign-in-expired: true      # an account needs signing in again
#   provider-exhausted: true   # every account of a provider is out, and when one is back
#   account-used-up: false     # one account used up its 5-hour or weekly limit
#   account-errors: false      # account errors and runs of failed requests

# Models released after this Fusebox version, served until it knows them. The dashboard's
# "Add model" writes here; an entry is removed once Fusebox or the provider lists the model.
# extra-models:
#   claude: [claude-opus-6]

# Optional named inference credentials; existing api-keys continue to work.
named-clients: []
#  - id: desktop
#    label: Work desktop
#    key: choose-a-distinct-random-key

# API keys (optional). Accounts (Claude, Codex, Antigravity, Kimi, xAI, Meta, Devin, Vertex)
# are added with `fusebox login <provider>` or from the dashboard.
claude-api-key: []
#  - api-key: "sk-ant-..."
#    base-url: "https://api.anthropic.com"   # optional

codex-api-key: []
#  - api-key: "sk-..."
#    base-url: "https://api.openai.com/v1"   # optional

gemini-api-key: []
#  - api-key: "AIza..."

vertex-api-key: []          # Vertex AI express mode; service accounts: `fusebox login vertex --file sa.json`
#  - api-key: "AQ..."

kimi-api-key: []
#  - api-key: "sk-kimi-..."                 # Kimi Code key
#  - api-key: "sk-..."                      # Moonshot platform key
#    base-url: "https://api.moonshot.ai/v1"

xai-api-key: []
#  - api-key: "xai-..."

meta-api-key: []
#  - api-key: "..."

openai-compatibility: []
#  - name: openrouter
#    base-url: "https://openrouter.ai/api/v1"
#    api-keys: ["sk-or-..."]
#    models:
#      - name: "moonshotai/kimi-k3"
#        alias: "kimi-k3"
"#;

/// The first of `names` that is set, so renamed variables keep their old spelling.
pub fn first_env(names: &[&str], get: impl Fn(&str) -> Option<String>) -> Option<String> {
    names.iter().find_map(|n| get(n).filter(|v| !v.is_empty()))
}

pub fn env_var(names: &[&str]) -> Option<String> {
    first_env(names, |n| std::env::var(n).ok())
}

/// 127.0.0.1, unless the environment says otherwise (the Docker image binds all interfaces).
fn default_host() -> String {
    // CLIPROXYAPI_RUST_DEFAULT_HOST: the name before the rename to Fusebox, still honoured.
    env_var(&["FUSEBOX_DEFAULT_HOST", "CLIPROXYAPI_RUST_DEFAULT_HOST"]).unwrap_or_else(|| "127.0.0.1".into())
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

/// `~/.fusebox`, unless only the pre-Fusebox `~/.cli-proxy-api` exists.
pub fn default_auth_dir_in(home: Option<&Path>) -> &'static str {
    match home {
        Some(h) if !h.join(".fusebox").exists() && h.join(".cli-proxy-api").is_dir() => LEGACY_AUTH_DIR,
        _ => DEFAULT_AUTH_DIR,
    }
}

pub fn default_auth_dir() -> &'static str {
    default_auth_dir_in(home_dir().as_deref())
}

/// The commented starter config, bound to the default host and auth directory.
pub fn template() -> String {
    let t = TEMPLATE.replacen("host: \"127.0.0.1\"", &format!("host: \"{}\"", default_host()), 1);
    if default_auth_dir() == LEGACY_AUTH_DIR { t.replacen("\"~/.fusebox\"", "\"~/.cli-proxy-api\"", 1) } else { t }
}

pub fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/").or_else(|| (p == "~").then_some(""))
        && let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(p)
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let mut created = false;
        if !path.exists() {
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir).ok();
            }
            std::fs::write(path, template()).with_context(|| format!("writing {}", path.display()))?;
            tracing::info!("created default config at {}", path.display());
            created = true;
        }
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut cfg = Self::parse(&text)?;
        // A new config written with the legacy directory still deserves the startup note.
        cfg.legacy_auth_dir |= created && cfg.auth_dir == LEGACY_AUTH_DIR;
        Ok(cfg)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut doc: Yaml = if text.trim().is_empty() {
            Yaml::Mapping(Default::default())
        } else {
            serde_yaml::from_str(text).context("invalid config")?
        };
        let ignored = crate::compat::normalize(&mut doc);
        let explicit = doc.get("auth-dir").is_some_and(|v| !v.is_null());
        let mut cfg: Config = serde_yaml::from_value(doc).context("invalid config")?;
        cfg.ignored = ignored;
        // An explicit auth-dir always wins; otherwise keep using an existing legacy directory.
        if !explicit && default_auth_dir() == LEGACY_AUTH_DIR {
            cfg.auth_dir = LEGACY_AUTH_DIR.into();
            cfg.legacy_auth_dir = true;
        }
        if cfg.usage.queue_capacity == 0
            || cfg.usage.queue_capacity > 65536
            || cfg.usage.retention_days == 0
            || cfg.usage.retention_days > 36500
        {
            anyhow::bail!("invalid usage queue capacity or retention");
        }
        let mut ids = std::collections::HashSet::new();
        for client in &cfg.named_clients {
            if client.id.is_empty()
                || client.key.is_empty()
                || client.key.starts_with("fbxc_")
                || !ids.insert(&client.id)
            {
                anyhow::bail!("named-clients require unique ids and nonempty inference keys");
            }
            crate::usage::types::valid_label(&client.id).map_err(anyhow::Error::msg)?;
            crate::usage::types::valid_label(&client.label).map_err(anyhow::Error::msg)?;
        }
        Ok(cfg)
    }

    pub fn auth_dir(&self) -> PathBuf {
        expand_home(&self.auth_dir)
    }

    pub fn is_loopback(&self) -> bool {
        matches!(self.host.as_str(), "127.0.0.1" | "localhost" | "::1")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renamed_variables_fall_back_to_their_old_names() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |n: &str| vars.iter().find(|(k, _)| *k == n).map(|(_, v)| v.to_string())
        };
        let names = ["FUSEBOX_DEFAULT_HOST", "CLIPROXYAPI_RUST_DEFAULT_HOST"];
        let both = env(&[("FUSEBOX_DEFAULT_HOST", "0.0.0.0"), ("CLIPROXYAPI_RUST_DEFAULT_HOST", "::")]);
        assert_eq!(first_env(&names, both).as_deref(), Some("0.0.0.0"));
        let old = env(&[("CLIPROXYAPI_RUST_DEFAULT_HOST", "::")]);
        assert_eq!(first_env(&names, old).as_deref(), Some("::"));
        let blank = env(&[("FUSEBOX_DEFAULT_HOST", ""), ("CLIPROXYAPI_RUST_DEFAULT_HOST", "::")]);
        assert_eq!(first_env(&names, blank).as_deref(), Some("::"));
        assert_eq!(first_env(&names, env(&[])), None);
    }

    #[test]
    fn the_default_auth_dir_falls_back_to_an_existing_legacy_directory() {
        let home = std::env::temp_dir().join(format!("fusebox-home-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        assert_eq!(default_auth_dir_in(Some(&home)), DEFAULT_AUTH_DIR);
        std::fs::create_dir_all(home.join(".cli-proxy-api")).unwrap();
        assert_eq!(default_auth_dir_in(Some(&home)), LEGACY_AUTH_DIR);
        std::fs::create_dir_all(home.join(".fusebox")).unwrap();
        assert_eq!(default_auth_dir_in(Some(&home)), DEFAULT_AUTH_DIR);
        assert_eq!(default_auth_dir_in(None), DEFAULT_AUTH_DIR);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn an_explicit_auth_dir_always_wins() {
        for (text, dir) in [
            ("auth-dir: /data/auths\n", "/data/auths"),
            ("auth-dir: \"~/.cli-proxy-api\"\n", LEGACY_AUTH_DIR),
            ("auth-dir: \"~/.fusebox\"\n", DEFAULT_AUTH_DIR),
            ("oauth:\n  auth-dir: /nested\n", "/nested"),
        ] {
            let cfg = Config::parse(text).unwrap();
            assert_eq!(cfg.auth_dir, dir);
            assert!(!cfg.legacy_auth_dir);
        }
        // Without one, the default applies (or the legacy directory, when only that exists).
        let cfg = Config::parse("port: 8317\n").unwrap();
        assert_eq!(cfg.auth_dir, default_auth_dir());
        assert_eq!(cfg.legacy_auth_dir, default_auth_dir() == LEGACY_AUTH_DIR);
    }

    #[test]
    fn the_template_names_fusebox() {
        let t = TEMPLATE;
        assert!(t.starts_with("# Fusebox configuration."));
        assert!(t.contains("auth-dir: \"~/.fusebox\""));
        assert!(t.contains("`fusebox login <provider>`"));
        assert_eq!(Config::parse(t).unwrap().auth_dir, DEFAULT_AUTH_DIR);
    }
}
