//! Standalone metadata-only collector and separately authenticated server ingestion.
use super::{
    imports,
    store::{self, Store},
    types::{Observation, valid_label},
};
use anyhow::{Result, anyhow, bail};
use axum::{
    Json,
    body::Bytes,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use clap::Subcommand;
use rand::RngCore;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;

const MAX_BATCH: usize = 200;
const MAX_BODY: usize = 512 * 1024;
const REQUESTS_PER_MINUTE: u64 = 120;
const CREDENTIAL_PREFIX: &str = "fbxc_";
const MAX_PROGRESS_COUNT: u64 = 1_000_000_000_000;
const MAX_LOCAL_DATABASE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_LOCAL_SQLITE_BYTES: u64 = 130 * 1024 * 1024;
#[derive(Debug, Subcommand)]
pub enum CollectorCommand {
    /// Install a server-issued collector credential; stdin is used without --credential-file.
    Enroll {
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        destination: String,
        #[arg(long)]
        credential_file: Option<PathBuf>,
        #[arg(long)]
        codex_root: Vec<PathBuf>,
        #[arg(long)]
        claude_root: Vec<PathBuf>,
    },
    /// Scan consented roots and deliver metadata, without loading proxy configuration.
    Run {
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        once: bool,
    },
    Status {
        #[arg(long)]
        state_dir: PathBuf,
    },
    Sync {
        #[arg(long)]
        state_dir: PathBuf,
    },
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalState {
    version: u32,
    id: String,
    destination: String,
    credential: String,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Batch {
    pub version: u32,
    pub observations: Vec<Observation>,
    #[serde(default)]
    pub pending: u64,
    #[serde(default)]
    pub superseded: Vec<CounterSupersession>,
    #[serde(default)]
    pub progress: Vec<SourceProgress>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressSource {
    Codex,
    ClaudeCode,
}
impl ProgressSource {
    fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude_code",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressState {
    Ready,
    Attention,
    Disabled,
    Unknown,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceProgress {
    pub source: ProgressSource,
    pub state: ProgressState,
    pub imported: u64,
    pub duplicate: u64,
    pub skipped: u64,
    pub unsupported: u64,
    pub failed: u64,
    pub last_scan_at_ms: Option<i64>,
}
impl SourceProgress {
    fn valid(&self) -> bool {
        [self.imported, self.duplicate, self.skipped, self.unsupported, self.failed]
            .into_iter()
            .all(|v| v <= MAX_PROGRESS_COUNT)
            && self
                .last_scan_at_ms
                .is_none_or(|at| (1_577_836_800_000..=chrono::Utc::now().timestamp_millis() + 86_400_000).contains(&at))
            && (self.state != ProgressState::Ready || self.last_scan_at_ms.is_some())
    }
}
fn source_progress(status: &Value) -> Result<Vec<SourceProgress>> {
    let roots = status["imports"].as_array().ok_or_else(|| anyhow!("import progress unavailable"))?;
    [ProgressSource::Codex, ProgressSource::ClaudeCode]
        .into_iter()
        .map(|source| {
            let selected: Vec<_> = roots.iter().filter(|root| root["source"].as_str() == Some(source.name())).collect();
            let enabled: Vec<_> = selected.iter().filter(|root| root["enabled"].as_bool() == Some(true)).collect();
            let count = |name: &str| -> Result<u64> {
                selected.iter().try_fold(0_u64, |sum, root| {
                    sum.checked_add(root[name].as_u64().unwrap_or(0))
                        .filter(|sum| *sum <= MAX_PROGRESS_COUNT)
                        .ok_or_else(|| anyhow!("import progress exceeds bounds"))
                })
            };
            let progress = SourceProgress {
                source,
                state: if selected.is_empty() {
                    ProgressState::Unknown
                } else if enabled.is_empty() {
                    ProgressState::Disabled
                } else if enabled.iter().any(|root| root["last_error"].is_string()) {
                    ProgressState::Attention
                } else if enabled.iter().any(|root| root["last_scan_at_ms"].as_i64().is_none()) {
                    ProgressState::Unknown
                } else {
                    ProgressState::Ready
                },
                imported: count("imported")?,
                duplicate: count("duplicate")?,
                skipped: count("skipped")?,
                unsupported: count("unsupported")?,
                failed: count("failed")?,
                last_scan_at_ms: selected.iter().filter_map(|root| root["last_scan_at_ms"].as_i64()).max(),
            };
            if !progress.valid() {
                bail!("invalid import progress")
            }
            Ok(progress)
        })
        .collect()
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CounterSupersession {
    pub source_event_id: String,
}
fn valid_supersession(id: &str) -> bool {
    id.len() == 72 && id.starts_with("counter:") && id[8..].bytes().all(|b| b.is_ascii_hexdigit())
}

fn credential() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    format!("{CREDENTIAL_PREFIX}{}", hex::encode(bytes))
}
fn valid_credential(value: &str) -> bool {
    value.len() == 69 && value.starts_with(CREDENTIAL_PREFIX) && value[5..].bytes().all(|b| b.is_ascii_hexdigit())
}
fn credential_hash(value: &str) -> String {
    imports::hash(value.as_bytes())
}
fn id_ok(id: &str) -> Result<()> {
    Uuid::parse_str(id).map_err(|_| anyhow!("invalid collector identity"))?;
    Ok(())
}
async fn init(store: &Store) -> Result<()> {
    store.call(|conn| {conn.execute_batch("CREATE TABLE IF NOT EXISTS usage_collectors(id TEXT PRIMARY KEY,label TEXT NOT NULL,credential_hash TEXT NOT NULL UNIQUE,revoked INTEGER NOT NULL DEFAULT 0,last_contact_at_ms INTEGER,last_sync_at_ms INTEGER,pending INTEGER NOT NULL DEFAULT 0,covered_sources TEXT NOT NULL DEFAULT '[]',time_start_ms INTEGER,time_end_ms INTEGER,rate_minute INTEGER NOT NULL DEFAULT 0,rate_count INTEGER NOT NULL DEFAULT 0);
    CREATE TABLE IF NOT EXISTS usage_collector_progress(collector_id TEXT NOT NULL,source TEXT NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(collector_id,source));")?;Ok(())}).await
}
pub async fn enroll(store: &Store, label: String) -> Result<Value> {
    valid_label(&label).map_err(|_| anyhow!("invalid collector label"))?;
    if label.trim().is_empty() {
        bail!("collector label required")
    }
    init(store).await?;
    let id = Uuid::new_v4().to_string();
    let token = credential();
    let digest = credential_hash(&token);
    let response = json!({"collector":{"id":id,"label":label},"credential":token});
    store
        .call(move |conn| {
            conn.execute(
                "INSERT INTO usage_collectors(id,label,credential_hash) VALUES(?1,?2,?3)",
                params![id, label, digest],
            )?;
            Ok(())
        })
        .await?;
    Ok(response)
}
pub async fn rotate(store: &Store, id: String) -> Result<Value> {
    id_ok(&id)?;
    init(store).await?;
    let token = credential();
    let digest = credential_hash(&token);
    let collector = store
        .call(move |conn| {
            let label: Option<String> = conn
                .query_row("SELECT label FROM usage_collectors WHERE id=?1 AND revoked=0", [&id], |r| r.get(0))
                .optional()?;
            let Some(label) = label else { bail!("active collector not found") };
            conn.execute("UPDATE usage_collectors SET credential_hash=?2 WHERE id=?1", params![id, digest])?;
            Ok(json!({"id":id,"label":label}))
        })
        .await?;
    Ok(json!({"collector":collector,"credential":token}))
}
pub async fn revoke(store: &Store, id: String) -> Result<Value> {
    id_ok(&id)?;
    init(store).await?;
    store
        .call(move |conn| {
            if conn.execute("UPDATE usage_collectors SET revoked=1 WHERE id=?1", [id])? == 0 {
                bail!("collector not found")
            };
            Ok(())
        })
        .await?;
    status(store).await
}
pub async fn status(store: &Store) -> Result<Value> {
    init(store).await?;
    let collectors=store.call(|conn|{
        let mut stmt=conn.prepare("SELECT id,label,revoked,last_contact_at_ms,last_sync_at_ms,pending,covered_sources,time_start_ms,time_end_ms FROM usage_collectors ORDER BY label,id")?;
        let rows=stmt.query_map([],|r|{let revoked:bool=r.get(2)?;let last:Option<i64>=r.get(3)?;let synced:Option<i64>=r.get(4)?;let now=chrono::Utc::now().timestamp_millis();Ok(json!({"id":r.get::<_,String>(0)?,"label":r.get::<_,String>(1)?,"revoked":revoked,"last_contact_at_ms":last,"last_sync_at_ms":synced,"pending":r.get::<_,u64>(5)?,"state":if revoked{"revoked"}else if last.is_none(){"enrolled"}else if last.is_some_and(|t|now-t>120000){"offline"}else if synced.is_none(){"contacted"}else{"synced"},"covered_sources":serde_json::from_str::<Value>(&r.get::<_,String>(6)?).unwrap_or(json!([])),"time_start_ms":r.get::<_,Option<i64>>(7)?,"time_end_ms":r.get::<_,Option<i64>>(8)?}))})?;
        let mut rows=rows.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut progress=conn.prepare("SELECT payload FROM usage_collector_progress WHERE collector_id=?1 ORDER BY source")?;
        for collector in &mut rows {
            let reports=progress.query_map([collector["id"].as_str().unwrap_or("")],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            collector["progress"]=serde_json::to_value(reports.into_iter().map(|v|serde_json::from_str::<SourceProgress>(&v)).collect::<serde_json::Result<Vec<_>>>()?)?;
        }
        Ok(rows)
    }).await?;
    Ok(json!({"collectors":collectors}))
}
fn response(status: StatusCode, code: &str) -> Response {
    (status, Json(json!({"error":code}))).into_response()
}

pub async fn ingest(store: &Store, headers: HeaderMap, body: Bytes) -> Response {
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");
    if !valid_credential(bearer) {
        return response(StatusCode::UNAUTHORIZED, "collector credential required");
    }
    if body.len() > MAX_BODY {
        return response(StatusCode::PAYLOAD_TOO_LARGE, "batch exceeds limit");
    }
    if init(store).await.is_err() {
        return response(StatusCode::SERVICE_UNAVAILABLE, "usage storage unavailable");
    }
    let digest = credential_hash(bearer);
    let lookup_digest = digest.clone();
    // Contact and request-rate counters are separate from a successfully committed sync.
    let identity = store
        .call(move |conn| {
            let row: Option<(String, u64, u64)> = conn
                .query_row(
                    "SELECT id,rate_minute,rate_count FROM usage_collectors WHERE credential_hash=?1 AND revoked=0",
                    [lookup_digest],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let Some((id, minute, count)) = row else { return Ok(None) };
            let now = chrono::Utc::now().timestamp_millis();
            let current = (now / 60000) as u64;
            let count = if minute == current { count + 1 } else { 1 };
            conn.execute(
                "UPDATE usage_collectors SET last_contact_at_ms=?2,rate_minute=?3,rate_count=?4 WHERE id=?1",
                params![id, now, current, count],
            )?;
            Ok(Some((id, count <= REQUESTS_PER_MINUTE)))
        })
        .await;
    let id = match identity {
        Ok(Some((id, true))) => id,
        Ok(Some((_, false))) => return response(StatusCode::TOO_MANY_REQUESTS, "collector rate limit"),
        Ok(None) => return response(StatusCode::UNAUTHORIZED, "collector credential invalid"),
        Err(_) => return response(StatusCode::SERVICE_UNAVAILABLE, "usage storage unavailable"),
    };
    let mut batch: Batch = match serde_json::from_slice(&body) {
        Ok(batch) => batch,
        Err(_) => return response(StatusCode::BAD_REQUEST, "invalid metadata batch"),
    };
    if batch.version != 1
        || batch.observations.len() > MAX_BATCH
        || batch.superseded.len() > MAX_BATCH
        || batch.superseded.iter().any(|s| !valid_supersession(&s.source_event_id))
        || batch.pending > (imports::OUTBOX_LIMIT * 2) as u64
        || batch.progress.len() > 2
        || batch.progress.iter().any(|p| !p.valid())
        || (batch.progress.len() == 2 && batch.progress[0].source == batch.progress[1].source)
    {
        return response(StatusCode::BAD_REQUEST, "unsupported batch or bounds");
    }
    for o in &mut batch.observations {
        if !matches!(o.source.as_str(), "claude_code" | "codex") {
            return response(StatusCode::BAD_REQUEST, "collector source not allowed");
        }
        if o.source == "claude_code" && o.provider != "anthropic" {
            return response(StatusCode::BAD_REQUEST, "collector provider mismatch");
        }
        if o.validate().is_err() {
            return response(StatusCode::BAD_REQUEST, "invalid usage metadata");
        }
        // Credentials bind origin. Imported identity cannot confer trusted proxy/client/account claims.
        o.origin_id = format!("collector:{id}");
        o.ingested_at_ms = chrono::Utc::now().timestamp_millis();
        o.account_id = None;
        o.auth_type = None;
        o.client_id = None;
        o.logical_request_id = None;
        o.attempt_id = None;
        o.status = None;
        o.logical_success = None;
        o.requested_model = None;
    }
    let count = batch.observations.len();
    let superseded_count = batch.superseded.len();
    let collector_id = id.clone();
    let result=store.call(move|conn|{
        // Recheck inside the serialized commit: rotate/revoke may have raced authentication.
        let active:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM usage_collectors WHERE id=?1 AND credential_hash=?2 AND revoked=0)",params![collector_id,digest],|r|r.get(0))?;
        if !active{bail!("collector revoked")}
        conn.execute_batch("SAVEPOINT usage_collector_ingest")?;
        let result=(||->Result<Value>{
            let result=store::insert_batch(conn,&batch.observations)?;
            for superseded in &batch.superseded{store::suppress_origin_event(conn,"codex",&superseded.source_event_id,&format!("collector:{collector_id}"),"superseded native response evidence")?;}
            for progress in &batch.progress {
                conn.execute("INSERT INTO usage_collector_progress(collector_id,source,payload) VALUES(?1,?2,?3) ON CONFLICT(collector_id,source) DO UPDATE SET payload=excluded.payload",params![collector_id,progress.source.name(),serde_json::to_string(progress)?])?;
            }
            let old_sources:String=conn.query_row("SELECT covered_sources FROM usage_collectors WHERE id=?1",[&collector_id],|r|r.get(0))?;
            let mut sources:std::collections::BTreeSet<String>=serde_json::from_str(&old_sources).unwrap_or_default();sources.extend(batch.observations.iter().map(|o|o.source.clone()));
            let start=batch.observations.iter().map(|o|o.event_at_ms).min();let end=batch.observations.iter().map(|o|o.event_at_ms).max();
            conn.execute("UPDATE usage_collectors SET last_sync_at_ms=?2,pending=?3,covered_sources=?4,time_start_ms=CASE WHEN ?5 IS NULL THEN time_start_ms WHEN time_start_ms IS NULL THEN ?5 ELSE MIN(time_start_ms,?5) END,time_end_ms=CASE WHEN ?6 IS NULL THEN time_end_ms WHEN time_end_ms IS NULL THEN ?6 ELSE MAX(time_end_ms,?6) END WHERE id=?1",params![collector_id,chrono::Utc::now().timestamp_millis(),batch.pending.saturating_sub((count+superseded_count) as u64),serde_json::to_string(&sources)?,start,end])?;
            Ok(result)
        })();
        match result{Ok(result)=>{conn.execute_batch("RELEASE usage_collector_ingest")?;Ok(result)},Err(e)=>{conn.execute_batch("ROLLBACK TO usage_collector_ingest; RELEASE usage_collector_ingest")?;Err(e)}}
    }).await;
    match result{Ok(result)=>Json(json!({"version":1,"durable":true,"collector_id":id,"acknowledged":count,"acknowledged_superseded":superseded_count,"result":result})).into_response(),Err(_)=>response(StatusCode::SERVICE_UNAVAILABLE,"batch not committed")}
}

fn validate_destination(destination: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(destination).map_err(|_| anyhow!("invalid collector destination"))?;
    if !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
        bail!("destination must not contain credentials, query, or fragment")
    }
    let host = url.host_str().ok_or_else(|| anyhow!("destination host required"))?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        bail!("non-loopback destination requires verified HTTPS")
    }
    if url.path() != "/api/usage-ingest" {
        bail!("destination must end in /api/usage-ingest")
    }
    Ok(url)
}
fn secure_dir(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("state directory must be absolute")
    }
    let mut current = PathBuf::new();
    for part in path.components() {
        current.push(part);
        if current.exists() && fs::symlink_metadata(&current)?.file_type().is_symlink() {
            bail!("symlink state directory is unsupported")
        }
    }
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = fs::metadata(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        if meta.file_type().is_symlink() {
            bail!("invalid state directory")
        }
    }
    Ok(())
}
fn protected_read(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).map_err(|_| anyhow!("protected file unavailable"))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 8192 {
        bail!("invalid protected file")
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("protected file requires mode 0600 or stricter")
        }
    }
    fs::read(path).map_err(|_| anyhow!("protected file unavailable"))
}
fn atomic_state(path: &Path, state: &LocalState) -> Result<()> {
    let parent = path.parent().ok_or_else(|| anyhow!("state directory required"))?;
    let temporary = parent.join(format!(".state-{}.tmp", Uuid::new_v4()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<()> {
        let mut file = options.open(&temporary)?;
        file.write_all(&serde_json::to_vec(state)?)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|_| anyhow!("collector state write failed"))
}
fn local_state(directory: &Path) -> Result<LocalState> {
    let state: LocalState = serde_json::from_slice(&protected_read(&directory.join("collector.json"))?)
        .map_err(|_| anyhow!("invalid collector state"))?;
    if state.version != 1 || !valid_credential(&state.credential) {
        bail!("invalid collector state")
    }
    id_ok(&state.id)?;
    validate_destination(&state.destination)?;
    Ok(state)
}
fn outbox_sidecar(path: &Path) -> Result<Option<fs::File>> {
    // DELETE-mode journals may disappear on commit, including while the previous
    // writer finishes shutting down. Inspect the link itself (even if dangling).
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => bail!("symlink outbox sidecar unsupported"),
        Ok(metadata) if !metadata.is_file() => bail!("invalid outbox sidecar"),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    match fs::File::open(path) {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
async fn local_store(directory: PathBuf) -> Result<Store> {
    let store = tokio::task::spawn_blocking(move || -> Result<Store> {
        secure_dir(&directory)?;
        let database = directory.join("outbox.sqlite3");
        if database.exists() && fs::symlink_metadata(&database)?.file_type().is_symlink() {
            bail!("symlink outbox unsupported")
        }
        for suffix in ["-wal", "-shm", "-journal"] {
            let sidecar = directory.join(format!("outbox.sqlite3{suffix}"));
            if let Some(file) = outbox_sidecar(&sidecar)? {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    // A descriptor remains valid if SQLite unlinks the sidecar.
                    file.set_permissions(fs::Permissions::from_mode(0o600))?;
                }
                #[cfg(not(unix))]
                drop(file);
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            if !database.exists() {
                fs::OpenOptions::new().create_new(true).write(true).mode(0o600).open(&database)?;
            }
            fs::set_permissions(&database, fs::Permissions::from_mode(0o600))?;
        }
        // SQLite inherits the database's 0600 mode for newly created sidecars.
        // Do not sweep paths after starting its worker: a commit can unlink a
        // rollback journal between an existence check and chmod.
        Store::open_collector(&database, MAX_LOCAL_DATABASE_BYTES)
    })
    .await??;
    imports::init(&store).await?;
    store.call(|conn|{conn.execute_batch("CREATE TABLE IF NOT EXISTS usage_collector_outbox(sequence INTEGER PRIMARY KEY AUTOINCREMENT,event_key TEXT NOT NULL UNIQUE,payload TEXT NOT NULL,created_at_ms INTEGER NOT NULL);
    CREATE TABLE IF NOT EXISTS usage_collector_local_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS usage_collector_supersessions(event_key TEXT PRIMARY KEY);")?;Ok(())}).await?;
    Ok(store)
}
async fn local_status(store: &Store, state: &LocalState) -> Result<Value> {
    let queue=store.call(|conn|{
        let (pending,bytes):(u64,u64)=conn.query_row("SELECT COUNT(*),COALESCE(SUM(length(payload)),0) FROM usage_collector_outbox",[],|r|Ok((r.get(0)?,r.get(1)?)))?;
        let superseded:u64=conn.query_row("SELECT COUNT(*) FROM usage_collector_supersessions",[],|r|r.get(0))?;
        let mut stmt=conn.prepare("SELECT key,value FROM usage_collector_local_meta WHERE key IN ('last_contact_at_ms','last_sync_at_ms','last_error','server_id')")?;
        let meta=stmt.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<std::collections::BTreeMap<_,_>>>()?;
        let page_size:u64=conn.query_row("PRAGMA page_size",[],|r|r.get(0))?;
        let pages:u64=conn.query_row("PRAGMA page_count",[],|r|r.get(0))?;
        let free:u64=conn.query_row("PRAGMA freelist_count",[],|r|r.get(0))?;
        let max_pages:u64=conn.query_row("PRAGMA max_page_count",[],|r|r.get(0))?;
        let storage_full=pages>=max_pages && free==0;
        Ok(json!({"pending":pending+superseded,"pending_bytes":bytes+superseded*72,"superseded":superseded,"max_pending":imports::OUTBOX_LIMIT,"max_pending_bytes":imports::OUTBOX_BYTES,"metadata":meta,"storage":{"journal_mode":"delete","database_bytes":pages*page_size,"free_page_bytes":free*page_size,"database_limit_bytes":MAX_LOCAL_DATABASE_BYTES,"sqlite_files_budget_bytes":MAX_LOCAL_SQLITE_BYTES,"state":if storage_full{"full"}else{"ready"}},"paused":storage_full || pending+superseded>=imports::OUTBOX_LIMIT as u64 || bytes+superseded*72>=imports::OUTBOX_BYTES as u64}))
    }).await?;
    Ok(
        json!({"collector":{"id":state.id,"destination":state.destination,"state":if queue["paused"]==true{"paused"}else if queue["metadata"]["last_error"].is_string(){"offline"}else if queue["pending"].as_u64().unwrap_or(0)>0{"pending"}else{"ready"},"outbox":queue},"imports":imports::status(store).await?}),
    )
}
async fn record_failure(store: &Store, code: &str) -> Result<()> {
    let code = code.to_string();
    store.call(move|conn|{conn.execute("INSERT INTO usage_collector_local_meta(key,value) VALUES('last_error',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[code])?;Ok(())}).await
}
async fn synchronize(store: &Store, state: &LocalState) -> Result<Value> {
    let url = validate_destination(&state.destination)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?;
    let mut sent = 0_u64;
    for _ in 0..50 {
        let progress = source_progress(&imports::status(store).await?)?;
        let (records, pending, superseded) = store
            .call(|conn| {
                let pending: u64 = conn.query_row("SELECT COUNT(*) FROM usage_collector_outbox", [], |r| r.get(0))?;
                let mut stmt =
                    conn.prepare("SELECT sequence,payload FROM usage_collector_outbox ORDER BY sequence LIMIT 200")?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get::<_, u64>(0)?, r.get::<_, String>(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let mut stmt =
                    conn.prepare("SELECT event_key FROM usage_collector_supersessions ORDER BY event_key LIMIT 200")?;
                let superseded = stmt
                    .query_map([], |r| Ok(CounterSupersession { source_event_id: r.get(0)? }))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let tombstones: u64 =
                    conn.query_row("SELECT COUNT(*) FROM usage_collector_supersessions", [], |r| r.get(0))?;
                Ok((rows, pending + tombstones, superseded))
            })
            .await?;
        let mut observations = Vec::new();
        let mut sequences = Vec::new();
        for (sequence, payload) in records {
            let o: Observation = serde_json::from_str(&payload).map_err(|_| anyhow!("invalid outbox metadata"))?;
            let tentative = Batch {
                version: 1,
                observations: observations.iter().cloned().chain(std::iter::once(o.clone())).collect(),
                pending,
                superseded: superseded.clone(),
                progress: progress.clone(),
            };
            if serde_json::to_vec(&tentative)?.len() > MAX_BODY {
                break;
            }
            observations.push(o);
            sequences.push((sequence, payload));
        }
        let batch = Batch { version: 1, observations, pending, superseded: superseded.clone(), progress };
        let count = batch.observations.len();
        let superseded_count = superseded.len();
        let reply = match client.post(url.clone()).bearer_auth(&state.credential).json(&batch).send().await {
            Ok(reply) => reply,
            Err(_) => {
                record_failure(store, "destination unavailable").await?;
                bail!("collector destination unavailable; outbox retained")
            }
        };
        // Keep contact distinct from durable acknowledgement.
        store.call(|conn|{conn.execute("INSERT INTO usage_collector_local_meta(key,value) VALUES('last_contact_at_ms',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[chrono::Utc::now().timestamp_millis().to_string()])?;Ok(())}).await?;
        if !reply.status().is_success() {
            record_failure(store, "destination rejected batch").await?;
            bail!("collector destination rejected batch; outbox retained")
        }
        let mut stream = reply.bytes_stream();
        let mut body = Vec::new();
        use futures::StreamExt;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| anyhow!("invalid acknowledgement"))?;
            if body.len() + chunk.len() > 8192 {
                bail!("acknowledgement exceeds limit")
            };
            body.extend_from_slice(&chunk);
        }
        let reply: Value = serde_json::from_slice(&body).map_err(|_| anyhow!("invalid acknowledgement"))?;
        if reply.get("version").and_then(Value::as_u64) != Some(1)
            || reply.get("durable").and_then(Value::as_bool) != Some(true)
            || reply.get("acknowledged").and_then(Value::as_u64) != Some(count as u64)
            || (superseded_count > 0
                && reply.get("acknowledged_superseded").and_then(Value::as_u64) != Some(superseded_count as u64))
        {
            bail!("batch was not durably acknowledged; outbox retained")
        }
        let server_id = reply
            .get("collector_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("missing server identity"))?
            .to_string();
        id_ok(&server_id)?;
        store.call(move|conn|{
            conn.execute_batch("SAVEPOINT collector_ack")?;
            let result=(||->Result<()>{for (sequence,payload) in sequences {conn.execute("DELETE FROM usage_collector_outbox WHERE sequence=?1 AND payload=?2",params![sequence,payload])?;}
                for item in superseded{conn.execute("DELETE FROM usage_collector_supersessions WHERE event_key=?1",[item.source_event_id])?;}
                for (key,value) in [("last_sync_at_ms",chrono::Utc::now().timestamp_millis().to_string()),("server_id",server_id)] {
                    conn.execute("INSERT INTO usage_collector_local_meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![key,value])?;
                }conn.execute("DELETE FROM usage_collector_local_meta WHERE key='last_error'",[])?;Ok(())})();
            match result{Ok(())=>{conn.execute_batch("RELEASE collector_ack")?;Ok(())},Err(e)=>{conn.execute_batch("ROLLBACK TO collector_ack; RELEASE collector_ack")?;Err(e)}}
        }).await?;
        sent += count as u64;
        if pending <= (count + superseded_count) as u64 || (count == 0 && superseded_count == 0) {
            break;
        }
    }
    Ok(json!({"sent":sent,"status":local_status(store,state).await?}))
}
async fn print_and_shutdown(store: &Store, result: Result<Value>) -> Result<()> {
    let shutdown = store.shutdown().await;
    let result = result?;
    shutdown?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

async fn stop_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}

async fn run_loop(store: &Store, state: &LocalState, once: bool) -> Result<()> {
    let mut delay = 1;
    loop {
        let scanned = imports::scan_outbox(store).await;
        if scanned.is_err() {
            record_failure(store, "native scan unavailable").await?
        }
        match synchronize(store, state).await {
            Ok(result) => {
                delay = 1;
                if once {
                    println!("{}", serde_json::to_string_pretty(&result)?);
                    return Ok(());
                }
            }
            Err(_) => {
                if once {
                    bail!("collector sync unavailable; durable outbox retained")
                };
                delay = (delay * 2).min(300);
            }
        }
        tokio::time::sleep(Duration::from_secs(if delay == 1 { 30 } else { delay })).await;
    }
}

pub async fn run_command(command: CollectorCommand) -> Result<()> {
    match command {
        CollectorCommand::Enroll { state_dir, destination, credential_file, codex_root, claude_root } => {
            validate_destination(&destination)?;
            let directory = state_dir.clone();
            let state = tokio::task::spawn_blocking(move || -> Result<LocalState> {
                secure_dir(&directory)?;
                let data = if let Some(file) = credential_file {
                    protected_read(&file)?
                } else {
                    let mut data = Vec::new();
                    std::io::stdin().take(8193).read_to_end(&mut data)?;
                    if data.len() > 8192 {
                        bail!("credential input exceeds limit")
                    };
                    data
                };
                let token = std::str::from_utf8(&data).map_err(|_| anyhow!("invalid credential"))?.trim().to_string();
                if !valid_credential(&token) {
                    bail!("invalid collector credential")
                }
                let state_path = directory.join("collector.json");
                let id = if state_path.exists() { local_state(&directory)?.id } else { Uuid::new_v4().to_string() };
                let state = LocalState { version: 1, id, destination, credential: token };
                atomic_state(&state_path, &state)?;
                Ok(state)
            })
            .await??;
            let store = local_store(state_dir).await?;
            let result = async {
                for root in codex_root {
                    imports::configure(&store, "codex", &root.to_string_lossy(), true).await?;
                }
                for root in claude_root {
                    imports::configure(&store, "claude_code", &root.to_string_lossy(), true).await?;
                }
                local_status(&store, &state).await
            }
            .await;
            print_and_shutdown(&store, result).await?;
        }
        CollectorCommand::Status { state_dir } => {
            let directory = state_dir.clone();
            let state = tokio::task::spawn_blocking(move || local_state(&directory)).await??;
            let store = local_store(state_dir).await?;
            print_and_shutdown(&store, local_status(&store, &state).await).await?;
        }
        CollectorCommand::Sync { state_dir } => {
            let directory = state_dir.clone();
            let state = tokio::task::spawn_blocking(move || local_state(&directory)).await??;
            let store = local_store(state_dir).await?;
            print_and_shutdown(&store, synchronize(&store, &state).await).await?;
        }
        CollectorCommand::Run { state_dir, once } => {
            let directory = state_dir.clone();
            let state = tokio::task::spawn_blocking(move || local_state(&directory)).await??;
            let store = local_store(state_dir).await?;
            let result = tokio::select! {
                result = run_loop(&store, &state, once) => result,
                result = stop_signal() => result,
            };
            let shutdown = store.shutdown().await;
            result?;
            shutdown?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "collector_tests.rs"]
mod tests;
