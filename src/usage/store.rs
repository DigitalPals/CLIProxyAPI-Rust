//! Durable metadata-only usage journal and deterministic, conservative accounting.
use super::{pricing::Catalogue, types::Observation};
use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, LocalResult, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use rusqlite::{Connection, OptionalExtension, params, types::Value as SqlValue};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
    },
};
use tokio::sync::{OnceCell, Semaphore, mpsc, oneshot};
mod dashboard;
mod read;
pub(crate) use read::ReadError;
#[cfg(test)]
mod reference;

pub(super) const VERSION: i64 = 3;
pub(super) const APPLICATION_ID: i64 = 0x46555345;
const MAX_QUERY_DAYS: i64 = 3660;
const ENTRY_COLUMNS: &str = "id,source,origin_id,source_event_id,canonical_key,association_key,event_at_ms,provider,model,account_id,client_id,logical_request_id,attempt_id,response_id,completeness,input,cache_read,cache_write,write_5m,write_1h,output,reasoning,cost_nanos,pricing_basis,catalogue_version,json_extract(snapshot_json,'$.partial') AS pricing_partial";
const WINNER_ORDER: &str = "trust_rank,CASE completeness WHEN 'complete' THEN 0 WHEN 'partial' THEN 1 ELSE 2 END,known_fields DESC,COALESCE(output,-1) DESC,source,origin_id,source_event_id,fingerprint";
type Job = Box<dyn FnOnce(&mut Connection) + Send>;
enum Command {
    Observe(Box<Observation>),
    Call(Job),
    /// Leave the worker loop, close the connection, then reply.
    Stop(oneshot::Sender<std::result::Result<(), String>>),
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
    gate: RwLock<()>,
}
#[derive(Clone)]
pub struct Store {
    tx: mpsc::Sender<Command>,
    health: Arc<Health>,
    path: Arc<PathBuf>,
    read_slots: Arc<Semaphore>,
    shutdown_result: Arc<OnceCell<std::result::Result<(), String>>>,
    session_id: Arc<String>,
    startup_repriced: u64,
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
    /// Observation rows: `raw` (default, every stored record) or `combined` (one per response).
    pub view: Option<String>,
    /// Combined trend grouping: `provider` (default) or `model`.
    pub stack: Option<String>,
}
#[cfg(test)]
#[derive(Default)]
struct CheckedSum;
#[cfg(test)]
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
#[cfg(test)]
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
        Self::start(path, queue_capacity, retention_days, catalogue, None)
    }
    /// Standalone outboxes install their allocation bound before migrations or
    /// writer-session startup can allocate pages. They never switch through WAL.
    pub(crate) fn open_collector(path: &Path, byte_limit: u64) -> Result<Self> {
        Self::start(path, 128, 3650, Catalogue::load(None)?, Some(byte_limit))
    }
    /// Admin/import access preserves the configured retention horizon and catalogue.
    pub fn open_existing(path: &Path, queue_capacity: usize) -> Result<Self> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let retention: String =
            conn.query_row("SELECT value FROM usage_meta WHERE key='retention_days'", [], |r| r.get(0))?;
        let raw: String = conn.query_row("SELECT value FROM usage_meta WHERE key='catalogue'", [], |r| r.get(0))?;
        let catalogue: Catalogue = serde_json::from_str(&raw)?;
        Self::start(path, queue_capacity, retention.parse().context("stored retention invalid")?, catalogue, None)
    }
    fn start(
        path: &Path,
        queue_capacity: usize,
        retention_days: u32,
        catalogue: Catalogue,
        byte_limit: Option<u64>,
    ) -> Result<Self> {
        if !(1..=65_536).contains(&queue_capacity) || !(1..=36_600).contains(&retention_days) {
            bail!("invalid stored usage settings");
        }
        if path.as_os_str().is_empty() || path == Path::new(":memory:") {
            bail!("usage requires a persistent database path");
        }
        let mut conn = Connection::open(path).context("open usage database")?;
        #[cfg(test)]
        register_aggregates(&conn)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")?;
        if let Some(limit) = byte_limit {
            let page_size: u64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
            let pages: u64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
            if limit < page_size || pages > limit / page_size {
                bail!("collector database exceeds size limit; pending outbox preserved");
            }
            conn.pragma_update(None, "max_page_count", limit / page_size)?;
            let effective: u64 = conn.query_row("PRAGMA max_page_count", [], |r| r.get(0))?;
            if effective > limit / page_size {
                bail!("collector database exceeds size limit; pending outbox preserved");
            }
            let mode: String = conn.query_row("PRAGMA journal_mode=DELETE", [], |r| r.get(0))?;
            if mode != "delete" {
                bail!("collector journal mode unavailable");
            }
        }
        migrate(&mut conn)?;
        if byte_limit.is_none() {
            conn.execute_batch("PRAGMA journal_mode=WAL;")?;
            // Bounded collector outboxes never serve dashboards. Keep their
            // compact timestamp index and allocation budget unchanged.
            upgrade_dashboard_index(&mut conn)?;
        }
        conn.execute("INSERT INTO usage_meta(key,value) VALUES('catalogue',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[serde_json::to_string(&catalogue)?])?;
        conn.execute("INSERT INTO usage_meta(key,value) VALUES('retention_days',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[retention_days.to_string()])?;
        let cutoff = (Utc::now().timestamp_millis() - i64::from(retention_days) * 86_400_000).max(0);
        purge_conn(&mut conn, cutoff)?;
        // Collector outboxes only forward evidence; the server prices it on ingestion.
        let startup_repriced = if byte_limit.is_none() { reprice(&mut conn)? } else { 0 };
        if startup_repriced > 0 {
            tracing::info!(repriced = startup_repriced, "updated previously unpriced usage estimates");
        }
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
                let stop = loop {
                    let received = runtime
                        .block_on(async { tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await });
                    let command = match received {
                        Ok(Some(command)) => Some(command),
                        Ok(None) => {
                            let _ = persist_health(&conn, &worker_session, &worker_health);
                            break None;
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
                        Command::Stop(reply) => break Some(reply),
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
                            match pending {
                                Some(Command::Call(f)) => f(&mut conn),
                                Some(Command::Stop(reply)) => break Some(reply),
                                _ => {}
                            }
                        }
                    }
                };
                // Refuse later commands, then close before replying. The last connection to
                // close checkpoints the WAL into the database file, so a shut down store must
                // not still be writing it when the caller reads, copies or reopens the file.
                drop(rx);
                let closed = conn.close().map_err(|(_, e)| format!("close usage database: {e}"));
                if let Some(reply) = stop {
                    let _ = reply.send(closed);
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
            startup_repriced,
        })
    }
    /// Rows the startup reprice step updated when this handle opened the database.
    pub fn startup_repriced(&self) -> u64 {
        self.startup_repriced
    }
    pub fn enqueue(&self, observation: Observation) -> bool {
        if self.health.closed.load(Ordering::Acquire) {
            return false;
        }
        let Ok(_guard) = self.health.gate.try_read() else {
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
    /// Close observation ingress, drain prior commands durably, mark this writer clean,
    /// then stop the writer and close its connection. Once this returns the store no longer
    /// touches the database file. Concurrent shutdown callers share the same acknowledged result.
    pub async fn shutdown(&self) -> Result<()> {
        let result = self
            .shutdown_result
            .get_or_init(|| async {
                {
                    let _gate = self.health.gate.write().unwrap();
                    self.health.closed.store(true, Ordering::Release);
                }
                let id = self.session_id.clone();
                let health = self.health.clone();
                let clean = self
                    .call_internal(move |conn| {
                        persist_health(conn, &id, &health)?;
                        conn.execute(
                            "UPDATE usage_writer_sessions SET clean=1,ended_at_ms=?2 WHERE id=?1",
                            params![id.as_str(), Utc::now().timestamp_millis()],
                        )?;
                        Ok(())
                    })
                    .await
                    .map_err(|e| e.to_string());
                // Stop even when the clean mark failed: the connection must close either way.
                let (tx, rx) = oneshot::channel();
                let closed = match self.tx.send(Command::Stop(tx)).await {
                    Ok(()) => rx.await.unwrap_or_else(|_| Err("usage writer stopped before closing".into())),
                    Err(_) => Err("usage writer stopped".into()),
                };
                clean.and(closed)
            })
            .await;
        result.clone().map_err(|e| anyhow!(e))
    }
    #[cfg(test)]
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
        read::run(self, f).await
    }
    /// Independent SQL reference and lifecycle diagnostics; never part of a production build.
    #[cfg(test)]
    pub async fn reference_summary(&self, q: Query) -> Result<Value> {
        self.read(move |conn| reference::summary(conn, &q)).await
    }
    /// Dashboard metrics share one exact WAL snapshot without the legacy source/lifecycle scans.
    pub async fn dashboard(&self, q: Query) -> Result<Value> {
        self.read(move |conn| dashboard::summary(conn, &q)).await
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
    if version < 0 {
        bail!("unsupported usage database migration");
    }
    if version == 0 {
        let tables: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )?;
        if tables > 0 {
            bail!("refusing to initialize usage schema in a nonempty unversioned database");
        }
    }
    // Each step upgrades exactly one version inside one transaction: 0→1→2→3.
    let tx = conn.transaction()?;
    if version < 1 {
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
    }
    if version < 2 {
        tx.execute_batch(WRITER_SESSIONS_SCHEMA)?;
    }
    if version < 3 {
        // Combined accounting drops imported copies of proxy responses by association key.
        tx.execute_batch("CREATE INDEX usage_entries_association ON usage_entries(association_key,source);")?;
    }
    tx.pragma_update(None, "user_version", VERSION)?;
    tx.commit()?;
    Ok(())
}

/// Keep the existing index name and schema version: older binaries can still
/// read this layout, including queries that explicitly name the timestamp index.
/// The extra scalar columns let facets filter and deduplicate without fetching
/// every observation's much larger payload/pricing pages.
fn upgrade_dashboard_index(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction()?;
    let partial: Option<bool> = tx
        .query_row(
            "SELECT partial FROM pragma_index_list('usage_observations') WHERE name='usage_event_time'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let columns = tx
        .prepare("PRAGMA index_info(usage_event_time)")?
        .query_map([], |row| row.get::<_, String>(2))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if partial != Some(false) {
        bail!("usage timestamp index is missing or has an unexpected shape");
    }
    if columns == ["event_at_ms", "provider", "model", "account_id", "client_id", "origin_id", "source"] {
        tx.commit()?;
        return Ok(());
    }
    if columns != ["event_at_ms"] {
        bail!("usage timestamp index has an unexpected shape");
    }
    // Both statements run in one transaction. A failed build restores
    // the previous index, preserving the journal and rollback compatibility.
    tx.execute_batch(
        "DROP INDEX usage_event_time;
         CREATE INDEX usage_event_time ON usage_observations(event_at_ms,provider,model,account_id,client_id,origin_id,source);",
    )?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
#[path = "store/index_tests.rs"]
mod index_tests;

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
/// Reselect the global and per-source accounting entry for one canonical group.
pub(super) fn rebuild_entries(conn: &Connection, key: &str, source: &str) -> Result<()> {
    conn.prepare_cached(&format!("INSERT OR REPLACE INTO usage_entries SELECT {ENTRY_COLUMNS} FROM usage_observations INDEXED BY usage_canonical WHERE canonical_key=?1 AND NOT EXISTS(SELECT 1 FROM usage_suppressed s WHERE s.source=usage_observations.source AND s.source_event_id=usage_observations.source_event_id AND (s.origin_id='' OR s.origin_id=usage_observations.origin_id)) ORDER BY {WINNER_ORDER} LIMIT 1"))?.execute([key])?;
    conn.prepare_cached(&format!("INSERT OR REPLACE INTO usage_source_entries SELECT {ENTRY_COLUMNS} FROM usage_observations INDEXED BY usage_canonical WHERE canonical_key=?1 AND source=?2 AND NOT EXISTS(SELECT 1 FROM usage_suppressed s WHERE s.source=usage_observations.source AND s.source_event_id=usage_observations.source_event_id AND (s.origin_id='' OR s.origin_id=usage_observations.origin_id)) ORDER BY {WINNER_ORDER} LIMIT 1"))?.execute(params![key,source])?;
    Ok(())
}
/// Retry unpriced observations affected by corrected date/region semantics.
/// Rows that already carry a price are never touched; safe to repeat.
pub fn reprice(conn: &mut Connection) -> Result<u64> {
    let raw: String = conn.query_row("SELECT value FROM usage_meta WHERE key='catalogue'", [], |r| r.get(0))?;
    let catalogue: Catalogue = serde_json::from_str(&raw)?;
    let (mut cursor, mut repriced) = (0_i64, 0_u64);
    loop {
        // Take the write lock first: a read that later upgrades fails at once (SQLITE_BUSY_SNAPSHOT)
        // when another connection wrote in between, where waiting for the lock just works.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let rows = tx.prepare("SELECT id,payload,canonical_key,source,pricing_basis FROM usage_observations WHERE id>?1 AND cost_nanos IS NULL AND pricing_basis IN ('outside_effective_period','local_override:outside_effective_period','unsupported_inference_region','local_override:unsupported_inference_region') ORDER BY id LIMIT 2000")?.query_map([cursor], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let Some(last) = rows.last() else { break };
        cursor = last.0;
        let mut groups = BTreeSet::new();
        for (id, payload, key, source, basis) in rows {
            // Payloads written by an incompatible build stay as stored rather than guessed.
            let Ok(o) = serde_json::from_str::<Observation>(&payload) else { continue };
            let snapshot = catalogue.price(&o);
            if snapshot.cost_nanos.is_none() && snapshot.basis == basis {
                continue;
            }
            tx.execute("UPDATE usage_observations SET cost_nanos=?2,pricing_basis=?3,catalogue_version=?4,snapshot_json=?5 WHERE id=?1 AND cost_nanos IS NULL",params![id,snapshot.cost_nanos,snapshot.basis,snapshot.catalogue_version,serde_json::to_string(&snapshot)?])?;
            groups.insert((key, source));
            repriced += 1;
        }
        for (key, source) in groups {
            rebuild_entries(&tx, &key, &source)?;
        }
        tx.commit()?;
    }
    Ok(repriced)
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
        rebuild_entries(&tx, &key, &source)?;
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
        rebuild_entries(&tx, key, source)?;
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
        rebuild_entries(&tx, &key, &source)?;
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
    if q.view.as_deref().is_some_and(|s| !matches!(s, "combined" | "raw")) {
        bail!("unsupported view");
    }
    if q.stack.as_deref().is_some_and(|s| !matches!(s, "provider" | "model")) {
        bail!("unsupported stack");
    }
    for s in [&q.provider, &q.model, &q.account, &q.client, &q.source].into_iter().flatten() {
        super::types::valid_label(s).map_err(|e| anyhow!(e))?;
    }
    Ok(Range { start, end, tz })
}
fn dimension(column: &str, alias: &str) -> String {
    if column == "client_id" {
        format!("COALESCE({alias}.client_id,CASE WHEN {alias}.origin_id LIKE 'collector:%' THEN {alias}.origin_id END)")
    } else {
        format!("{alias}.{column}")
    }
}
/// Combined accounting: one entry per provider response. An imported entry is dropped when
/// any proxy entry shares its association key, even outside the range, so the trusted
/// proxy evidence wins and a pair split across local midnight still counts once.
fn combined(where_sql: &str) -> String {
    format!(
        "{where_sql} AND (o.source='proxy' OR NOT EXISTS(SELECT 1 FROM usage_entries p INDEXED BY usage_entries_association WHERE p.association_key=o.association_key AND p.source='proxy'))"
    )
}
fn collector_label(conn: &Connection, origin: &str) -> Result<Option<String>> {
    let Some(id) = origin.strip_prefix("collector:") else { return Ok(None) };
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='usage_collectors')",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(None);
    }
    Ok(conn.query_row("SELECT label FROM usage_collectors WHERE id=?1", [id], |r| r.get(0)).optional()?)
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
            sql.push_str(&format!(" AND {}=?", dimension(column, alias)));
            values.push(value.clone().into());
        }
    }
    (sql, values)
}
fn details(conn: &mut Connection, q: &Query) -> Result<Value> {
    let r = range(q)?;
    let (where_sql, mut values) = filter(q, &r, "o");
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let offset = q.offset.unwrap_or(0);
    if offset > 1_000_000 {
        bail!("offset exceeds pagination bound");
    }
    // Combined rows are accounting entries joined back to their stored observation.
    let combined_view = q.view.as_deref() == Some("combined");
    let (table, join, x, where_sql) = if combined_view {
        (
            "usage_entries o INDEXED BY usage_entries_event_time",
            " JOIN usage_observations x ON x.id=o.id",
            "x",
            combined(&where_sql),
        )
    } else {
        ("usage_observations o INDEXED BY usage_event_time", "", "o", where_sql)
    };
    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE {where_sql}"),
        rusqlite::params_from_iter(&values),
        |row| row.get(0),
    )?;
    values.push(i64::from(limit).into());
    values.push(i64::from(offset).into());
    let mut statement=conn.prepare(&format!("SELECT {x}.id,{x}.payload,{x}.cost_nanos,{x}.pricing_basis,{x}.snapshot_json,{x}.canonical_key,{x}.ingested_at_ms,EXISTS(SELECT 1 FROM usage_suppressed s WHERE s.source={x}.source AND s.source_event_id={x}.source_event_id AND (s.origin_id='' OR s.origin_id={x}.origin_id)),{x}.association_key FROM {table}{join} WHERE {where_sql} ORDER BY o.event_at_ms DESC,o.id DESC LIMIT ? OFFSET ?"))?;
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
            row.get::<_, String>(8)?,
        ))
    })?;
    let mut items = Vec::new();
    for row in rows {
        read::check()?;
        let (id, payload, cost, basis, snapshot, key, ingested, suppressed, association) = row?;
        let mut value: Value = serde_json::from_str(&payload)?;
        value["id"] = json!(id);
        value["ingested_at_ms"] = json!(ingested);
        value["estimated_cost_nanos"] = json!(cost);
        value["pricing_basis"] = json!(basis);
        value["pricing_snapshot"] = serde_json::from_str(&snapshot)?;
        value["canonical_key"] = json!(key);
        value["superseded"] = json!(suppressed);
        if let Some(origin) = value["origin_id"].as_str()
            && let Some(label) = collector_label(conn, origin)?
        {
            value["collector_label"] = json!(label);
        }
        if combined_view {
            let source = value["source"].as_str().unwrap_or("").to_owned();
            let origin = value["origin_id"].as_str().unwrap_or("").to_owned();
            value["matched_sources"] = if source == "proxy" {
                let mut statement = conn.prepare_cached("SELECT DISTINCT source FROM usage_entries INDEXED BY usage_entries_association WHERE association_key=?1 AND source<>'proxy' ORDER BY source")?;
                json!(
                    statement
                        .query_map([&association], |r| r.get::<_, String>(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?
                )
            } else {
                json!([])
            };
            value["origin_label"] = match (source.as_str(), origin.as_str()) {
                ("proxy", _) => Value::Null,
                (_, "local") => json!("This server"),
                _ => json!(value["collector_label"].as_str()),
            };
        }
        items.push(value);
    }
    read::check()?;
    Ok(json!({"items":items,"total":total,"limit":limit,"offset":offset}))
}
#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
