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

#[tokio::test]
async fn usage_commands_close_writer_on_success_and_failure() {
    let dir = TestDir::new();
    dir.store().shutdown().await.unwrap();
    let database = dir.0.join("usage.sqlite3");
    run_command(UsageCommand::Status { database: database.clone() }).await.unwrap();
    let reopened = dir.store();
    assert_eq!(reopened.health()["prior_unclosed_sessions"], 0);
    reopened.shutdown().await.unwrap();
    assert!(
        run_command(UsageCommand::Enable { database, source: "invalid-source".into(), root: dir.0.clone() })
            .await
            .is_err()
    );
    let reopened = dir.store();
    assert_eq!(reopened.health()["prior_unclosed_sessions"], 0);
    reopened.shutdown().await.unwrap();
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
fn malformed_modern_cannot_suppress_existing_or_future_legacy() {
    let mut invalid = modern();
    invalid["payload"]["usage"]["reasoning_output_tokens"] = json!(201);
    for legacy_first in [true, false] {
        let mut state = Context::default();
        parse_record("codex", &meta("openai"), &mut state).unwrap();
        if legacy_first {
            parse_record("codex", &legacy(), &mut state).unwrap().unwrap();
        }
        let before = serde_json::to_value(&state).unwrap();
        assert!(parse_record("codex", &invalid, &mut state).is_err());
        assert_eq!(serde_json::to_value(&state).unwrap(), before);
        assert!(state.superseded.is_empty());
        if !legacy_first {
            assert!(parse_record("codex", &legacy(), &mut state).unwrap().is_some());
        }
    }
}

#[test]
fn malformed_legacy_does_not_advance_baseline_or_mark_reset() {
    let mut state = Context::default();
    parse_record("codex", &meta("openai"), &mut state).unwrap();
    parse_record("codex", &legacy(), &mut state).unwrap().unwrap();
    let before = serde_json::to_value(&state).unwrap();
    let mut missing_timestamp = legacy();
    missing_timestamp["timestamp"] = json!("invalid");
    missing_timestamp["payload"]["info"]["total_token_usage"] = json!({
        "input_tokens":500,"cached_input_tokens":300,"cache_write_input_tokens":50,
        "output_tokens":100,"reasoning_output_tokens":40
    });
    let mut out_of_bounds_timestamp = missing_timestamp.clone();
    out_of_bounds_timestamp["timestamp"] = json!("2010-01-01T00:00:00Z");
    let mut invalid_subset = legacy();
    invalid_subset["payload"]["info"]["total_token_usage"]["reasoning_output_tokens"] = json!(201);
    let mut invalid_delta = legacy();
    invalid_delta["payload"]["info"]["total_token_usage"]["output_tokens"] = json!(210);
    invalid_delta["payload"]["info"]["total_token_usage"]["reasoning_output_tokens"] = json!(100);
    for row in [missing_timestamp, out_of_bounds_timestamp, invalid_subset, invalid_delta] {
        assert!(parse_record("codex", &row, &mut state).is_err());
        assert_eq!(serde_json::to_value(&state).unwrap(), before);
    }
    let mut next = legacy();
    next["payload"]["info"]["total_token_usage"] = json!({
        "input_tokens":2000,"cached_input_tokens":1200,"cache_write_input_tokens":200,
        "output_tokens":400,"reasoning_output_tokens":160
    });
    let observation = parse_record("codex", &next, &mut state).unwrap().unwrap();
    assert_eq!(observation.tokens, codex_tokens(&usage()));
}

#[test]
fn legacy_reasoning_is_unknown_when_current_or_baseline_is_missing() {
    for explicit_null in [false, true] {
        let mut state = Context::default();
        parse_record("codex", &meta("openai"), &mut state).unwrap();
        let mut row = legacy();
        row["payload"]["info"].as_object_mut().unwrap().remove("last_token_usage");
        if explicit_null {
            row["payload"]["info"]["total_token_usage"]["reasoning_output_tokens"] = Value::Null;
        } else {
            row["payload"]["info"]["total_token_usage"].as_object_mut().unwrap().remove("reasoning_output_tokens");
        }
        assert_eq!(parse_record("codex", &row, &mut state).unwrap().unwrap().tokens.reasoning, None);
        state = serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
        for (scale, expected) in [(2, None), (3, Some(80)), (4, None)] {
            row["payload"]["info"]["total_token_usage"] = json!({
                "input_tokens":1000*scale,"cached_input_tokens":600*scale,
                "cache_write_input_tokens":100*scale,"output_tokens":200*scale,
                "reasoning_output_tokens":80*scale
            });
            if scale == 4 {
                row["payload"]["info"]["total_token_usage"].as_object_mut().unwrap().remove("reasoning_output_tokens");
            }
            let observed = parse_record("codex", &row, &mut state).unwrap().unwrap();
            assert_eq!(observed.tokens.reasoning, expected);
            assert_eq!(observed.tokens.output, Some(200));
        }
    }
}

#[test]
fn legacy_cache_write_null_is_rejected_but_absence_defaults_zero() {
    let mut state = Context::default();
    parse_record("codex", &meta("openai"), &mut state).unwrap();
    let mut row = legacy();
    row["payload"]["info"].as_object_mut().unwrap().remove("last_token_usage");
    row["payload"]["info"]["total_token_usage"]["cache_write_input_tokens"] = Value::Null;
    let before = serde_json::to_value(&state).unwrap();
    assert!(parse_record("codex", &row, &mut state).is_err());
    assert_eq!(serde_json::to_value(&state).unwrap(), before);
    row["payload"]["info"]["total_token_usage"].as_object_mut().unwrap().remove("cache_write_input_tokens");
    let observed = parse_record("codex", &row, &mut state).unwrap().unwrap();
    assert_eq!(observed.tokens.cache_write, Some(0));
    assert_eq!(observed.tokens.input, Some(400));
    let before = serde_json::to_value(&state).unwrap();
    row["payload"]["info"]["total_token_usage"]["cache_write_input_tokens"] = Value::Null;
    assert!(parse_record("codex", &row, &mut state).is_err());
    assert_eq!(serde_json::to_value(&state).unwrap(), before);
}

#[test]
fn legacy_resume_counts_only_last_response_and_prices_its_context() {
    let mut state = Context::default();
    parse_record("codex", &meta("openai"), &mut state).unwrap();
    parse_record("codex", &context(), &mut state).unwrap();
    let mut row = legacy();
    row["payload"]["info"]["total_token_usage"] = json!({"input_tokens":500000,"cached_input_tokens":300000,"cache_write_input_tokens":50000,"output_tokens":100000,"reasoning_output_tokens":40000});
    let o = parse_record("codex", &row, &mut state).unwrap().unwrap();
    assert_eq!(o.tokens, codex_tokens(&usage()));
    assert_eq!(o.numeric_metadata["per_request_usage"], 1);
    let snapshot = super::super::pricing::Catalogue::load(None).unwrap().price(&o);
    assert_eq!(snapshot.cost_nanos, Some(2_910_000));
    assert!(snapshot.partial, "legacy evidence remains an estimate");
    assert!(parse_record("codex", &row, &mut state).unwrap().is_none());
    // A distinct response with identical counts has a larger cumulative total.
    // It must survive even though last_token_usage repeats byte for byte.
    row["payload"]["info"]["total_token_usage"] = json!({"input_tokens":501000,"cached_input_tokens":300600,"cache_write_input_tokens":50100,"output_tokens":100200,"reasoning_output_tokens":40080});
    state = serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
    let next = parse_record("codex", &row, &mut state).unwrap().unwrap();
    assert_ne!(o.source_event_id, next.source_event_id);
    assert_eq!(next.tokens, o.tokens);
    assert_eq!(next.numeric_metadata["per_request_usage"], 1);
}

#[test]
fn legacy_gaps_are_unpriced_and_bad_last_usage_cannot_move_baseline() {
    let mut state = Context::default();
    parse_record("codex", &meta("openai"), &mut state).unwrap();
    parse_record("codex", &context(), &mut state).unwrap();
    parse_record("codex", &legacy(), &mut state).unwrap();
    let mut gap = legacy();
    gap["payload"]["info"]["total_token_usage"] = json!({"input_tokens":3000,"cached_input_tokens":1800,"cache_write_input_tokens":300,"output_tokens":600,"reasoning_output_tokens":240});
    let mut invalid = gap.clone();
    invalid["payload"]["info"]["last_token_usage"]["cached_input_tokens"] = json!(-1);
    let before = serde_json::to_value(&state).unwrap();
    assert!(parse_record("codex", &invalid, &mut state).is_err());
    assert_eq!(serde_json::to_value(&state).unwrap(), before);
    let o = parse_record("codex", &gap, &mut state).unwrap().unwrap();
    assert_eq!(o.tokens.total(), Some(2400));
    assert!(!o.numeric_metadata.contains_key("per_request_usage"));
    assert_eq!(
        super::super::pricing::Catalogue::load(None).unwrap().price(&o).basis,
        "cumulative_usage_not_per_request"
    );
}

#[test]
fn codex_speed_switches_survive_checkpoints_and_late_responses() {
    let settings =
        |tier: Value| json!({"type":"event_msg","payload":{"type":"thread_settings_applied","thread_settings":tier}});
    let mut state = Context::default();
    parse_record("codex", &meta("openai"), &mut state).unwrap();
    parse_record("codex", &settings(json!({"service_tier":"priority"})), &mut state).unwrap();
    parse_record("codex", &context(), &mut state).unwrap();
    state = serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
    assert_eq!(
        parse_record("codex", &legacy(), &mut state).unwrap().unwrap().service_tier.as_deref(),
        Some("priority")
    );
    parse_record("codex", &settings(json!({"service_tier":"ultrafast"})), &mut state).unwrap();
    let mut second = context();
    second["payload"]["turn_id"] = json!("turn-two");
    parse_record("codex", &second, &mut state).unwrap();
    let first = parse_record("codex", &modern(), &mut state).unwrap().unwrap();
    assert_eq!(first.service_tier.as_deref(), Some("priority"));
    let mut late = modern();
    late["payload"]["turn_id"] = json!("turn-two");
    assert_eq!(parse_record("codex", &late, &mut state).unwrap().unwrap().service_tier.as_deref(), Some("ultrafast"));
    parse_record("codex", &settings(json!({"model":"gpt-6.1-sol"})), &mut state).unwrap();
    late["payload"].as_object_mut().unwrap().remove("turn_id");
    assert_eq!(parse_record("codex", &late, &mut state).unwrap().unwrap().service_tier.as_deref(), Some("standard"));
}

#[test]
fn subagent_parent_copies_are_ambiguous_but_response_id_usage_is_kept() {
    let mut m = meta("openai");
    m["payload"]["source"] = json!({"subagent":{"thread_spawn":{"parent_thread_id":"parent"}}});
    let mut state = Context::default();
    parse_record("codex", &m, &mut state).unwrap();
    let mut parent = meta("openai");
    parent["payload"]["id"] = json!("parent");
    parse_record("codex", &parent, &mut state).unwrap();
    assert_eq!(state.thread.as_deref(), Some("synthetic-thread"));
    assert!(parse_record("codex", &legacy(), &mut state).unwrap_err().to_string().starts_with("unsupported"));
    assert!(parse_record("codex", &modern(), &mut state).unwrap().is_some());
}

#[test]
fn claude_native_speed_and_thinking_follow_reported_usage() {
    let mut row = claude("fast", 50);
    row["message"]["model"] = json!("claude-opus-5-5");
    row["message"]["usage"]["speed"] = json!("fast");
    row["message"]["usage"]["output_tokens_details"] = json!({"thinking_tokens":40});
    let o = parse_record("claude_code", &row, &mut Context::default()).unwrap().unwrap();
    assert_eq!(o.service_tier.as_deref(), Some("fast"));
    assert_eq!(o.tokens.reasoning, Some(40));
    assert_eq!(o.tokens.total(), Some(1050));
    row["message"]["usage"]["output_tokens_details"]["thinking_tokens"] = json!(51);
    assert!(parse_record("claude_code", &row, &mut Context::default()).is_err());
}

#[test]
fn counter_reset_disables_ambiguous_legacy_across_restart() {
    let mut state = Context::default();
    parse_record("codex", &meta("openai"), &mut state).unwrap();
    parse_record("codex", &legacy(), &mut state).unwrap().unwrap();
    let baseline = state.cumulative.clone();
    let mut reset = legacy();
    reset["payload"]["info"]["total_token_usage"] = json!({
        "input_tokens":500,"cached_input_tokens":300,"cache_write_input_tokens":50,
        "output_tokens":100,"reasoning_output_tokens":40
    });
    assert!(parse_record("codex", &reset, &mut state).unwrap_err().to_string().starts_with("unsupported"));
    assert_eq!(state.cumulative, baseline);
    state = serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
    assert!(parse_record("codex", &legacy(), &mut state).unwrap_err().to_string().starts_with("unsupported"));
    assert!(parse_record("codex", &modern(), &mut state).unwrap().is_some());
}

#[tokio::test]
async fn malformed_modern_does_not_remove_committed_legacy() {
    let dir = TestDir::new();
    let logs = dir.0.join("logs");
    fs::create_dir(&logs).unwrap();
    let file = logs.join("rollout.jsonl");
    write_rows(&file, &[meta("openai"), legacy()]);
    let store = dir.store();
    configure(&store, "codex", logs.to_str().unwrap(), true).await.unwrap();
    scan(&store, None).await.unwrap();
    let mut invalid = modern();
    invalid["payload"]["usage"]["reasoning_output_tokens"] = json!(201);
    writeln!(fs::OpenOptions::new().append(true).open(file).unwrap(), "{}", invalid).unwrap();
    scan(&store, None).await.unwrap();
    assert_eq!(count(&store).await, 1);
    let event = store
        .call(|c| Ok(c.query_row("SELECT source_event_id FROM usage_entries", [], |r| r.get::<_, String>(0))?))
        .await
        .unwrap();
    assert!(event.starts_with("counter:"));
}

#[tokio::test]
async fn sibling_thread_counters_and_supersession_stay_separate() {
    let dir = TestDir::new();
    let logs = dir.0.join("logs");
    fs::create_dir(&logs).unwrap();
    let mut child_a = meta("openai");
    child_a["payload"]["id"] = json!("child-a");
    let mut child_b = meta("openai");
    child_b["payload"]["id"] = json!("child-b");
    write_rows(&logs.join("child-a.jsonl"), &[child_a.clone(), legacy()]);
    let child_b_file = logs.join("child-b.jsonl");
    write_rows(&child_b_file, &[child_b, legacy()]);
    let store = dir.store();
    configure(&store, "codex", logs.to_str().unwrap(), true).await.unwrap();
    scan(&store, None).await.unwrap();
    assert_eq!(count(&store).await, 2);
    let mut expected_a = Context::default();
    parse_record("codex", &child_a, &mut expected_a).unwrap();
    let expected_a = parse_record("codex", &legacy(), &mut expected_a).unwrap().unwrap().source_event_id;
    let mut child_b_modern = modern();
    child_b_modern["payload"]["thread_id"] = json!("child-b");
    writeln!(fs::OpenOptions::new().append(true).open(child_b_file).unwrap(), "{}", child_b_modern).unwrap();
    scan(&store, None).await.unwrap();
    assert_eq!(count(&store).await, 2);
    let events = store
        .call(|c| {
            let mut stmt = c.prepare("SELECT source_event_id FROM usage_entries ORDER BY source_event_id")?;
            Ok(stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
        .unwrap();
    assert!(events.contains(&expected_a));
    assert!(events.contains(&"response:resp-one".to_string()));
}

#[test]
fn older_checkpoint_without_thread_cannot_guess_legacy_identity() {
    let mut state = Context::default();
    parse_record("codex", &meta("openai"), &mut state).unwrap();
    let mut checkpoint = serde_json::to_value(&state).unwrap();
    checkpoint.as_object_mut().unwrap().remove("thread");
    state = serde_json::from_value(checkpoint).unwrap();
    assert!(parse_record("codex", &legacy(), &mut state).unwrap_err().to_string().starts_with("unsupported"));
    assert!(parse_record("codex", &modern(), &mut state).unwrap().is_some());
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
#[tokio::test]
async fn imported_claude_history_is_priced_on_its_original_local_date() {
    let dir = TestDir::new();
    let logs = dir.0.join("logs");
    fs::create_dir(&logs).unwrap();
    // 00:30 CEST on 30 March in Amsterdam, the night after the spring DST change.
    let mut row = claude("msg-history", 50);
    row["timestamp"] = json!("2026-03-29T22:30:00Z");
    write_rows(&logs.join("claude.jsonl"), &[row]);
    let store = dir.store();
    configure(&store, "claude_code", logs.to_str().unwrap(), true).await.unwrap();
    scan(&store, None).await.unwrap();
    let q = store::Query {
        start: Some("2026-03-27".into()),
        end: Some("2026-04-01".into()),
        timezone: Some("Europe/Amsterdam".into()),
        ..Default::default()
    };
    let summary = store.query(q).await.unwrap();
    let day = summary["trend"].as_array().unwrap().iter().find(|d| d["source"] == "claude_code").unwrap();
    assert_eq!(day["date"], "2026-03-30");
    assert_eq!(day["unpriced"], 0, "{day}");
    assert!(day["estimated_cost_nanos"].as_i64().unwrap() > 0);
    store.shutdown().await.unwrap();
}
