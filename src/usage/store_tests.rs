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
async fn collector_limit_precedes_startup_and_concurrent_open_preserves_delete_mode() {
    let p = path("collector-startup-cap");
    let limit = 512 * 1024;
    let store = Store::open_collector(&p, limit).unwrap();
    let second = Store::open_collector(&p, limit).unwrap();
    for opened in [&store, &second] {
        opened
            .call(move |c| {
                assert_eq!(c.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))?, "delete");
                let page: u64 = c.query_row("PRAGMA page_size", [], |r| r.get(0))?;
                let max: u64 = c.query_row("PRAGMA max_page_count", [], |r| r.get(0))?;
                assert_eq!(page * max, limit);
                Ok(())
            })
            .await
            .unwrap();
    }
    second.shutdown().await.unwrap();
    drop(second);
    store
        .call(|c| {
            c.execute_batch("CREATE TABLE cap_fixture(data BLOB)")?;
            loop {
                match c.execute("INSERT INTO cap_fixture VALUES(zeroblob(4096))", []) {
                    Ok(_) => {}
                    Err(e) => {
                        assert_eq!(e.sqlite_error_code(), Some(rusqlite::ErrorCode::DiskFull));
                        break;
                    }
                }
            }
            Ok(())
        })
        .await
        .unwrap();
    let _ = store.shutdown().await;
    drop(store);
    let reopened = Store::open_collector(&p, limit);
    if let Ok(store) = reopened {
        assert_eq!(
            store.call(|c| Ok(c.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))?)).await.unwrap(),
            "delete"
        );
        let _ = store.shutdown().await;
    }
    assert!(std::fs::metadata(&p).unwrap().len() <= limit);
    // A lower cap must reject before any catalogue/session/migration writes.
    // Hold a shared read lock so a dropped writer's final health persistence
    // cannot race this byte-for-byte nonmutation check.
    let guard = Connection::open(&p).unwrap();
    guard.execute_batch("BEGIN; SELECT COUNT(*) FROM usage_meta;").unwrap();
    let before = Sha256::digest(std::fs::read(&p).unwrap());
    let error = Store::open_collector(&p, limit / 2).err().unwrap().to_string();
    assert!(error.contains("exceeds size limit"), "{error}");
    assert_eq!(Sha256::digest(std::fs::read(&p).unwrap()), before);
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
    let value = store.reference_summary(all()).await.unwrap();
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
        let summary = store.reference_summary(all()).await.unwrap();
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
    let summary = store.reference_summary(all()).await.unwrap();
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
    let old = c.price(&a);
    assert_eq!(
        (old.basis.as_str(), old.cost_nanos, old.backdated),
        ("current_rate_equivalent", Some(1_089_124_000), true)
    );
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
    assert!(store.reference_summary(Query { timezone: Some("Nope".into()), ..Default::default() }).await.is_err());
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
    assert_eq!(store.reference_summary(all()).await.unwrap()["proxy"]["conflicts"], 0);
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
    let summary = store.reference_summary(Query::default()).await.unwrap();
    let query = start.elapsed();
    let start = std::time::Instant::now();
    let dashboard = store.dashboard(Query::default()).await.unwrap();
    let dashboard_query = start.elapsed();
    assert_eq!(dashboard["combined"]["totals"]["observations"], summary["combined"]["totals"]["observations"]);
    assert_eq!(dashboard["combined"]["totals"]["tokens"], summary["combined"]["totals"]["tokens"]);
    assert_eq!(
        dashboard["combined"]["totals"]["estimated_cost_nanos"],
        summary["combined"]["totals"]["estimated_cost_nanos"]
    );
    let start = std::time::Instant::now();
    let empty = store
        .reference_summary(Query {
            start: Some("2026-01-01".into()),
            end: Some("2026-01-02".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(empty["proxy"]["observations"], 0);
    let empty_query = start.elapsed();
    let start = std::time::Instant::now();
    let narrow = store
        .reference_summary(Query {
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
    let active = tokio::spawn(async move { querying.dashboard(Query::default()).await.unwrap() });
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
        "SYNTHETIC 100000 records: insert={ingest:?}; summary={query:?}; dashboard={dashboard_query:?}; empty={empty_query:?}; narrow2={narrow_query:?}; concurrent1000_dropped=0; details100={page:?}; database_bytes={}; proxy_entries={}",
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
    assert_eq!(store.reference_summary(all()).await.unwrap()["sources"][2]["observations"], 1);
    store
        .call(|c| suppress_source_event(c, "codex", "counter:stable", "all native fallback superseded"))
        .await
        .unwrap();
    assert_eq!(store.reference_summary(all()).await.unwrap()["sources"][2]["observations"], 0);
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
    assert!(store.reference_summary(Query { timezone: Some("Invalid".into()), ..Default::default() }).await.is_err());
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
    store.reference_summary(Query::default()).await.unwrap();
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
    let value = store.reference_summary(all()).await.unwrap();
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
    let value = store.reference_summary(all()).await.unwrap();
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
    let value = store.reference_summary(all()).await.unwrap();
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
    let value = store.reference_summary(all()).await.unwrap();
    assert_eq!(value["trend"].as_array().unwrap().len(), 2);
    for day in value["trend"].as_array().unwrap() {
        assert_eq!(day["logical_requests"], 1);
        assert_eq!(day["attempts"], 1);
    }
    let value = store.reference_summary(Query { client: Some("client-two".into()), ..all() }).await.unwrap();
    assert_eq!(value["proxy"]["logical_requests"], 1);
    assert_eq!(value["proxy"]["attempts"], 1);
    let mut unknown = event("proxy", "unknown");
    unknown.logical_request_id = None;
    unknown.attempt_id = None;
    store.call(move |c| insert_batch(c, &[unknown.clone(), unknown])).await.unwrap();
    let value = store.reference_summary(all()).await.unwrap();
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
    let value = store.reference_summary(all()).await.unwrap();
    let source = &value["sources"][2];
    assert_eq!(source["observations"], 200);
    assert_eq!(source["unpriced"], 0);
    assert_eq!(source["aggregation_overflow"], true);
    assert!(source["estimated_cost_nanos"].is_null());
    assert!(source["known_cost_nanos"].is_null());
    assert_eq!(source["tokens"]["output"], 200_000_000_000_000_u64);
    assert_dashboard_matches(&store, all()).await;
    assert_eq!(store.details(all()).await.unwrap()["total"], 200);
    assert_eq!(store.health()["writer_errors"], 0);
    store.shutdown().await.unwrap();
}

// Compare independent SQL and streaming implementations at their shared contract.
fn dashboard_metrics(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            for key in ["logical_requests", "attempts", "logical_requests_unknown", "attempts_unknown", "trends"] {
                fields.remove(key);
            }
            for value in fields.values_mut() {
                dashboard_metrics(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                dashboard_metrics(value);
            }
        }
        _ => {}
    }
}
async fn assert_dashboard_matches(store: &Store, q: Query) {
    let mut full = store.reference_summary(q.clone()).await.unwrap();
    let mut fast = store.dashboard(q).await.unwrap();
    dashboard_metrics(&mut full);
    dashboard_metrics(&mut fast);
    for field in ["range", "combined", "facets"] {
        assert_eq!(fast[field], full[field], "dashboard contract differs at {field}");
    }
    for field in ["version", "verified_at"] {
        assert_eq!(fast["pricing"][field], full["pricing"][field]);
    }
}
#[tokio::test]
async fn dashboard_matches_full_summary_with_overlap_revisions_filters_and_fresh_writes() {
    let store = Store::open(&path("dashboard-equivalence"), 64, 3650, None).unwrap();
    let enrollment = super::super::collector::enroll(&store, "Test laptop".into()).await.unwrap();
    let origin = format!("collector:{}", enrollment["collector"]["id"].as_str().unwrap());
    let mut records = Vec::new();
    for i in 0..40 {
        let mut o = event(["proxy", "codex", "claude_code"][i % 3], &format!("dashboard-{i}"));
        o.event_at_ms = boundary("2026-10-01T22:00:00Z", chrono_tz::UTC).unwrap() + i as i64 * 3_600_000;
        o.provider = if i % 2 == 0 { "anthropic" } else { "openai" }.into();
        o.actual_model =
            if i % 5 == 0 { None } else { Some(if i % 2 == 0 { "claude-sonnet-4-6" } else { "gpt-6.1-sol" }.into()) };
        o.account_id = (i % 4 != 0).then(|| format!("account-{}", i % 3));
        o.client_id = (i % 7 == 0).then(|| "named-client".into());
        if i % 3 != 0 {
            o.origin_id = origin.clone();
        }
        if i % 11 == 0 {
            o.tokens = Tokens::default();
            o.completeness = "missing".into();
        } else if i % 5 == 0 {
            o.tokens.cache_write = None;
            o.completeness = "partial".into();
        }
        records.push(o);
    }
    let mut proxy = anthropic("proxy", "trusted", Some("shared"), "2026-10-01T21:59:00Z");
    proxy.account_id = Some("outside-filter".into());
    let mut history = anthropic("claude_code", "matched", Some("shared"), "2026-10-02T22:01:00Z");
    history.origin_id = origin.clone();
    records.extend([proxy, history]);
    let mut copy = records[1].clone();
    copy.origin_id = "local-copy".into();
    records.push(copy);
    store.call(move |c| insert_batch(c, &records)).await.unwrap();
    let base = Query {
        start: Some("2026-10-02".into()),
        end: Some("2026-10-04".into()),
        timezone: Some("Europe/Amsterdam".into()),
        ..Default::default()
    };
    let filters = [
        Query::default(),
        Query { stack: Some("model".into()), ..Default::default() },
        Query { provider: Some("anthropic".into()), ..Default::default() },
        Query { model: Some("gpt-6.1-sol".into()), ..Default::default() },
        Query { account: Some("account-1".into()), ..Default::default() },
        Query { client: Some(origin), ..Default::default() },
        Query { source: Some("claude_code".into()), ..Default::default() },
        Query { client: Some("does-not-exist".into()), ..Default::default() },
    ];
    for filter in filters {
        assert_dashboard_matches(
            &store,
            Query { start: base.start.clone(), end: base.end.clone(), timezone: base.timezone.clone(), ..filter },
        )
        .await;
    }
    let before = store.dashboard(base.clone()).await.unwrap();
    let mut fresh = anthropic("proxy", "fresh", None, "2026-10-02T10:00:00Z");
    fresh.client_id = Some("just-arrived".into());
    store.call(move |c| insert_batch(c, &[fresh])).await.unwrap();
    let after = store.dashboard(base.clone()).await.unwrap();
    assert_eq!(
        after["combined"]["totals"]["observations"].as_i64().unwrap(),
        before["combined"]["totals"]["observations"].as_i64().unwrap() + 1
    );
    assert_dashboard_matches(&store, base.clone()).await;
    store.purge(boundary("2026-10-03T00:00:00Z", chrono_tz::UTC).unwrap()).await.unwrap();
    assert_dashboard_matches(&store, base).await;
    assert!(store.dashboard(Query { timezone: Some("invalid".into()), ..all() }).await.is_err());
    store.shutdown().await.unwrap();
}
#[tokio::test]
async fn dashboard_groups_dst_days_and_bounds_facets_and_breakdowns() {
    let store = Store::open(&path("dashboard-bounds-dst"), 64, 3650, None).unwrap();
    let mut records = Vec::new();
    for i in 0..520 {
        let mut o = event("proxy", &format!("dst-{i}"));
        o.event_at_ms = boundary("2026-03-28T22:30:00Z", chrono_tz::UTC).unwrap() + i * 900_000;
        o.actual_model = Some(format!("unknown-{i:03}"));
        o.account_id = Some(format!("account-{i:03}"));
        o.client_id = Some(format!("client-{i:03}"));
        records.push(o);
    }
    store.call(move |c| insert_batch(c, &records)).await.unwrap();
    for tz in ["Europe/Amsterdam", "America/Los_Angeles", "Asia/Kathmandu"] {
        assert_dashboard_matches(
            &store,
            Query {
                start: Some("2026-03-27".into()),
                end: Some("2026-04-06".into()),
                timezone: Some(tz.into()),
                stack: Some("model".into()),
                ..Default::default()
            },
        )
        .await;
    }
    let q = Query { start: Some("2026-03-27".into()), end: Some("2026-04-06".into()), ..Default::default() };
    let fast = store.dashboard(q).await.unwrap();
    assert_eq!(fast["facets"]["models"].as_array().unwrap().len(), 500);
    assert_eq!(fast["combined"]["breakdowns"]["account"].as_array().unwrap().len(), 500);
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn simultaneous_publishers_share_ingress_gate_without_artificial_drops() {
    const PRODUCERS: usize = 32;
    const EVENTS_PER_PRODUCER: usize = 8;
    let store = Store::open(&path("simultaneous-publishers"), 512, 90, None).unwrap();
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocker = store.clone();
    let blocked = tokio::spawn(async move {
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
    let barrier = Arc::new(std::sync::Barrier::new(PRODUCERS));
    let producers = (0..PRODUCERS)
        .map(|producer| {
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let observations = (0..EVENTS_PER_PRODUCER)
                    .map(|n| event("proxy", &format!("producer-{producer}-{n}")))
                    .collect::<Vec<_>>();
                observations
                    .into_iter()
                    .map(|observation| {
                        barrier.wait();
                        usize::from(store.enqueue(observation))
                    })
                    .sum::<usize>()
            })
        })
        .collect::<Vec<_>>();
    let accepted = producers.into_iter().map(|thread| thread.join().unwrap()).sum::<usize>();
    let health = store.health();
    // Release the worker before assertions so a failure cannot strand its task.
    release_tx.send(()).unwrap();
    blocked.await.unwrap();
    assert_eq!(accepted, PRODUCERS * EVENTS_PER_PRODUCER);
    assert_eq!(health["queue_depth"], PRODUCERS * EVENTS_PER_PRODUCER);
    assert_eq!(health["dropped"], 0);
    store.flush().await.unwrap();
    assert_eq!(store.details(all()).await.unwrap()["total"], PRODUCERS * EVENTS_PER_PRODUCER);
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn sqlite_full_rolls_back_records_and_exposes_proxy_loss() {
    let store = Store::open(&path("sqlite-full"), 512, 90, None).unwrap();
    store
        .call(|conn| {
            let pages: i64 = conn.pragma_query_value(None, "page_count", |row| row.get(0))?;
            conn.pragma_update(None, "max_page_count", pages)?;
            Ok(())
        })
        .await
        .unwrap();
    let oversized = |id: &str| {
        let mut observation = event("proxy", id);
        let label = "x".repeat(256);
        observation.requested_model = Some(label.clone());
        observation.account_id = Some(label.clone());
        observation.auth_type = Some(label.clone());
        observation.client_id = Some(label.clone());
        observation.logical_request_id = Some(label.clone());
        observation.attempt_id = Some(label.clone());
        observation.provider_request_id = Some(label.clone());
        observation.response_id = Some(label.clone());
        observation.session_id = Some(label.clone());
        observation.service_tier = Some(label.clone());
        observation.inference_geo = Some(label);
        observation.validate().unwrap();
        observation
    };
    let records = (0..200).map(|i| oversized(&format!("atomic-{i}"))).collect::<Vec<_>>();
    let error = store.call(move |conn| insert_batch(conn, &records)).await.unwrap_err();
    let code = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<rusqlite::Error>().and_then(rusqlite::Error::sqlite_error_code));
    assert_eq!(code, Some(rusqlite::ErrorCode::DiskFull));
    assert_eq!(store.details(all()).await.unwrap()["total"], 0);
    assert_eq!(store.reference_summary(all()).await.unwrap()["proxy"]["observations"], 0);
    assert!(store.enqueue(oversized("failed-proxy")));
    // A custom call is a queue barrier even though the earlier full-disk gap makes
    // flush return an error. All three accounting tables must remain unchanged.
    store
        .call(|conn| {
            for table in ["usage_observations", "usage_entries", "usage_source_entries"] {
                let count: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))?;
                assert_eq!(count, 0);
            }
            Ok(())
        })
        .await
        .unwrap();
    assert!(store.flush().await.is_err());
    assert_eq!(store.health()["state"], "degraded");
    assert_eq!(store.health()["dropped"], 1);
    assert!(store.health()["writer_errors"].as_u64().unwrap() >= 2);
    assert!(store.health()["message"].as_str().unwrap().contains("full"));
    store
        .call(|conn| {
            conn.pragma_update(None, "max_page_count", 1_000_000_i64)?;
            Ok(())
        })
        .await
        .unwrap();
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn collector_filter_retains_raw_evidence_when_local_copy_wins_accounting() {
    let store = Store::open(&path("collector-filter-evidence"), 8, 90, None).unwrap();
    let mut local = event("claude_code", "native-copied-message");
    local.provider = "anthropic".into();
    local.actual_model = Some("claude-sonnet-4-6".into());
    local.response_id = Some("native-copied-message".into());
    let mut collector_x = local.clone();
    collector_x.origin_id = "collector:X".into();
    let mut collector_y = local.clone();
    collector_y.origin_id = "collector:Y".into();
    store.call(move |conn| insert_batch(conn, &[collector_x, collector_y, local])).await.unwrap();
    let complete = store.reference_summary(all()).await.unwrap();
    let source = complete["sources"].as_array().unwrap().iter().find(|entry| entry["source"] == "claude_code").unwrap();
    assert_eq!(source["observations"], 1);
    assert_eq!(source["source_record_count"], 3);
    let query = Query { client: Some("collector:X".into()), ..all() };
    let summary = store.reference_summary(query.clone()).await.unwrap();
    let source = summary["sources"].as_array().unwrap().iter().find(|entry| entry["source"] == "claude_code").unwrap();
    assert_eq!(source["observations"], 0);
    assert_eq!(source["source_record_count"], 1);
    assert_eq!(store.details(query).await.unwrap()["total"], 1);
    assert!(summary["facets"]["clients"].as_array().unwrap().iter().any(|client| client["id"] == "collector:X"));
    store.shutdown().await.unwrap();
}

fn query(v: Value) -> Query {
    serde_json::from_value(v).unwrap()
}
fn anthropic(source: &str, id: &str, response: Option<&str>, at: &str) -> Observation {
    let mut o = event(source, id);
    o.provider = "anthropic".into();
    o.actual_model = Some("claude-sonnet-4-6".into());
    o.response_id = response.map(Into::into);
    o.event_at_ms = boundary(at, chrono_tz::UTC).unwrap();
    o
}
#[tokio::test]
async fn combined_set_counts_shared_response_once_and_proxy_wins() {
    let store = Store::open(&path("combined"), 8, 90, None).unwrap();
    let mut p1 = anthropic("proxy", "p1", Some("r1"), "2026-10-01T10:00:00Z");
    p1.account_id = Some("acct".into());
    let mut c1 = anthropic("claude_code", "c1", Some("r1"), "2026-10-01T10:00:00Z");
    c1.tokens.output = Some(999);
    let c2 = anthropic("claude_code", "c2", Some("r2"), "2026-10-01T11:00:00Z");
    // A proxy entry and its imported copy on opposite sides of midnight.
    let mut p3 = anthropic("proxy", "p3", Some("r3"), "2026-10-01T23:59:30Z");
    p3.account_id = Some("acct".into());
    let c3 = anthropic("claude_code", "c3", Some("r3"), "2026-10-02T00:00:30Z");
    let mut weak = event("codex", "weak");
    weak.event_at_ms = boundary("2026-10-02T12:00:00Z", chrono_tz::UTC).unwrap();
    let key = association_key(&p1);
    store.call(move |c| insert_batch(c, &[p1, c1, c2, p3, c3, weak])).await.unwrap();
    // Root cause: the account-scoped proxy key and the imported key both survive.
    let copies: i64 = store
        .call(move |c| {
            Ok(c.query_row("SELECT COUNT(*) FROM usage_entries WHERE association_key=?1", [key], |r| r.get(0))?)
        })
        .await
        .unwrap();
    assert_eq!(copies, 2);
    let range = |start: &str, end: &str| query(json!({"start":start,"end":end,"timezone":"UTC"}));
    let summary = store.reference_summary(range("2026-10-01", "2026-10-03")).await.unwrap();
    let combined = &summary["combined"];
    assert_eq!(combined["totals"]["observations"], 4, "{combined}");
    assert_eq!(combined["totals"]["history_only"], 2);
    assert_eq!(combined["totals"]["matched"], 2);
    assert_eq!(combined["totals"]["weak_identity"], 1);
    assert_eq!(combined["totals"]["tokens"]["output"], 400);
    assert_eq!(combined["totals"]["unpriced"], 0);
    assert_eq!(combined["stack"], "provider");
    assert!(combined["basis"].as_str().unwrap().contains("share a response ID"));
    let account = combined["breakdowns"]["account"].as_array().unwrap();
    let acct = account.iter().find(|a| a["id"] == "acct").unwrap();
    assert_eq!((acct["observations"].as_i64(), acct["provider"].as_str()), (Some(2), Some("anthropic")));
    let first = combined["proxy_first_event_at_ms"].as_i64().unwrap();
    assert_eq!(first, boundary("2026-10-01T10:00:00Z", chrono_tz::UTC).unwrap());
    // The imported copy after midnight is still matched to the proxy row before it.
    let late = store.reference_summary(range("2026-10-02", "2026-10-03")).await.unwrap();
    assert_eq!(late["combined"]["totals"]["observations"], 1);
    assert_eq!(late["combined"]["totals"]["matched"], 1);
    assert_eq!(late["combined"]["totals"]["history_only"], 1);
    assert_eq!(late["combined"]["proxy_first_event_at_ms"], json!(first));
    let early = store.reference_summary(range("2026-10-01", "2026-10-02")).await.unwrap();
    assert_eq!(early["combined"]["totals"]["observations"], 3);
    assert_eq!(early["combined"]["totals"]["matched"], 1);
    // Existing per-source fields remain for older clients.
    assert_eq!(summary["proxy"]["observations"], 2);
    assert!(summary["reconciliation"]["cross_source_grand_total"].is_null());
    store.shutdown().await.unwrap();
}
#[tokio::test]
async fn combined_trend_groups_by_provider_and_model_across_dst() {
    let store = Store::open(&path("combined-dst"), 8, 3650, None).unwrap();
    // Amsterdam moves from CET (+1) to CEST (+2) at 2026-03-29T01:00Z.
    let a = anthropic("claude_code", "a", Some("ra"), "2026-03-28T22:30:00Z");
    let mut b = event("proxy", "b");
    b.event_at_ms = boundary("2026-03-28T23:30:00Z", chrono_tz::UTC).unwrap();
    let mut c = event("codex", "c");
    c.event_at_ms = boundary("2026-03-29T21:30:00Z", chrono_tz::UTC).unwrap();
    let d = anthropic("proxy", "d", Some("rd"), "2026-03-29T22:30:00Z");
    store.call(move |conn| insert_batch(conn, &[a, b, c, d])).await.unwrap();
    let base = json!({"start":"2026-03-27","end":"2026-04-01","timezone":"Europe/Amsterdam"});
    let rows = |v: &Value| {
        v["combined"]["trend"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                assert!(r["estimated_cost_nanos"].as_i64().unwrap() > 0, "{r}");
                assert_eq!(r["unpriced"], 0);
                (
                    r["date"].as_str().unwrap().to_owned(),
                    r["group"].as_str().unwrap().to_owned(),
                    r["observations"].as_i64().unwrap(),
                )
            })
            .collect::<Vec<_>>()
    };
    let summary = store.reference_summary(query(base.clone())).await.unwrap();
    let expected = |a: &str, o: &str| {
        vec![
            ("2026-03-28".to_owned(), a.to_owned(), 1),
            ("2026-03-29".to_owned(), o.to_owned(), 2),
            ("2026-03-30".to_owned(), a.to_owned(), 1),
        ]
    };
    assert_eq!(rows(&summary), expected("anthropic", "openai"));
    let mut by_model = base.clone();
    by_model["stack"] = json!("model");
    let summary = store.reference_summary(query(by_model)).await.unwrap();
    assert_eq!(summary["combined"]["stack"], "model");
    assert_eq!(rows(&summary), expected("claude-sonnet-4-6", "gpt-6.1-sol"));
    let mut bad = base;
    bad["stack"] = json!("source");
    assert!(store.reference_summary(query(bad)).await.is_err());
    store.shutdown().await.unwrap();
}
#[tokio::test]
async fn combined_observations_report_matched_sources_and_origin_label() {
    let store = Store::open(&path("combined-records"), 8, 90, None).unwrap();
    let enrolled = super::super::collector::enroll(&store, "Work laptop".into()).await.unwrap();
    let origin = format!("collector:{}", enrolled["collector"]["id"].as_str().unwrap());
    let mut p1 = anthropic("proxy", "p1", Some("r1"), "2026-10-01T10:00:00Z");
    p1.account_id = Some("acct".into());
    let c1 = anthropic("claude_code", "c1", Some("r1"), "2026-10-01T10:00:00Z");
    let mut copy = c1.clone();
    copy.origin_id = origin.clone();
    let mut c2 = anthropic("claude_code", "c2", Some("r2"), "2026-10-01T11:00:00Z");
    c2.origin_id = origin;
    let mut weak = event("codex", "weak");
    weak.event_at_ms = boundary("2026-10-01T12:00:00Z", chrono_tz::UTC).unwrap();
    store.call(move |c| insert_batch(c, &[p1, c1, copy, c2, weak])).await.unwrap();
    let base = json!({"start":"2026-10-01","end":"2026-10-02","timezone":"UTC"});
    assert_eq!(store.details(query(base.clone())).await.unwrap()["total"], 5);
    let mut raw = base.clone();
    raw["view"] = json!("raw");
    assert_eq!(store.details(query(raw)).await.unwrap()["total"], 5);
    let mut combined = base.clone();
    combined["view"] = json!("combined");
    let page = store.details(query(combined)).await.unwrap();
    assert_eq!(page["total"], 3, "{page}");
    let items = page["items"].as_array().unwrap();
    let ids = items.iter().map(|i| i["source_event_id"].as_str().unwrap()).collect::<Vec<_>>();
    assert_eq!(ids, ["weak", "c2", "p1"]);
    assert_eq!(items[0]["origin_label"], "This server");
    assert_eq!(items[0]["matched_sources"], json!([]));
    assert_eq!(items[1]["origin_label"], "Work laptop");
    assert_eq!(items[1]["collector_label"], "Work laptop");
    assert_eq!(items[2]["matched_sources"], json!(["claude_code"]));
    assert!(items[2]["origin_label"].is_null());
    assert!(items[2]["pricing_snapshot"]["cost_nanos"].is_i64());
    let mut bad = base;
    bad["view"] = json!("sources");
    assert!(store.details(query(bad)).await.is_err());
    store.shutdown().await.unwrap();
}
/// Insert observations as releases before current-rate backdating stored them: a rate
/// without a start date began on the catalogue's verification day.
fn insert_legacy(c: &mut Connection, observations: &[Observation]) -> Result<Value> {
    let raw: String = c.query_row("SELECT value FROM usage_meta WHERE key='catalogue'", [], |r| r.get(0))?;
    let mut legacy: Catalogue = serde_json::from_str(&raw)?;
    for rate in &mut legacy.rates {
        rate.effective_from = rate.effective_from.clone().or_else(|| Some(legacy.verified_at.clone()));
    }
    c.execute("UPDATE usage_meta SET value=?1 WHERE key='catalogue'", [serde_json::to_string(&legacy)?])?;
    let result = insert_batch(c, observations)?;
    c.execute("UPDATE usage_meta SET value=?1 WHERE key='catalogue'", [raw])?;
    Ok(result)
}
#[tokio::test]
async fn startup_reprice_prices_old_unpriced_rows_once_and_keeps_priced_rows() {
    let p = path("reprice");
    let store = Store::open(&p, 8, 3650, None).unwrap();
    let mut old = (0..2100)
        .map(|i| anthropic("claude_code", &format!("old-{i}"), Some(&format!("old-r{i}")), "2026-03-01T12:00:00Z"))
        .collect::<Vec<_>>();
    let mut proxy = anthropic("proxy", "old-proxy", Some("old-r0"), "2026-03-01T12:00:00Z");
    proxy.account_id = Some("acct".into());
    proxy.service_tier = Some("standard".into());
    proxy.inference_geo = Some("global".into());
    old.push(proxy);
    let mut counter = event("codex", "counter:old");
    counter.event_at_ms = boundary("2026-03-01T12:00:00Z", chrono_tz::UTC).unwrap();
    old.push(counter);
    let current = event("proxy", "current");
    store.call(move |c| insert_legacy(c, &old)).await.unwrap();
    store.call(move |c| insert_batch(c, &[current])).await.unwrap();
    let snapshot = |s: Store| async move {
        s.call(|c| {
            let unpriced: i64 = c.query_row(
                "SELECT COUNT(*) FROM usage_observations WHERE pricing_basis='outside_effective_period'",
                [],
                |r| r.get(0),
            )?;
            let current: String =
                c.query_row("SELECT snapshot_json FROM usage_observations WHERE source_event_id='current'", [], |r| {
                    r.get(0)
                })?;
            let entries: i64 =
                c.query_row("SELECT COUNT(*) FROM usage_source_entries WHERE cost_nanos IS NULL", [], |r| r.get(0))?;
            Ok((unpriced, current, entries))
        })
        .await
        .unwrap()
    };
    let (unpriced, current_before, unpriced_entries) = snapshot(store.clone()).await;
    assert_eq!((unpriced, unpriced_entries), (2102, 2102));
    let q = || query(json!({"start":"2026-03-01","end":"2026-03-02","timezone":"UTC"}));
    assert_eq!(store.reference_summary(q()).await.unwrap()["combined"]["totals"]["unpriced"], 2101);
    store.shutdown().await.unwrap();
    drop(store);
    // The CLI path opens through the same startup step and reports its count.
    let admin = Store::open_existing(&p, 8).unwrap();
    assert_eq!(super::super::imports::reprice(&admin).await.unwrap(), json!({"repriced":2102}));
    admin.shutdown().await.unwrap();
    drop(admin);
    let store = Store::open(&p, 8, 3650, None).unwrap();
    assert_eq!(store.startup_repriced(), 0);
    assert_eq!(super::super::imports::reprice(&store).await.unwrap(), json!({"repriced":0}));
    let (unpriced, current_after, unpriced_entries) = snapshot(store.clone()).await;
    // Codex cumulative counters keep their own unpriced basis.
    assert_eq!((unpriced, unpriced_entries), (0, 1));
    assert_eq!(current_before, current_after);
    let summary = store.reference_summary(q()).await.unwrap();
    let totals = &summary["combined"]["totals"];
    assert_eq!((totals["observations"].as_i64(), totals["unpriced"].as_i64()), (Some(2101), Some(1)));
    assert_eq!(totals["matched"], 1);
    let page = store
        .details(query(
            json!({"start":"2026-03-01","end":"2026-03-02","timezone":"UTC","view":"combined","source":"proxy"}),
        ))
        .await
        .unwrap();
    let item = &page["items"][0];
    assert_eq!(item["pricing_basis"], "current_rate_equivalent");
    assert_eq!(item["pricing_snapshot"]["backdated"], true);
    assert_eq!(item["pricing_snapshot"]["partial"], false);
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn startup_repairs_unpriced_claude_regions_and_preserves_priced_snapshots() {
    let p = path("region-reprice");
    let store = Store::open(&p, 8, 3650, None).unwrap();
    let mut unavailable = anthropic("proxy", "unavailable", Some("msg-unavailable"), &Utc::now().to_rfc3339());
    unavailable.inference_geo = Some("not_available".into());
    let mut unknown = unavailable.clone();
    unknown.source_event_id = "unknown".into();
    unknown.response_id = Some("msg-unknown".into());
    unknown.inference_geo = Some("unknown_region".into());
    let priced = event("proxy", "priced");
    store.call(move |c| {
        insert_batch(c, &[unavailable, unknown, priced])?;
        // Recreate an old build's unpriced snapshot while retaining its native
        // metadata. The same startup path is used on the production database.
        c.execute("UPDATE usage_observations SET cost_nanos=NULL,pricing_basis='unsupported_inference_region',snapshot_json='{}' WHERE source_event_id='unavailable'", [])?;
        let key: String = c.query_row("SELECT canonical_key FROM usage_observations WHERE source_event_id='unavailable'", [], |r| r.get(0))?;
        rebuild_entries(c, &key, "proxy")?;
        Ok(())
    }).await.unwrap();
    let before: String = store
        .call(|c| {
            Ok(c.query_row("SELECT snapshot_json FROM usage_observations WHERE source_event_id='priced'", [], |r| {
                r.get(0)
            })?)
        })
        .await
        .unwrap();
    store.shutdown().await.unwrap();
    let store = Store::open(&p, 8, 3650, None).unwrap();
    assert_eq!(store.startup_repriced(), 1);
    assert_eq!(store.reference_summary(all()).await.unwrap()["proxy"]["unpriced"], 1);
    store
        .call(move |c| {
            let (cost, raw): (i64, String) = c.query_row(
                "SELECT cost_nanos,snapshot_json FROM usage_observations WHERE source_event_id='unavailable'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            assert!(cost > 0);
            let snapshot: Value = serde_json::from_str(&raw)?;
            assert_eq!(snapshot["partial"], true);
            assert!(
                snapshot["assumptions"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("inference_region_unavailable_global_rate_assumed"))
            );
            let after: String =
                c.query_row("SELECT snapshot_json FROM usage_observations WHERE source_event_id='priced'", [], |r| {
                    r.get(0)
                })?;
            assert_eq!(before, after);
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(store.call(reprice).await.unwrap(), 0);
    store.shutdown().await.unwrap();
}
#[tokio::test]
async fn migrations_chain_to_v3_and_keep_existing_rows() {
    let indexed = |c: &Connection| -> rusqlite::Result<(i64, bool, bool)> {
        Ok((
            c.query_row("PRAGMA user_version", [], |r| r.get(0))?,
            c.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='usage_entries_association')",
                [],
                |r| r.get(0),
            )?,
            c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='usage_writer_sessions')", [], |r| {
                r.get(0)
            })?,
        ))
    };
    for from in [2, 1] {
        let p = path(&format!("migrate-v{from}"));
        let store = Store::open(&p, 8, 90, None).unwrap();
        assert_eq!(store.call(move |c| Ok(indexed(c)?)).await.unwrap(), (3, true, true));
        let mut proxy = anthropic("proxy", "p", Some("r"), "2026-10-01T10:00:00Z");
        proxy.account_id = Some("acct".into());
        let copy = anthropic("claude_code", "c", Some("r"), "2026-10-01T10:00:00Z");
        store.call(move |c| insert_batch(c, &[proxy, copy])).await.unwrap();
        store.shutdown().await.unwrap();
        drop(store);
        let c = Connection::open(&p).unwrap();
        c.execute_batch("DROP INDEX usage_entries_association;").unwrap();
        if from == 1 {
            c.execute_batch("DROP TABLE usage_writer_sessions;").unwrap();
        }
        c.pragma_update(None, "user_version", from).unwrap();
        assert_eq!(indexed(&c).unwrap(), (from, false, from == 2));
        drop(c);
        let store = Store::open(&p, 8, 90, None).unwrap();
        assert_eq!(store.call(move |c| Ok(indexed(c)?)).await.unwrap(), (3, true, true));
        let summary = store
            .reference_summary(query(json!({"start":"2026-10-01","end":"2026-10-02","timezone":"UTC"})))
            .await
            .unwrap();
        assert_eq!(summary["combined"]["totals"]["observations"], 1);
        assert_eq!(summary["combined"]["totals"]["matched"], 1);
        assert_eq!(store.details(all()).await.unwrap()["total"], 2);
        store.shutdown().await.unwrap();
    }
}
