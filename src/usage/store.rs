//! Durable metadata-only usage journal and deterministic, conservative accounting.
use super::{pricing::Catalogue, types::Observation};
use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, LocalResult, NaiveDate, Offset, TimeZone, Utc};
use chrono_tz::Tz;
use rusqlite::{Connection, params, types::Value as SqlValue};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
    },
};
use tokio::sync::{OnceCell, Semaphore, mpsc, oneshot};

const VERSION: i64 = 2;
const APPLICATION_ID: i64 = 0x46555345;
const MAX_QUERY_DAYS: i64 = 3660;
const ENTRY_COLUMNS: &str = "id,source,origin_id,source_event_id,canonical_key,association_key,event_at_ms,provider,model,account_id,client_id,logical_request_id,attempt_id,response_id,completeness,input,cache_read,cache_write,write_5m,write_1h,output,reasoning,cost_nanos,pricing_basis,catalogue_version,json_extract(snapshot_json,'$.partial') AS pricing_partial";
const WINNER_ORDER: &str = "trust_rank,CASE completeness WHEN 'complete' THEN 0 WHEN 'partial' THEN 1 ELSE 2 END,known_fields DESC,COALESCE(output,-1) DESC,source,origin_id,source_event_id,fingerprint";
type Job = Box<dyn FnOnce(&mut Connection) + Send>;
enum Command {
    Observe(Box<Observation>),
    Call(Job),
}
#[derive(Default)]
struct Health {
    accepted: AtomicU64,
    dropped: AtomicU64,
    written: AtomicU64,
    rejected: AtomicU64,
    errors: AtomicU64,
    last_commit: AtomicI64,
    error: Mutex<Option<String>>,
    historical_dropped: u64,
    historical_rejected: u64,
    historical_errors: u64,
    prior_unclosed: u64,
    closed: AtomicBool,
    gate: Mutex<()>,
}
#[derive(Clone)]
pub struct Store {
    tx: mpsc::Sender<Command>,
    health: Arc<Health>,
    path: Arc<PathBuf>,
    read_slots: Arc<Semaphore>,
    shutdown_result: Arc<OnceCell<std::result::Result<(), String>>>,
    session_id: Arc<String>,
}
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Query {
    pub start: Option<String>,
    pub end: Option<String>,
    pub timezone: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub account: Option<String>,
    pub client: Option<String>,
    pub source: Option<String>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
    pub group_by: Option<String>,
}
#[derive(Default)]
struct CheckedSum;
impl rusqlite::functions::Aggregate<(Option<i128>, bool), Option<i64>> for CheckedSum {
    fn init(&self, _: &mut rusqlite::functions::Context<'_>) -> rusqlite::Result<(Option<i128>, bool)> {
        Ok((None, false))
    }
    fn step(&self, ctx: &mut rusqlite::functions::Context<'_>, acc: &mut (Option<i128>, bool)) -> rusqlite::Result<()> {
        if let Some(value) = ctx.get::<Option<i64>>(0)?
            && !acc.1
        {
            match acc.0.unwrap_or(0).checked_add(i128::from(value)) {
                Some(sum) => acc.0 = Some(sum),
                None => acc.1 = true,
            }
        }
        Ok(())
    }
    fn finalize(
        &self,
        _: &mut rusqlite::functions::Context<'_>,
        acc: Option<(Option<i128>, bool)>,
    ) -> rusqlite::Result<Option<i64>> {
        Ok(acc.and_then(|(value, overflow)| if overflow { None } else { value.and_then(|n| i64::try_from(n).ok()) }))
    }
}
fn register_aggregates(conn: &Connection) -> Result<()> {
    conn.create_aggregate_function(
        "usage_sum",
        1,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8 | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        CheckedSum,
    )?;
    Ok(())
}
const WRITER_SESSIONS_SCHEMA: &str = "CREATE TABLE usage_writer_sessions(id TEXT PRIMARY KEY,started_at_ms INTEGER NOT NULL,ended_at_ms INTEGER,clean INTEGER NOT NULL DEFAULT 0,dropped INTEGER NOT NULL DEFAULT 0,rejected INTEGER NOT NULL DEFAULT 0,writer_errors INTEGER NOT NULL DEFAULT 0,written INTEGER NOT NULL DEFAULT 0,last_commit_at_ms INTEGER NOT NULL DEFAULT 0);";
static SESSION_SEQUENCE: AtomicU64 = AtomicU64::new(0);
fn persist_health(conn: &Connection, id: &str, health: &Health) -> Result<()> {
    conn.execute("UPDATE usage_writer_sessions SET dropped=?2,rejected=?3,writer_errors=?4,written=?5,last_commit_at_ms=?6 WHERE id=?1",params![id,health.dropped.load(Ordering::Relaxed),health.rejected.load(Ordering::Relaxed),health.errors.load(Ordering::Relaxed),health.written.load(Ordering::Relaxed),health.last_commit.load(Ordering::Relaxed)])?;
    Ok(())
}
fn database_failure(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|e| e.downcast_ref::<rusqlite::Error>())
        .filter_map(rusqlite::Error::sqlite_error_code)
        .any(|code| {
            matches!(
                code,
                rusqlite::ErrorCode::DatabaseBusy
                    | rusqlite::ErrorCode::DatabaseLocked
                    | rusqlite::ErrorCode::PermissionDenied
                    | rusqlite::ErrorCode::ReadOnly
                    | rusqlite::ErrorCode::SystemIoFailure
                    | rusqlite::ErrorCode::DiskFull
                    | rusqlite::ErrorCode::CannotOpen
                    | rusqlite::ErrorCode::DatabaseCorrupt
                    | rusqlite::ErrorCode::NotADatabase
                    | rusqlite::ErrorCode::OutOfMemory
            )
        })
}
impl Health {
    fn fail(&self, e: &anyhow::Error) {
        self.errors.fetch_add(1, Ordering::Relaxed);
        *self.error.lock().unwrap() = Some(e.to_string());
    }
    fn committed(&self) {
        self.last_commit.store(Utc::now().timestamp_millis(), Ordering::Relaxed);
    }
}
impl Store {
    pub fn open(path: &Path, queue_capacity: usize, retention_days: u32, overrides: Option<&Path>) -> Result<Self> {
        if !(1..=65_536).contains(&queue_capacity) {
            bail!("usage queue capacity must be 1..65536");
        }
        if !(1..=36_600).contains(&retention_days) {
            bail!("usage retention days must be 1..36600");
        }
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let catalogue = Catalogue::load(overrides)?;
        Self::start(path, queue_capacity, retention_days, catalogue)
    }
    /// Admin/import access preserves the configured retention horizon and catalogue.
    pub fn open_existing(path: &Path, queue_capacity: usize) -> Result<Self> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let retention: String =
            conn.query_row("SELECT value FROM usage_meta WHERE key='retention_days'", [], |r| r.get(0))?;
        let raw: String = conn.query_row("SELECT value FROM usage_meta WHERE key='catalogue'", [], |r| r.get(0))?;
        let catalogue: Catalogue = serde_json::from_str(&raw)?;
        Self::start(path, queue_capacity, retention.parse().context("stored retention invalid")?, catalogue)
    }
    fn start(path: &Path, queue_capacity: usize, retention_days: u32, catalogue: Catalogue) -> Result<Self> {
        if !(1..=65_536).contains(&queue_capacity) || !(1..=36_600).contains(&retention_days) {
            bail!("invalid stored usage settings");
        }
        if path.as_os_str().is_empty() || path == Path::new(":memory:") {
            bail!("usage requires a persistent database path");
        }
        let mut conn = Connection::open(path).context("open usage database")?;
        register_aggregates(&conn)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&mut conn)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")?;
        conn.execute("INSERT INTO usage_meta(key,value) VALUES('catalogue',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[serde_json::to_string(&catalogue)?])?;
        conn.execute("INSERT INTO usage_meta(key,value) VALUES('retention_days',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[retention_days.to_string()])?;
        let cutoff = (Utc::now().timestamp_millis() - i64::from(retention_days) * 86_400_000).max(0);
        purge_conn(&mut conn, cutoff)?;
        let (tx, mut rx) = mpsc::channel(queue_capacity);
        let (historical_dropped,historical_rejected,historical_errors,prior_unclosed):(u64,u64,u64,u64)=conn.query_row("SELECT COALESCE(SUM(dropped),0),COALESCE(SUM(rejected),0),COALESCE(SUM(writer_errors),0),COALESCE(SUM(clean=0),0) FROM usage_writer_sessions",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        let session_id = hex::encode(Sha256::digest(
            format!(
                "{}:{}:{}",
                Utc::now().timestamp_nanos_opt().unwrap_or_default(),
                std::process::id(),
                SESSION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            )
            .as_bytes(),
        ));
        conn.execute(
            "INSERT INTO usage_writer_sessions(id,started_at_ms) VALUES(?1,?2)",
            params![session_id, Utc::now().timestamp_millis()],
        )?;
        let health = Arc::new(Health {
            historical_dropped,
            historical_rejected,
            historical_errors,
            prior_unclosed,
            ..Default::default()
        });
        let worker_health = health.clone();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .context("initialize usage worker timer")?;
        let worker_session = session_id.clone();
        std::thread::Builder::new()
            .name("fusebox-usage-db".into())
            .spawn(move || {
                let mut last_retention = Utc::now().timestamp_millis();
                loop {
                    let received = runtime
                        .block_on(async { tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await });
                    let command = match received {
                        Ok(Some(command)) => Some(command),
                        Ok(None) => {
                            let _ = persist_health(&conn, &worker_session, &worker_health);
                            break;
                        }
                        Err(_) => None,
                    };
                    let now = Utc::now().timestamp_millis();
                    if now - last_retention >= 3_600_000 {
                        if let Err(e) = purge_conn(&mut conn, (now - i64::from(retention_days) * 86_400_000).max(0)) {
                            worker_health.fail(&e);
                        }
                        last_retention = now;
                    }
                    if let Err(e) = persist_health(&conn, &worker_session, &worker_health) {
                        worker_health.fail(&e);
                    }
                    let Some(command) = command else {
                        continue;
                    };
                    match command {
                        Command::Call(f) => f(&mut conn),
                        Command::Observe(o) => {
                            let mut batch = vec![*o];
                            // Do not cross a queued durable barrier or custom transaction.
                            let mut pending = None;
                            while batch.len() < 200 {
                                match rx.try_recv() {
                                    Ok(Command::Observe(o)) => batch.push(*o),
                                    Ok(c) => {
                                        pending = Some(c);
                                        break;
                                    }
                                    Err(_) => break,
                                }
                            }
                            match insert_batch(&mut conn, &batch) {
                                Ok(result) => {
                                    worker_health
                                        .written
                                        .fetch_add(result["inserted"].as_u64().unwrap_or(0), Ordering::Relaxed);
                                    worker_health
                                        .rejected
                                        .fetch_add(result["purged"].as_u64().unwrap_or(0), Ordering::Relaxed);
                                    worker_health.committed();
                                }
                                Err(e) => {
                                    worker_health.dropped.fetch_add(batch.len() as u64, Ordering::Relaxed);
                                    worker_health.fail(&e);
                                }
                            }
                            if let Err(e) = persist_health(&conn, &worker_session, &worker_health) {
                                worker_health.fail(&e);
                            }
                            if let Some(Command::Call(f)) = pending {
                                f(&mut conn);
                            }
                        }
                    }
                }
            })
            .context("start usage database worker")?;
        Ok(Self {
            tx,
            health,
            path: Arc::new(path.to_path_buf()),
            read_slots: Arc::new(Semaphore::new(4)),
            shutdown_result: Arc::new(OnceCell::new()),
            session_id: Arc::new(session_id),
        })
    }
    pub fn enqueue(&self, observation: Observation) -> bool {
        if self.health.closed.load(Ordering::Acquire) {
            return false;
        }
        let Ok(_guard) = self.health.gate.try_lock() else {
            self.health.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        if self.health.closed.load(Ordering::Acquire) {
            return false;
        }
        if observation.validate().is_err() {
            self.health.rejected.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        match self.tx.try_send(Command::Observe(Box::new(observation))) {
            Ok(()) => {
                self.health.accepted.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(_) => {
                self.health.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }
    pub async fn call<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut Connection) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        if self.health.closed.load(Ordering::Acquire) {
            bail!("usage writer has shut down");
        }
        self.call_internal(f).await
    }
    async fn call_internal<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut Connection) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let health = self.health.clone();
        self.tx
            .send(Command::Call(Box::new(move |conn| {
                let changes_before = conn.total_changes();
                let mut result = f(conn);
                if !conn.is_autocommit() {
                    let rollback = conn.execute_batch("ROLLBACK");
                    if result.is_ok() {
                        result = Err(anyhow!("usage call returned with an uncommitted transaction"));
                    }
                    if let Err(e) = rollback {
                        health.fail(&anyhow!(e));
                    }
                }
                if let Err(e) = &result {
                    if database_failure(e) {
                        health.fail(e);
                    }
                } else {
                    if conn.total_changes() > changes_before {
                        health.committed();
                    }
                }
                let _ = tx.send(result);
            })))
            .await
            .map_err(|_| anyhow!("usage writer stopped"))?;
        rx.await.context("usage writer dropped transaction reply")?
    }
    /// Close observation ingress, drain prior commands durably, then mark this writer
    /// clean. Concurrent shutdown callers share the same acknowledged result.
    pub async fn shutdown(&self) -> Result<()> {
        let result = self
            .shutdown_result
            .get_or_init(|| async {
                {
                    let _gate = self.health.gate.lock().unwrap();
                    self.health.closed.store(true, Ordering::Release);
                }
                let id = self.session_id.clone();
                let health = self.health.clone();
                self.call_internal(move |conn| {
                    persist_health(conn, &id, &health)?;
                    conn.execute(
                        "UPDATE usage_writer_sessions SET clean=1,ended_at_ms=?2 WHERE id=?1",
                        params![id.as_str(), Utc::now().timestamp_millis()],
                    )?;
                    Ok(())
                })
                .await
                .map_err(|e| e.to_string())
            })
            .await;
        result.clone().map_err(|e| anyhow!(e))
    }
    pub async fn flush(&self) -> Result<()> {
        self.call(|conn| {
            conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE)")?;
            Ok(())
        })
        .await?;
        if let Some(e) = self.health.error.lock().unwrap().clone() {
            bail!("usage writer has reported data gaps: {e}");
        }
        Ok(())
    }
    pub fn health(&self) -> Value {
        let error = self.health.error.lock().unwrap().clone();
        json!({"state":if self.tx.is_closed() || self.health.closed.load(Ordering::Acquire){"stopped"}else if error.is_some() || self.health.dropped.load(Ordering::Relaxed)>0 || self.health.rejected.load(Ordering::Relaxed)>0 || self.health.historical_dropped>0 || self.health.historical_rejected>0 || self.health.historical_errors>0 || self.health.prior_unclosed>0{"degraded"}else{"healthy"},"message":error,"session_id":self.session_id.as_str(),"historical_gap":{"dropped":self.health.historical_dropped,"rejected":self.health.historical_rejected,"writer_errors":self.health.historical_errors},"prior_unclosed_sessions":self.health.prior_unclosed,"recovery_warning":if self.health.prior_unclosed>0{Some("Previous unclosed writer sessions may represent concurrent writers or an unclean shutdown; queued observations may be missing.")}else{None},"counter_scope":"dropped/rejected/writer_errors describe this writer; historical_gap describes earlier sessions; counters persist best effort between commands and on a one-second idle timer; busy transactions or disk failure can delay persistence", "queue_depth":self.tx.max_capacity()-self.tx.capacity(),"queue_capacity":self.tx.max_capacity(),"dropped":self.health.dropped.load(Ordering::Relaxed),"rejected":self.health.rejected.load(Ordering::Relaxed),"accepted":self.health.accepted.load(Ordering::Relaxed),"written":self.health.written.load(Ordering::Relaxed),"writer_errors":self.health.errors.load(Ordering::Relaxed),"last_commit_at_ms":self.health.last_commit.load(Ordering::Relaxed),"durability":"enqueued observations become durable at committed transaction; queued plus current batch are crash-loss bound"})
    }
    async fn read<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut Connection) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        // Independent WAL read snapshots cannot stall the bounded proxy writer.
        // Four slots bound blocking threads and concurrent aggregation work.
        let permit = self.read_slots.clone().acquire_owned().await.context("usage readers closed")?;
        let path = self.path.clone();
        let health = self.health.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let result = (|| {
                let mut conn = Connection::open_with_flags(
                    path.as_path(),
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )?;
                register_aggregates(&conn)?;
                conn.busy_timeout(std::time::Duration::from_secs(5))?;
                conn.execute_batch("PRAGMA query_only=ON; BEGIN;")?;
                f(&mut conn)
            })();
            if let Err(e) = &result
                && database_failure(e)
            {
                health.fail(e);
            }
            result
        })
        .await
        .context("usage query worker panicked")?
    }
    pub async fn query(&self, q: Query) -> Result<Value> {
        self.read(move |conn| summary(conn, &q)).await
    }
    pub async fn details(&self, q: Query) -> Result<Value> {
        self.read(move |conn| details(conn, &q)).await
    }
    pub async fn purge(&self, before_ms: i64) -> Result<Value> {
        self.call(move |conn| purge_conn(conn, before_ms)).await
    }
}
fn migrate(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let application_id: i64 = conn.pragma_query_value(None, "application_id", |r| r.get(0))?;
    if application_id != 0 && application_id != APPLICATION_ID {
        bail!("database belongs to another application");
    }
    if version > 0 && application_id != APPLICATION_ID {
        bail!("versioned usage database has an invalid application identity");
    }
    if version > VERSION {
        bail!("usage database schema {version} is newer than supported {VERSION}");
    }
    if version == VERSION {
        return Ok(());
    }
    if version == 1 {
        let tx = conn.transaction()?;
        tx.execute_batch(WRITER_SESSIONS_SCHEMA)?;
        tx.pragma_update(None, "user_version", VERSION)?;
        tx.commit()?;
        return Ok(());
    }
    if version != 0 {
        bail!("unsupported usage database migration");
    }
    let tables: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
        [],
        |r| r.get(0),
    )?;
    if tables > 0 {
        bail!("refusing to initialize usage schema in a nonempty unversioned database");
    }
    let tx = conn.transaction()?;
    tx.execute_batch(&format!("CREATE TABLE usage_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
    INSERT INTO usage_meta VALUES('purge_before_ms','0');
    CREATE TABLE usage_observations(
      id INTEGER PRIMARY KEY,source TEXT NOT NULL,origin_id TEXT NOT NULL,source_event_id TEXT NOT NULL,
      fingerprint TEXT NOT NULL,canonical_key TEXT NOT NULL,association_key TEXT NOT NULL,trust_rank INTEGER NOT NULL,
      event_at_ms INTEGER NOT NULL,ingested_at_ms INTEGER NOT NULL,provider TEXT NOT NULL,model TEXT,account_id TEXT,client_id TEXT,
      logical_request_id TEXT,attempt_id TEXT,response_id TEXT,completeness TEXT NOT NULL,known_fields INTEGER NOT NULL,
      input INTEGER,cache_read INTEGER,cache_write INTEGER,write_5m INTEGER,write_1h INTEGER,output INTEGER,reasoning INTEGER,
      cost_nanos INTEGER,pricing_basis TEXT NOT NULL,catalogue_version TEXT NOT NULL,snapshot_json TEXT NOT NULL,payload TEXT NOT NULL,
      UNIQUE(source,origin_id,source_event_id,fingerprint));
    CREATE TABLE usage_suppressed(source TEXT NOT NULL,source_event_id TEXT NOT NULL,origin_id TEXT NOT NULL DEFAULT '',reason TEXT NOT NULL,PRIMARY KEY(source,source_event_id,origin_id));
    CREATE INDEX usage_event_time ON usage_observations(event_at_ms);
    CREATE INDEX usage_canonical ON usage_observations(canonical_key,trust_rank);
    CREATE INDEX usage_association ON usage_observations(association_key);
    CREATE INDEX usage_source_event ON usage_observations(source,source_event_id);
    CREATE INDEX usage_source_canonical ON usage_observations(source,canonical_key,trust_rank);
    CREATE INDEX usage_filters ON usage_observations(provider,model,account_id,client_id,event_at_ms);
    CREATE TABLE usage_entries AS SELECT {ENTRY_COLUMNS} FROM usage_observations WHERE 0;
    CREATE UNIQUE INDEX usage_entries_key ON usage_entries(canonical_key);
    CREATE INDEX usage_entries_event_time ON usage_entries(event_at_ms);
    CREATE INDEX usage_entries_filters ON usage_entries(provider,model,account_id,client_id,event_at_ms);
    CREATE TABLE usage_source_entries AS SELECT {ENTRY_COLUMNS} FROM usage_observations WHERE 0;
    CREATE UNIQUE INDEX usage_source_entries_key ON usage_source_entries(source,canonical_key);
    CREATE INDEX usage_source_entries_event_time ON usage_source_entries(event_at_ms);
    CREATE INDEX usage_source_entries_filters ON usage_source_entries(provider,model,account_id,client_id,event_at_ms);
    PRAGMA user_version=1; PRAGMA application_id=1179996997;"))?;
    tx.execute_batch(WRITER_SESSIONS_SCHEMA)?;
    tx.pragma_update(None, "user_version", VERSION)?;
    tx.commit()?;
    Ok(())
}
fn association_key(o: &Observation) -> String {
    if let Some(id) = o.response_id.as_ref().filter(|s| !s.is_empty()) {
        return serde_json::to_string(&("response", &o.provider, id)).unwrap();
    }
    // Stable native event identities are scoped to their source namespace. Parser must
    // incorporate session/response identity into source_event_id for weaker formats.
    serde_json::to_string(&("event", &o.provider, &o.source, &o.source_event_id)).unwrap()
}
fn canonical_key(o: &Observation) -> String {
    let base = association_key(o);
    if o.source == "proxy"
        && o.response_id.is_some()
        && let Some(account) = &o.account_id
    {
        return serde_json::to_string(&("account_response", base, account)).unwrap();
    }
    base
}
/// Savepoint makes records + caller-owned checkpoint/outbox transactions atomic.
pub fn insert_batch(conn: &mut Connection, observations: &[Observation]) -> Result<Value> {
    if observations.len() > 10_000 {
        bail!("usage insertion batch exceeds 10000");
    }
    for o in observations {
        o.validate().map_err(|e| anyhow!(e))?;
    }
    let raw: String = conn.query_row("SELECT value FROM usage_meta WHERE key='catalogue'", [], |r| r.get(0))?;
    let catalogue: Catalogue = serde_json::from_str(&raw)?;
    let watermark: i64 =
        conn.query_row("SELECT CAST(value AS INTEGER) FROM usage_meta WHERE key='purge_before_ms'", [], |r| r.get(0))?;
    let tx = conn.savepoint()?;
    let mut changed_groups = BTreeSet::new();
    let mut inserted = 0;
    let mut duplicates = 0;
    let mut purged = 0;
    for o in observations {
        if o.event_at_ms < watermark {
            purged += 1;
            continue;
        }
        let mut normalized = o.clone();
        normalized.ingested_at_ms = 0;
        let normalized_payload = serde_json::to_string(&normalized)?;
        let fingerprint = hex::encode(Sha256::digest(normalized_payload.as_bytes()));
        let snapshot = catalogue.price(o);
        let tokens = &o.tokens;
        let known =
            [tokens.input, tokens.cache_read, tokens.cache_write, tokens.output].iter().filter(|v| v.is_some()).count();
        let rank = if o.source == "proxy" {
            0
        } else if o.origin_id == "local" {
            1
        } else {
            2
        };
        let changed=tx.prepare_cached("INSERT OR IGNORE INTO usage_observations(source,origin_id,source_event_id,fingerprint,canonical_key,association_key,trust_rank,event_at_ms,ingested_at_ms,provider,model,account_id,client_id,logical_request_id,attempt_id,response_id,completeness,known_fields,input,cache_read,cache_write,write_5m,write_1h,output,reasoning,cost_nanos,pricing_basis,catalogue_version,snapshot_json,payload) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29,?30)")?.execute(params![o.source,o.origin_id,o.source_event_id,fingerprint,canonical_key(o),association_key(o),rank,o.event_at_ms,Utc::now().timestamp_millis(),o.provider,o.actual_model.as_ref(),o.account_id,o.client_id,o.logical_request_id,o.attempt_id,o.response_id,o.completeness,known as i64,tokens.input,tokens.cache_read,tokens.cache_write,tokens.write_5m,tokens.write_1h,tokens.output,tokens.reasoning,snapshot.cost_nanos,snapshot.basis,snapshot.catalogue_version,serde_json::to_string(&snapshot)?,serde_json::to_string(o)?])?;
        if changed > 0 {
            changed_groups.insert((canonical_key(o), o.source.clone()));
            inserted += 1;
        } else {
            duplicates += 1;
        }
    }
    for (key, source) in changed_groups {
        tx.prepare_cached(&format!("INSERT OR REPLACE INTO usage_entries SELECT {ENTRY_COLUMNS} FROM usage_observations INDEXED BY usage_canonical WHERE canonical_key=?1 AND NOT EXISTS(SELECT 1 FROM usage_suppressed s WHERE s.source=usage_observations.source AND s.source_event_id=usage_observations.source_event_id AND (s.origin_id='' OR s.origin_id=usage_observations.origin_id)) ORDER BY {WINNER_ORDER} LIMIT 1"))?.execute([&key])?;
        tx.prepare_cached(&format!("INSERT OR REPLACE INTO usage_source_entries SELECT {ENTRY_COLUMNS} FROM usage_observations INDEXED BY usage_canonical WHERE canonical_key=?1 AND source=?2 AND NOT EXISTS(SELECT 1 FROM usage_suppressed s WHERE s.source=usage_observations.source AND s.source_event_id=usage_observations.source_event_id AND (s.origin_id='' OR s.origin_id=usage_observations.origin_id)) ORDER BY {WINNER_ORDER} LIMIT 1"))?.execute(params![key,source])?;
    }
    tx.commit()?;
    Ok(json!({"inserted":inserted,"duplicates":duplicates,"purged":purged}))
}
/// Preserve parser fallback audit records while excluding superseded usage from spend.
/// Native response records can supersede weaker cumulative fallback evidence atomically.
pub fn suppress_source_event(
    conn: &mut Connection,
    source: &str,
    source_event_id: &str,
    reason: &str,
) -> Result<Value> {
    if !matches!(source, "codex" | "claude_code") {
        bail!("only native fallback observations may be suppressed");
    }
    super::types::valid_label(source_event_id).map_err(|e| anyhow!(e))?;
    super::types::valid_label(reason).map_err(|e| anyhow!(e))?;
    suppress_origin_event(conn, source, source_event_id, "", reason)
}
pub fn suppress_origin_event(
    conn: &mut Connection,
    source: &str,
    source_event_id: &str,
    origin: &str,
    reason: &str,
) -> Result<Value> {
    if !matches!(source, "codex" | "claude_code") {
        bail!("only native fallback observations may be suppressed");
    }
    for label in [source_event_id, origin, reason] {
        super::types::valid_label(label).map_err(|e| anyhow!(e))?;
    }
    let tx = conn.savepoint()?;
    tx.execute("INSERT INTO usage_suppressed(source,source_event_id,origin_id,reason) VALUES(?1,?2,?3,?4) ON CONFLICT(source,source_event_id,origin_id) DO UPDATE SET reason=excluded.reason",params![source,source_event_id,origin,reason])?;
    let keys = {
        let mut statement =
            tx.prepare("SELECT DISTINCT canonical_key FROM usage_observations WHERE source=?1 AND source_event_id=?2")?;
        statement
            .query_map(params![source, source_event_id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for key in &keys {
        tx.execute("DELETE FROM usage_entries WHERE canonical_key=?1", [key])?;
        tx.execute("DELETE FROM usage_source_entries WHERE canonical_key=?1 AND source=?2", params![key, source])?;
        tx.prepare_cached(&format!("INSERT OR REPLACE INTO usage_entries SELECT {ENTRY_COLUMNS} FROM usage_observations INDEXED BY usage_canonical WHERE canonical_key=?1 AND NOT EXISTS(SELECT 1 FROM usage_suppressed s WHERE s.source=usage_observations.source AND s.source_event_id=usage_observations.source_event_id AND (s.origin_id='' OR s.origin_id=usage_observations.origin_id)) ORDER BY {WINNER_ORDER} LIMIT 1"))?.execute([key])?;
        tx.prepare_cached(&format!("INSERT OR REPLACE INTO usage_source_entries SELECT {ENTRY_COLUMNS} FROM usage_observations INDEXED BY usage_canonical WHERE canonical_key=?1 AND source=?2 AND NOT EXISTS(SELECT 1 FROM usage_suppressed s WHERE s.source=usage_observations.source AND s.source_event_id=usage_observations.source_event_id AND (s.origin_id='' OR s.origin_id=usage_observations.origin_id)) ORDER BY {WINNER_ORDER} LIMIT 1"))?.execute(params![key,source])?;
    }
    tx.commit()?;
    Ok(json!({"suppressed_groups":keys.len()}))
}
fn purge_conn(conn: &mut Connection, before_ms: i64) -> Result<Value> {
    if before_ms < 0 || before_ms > Utc::now().timestamp_millis() + 86_400_000 {
        bail!("purge timestamp out of bounds");
    }
    let tx = conn.savepoint()?;
    let mut affected = Vec::new();
    {
        let mut statement=tx.prepare("SELECT canonical_key,source FROM usage_entries WHERE event_at_ms<?1 UNION SELECT canonical_key,source FROM usage_source_entries WHERE event_at_ms<?1")?;
        let rows = statement.query_map([before_ms], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            affected.push(row?);
        }
    }
    let deleted = tx.execute("DELETE FROM usage_observations WHERE event_at_ms<?1", [before_ms])?;
    tx.execute("DELETE FROM usage_entries WHERE event_at_ms<?1", [before_ms])?;
    tx.execute("DELETE FROM usage_source_entries WHERE event_at_ms<?1", [before_ms])?;
    tx.execute("DELETE FROM usage_suppressed WHERE NOT EXISTS(SELECT 1 FROM usage_observations o WHERE o.source=usage_suppressed.source AND o.source_event_id=usage_suppressed.source_event_id)",[])?;
    for (key, source) in affected {
        tx.prepare_cached(&format!("INSERT OR REPLACE INTO usage_entries SELECT {ENTRY_COLUMNS} FROM usage_observations INDEXED BY usage_canonical WHERE canonical_key=?1 AND NOT EXISTS(SELECT 1 FROM usage_suppressed s WHERE s.source=usage_observations.source AND s.source_event_id=usage_observations.source_event_id AND (s.origin_id='' OR s.origin_id=usage_observations.origin_id)) ORDER BY {WINNER_ORDER} LIMIT 1"))?.execute([&key])?;
        tx.prepare_cached(&format!("INSERT OR REPLACE INTO usage_source_entries SELECT {ENTRY_COLUMNS} FROM usage_observations INDEXED BY usage_canonical WHERE canonical_key=?1 AND source=?2 AND NOT EXISTS(SELECT 1 FROM usage_suppressed s WHERE s.source=usage_observations.source AND s.source_event_id=usage_observations.source_event_id AND (s.origin_id='' OR s.origin_id=usage_observations.origin_id)) ORDER BY {WINNER_ORDER} LIMIT 1"))?.execute(params![key,source])?;
    }
    tx.execute(
        "UPDATE usage_meta SET value=CAST(MAX(CAST(value AS INTEGER),?1) AS TEXT) WHERE key='purge_before_ms'",
        [before_ms],
    )?;
    let watermark: i64 =
        tx.query_row("SELECT CAST(value AS INTEGER) FROM usage_meta WHERE key='purge_before_ms'", [], |r| r.get(0))?;
    tx.commit()?;
    Ok(json!({"deleted":deleted,"purge_before_ms":watermark}))
}

struct Range {
    start: i64,
    end: i64,
    tz: Tz,
}
fn boundary(value: &str, tz: Tz) -> Result<i64> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Ok(dt.timestamp_millis());
    }
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d").context("range must be RFC3339 or local YYYY-MM-DD")?;
    match tz.from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap()) {
        LocalResult::Single(v) => Ok(v.timestamp_millis()),
        LocalResult::Ambiguous(a, b) => Ok(a.min(b).timestamp_millis()),
        LocalResult::None => bail!("local midnight does not exist in selected timezone; use RFC3339"),
    }
}
fn range(q: &Query) -> Result<Range> {
    let tz: Tz = q.timezone.as_deref().unwrap_or("UTC").parse().context("invalid IANA timezone")?;
    let today = Utc::now().with_timezone(&tz).date_naive();
    let start = q
        .start
        .as_deref()
        .map(|v| boundary(v, tz))
        .transpose()?
        .unwrap_or(boundary(&(today - chrono::Duration::days(29)).to_string(), tz)?);
    let end = q
        .end
        .as_deref()
        .map(|v| boundary(v, tz))
        .transpose()?
        .unwrap_or(boundary(&(today + chrono::Duration::days(1)).to_string(), tz)?);
    if end <= start || end.saturating_sub(start) > MAX_QUERY_DAYS * 86_400_000 {
        bail!("range must be positive and no longer than 3660 days");
    }
    if q.group_by
        .as_deref()
        .is_some_and(|s| !matches!(s, "day" | "source" | "provider" | "model" | "account" | "client"))
    {
        bail!("unsupported group_by");
    }
    for s in [&q.provider, &q.model, &q.account, &q.client, &q.source].into_iter().flatten() {
        super::types::valid_label(s).map_err(|e| anyhow!(e))?;
    }
    Ok(Range { start, end, tz })
}
fn filter(q: &Query, r: &Range, alias: &str) -> (String, Vec<SqlValue>) {
    let mut sql = format!("{alias}.event_at_ms>=? AND {alias}.event_at_ms<?");
    let mut values = vec![r.start.into(), r.end.into()];
    for (column, value) in [
        ("provider", &q.provider),
        ("model", &q.model),
        ("account_id", &q.account),
        ("client_id", &q.client),
        ("source", &q.source),
    ] {
        if let Some(value) = value {
            sql.push_str(&format!(" AND {alias}.{column}=?"));
            values.push(value.clone().into());
        }
    }
    (sql, values)
}
const AGG: &str = "COUNT(*),COUNT(DISTINCT logical_request_id),COUNT(DISTINCT COALESCE(attempt_id,source||':'||source_event_id)),usage_sum(cost_nanos),SUM(cost_nanos IS NULL),SUM(completeness='partial'),SUM(completeness='missing'),usage_sum(input),usage_sum(cache_read),usage_sum(cache_write),usage_sum(write_5m),usage_sum(write_1h),usage_sum(output),usage_sum(reasoning),SUM(input IS NULL),SUM(cache_read IS NULL),SUM(cache_write IS NULL),SUM(output IS NULL),SUM(reasoning IS NULL),SUM(pricing_partial=1),SUM(logical_request_id IS NULL),COUNT(cost_nanos),COUNT(input),COUNT(cache_read),COUNT(cache_write),COUNT(write_5m),COUNT(write_1h),COUNT(output),COUNT(reasoning)";
fn aggregate(conn: &Connection, table: &str, where_sql: &str, values: &[SqlValue]) -> Result<Value> {
    Ok(conn.query_row(
        &format!("SELECT {AGG} FROM {table} o INDEXED BY {table}_event_time WHERE {where_sql}"),
        rusqlite::params_from_iter(values),
        |row| aggregate_row(row, 0),
    )?)
}
fn aggregate_row(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<Value> {
    let mut nums = Vec::new();
    for i in 0..29 {
        nums.push(row.get::<_, Option<i64>>(i + offset)?);
    }
    let mut tokens = serde_json::Map::new();
    for (i, name) in
        ["input", "cache_read", "cache_write", "write_5m", "write_1h", "output", "reasoning"].iter().enumerate()
    {
        tokens.insert((*name).into(), json!(nums[7 + i]));
    }
    let mut overflow_fields = Vec::new();
    for (i, (name, sum_index)) in [
        ("estimated_cost_nanos", 3),
        ("input", 7),
        ("cache_read", 8),
        ("cache_write", 9),
        ("write_5m", 10),
        ("write_1h", 11),
        ("output", 12),
        ("reasoning", 13),
    ]
    .into_iter()
    .enumerate()
    {
        if nums[21 + i].unwrap_or(0) > 0 && nums[sum_index].is_none() {
            overflow_fields.push(name);
        }
    }
    let known_cost = if overflow_fields.contains(&"estimated_cost_nanos") { None } else { Some(nums[3].unwrap_or(0)) };
    Ok(
        json!({"observations":nums[0].unwrap_or(0),"logical_requests":nums[1].unwrap_or(0),"attempts":nums[2].unwrap_or(0),"estimated_cost_nanos":nums[3],"known_cost_nanos":known_cost,"aggregation_overflow":!overflow_fields.is_empty(),"aggregation_overflow_fields":overflow_fields,"unpriced":nums[4].unwrap_or(0),"partial":nums[5].unwrap_or(0),"missing_usage":nums[6].unwrap_or(0),"tokens":tokens,"pricing_partial":nums[19].unwrap_or(0),"logical_requests_unknown":nums[20].unwrap_or(0),"missing_token_counts":{"input":nums[14].unwrap_or(0),"cache_read":nums[15].unwrap_or(0),"cache_write":nums[16].unwrap_or(0),"output":nums[17].unwrap_or(0),"reasoning":nums[18].unwrap_or(0)}}),
    )
}
fn local_date_expression(r: &Range) -> Result<String> {
    let offset_at = |ms| -> Result<i32> {
        Ok(DateTime::from_timestamp_millis(ms)
            .context("range timestamp out of bounds")?
            .with_timezone(&r.tz)
            .offset()
            .fix()
            .local_minus_utc())
    };
    let mut offset = offset_at(r.start)?;
    let mut clauses = String::new();
    let mut previous = r.start;
    let mut cursor = r.start;
    // Hourly sampling covers modern IANA transitions within the supported range.
    while cursor < r.end {
        cursor = (cursor + 3_600_000).min(r.end);
        let next = offset_at(cursor)?;
        if next != offset {
            let (mut low, mut high) = (previous, cursor);
            while high - low > 1 {
                let middle = low + (high - low) / 2;
                if offset_at(middle)? == offset {
                    low = middle;
                } else {
                    high = middle;
                }
            }
            clauses.push_str(&format!(" WHEN o.event_at_ms<{high} THEN {offset}"));
            offset = next;
        }
        previous = cursor;
    }
    let offset_sql = if clauses.is_empty() { offset.to_string() } else { format!("CASE{clauses} ELSE {offset} END") };
    Ok(format!("strftime('%Y-%m-%d',o.event_at_ms/1000+({offset_sql}),'unixepoch')"))
}

// Request lifecycle identities belong to the raw proxy journal. Canonical charge
// selection may merge an idempotent provider replay without merging client requests.
const REQUEST_COUNTS: &str = "COUNT(DISTINCT NULLIF(logical_request_id,'')),COUNT(DISTINCT NULLIF(attempt_id,'')),COUNT(DISTINCT CASE WHEN logical_request_id IS NULL OR logical_request_id='' THEN json_array(origin_id,source_event_id) END),COUNT(DISTINCT CASE WHEN attempt_id IS NULL OR attempt_id='' THEN json_array(origin_id,source_event_id) END)";
fn request_counts_row(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<Value> {
    Ok(
        json!({"logical_requests":row.get::<_,i64>(offset)?,"attempts":row.get::<_,i64>(offset+1)?,"logical_requests_unknown":row.get::<_,i64>(offset+2)?,"attempts_unknown":row.get::<_,i64>(offset+3)?}),
    )
}
fn raw_proxy_counts(conn: &Connection, where_sql: &str, values: &[SqlValue]) -> Result<Value> {
    Ok(conn.query_row(&format!("SELECT {REQUEST_COUNTS} FROM usage_observations o INDEXED BY usage_event_time WHERE {where_sql} AND o.source='proxy'"),rusqlite::params_from_iter(values),|row|request_counts_row(row,0))?)
}
fn apply_request_counts(value: &mut Value, counts: &Value) {
    for key in ["logical_requests", "attempts", "logical_requests_unknown", "attempts_unknown"] {
        value[key] = counts[key].clone();
    }
}
fn summary(conn: &mut Connection, q: &Query) -> Result<Value> {
    let r = range(q)?;
    let (where_sql, values) = filter(q, &r, "o");
    let mut proxy = aggregate(conn, "usage_entries", &format!("{where_sql} AND o.source='proxy'"), &values)?;
    let request_counts = raw_proxy_counts(conn, &where_sql, &values)?;
    apply_request_counts(&mut proxy, &request_counts);
    let conflicts:i64=conn.query_row(&format!("SELECT COUNT(*) FROM usage_entries o INDEXED BY usage_entries_event_time WHERE {where_sql} AND o.source='proxy' AND EXISTS(SELECT 1 FROM usage_observations x WHERE x.association_key=o.association_key AND ((x.model IS NOT NULL AND o.model IS NOT NULL AND x.model<>o.model) OR (x.account_id IS NOT NULL AND o.account_id IS NOT NULL AND x.account_id<>o.account_id) OR (x.input IS NOT NULL AND o.input IS NOT NULL AND x.input<>o.input) OR (x.cache_read IS NOT NULL AND o.cache_read IS NOT NULL AND x.cache_read<>o.cache_read) OR (x.cache_write IS NOT NULL AND o.cache_write IS NOT NULL AND x.cache_write<>o.cache_write) OR (x.output IS NOT NULL AND o.output IS NOT NULL AND x.output<>o.output)))"),rusqlite::params_from_iter(&values),|row|row.get(0))?;
    proxy["conflicts"] = json!(conflicts);
    let mut sources = Vec::new();
    for source in ["proxy", "claude_code", "codex"] {
        let mut source_values = values.clone();
        source_values.push(source.to_owned().into());
        let mut totals =
            aggregate(conn, "usage_source_entries", &format!("{where_sql} AND o.source=?"), &source_values)?;
        let records: i64 = conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM usage_observations o INDEXED BY usage_event_time WHERE {where_sql} AND o.source=?"
            ),
            rusqlite::params_from_iter(&source_values),
            |row| row.get(0),
        )?;
        let source_conflicts:i64=conn.query_row(&format!("SELECT COUNT(*) FROM usage_source_entries o INDEXED BY usage_source_entries_event_time WHERE {where_sql} AND o.source=? AND EXISTS(SELECT 1 FROM usage_observations x INDEXED BY usage_association WHERE x.association_key=o.association_key AND x.source=o.source AND ((x.model IS NOT NULL AND o.model IS NOT NULL AND x.model<>o.model) OR (x.account_id IS NOT NULL AND o.account_id IS NOT NULL AND x.account_id<>o.account_id) OR (x.input IS NOT NULL AND o.input IS NOT NULL AND x.input<>o.input) OR (x.cache_read IS NOT NULL AND o.cache_read IS NOT NULL AND x.cache_read<>o.cache_read) OR (x.cache_write IS NOT NULL AND o.cache_write IS NOT NULL AND x.cache_write<>o.cache_write) OR (x.output IS NOT NULL AND o.output IS NOT NULL AND x.output<>o.output)))"),rusqlite::params_from_iter(&source_values),|row|row.get(0))?;
        if source == "proxy" {
            apply_request_counts(&mut totals, &request_counts);
        }
        totals["conflicts"] = json!(source_conflicts);
        totals["source"] = json!(source);
        totals["source_record_count"] = json!(records);
        totals["possibly_overlapping"] = json!(source != "proxy");
        sources.push(totals);
    }
    // UTC offset transitions are derived from the timezone database. SQL groups
    // local dates in one pass without materializing event rows in application RAM.
    let date_expression = local_date_expression(&r)?;
    let mut trend = Vec::new();
    let mut statement=conn.prepare(&format!("SELECT {date_expression},o.source,{AGG} FROM usage_source_entries o INDEXED BY usage_source_entries_event_time WHERE {where_sql} GROUP BY 1,2 ORDER BY 1,2 LIMIT 10980"))?;
    let rows = statement.query_map(rusqlite::params_from_iter(&values), |row| {
        let mut value = aggregate_row(row, 2)?;
        value["date"] = json!(row.get::<_, String>(0)?);
        value["source"] = json!(row.get::<_, String>(1)?);
        Ok(value)
    })?;
    for row in rows {
        trend.push(row?);
    }
    let mut proxy_days = std::collections::BTreeMap::new();
    {
        let mut statement=conn.prepare(&format!("SELECT {date_expression},{REQUEST_COUNTS} FROM usage_observations o INDEXED BY usage_event_time WHERE {where_sql} AND o.source='proxy' GROUP BY 1 ORDER BY 1 LIMIT 3660"))?;
        let rows = statement.query_map(rusqlite::params_from_iter(&values), |row| {
            Ok((row.get::<_, String>(0)?, request_counts_row(row, 1)?))
        })?;
        for row in rows {
            let (date, counts) = row?;
            proxy_days.insert(date, counts);
        }
    }
    for day in &mut trend {
        if day["source"] == "proxy"
            && let Some(counts) = proxy_days.get(day["date"].as_str().unwrap_or(""))
        {
            apply_request_counts(day, counts);
        }
    }
    // A replay can fall on a later local day than its selected charge. Preserve the
    // lifecycle-only day even when canonical accounting has no entry for that day.
    for (date, counts) in proxy_days {
        if !trend.iter().any(|day| day["source"] == "proxy" && day["date"] == date) {
            let mut empty = aggregate(conn, "usage_source_entries", "0", &[])?;
            empty["date"] = json!(date);
            empty["source"] = json!("proxy");
            apply_request_counts(&mut empty, &counts);
            trend.push(empty);
        }
    }
    trend.sort_by(|a, b| {
        a["date"].as_str().cmp(&b["date"].as_str()).then(a["source"].as_str().cmp(&b["source"].as_str()))
    });
    let mut breakdowns = serde_json::Map::new();
    for (name, column) in
        [("provider", "provider"), ("model", "model"), ("account", "account_id"), ("client", "client_id")]
    {
        let mut statement=conn.prepare(&format!("SELECT o.{column},o.source,{AGG} FROM usage_source_entries o INDEXED BY usage_source_entries_event_time WHERE {where_sql} GROUP BY 1,2 ORDER BY COUNT(*) DESC,1,2 LIMIT 500"))?;
        let rows = statement.query_map(rusqlite::params_from_iter(&values), |row| {
            let mut value = aggregate_row(row, 2)?;
            value["id"] = json!(row.get::<_, Option<String>>(0)?);
            value["source"] = json!(row.get::<_, String>(1)?);
            Ok(value)
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        let mut raw_statement=conn.prepare(&format!("SELECT o.{column},{REQUEST_COUNTS} FROM usage_observations o INDEXED BY usage_event_time WHERE {where_sql} AND o.source='proxy' GROUP BY 1 ORDER BY COUNT(*) DESC,1 LIMIT 500"))?;
        let rows = raw_statement.query_map(rusqlite::params_from_iter(&values), |row| {
            Ok((row.get::<_, Option<String>>(0)?, request_counts_row(row, 1)?))
        })?;
        for row in rows {
            let (id, counts) = row?;
            if let Some(group) = out.iter_mut().find(|v| v["source"] == "proxy" && v["id"] == json!(id)) {
                apply_request_counts(group, &counts);
            } else {
                let mut group = aggregate(conn, "usage_source_entries", "0", &[])?;
                group["id"] = json!(id);
                group["source"] = json!("proxy");
                apply_request_counts(&mut group, &counts);
                out.push(group);
            }
        }
        breakdowns.insert(name.into(), json!(out));
    }
    let mut facets = serde_json::Map::new();
    for (name, column, labelled) in [
        ("providers", "provider", false),
        ("models", "model", false),
        ("accounts", "account_id", true),
        ("clients", "client_id", true),
        ("sources", "source", false),
    ] {
        let mut statement=conn.prepare(&format!("SELECT DISTINCT o.{column} FROM usage_entries o INDEXED BY usage_entries_event_time WHERE {where_sql} AND o.{column} IS NOT NULL ORDER BY o.{column} LIMIT 500"))?;
        let rows = statement.query_map(rusqlite::params_from_iter(&values), |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for value in rows {
            let value = value?;
            out.push(if labelled { json!({"id":value,"label":value}) } else { json!(value) });
        }
        facets.insert(name.into(), json!(out));
    }
    let raw: String = conn.query_row("SELECT value FROM usage_meta WHERE key='catalogue'", [], |row| row.get(0))?;
    let catalogue: Catalogue = serde_json::from_str(&raw)?;
    Ok(
        json!({"range":{"start":r.start,"end":r.end,"timezone":r.tz.to_string()},"proxy":proxy,"sources":sources,"trend":trend,"group_by":q.group_by.as_deref().unwrap_or("day"),"breakdowns":breakdowns,"facets":facets,"pricing":{"version":catalogue.version,"verified_at":catalogue.verified_at,"basis":"API list-price equivalent estimate; source totals may overlap; historical undocumented periods unpriced"},"reconciliation":{"basis":"proxy evidence only","cross_source_grand_total":null,"identity":"provider response ID only; native stable source event ID within source","conflicts":conflicts}}),
    )
}
fn details(conn: &mut Connection, q: &Query) -> Result<Value> {
    let r = range(q)?;
    let (where_sql, mut values) = filter(q, &r, "o");
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let offset = q.offset.unwrap_or(0);
    if offset > 1_000_000 {
        bail!("offset exceeds pagination bound");
    }
    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM usage_observations o INDEXED BY usage_event_time WHERE {where_sql}"),
        rusqlite::params_from_iter(&values),
        |row| row.get(0),
    )?;
    values.push(i64::from(limit).into());
    values.push(i64::from(offset).into());
    let mut statement=conn.prepare(&format!("SELECT o.id,o.payload,o.cost_nanos,o.pricing_basis,o.snapshot_json,o.canonical_key,o.ingested_at_ms,EXISTS(SELECT 1 FROM usage_suppressed s WHERE s.source=o.source AND s.source_event_id=o.source_event_id AND (s.origin_id='' OR s.origin_id=o.origin_id)) FROM usage_observations o INDEXED BY usage_event_time WHERE {where_sql} ORDER BY o.event_at_ms DESC,o.id DESC LIMIT ? OFFSET ?"))?;
    let rows = statement.query_map(rusqlite::params_from_iter(&values), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, bool>(7)?,
        ))
    })?;
    let mut items = Vec::new();
    for row in rows {
        let (id, payload, cost, basis, snapshot, key, ingested, suppressed) = row?;
        let mut value: Value = serde_json::from_str(&payload)?;
        value["id"] = json!(id);
        value["ingested_at_ms"] = json!(ingested);
        value["estimated_cost_nanos"] = json!(cost);
        value["pricing_basis"] = json!(basis);
        value["pricing_snapshot"] = serde_json::from_str(&snapshot)?;
        value["canonical_key"] = json!(key);
        value["superseded"] = json!(suppressed);
        items.push(value);
    }
    Ok(json!({"items":items,"total":total,"limit":limit,"offset":offset}))
}
#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
