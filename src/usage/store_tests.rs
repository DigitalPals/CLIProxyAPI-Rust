use super::super::types::Tokens;
use super::*;
fn event(source: &str, id: &str) -> Observation {
    let mut o = Observation::new(source, id.into(), "openai", Utc::now().timestamp_millis());
    o.actual_model = Some("gpt-6.1-sol".into());
    o.completeness = "complete".into();
    o.tokens = Tokens {
        input: Some(1000),
        cache_read: Some(100),
        cache_write: Some(0),
        output: Some(100),
        reasoning: Some(80),
        ..Default::default()
    };
    o
}
fn path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("fusebox-usage-test-{}-{}-{name}.sqlite", std::process::id(), rand_id()))
}
fn rand_id() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
}
fn all() -> Query {
    Query {
        start: Some("2020-01-01".into()),
        end: Some((Utc::now() + chrono::Duration::days(1)).to_rfc3339()),
        ..Default::default()
    }
}
#[tokio::test]
async fn persists_more_than_ring_after_restart() {
    let p = path("restart");
    {
        let store = Store::open(&p, 1024, 90, None).unwrap();
        for i in 0..601 {
            assert!(store.enqueue(event("proxy", &i.to_string())));
        }
        store.flush().await.unwrap();
        assert_eq!(store.details(all()).await.unwrap()["total"], 601);
    }
    let store = Store::open(&p, 1024, 90, None).unwrap();
    let value = store.query(all()).await.unwrap();
    assert_eq!(value["proxy"]["observations"], 601);
}
#[tokio::test]
async fn reconcile_priority_order_and_equal_tokens_distinct() {
    for reverse in [false, true] {
        let store = Store::open(&path("reconcile"), 8, 90, None).unwrap();
        let mut trusted = event("proxy", "p");
        trusted.response_id = Some("r".into());
        trusted.account_id = Some("actual".into());
        let mut untrusted = event("codex", "i");
        untrusted.response_id = Some("r".into());
        untrusted.origin_id = "collector:bad".into();
        untrusted.account_id = Some("claimed".into());
        untrusted.tokens.output = Some(900);
        let distinct = event("proxy", "distinct");
        let v = if reverse { vec![untrusted, trusted, distinct] } else { vec![trusted, untrusted, distinct] };
        store.call(move |c| insert_batch(c, &v)).await.unwrap();
        let summary = store.query(all()).await.unwrap();
        assert_eq!(summary["proxy"]["observations"], 2);
        assert_eq!(summary["proxy"]["tokens"]["output"], 200);
        assert_eq!(summary["proxy"]["conflicts"], 1);
        assert_eq!(summary["sources"][2]["observations"], 1);
    }
}
#[tokio::test]
async fn copy_revision_purge_and_atomic_checkpoint() {
    let store = Store::open(&path("copy"), 8, 90, None).unwrap();
    let a = event("claude_code", "msg-stable");
    let mut b = a.clone();
    b.origin_id = "collector:copy".into();
    let mut revision = a.clone();
    revision.tokens.output = Some(200);
    let stamp = a.event_at_ms;
    store
        .call(move |c| {
            let tx = c.transaction()?;
            tx.execute_batch("CREATE TABLE checkpoint(n INTEGER);")?;
            tx.commit()?;
            c.execute_batch("BEGIN IMMEDIATE")?;
            let value = insert_batch(c, &[a.clone(), b, revision, a])?;
            c.execute("INSERT INTO checkpoint VALUES(1)", [])?;
            c.execute_batch("COMMIT")?;
            Ok(value)
        })
        .await
        .unwrap();
    let summary = store.query(all()).await.unwrap();
    assert_eq!(summary["sources"][1]["observations"], 1);
    assert_eq!(summary["sources"][1]["source_record_count"], 3);
    assert_eq!(summary["sources"][1]["tokens"]["output"], 200);
    store.purge(stamp + 1).await.unwrap();
    let mut replay = event("claude_code", "msg-stable");
    replay.event_at_ms = stamp;
    assert_eq!(store.call(move |c| insert_batch(c, &[replay])).await.unwrap()["purged"], 1);
    assert_eq!(store.details(all()).await.unwrap()["total"], 0);
}
#[test]
fn exact_price_cache_reasoning_thresholds_and_historical() {
    let c = Catalogue::load(None).unwrap();
    let mut a = event("proxy", "a");
    assert_eq!(c.price(&a).cost_nanos, Some(3_010_000));
    a.tokens.reasoning = Some(0);
    assert_eq!(c.price(&a).cost_nanos, Some(3_010_000));
    a.tokens.input = Some(271900);
    assert_eq!(c.price(&a).cost_nanos, Some(544_810_000));
    a.tokens.input = Some(271901);
    assert_eq!(c.price(&a).cost_nanos, Some(1_089_124_000));
    a.event_at_ms = 1_700_000_000_000;
    assert_eq!(c.price(&a).basis, "outside_effective_period");
    a = event("proxy", "b");
    a.provider = "anthropic".into();
    a.actual_model = Some("claude-sonnet-4-6".into());
    a.tokens.cache_write = Some(300);
    a.tokens.write_5m = Some(200);
    a.tokens.write_1h = Some(100);
    assert_eq!(c.price(&a).cost_nanos, Some(5_880_000));
    a.tokens.write_1h = None;
    assert_eq!(c.price(&a).basis, "unknown_cache_write_ttl");
}
#[tokio::test]
async fn bounds_and_dst() {
    let store = Store::open(&path("bounds"), 8, 90, None).unwrap();
    let mut o = event("proxy", "invalid");
    o.tokens.output = Some(1);
    assert!(!store.enqueue(o));
    assert_eq!(store.health()["rejected"], 1);
    assert_eq!(store.health()["state"], "degraded");
    assert!(store.query(Query { timezone: Some("Nope".into()), ..Default::default() }).await.is_err());
    let q = Query {
        start: Some("2026-03-29".into()),
        end: Some("2026-03-30".into()),
        timezone: Some("Europe/Amsterdam".into()),
        ..Default::default()
    };
    let r = range(&q).unwrap();
    assert_eq!(r.end - r.start, 23 * 3600 * 1000);
    let q = Query {
        start: Some("2026-10-25".into()),
        end: Some("2026-10-26".into()),
        timezone: Some("Europe/Amsterdam".into()),
        ..Default::default()
    };
    let r = range(&q).unwrap();
    assert_eq!(r.end - r.start, 25 * 3600 * 1000);
}
#[tokio::test]
async fn disk_failure_visible_and_migration_fail_closed() {
    let p = path("failure");
    let store = Store::open(&p, 8, 90, None).unwrap();
    store
        .call(|c| {
            c.pragma_update(None, "query_only", true)?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(store.enqueue(event("proxy", "lost")));
    assert!(store.flush().await.is_err());
    assert!(store.health()["writer_errors"].as_u64().unwrap() >= 1);
    assert_eq!(store.health()["dropped"], 1);
    let p = path("version");
    let c = Connection::open(&p).unwrap();
    c.pragma_update(None, "user_version", 999).unwrap();
    assert!(Store::open(&p, 8, 90, None).is_err());
}

#[tokio::test]
async fn bounded_queue_nonblocking_and_durable_ordering() {
    let store = Store::open(&path("queue"), 2, 90, None).unwrap();
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocker = store.clone();
    let task = tokio::spawn(async move {
        blocker
            .call(move |_| {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
            .await
            .unwrap();
    });
    started_rx.await.unwrap();
    let start = std::time::Instant::now();
    assert!(store.enqueue(event("proxy", "one")));
    assert!(store.enqueue(event("proxy", "two")));
    assert!(!store.enqueue(event("proxy", "three")));
    assert!(start.elapsed() < std::time::Duration::from_millis(100));
    assert_eq!(store.health()["queue_depth"], 2);
    assert_eq!(store.health()["dropped"], 1);
    release_tx.send(()).unwrap();
    task.await.unwrap();
    store.flush().await.unwrap();
    assert_eq!(store.details(all()).await.unwrap()["total"], 2);
}
#[tokio::test]
async fn durable_transaction_rollback_and_request_id_namespace() {
    let store = Store::open(&path("atomic"), 8, 90, None).unwrap();
    let a = event("proxy", "rollback");
    assert!(
        store
            .call(move |c| -> Result<()> {
                c.execute_batch("BEGIN IMMEDIATE")?;
                insert_batch(c, &[a])?;
                bail!("checkpoint failed")
            })
            .await
            .is_err()
    );
    assert_eq!(store.details(all()).await.unwrap()["total"], 0);
    let mut proxy = event("proxy", "p");
    proxy.response_id = Some("same-text".into());
    let mut local = event("codex", "l");
    local.provider_request_id = Some("same-text".into());
    store.call(move |c| insert_batch(c, &[proxy, local])).await.unwrap();
    assert_eq!(store.query(all()).await.unwrap()["proxy"]["conflicts"], 0);
}
#[test]
fn overflow_missing_unsupported_regions_tools_and_catalogue_validation() {
    let c = Catalogue::load(None).unwrap();
    let mut a = event("proxy", "a");
    assert!(c.price(&a).partial);
    a.service_tier = Some("standard".into());
    a.inference_geo = Some("global".into());
    assert!(!c.price(&a).partial);
    a.inference_geo = Some("us-only".into());
    assert_eq!(c.price(&a).cost_nanos, Some(3_311_000));
    a.inference_geo = Some("unknown".into());
    assert_eq!(c.price(&a).basis, "unsupported_inference_region");
    a.inference_geo = None;
    a.service_tier = Some("unverified-contract-tier".into());
    assert_eq!(c.price(&a).basis, "unsupported_service_tier");
    a.service_tier = None;
    a.numeric_metadata.insert("image_tokens".into(), 1);
    assert_eq!(c.price(&a).basis, "unsupported_modality_or_tools");
    a.numeric_metadata.clear();
    a.tokens.input = None;
    assert_eq!(c.price(&a).basis, "missing_tokens");
    a.tokens.input = Some(super::super::types::MAX_TOKENS + 1);
    assert!(a.validate().is_err());
    a.tokens.input = Some(2);
    let mut huge = c.clone();
    for rate in &mut huge.rates {
        rate.input_nanos_per_token = i64::MAX;
    }
    assert_eq!(huge.price(&a).cost_nanos, None);
    let mut duplicate = c.clone();
    duplicate.rates.push(duplicate.rates[0].clone());
    let p = path("rates");
    std::fs::write(&p, serde_json::to_vec(&duplicate).unwrap()).unwrap();
    assert!(Catalogue::load(Some(&p)).is_err());
}
#[tokio::test]
async fn ingestion_metadata_only_and_price_snapshot_immutable() {
    let p = path("snapshot");
    let store = Store::open(&p, 8, 90, None).unwrap();
    let a = event("proxy", "a");
    store.call(move |c| insert_batch(c, &[a])).await.unwrap();
    let before = store.details(all()).await.unwrap();
    let override_file = path("override");
    let mut c = Catalogue::load(None).unwrap();
    c.version = "local-test-override".into();
    for rate in &mut c.rates {
        rate.input_nanos_per_token = 99;
    }
    std::fs::write(&override_file, serde_json::to_vec(&c).unwrap()).unwrap();
    let reopened = Store::open(&p, 8, 90, Some(&override_file)).unwrap();
    let after = reopened.details(all()).await.unwrap();
    assert_eq!(before["items"][0]["pricing_snapshot"], after["items"][0]["pricing_snapshot"]);
    let mut invalid = serde_json::to_value(event("codex", "bad")).unwrap();
    invalid["prompt"] = json!("must never persist");
    assert!(serde_json::from_value::<Observation>(invalid).is_err());
}
#[tokio::test]
async fn generated_history_measurement() {
    let p = path("performance");
    let store = Store::open(&p, 1024, 90, None).unwrap();
    let start = std::time::Instant::now();
    let base_ms = Utc::now().timestamp_millis();
    for chunk in 0..50 {
        let events = (0..2000)
            .map(|i| {
                let id = chunk * 2000 + i;
                let mut o = event(if id % 3 == 0 { "codex" } else { "proxy" }, &id.to_string());
                o.event_at_ms = base_ms - i64::from(id) * 1000;
                o.logical_request_id = Some(format!("request-{}", id / 2));
                o.attempt_id = Some(format!("attempt-{id}"));
                o
            })
            .collect::<Vec<_>>();
        store.call(move |c| insert_batch(c, &events)).await.unwrap();
    }
    let ingest = start.elapsed();
    store.flush().await.unwrap();
    let start = std::time::Instant::now();
    let summary = store.query(Query::default()).await.unwrap();
    let query = start.elapsed();
    let start = std::time::Instant::now();
    let empty = store
        .query(Query { start: Some("2026-01-01".into()), end: Some("2026-01-02".into()), ..Default::default() })
        .await
        .unwrap();
    assert_eq!(empty["proxy"]["observations"], 0);
    let empty_query = start.elapsed();
    let start = std::time::Instant::now();
    let narrow = store
        .query(Query {
            start: Some(DateTime::from_timestamp_millis(base_ms - 1000).unwrap().to_rfc3339()),
            end: Some(DateTime::from_timestamp_millis(base_ms + 1).unwrap().to_rfc3339()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        narrow["sources"][0]["observations"].as_i64().unwrap() + narrow["sources"][2]["observations"].as_i64().unwrap(),
        2
    );
    let narrow_query = start.elapsed();
    let querying = store.clone();
    let active = tokio::spawn(async move { querying.query(Query::default()).await.unwrap() });
    for i in 0..1000 {
        assert!(store.enqueue(event("proxy", &format!("concurrent-{i}"))));
        if i % 10 == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }
    active.await.unwrap();
    store.flush().await.unwrap();
    assert_eq!(store.health()["dropped"], 0);
    let start = std::time::Instant::now();
    assert_eq!(store.details(Query::default()).await.unwrap()["total"], 101000);
    let page = start.elapsed();
    println!(
        "SYNTHETIC 100000 records: insert={ingest:?}; summary={query:?}; empty={empty_query:?}; narrow2={narrow_query:?}; concurrent1000_dropped=0; details100={page:?}; database_bytes={}; proxy_entries={}",
        std::fs::metadata(p).unwrap().len(),
        summary["proxy"]["observations"]
    );
}
#[tokio::test]
async fn admin_preserves_settings_and_suppression_is_origin_scoped() {
    let p = path("admin");
    let store = Store::open(&p, 8, 365, None).unwrap();
    let mut a = event("codex", "counter:stable");
    a.origin_id = "collector:a".into();
    let mut b = a.clone();
    b.origin_id = "collector:b".into();
    store.call(move |c| insert_batch(c, &[a, b])).await.unwrap();
    store
        .call(|c| {
            suppress_origin_event(
                c,
                "codex",
                "counter:stable",
                "collector:a",
                "modern response supersedes cumulative fallback",
            )
        })
        .await
        .unwrap();
    assert_eq!(store.query(all()).await.unwrap()["sources"][2]["observations"], 1);
    store
        .call(|c| suppress_source_event(c, "codex", "counter:stable", "all native fallback superseded"))
        .await
        .unwrap();
    assert_eq!(store.query(all()).await.unwrap()["sources"][2]["observations"], 0);
    assert_eq!(store.details(all()).await.unwrap()["total"], 2);
    let admin = Store::open_existing(&p, 8).unwrap();
    assert_eq!(
        admin
            .call(|c| Ok(
                c.query_row("SELECT value FROM usage_meta WHERE key='retention_days'", [], |r| r.get::<_, String>(0))?
            ))
            .await
            .unwrap(),
        "365"
    );
}
#[tokio::test]
async fn bad_queries_and_reads_never_corrupt_writer_health() {
    let store = Store::open(&path("health"), 8, 90, None).unwrap();
    assert!(store.query(Query { timezone: Some("Invalid".into()), ..Default::default() }).await.is_err());
    assert!(
        store
            .call(|c| Ok(
                c.query_row("SELECT value FROM usage_meta WHERE key='unknown'", [], |r| r.get::<_, String>(0))?
            ))
            .await
            .is_err()
    );
    assert_eq!(store.health()["writer_errors"], 0);
    assert_eq!(store.health()["state"], "healthy");
    assert!(store.enqueue(event("proxy", "one")));
    store.flush().await.unwrap();
    let before = store.health()["last_commit_at_ms"].clone();
    tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    store.query(Query::default()).await.unwrap();
    store.call(|_| Ok(())).await.unwrap();
    assert_eq!(store.health()["last_commit_at_ms"], before);
}
#[tokio::test]
async fn distinct_known_accounts_never_collapse_shared_response_claim() {
    let store = Store::open(&path("accounts"), 8, 90, None).unwrap();
    let mut a = event("proxy", "a");
    a.response_id = Some("same".into());
    a.account_id = Some("account-a".into());
    let mut b = a.clone();
    b.source_event_id = "b".into();
    b.account_id = Some("account-b".into());
    store.call(move |c| insert_batch(c, &[a, b])).await.unwrap();
    let value = store.query(all()).await.unwrap();
    assert_eq!(value["proxy"]["observations"], 2);
    assert_eq!(value["proxy"]["conflicts"], 2);
}
#[tokio::test]
async fn logical_requests_and_attempts_are_independent_of_replayed_charges() {
    let store = Store::open(&path("lifecycle"), 8, 90, None).unwrap();
    let mut a = event("proxy", "first-client");
    a.response_id = Some("idempotent-response".into());
    a.account_id = Some("same-account".into());
    a.logical_request_id = Some("logical-one".into());
    a.attempt_id = Some("attempt-one".into());
    let mut b = a.clone();
    b.source_event_id = "second-client".into();
    b.logical_request_id = Some("logical-two".into());
    b.attempt_id = Some("attempt-two".into());
    store.call(move |c| insert_batch(c, &[a, b])).await.unwrap();
    let value = store.query(all()).await.unwrap();
    assert_eq!(value["proxy"]["observations"], 1);
    assert_eq!(value["proxy"]["tokens"]["input"], 1000);
    assert_eq!(value["proxy"]["logical_requests"], 2);
    assert_eq!(value["proxy"]["attempts"], 2);
    assert_eq!(value["sources"][0]["logical_requests"], 2);
    assert_eq!(value["trend"][0]["logical_requests"], 2);
    assert_eq!(value["trend"][0]["attempts"], 2);
    let mut a = event("proxy", "retry-one");
    a.response_id = Some("retry-response-one".into());
    a.logical_request_id = Some("logical-retry".into());
    a.attempt_id = Some("retry-attempt-one".into());
    let mut b = a.clone();
    b.source_event_id = "retry-two".into();
    b.response_id = Some("retry-response-two".into());
    b.attempt_id = Some("retry-attempt-two".into());
    store.call(move |c| insert_batch(c, &[a, b])).await.unwrap();
    let value = store.query(all()).await.unwrap();
    assert_eq!(value["proxy"]["observations"], 3);
    assert_eq!(value["proxy"]["logical_requests"], 3);
    assert_eq!(value["proxy"]["attempts"], 4);
    assert_eq!(value["proxy"]["tokens"]["input"], 3000);
}
#[tokio::test]
async fn replay_lifecycle_counts_respect_raw_date_and_client_filters() {
    let store = Store::open(&path("lifecycle-range"), 8, 90, None).unwrap();
    let mut a = event("proxy", "first-day");
    a.response_id = Some("shared-response".into());
    a.account_id = Some("same-account".into());
    a.logical_request_id = Some("one".into());
    a.attempt_id = Some("one".into());
    a.client_id = Some("client-one".into());
    a.event_at_ms = boundary("2026-10-05T10:00:00Z", chrono_tz::UTC).unwrap();
    let mut b = a.clone();
    b.source_event_id = "second-day".into();
    b.logical_request_id = Some("two".into());
    b.attempt_id = Some("two".into());
    b.client_id = Some("client-two".into());
    b.event_at_ms = boundary("2026-10-06T10:00:00Z", chrono_tz::UTC).unwrap();
    store.call(move |c| insert_batch(c, &[a, b])).await.unwrap();
    let value = store.query(all()).await.unwrap();
    assert_eq!(value["trend"].as_array().unwrap().len(), 2);
    for day in value["trend"].as_array().unwrap() {
        assert_eq!(day["logical_requests"], 1);
        assert_eq!(day["attempts"], 1);
    }
    let value = store.query(Query { client: Some("client-two".into()), ..all() }).await.unwrap();
    assert_eq!(value["proxy"]["logical_requests"], 1);
    assert_eq!(value["proxy"]["attempts"], 1);
    let mut unknown = event("proxy", "unknown");
    unknown.logical_request_id = None;
    unknown.attempt_id = None;
    store.call(move |c| insert_batch(c, &[unknown.clone(), unknown])).await.unwrap();
    let value = store.query(all()).await.unwrap();
    assert_eq!(value["proxy"]["logical_requests_unknown"], 1);
    assert_eq!(value["proxy"]["attempts_unknown"], 1);
    assert_eq!(value["proxy"]["attempts"], 2);
}
#[tokio::test]
async fn durable_gaps_and_unclean_or_concurrent_sessions_survive_restart() {
    let p = path("durable-gaps");
    let store = Store::open(&p, 1, 90, None).unwrap();
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocker = store.clone();
    let task = tokio::spawn(async move {
        blocker
            .call(move |_| {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
            .await
            .unwrap();
    });
    started_rx.await.unwrap();
    assert!(store.enqueue(event("proxy", "accepted")));
    assert!(!store.enqueue(event("proxy", "dropped")));
    release_tx.send(()).unwrap();
    task.await.unwrap();
    store.flush().await.unwrap();
    let concurrent = Store::open_existing(&p, 8).unwrap();
    assert_eq!(concurrent.health()["historical_gap"]["dropped"], 1);
    assert!(concurrent.health()["recovery_warning"].as_str().unwrap().contains("concurrent"));
    concurrent.shutdown().await.unwrap();
    store.shutdown().await.unwrap();
    assert!(!store.enqueue(event("proxy", "after-shutdown")));
    store.shutdown().await.unwrap();
    let reopened = Store::open_existing(&p, 8).unwrap();
    assert_eq!(reopened.health()["historical_gap"]["dropped"], 1);
    assert!(reopened.health()["recovery_warning"].is_null());
    assert_eq!(reopened.health()["state"], "degraded");
    reopened.shutdown().await.unwrap();
    let clean = path("clean-shutdown");
    let store = Store::open(&clean, 8, 90, None).unwrap();
    assert!(store.enqueue(event("proxy", "a")));
    store.shutdown().await.unwrap();
    let reopened = Store::open_existing(&clean, 8).unwrap();
    assert!(reopened.health()["recovery_warning"].is_null());
    assert_eq!(reopened.health()["state"], "healthy");
    reopened.shutdown().await.unwrap();
}
#[tokio::test]
async fn large_exact_integer_cost_overflow_returns_explicit_null() {
    let store = Store::open(&path("aggregate-overflow"), 8, 90, None).unwrap();
    let records = (0..200)
        .map(|i| {
            let mut o = event("codex", &i.to_string());
            o.actual_model = Some("gpt-6-astra".into());
            o.tokens.input = Some(0);
            o.tokens.cache_read = Some(0);
            o.tokens.output = Some(1_000_000_000_000);
            o.tokens.reasoning = Some(0);
            o
        })
        .collect::<Vec<_>>();
    store.call(move |c| insert_batch(c, &records)).await.unwrap();
    let value = store.query(all()).await.unwrap();
    let source = &value["sources"][2];
    assert_eq!(source["observations"], 200);
    assert_eq!(source["unpriced"], 0);
    assert_eq!(source["aggregation_overflow"], true);
    assert!(source["estimated_cost_nanos"].is_null());
    assert!(source["known_cost_nanos"].is_null());
    assert_eq!(source["tokens"]["output"], 200_000_000_000_000_u64);
    assert_eq!(store.details(all()).await.unwrap()["total"], 200);
    assert_eq!(store.health()["writer_errors"], 0);
    store.shutdown().await.unwrap();
}
