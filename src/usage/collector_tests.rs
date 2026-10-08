use super::*;
use axum::{Router, extract::State, routing::post};
use std::sync::Arc;
struct TestDir(PathBuf);
impl TestDir {
    fn new() -> Self {
        // Resolved, since symlinked state and import paths are refused (macOS's /var is one).
        let p = std::env::temp_dir().join(format!("fusebox-collector-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&p).unwrap();
        Self(fs::canonicalize(&p).unwrap())
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
fn observation(id: &str) -> Observation {
    let mut o = Observation::new("codex", format!("response:{id}"), "openai", chrono::Utc::now().timestamp_millis());
    o.actual_model = Some("gpt-6.1-sol".into());
    o.response_id = Some(id.into());
    o.tokens = super::super::types::Tokens {
        input: Some(10),
        cache_read: Some(0),
        cache_write: Some(0),
        output: Some(2),
        reasoning: Some(1),
        ..Default::default()
    };
    o.completeness = "complete".into();
    o
}
fn headers(token: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(axum::http::header::AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
    h
}
fn batch(o: Vec<Observation>) -> Bytes {
    Bytes::from(
        serde_json::to_vec(&Batch {
            version: 1,
            observations: o,
            pending: 1,
            superseded: Vec::new(),
            progress: Vec::new(),
        })
        .unwrap(),
    )
}
async fn body(response: Response) -> Value {
    serde_json::from_slice(&axum::body::to_bytes(response.into_body(), MAX_BODY).await.unwrap()).unwrap()
}
async fn endpoint(State(store): State<Store>, headers: HeaderMap, body: Bytes) -> Response {
    ingest(&store, headers, body).await
}
async fn server(store: Store) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/api/usage-ingest", listener.local_addr().unwrap());
    let app = Router::new().route("/api/usage-ingest", post(endpoint)).with_state(store);
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, handle)
}

#[tokio::test]
#[cfg(unix)]
async fn rollback_journal_symlink_is_rejected_before_database_open() {
    let d = TestDir::new();
    let outside = d.0.join("protected-fixture");
    fs::write(&outside, b"untouched synthetic marker").unwrap();
    let state = d.0.join("collector");
    secure_dir(&state).unwrap();
    std::os::unix::fs::symlink(&outside, state.join("outbox.sqlite3-journal")).unwrap();
    assert!(local_store(state.clone()).await.is_err());
    assert!(!state.join("outbox.sqlite3").exists());
    assert_eq!(fs::read(&outside).unwrap(), b"untouched synthetic marker");
    fs::remove_file(&outside).unwrap();
    assert!(local_store(state.clone()).await.is_err(), "dangling sidecar links must also be rejected");
    assert!(!state.join("outbox.sqlite3").exists());
}
#[tokio::test]
#[cfg(unix)]
async fn rollback_journal_permissions_survive_commit_unlink() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TestDir::new();
    let store = local_store(dir.0.clone()).await.unwrap();
    let journal = dir.0.join("outbox.sqlite3-journal");
    store
        .call(move |conn| {
            conn.execute_batch("CREATE TABLE journal_permission_fixture(value INTEGER)")?;
            let tx = conn.transaction()?;
            tx.execute("INSERT INTO journal_permission_fixture VALUES(1)", [])?;
            let file = outbox_sidecar(&journal)?.expect("active DELETE transaction has a rollback journal");
            // Newly created journals inherit the protected database permissions.
            assert_eq!(file.metadata()?.permissions().mode() & 0o777, 0o600);
            tx.commit()?;
            assert!(!journal.exists());
            // This is the exact lifecycle that made path-based chmod fail with
            // ENOENT. The checked descriptor can still be protected after unlink.
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            assert!(outbox_sidecar(&journal)?.is_none());
            let rows: u64 = conn.query_row("SELECT COUNT(*) FROM journal_permission_fixture", [], |r| r.get(0))?;
            assert_eq!(rows, 1);
            Ok(())
        })
        .await
        .unwrap();
    store.shutdown().await.unwrap();
}
#[tokio::test]
async fn server_binds_identity_strips_claims_and_rotates() {
    let dir = TestDir::new();
    let store = dir.store();
    let enrollment = enroll(&store, "machine-a".into()).await.unwrap();
    let token = enrollment["credential"].as_str().unwrap();
    let id = enrollment["collector"]["id"].as_str().unwrap().to_string();
    let mut o = observation("resp-a");
    o.origin_id = "collector:impersonation".into();
    o.account_id = Some("account-spoof".into());
    o.auth_type = Some("api-key".into());
    o.logical_request_id = Some("trusted-logical-spoof".into());
    o.attempt_id = Some("attempt-spoof".into());
    o.client_id = Some("client-spoof".into());
    let response = ingest(&store, headers(token), batch(vec![o])).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body(response).await["durable"], true);
    let stored = store
        .call(|c| Ok(c.query_row("SELECT payload FROM usage_observations LIMIT 1", [], |r| r.get::<_, String>(0))?))
        .await
        .unwrap();
    let stored: Observation = serde_json::from_str(&stored).unwrap();
    assert_eq!(stored.origin_id, format!("collector:{id}"));
    assert!(stored.account_id.is_none());
    assert!(stored.client_id.is_none());
    assert!(stored.attempt_id.is_none());
    let rotated = rotate(&store, id.clone()).await.unwrap();
    assert_eq!(ingest(&store, headers(token), batch(vec![])).await.status(), StatusCode::UNAUTHORIZED);
    let new_token = rotated["credential"].as_str().unwrap();
    assert_eq!(ingest(&store, headers(new_token), batch(vec![])).await.status(), StatusCode::OK);
    revoke(&store, id).await.unwrap();
    assert_eq!(ingest(&store, headers(new_token), batch(vec![])).await.status(), StatusCode::UNAUTHORIZED);
    let server_rows = store
        .call(|c| {
            Ok(c.query_row("SELECT GROUP_CONCAT(credential_hash) FROM usage_collectors", [], |r| {
                r.get::<_, String>(0)
            })?)
        })
        .await
        .unwrap();
    assert!(!server_rows.contains(token));
    assert!(!server_rows.contains(new_token));
}
#[tokio::test]
async fn strict_ingestion_bounds_privacy_and_custom_provider() {
    let dir = TestDir::new();
    let store = dir.store();
    let e = enroll(&store, "machine".into()).await.unwrap();
    let token = e["credential"].as_str().unwrap();
    let mut proxy = observation("spoof");
    proxy.source = "proxy".into();
    assert_eq!(ingest(&store, headers(token), batch(vec![proxy])).await.status(), StatusCode::BAD_REQUEST);
    let mut raw: Value = serde_json::from_slice(&batch(vec![observation("r")])).unwrap();
    raw["observations"][0]["prompt"] = json!("PRIVATE-PROMPT-MARKER");
    assert_eq!(
        ingest(&store, headers(token), Bytes::from(serde_json::to_vec(&raw).unwrap())).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        ingest(&store, headers(token), Bytes::from(vec![0; MAX_BODY + 1])).await.status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(
        ingest(&store, headers(token), batch((0..201).map(|i| observation(&format!("r{i}"))).collect())).await.status(),
        StatusCode::BAD_REQUEST
    );
    let mut o = observation("custom");
    o.provider = "fusebox".into();
    assert_eq!(ingest(&store, headers(token), batch(vec![o])).await.status(), StatusCode::OK);
    let payload = store
        .call(|c| Ok(c.query_row("SELECT payload FROM usage_observations LIMIT 1", [], |r| r.get::<_, String>(0))?))
        .await
        .unwrap();
    assert!(!payload.contains("PRIVATE-PROMPT-MARKER"));
    assert!(payload.contains("fusebox"));
}
#[tokio::test]
async fn two_collectors_durable_offline_restart_and_replay() {
    let server_dir = TestDir::new();
    let server_store = server_dir.store();
    let first = enroll(&server_store, "machine-a".into()).await.unwrap();
    let second = enroll(&server_store, "machine-b".into()).await.unwrap();
    let (url, handle) = server(server_store.clone()).await;
    let a = TestDir::new();
    let b = TestDir::new();
    let stores = [local_store(a.0.clone()).await.unwrap(), local_store(b.0.clone()).await.unwrap()];
    let enrollments = [first, second];
    for (i, store) in stores.iter().enumerate() {
        let o = observation(&format!("machine-{i}-response"));
        let payload = serde_json::to_string(&o).unwrap();
        store
            .call(move |c| {
                c.execute(
                    "INSERT INTO usage_collector_outbox(event_key,payload,created_at_ms) VALUES(?1,?2,?3)",
                    params![o.source_event_id, payload, o.event_at_ms],
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }
    let states: Vec<_> = enrollments
        .iter()
        .map(|e| LocalState {
            version: 1,
            id: Uuid::new_v4().to_string(),
            destination: url.clone(),
            credential: e["credential"].as_str().unwrap().into(),
        })
        .collect();
    let mut offline = states[0].clone();
    offline.destination = "http://127.0.0.1:1/api/usage-ingest".into();
    assert!(synchronize(&stores[0], &offline).await.is_err());
    assert_eq!(local_status(&stores[0], &states[0]).await.unwrap()["collector"]["outbox"]["pending"], 1);
    let reopened = local_store(a.0.clone()).await.unwrap();
    synchronize(&reopened, &states[0]).await.unwrap();
    synchronize(&stores[1], &states[1]).await.unwrap();
    for state in &states {
        synchronize(&reopened, state).await.unwrap();
    }
    let rows = server_store
        .call(|c| {
            Ok(c.query_row("SELECT COUNT(DISTINCT origin_id) FROM usage_observations", [], |r| r.get::<_, u64>(0))?)
        })
        .await
        .unwrap();
    assert_eq!(rows, 2);
    assert_eq!(local_status(&reopened, &states[0]).await.unwrap()["collector"]["outbox"]["pending"], 0);
    let collectors = status(&server_store).await.unwrap();
    assert_eq!(collectors["collectors"].as_array().unwrap().len(), 2);
    for collector in collectors["collectors"].as_array().unwrap() {
        let progress = collector["progress"].as_array().unwrap();
        assert_eq!(progress.len(), 2);
        assert!(progress.iter().all(|p| p["state"] == "unknown" && p["last_scan_at_ms"].is_null()));
    }
    assert!(collectors["collectors"].as_array().unwrap().iter().all(|c| c["last_sync_at_ms"].is_number()));
    handle.abort();
}
#[tokio::test]
async fn acknowledgement_does_not_delete_new_unsent_revision() {
    #[derive(Clone)]
    struct BlockingAck {
        seen: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        id: String,
    }
    async fn ack(State(state): State<BlockingAck>, Json(batch): Json<Batch>) -> Json<Value> {
        state.seen.notify_one();
        state.release.notified().await;
        Json(json!({"version":1,"durable":true,"collector_id":state.id,"acknowledged":batch.observations.len()}))
    }
    let dir = TestDir::new();
    let store = local_store(dir.0.clone()).await.unwrap();
    let o = observation("revision");
    let payload = serde_json::to_string(&o).unwrap();
    store
        .call(move |c| {
            c.execute(
                "INSERT INTO usage_collector_outbox(event_key,payload,created_at_ms) VALUES('revision',?1,0)",
                [payload],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let blocking = BlockingAck {
        seen: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
        id: Uuid::new_v4().to_string(),
    };
    let app = Router::new().route("/api/usage-ingest", post(ack)).with_state(blocking.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let state = LocalState {
        version: 1,
        id: Uuid::new_v4().to_string(),
        destination: format!("http://{address}/api/usage-ingest"),
        credential: credential(),
    };
    let cloned_store = store.clone();
    let task = tokio::spawn(async move { synchronize(&cloned_store, &state).await });
    blocking.seen.notified().await;
    let mut revised = observation("revision");
    revised.tokens.output = Some(5);
    let payload = serde_json::to_string(&revised).unwrap();
    store
        .call(move |c| {
            c.execute("UPDATE usage_collector_outbox SET payload=?1 WHERE event_key='revision'", [payload])?;
            Ok(())
        })
        .await
        .unwrap();
    blocking.release.notify_one();
    task.await.unwrap().unwrap();
    let queued = store
        .call(|c| {
            Ok(c.query_row("SELECT payload FROM usage_collector_outbox WHERE event_key='revision'", [], |r| {
                r.get::<_, String>(0)
            })?)
        })
        .await
        .unwrap();
    let queued: Observation = serde_json::from_str(&queued).unwrap();
    assert_eq!(queued.tokens.output, Some(5));
    server.abort();
}
#[tokio::test]
async fn collector_supersession_cannot_suppress_another_identity() {
    let dir = TestDir::new();
    let store = dir.store();
    let a = enroll(&store, "a".into()).await.unwrap();
    let b = enroll(&store, "b".into()).await.unwrap();
    let id = format!("counter:{}", "a".repeat(64));
    let mut o = observation("legacy");
    o.response_id = None;
    o.source_event_id = id.clone();
    assert_eq!(
        ingest(&store, headers(a["credential"].as_str().unwrap()), batch(vec![o.clone()])).await.status(),
        StatusCode::OK
    );
    let claim = Batch {
        version: 1,
        observations: vec![],
        pending: 1,
        superseded: vec![CounterSupersession { source_event_id: id.clone() }],
        progress: Vec::new(),
    };
    assert_eq!(
        ingest(&store, headers(b["credential"].as_str().unwrap()), Bytes::from(serde_json::to_vec(&claim).unwrap()))
            .await
            .status(),
        StatusCode::OK
    );
    let count = store
        .call(|c| Ok(c.query_row("SELECT COUNT(*) FROM usage_entries", [], |r| r.get::<_, u64>(0))?))
        .await
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(
        ingest(&store, headers(a["credential"].as_str().unwrap()), Bytes::from(serde_json::to_vec(&claim).unwrap()))
            .await
            .status(),
        StatusCode::OK
    );
    let count = store
        .call(|c| Ok(c.query_row("SELECT COUNT(*) FROM usage_entries", [], |r| r.get::<_, u64>(0))?))
        .await
        .unwrap();
    assert_eq!(count, 0);
}
#[test]
fn transport_and_protected_credentials() {
    assert!(validate_destination("http://example.com/api/usage-ingest").is_err());
    assert!(validate_destination("https://user:secret@example.com/api/usage-ingest").is_err());
    assert!(validate_destination("https://example.com/api/usage-ingest?key=secret").is_err());
    assert!(validate_destination("http://127.0.0.1/api/usage-ingest").is_ok());
    assert!(validate_destination("http://[::1]/api/usage-ingest").is_ok());
    let dir = TestDir::new();
    let state = LocalState {
        version: 1,
        id: Uuid::new_v4().to_string(),
        destination: "https://example.com/api/usage-ingest".into(),
        credential: credential(),
    };
    atomic_state(&dir.0.join("collector.json"), &state).unwrap();
    assert_eq!(local_state(&dir.0).unwrap().id, state.id);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(dir.0.join("collector.json")).unwrap().permissions().mode() & 0o777, 0o600);
        fs::set_permissions(dir.0.join("collector.json"), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(local_state(&dir.0).is_err());
    }
}

#[tokio::test]
async fn standalone_commands_close_writer_even_when_offline() {
    let dir = TestDir::new();
    secure_dir(&dir.0).unwrap();
    let state = LocalState {
        version: 1,
        id: Uuid::new_v4().to_string(),
        destination: "http://127.0.0.1:0/api/usage-ingest".into(),
        credential: credential(),
    };
    atomic_state(&dir.0.join("collector.json"), &state).unwrap();
    for command in [
        CollectorCommand::Status { state_dir: dir.0.clone() },
        CollectorCommand::Sync { state_dir: dir.0.clone() },
        CollectorCommand::Run { state_dir: dir.0.clone(), once: true },
    ] {
        let succeeds = matches!(&command, CollectorCommand::Status { .. });
        assert_eq!(run_command(command).await.is_ok(), succeeds);
        let reopened = local_store(dir.0.clone()).await.unwrap();
        assert_eq!(reopened.health()["prior_unclosed_sessions"], 0);
        reopened.shutdown().await.unwrap();
    }
}

fn progress() -> Value {
    json!({"source":"codex","state":"ready","imported":7,"duplicate":2,"skipped":3,"unsupported":4,"failed":1,"last_scan_at_ms":chrono::Utc::now().timestamp_millis()})
}

#[test]
fn progress_aggregation_is_allowlisted_and_never_invents_scan_evidence() {
    let now = chrono::Utc::now().timestamp_millis();
    let mut status = json!({"imports":[{"source":"codex","root":"PRIVATE-ROOT-PROJECT-MARKER","enabled":true,"last_error":null,"last_scan_at_ms":null,"imported":5,"duplicate":1,"skipped":2,"unsupported":3,"failed":0}],"candidates":[{"root":"PRIVATE-HOME-MARKER"}]});
    let report = source_progress(&status).unwrap();
    assert_eq!(report[0].state, ProgressState::Unknown);
    assert_eq!(report[1].state, ProgressState::Unknown);
    status["imports"][0]["last_scan_at_ms"] = json!(now);
    assert_eq!(source_progress(&status).unwrap()[0].state, ProgressState::Ready);
    status["imports"].as_array_mut().unwrap().push(json!({"source":"codex","root":"PRIVATE-SECOND-ROOT","enabled":false,"last_error":null,"last_scan_at_ms":now,"imported":7,"duplicate":2,"skipped":3,"unsupported":4,"failed":1}));
    let report = source_progress(&status).unwrap();
    assert_eq!(report[0].imported, 12);
    assert_eq!(report[0].duplicate, 3);
    let wire = serde_json::to_string(&Batch {
        version: 1,
        observations: vec![],
        pending: 0,
        superseded: vec![],
        progress: report,
    })
    .unwrap();
    assert!(!wire.contains("PRIVATE"));
    assert!(!wire.contains("root"));
    assert!(!wire.contains("total"));
    status["imports"][0]["last_error"] = json!("scan unavailable");
    assert_eq!(source_progress(&status).unwrap()[0].state, ProgressState::Attention);
    status["imports"][0]["enabled"] = json!(false);
    assert_eq!(source_progress(&status).unwrap()[0].state, ProgressState::Disabled);
    let legacy: Batch = serde_json::from_value(json!({"version":1,"observations":[]})).unwrap();
    assert!(legacy.progress.is_empty());
}

#[tokio::test]
async fn progress_bounds_privacy_and_atomic_roundtrip() {
    let dir = TestDir::new();
    let store = dir.store();
    let enrolled = enroll(&store, "progress-test".into()).await.unwrap();
    let token = enrolled["credential"].as_str().unwrap();
    let mut good = json!({"version":1,"observations":[],"progress":[progress()]});
    assert_eq!(
        ingest(&store, headers(token), Bytes::from(serde_json::to_vec(&good).unwrap())).await.status(),
        StatusCode::OK
    );
    let saved = status(&store).await.unwrap()["collectors"][0]["progress"].clone();
    assert_eq!(saved, good["progress"]);
    for (field, value) in [
        ("source", json!("proxy")),
        ("state", json!("complete")),
        ("imported", json!(MAX_PROGRESS_COUNT + 1)),
        ("duplicate", json!(-1)),
        ("last_scan_at_ms", json!(0)),
        ("last_scan_at_ms", json!(chrono::Utc::now().timestamp_millis() + 172_800_000)),
        ("last_scan_at_ms", Value::Null),
        ("root", json!("PRIVATE-ROOT-MARKER")),
    ] {
        let mut invalid = good.clone();
        invalid["progress"][0][field] = value;
        assert_eq!(
            ingest(&store, headers(token), Bytes::from(serde_json::to_vec(&invalid).unwrap())).await.status(),
            StatusCode::BAD_REQUEST
        );
    }
    for reports in [json!([progress(), progress()]), json!([progress(), progress(), progress()])] {
        let mut invalid = good.clone();
        invalid["progress"] = reports;
        assert_eq!(
            ingest(&store, headers(token), Bytes::from(serde_json::to_vec(&invalid).unwrap())).await.status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(status(&store).await.unwrap()["collectors"][0]["progress"], saved);
    store.call(|c|{c.execute_batch("CREATE TRIGGER fail_progress BEFORE UPDATE ON usage_collector_progress BEGIN SELECT RAISE(FAIL,'test failure'); END;")?;Ok(())}).await.unwrap();
    good["observations"] = serde_json::to_value(vec![observation("progress-atomic")]).unwrap();
    good["progress"][0]["imported"] = json!(8);
    assert_eq!(
        ingest(&store, headers(token), Bytes::from(serde_json::to_vec(&good).unwrap())).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(status(&store).await.unwrap()["collectors"][0]["progress"], saved);
    let count = store
        .call(|c| Ok(c.query_row("SELECT COUNT(*) FROM usage_entries", [], |r| r.get::<_, u64>(0))?))
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn local_database_limit_and_delete_journal_preserve_pending_on_full() {
    let dir = TestDir::new();
    let store = local_store(dir.0.clone()).await.unwrap();
    let pending = serde_json::to_string(&observation("preserved-at-full")).unwrap();
    let expected = pending.clone();
    store
        .call(move |c| {
            let mode: String = c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
            let durability: u64 = c.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
            let size: u64 = c.query_row("PRAGMA page_size", [], |r| r.get(0))?;
            let pages: u64 = c.query_row("PRAGMA max_page_count", [], |r| r.get(0))?;
            assert_eq!(mode, "delete");
            assert_eq!(durability, 2);
            assert!(size * pages <= MAX_LOCAL_DATABASE_BYTES);
            c.execute(
                "INSERT INTO usage_collector_outbox(event_key,payload,created_at_ms) VALUES('preserved',?1,0)",
                [pending],
            )?;
            c.execute_batch("CREATE TABLE synthetic_capacity_padding(payload BLOB);")?;
            let error = c
                .execute("INSERT INTO synthetic_capacity_padding VALUES(zeroblob(?1))", [MAX_LOCAL_DATABASE_BYTES])
                .unwrap_err();
            assert!(matches!(error,rusqlite::Error::SqliteFailure(e,_) if e.code==rusqlite::ErrorCode::DiskFull));
            Ok(())
        })
        .await
        .unwrap();
    store.shutdown().await.unwrap();
    assert!(fs::metadata(dir.0.join("outbox.sqlite3")).unwrap().len() <= MAX_LOCAL_DATABASE_BYTES);
    assert!(!dir.0.join("outbox.sqlite3-wal").exists());
    let reopened = local_store(dir.0.clone()).await.unwrap();
    let saved = reopened
        .call(|c| {
            Ok(c.query_row("SELECT payload FROM usage_collector_outbox WHERE event_key='preserved'", [], |r| {
                r.get::<_, String>(0)
            })?)
        })
        .await
        .unwrap();
    assert_eq!(saved, expected);
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn full_outbox_preserves_checkpoint_and_restart_retries() {
    let dir = TestDir::new();
    let store = local_store(dir.0.clone()).await.unwrap();
    let logs = dir.0.join("logs");
    fs::create_dir(&logs).unwrap();
    let row = json!({"type":"assistant","uuid":"row-one","timestamp":chrono::Utc::now().to_rfc3339(),"message":{"id":"bounded-msg","model":"claude-sonnet-4-6","content":[{"text":"PRIVATE-PROMPT-MARKER"}],"usage":{"input_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":1}}});
    fs::write(logs.join("session.jsonl"), format!("{row}\n")).unwrap();
    imports::configure(&store, "claude_code", logs.to_str().unwrap(), true).await.unwrap();
    store
        .call(|c| {
            let tx = c.transaction()?;
            for i in 0..imports::OUTBOX_LIMIT {
                tx.execute(
                    "INSERT INTO usage_collector_outbox(event_key,payload,created_at_ms) VALUES(?1,'{}',0)",
                    [i.to_string()],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
        .unwrap();
    imports::scan_outbox(&store).await.unwrap();
    let checkpoints = store
        .call(|c| Ok(c.query_row("SELECT COUNT(*) FROM usage_import_checkpoints", [], |r| r.get::<_, u64>(0))?))
        .await
        .unwrap();
    assert_eq!(checkpoints, 0);
    store
        .call(|c| {
            c.execute("DELETE FROM usage_collector_outbox", [])?;
            Ok(())
        })
        .await
        .unwrap();
    let reopened = local_store(dir.0.clone()).await.unwrap();
    imports::scan_outbox(&reopened).await.unwrap();
    let pending = reopened
        .call(|c| Ok(c.query_row("SELECT COUNT(*) FROM usage_collector_outbox", [], |r| r.get::<_, u64>(0))?))
        .await
        .unwrap();
    assert_eq!(pending, 1);
    let payload = reopened
        .call(|c| Ok(c.query_row("SELECT payload FROM usage_collector_outbox", [], |r| r.get::<_, String>(0))?))
        .await
        .unwrap();
    assert!(!payload.contains("PRIVATE-PROMPT-MARKER"));
}
