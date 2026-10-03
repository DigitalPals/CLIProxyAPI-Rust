//! Drop-in compatibility with CLIProxyAPI: its config files (the nested v8
//! layout and the older flat one), in-place edits that keep a file usable by
//! CLIProxyAPI, and its command-line flags.

use serde_yaml::{Mapping, Value as Yaml};

fn key(s: &str) -> Yaml {
    Yaml::String(s.into())
}

fn path<'a>(v: &'a Yaml, p: &[&str]) -> Option<&'a Yaml> {
    p.iter().try_fold(v, |cur, k| cur.as_mapping()?.get(k)).filter(|v| !v.is_null())
}

/// Copies `from` (a nested path) to the flat `to` key. `force` = the new spelling wins.
fn lift(doc: &mut Yaml, from: &[&str], to: &str, force: bool) {
    let Some(v) = path(doc, from).cloned() else { return };
    let root = doc.as_mapping_mut().unwrap();
    if force || root.get(to).is_none_or(Yaml::is_null) {
        root.insert(key(to), v);
    }
}

/// Provider group names under v8 `api-keys:` and the flat key they become.
const GROUPS: &[(&str, &str)] = &[
    ("claude", "claude-api-key"),
    ("codex", "codex-api-key"),
    ("gemini", "gemini-api-key"),
    ("vertex", "vertex-api-key"),
    ("kimi", "kimi-api-key"),
    ("xai", "xai-api-key"),
    ("meta", "meta-api-key"),
];

const SHARED: &[&str] = &["base-url", "proxy-url", "headers", "models", "excluded-models", "prefix"];

/// A v8 group (`{name, base-url, ..., keys: [{api-key, ...}]}`) as flat key entries.
fn flatten_group(group: &Mapping) -> Vec<Yaml> {
    let keys = group.get("keys").and_then(Yaml::as_sequence).cloned().unwrap_or_default();
    keys.iter()
        .filter_map(Yaml::as_mapping)
        .map(|k| {
            let mut entry = Mapping::new();
            for f in SHARED {
                if let Some(v) = k.get(*f).filter(|v| !v.is_null()).or_else(|| group.get(*f).filter(|v| !v.is_null())) {
                    entry.insert(key(f), v.clone());
                }
            }
            if let Some(v) = k.get("api-key") {
                entry.insert(key("api-key"), v.clone());
            }
            Yaml::Mapping(entry)
        })
        .collect()
}

/// A v8 openai-compatibility group as one flat compat entry.
fn compat_group(group: &Mapping) -> Yaml {
    let mut entry = group.clone();
    let keys = entry.remove("keys").and_then(|k| k.as_sequence().cloned()).unwrap_or_default();
    entry.insert(key("api-key-entries"), Yaml::Sequence(keys));
    Yaml::Mapping(entry)
}

/// Rewrites a CLIProxyAPI config into this crate's flat layout, in memory.
/// Returns the CLIProxyAPI features that have no effect here.
pub fn normalize(doc: &mut Yaml) -> Vec<String> {
    let mut ignored = Vec::new();
    let Some(root) = doc.as_mapping_mut() else { return ignored };

    // v8 reuses the root `api-keys` for provider groups; client keys moved to access.api-keys.
    let groups = match root.get("api-keys") {
        Some(Yaml::Mapping(_)) => root.remove("api-keys").and_then(|v| v.as_mapping().cloned()),
        _ => None,
    };

    // New (v8) spellings win over the old flat ones.
    lift(doc, &["access", "api-keys"], "api-keys", true);
    lift(doc, &["server", "host"], "host", true);
    lift(doc, &["server", "port"], "port", true);
    lift(doc, &["server", "tls"], "tls", true);
    lift(doc, &["management", "secret-key"], "management-key", true);
    lift(doc, &["management", "allow-remote"], "management-allow-remote", true);
    lift(doc, &["remote-management", "secret-key"], "management-key", false);
    lift(doc, &["remote-management", "allow-remote"], "management-allow-remote", false);
    lift(doc, &["routing", "retry", "request-retry"], "request-retry", true);
    lift(doc, &["routing", "force-model-prefix"], "force-model-prefix", true);
    lift(doc, &["routing", "session-affinity"], "session-affinity", true);
    lift(doc, &["requests", "proxy-url"], "proxy-url", true);
    lift(doc, &["oauth", "auth-dir"], "auth-dir", true);
    lift(doc, &["oauth", "model-alias"], "oauth-model-alias", true);
    lift(doc, &["oauth", "excluded-models"], "oauth-excluded-models", true);
    lift(doc, &["observability", "logs", "debug"], "debug", true);

    let disable_cloak = path(doc, &["oauth", "providers", "claude", "disable-claude-cloak-mode"])
        .or_else(|| path(doc, &["disable-claude-cloak-mode"]))
        .and_then(Yaml::as_bool);
    let root = doc.as_mapping_mut().unwrap();
    if let Some(off) = disable_cloak
        && root.get("claude-cloak").is_none()
    {
        root.insert(key("claude-cloak"), Yaml::Bool(!off));
    }

    // Very old configs listed Gemini keys as plain strings.
    if let Some(Yaml::Sequence(old)) = root.remove("generative-language-api-key")
        && root.get("gemini-api-key").is_none()
    {
        let entries = old.into_iter().map(|k| Yaml::Mapping(Mapping::from_iter([(key("api-key"), k)]))).collect();
        root.insert(key("gemini-api-key"), Yaml::Sequence(entries));
    }

    if let Some(groups) = groups {
        for (name, list) in groups {
            let name = name.as_str().unwrap_or_default().to_string();
            let list: Vec<Mapping> =
                list.as_sequence().into_iter().flatten().filter_map(|g| g.as_mapping().cloned()).collect();
            if name == "openai-compatibility" {
                let entries = list.iter().map(compat_group).collect();
                root.insert(key("openai-compatibility"), Yaml::Sequence(entries));
            } else if let Some((_, flat)) = GROUPS.iter().find(|(g, _)| *g == name) {
                let entries = list.iter().flat_map(flatten_group).collect();
                root.insert(key(flat), Yaml::Sequence(entries));
            } else if !list.is_empty() {
                ignored.push(format!("api-keys.{name}"));
            }
        }
    }

    // Older compat entries keep their keys in api-key-entries: [{api-key, proxy-url}].
    if let Some(Yaml::Sequence(list)) = root.get_mut("openai-compatibility") {
        for entry in list.iter_mut().filter_map(Yaml::as_mapping_mut) {
            let Some(Yaml::Sequence(keys)) = entry.remove("api-key-entries") else { continue };
            if entry.get("api-keys").is_none() {
                let plain: Vec<Yaml> = keys
                    .iter()
                    .filter_map(|k| k.as_mapping().and_then(|m| m.get("api-key")).or(Some(k)).filter(|v| v.is_string()))
                    .cloned()
                    .collect();
                entry.insert(key("api-keys"), Yaml::Sequence(plain));
            }
            let proxy = keys.iter().find_map(|k| k.as_mapping()?.get("proxy-url").filter(|v| v.is_string()).cloned());
            if let Some(p) = proxy
                && entry.get("proxy-url").is_none_or(Yaml::is_null)
            {
                entry.insert(key("proxy-url"), p);
            }
        }
    }

    // Features this binary doesn't have, so the startup log can say so.
    let mut note = |present: bool, what: &str| {
        if present {
            ignored.push(what.to_string());
        }
    };
    note(path(doc, &["payload"]).or(path(doc, &["requests", "payload"])).is_some(), "payload rules");
    note(path(doc, &["plugins", "enabled"]).and_then(Yaml::as_bool) == Some(true), "plugins");
    let strategy = path(doc, &["routing", "strategy"]).and_then(Yaml::as_str).unwrap_or_default();
    note(strategy == "weighted-round-robin", "weighted routing (round-robin is used)");
    note(path(doc, &["interactions-api-key"]).is_some(), "interactions-api-key");
    ignored
}

// ------------------------------------------------------------------ editing

/// Which layout a config file uses, so edits land where CLIProxyAPI also reads them.
fn is_v8(doc: &Yaml) -> bool {
    path(doc, &["config-version"]).and_then(Yaml::as_i64).is_some_and(|v| v >= 8)
        || path(doc, &["api-keys"]).is_some_and(Yaml::is_mapping)
}

fn seq<'a>(m: &'a mut Mapping, k: &str) -> &'a mut Vec<Yaml> {
    if !m.get(k).is_some_and(Yaml::is_sequence) {
        m.insert(key(k), Yaml::Sequence(vec![]));
    }
    m.get_mut(k).and_then(Yaml::as_sequence_mut).unwrap()
}

fn map<'a>(m: &'a mut Mapping, k: &str) -> &'a mut Mapping {
    if !m.get(k).is_some_and(Yaml::is_mapping) {
        m.insert(key(k), Yaml::Mapping(Mapping::new()));
    }
    m.get_mut(k).and_then(Yaml::as_mapping_mut).unwrap()
}

pub struct NewKey<'a> {
    /// claude, codex, gemini, vertex, kimi, xai, meta or openai-compatibility.
    pub group: &'a str,
    pub api_key: &'a str,
    pub base_url: Option<&'a str>,
    pub models: Vec<(String, Option<String>)>,
    /// openai-compatibility only.
    pub name: Option<&'a str>,
}

fn models_yaml(models: &[(String, Option<String>)]) -> Yaml {
    Yaml::Sequence(
        models
            .iter()
            .map(|(name, alias)| {
                let mut m = Mapping::from_iter([(key("name"), key(name))]);
                if let Some(a) = alias {
                    m.insert(key("alias"), key(a));
                }
                Yaml::Mapping(m)
            })
            .collect(),
    )
}

/// Adds an upstream API key in whichever layout the file already uses.
pub fn add_key(doc: &mut Yaml, k: &NewKey) {
    if doc.is_null() {
        *doc = Yaml::Mapping(Mapping::new());
    }
    // CLIProxyAPI's v8 groups have no `kimi`; keep those keys in the flat spelling.
    let v8 = is_v8(doc) && k.group != "kimi";
    let root = doc.as_mapping_mut().unwrap();
    let compat = k.group == "openai-compatibility";
    let mut entry = Mapping::new();
    if let Some(b) = k.base_url {
        entry.insert(key("base-url"), key(b));
    }
    if !k.models.is_empty() {
        entry.insert(key("models"), models_yaml(&k.models));
    }
    let key_entry = || Yaml::Mapping(Mapping::from_iter([(key("api-key"), key(k.api_key))]));

    if compat {
        let name = k.name.unwrap_or("provider");
        let list =
            if v8 { seq(map(root, "api-keys"), "openai-compatibility") } else { seq(root, "openai-compatibility") };
        let keys_field = if v8 { "keys" } else { "api-key-entries" };
        let existing = list.iter_mut().filter_map(Yaml::as_mapping_mut).find(|e| {
            e.get("name").and_then(Yaml::as_str) == Some(name) && e.get("base-url").and_then(Yaml::as_str) == k.base_url
        });
        match existing {
            Some(e) => {
                if !k.api_key.is_empty() {
                    match e.get_mut("api-keys") {
                        Some(Yaml::Sequence(plain)) => plain.push(key(k.api_key)),
                        _ => seq(e, keys_field).push(key_entry()),
                    }
                }
                let models = seq(e, "models");
                for (n, a) in &k.models {
                    let public = a.as_deref().unwrap_or(n);
                    let known = models.iter().any(|m| {
                        let m = m.as_mapping();
                        let alias = m.and_then(|m| m.get("alias")).and_then(Yaml::as_str).filter(|s| !s.is_empty());
                        alias.or_else(|| m.and_then(|m| m.get("name")).and_then(Yaml::as_str)) == Some(public)
                    });
                    if !known {
                        models.push(models_yaml(&[(n.clone(), a.clone())]).as_sequence().unwrap()[0].clone());
                    }
                }
            }
            None => {
                entry.insert(key("name"), key(name));
                let keys = if k.api_key.is_empty() { vec![] } else { vec![key_entry()] };
                entry.insert(key(keys_field), Yaml::Sequence(keys));
                list.push(Yaml::Mapping(entry));
            }
        }
        return;
    }
    if v8 {
        let groups = seq(map(root, "api-keys"), k.group);
        let n = groups.len() + 1;
        entry.insert(key("name"), key(&format!("{}-{n}", k.group)));
        entry.insert(key("keys"), Yaml::Sequence(vec![key_entry()]));
        groups.push(Yaml::Mapping(entry));
    } else {
        entry.insert(key("api-key"), key(k.api_key));
        let flat = GROUPS.iter().find(|(g, _)| *g == k.group).map(|(_, f)| *f).unwrap_or("claude-api-key");
        seq(root, flat).push(Yaml::Mapping(entry));
    }
}

/// Removes an upstream API key from every place it appears (both layouts).
/// `group` limits compat removal to one openai-compatibility entry.
pub fn remove_key(doc: &mut Yaml, api_key: &str, compat_group: Option<&str>) {
    let Some(root) = doc.as_mapping_mut() else { return };
    let is_it = |v: &Yaml| {
        v.as_mapping().and_then(|m| m.get("api-key")).and_then(Yaml::as_str).map(str::trim) == Some(api_key)
            || v.as_str().map(str::trim) == Some(api_key)
    };
    let prune_compat = |list: &mut Vec<Yaml>| {
        list.retain_mut(|e| {
            let Some(m) = e.as_mapping_mut() else { return true };
            if compat_group.is_some_and(|g| m.get("name").and_then(Yaml::as_str) != Some(g)) {
                return true;
            }
            let mut had = false;
            for f in ["api-keys", "api-key-entries", "keys"] {
                if let Some(Yaml::Sequence(keys)) = m.get_mut(f) {
                    had |= !keys.is_empty();
                    if api_key.is_empty() {
                        keys.clear();
                    } else {
                        keys.retain(|k| !is_it(k));
                    }
                }
            }
            let left = ["api-keys", "api-key-entries", "keys"]
                .iter()
                .any(|f| m.get(*f).and_then(Yaml::as_sequence).is_some_and(|s| !s.is_empty()));
            // Removing a group's last key removes the group (a keyless one would stay usable).
            !(had || api_key.is_empty()) || left
        });
    };
    for (_, flat) in GROUPS {
        if let Some(Yaml::Sequence(list)) = root.get_mut(*flat) {
            list.retain(|e| !is_it(e));
        }
    }
    if let Some(Yaml::Sequence(list)) = root.get_mut("openai-compatibility") {
        prune_compat(list);
    }
    if let Some(Yaml::Mapping(groups)) = root.get_mut("api-keys") {
        for (name, list) in groups.iter_mut() {
            let Some(list) = list.as_sequence_mut() else { continue };
            if name.as_str() == Some("openai-compatibility") {
                prune_compat(list);
                continue;
            }
            list.retain_mut(|g| {
                let Some(Yaml::Sequence(keys)) = g.as_mapping_mut().and_then(|m| m.get_mut("keys")) else {
                    return true;
                };
                let before = keys.len();
                keys.retain(|k| !is_it(k));
                before == keys.len() || !keys.is_empty()
            });
        }
    }
}

// ------------------------------------------------------------------- flags

/// Maps CLIProxyAPI's Go-style flags (`-config x`, `-claude-login`, ...) to
/// this binary's arguments, so existing scripts and service units keep working.
pub fn translate_args(args: Vec<String>) -> Vec<String> {
    let mut out = vec![args.first().cloned().unwrap_or_default()];
    let mut login: Option<String> = None;
    let mut rest = Vec::new();
    let mut it = args.into_iter();
    it.next();
    while let Some(a) = it.next() {
        let flag = a.trim_start_matches('-');
        let (name, inline) = match flag.split_once('=') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (flag.to_string(), None),
        };
        if !a.starts_with('-') || a == "-" {
            rest.push(a);
            continue;
        }
        let value = |it: &mut std::vec::IntoIter<String>| inline.clone().or_else(|| it.next());
        match name.as_str() {
            "config" => {
                if let Some(v) = value(&mut it) {
                    out.push("--config".into());
                    out.push(v);
                }
            }
            "no-browser" => rest.push("--no-browser".into()),
            "claude-login" => login = Some("claude".into()),
            "codex-login" | "codex-device-login" => login = Some("codex".into()),
            "antigravity-login" => login = Some("antigravity".into()),
            "kimi-login" | "kimi-ai-login" => login = Some("kimi".into()),
            "xai-login" => login = Some("xai".into()),
            "meta-login" => login = Some("meta".into()),
            "devin-login" => login = Some("devin".into()),
            "vertex-import" => {
                if let Some(v) = value(&mut it) {
                    login = Some("vertex".into());
                    rest.push("--file".into());
                    rest.push(v);
                }
            }
            "local-model" | "standalone" | "home-disable-cluster-discovery" => {}
            "oauth-callback-port" | "password" | "home-jwt" | "management-base-url" | "vertex-import-prefix" => {
                let _ = value(&mut it);
            }
            _ if a.starts_with("--") || name.len() == 1 => rest.push(a),
            _ => rest.push(format!("--{name}")),
        }
    }
    if let Some(p) = login {
        out.push("login".into());
        out.push(p);
    }
    out.extend(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Routing};

    const V8: &str = r#"
config-version: 8
server:
  host: ""
  port: 8417
  tls: { enable: false }
management:
  allow-remote: true
  secret-key: "$2a$10$abcdefghijklmnopqrstuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuu"
access:
  api-keys: ["client-1", "client-2"]
routing:
  strategy: "fill-first"
  retry: { request-retry: 5 }
requests:
  proxy-url: "socks5://127.0.0.1:1080"
oauth:
  auth-dir: "/data/auths"
  model-alias:
    claude:
      - { name: "claude-opus-5-5", alias: "opus", fork: true }
api-keys:
  gemini:
    - name: g1
      base-url: "https://gemini.example"
      excluded-models: ["gemini-2.5-*"]
      keys:
        - api-key: "AIza-1"
        - api-key: "AIza-2"
          proxy-url: "direct"
  openai-compatibility:
    - name: openrouter
      base-url: "https://openrouter.ai/api/v1"
      keys: [{ api-key: "sk-or-1" }]
      models: [{ name: "moonshotai/kimi-k3", alias: "kimi-k3" }]
payload: { default: [] }
"#;

    const LEGACY: &str = r#"
host: ""
port: 8317
auth-dir: "~/.cli-proxy-api"
api-keys: ["client-1"]
remote-management:
  allow-remote: false
  secret-key: "plain"
request-retry: 3
routing:
  strategy: "weighted-round-robin"
proxy-url: ""
quota-exceeded: { switch-project: true }
usage-statistics-enabled: true
claude-api-key:
  - api-key: "sk-ant-1"
    prefix: "team"
codex-api-key:
  - api-key: "sk-1"
    base-url: "https://api.openai.com/v1"
    models: [{ name: "gpt-5.5", alias: "fast" }]
openai-compatibility:
  - name: ollama
    base-url: "http://127.0.0.1:11434/v1"
    api-key-entries:
      - api-key: "ok"
        proxy-url: "http://proxy:8080"
    models: [{ name: "qwen3", alias: "" }]
disable-claude-cloak-mode: true
"#;

    #[test]
    fn reads_the_v8_layout() {
        let cfg = Config::parse(V8).unwrap();
        assert_eq!((cfg.host.as_str(), cfg.port), ("", 8417));
        assert_eq!(cfg.api_keys, vec!["client-1", "client-2"]);
        assert!(cfg.management_key.starts_with("$2a$"));
        assert_eq!(cfg.management_allow_remote, Some(true));
        assert_eq!(cfg.routing, Routing::FillFirst);
        assert_eq!(cfg.request_retry, 5);
        assert_eq!(cfg.proxy_url, "socks5://127.0.0.1:1080");
        assert_eq!(cfg.auth_dir, "/data/auths");
        assert_eq!(cfg.oauth_model_alias["claude"][0].alias, "opus");
        assert_eq!(cfg.gemini_api_key.len(), 2);
        assert_eq!(cfg.gemini_api_key[1].proxy_url.as_deref(), Some("direct"));
        assert_eq!(cfg.gemini_api_key[0].base_url.as_deref(), Some("https://gemini.example"));
        assert_eq!(cfg.gemini_api_key[0].excluded_models, vec!["gemini-2.5-*"]);
        assert_eq!(cfg.openai_compatibility[0].api_keys, vec!["sk-or-1"]);
        assert_eq!(cfg.openai_compatibility[0].models[0].public(), "kimi-k3");
        assert!(cfg.ignored.iter().any(|i| i == "payload rules"));
    }

    #[test]
    fn reads_the_legacy_layout() {
        let cfg = Config::parse(LEGACY).unwrap();
        assert_eq!(cfg.api_keys, vec!["client-1"]);
        assert_eq!(cfg.management_key, "plain");
        assert_eq!(cfg.management_allow_remote, Some(false));
        assert_eq!(cfg.routing, Routing::RoundRobin);
        assert_eq!(cfg.claude_api_key[0].prefix.as_deref(), Some("team"));
        assert_eq!(cfg.codex_api_key[0].models[0].public(), "fast");
        let c = &cfg.openai_compatibility[0];
        assert_eq!(c.api_keys, vec!["ok"]);
        assert_eq!(c.proxy_url.as_deref(), Some("http://proxy:8080"));
        assert_eq!(c.models[0].public(), "qwen3");
        assert!(!cfg.claude_cloak);
        assert!(cfg.ignored.iter().any(|i| i.starts_with("weighted")));
    }

    #[test]
    fn edits_keep_the_files_layout() {
        let mut v8: Yaml = serde_yaml::from_str(V8).unwrap();
        add_key(&mut v8, &NewKey { group: "claude", api_key: "sk-new", base_url: None, models: vec![], name: None });
        assert_eq!(v8["api-keys"]["claude"][0]["keys"][0]["api-key"], key("sk-new"));
        assert!(v8.get("claude-api-key").is_none());
        remove_key(&mut v8, "AIza-1", None);
        assert_eq!(v8["api-keys"]["gemini"][0]["keys"].as_sequence().unwrap().len(), 1);
        remove_key(&mut v8, "sk-or-1", Some("openrouter"));
        assert!(v8["api-keys"]["openai-compatibility"].as_sequence().unwrap().is_empty());
        // Untouched sections survive.
        assert_eq!(v8["payload"], serde_yaml::from_str::<Yaml>("{ default: [] }").unwrap());

        let mut legacy: Yaml = serde_yaml::from_str(LEGACY).unwrap();
        add_key(&mut legacy, &NewKey { group: "xai", api_key: "xai-1", base_url: None, models: vec![], name: None });
        assert_eq!(legacy["xai-api-key"][0]["api-key"], key("xai-1"));
        let models = vec![("qwen3-coder".to_string(), None)];
        let k = NewKey {
            group: "openai-compatibility",
            api_key: "k2",
            base_url: Some("http://127.0.0.1:11434/v1"),
            models,
            name: Some("ollama"),
        };
        add_key(&mut legacy, &k);
        let ollama = &legacy["openai-compatibility"][0];
        assert_eq!(ollama["api-key-entries"].as_sequence().unwrap().len(), 2);
        assert_eq!(ollama["models"].as_sequence().unwrap().len(), 2);
        remove_key(&mut legacy, "sk-ant-1", None);
        assert!(legacy["claude-api-key"].as_sequence().unwrap().is_empty());
        // Client keys are never touched.
        remove_key(&mut legacy, "client-1", None);
        assert_eq!(legacy["api-keys"].as_sequence().unwrap().len(), 1);
    }

    #[test]
    fn go_style_flags_translate() {
        let a = |v: &[&str]| translate_args(v.iter().map(|s| s.to_string()).collect());
        assert_eq!(a(&["x", "-config", "/c.yaml"]), ["x", "--config", "/c.yaml"]);
        assert_eq!(
            a(&["x", "--config=/c.yaml", "-claude-login", "-no-browser"]),
            ["x", "--config", "/c.yaml", "login", "claude", "--no-browser"]
        );
        assert_eq!(a(&["x", "-vertex-import", "k.json"]), ["x", "login", "vertex", "--file", "k.json"]);
        assert_eq!(a(&["x", "login", "codex", "--no-browser"]), ["x", "login", "codex", "--no-browser"]);
        assert_eq!(a(&["x", "-c", "a.yaml"]), ["x", "-c", "a.yaml"]);
    }
}
