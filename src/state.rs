use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::accounts::Pool;
use crate::config::Config;
use crate::ir::Usage;

pub struct App {
    cfg: ArcSwap<Config>,
    pub cfg_path: PathBuf,
    pub config_write: Mutex<()>,
    /// Startup settings remain separate from hot-reloaded settings.
    pub startup_config: Config,
    pub pool: Pool,
    pub sessions: Arc<crate::affinity::Sessions>,
    pub refresh_tasks: crate::oauth::RefreshTasks,
    pub http: Http,
    pub stats: Stats,
    pub usage: Option<crate::usage::store::Store>,
    pub usage_error: Option<String>,
    pub logins: Mutex<HashMap<String, crate::mgmt::Login>>,
    pub reset_quotes: Mutex<HashMap<String, crate::banked_resets::Quote>>,
    #[cfg(test)]
    pub reset_test_origin: Mutex<Option<String>>,
    pub started: DateTime<Utc>,
    pub live: broadcast::Sender<String>,
    /// Ignore our own writes to the auth dir in the file watcher.
    quiet_until: AtomicI64,
}

impl App {
    pub fn new(cfg: Config, cfg_path: PathBuf) -> Arc<Self> {
        let pool = Pool::default();
        pool.reload(&cfg);
        let (live, _) = broadcast::channel(512);
        let sessions = Arc::new(crate::affinity::Sessions::load(&cfg.auth_dir(), cfg.session_affinity_idle_seconds));
        // Tests opt in with an explicit isolated database; never touch a user's default DB.
        let usage_enabled = cfg.usage.enabled && (!cfg!(test) || cfg.usage.database.is_some());
        let (usage, usage_error) = if usage_enabled {
            match crate::usage::store::Store::open(
                &crate::usage::database_path(&cfg, &cfg_path),
                cfg.usage.queue_capacity,
                cfg.usage.retention_days,
                cfg.usage.pricing_overrides.as_deref().map(std::path::Path::new),
            ) {
                Ok(store) => (Some(store), None),
                Err(_) => {
                    tracing::error!("usage database unavailable; proxy remains available with an analytics gap");
                    (
                        None,
                        Some("Usage database unavailable; check file access, disk space and migration version".into()),
                    )
                }
            }
        } else {
            (None, None)
        };
        Arc::new(Self {
            usage,
            usage_error,
            http: Http::new(&cfg.proxy_url),
            startup_config: cfg.clone(),
            cfg: ArcSwap::from_pointee(cfg),
            cfg_path,
            config_write: Mutex::new(()),
            pool,
            sessions,
            refresh_tasks: Default::default(),
            stats: Stats::default(),
            logins: Mutex::new(HashMap::new()),
            reset_quotes: Mutex::new(HashMap::new()),
            #[cfg(test)]
            reset_test_origin: Mutex::new(None),
            started: Utc::now(),
            live,
            quiet_until: AtomicI64::new(0),
        })
    }

    pub fn cfg(&self) -> Arc<Config> {
        self.cfg.load_full()
    }

    pub fn set_config(&self, cfg: Config) {
        self.http.set_default_proxy(&cfg.proxy_url);
        self.pool.reload(&cfg);
        self.cfg.store(Arc::new(cfg));
        self.broadcast("accounts", serde_json::Value::Null);
    }

    pub fn reload_accounts(&self) {
        self.pool.reload(&self.cfg());
        self.broadcast("accounts", serde_json::Value::Null);
    }

    pub fn suppress_reload(&self) {
        self.quiet_until.store(Utc::now().timestamp() + 3, Ordering::Relaxed);
    }

    pub fn reload_suppressed(&self) -> bool {
        Utc::now().timestamp() < self.quiet_until.load(Ordering::Relaxed)
    }

    pub fn broadcast(&self, kind: &str, data: impl Serialize) {
        if self.live.receiver_count() > 0 {
            let msg = serde_json::json!({ "type": kind, "data": data }).to_string();
            let _ = self.live.send(msg);
        }
    }
}

// ------------------------------------------------------------------------ http

pub struct Http {
    default_proxy: Mutex<String>,
    clients: Mutex<HashMap<String, reqwest::Client>>,
}

impl Http {
    fn new(proxy: &str) -> Self {
        Self { default_proxy: Mutex::new(proxy.to_string()), clients: Mutex::new(HashMap::new()) }
    }

    fn set_default_proxy(&self, proxy: &str) {
        *self.default_proxy.lock() = proxy.to_string();
    }

    /// Client for the given proxy (falls back to the configured default).
    pub fn client(&self, proxy: Option<&str>) -> reqwest::Client {
        self.build(proxy, None, false, false)
    }

    /// Short, bounded requests for credentials, discovery and usage data.
    pub fn control(&self, proxy: Option<&str>) -> reqwest::Client {
        self.build(proxy, None, false, true)
    }

    pub fn control_for_account(&self, acct: &crate::accounts::Account) -> reqwest::Client {
        let h1_pool = (acct.provider == crate::accounts::Provider::Antigravity).then_some(acct.id.as_str());
        self.build(acct.proxy_url.as_deref(), h1_pool, false, true)
    }

    /// Spending requests must never follow redirects or retry in the HTTP layer.
    pub fn for_reset(&self, proxy: Option<&str>) -> reqwest::Client {
        self.build(proxy, None, true, false)
    }

    /// Antigravity accounts each get their own HTTP/1.1 pool, as the IDE does;
    /// Google's backend treats shared HTTP/2 connections less kindly.
    pub fn for_account(&self, acct: &crate::accounts::Account) -> reqwest::Client {
        match acct.provider {
            crate::accounts::Provider::Antigravity => {
                self.build(acct.proxy_url.as_deref(), Some(&acct.id), false, false)
            }
            _ => self.build(acct.proxy_url.as_deref(), None, false, false),
        }
    }

    fn build(&self, proxy: Option<&str>, h1_pool: Option<&str>, reset: bool, control: bool) -> reqwest::Client {
        let proxy =
            proxy.filter(|p| !p.is_empty()).map(String::from).unwrap_or_else(|| self.default_proxy.lock().clone());
        let mut key = match h1_pool {
            Some(id) => format!("{proxy}\0h1:{id}"),
            None => proxy.clone(),
        };
        if reset {
            key.push_str("\0reset");
        }
        if control {
            key.push_str("\0control");
        }
        let mut clients = self.clients.lock();
        if let Some(c) = clients.get(&key) {
            return c.clone();
        }
        let mut b = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(600))
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(30));
        if control {
            b = b
                .connect_timeout(Duration::from_secs(10))
                .read_timeout(Duration::from_secs(30))
                .timeout(Duration::from_secs(30));
        }
        if reset {
            b = b.redirect(reqwest::redirect::Policy::none()).retry(reqwest::retry::never());
        }
        if proxy == "direct" || proxy == "none" {
            // CLIProxyAPI's spelling for "no proxy, not even the default one".
            b = b.no_proxy();
        } else if !proxy.is_empty() {
            match reqwest::Proxy::all(&proxy) {
                Ok(p) => b = b.proxy(p),
                Err(e) => tracing::error!("invalid proxy-url {proxy}: {e}"),
            }
        }
        if h1_pool.is_some() {
            b = b.http1_only().pool_idle_timeout(Duration::from_secs(200));
        }
        let c = b.build().expect("http client");
        clients.insert(key, c.clone());
        c
    }
}

// ----------------------------------------------------------------------- stats

#[derive(Debug, Clone, Serialize)]
pub struct RoutingAttempt {
    pub account_id: String,
    pub account: String,
    pub reason: &'static str,
    pub previous_account: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RequestLog {
    pub id: u64,
    pub ts: DateTime<Utc>,
    pub client: &'static str,
    /// The client program, when its User-Agent names one (Claude Code, Codex, an SDK).
    pub client_app: Option<&'static str>,
    pub provider: String,
    pub model: String,
    pub account: String,
    /// Id of the account that answered (empty when none was tried).
    pub account_id: String,
    /// Client-scoped fingerprint; never the client's raw session ID or API key.
    pub session_id: Option<String>,
    pub session_source: Option<&'static str>,
    pub routing_strategy: crate::config::Routing,
    pub routing_reason: Option<&'static str>,
    pub routing_warning: Option<&'static str>,
    pub routing_attempts: Vec<RoutingAttempt>,
    pub status: u16,
    pub latency_ms: u64,
    pub ttft_ms: Option<u64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_tokens: u64,
    pub stream: bool,
    pub transport: &'static str,
    pub attempts: u32,
    pub error: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Totals {
    pub requests: u64,
    pub ok: u64,
    pub failed: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_tokens: u64,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Bucket {
    pub minute: i64,
    pub requests: u64,
    pub failed: u64,
    pub tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_tokens: u64,
}

impl Bucket {
    fn add(&mut self, log: &RequestLog) {
        self.requests += 1;
        if log.status >= 400 {
            self.failed += 1;
        }
        self.tokens += log.input_tokens + log.output_tokens + log.cache_tokens;
        self.input_tokens += log.input_tokens;
        self.output_tokens += log.output_tokens;
        self.cache_tokens += log.cache_tokens;
    }
}

/// Adds a request to the minute it belongs to, keeping the last hour.
fn add_to_minute(series: &mut VecDeque<Bucket>, log: &RequestLog) {
    let minute = log.ts.timestamp() / 60;
    let cutoff = Utc::now().timestamp() / 60 - MINUTES as i64 + 1;
    series.retain(|b| b.minute >= cutoff);
    if minute < cutoff {
        return;
    }
    let index = series.partition_point(|b| b.minute < minute);
    if series.get(index).is_none_or(|b| b.minute != minute) {
        series.insert(index, Bucket { minute, ..Default::default() });
    }
    series[index].add(log);
}

/// The last 60 minutes, oldest first, with empty minutes filled in.
fn last_hour(series: &VecDeque<Bucket>) -> Vec<Bucket> {
    let now = Utc::now().timestamp() / 60;
    (0..MINUTES as i64)
        .rev()
        .map(|ago| {
            let m = now - ago;
            series.iter().find(|b| b.minute == m).cloned().unwrap_or(Bucket { minute: m, ..Default::default() })
        })
        .collect()
}

#[derive(Default)]
pub struct Stats {
    pub totals: Mutex<Totals>,
    pub recent: Mutex<VecDeque<RequestLog>>,
    pub series: Mutex<VecDeque<Bucket>>,
    /// The same minutes per account id.
    accounts: Mutex<HashMap<String, VecDeque<Bucket>>>,
    pub active: std::sync::atomic::AtomicU64,
    next_id: std::sync::atomic::AtomicU64,
}

const RECENT: usize = 300;
const MINUTES: usize = 60;

impl Stats {
    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn record(&self, log: &RequestLog) {
        let ok = log.status < 400;
        {
            let mut t = self.totals.lock();
            t.requests += 1;
            if ok {
                t.ok += 1
            } else {
                t.failed += 1
            }
            t.input_tokens += log.input_tokens;
            t.output_tokens += log.output_tokens;
            t.cache_tokens += log.cache_tokens;
        }
        add_to_minute(&mut self.series.lock(), log);
        if !log.account_id.is_empty() {
            let mut accounts = self.accounts.lock();
            // Forget accounts that have been quiet for an hour (or were removed).
            let cutoff = Utc::now().timestamp() / 60 - MINUTES as i64;
            accounts.retain(|_, s| s.back().is_some_and(|b| b.minute > cutoff));
            add_to_minute(accounts.entry(log.account_id.clone()).or_default(), log);
        }
        let mut r = self.recent.lock();
        r.push_back(log.clone());
        while r.len() > RECENT {
            r.pop_front();
        }
    }

    pub fn series(&self) -> Vec<Bucket> {
        last_hour(&self.series.lock())
    }

    /// One account's last 60 minutes.
    pub fn account_series(&self, id: &str) -> Vec<Bucket> {
        last_hour(self.accounts.lock().get(id).unwrap_or(&VecDeque::new()))
    }
}

pub fn usage_tokens(u: &Usage) -> (u64, u64, u64) {
    (u.input + u.cache_write, u.output, u.cache_read)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(minute: i64, tokens: u64) -> RequestLog {
        RequestLog {
            id: 0,
            ts: DateTime::from_timestamp(minute * 60, 0).unwrap(),
            client: "responses",
            client_app: None,
            provider: "codex".into(),
            model: "test".into(),
            account: "test".into(),
            account_id: "account".into(),
            session_id: None,
            session_source: None,
            routing_strategy: crate::config::Routing::RoundRobin,
            routing_reason: None,
            routing_warning: None,
            routing_attempts: vec![],
            status: 200,
            latency_ms: 0,
            ttft_ms: None,
            input_tokens: tokens,
            output_tokens: 2,
            cache_tokens: 3,
            stream: true,
            transport: "http",
            attempts: 1,
            error: None,
        }
    }

    #[test]
    fn concurrent_streams_finishing_out_of_order_share_their_start_minute() {
        let now = Utc::now().timestamp() / 60;
        let stats = Stats::default();
        for (minute, tokens) in [(now - 2, 100), (now, 200), (now - 2, 300), (now - 1, 400), (now - 2, 500)] {
            stats.record(&log(minute, tokens));
        }
        let series = stats.series();
        assert_eq!(series.iter().map(|b| b.requests).sum::<u64>(), 5);
        assert_eq!(series.iter().map(|b| b.input_tokens).sum::<u64>(), 1500);
        let first = series.iter().find(|b| b.minute == now - 2).unwrap();
        assert_eq!((first.requests, first.input_tokens, first.tokens), (3, 900, 915));
        assert_eq!(stats.series.lock().len(), 3);
        assert!(series.windows(2).all(|b| b[0].minute < b[1].minute));
        assert_eq!(stats.account_series("account").iter().map(|b| b.input_tokens).sum::<u64>(), 1500);
    }

    #[test]
    fn a_late_completion_does_not_evict_the_current_hour() {
        let now = Utc::now().timestamp() / 60;
        let stats = Stats::default();
        for ago in (0..MINUTES as i64).rev() {
            stats.record(&log(now - ago, 10));
        }
        stats.record(&log(now - 65, 1000));
        let series = stats.series();
        assert_eq!(series.iter().map(|b| b.requests).sum::<u64>(), 60);
        assert_eq!(series.iter().map(|b| b.input_tokens).sum::<u64>(), 600);
        assert_eq!(stats.account_series("account").iter().map(|b| b.requests).sum::<u64>(), 60);
        assert_eq!(stats.totals.lock().input_tokens, 1600);
    }
}
