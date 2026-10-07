//! Explicitly consented, bounded native history imports. Raw records never enter SQLite.
use super::{
    store::{self, Store},
    types::{MAX_TOKENS, Observation, Tokens},
};
use anyhow::{Result, anyhow, bail};
use clap::Subcommand;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    sync::OnceLock,
};

const PARSER: &str = "native-2026-10-07-v1";
const MAX_FILES: usize = 4096;
const MAX_BYTES: u64 = 32 * 1024 * 1024;
const MAX_FILE: u64 = 256 * 1024 * 1024;
const MAX_LINE: usize = 512 * 1024;
const MAX_ROWS: usize = 2000;
pub(crate) const OUTBOX_LIMIT: usize = 10000;
pub(crate) const OUTBOX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Subcommand)]
pub enum UsageCommand {
    /// Report candidate homes; do not read history without enabled roots.
    Status {
        #[arg(long)]
        database: PathBuf,
    },
    Enable {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        source: String,
        #[arg(long)]
        root: PathBuf,
    },
    Disable {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        source: String,
        #[arg(long)]
        root: PathBuf,
    },
    Scan {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        source: Option<String>,
    },
    /// Reset opted-in checkpoints; stable observations remain idempotent.
    Backfill {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        source: Option<String>,
    },
    /// Price unpriced rows that predate the catalogue at current-rate equivalents.
    Reprice {
        #[arg(long)]
        database: PathBuf,
    },
}
pub async fn run_command(command: UsageCommand) -> Result<()> {
    let database = match &command {
        UsageCommand::Status { database }
        | UsageCommand::Enable { database, .. }
        | UsageCommand::Disable { database, .. }
        | UsageCommand::Scan { database, .. }
        | UsageCommand::Backfill { database, .. }
        | UsageCommand::Reprice { database } => database,
    };
    let database = database.clone();
    let store = tokio::task::spawn_blocking(move || Store::open_existing(&database, 128)).await??;
    let result = match command {
        UsageCommand::Status { .. } => status(&store).await,
        UsageCommand::Enable { source, root, .. } => configure(&store, &source, &root.to_string_lossy(), true).await,
        UsageCommand::Disable { source, root, .. } => configure(&store, &source, &root.to_string_lossy(), false).await,
        UsageCommand::Scan { source, .. } => scan(&store, source).await,
        UsageCommand::Backfill { source, .. } => backfill(&store, source).await,
        UsageCommand::Reprice { .. } => reprice(&store).await,
    };
    let shutdown = store.shutdown().await;
    let result = result?;
    shutdown?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

pub(crate) fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
fn source_ok(source: &str) -> Result<()> {
    if !matches!(source, "codex" | "claude_code") {
        bail!("unsupported import source");
    }
    Ok(())
}
pub(crate) async fn init(store: &Store) -> Result<()> {
    store.call(|conn| {
        conn.execute_batch("CREATE TABLE IF NOT EXISTS usage_import_roots(id TEXT PRIMARY KEY,source TEXT NOT NULL,root TEXT NOT NULL,enabled INTEGER NOT NULL,last_scan_at_ms INTEGER,last_error TEXT,imported INTEGER NOT NULL DEFAULT 0,duplicate INTEGER NOT NULL DEFAULT 0,skipped INTEGER NOT NULL DEFAULT 0,unsupported INTEGER NOT NULL DEFAULT 0,failed INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS usage_import_checkpoints(root_id TEXT NOT NULL,file_id TEXT NOT NULL,offset INTEGER NOT NULL,prefix_len INTEGER NOT NULL,fingerprint TEXT NOT NULL,context TEXT NOT NULL,PRIMARY KEY(root_id,file_id));")?;
        Ok(())
    }).await
}
fn root_id(source: &str, root: &str) -> String {
    hash(format!("{source}\0{root}").as_bytes())
}
fn validate_root(root: &Path) -> Result<PathBuf> {
    if !root.is_absolute() {
        bail!("import root must be absolute");
    }
    let mut part = PathBuf::new();
    for component in root.components() {
        part.push(component);
        if fs::symlink_metadata(&part)?.file_type().is_symlink() {
            bail!("symlink import roots are unsupported");
        }
    }
    if !root.is_dir() {
        bail!("import root must be a directory");
    }
    let canonical = fs::canonicalize(root)?;
    // No provider/config reads: kernel mount metadata prevents traversing network filesystems.
    #[cfg(target_os = "linux")]
    if let Ok(mounts) = fs::read_to_string("/proc/self/mountinfo") {
        let mut selected: Option<(usize, bool)> = None;
        for line in mounts.lines() {
            let Some((left, right)) = line.split_once(" - ") else { continue };
            let Some(mount) = left.split_whitespace().nth(4) else { continue };
            let mount = mount.replace("\\040", " ").replace("\\011", "\t").replace("\\134", "\\");
            if canonical.starts_with(&mount) {
                let kind = right.split_whitespace().next().unwrap_or("");
                let network = matches!(
                    kind,
                    "nfs"
                        | "nfs4"
                        | "cifs"
                        | "smb3"
                        | "9p"
                        | "ceph"
                        | "afs"
                        | "fuse.sshfs"
                        | "fuse.rclone"
                        | "fuse.s3fs"
                );
                if selected.as_ref().is_none_or(|(length, _)| mount.len() > *length) {
                    selected = Some((mount.len(), network));
                }
            }
        }
        if selected.is_some_and(|(_, network)| network) {
            bail!("network import roots are unsupported");
        }
    }
    Ok(canonical)
}
pub async fn configure(store: &Store, source: &str, root: &str, enabled: bool) -> Result<Value> {
    source_ok(source)?;
    let raw = PathBuf::from(root);
    if root.len() > 4096 || root.chars().any(char::is_control) {
        bail!("invalid import root");
    }
    let canonical = if enabled {
        tokio::task::spawn_blocking(move || validate_root(&raw))
            .await?
            .map_err(|_| anyhow!("root unavailable or unsupported"))?
    } else {
        if !raw.is_absolute() {
            bail!("import root must be absolute")
        };
        tokio::task::spawn_blocking(move || fs::canonicalize(&raw).unwrap_or(raw)).await?
    };
    let path = canonical.to_string_lossy().into_owned();
    init(store).await?;
    let source = source.to_string();
    let id = root_id(&source, &path);
    store.call(move |conn| {
        conn.execute("INSERT INTO usage_import_roots(id,source,root,enabled) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET enabled=excluded.enabled,last_error=NULL",params![id,source,path,enabled])?;
        Ok(())
    }).await?;
    status(store).await
}
pub async fn status(store: &Store) -> Result<Value> {
    init(store).await?;
    let imports=store.call(|conn| {
        let mut stmt=conn.prepare("SELECT source,root,enabled,last_scan_at_ms,last_error,imported,duplicate,skipped,unsupported,failed FROM usage_import_roots ORDER BY source,root")?;
        let rows=stmt.query_map([],|row| {
            let enabled:bool=row.get(2)?;let error:Option<String>=row.get(4)?;
            Ok(json!({"source":row.get::<_,String>(0)?,"root":row.get::<_,String>(1)?,"enabled":enabled,"state":if !enabled {"disabled"} else if error.is_some(){"attention"}else{"ready"},"last_scan_at_ms":row.get::<_,Option<i64>>(3)?,"last_error":error,"imported":row.get::<_,i64>(5)?,"duplicate":row.get::<_,i64>(6)?,"skipped":row.get::<_,i64>(7)?,"unsupported":row.get::<_,i64>(8)?,"failed":row.get::<_,i64>(9)?}))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }).await?;
    // Candidates are names only; no filesystem access/discovery or configuration reads.
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let codex = std::env::var_os("CODEX_HOME").map(PathBuf::from).or_else(|| home.as_ref().map(|p| p.join(".codex")));
    let claude =
        std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).or_else(|| home.as_ref().map(|p| p.join(".claude")));
    Ok(
        json!({"imports":imports,"candidates":[{"source":"codex","root":codex,"scanned":false},{"source":"claude_code","root":claude,"scanned":false}]}),
    )
}
#[derive(Clone)]
struct Root {
    id: String,
    source: String,
    path: PathBuf,
}
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(crate) struct Context {
    session: Option<String>,
    #[serde(default)]
    thread: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    #[serde(default)]
    meta_seen: bool,
    #[serde(default)]
    legacy_fork: bool,
    #[serde(default)]
    legacy_reset: bool,
    #[serde(default)]
    legacy_emitted: Vec<String>,
    #[serde(skip)]
    superseded: Vec<String>,
    #[serde(default)]
    turns: BTreeMap<String, String>,
    cumulative: Option<NativeTokens>,
    #[serde(default)]
    modern_totals: Vec<String>,
}
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct NativeTokens {
    input: u64,
    read: u64,
    write: u64,
    output: u64,
    #[serde(default)]
    reasoning: Option<u64>,
}
impl NativeTokens {
    fn key(&self) -> String {
        format!(
            "{}:{}:{}:{}:{}",
            self.input,
            self.read,
            self.write,
            self.output,
            self.reasoning.map(|v| v.to_string()).unwrap_or_else(|| "?".into())
        )
    }
    fn diff(&self, previous: &Self) -> Option<Self> {
        Some(Self {
            input: self.input.checked_sub(previous.input)?,
            read: self.read.checked_sub(previous.read)?,
            write: self.write.checked_sub(previous.write)?,
            output: self.output.checked_sub(previous.output)?,
            reasoning: match (self.reasoning, previous.reasoning) {
                (Some(current), Some(previous)) => Some(current.checked_sub(previous)?),
                _ => None,
            },
        })
    }
    fn tokens(&self) -> Option<Tokens> {
        Some(Tokens {
            input: Some(self.input.checked_sub(self.read)?.checked_sub(self.write)?),
            cache_read: Some(self.read),
            cache_write: Some(self.write),
            output: Some(self.output),
            reasoning: self.reasoning,
            ..Tokens::default()
        })
    }
}
#[derive(Clone, Default)]
struct Checkpoint {
    offset: u64,
    prefix_len: u64,
    fingerprint: String,
    context: Context,
}
#[derive(Default, Serialize, Clone)]
pub(crate) struct Counts {
    pub imported: u64,
    pub duplicate: u64,
    pub skipped: u64,
    pub unsupported: u64,
    pub failed: u64,
    pub deferred: u64,
}
struct ParsedFile {
    id: String,
    expected: Option<u64>,
    checkpoint: Checkpoint,
    observations: Vec<Observation>,
    counts: Counts,
}
fn label(value: Option<&Value>) -> Option<String> {
    value?
        .as_str()
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 200
                && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
        })
        .map(str::to_string)
}
fn number(value: Option<&Value>) -> Option<u64> {
    value?.as_u64().filter(|v| *v <= MAX_TOKENS)
}
fn timestamp(v: &Value) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(v.get("timestamp")?.as_str()?).ok().map(|t| t.timestamp_millis())
}
fn codex_tokens(v: &Value) -> Tokens {
    let inclusive = number(v.get("input_tokens"));
    let read = number(v.get("cached_input_tokens"));
    let write =
        if v.get("cache_write_input_tokens").is_none() { Some(0) } else { number(v.get("cache_write_input_tokens")) };
    Tokens {
        input: inclusive.and_then(|i| i.checked_sub(read?).and_then(|i| i.checked_sub(write?))),
        cache_read: read,
        cache_write: write,
        output: number(v.get("output_tokens")),
        reasoning: number(v.get("reasoning_output_tokens")),
        ..Tokens::default()
    }
}
fn valid_numbers(v: &Value, names: &[&str]) -> Result<()> {
    for name in names {
        if let Some(value) = v.get(*name).filter(|v| !v.is_null())
            && number(Some(value)).is_none()
        {
            bail!("invalid numeric metadata")
        }
    }
    Ok(())
}
fn native(v: &Value) -> Option<NativeTokens> {
    Some(NativeTokens {
        input: number(v.get("input_tokens"))?,
        read: number(v.get("cached_input_tokens"))?,
        write: if v.get("cache_write_input_tokens").is_none() { 0 } else { number(v.get("cache_write_input_tokens"))? },
        output: number(v.get("output_tokens"))?,
        reasoning: number(v.get("reasoning_output_tokens")),
    })
}
fn validated_native(v: &Value) -> Result<NativeTokens> {
    valid_numbers(
        v,
        &[
            "input_tokens",
            "cached_input_tokens",
            "cache_write_input_tokens",
            "output_tokens",
            "reasoning_output_tokens",
        ],
    )?;
    let total = native(v).ok_or_else(|| anyhow!("estimated or invalid cumulative usage"))?;
    total
        .tokens()
        .ok_or_else(|| anyhow!("inconsistent cumulative categories"))?
        .validate()
        .map_err(|_| anyhow!("invalid cumulative metadata"))?;
    Ok(total)
}

/// Converts an allowlisted native row only. No transcript/path/credential fields survive.
pub(crate) fn parse_record(source: &str, v: &Value, context: &mut Context) -> Result<Option<Observation>> {
    let mut candidate = context.clone();
    let result = parse_record_inner(source, v, &mut candidate);
    if result.is_ok() {
        *context = candidate;
    } else if candidate.legacy_reset {
        // A validated reset deliberately disables ambiguous legacy evidence. All
        // other failed rows leave checkpoints, supersession, and baselines intact.
        context.legacy_reset = true;
    }
    result
}

fn parse_record_inner(source: &str, v: &Value, context: &mut Context) -> Result<Option<Observation>> {
    if source == "claude_code" {
        if v.get("type").and_then(Value::as_str) != Some("assistant") {
            return Ok(None);
        }
        let Some(message) = v.get("message") else { return Ok(None) };
        let Some(usage) = message.get("usage") else { return Ok(None) };
        valid_numbers(
            usage,
            &["input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"],
        )?;
        if let Some(ttl) = usage.get("cache_creation") {
            valid_numbers(ttl, &["ephemeral_5m_input_tokens", "ephemeral_1h_input_tokens"])?;
        }
        if let Some(details) = usage.get("output_tokens_details") {
            valid_numbers(details, &["reasoning_tokens"])?;
        }
        let Some(at) = timestamp(v) else { bail!("missing timestamp") };
        let message_id = label(message.get("id"));
        let request_id = label(v.get("requestId"));
        let identity =
            message_id.clone().or_else(|| label(v.get("uuid"))).ok_or_else(|| anyhow!("missing event identity"))?;
        let mut o = Observation::new(source, format!("message:{identity}"), "anthropic", at);
        o.parser_version = PARSER.into();
        o.actual_model = label(message.get("model"));
        o.response_id = message_id;
        o.provider_request_id = request_id;
        o.session_id = label(v.get("sessionId"));
        o.service_tier = label(usage.get("service_tier"));
        o.inference_geo = label(usage.get("inference_geo"));
        o.tokens = Tokens {
            input: number(usage.get("input_tokens")),
            cache_read: number(usage.get("cache_read_input_tokens")),
            cache_write: number(usage.get("cache_creation_input_tokens")),
            write_5m: number(usage.pointer("/cache_creation/ephemeral_5m_input_tokens")),
            write_1h: number(usage.pointer("/cache_creation/ephemeral_1h_input_tokens")),
            output: number(usage.get("output_tokens")),
            reasoning: number(usage.pointer("/output_tokens_details/reasoning_tokens")),
        };
        o.completeness = if o.tokens.total().is_some() { "complete" } else { "partial" }.into();
        o.validate().map_err(|_| anyhow!("invalid usage metadata"))?;
        return Ok(Some(o));
    }
    if source != "codex" {
        bail!("unsupported source")
    }
    let Some(payload) = v.get("payload") else { return Ok(None) };
    match v.get("type").and_then(Value::as_str) {
        Some("session_meta") => {
            if !context.meta_seen {
                context.meta_seen = true;
                context.session = label(payload.get("session_id")).or_else(|| label(payload.get("id")));
                context.thread = label(payload.get("id"));
                context.provider = label(payload.get("model_provider"));
                context.legacy_fork = payload.get("forked_from_id").is_some_and(|v| !v.is_null());
            }
            Ok(None)
        }
        Some("turn_context") => {
            context.model = label(payload.get("model"));
            if let (Some(turn), Some(model)) = (label(payload.get("turn_id")), context.model.clone()) {
                if context.turns.len() >= 64 {
                    context.turns.clear();
                }
                context.turns.insert(turn, model);
            }
            Ok(None)
        }
        Some("token_usage_record") => {
            let usage = payload.get("usage").ok_or_else(|| anyhow!("missing response usage"))?;
            valid_numbers(
                usage,
                &[
                    "input_tokens",
                    "cached_input_tokens",
                    "cache_write_input_tokens",
                    "output_tokens",
                    "reasoning_output_tokens",
                ],
            )?;
            let id = label(payload.get("response_id")).ok_or_else(|| anyhow!("missing response identity"))?;
            let at = timestamp(v).ok_or_else(|| anyhow!("missing timestamp"))?;
            let mut o = Observation::new(
                source,
                format!("response:{id}"),
                context.provider.as_deref().unwrap_or("unknown"),
                at,
            );
            o.parser_version = PARSER.into();
            o.response_id = Some(id);
            o.session_id = label(payload.get("session_id")).or_else(|| context.session.clone());
            o.actual_model = match label(payload.get("turn_id")) {
                Some(id) => context.turns.get(&id).cloned(),
                None => context.model.clone(),
            };
            o.tokens = codex_tokens(usage);
            o.completeness = if o.tokens.total().is_some() { "complete" } else { "partial" }.into();
            if number(usage.get("input_tokens")).is_some()
                && o.tokens.cache_read.is_some()
                && o.tokens.cache_write.is_some()
                && o.tokens.input.is_none()
            {
                bail!("inconsistent input categories")
            }
            if let Some(total) = payload.get("thread_token_usage").and_then(|v| validated_native(v).ok()) {
                let key = total.key();
                if let Some(thread) = &context.thread {
                    let fallback = format!("counter:{}", hash(format!("thread:{thread}:{key}").as_bytes()));
                    if let Some(index) = context.legacy_emitted.iter().position(|id| id == &fallback) {
                        context.legacy_emitted.remove(index);
                        context.superseded.push(fallback);
                    }
                }
                if context.modern_totals.len() >= 64 {
                    context.modern_totals.remove(0);
                }
                context.modern_totals.push(key);
            }
            o.validate().map_err(|_| anyhow!("invalid usage metadata"))?;
            Ok(Some(o))
        }
        Some("event_msg") if payload.get("type").and_then(Value::as_str) == Some("token_count") => {
            if context.legacy_fork {
                bail!("unsupported ambiguous legacy fork usage")
            }
            if context.legacy_reset {
                bail!("unsupported ambiguous legacy usage after counter reset")
            }
            let Some(info) = payload.get("info").filter(|v| !v.is_null()) else { return Ok(None) };
            let total = validated_native(
                info.get("total_token_usage").ok_or_else(|| anyhow!("estimated or invalid cumulative usage"))?,
            )?;
            let session = context.session.clone().ok_or_else(|| anyhow!("missing session identity"))?;
            let thread =
                context.thread.as_deref().ok_or_else(|| anyhow!("unsupported missing legacy thread identity"))?;
            let at = timestamp(v).ok_or_else(|| anyhow!("missing timestamp"))?;
            let identity = hash(format!("thread:{thread}:{}", total.key()).as_bytes());
            let mut o = Observation::new(
                source,
                format!("counter:{identity}"),
                context.provider.as_deref().unwrap_or("unknown"),
                at,
            );
            o.parser_version = PARSER.into();
            o.session_id = Some(session);
            o.actual_model = context.model.clone();
            o.tokens = total.tokens().ok_or_else(|| anyhow!("inconsistent cumulative categories"))?;
            // Legacy token_count can contain estimates/replay; explicitly partial evidence.
            o.completeness = "partial".into();
            // Validate timestamps and cumulative subsets before recording a reset.
            o.validate().map_err(|_| anyhow!("invalid usage metadata"))?;
            let previous = context
                .cumulative
                .replace(total.clone())
                .unwrap_or_else(|| NativeTokens { reasoning: Some(0), ..NativeTokens::default() });
            if total == previous {
                return Ok(None);
            }
            let Some(delta) = total.diff(&previous) else {
                context.legacy_reset = true;
                bail!("unsupported ambiguous legacy usage after counter reset")
            };
            if context.modern_totals.contains(&total.key()) {
                return Ok(None);
            }
            if context.legacy_emitted.len() >= 64 {
                context.legacy_emitted.remove(0);
            }
            context.legacy_emitted.push(o.source_event_id.clone());
            o.tokens = delta.tokens().ok_or_else(|| anyhow!("inconsistent cumulative categories"))?;
            o.validate().map_err(|_| anyhow!("invalid usage metadata"))?;
            Ok(Some(o))
        }
        _ => Ok(None),
    }
}

fn files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut stack = vec![root.to_path_buf()];
    let mut found = Vec::new();
    let mut dirs = 0;
    #[cfg(unix)]
    let device = {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(root)?.dev()
    };
    while let Some(dir) = stack.pop() {
        dirs += 1;
        if dirs > MAX_FILES {
            bail!("directory scan bound exceeded")
        }
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_symlink() {
                continue;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if entry.metadata()?.dev() != device {
                    continue;
                }
            }
            if kind.is_dir() {
                stack.push(entry.path())
            } else if kind.is_file()
                && entry
                    .path()
                    .file_name()
                    .and_then(|v| v.to_str())
                    .is_some_and(|s| s.ends_with(".jsonl") || s.ends_with(".jsonl.zst"))
            {
                if found.len() >= MAX_FILES {
                    bail!("file scan bound exceeded")
                };
                let path = entry.path();
                if path.extension().and_then(|v| v.to_str()) == Some("zst") && path.with_extension("").exists() {
                    continue;
                };
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}
fn reader(path: &Path) -> Result<Box<dyn Read + Send>> {
    let file = fs::File::open(path)?;
    if path.extension().and_then(|v| v.to_str()) == Some("zst") {
        Ok(Box::new(zstd::stream::read::Decoder::new(file)?))
    } else {
        Ok(Box::new(file))
    }
}
fn file_signature(metadata: &fs::Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        format!("{}:{}", metadata.dev(), metadata.ino())
    }
    #[cfg(not(unix))]
    {
        metadata
            .created()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|t| t.as_nanos().to_string())
            .unwrap_or_default()
    }
}
fn file_fingerprint(prefix: &[u8], signature: &str) -> String {
    let mut bytes = prefix.to_vec();
    bytes.extend_from_slice(signature.as_bytes());
    hash(&bytes)
}
fn read_file(path: &Path, root: &Root, checkpoint: Option<&Checkpoint>, remaining: &mut u64) -> Result<ParsedFile> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        bail!("symlink skipped")
    }
    let logical = if path.extension().and_then(|v| v.to_str()) == Some("zst") {
        path.with_extension("")
    } else {
        path.to_path_buf()
    };
    let id = hash(logical.as_os_str().as_encoded_bytes());
    let signature = file_signature(&fs::metadata(path)?);
    let mut cp = checkpoint.cloned().unwrap_or_default();
    let expected = checkpoint.map(|c| c.offset);
    let mut input = BufReader::new(reader(path)?.take(MAX_FILE + 1));
    let mut prefix = vec![0; cp.prefix_len.min(4096) as usize];
    let n = input.read(&mut prefix)?;
    prefix.truncate(n);
    if cp.offset > 0 && (n as u64 != cp.prefix_len || file_fingerprint(&prefix, &signature) != cp.fingerprint) {
        cp = Checkpoint::default();
        input = BufReader::new(reader(path)?.take(MAX_FILE + 1));
        prefix.clear();
    } else {
        let skip = cp.offset.saturating_sub(n as u64);
        let skipped = std::io::copy(&mut input.by_ref().take(skip), &mut std::io::sink())?;
        if skipped != skip {
            cp = Checkpoint::default();
            input = BufReader::new(reader(path)?.take(MAX_FILE + 1));
            prefix.clear();
        }
    }
    let mut observations = BTreeMap::<String, Observation>::new();
    let mut counts = Counts::default();
    let start = cp.offset;
    loop {
        if *remaining == 0 || observations.len() >= MAX_ROWS || cp.offset.saturating_sub(start) >= 8 * 1024 * 1024 {
            break;
        }
        let mut line = Vec::new();
        let mut complete = false;
        let mut overflow = false;
        let mut consumed = 0_u64;
        loop {
            let available = input.fill_buf()?;
            if available.is_empty() {
                break;
            }
            let take = available.iter().position(|b| *b == b'\n').map_or(available.len(), |i| i + 1);
            if prefix.len() < 4096 {
                prefix.extend_from_slice(&available[..take.min(4096 - prefix.len())]);
            }
            if !overflow {
                if line.len() + take > MAX_LINE {
                    overflow = true;
                    line.clear()
                } else {
                    line.extend_from_slice(&available[..take]);
                }
            }
            consumed += take as u64;
            complete = available[take - 1] == b'\n';
            input.consume(take);
            if complete || consumed > *remaining || consumed > MAX_FILE {
                break;
            }
        }
        if consumed == 0 {
            break;
        }
        if !complete {
            if overflow || cp.offset + consumed >= MAX_FILE {
                counts.unsupported += 1;
            }
            counts.deferred += 1;
            break;
        } // trailing fragment is retried from its start.
        if consumed > *remaining {
            counts.deferred += 1;
            break;
        }
        *remaining -= consumed;
        cp.offset += consumed;
        if cp.offset > MAX_FILE {
            counts.unsupported += 1;
            break;
        }
        if overflow {
            counts.unsupported += 1;
            continue;
        }
        match serde_json::from_slice::<Value>(&line) {
            Ok(v) => match parse_record(&root.source, &v, &mut cp.context) {
                Ok(Some(o)) => {
                    for superseded in &cp.context.superseded {
                        observations.remove(superseded);
                    }
                    if observations.insert(o.source_event_id.clone(), o).is_some() {
                        counts.duplicate += 1;
                    }
                }
                Ok(None) => counts.skipped += 1,
                Err(e) => {
                    if e.to_string().starts_with("unsupported") {
                        counts.unsupported += 1
                    } else {
                        counts.failed += 1
                    }
                }
            },
            Err(_) => counts.failed += 1,
        }
    }
    // Fingerprint only committed bytes, never the partially written suffix.
    cp.prefix_len = cp.offset.min(4096);
    let mut input = reader(path)?;
    let mut current_prefix = vec![0; cp.prefix_len as usize];
    input.read_exact(&mut current_prefix)?;
    if file_signature(&fs::metadata(path)?) != signature
        || prefix.get(..cp.prefix_len as usize) != Some(current_prefix.as_slice())
    {
        bail!("history changed during scan")
    }
    cp.fingerprint = file_fingerprint(&current_prefix, &signature);
    Ok(ParsedFile { id, expected, checkpoint: cp, observations: observations.into_values().collect(), counts })
}

pub async fn scan(store: &Store, source: Option<String>) -> Result<Value> {
    scan_mode(store, source, false, false).await
}
pub(crate) async fn scan_outbox(store: &Store) -> Result<Value> {
    scan_mode(store, None, true, false).await
}
async fn scan_mode(store: &Store, source: Option<String>, outbox: bool, reimport: bool) -> Result<Value> {
    if let Some(source) = &source {
        source_ok(source)?
    }
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    let _guard = LOCK.get_or_init(|| tokio::sync::Mutex::new(())).lock().await;
    init(store).await?;
    let roots=store.call(move |conn| {
        let mut stmt=conn.prepare("SELECT id,source,root FROM usage_import_roots WHERE enabled=1 AND (?1 IS NULL OR source=?1) ORDER BY id")?;
        Ok(stmt.query_map([source],|row|Ok(Root{id:row.get(0)?,source:row.get(1)?,path:PathBuf::from(row.get::<_,String>(2)?)}))?.collect::<rusqlite::Result<Vec<_>>>()?)
    }).await?;
    let mut all = Vec::new();
    for root in roots {
        let root_id = root.id.clone();
        let checkpoints=store.call(move |conn| {
            let mut stmt=conn.prepare("SELECT file_id,offset,prefix_len,fingerprint,context FROM usage_import_checkpoints WHERE root_id=?1")?;
            let rows=stmt.query_map([root_id],|r|Ok((r.get::<_,String>(0)?,Checkpoint{offset:r.get::<_,u64>(1)?,prefix_len:r.get::<_,u64>(2)?,fingerprint:r.get(3)?,context:serde_json::from_str(&r.get::<_,String>(4)?).unwrap_or_default()})))?;
            Ok(rows.collect::<rusqlite::Result<BTreeMap<_,_>>>()?)
        }).await?;
        let parse_root = root.clone();
        let parsed = tokio::task::spawn_blocking(move || -> Result<Vec<ParsedFile>> {
            validate_root(&parse_root.path)?;
            let paths = files(&parse_root.path)?;
            let mut remaining = MAX_BYTES;
            let mut results = Vec::new();
            for path in paths {
                if remaining == 0 {
                    break;
                }
                let logical = if path.extension().and_then(|v| v.to_str()) == Some("zst") {
                    path.with_extension("")
                } else {
                    path.clone()
                };
                let id = hash(logical.as_os_str().as_encoded_bytes());
                match read_file(&path, &parse_root, checkpoints.get(&id), &mut remaining) {
                    Ok(file) => results.push(file),
                    Err(_) => results.push(ParsedFile {
                        id,
                        expected: None,
                        checkpoint: Checkpoint::default(),
                        observations: Vec::new(),
                        counts: Counts { failed: 1, ..Counts::default() },
                    }),
                }
            }
            Ok(results)
        })
        .await?;
        let root_id = root.id.clone();
        let result=store.call(move |conn| {
            let mut counts=Counts::default();let mut error=None;
            match parsed {
                Err(_)=>{counts.failed=1;error=Some("root unavailable or scan bound exceeded")},
                Ok(files)=>for file in files {
                    if file.counts.failed>0 && file.checkpoint.offset==0 && file.observations.is_empty() {counts.failed+=file.counts.failed;continue}
                    let enabled:bool=conn.query_row("SELECT enabled FROM usage_import_roots WHERE id=?1",[&root_id],|r|r.get(0))?;if !enabled{break}
                    let offset:Option<u64>=conn.query_row("SELECT offset FROM usage_import_checkpoints WHERE root_id=?1 AND file_id=?2",params![root_id,file.id],|r|r.get(0)).optional()?;
                    if offset!=file.expected {counts.deferred+=1;continue}
                    conn.execute_batch("SAVEPOINT usage_import_commit")?;
                    let commit=(||->Result<Value>{
                        if outbox {
                            let (n,bytes):(usize,usize)=conn.query_row("SELECT COUNT(*),COALESCE(SUM(length(payload)),0) FROM usage_collector_outbox",[],|r|Ok((r.get(0)?,r.get(1)?)))?;
                            let tombstones:usize=conn.query_row("SELECT COUNT(*) FROM usage_collector_supersessions",[],|r|r.get(0))?;
                            if n+tombstones+file.observations.len()+file.checkpoint.context.superseded.len()>OUTBOX_LIMIT{bail!("outbox bound reached")}
                            for id in &file.checkpoint.context.superseded{conn.execute("INSERT OR IGNORE INTO usage_collector_supersessions(event_key) VALUES(?1)",[id])?;conn.execute("DELETE FROM usage_collector_outbox WHERE event_key=?1",[format!("codex:{id}")])?;}
                            let pending=file.observations.iter().map(serde_json::to_string).collect::<serde_json::Result<Vec<_>>>()?;
                            if n+pending.len()>OUTBOX_LIMIT || bytes+tombstones*72+pending.iter().map(String::len).sum::<usize>()+file.checkpoint.context.superseded.len()*72>OUTBOX_BYTES {bail!("outbox bound reached")}
                            let mut inserted=0;
                            for (o,payload) in file.observations.iter().zip(pending) {
                                inserted+=conn.execute("INSERT INTO usage_collector_outbox(event_key,payload,created_at_ms) VALUES(?1,?2,?3) ON CONFLICT(event_key) DO UPDATE SET payload=excluded.payload",params![format!("{}:{}",o.source,o.source_event_id),payload,chrono::Utc::now().timestamp_millis()])?;
                            }
                            conn.execute("INSERT INTO usage_import_checkpoints(root_id,file_id,offset,prefix_len,fingerprint,context) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(root_id,file_id) DO UPDATE SET offset=excluded.offset,prefix_len=excluded.prefix_len,fingerprint=excluded.fingerprint,context=excluded.context",params![root_id,file.id,file.checkpoint.offset,file.checkpoint.prefix_len,file.checkpoint.fingerprint,serde_json::to_string(&file.checkpoint.context)?])?;
                            Ok(json!({"inserted":inserted,"duplicates":0}))
                        } else {
                            let watermark:String=conn.query_row("SELECT value FROM usage_meta WHERE key='purge_before_ms'",[],|r|r.get(0))?;
                            if reimport{conn.execute("UPDATE usage_meta SET value='0' WHERE key='purge_before_ms'",[])?;}
                            let result=store::insert_batch(conn,&file.observations)?;
                            for id in &file.checkpoint.context.superseded{store::suppress_source_event(conn,"codex",id,"superseded native response evidence")?;}
                            if reimport{conn.execute("UPDATE usage_meta SET value=?1 WHERE key='purge_before_ms'",[watermark])?;}
                            conn.execute("INSERT INTO usage_import_checkpoints(root_id,file_id,offset,prefix_len,fingerprint,context) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(root_id,file_id) DO UPDATE SET offset=excluded.offset,prefix_len=excluded.prefix_len,fingerprint=excluded.fingerprint,context=excluded.context",params![root_id,file.id,file.checkpoint.offset,file.checkpoint.prefix_len,file.checkpoint.fingerprint,serde_json::to_string(&file.checkpoint.context)?])?;Ok(result)
                        }
                    })();
                    match commit {
                        Ok(result)=>{conn.execute_batch("RELEASE usage_import_commit")?;counts.imported+=result.get("inserted").and_then(Value::as_u64).or_else(||result.get("imported").and_then(Value::as_u64)).unwrap_or(file.observations.len() as u64);counts.duplicate+=result.get("duplicates").and_then(Value::as_u64).or_else(||result.get("duplicate").and_then(Value::as_u64)).unwrap_or(0);counts.duplicate+=file.counts.duplicate;counts.skipped+=file.counts.skipped+result.get("purged").and_then(Value::as_u64).unwrap_or(0);counts.failed+=file.counts.failed;counts.unsupported+=file.counts.unsupported;counts.deferred+=file.counts.deferred;},
                        Err(_)=>{conn.execute_batch("ROLLBACK TO usage_import_commit; RELEASE usage_import_commit")?;counts.deferred+=1;error=Some(if outbox{"outbox full or storage unavailable"}else{"storage unavailable"});break},
                    }
                }
            }
            if counts.failed>0 && error.is_none(){error=Some("one or more records unavailable or invalid")}
            conn.execute("UPDATE usage_import_roots SET last_scan_at_ms=?2,last_error=?3,imported=imported+?4,duplicate=duplicate+?5,skipped=skipped+?6,unsupported=unsupported+?7,failed=failed+?8 WHERE id=?1",params![root_id,chrono::Utc::now().timestamp_millis(),error,counts.imported,counts.duplicate,counts.skipped,counts.unsupported,counts.failed])?;
            Ok(json!({"counts":counts,"last_error":error}))
        }).await?;
        all.push(json!({"source":root.source,"report":result}));
    }
    Ok(json!({"scans":all,"status":status(store).await?}))
}
pub async fn backfill(store: &Store, source: Option<String>) -> Result<Value> {
    if let Some(source) = &source {
        source_ok(source)?
    }
    init(store).await?;
    let filter = source.clone();
    store.call(move |conn|{conn.execute("DELETE FROM usage_import_checkpoints WHERE root_id IN (SELECT id FROM usage_import_roots WHERE enabled=1 AND (?1 IS NULL OR source=?1))",[filter])?;Ok(())}).await?;
    scan_mode(store, source, false, true).await
}
/// Opening the database already runs the startup step; report both passes together.
pub async fn reprice(s: &Store) -> Result<Value> {
    let now = s.call(store::reprice).await?;
    Ok(json!({"repriced": s.startup_repriced() + now}))
}
pub async fn poller(store: Store) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
    loop {
        interval.tick().await;
        if scan(&store, None).await.is_err() {
            tracing::warn!("native usage scan unavailable");
        }
    }
}

#[cfg(test)]
#[path = "import_tests.rs"]
mod tests;
