//! One streaming pass for the metrics displayed by the Usage page. Request/attempt
//! identities and source evidence remain available in the ledger and raw records.
use super::*;
use std::collections::BTreeMap;

const METRICS: [&str; 8] =
    ["estimated_cost_nanos", "input", "cache_read", "cache_write", "write_5m", "write_1h", "output", "reasoning"];
const DIMS: [&str; 4] = ["provider", "model", "account", "client"];

#[derive(Default)]
struct Metrics {
    observations: i64,
    sums: [Option<i128>; 8],
    overflow: [bool; 8],
    known: [i64; 8],
    partial: i64,
    missing: i64,
    pricing_partial: i64,
}
impl Metrics {
    fn add(&mut self, amounts: &[Option<i64>; 8], completeness: &str, pricing_partial: bool) {
        self.observations += 1;
        self.partial += i64::from(completeness == "partial");
        self.missing += i64::from(completeness == "missing");
        self.pricing_partial += i64::from(pricing_partial);
        for (i, amount) in amounts.iter().enumerate() {
            if let Some(amount) = amount {
                self.known[i] += 1;
                if !self.overflow[i] {
                    self.sums[i] = self.sums[i].unwrap_or(0).checked_add(i128::from(*amount));
                    self.overflow[i] = self.sums[i].is_none();
                }
            }
        }
    }
    fn value(&self) -> Value {
        let sums = self.sums.map(|n| n.and_then(|n| i64::try_from(n).ok()));
        let overflow: Vec<_> = METRICS
            .iter()
            .enumerate()
            .filter_map(|(i, name)| (self.known[i] > 0 && sums[i].is_none()).then_some(*name))
            .collect();
        let tokens: serde_json::Map<_, _> =
            METRICS.iter().enumerate().skip(1).map(|(i, name)| ((*name).into(), json!(sums[i]))).collect();
        let missing: serde_json::Map<_, _> = [1, 2, 3, 6, 7]
            .into_iter()
            .map(|i| (METRICS[i].into(), json!(self.observations - self.known[i])))
            .collect();
        json!({"observations":self.observations,"estimated_cost_nanos":sums[0],
            "known_cost_nanos":if self.known[0]>0 && sums[0].is_none(){None}else{Some(sums[0].unwrap_or(0))},
            "aggregation_overflow":!overflow.is_empty(),"aggregation_overflow_fields":overflow,
            "unpriced":self.observations-self.known[0],"partial":self.partial,"missing_usage":self.missing,
            "pricing_partial":self.pricing_partial,"tokens":tokens,"missing_token_counts":missing})
    }
}
#[derive(Default)]
struct Group {
    metrics: Metrics,
    accounts: BTreeSet<String>,
    provider: Option<String>,
}

// Repeated dimensions dominate large reads. Borrow their keys for lookups and
// allocate an owned key only when a group is first encountered.
fn update_group<T: Default>(groups: &mut BTreeMap<Option<String>, T>, key: &Option<String>, add: impl FnOnce(&mut T)) {
    if let Some(group) = groups.get_mut(key) {
        add(group);
    } else {
        let mut group = T::default();
        add(&mut group);
        groups.insert(key.clone(), group);
    }
}

pub(super) fn summary(conn: &Connection, q: &Query) -> Result<Value> {
    let r = range(q)?;
    let (where_sql, values) = filter(q, &r, "o");
    let client = dimension("client_id", "o");
    let mut totals = Metrics::default();
    let (mut matched, mut history_only, mut weak_identity) = (0_i64, 0_i64, 0_i64);
    let mut trends: [BTreeMap<NaiveDate, BTreeMap<Option<String>, Metrics>>; 2] = Default::default();
    let mut breakdowns: [BTreeMap<Option<String>, Group>; 4] = Default::default();
    // Match against the entire journal, including proxy evidence outside this range/filter.
    // Only scalar accounting columns are read; no payload or pricing JSON is parsed.
    let mut statement = conn.prepare(&format!(
        "SELECT CASE WHEN o.source='proxy' THEN 0 ELSE EXISTS(SELECT 1 FROM usage_entries p INDEXED BY usage_entries_association WHERE p.association_key=o.association_key AND p.source='proxy') END,
         o.source<>'proxy',COALESCE(o.response_id,'')='',o.event_at_ms,o.provider,o.model,o.account_id,{client},
         o.cost_nanos,o.input,o.cache_read,o.cache_write,o.write_5m,o.write_1h,o.output,o.reasoning,o.completeness,o.pricing_partial
         FROM usage_entries o INDEXED BY usage_entries_event_time WHERE {where_sql}"
    ))?;
    let mut rows = statement.query(rusqlite::params_from_iter(&values))?;
    while let Some(row) = rows.next()? {
        read::check()?;
        if row.get::<_, bool>(0)? {
            matched += 1;
            continue;
        }
        let history = row.get::<_, bool>(1)?;
        history_only += i64::from(history);
        weak_identity += i64::from(history && row.get::<_, bool>(2)?);
        let date = DateTime::from_timestamp_millis(row.get(3)?)
            .context("usage timestamp out of bounds")?
            .with_timezone(&r.tz)
            .date_naive();
        let dims: [Option<String>; 4] = [row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?];
        let mut amounts = [None; 8];
        for (i, amount) in amounts.iter_mut().enumerate() {
            *amount = row.get(8 + i)?;
        }
        let completeness = row.get_ref(16)?.as_str()?;
        let pricing_partial = row.get::<_, Option<i64>>(17)? == Some(1);
        totals.add(&amounts, completeness, pricing_partial);
        for (i, trend) in trends.iter_mut().enumerate() {
            update_group(trend.entry(date).or_default(), &dims[i], |metrics| {
                metrics.add(&amounts, completeness, pricing_partial)
            });
        }
        for (i, breakdown) in breakdowns.iter_mut().enumerate() {
            update_group(breakdown, &dims[i], |group| {
                group.metrics.add(&amounts, completeness, pricing_partial);
                if let Some(account) = &dims[2]
                    && !group.accounts.contains(account)
                {
                    group.accounts.insert(account.clone());
                }
                if group.provider.is_none() || dims[0] < group.provider {
                    group.provider.clone_from(&dims[0]);
                }
            });
        }
    }
    let mut totals = totals.value();
    totals["matched"] = json!(matched);
    totals["history_only"] = json!(history_only);
    totals["weak_identity"] = json!(weak_identity);
    let mut trend_values: [Vec<Value>; 2] = Default::default();
    for (output, groups) in trend_values.iter_mut().zip(trends) {
        for (date, groups) in groups {
            for (group, metrics) in groups {
                read::check()?;
                let mut value = metrics.value();
                value["date"] = json!(date.to_string());
                value["group"] = json!(group);
                output.push(value);
            }
        }
    }
    let trends = trend_values;
    let mut out = serde_json::Map::new();
    for (name, groups) in DIMS.into_iter().zip(breakdowns) {
        let mut group_values = Vec::with_capacity(groups.len());
        for (id, group) in groups {
            read::check()?;
            let mut value = group.metrics.value();
            value["id"] = json!(id);
            value["accounts"] = json!(group.accounts.len());
            value["provider"] = json!(group.provider);
            group_values.push(value);
        }
        let mut groups = group_values;
        read::check()?;
        groups.sort_by(|a, b| {
            b["estimated_cost_nanos"]
                .as_i64()
                .cmp(&a["estimated_cost_nanos"].as_i64())
                .then(b["observations"].as_i64().cmp(&a["observations"].as_i64()))
                .then(a["id"].as_str().cmp(&b["id"].as_str()))
        });
        read::check()?;
        groups.truncate(500);
        out.insert(name.into(), json!(groups));
    }
    let proxy_first: Option<i64> = conn
        .query_row("SELECT event_at_ms FROM usage_observations INDEXED BY usage_event_time WHERE source='proxy' ORDER BY event_at_ms LIMIT 1",[],|r|r.get(0))
        .optional()?;
    let raw: String = conn.query_row("SELECT value FROM usage_meta WHERE key='catalogue'", [], |row| row.get(0))?;
    let catalogue: Catalogue = serde_json::from_str(&raw)?;
    let stack = q.stack.as_deref().unwrap_or("provider");
    let facets = facets(conn, &where_sql, &values)?;
    read::check()?;
    Ok(json!({"range":{"start":r.start,"end":r.end,"timezone":r.tz.to_string()},
        "combined":{"basis":"One entry per provider response. Imported entries that share a response ID with a proxy entry are excluded.",
            "totals":totals,"proxy_first_event_at_ms":proxy_first,"stack":stack,
            "trend":trends[usize::from(stack=="model")],"trends":{"provider":trends[0],"model":trends[1]},"breakdowns":out},
        "facets":facets,
        "pricing":{"version":catalogue.version,"verified_at":catalogue.verified_at,
            "basis":"API list-price equivalent estimate; usage older than the catalogue is priced at today's rates as a backdated current-rate equivalent, not what was paid at the time"}}))
}

fn facets(conn: &Connection, where_sql: &str, values: &[SqlValue]) -> Result<Value> {
    let mut facets: [BTreeSet<String>; 5] = Default::default();
    // Distinct dimension tuples keep memory tied to facets, not record count, and
    // retain raw-only origins whose canonical evidence is selected elsewhere.
    let client = dimension("client_id", "o");
    let mut statement = conn.prepare(&format!(
        "SELECT DISTINCT o.provider,o.model,o.account_id,{client},o.source FROM usage_observations o INDEXED BY usage_event_time WHERE {where_sql}"
    ))?;
    let mut rows = statement.query(rusqlite::params_from_iter(values))?;
    while let Some(row) = rows.next()? {
        read::check()?;
        for (i, facet) in facets.iter_mut().enumerate() {
            if let Some(value) = row.get::<_, Option<String>>(i)? {
                facet.insert(value);
            }
        }
    }
    let mut out = serde_json::Map::new();
    for (i, (name, values)) in
        ["providers", "models", "accounts", "clients", "sources"].into_iter().zip(facets).enumerate()
    {
        let mut items = Vec::new();
        for value in values.into_iter().take(500) {
            read::check()?;
            items.push(if matches!(i, 2 | 3) {
                let label = if i == 3 {
                    collector_label(conn, &value)?.unwrap_or_else(|| value.clone())
                } else {
                    value.clone()
                };
                json!({"id":value,"label":label})
            } else {
                json!(value)
            });
        }
        out.insert(name.into(), json!(items));
    }
    read::check()?;
    Ok(json!(out))
}
