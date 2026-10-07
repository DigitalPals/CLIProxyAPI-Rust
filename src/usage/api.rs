//! Management-only analytics; ingestion uses a separate credential boundary.
use super::{
    collector, imports,
    store::{Query as UsageQuery, Store},
};
use crate::state::App;
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub fn router() -> Router<Arc<App>> {
    Router::new()
        .route("/usage/summary", get(summary))
        .route("/usage/observations", get(observations))
        .route("/usage/status", get(status))
        .route("/usage/export", get(export))
        .route("/usage/imports", post(configure))
        .route("/usage/imports/scan", post(scan))
        .route("/usage/imports/backfill", post(backfill))
        .route("/usage/collectors", post(enroll))
        .route("/usage/collectors/{id}/rotate", post(rotate))
        .route("/usage/collectors/{id}/revoke", post(revoke))
        .route("/usage/retention", post(purge))
}
fn store(app: &App) -> Result<Store, Box<Response>> {
    app.usage
        .clone()
        .ok_or_else(|| Box::new(error(StatusCode::SERVICE_UNAVAILABLE, "Analytics unavailable or disabled")))
}
fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error":message}))).into_response()
}
fn result(value: anyhow::Result<Value>) -> Response {
    match value {
        Ok(v) => Json(v).into_response(),
        Err(_) => error(
            StatusCode::BAD_REQUEST,
            "Usage operation failed; check the range, settings, permissions and storage health",
        ),
    }
}
fn labels(app: &App, v: &mut Value) {
    let accounts = app.pool.all();
    let cfg = app.cfg();
    if let Some(items) = v.pointer_mut("/facets/accounts").and_then(Value::as_array_mut) {
        for item in items {
            if let Some(a) = accounts.iter().find(|a| item["id"] == a.id) {
                item["label"] = a.label.clone().into();
            }
        }
    }
    if let Some(items) = v.pointer_mut("/facets/clients").and_then(Value::as_array_mut) {
        for item in items {
            if let Some(c) = cfg.named_clients.iter().find(|c| item["id"] == c.id) {
                item["label"] = c.label.clone().into();
            }
        }
    }
    if let Some(items) = v.get_mut("items").and_then(Value::as_array_mut) {
        for item in items {
            if let Some(a) = accounts.iter().find(|a| item["account_id"] == a.id) {
                item["account_label"] = a.label.clone().into();
            }
            if let Some(c) = cfg.named_clients.iter().find(|c| item["client_id"] == c.id) {
                item["client_label"] = c.label.clone().into();
            }
        }
    }
}
async fn summary(State(app): State<Arc<App>>, Query(q): Query<UsageQuery>) -> Response {
    let s = match store(&app) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    match s.query(q).await {
        Ok(mut v) => {
            labels(&app, &mut v);
            Json(v).into_response()
        }
        Err(e) => result(Err(e)),
    }
}
async fn observations(State(app): State<Arc<App>>, Query(q): Query<UsageQuery>) -> Response {
    let s = match store(&app) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    match s.details(q).await {
        Ok(mut v) => {
            labels(&app, &mut v);
            Json(v).into_response()
        }
        Err(e) => result(Err(e)),
    }
}
async fn status(State(app): State<Arc<App>>) -> Response {
    let Some(s) = &app.usage else {
        return Json(json!({"health":{"state":if app.usage_error.is_some(){"error"}else{"disabled"},"message":app.usage_error},"imports":[],"collectors":[]})).into_response();
    };
    let (i, c) = tokio::join!(imports::status(s), collector::status(s));
    match (i,c){(Ok(i),Ok(c))=>Json(json!({"health":s.health(),"imports":i["imports"],"import_candidates":i["candidates"],"collectors":c["collectors"],"pricing":super::pricing::catalogue_info()})).into_response(),_=>error(StatusCode::SERVICE_UNAVAILABLE,"Usage status unavailable")}
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportConfig {
    source: String,
    root: String,
    enabled: bool,
}
async fn configure(State(app): State<Arc<App>>, Json(v): Json<ImportConfig>) -> Response {
    let s = match store(&app) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    result(imports::configure(&s, &v.source, &v.root, v.enabled).await)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    source: Option<String>,
}
async fn scan(State(app): State<Arc<App>>, Json(v): Json<Source>) -> Response {
    let s = match store(&app) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    result(imports::scan(&s, v.source).await)
}
async fn backfill(State(app): State<Arc<App>>, Json(v): Json<Source>) -> Response {
    let s = match store(&app) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    result(imports::backfill(&s, v.source).await)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Label {
    label: String,
}
async fn enroll(State(app): State<Arc<App>>, Json(v): Json<Label>) -> Response {
    let s = match store(&app) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    secret_result(collector::enroll(&s, v.label).await)
}
async fn rotate(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let s = match store(&app) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    secret_result(collector::rotate(&s, id).await)
}
fn secret_result(value: anyhow::Result<Value>) -> Response {
    let mut r = result(value);
    r.headers_mut().insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    r
}
async fn revoke(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    let s = match store(&app) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    result(collector::revoke(&s, id).await)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Purge {
    before: String,
}
async fn purge(State(app): State<Arc<App>>, Json(v): Json<Purge>) -> Response {
    let s = match store(&app) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let before = match chrono::DateTime::parse_from_rfc3339(&v.before) {
        Ok(d) => d.timestamp_millis(),
        Err(_) => return error(StatusCode::BAD_REQUEST, "before must be an RFC3339 timestamp"),
    };
    if before > chrono::Utc::now().timestamp_millis() {
        return error(StatusCode::BAD_REQUEST, "Cannot purge future periods");
    }
    result(s.purge(before).await)
}
pub async fn ingest(State(app): State<Arc<App>>, headers: HeaderMap, body: Bytes) -> Response {
    let s = match store(&app) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    collector::ingest(&s, headers, body).await
}

async fn export(
    State(app): State<Arc<App>>,
    Query(mut fields): Query<std::collections::BTreeMap<String, String>>,
) -> Response {
    let format = fields.remove("format");
    // Reuse the typed URL decoder: serde flatten erases URL scalar parsing and
    // otherwise rejects browser pagination ("50" as a string rather than u32).
    let encoded = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(fields).finish();
    let Ok(uri) = format!("/?{encoded}").parse::<axum::http::Uri>() else {
        return error(StatusCode::BAD_REQUEST, "Invalid export query");
    };
    let q = match Query::<UsageQuery>::try_from_uri(&uri) {
        Ok(Query(q)) => q,
        Err(_) => return error(StatusCode::BAD_REQUEST, "Invalid export query"),
    };
    let s = match store(&app) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let v = match s.details(q).await {
        Ok(v) => v,
        Err(e) => return result(Err(e)),
    };
    let (mime, name, body) = match format.as_deref().unwrap_or("json") {
        "json" => ("application/json", "usage.json", v.to_string()),
        "csv" => {
            let fields = [
                "id",
                "event_at_ms",
                "source",
                "provider",
                "actual_model",
                "account_id",
                "client_id",
                "origin_id",
                "completeness",
                "estimated_cost_nanos",
            ];
            let mut csv = fields.join(",") + ",input,cache_read,cache_write,write_5m,write_1h,output,reasoning\r\n";
            if let Some(items) = v["items"].as_array() {
                for i in items {
                    let mut cells: Vec<String> = fields.iter().map(|f| csv_cell(&i[*f])).collect();
                    for f in ["input", "cache_read", "cache_write", "write_5m", "write_1h", "output", "reasoning"] {
                        cells.push(csv_cell(&i["tokens"][f]));
                    }
                    csv.push_str(&cells.join(","));
                    csv.push_str("\r\n");
                }
            }
            ("text/csv; charset=utf-8", "usage.csv", csv)
        }
        _ => return error(StatusCode::BAD_REQUEST, "Export format must be csv or json"),
    };
    (
        [
            (header::CONTENT_TYPE, mime.to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"")),
            (header::CACHE_CONTROL, "no-store".into()),
        ],
        body,
    )
        .into_response()
}
fn csv_cell(v: &Value) -> String {
    let text = match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        v => v.to_string(),
    };
    let trimmed = text.trim_start_matches(char::is_whitespace);
    let text = if trimmed.starts_with(['=', '+', '-', '@']) { format!("'{text}") } else { text };
    format!("\"{}\"", text.replace('"', "\"\""))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn csv_formula_and_quotes() {
        assert_eq!(csv_cell(&json!("  =cmd()")), "\"'  =cmd()\"");
        assert_eq!(csv_cell(&json!("a\"b")), "\"a\"\"b\"");
        assert_eq!(csv_cell(&Value::Null), "\"\"");
    }
}
