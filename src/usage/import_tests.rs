use super::*;
use std::io::Write;

struct TestDir(PathBuf);
impl TestDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("fusebox-import-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn store(&self) -> Store {
        Store::open(&self.0.join("usage.sqlite3"), 128, 3650, None).unwrap()
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}
fn meta(provider: &str) -> Value {
    json!({"timestamp":now(),"type":"session_meta","payload":{"id":"synthetic-thread","session_id":"synthetic-root","model_provider":provider,"cwd":"PRIVATE-PROJECT-MARKER"}})
}
fn context() -> Value {
    json!({"timestamp":now(),"type":"turn_context","payload":{"turn_id":"turn-one","model":"gpt-6.1-sol","cwd":"PRIVATE-PROJECT-MARKER","base_instructions":"PRIVATE-PROMPT-MARKER"}})
}
fn usage() -> Value {
    json!({"input_tokens":1000,"cached_input_tokens":600,"cache_write_input_tokens":100,"output_tokens":200,"reasoning_output_tokens":80,"total_tokens":1200})
}
fn modern() -> Value {
    json!({"timestamp":now(),"type":"token_usage_record","payload":{"thread_id":"synthetic-thread","session_id":"synthetic-root","turn_id":"turn-one","response_id":"resp-one","usage":usage(),"thread_token_usage":usage(),"turn_token_usage":usage()}})
}
fn legacy() -> Value {
    json!({"timestamp":now(),"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":usage(),"last_token_usage":usage()},"rate_limits":null}})
}
fn claude(id: &str, output: u64) -> Value {
    json!({"type":"assistant","uuid":"row-one","sessionId":"session-one","requestId":"request-one","timestamp":now(),"cwd":"PRIVATE-PROJECT-MARKER","message":{"id":id,"model":"claude-sonnet-4-6","content":[{"type":"text","text":"PRIVATE-PROMPT-MARKER"}],"usage":{"input_tokens":100,"cache_read_input_tokens":600,"cache_creation_input_tokens":300,"cache_creation":{"ephemeral_5m_input_tokens":200,"ephemeral_1h_input_tokens":100},"output_tokens":output,"service_tier":"standard","inference_geo":"us"}}})
}
fn write_rows(path: &Path, rows: &[Value]) {
    let mut f = fs::File::create(path).unwrap();
    for r in rows {
        writeln!(f, "{}", r).unwrap();
    }
}
async fn count(store: &Store) -> i64 {
    store.call(|c| Ok(c.query_row("SELECT COUNT(*) FROM usage_entries", [], |r| r.get(0))?)).await.unwrap()
}

#[test]
fn native_categories_and_missing_values() {
    let mut state = Context::default();
    parse_record("codex", &meta("openai"), &mut state).unwrap();
    parse_record("codex", &context(), &mut state).unwrap();
    let o = parse_record("codex", &modern(), &mut state).unwrap().unwrap();
    assert_eq!(o.tokens.input, Some(300));
    assert_eq!(o.tokens.cache_write, Some(100));
    assert_eq!(o.tokens.output, Some(200));
    assert_eq!(o.tokens.reasoning, Some(80));
    assert_eq!(o.account_id, None);
    let mut partial = modern();
    partial["payload"]["usage"].as_object_mut().unwrap().remove("cached_input_tokens");
    partial["payload"]["usage"].as_object_mut().unwrap().remove("reasoning_output_tokens");
    let o = parse_record("codex", &partial, &mut state).unwrap().unwrap();
    assert_eq!(o.tokens.input, None);
    assert_eq!(o.tokens.cache_read, None);
    assert_eq!(o.tokens.reasoning, None);
    assert_eq!(o.completeness, "partial");
    let mut invalid = modern();
    invalid["payload"]["usage"]["cached_input_tokens"] = json!(-1);
    assert!(parse_record("codex", &invalid, &mut state).is_err());
}
#[test]
fn custom_provider_is_never_upgraded_to_openai() {
    let mut state = Context::default();
    parse_record("codex", &meta("fusebox"), &mut state).unwrap();
    parse_record("codex", &context(), &mut state).unwrap();
    assert_eq!(parse_record("codex", &modern(), &mut state).unwrap().unwrap().provider, "fusebox");
    let mut state = Context::default();
    let mut m = meta("openai");
    m["payload"].as_object_mut().unwrap().remove("model_provider");
    parse_record("codex", &m, &mut state).unwrap();
    assert_eq!(parse_record("codex", &modern(), &mut state).unwrap().unwrap().provider, "unknown");
}
#[test]
fn claude_input_ttl_and_privacy() {
    let o = parse_record("claude_code", &claude("msg-one", 50), &mut Context::default()).unwrap().unwrap();
    assert_eq!(o.tokens.input_total(), Some(1000));
    assert_eq!(o.tokens.write_1h, Some(100));
    assert_eq!(o.inference_geo.as_deref(), Some("us"));
    assert_eq!(o.provider_request_id.as_deref(), Some("request-one"));
    assert_eq!(o.response_id.as_deref(), Some("msg-one"));
    let payload = serde_json::to_string(&o).unwrap();
    assert!(!payload.contains("PRIVATE-"));
}
#[tokio::test]
async fn opt_in_incremental_partial_restart_and_copy() {
    let dir = TestDir::new();
    let logs = dir.0.join("logs");
    fs::create_dir(&logs).unwrap();
    let file = logs.join("rollout.jsonl");
    write_rows(&file, &[meta("openai"), context(), modern(), legacy(), legacy()]);
    let store = dir.store();
    scan(&store, None).await.unwrap();
    assert_eq!(count(&store).await, 0);
    configure(&store, "codex", logs.to_str().unwrap(), true).await.unwrap();
    scan(&store, None).await.unwrap();
    assert_eq!(count(&store).await, 1);
    let raw = serde_json::to_string(&claude("msg-partial", 75)).unwrap();
    let file2 = logs.join("claude.jsonl");
    fs::write(&file2, &raw[..raw.len() / 2]).unwrap();
    configure(&store, "claude_code", logs.to_str().unwrap(), true).await.unwrap();
    scan(&store, Some("claude_code".into())).await.unwrap();
    assert_eq!(count(&store).await, 1);
    drop(store);
    let store = dir.store();
    let mut f = fs::OpenOptions::new().append(true).open(&file2).unwrap();
    writeln!(f, "{}", &raw[raw.len() / 2..]).unwrap();
    scan(&store, Some("claude_code".into())).await.unwrap();
    assert_eq!(count(&store).await, 2);
    fs::copy(&file, logs.join("copy.jsonl")).unwrap();
    scan(&store, Some("codex".into())).await.unwrap();
    assert_eq!(count(&store).await, 2);
    let payloads = store
        .call(|c| {
            Ok(c.query_row("SELECT GROUP_CONCAT(payload) FROM usage_observations", [], |r| r.get::<_, String>(0))?)
        })
        .await
        .unwrap();
    assert!(!payloads.contains("PRIVATE-"));
    let checkpoints = store
        .call(|c| {
            Ok(c.query_row("SELECT GROUP_CONCAT(context) FROM usage_import_checkpoints", [], |r| {
                r.get::<_, String>(0)
            })?)
        })
        .await
        .unwrap();
    assert!(!checkpoints.contains("PRIVATE-"));
}
#[tokio::test]
async fn legacy_first_then_modern_supersedes_even_across_scan() {
    let dir = TestDir::new();
    let logs = dir.0.join("logs");
    fs::create_dir(&logs).unwrap();
    let file = logs.join("rollout.jsonl");
    write_rows(&file, &[meta("openai"), context(), legacy()]);
    let store = dir.store();
    configure(&store, "codex", logs.to_str().unwrap(), true).await.unwrap();
    scan(&store, None).await.unwrap();
    assert_eq!(count(&store).await, 1);
    let mut f = fs::OpenOptions::new().append(true).open(file).unwrap();
    writeln!(f, "{}", modern()).unwrap();
    scan(&store, None).await.unwrap();
    assert_eq!(count(&store).await, 1);
    let sources = store
        .call(|c| {
            Ok(c.query_row("SELECT GROUP_CONCAT(source_event_id) FROM usage_entries", [], |r| r.get::<_, String>(0))?)
        })
        .await
        .unwrap();
    assert!(sources.contains("response:resp-one"));
    assert!(!sources.contains("counter:"));
}
#[tokio::test]
async fn rotation_compression_and_plain_sibling_dedupe() {
    let dir = TestDir::new();
    let logs = dir.0.join("logs");
    fs::create_dir(&logs).unwrap();
    let file = logs.join("rollout.jsonl");
    write_rows(&file, &[meta("openai"), context(), modern()]);
    let compressed = zstd::stream::encode_all(fs::File::open(&file).unwrap(), 1).unwrap();
    fs::write(logs.join("rollout.jsonl.zst"), compressed).unwrap();
    let store = dir.store();
    configure(&store, "codex", logs.to_str().unwrap(), true).await.unwrap();
    scan(&store, None).await.unwrap();
    assert_eq!(count(&store).await, 1);
    fs::remove_file(&file).unwrap();
    scan(&store, None).await.unwrap();
    assert_eq!(count(&store).await, 1);
    let mut second = modern();
    second["payload"]["response_id"] = json!("resp-two");
    write_rows(&file, &[meta("openai"), context(), second]);
    scan(&store, None).await.unwrap();
    assert_eq!(count(&store).await, 2);
}
#[tokio::test]
async fn revisions_and_ambiguous_forks() {
    let dir = TestDir::new();
    let logs = dir.0.join("logs");
    fs::create_dir(&logs).unwrap();
    let file = logs.join("claude.jsonl");
    write_rows(&file, &[claude("msg-one", 10), claude("msg-one", 50)]);
    let store = dir.store();
    configure(&store, "claude_code", logs.to_str().unwrap(), true).await.unwrap();
    scan(&store, None).await.unwrap();
    assert_eq!(count(&store).await, 1);
    let output =
        store.call(|c| Ok(c.query_row("SELECT output FROM usage_entries", [], |r| r.get::<_, u64>(0))?)).await.unwrap();
    assert_eq!(output, 50);
    let mut m = meta("openai");
    m["payload"]["forked_from_id"] = json!("parent-thread");
    let mut state = Context::default();
    parse_record("codex", &m, &mut state).unwrap();
    assert!(parse_record("codex", &legacy(), &mut state).unwrap_err().to_string().contains("unsupported"));
}
#[tokio::test]
async fn backfill_bypasses_watermark_only_with_explicit_consent() {
    let dir = TestDir::new();
    let logs = dir.0.join("logs");
    fs::create_dir(&logs).unwrap();
    let file = logs.join("claude.jsonl");
    let mut row = claude("msg-old", 50);
    let old = chrono::Utc::now() - chrono::Duration::days(5);
    row["timestamp"] = json!(old.to_rfc3339());
    write_rows(&file, &[row]);
    let store = dir.store();
    store.purge(chrono::Utc::now().timestamp_millis() - 86400000).await.unwrap();
    configure(&store, "claude_code", logs.to_str().unwrap(), true).await.unwrap();
    scan(&store, None).await.unwrap();
    assert_eq!(count(&store).await, 0);
    backfill(&store, Some("claude_code".into())).await.unwrap();
    assert_eq!(count(&store).await, 1);
    let watermark = store
        .call(|c| {
            Ok(c.query_row("SELECT CAST(value AS INTEGER) FROM usage_meta WHERE key='purge_before_ms'", [], |r| {
                r.get::<_, i64>(0)
            })?)
        })
        .await
        .unwrap();
    assert!(watermark > old.timestamp_millis());
}
#[cfg(unix)]
#[tokio::test]
async fn symlinks_are_not_scanned() {
    let dir = TestDir::new();
    let logs = dir.0.join("logs");
    fs::create_dir(&logs).unwrap();
    let outside = dir.0.join("outside.jsonl");
    write_rows(&outside, &[claude("msg-one", 50)]);
    std::os::unix::fs::symlink(&outside, logs.join("linked.jsonl")).unwrap();
    let store = dir.store();
    configure(&store, "claude_code", logs.to_str().unwrap(), true).await.unwrap();
    scan(&store, None).await.unwrap();
    assert_eq!(count(&store).await, 0);
    let linked = dir.0.join("root-link");
    std::os::unix::fs::symlink(&logs, &linked).unwrap();
    assert!(configure(&store, "claude_code", linked.to_str().unwrap(), true).await.is_err());
}

#[test]
fn explicit_unmatched_turn_is_unknown() {
    let mut state = Context::default();
    parse_record("codex", &meta("openai"), &mut state).unwrap();
    parse_record("codex", &context(), &mut state).unwrap();
    let mut second = context();
    second["payload"]["turn_id"] = json!("turn-two");
    second["payload"]["model"] = json!("gpt-6-astra");
    parse_record("codex", &second, &mut state).unwrap();
    let mut record = modern();
    record["payload"]["turn_id"] = json!("unknown-turn");
    assert!(parse_record("codex", &record, &mut state).unwrap().unwrap().actual_model.is_none());
}
