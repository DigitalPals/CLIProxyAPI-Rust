//! Test-only SQL accounting reference and lifecycle diagnostics.
//!
//! The production summary endpoint is retired. Keep this independently computed
//! reference outside release builds so tests continue to verify exact dashboard
//! totals, source conflicts, request/attempt identities, and deduplication.
use super::*;
use chrono::Offset;

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
pub(super) fn summary(conn: &mut Connection, q: &Query) -> Result<Value> {
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
        totals["coverage_basis"] = json!(
            "Source records include revisions, superseded counters and copies; accounting entries are globally selected evidence. A filtered origin can have records whose accounting evidence is selected under another origin."
        );
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
        let column = dimension(column, "o");
        let mut statement=conn.prepare(&format!("SELECT {column},o.source,{AGG} FROM usage_source_entries o INDEXED BY usage_source_entries_event_time WHERE {where_sql} GROUP BY 1,2 ORDER BY COUNT(*) DESC,1,2 LIMIT 500"))?;
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
        let mut raw_statement=conn.prepare(&format!("SELECT {column},{REQUEST_COUNTS} FROM usage_observations o INDEXED BY usage_event_time WHERE {where_sql} AND o.source='proxy' GROUP BY 1 ORDER BY COUNT(*) DESC,1 LIMIT 500"))?;
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
        let column = dimension(column, "o");
        let mut statement=conn.prepare(&format!("SELECT DISTINCT {column} FROM usage_observations o INDEXED BY usage_event_time WHERE {where_sql} AND {column} IS NOT NULL ORDER BY {column} LIMIT 500"))?;
        let rows = statement.query_map(rusqlite::params_from_iter(&values), |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for value in rows {
            let value = value?;
            let label = if name == "clients" {
                collector_label(conn, &value)?.unwrap_or_else(|| value.clone())
            } else {
                value.clone()
            };
            out.push(if labelled { json!({"id":value,"label":label}) } else { json!(value) });
        }
        facets.insert(name.into(), json!(out));
    }
    let raw: String = conn.query_row("SELECT value FROM usage_meta WHERE key='catalogue'", [], |row| row.get(0))?;
    let catalogue: Catalogue = serde_json::from_str(&raw)?;
    let combined = combined_summary(conn, q, &where_sql, &values, &date_expression)?;
    Ok(
        json!({"range":{"start":r.start,"end":r.end,"timezone":r.tz.to_string()},"proxy":proxy,"sources":sources,"combined":combined,"trend":trend,"group_by":q.group_by.as_deref().unwrap_or("day"),"breakdowns":breakdowns,"facets":facets,"pricing":{"version":catalogue.version,"verified_at":catalogue.verified_at,"basis":"API list-price equivalent estimate; source totals may overlap; usage older than the catalogue is priced at today's rates as a backdated current-rate equivalent, not what was paid at the time"},"reconciliation":{"basis":"proxy evidence only","cross_source_grand_total":null,"identity":"provider response ID only; native stable source event ID within source","conflicts":conflicts}}),
    )
}
fn combined_summary(
    conn: &Connection,
    q: &Query,
    where_sql: &str,
    values: &[SqlValue],
    date_expression: &str,
) -> Result<Value> {
    let set = combined(where_sql);
    let params = || rusqlite::params_from_iter(values);
    let mut totals = aggregate(conn, "usage_entries", &set, values)?;
    let (history_only, weak_identity): (i64, i64) = conn.query_row(&format!("SELECT COALESCE(SUM(o.source<>'proxy'),0),COALESCE(SUM(o.source<>'proxy' AND COALESCE(o.response_id,'')=''),0) FROM usage_entries o INDEXED BY usage_entries_event_time WHERE {set}"),params(),|r|Ok((r.get(0)?,r.get(1)?)))?;
    let matched: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM usage_entries o INDEXED BY usage_entries_event_time WHERE {where_sql} AND o.source<>'proxy' AND EXISTS(SELECT 1 FROM usage_entries p INDEXED BY usage_entries_association WHERE p.association_key=o.association_key AND p.source='proxy')"),params(),|r|r.get(0))?;
    totals["history_only"] = json!(history_only);
    totals["matched"] = json!(matched);
    totals["weak_identity"] = json!(weak_identity);
    let proxy_first: Option<i64> = conn.query_row("SELECT event_at_ms FROM usage_observations INDEXED BY usage_event_time WHERE source='proxy' ORDER BY event_at_ms LIMIT 1",[],|r|r.get(0)).optional()?;
    let stack = q.stack.as_deref().unwrap_or("provider");
    let mut trend = Vec::new();
    let mut statement = conn.prepare(&format!("SELECT {date_expression},o.{stack},{AGG} FROM usage_entries o INDEXED BY usage_entries_event_time WHERE {set} GROUP BY 1,2 ORDER BY 1,2"))?;
    for row in statement.query_map(params(), |row| {
        let mut value = aggregate_row(row, 2)?;
        value["date"] = json!(row.get::<_, String>(0)?);
        value["group"] = json!(row.get::<_, Option<String>>(1)?);
        Ok(value)
    })? {
        trend.push(row?);
    }
    let mut breakdowns = serde_json::Map::new();
    for (name, column) in
        [("provider", "provider"), ("model", "model"), ("account", "account_id"), ("client", "client_id")]
    {
        let column = dimension(column, "o");
        let mut statement = conn.prepare(&format!("SELECT {column},COUNT(DISTINCT o.account_id),MIN(o.provider),{AGG} FROM usage_entries o INDEXED BY usage_entries_event_time WHERE {set} GROUP BY 1 ORDER BY usage_sum(o.cost_nanos) DESC,COUNT(*) DESC,1 LIMIT 500"))?;
        let mut out = Vec::new();
        for row in statement.query_map(params(), |row| {
            let mut value = aggregate_row(row, 3)?;
            value["id"] = json!(row.get::<_, Option<String>>(0)?);
            value["accounts"] = json!(row.get::<_, i64>(1)?);
            value["provider"] = json!(row.get::<_, Option<String>>(2)?);
            Ok(value)
        })? {
            out.push(row?);
        }
        breakdowns.insert(name.into(), json!(out));
    }
    Ok(
        json!({"basis":"One entry per provider response. Imported entries that share a response ID with a proxy entry are excluded.","totals":totals,"proxy_first_event_at_ms":proxy_first,"stack":stack,"trend":trend,"breakdowns":breakdowns}),
    )
}
