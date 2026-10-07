//! One-time imports of the normalized CPA usage keeper v1.15.4 SQLite schema.
//! Only selected metadata is read. No keeper credentials or raw messages are copied.
use super::{
    imports::hash,
    pricing::{Catalogue, Snapshot},
    store::{self, APPLICATION_ID, VERSION},
    types::{MAX_TOKENS, Observation, Tokens, valid_label},
};
use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags, Row, params};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, time::Duration};

const PARSER: &str = "keeper-v1.15.4-2026-10-07-v1";
const SELECT: &str = "SELECT id,timestamp,provider,model,model_alias,auth_type,auth_index,executor_type,service_tier,response_service_tier,failed,generate,input_tokens,output_tokens,reasoning_tokens,cache_read_tokens,cache_creation_tokens,total_tokens FROM usage_events ORDER BY id";
const BATCH: usize = 100;
const CACHE_ESTIMATE_METHOD: &str = "keeper-cache-5m-2026-10-07-v1";
// Force the primary rowid range: the source/origin index would rescan and sort
// the entire imported history for every small batch.
const CACHE_ESTIMATE_SELECT: &str = "SELECT id,payload,snapshot_json,canonical_key,model FROM usage_observations NOT INDEXED WHERE id>?1 AND source='proxy' AND provider='anthropic' AND origin_id=?2 AND cost_nanos IS NULL AND pricing_basis IN ('unknown_cache_write_ttl','local_override:unknown_cache_write_ttl') ORDER BY id LIMIT 100";

fn label(row: &Row<'_>, index: usize) -> Result<Option<String>> {
    let value: Option<String> = row.get(index).map_err(|_| anyhow!("invalid_metadata"))?;
    let value = value.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    if let Some(s) = &value {
        valid_label(s).map_err(|_| anyhow!("invalid_metadata"))?;
    }
    Ok(value)
}
fn number(row: &Row<'_>, index: usize) -> Result<Option<u64>> {
    row.get::<_, Option<i64>>(index)
        .map_err(|_| anyhow!("invalid_tokens"))?
        .map(|n| u64::try_from(n).ok().filter(|n| *n <= MAX_TOKENS).ok_or_else(|| anyhow!("invalid_tokens")))
        .transpose()
}
fn boolean(row: &Row<'_>, index: usize) -> Result<Option<bool>> {
    match row.get::<_, Option<i64>>(index).map_err(|_| anyhow!("invalid_metadata"))? {
        Some(0) => Ok(Some(false)),
        Some(1) => Ok(Some(true)),
        None => Ok(None),
        _ => bail!("invalid_metadata"),
    }
}
fn observation(row: &Row<'_>, origin: &str) -> Result<Option<Observation>> {
    if boolean(row, 11)? != Some(true) {
        return Ok(None);
    }
    let id: i64 = row.get(0).map_err(|_| anyhow!("invalid_identity"))?;
    if id <= 0 {
        bail!("invalid_identity");
    }
    let timestamp: String = row.get(1).map_err(|_| anyhow!("invalid_timestamp"))?;
    let event_at_ms =
        DateTime::parse_from_rfc3339(&timestamp).map_err(|_| anyhow!("invalid_timestamp"))?.timestamp_millis();
    let provider = label(row, 2)?.ok_or_else(|| anyhow!("unsupported_executor"))?;
    let executor = label(row, 7)?.ok_or_else(|| anyhow!("unsupported_executor"))?;
    // Both handlers store inclusive input and output after keeper normalization.
    // Other executors/older schemas need their own verified normalization contract.
    let provider = match (provider.as_str(), executor.as_str()) {
        ("codex" | "openai", "CodexExecutor" | "CodexWebsocketsExecutor") => "openai",
        ("claude" | "anthropic", "ClaudeExecutor") => "anthropic",
        _ => bail!("unsupported_executor"),
    };
    // request_id/event_key are reused across real events. The keeper row identity
    // and explicit origin remain stable across retries and backup-file copies.
    let identity = hash(format!("{origin}\0{id}").as_bytes());
    let mut o = Observation::new("proxy", format!("keeper:{identity}"), provider, event_at_ms);
    o.origin_id = origin.to_owned();
    o.parser_version = PARSER.into();
    o.actual_model = label(row, 3)?.filter(|m| m != "unknown");
    o.requested_model = label(row, 4)?;
    o.auth_type = label(row, 5)?;
    o.account_id =
        label(row, 6)?.map(|id| format!("keeper-account:{}", hash(format!("{origin}\0{provider}\0{id}").as_bytes())));
    // A requested priority tier can resolve to default. Prefer the response tier.
    o.service_tier = label(row, 9)?.or(label(row, 8)?);
    o.logical_success = boolean(row, 10)?.map(|failed| !failed);
    let (input, output, reasoning, read, write, total) =
        (number(row, 12)?, number(row, 13)?, number(row, 14)?, number(row, 15)?, number(row, 16)?, number(row, 17)?);
    let uncached = match (input, read, write) {
        (Some(i), Some(r), Some(w)) => {
            Some(i.checked_sub(r).and_then(|n| n.checked_sub(w)).ok_or_else(|| anyhow!("invalid_tokens"))?)
        }
        _ => None,
    };
    if let (Some(i), Some(o), Some(t)) = (input, output, total)
        && i.checked_add(o) != Some(t)
    {
        bail!("inconsistent_total");
    }
    // Failed all-zero records reflect unavailable usage, not evidence of a free call.
    if o.logical_success == Some(false)
        && [input, output, reasoning, read, write, total].iter().all(|n| n.is_none_or(|v| v == 0))
    {
        o.completeness = "missing".into();
    } else {
        o.tokens =
            Tokens { input: uncached, cache_read: read, cache_write: write, output, reasoning, ..Default::default() };
        // TTL, inference region, response identity and request/attempt grouping were
        // not retained. Never manufacture TTL splits or lifecycle identities.
        o.completeness = "partial".into();
        if let Some(n) = input {
            o.numeric_metadata.insert("input_total".into(), n);
        }
        if let Some(n) = total {
            o.numeric_metadata.insert("total_tokens".into(), n);
        }
    }
    o.validate().map_err(|_| anyhow!("invalid_observation"))?;
    Ok(Some(o))
}

/// Price only imported keeper observations with known tokens and unknown cache lifetime.
/// Temporary lifetime splits are used for calculation, never written as reported tokens.
fn cache_estimate(catalogue: &Catalogue, payload: &str, original: &str, origin: &str, at: i64) -> Result<Value> {
    let o: Observation = serde_json::from_str(payload)?;
    let previous: Snapshot = serde_json::from_str(original)?;
    o.validate().map_err(|e| anyhow!(e))?;
    if o.source != "proxy"
        || o.provider != "anthropic"
        || o.origin_id != origin
        || o.parser_version != PARSER
        || o.tokens.write_5m.is_some()
        || o.tokens.write_1h.is_some()
        || previous.cost_nanos.is_some()
        || previous.basis.trim_start_matches("local_override:") != "unknown_cache_write_ttl"
        || previous.catalogue_version != catalogue.version
    {
        bail!("incompatible keeper cache evidence");
    }
    let write = o.tokens.cache_write.filter(|n| *n > 0).ok_or_else(|| anyhow!("missing cache-write tokens"))?;
    let mut assumed = o.clone();
    assumed.tokens.write_5m = Some(write);
    assumed.tokens.write_1h = Some(0);
    let mut short = catalogue.price(&assumed);
    assumed.tokens.write_5m = Some(0);
    assumed.tokens.write_1h = Some(write);
    let long = catalogue.price(&assumed);
    let (Some(short_cost), Some(long_cost)) = (short.cost_nanos, long.cost_nanos) else {
        bail!("keeper cache estimate requires both lifetime rates");
    };
    short.basis = if short.local_override {
        "local_override:historical_cache_write_estimate"
    } else {
        "historical_cache_write_estimate"
    }
    .into();
    short.partial = true;
    short.assumptions.push("historical_cache_write_lifetime_5_minutes_assumed".into());
    let mut snapshot = serde_json::to_value(short)?;
    snapshot["historical_estimate"] = json!({
        "method":CACHE_ESTIMATE_METHOD,"assumed_cache_write_lifetime":"5m",
        "cache_lifetime_min_cost_nanos":short_cost.min(long_cost),
        "cache_lifetime_max_cost_nanos":short_cost.max(long_cost),
        "one_hour_cost_nanos":long_cost,"original_basis":previous.basis,
        "original_snapshot_sha256":hash(original.as_bytes()),"applied_at_ms":at
    });
    Ok(snapshot)
}

/// Opt-in historical estimate, with a read-only preview and atomic batches for live WAL databases.
/// Already-priced records, native capture, original payloads and all token columns stay unchanged.
pub fn estimate_cache(database: &Path, origin: &str, apply: bool) -> Result<Value> {
    valid_label(origin).map_err(|_| anyhow!("invalid origin"))?;
    if !origin.starts_with("keeper:") || origin.len() <= "keeper:".len() {
        bail!("origin must be a stable keeper: identity");
    }
    let flags = if apply { OpenFlags::SQLITE_OPEN_READ_WRITE } else { OpenFlags::SQLITE_OPEN_READ_ONLY };
    let mut conn = Connection::open_with_flags(database, flags)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    if conn.query_row("PRAGMA application_id", [], |r| r.get::<_, i64>(0))? != APPLICATION_ID
        || conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? != VERSION
    {
        bail!("destination must be an existing Fusebox usage database with the supported schema");
    }
    if apply {
        conn.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")?;
    } else {
        conn.execute_batch("PRAGMA query_only=ON; BEGIN;")?;
    }
    let catalogue: Catalogue =
        serde_json::from_str(
            &conn.query_row("SELECT value FROM usage_meta WHERE key='catalogue'", [], |r| r.get::<_, String>(0))?,
        )?;
    let at = Utc::now().timestamp_millis();
    let mut cursor = 0_i64;
    let mut estimated = 0_u64;
    let mut updated = 0_u64;
    // Count, assumed cost, minimum and maximum over the known cache lifetime rates.
    let mut totals = [0_i128; 4];
    let mut models = BTreeMap::<String, [i128; 4]>::new();
    loop {
        // Immediate writer ownership avoids a WAL read-to-write upgrade race.
        // Preview savepoints stay inside one read-only snapshot.
        if apply {
            conn.execute_batch("BEGIN IMMEDIATE;")?;
        }
        let tx = conn.savepoint()?;
        let rows = tx
            .prepare(CACHE_ESTIMATE_SELECT)?
            .query_map(params![cursor, origin], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let Some(last) = rows.last() else {
            tx.commit()?;
            if apply {
                conn.execute_batch("COMMIT;")?;
            }
            break;
        };
        cursor = last.0;
        for (id, payload, original, key, model) in rows {
            let snapshot = cache_estimate(&catalogue, &payload, &original, origin, at)?;
            let cost = snapshot["cost_nanos"].as_i64().ok_or_else(|| anyhow!("invalid estimated cost"))?;
            let evidence = &snapshot["historical_estimate"];
            let values = [
                1,
                i128::from(cost),
                i128::from(evidence["cache_lifetime_min_cost_nanos"].as_i64().unwrap()),
                i128::from(evidence["cache_lifetime_max_cost_nanos"].as_i64().unwrap()),
            ];
            for sums in [&mut totals, models.entry(model).or_default()] {
                for (sum, value) in sums.iter_mut().zip(values) {
                    *sum = sum.checked_add(value).ok_or_else(|| anyhow!("estimate aggregate overflow"))?;
                }
            }
            estimated += 1;
            if apply {
                let changed = tx.execute("UPDATE usage_observations SET cost_nanos=?2,pricing_basis=?3,snapshot_json=?4 WHERE id=?1 AND cost_nanos IS NULL AND snapshot_json=?5", params![id,cost,snapshot["basis"].as_str().unwrap(),serde_json::to_string(&snapshot)?,original])?;
                if changed != 1 {
                    bail!("keeper estimate evidence changed");
                }
                store::rebuild_entries(&tx, &key, "proxy")?;
                updated += 1;
            }
        }
        tx.commit()?;
        if apply {
            conn.execute_batch("COMMIT;")?;
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    let model_totals = models.into_iter().map(|(model,s)| {
        Ok((model,json!({"observations":i64::try_from(s[0])?,"estimated_cost_nanos":i64::try_from(s[1])?,"cache_lifetime_min_cost_nanos":i64::try_from(s[2])?,"cache_lifetime_max_cost_nanos":i64::try_from(s[3])?})))
    }).collect::<Result<BTreeMap<_,_>>>()?;
    Ok(json!({
        "mode":if apply {"applied"} else {"preview"},"origin":origin,"method":CACHE_ESTIMATE_METHOD,
        "estimated":estimated,"updated":updated,"catalogue_version":catalogue.version,
        "estimated_cost_nanos":i64::try_from(totals[1])?,
        "cache_lifetime_min_cost_nanos":i64::try_from(totals[2])?,
        "cache_lifetime_max_cost_nanos":i64::try_from(totals[3])?,"models":model_totals,
        "assumption":"All cache writes with unknown lifetime use the five-minute rate. Original reported tokens remain unchanged.",
        "completed_at_utc":Utc::now().to_rfc3339()
    }))
}

/// Preview never opens a writer or starts a Store (which would run retention/repricing).
/// Apply uses the same pricing/deduplication path as native capture, with short commits
/// and a yield between batches so a live proxy writer can keep committing.
pub fn import(database: &Path, input_database: &Path, origin: &str, apply: bool) -> Result<Value> {
    valid_label(origin).map_err(|_| anyhow!("invalid origin"))?;
    if !origin.starts_with("keeper:") || origin.len() <= "keeper:".len() {
        bail!("origin must be a stable keeper: identity");
    }
    if std::fs::canonicalize(database)? == std::fs::canonicalize(input_database)? {
        bail!("source and destination must be different databases");
    }
    let source = Connection::open_with_flags(input_database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    source.busy_timeout(Duration::from_secs(5))?;
    source.execute_batch("PRAGMA query_only=ON; BEGIN;")?;
    let mode = if apply { OpenFlags::SQLITE_OPEN_READ_WRITE } else { OpenFlags::SQLITE_OPEN_READ_ONLY };
    let mut destination = Connection::open_with_flags(database, mode)?;
    destination.busy_timeout(Duration::from_secs(5))?;
    if destination.query_row("PRAGMA application_id", [], |r| r.get::<_, i64>(0))? != APPLICATION_ID
        || destination.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? != VERSION
    {
        bail!("destination must be an existing Fusebox usage database with the supported schema");
    }
    let catalogue: Catalogue = serde_json::from_str(&destination.query_row(
        "SELECT value FROM usage_meta WHERE key='catalogue'",
        [],
        |r| r.get::<_, String>(0),
    )?)?;
    let watermark: i64 = destination.query_row(
        "SELECT CAST(value AS INTEGER) FROM usage_meta WHERE key='purge_before_ms'",
        [],
        |r| r.get(0),
    )?;
    let mut statement = source.prepare(SELECT).map_err(|_| anyhow!("unsupported keeper schema"))?;
    let mut skipped = BTreeMap::<String, u64>::new();
    let (mut rows, mut eligible, mut priced, mut cost) = (0_u64, 0_u64, 0_u64, 0_i128);
    let (mut first, mut last): (Option<i64>, Option<i64>) = (None, None);
    let mut pricing_reasons = BTreeMap::<String, u64>::new();
    let mut tokens = BTreeMap::<String, u64>::new();
    let mut cursor = statement.query([])?;
    // Validate and calculate the full plan before the first destination mutation.
    while let Some(row) = cursor.next()? {
        rows += 1;
        match observation(row, origin) {
            Ok(Some(o)) if o.event_at_ms >= watermark => {
                eligible += 1;
                first = Some(first.map_or(o.event_at_ms, |n| n.min(o.event_at_ms)));
                last = Some(last.map_or(o.event_at_ms, |n| n.max(o.event_at_ms)));
                let snapshot = catalogue.price(&o);
                *pricing_reasons.entry(snapshot.basis).or_default() += 1;
                if let Some(n) = snapshot.cost_nanos {
                    priced += 1;
                    cost += i128::from(n);
                }
                for (name, value) in [
                    ("input", o.tokens.input),
                    ("cache_read", o.tokens.cache_read),
                    ("cache_write", o.tokens.cache_write),
                    ("output", o.tokens.output),
                    ("reasoning", o.tokens.reasoning),
                    ("total", o.tokens.total()),
                ] {
                    if let Some(n) = value {
                        let sum = tokens.entry(name.to_owned()).or_default();
                        *sum = sum.checked_add(n).ok_or_else(|| anyhow!("token aggregate overflow"))?;
                    }
                }
            }
            Ok(Some(_)) => *skipped.entry("outside_retention".into()).or_default() += 1,
            Ok(None) => *skipped.entry("not_generation".into()).or_default() += 1,
            Err(e) => *skipped.entry(e.to_string()).or_default() += 1,
        }
    }
    drop(cursor);
    let overlapping_native: i64 = match (first, last) {
        (Some(first), Some(last)) => destination.query_row(
            "SELECT count(*) FROM usage_observations WHERE source='proxy' AND origin_id NOT LIKE 'keeper:%' AND event_at_ms BETWEEN ?1 AND ?2",
            rusqlite::params![first, last],
            |r| r.get(0),
        )?,
        _ => 0,
    };
    let mut result = json!({
        "mode":if apply {"applied"} else {"preview"},"origin":origin,"parser_version":PARSER,
        "source_rows":rows,"eligible":eligible,"skipped":skipped,"priced":priced,"unpriced":eligible-priced,
        "estimated_cost_nanos":i64::try_from(cost).map_err(|_| anyhow!("cost aggregate overflow"))?,
        "catalogue_version":catalogue.version,"pricing_reasons":pricing_reasons,"tokens":tokens,
        "first_event_at_ms":first,"last_event_at_ms":last,"inserted":0,"duplicates":0,"purged":0,
        "first_event_utc":first.and_then(DateTime::from_timestamp_millis).map(|t| t.to_rfc3339()),
        "last_event_utc":last.and_then(DateTime::from_timestamp_millis).map(|t| t.to_rfc3339()),
        "cost_basis":"current catalogue API equivalents; incomplete records and unknown cache TTL remain unpriced",
        "overlapping_native_observations":overlapping_native,
        "completed_at_utc":Utc::now().to_rfc3339()
    });
    if apply {
        if overlapping_native > 0 {
            bail!(
                "keeper history overlaps native proxy observations; reliable response identities are required to reconcile them"
            );
        }
        destination.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")?;
        let mut batch = Vec::with_capacity(BATCH);
        let mut cursor = statement.query([])?;
        while let Some(row) = cursor.next()? {
            if let Ok(Some(o)) = observation(row, origin)
                && o.event_at_ms >= watermark
            {
                batch.push(o);
            }
            if batch.len() == BATCH {
                commit(&mut destination, &mut batch, &mut result)?;
            }
        }
        commit(&mut destination, &mut batch, &mut result)?;
    }
    result["completed_at_utc"] = json!(Utc::now().to_rfc3339());
    Ok(result)
}
fn commit(connection: &mut Connection, batch: &mut Vec<Observation>, result: &mut Value) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let saved = store::insert_batch(connection, batch)?;
    for key in ["inserted", "duplicates", "purged"] {
        result[key] = json!(result[key].as_u64().unwrap_or(0) + saved[key].as_u64().unwrap_or(0));
    }
    batch.clear();
    std::thread::sleep(Duration::from_millis(10));
    Ok(())
}

#[cfg(test)]
#[path = "keeper_tests.rs"]
mod tests;
