use super::*;
use crate::usage::store::Store;
use rusqlite::params;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

const ORIGIN: &str = "keeper:test-production";
const SCHEMA: &str = "CREATE TABLE usage_events(id INTEGER PRIMARY KEY,timestamp TEXT,provider TEXT,model TEXT,model_alias TEXT,auth_type TEXT,auth_index TEXT,executor_type TEXT,service_tier TEXT,response_service_tier TEXT,failed INTEGER,generate INTEGER,input_tokens INTEGER,output_tokens INTEGER,reasoning_tokens INTEGER,cache_read_tokens INTEGER,cache_creation_tokens INTEGER,total_tokens INTEGER,request_id TEXT,api_group_key TEXT,source TEXT,client_ip TEXT,user_agent TEXT)";
struct Fixture {
    root: PathBuf,
    source: PathBuf,
    destination: PathBuf,
}
impl Fixture {
    async fn new() -> Self {
        let root = std::env::temp_dir().join(format!("fusebox-keeper-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let fixture = Self { source: root.join("keeper.sqlite3"), destination: root.join("usage.sqlite3"), root };
        Connection::open(&fixture.source).unwrap().execute_batch(SCHEMA).unwrap();
        let store = Store::open(&fixture.destination, 1024, 90, None).unwrap();
        store.shutdown().await.unwrap();
        fixture
    }
    fn add(&self, id: i64, claude: bool) {
        let c = Connection::open(&self.source).unwrap();
        c.execute("INSERT INTO usage_events VALUES(?1,?2,?3,?4,'friendly-alias','oauth','opaque-historical-account',?5,'priority',?6,0,1,200,30,10,80,?7,230,'reused-client-request','sk-do-not-copy','private@example.invalid','192.0.2.1','private-user-agent')",params![id,(Utc::now()-chrono::Duration::days(1)).to_rfc3339(),if claude {"claude"} else {"codex"},if claude {"claude-opus-5-5"} else {"gpt-6.1-sol"},if claude {"ClaudeExecutor"} else {"CodexExecutor"},if claude {"auto"} else {"default"},if claude {30} else {0}]).unwrap();
    }
    fn change(&self, id: i64, sql: &str) {
        Connection::open(&self.source)
            .unwrap()
            .execute(&format!("UPDATE usage_events SET {sql} WHERE id=?1"), [id])
            .unwrap();
    }
    fn run(&self, apply: bool) -> Value {
        import(&self.destination, &self.source, ORIGIN, apply).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn preview_is_read_only_and_preserves_inclusive_token_semantics() {
    let f = Fixture::new().await;
    f.add(1, true);
    f.add(2, false);
    let source_before = Sha256::digest(std::fs::read(&f.source).unwrap());
    let destination_before = Sha256::digest(std::fs::read(&f.destination).unwrap());
    let plan = f.run(false);
    assert_eq!(plan["eligible"], 2);
    assert_eq!(plan["tokens"]["input"], 210);
    assert_eq!(plan["tokens"]["cache_read"], 160);
    assert_eq!(plan["tokens"]["cache_write"], 30);
    assert_eq!(plan["tokens"]["output"], 60);
    assert_eq!(plan["tokens"]["reasoning"], 20);
    assert_eq!(plan["tokens"]["total"], 460);
    assert_eq!(plan["pricing_reasons"]["unknown_cache_write_ttl"], 1);
    assert_eq!(plan["priced"], 1);
    // Applied default response tier overrides the requested priority tier.
    assert_eq!(plan["estimated_cost_nanos"], 120 * 2000 + 80 * 100 + 30 * 10000);
    assert_eq!(source_before, Sha256::digest(std::fs::read(&f.source).unwrap()));
    assert_eq!(destination_before, Sha256::digest(std::fs::read(&f.destination).unwrap()));
}

#[tokio::test]
async fn replay_is_idempotent_and_reused_request_ids_remain_distinct() {
    let f = Fixture::new().await;
    f.add(1, false);
    f.add(2, false);
    let source_before = Sha256::digest(std::fs::read(&f.source).unwrap());
    let store = Store::open_existing(&f.destination, 128).unwrap();
    let mut native = Observation::new("proxy", "native-existing".into(), "openai", Utc::now().timestamp_millis());
    native.actual_model = Some("gpt-6.1-sol".into());
    native.tokens =
        Tokens { input: Some(100), cache_read: Some(10), cache_write: Some(0), output: Some(20), ..Default::default() };
    native.completeness = "complete".into();
    store.call(move |c| store::insert_batch(c, &[native])).await.unwrap();
    store.shutdown().await.unwrap();
    let original = Connection::open(&f.destination)
        .unwrap()
        .query_row(
            "SELECT payload,snapshot_json FROM usage_observations WHERE source_event_id='native-existing'",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .unwrap();
    assert_eq!(f.run(true)["inserted"], 2);
    let again = f.run(true);
    assert_eq!(again["inserted"], 0);
    assert_eq!(again["duplicates"], 2);
    let c = Connection::open(&f.destination).unwrap();
    assert_eq!(c.query_row("SELECT count(*) FROM usage_entries", [], |r| r.get::<_, i64>(0)).unwrap(), 3);
    assert_eq!(
        original,
        c.query_row(
            "SELECT payload,snapshot_json FROM usage_observations WHERE source_event_id='native-existing'",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        )
        .unwrap()
    );
    assert_eq!(source_before, Sha256::digest(std::fs::read(&f.source).unwrap()));
    for payload in c
        .prepare("SELECT payload FROM usage_observations WHERE origin_id=?1")
        .unwrap()
        .query_map([ORIGIN], |r| r.get::<_, String>(0))
        .unwrap()
    {
        let payload = payload.unwrap();
        for excluded in [
            "sk-do-not-copy",
            "private@example.invalid",
            "192.0.2.1",
            "private-user-agent",
            "opaque-historical-account",
            "reused-client-request",
        ] {
            assert!(!payload.contains(excluded));
        }
        let o: Observation = serde_json::from_str(&payload).unwrap();
        assert!(o.account_id.unwrap().starts_with("keeper-account:"));
        assert!(o.logical_request_id.is_none());
        assert!(o.attempt_id.is_none());
    }
}

#[tokio::test]
async fn failed_missing_usage_is_unpriced_and_reported_failed_usage_remains() {
    let f = Fixture::new().await;
    f.add(1, false);
    f.add(2, false);
    f.change(1,"failed=1,input_tokens=0,output_tokens=0,reasoning_tokens=0,cache_read_tokens=0,cache_creation_tokens=0,total_tokens=0");
    f.change(2, "failed=1");
    let result = f.run(true);
    assert_eq!(result["inserted"], 2);
    assert_eq!(result["priced"], 1);
    assert_eq!(result["pricing_reasons"]["missing_tokens"], 1);
    let c = Connection::open(&f.destination).unwrap();
    let (input, cost): (Option<i64>, Option<i64>) = c
        .query_row("SELECT input,cost_nanos FROM usage_observations WHERE completeness='missing'", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((input, cost), (None, None));
    assert_eq!(result["tokens"]["total"], 230);
}

#[tokio::test]
async fn unsupported_and_invalid_records_are_reported_without_inventing_counts() {
    let f = Fixture::new().await;
    for id in 1..=7 {
        f.add(id, false);
    }
    f.change(1, "input_tokens=10");
    f.change(2, "reasoning_tokens=99");
    f.change(3, "total_tokens=999");
    f.change(4, "executor_type='UnknownExecutor'");
    f.change(5, "timestamp='untrusted invalid time'");
    f.change(6, "generate=0");
    f.change(7, "cache_read_tokens=-1");
    let result = f.run(true);
    assert_eq!(result["eligible"], 0);
    assert_eq!(result["inserted"], 0);
    assert_eq!(result["skipped"]["invalid_tokens"], 2);
    assert_eq!(result["skipped"]["invalid_observation"], 1);
    assert_eq!(result["skipped"]["inconsistent_total"], 1);
    assert_eq!(result["skipped"]["unsupported_executor"], 1);
    assert_eq!(result["skipped"]["invalid_timestamp"], 1);
    assert_eq!(result["skipped"]["not_generation"], 1);
    assert!(!result.to_string().contains("untrusted invalid time"));
}

#[tokio::test]
async fn timezone_nanosecond_timestamp_and_retention_are_respected() {
    let f = Fixture::new().await;
    f.add(1, false);
    let timestamp = (Utc::now() - chrono::Duration::days(1))
        .with_timezone(&chrono::FixedOffset::east_opt(7200).unwrap())
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, false);
    Connection::open(&f.source)
        .unwrap()
        .execute("UPDATE usage_events SET timestamp=?1 WHERE id=1", [&timestamp])
        .unwrap();
    assert_eq!(f.run(false)["first_event_at_ms"], DateTime::parse_from_rfc3339(&timestamp).unwrap().timestamp_millis());
    f.add(2, false);
    let expired = (Utc::now() - chrono::Duration::days(91)).to_rfc3339();
    Connection::open(&f.source).unwrap().execute("UPDATE usage_events SET timestamp=?1 WHERE id=2", [expired]).unwrap();
    let report = f.run(true);
    assert_eq!(report["eligible"], 1);
    assert_eq!(report["skipped"]["outside_retention"], 1);
    assert_eq!(report["purged"], 0);
}

#[tokio::test]
async fn refuses_wrong_destination_identity_missing_files_and_unstable_origin() {
    let f = Fixture::new().await;
    assert!(import(&f.source, &f.source, ORIGIN, true).is_err());
    assert!(import(&f.destination, &f.source, "local", true).is_err());
    assert!(import(&f.source, &f.destination, ORIGIN, true).is_err());
    let nonexistent = f.root.join("missing.sqlite3");
    assert!(import(&nonexistent, &f.source, ORIGIN, true).is_err());
    assert!(!nonexistent.exists());
}

#[tokio::test]
async fn overlapping_native_history_requires_reconciliation_before_any_import() {
    let f = Fixture::new().await;
    f.add(1, false);
    let time: String = Connection::open(&f.source)
        .unwrap()
        .query_row("SELECT timestamp FROM usage_events WHERE id=1", [], |r| r.get(0))
        .unwrap();
    let store = Store::open_existing(&f.destination, 128).unwrap();
    let native = Observation::new(
        "proxy",
        "overlapping-native".into(),
        "openai",
        DateTime::parse_from_rfc3339(&time).unwrap().timestamp_millis(),
    );
    store.call(move |c| store::insert_batch(c, &[native])).await.unwrap();
    store.shutdown().await.unwrap();
    assert_eq!(f.run(false)["overlapping_native_observations"], 1);
    assert!(import(&f.destination, &f.source, ORIGIN, true).unwrap_err().to_string().contains("overlaps native"));
    assert_eq!(
        Connection::open(&f.destination)
            .unwrap()
            .query_row("SELECT count(*) FROM usage_observations", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn importing_batches_keeps_concurrent_native_capture_durable() {
    let f = Fixture::new().await;
    let c = Connection::open(&f.source).unwrap();
    c.execute("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<3000) INSERT INTO usage_events(id,timestamp,provider,model,auth_type,auth_index,executor_type,service_tier,response_service_tier,failed,generate,input_tokens,output_tokens,reasoning_tokens,cache_read_tokens,cache_creation_tokens,total_tokens) SELECT i,?1,'codex','gpt-6.1-sol','oauth','historical','CodexExecutor','priority','default',0,1,200,30,10,80,0,230 FROM n",[(Utc::now()-chrono::Duration::days(1)).to_rfc3339()]).unwrap();
    drop(c);
    let store = Store::open_existing(&f.destination, 1024).unwrap();
    let destination = f.destination.clone();
    let source = f.source.clone();
    let importer = tokio::task::spawn_blocking(move || import(&destination, &source, ORIGIN, true));
    for i in 0..200 {
        let mut o =
            Observation::new("proxy", format!("concurrent-native-{i}"), "openai", Utc::now().timestamp_millis());
        o.tokens = Tokens {
            input: Some(100),
            cache_read: Some(0),
            cache_write: Some(0),
            output: Some(10),
            ..Default::default()
        };
        assert!(store.enqueue(o));
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let result = importer.await.unwrap().unwrap();
    assert_eq!(result["inserted"], 3000);
    store.flush().await.unwrap();
    for key in ["dropped", "rejected", "writer_errors"] {
        assert_eq!(store.health()[key], 0);
    }
    assert_eq!(
        store
            .call(|c| Ok(c.query_row("SELECT count(*) FROM usage_entries", [], |r| r.get::<_, i64>(0))?))
            .await
            .unwrap(),
        3200
    );
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn cache_estimate_preview_is_read_only_and_prices_both_lifetime_bounds() {
    let f = Fixture::new().await;
    f.add(1, true);
    f.add(2, false);
    f.run(true);
    let before = Sha256::digest(std::fs::read(&f.destination).unwrap());
    let result = estimate_cache(&f.destination, ORIGIN, false).unwrap();
    assert_eq!((result["estimated"].as_u64(), result["updated"].as_u64()), (Some(1), Some(0)));
    assert_eq!(result["estimated_cost_nanos"], 1_126_000);
    assert_eq!(result["cache_lifetime_min_cost_nanos"], 1_126_000);
    assert_eq!(result["cache_lifetime_max_cost_nanos"], 1_216_000);
    assert_eq!(before, Sha256::digest(std::fs::read(&f.destination).unwrap()));
    assert_eq!(estimate_cache(&f.destination, "keeper:other", false).unwrap()["estimated"], 0);
    assert!(estimate_cache(&f.destination, "local", true).is_err());
    let missing = f.root.join("missing.sqlite3");
    assert!(estimate_cache(&missing, ORIGIN, true).is_err());
    assert!(!missing.exists());
}

#[tokio::test]
async fn cache_estimate_batches_use_the_primary_range_without_resorting_history() {
    let f = Fixture::new().await;
    let c = Connection::open(&f.destination).unwrap();
    let plan = c
        .prepare(&format!("EXPLAIN QUERY PLAN {CACHE_ESTIMATE_SELECT}"))
        .unwrap()
        .query_map(params![0, ORIGIN], |r| r.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert!(plan.iter().any(|s| s.contains("INTEGER PRIMARY KEY (rowid>?)")), "{plan:?}");
    assert!(plan.iter().all(|s| !s.contains("TEMP B-TREE")), "{plan:?}");
}

#[tokio::test]
async fn cache_estimate_preserves_evidence_prices_and_indexes_and_is_idempotent() {
    let f = Fixture::new().await;
    f.add(1, true);
    f.add(2, false);
    f.add(3, true);
    f.change(3,"failed=1,input_tokens=0,output_tokens=0,reasoning_tokens=0,cache_read_tokens=0,cache_creation_tokens=0,total_tokens=0");
    f.run(true);
    let c = Connection::open(&f.destination).unwrap();
    let original: (String,String,String) = c.query_row("SELECT payload,fingerprint,snapshot_json FROM usage_observations WHERE model='claude-opus-5-5' AND cost_nanos IS NULL AND pricing_basis='unknown_cache_write_ttl'",[],|r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    let priced: (i64, String) = c
        .query_row("SELECT cost_nanos,snapshot_json FROM usage_observations WHERE cost_nanos IS NOT NULL", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    drop(c);
    let result = estimate_cache(&f.destination, ORIGIN, true).unwrap();
    assert_eq!(result["updated"], 1);
    let c = Connection::open(&f.destination).unwrap();
    let (payload,fingerprint,snapshot): (String,String,String) = c.query_row("SELECT payload,fingerprint,snapshot_json FROM usage_observations WHERE pricing_basis='historical_cache_write_estimate'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!((&payload, &fingerprint), (&original.0, &original.1));
    let snapshot: Value = serde_json::from_str(&snapshot).unwrap();
    assert_eq!(snapshot["cost_nanos"], 1_126_000);
    assert_eq!(snapshot["partial"], true);
    assert!(
        snapshot["assumptions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "historical_cache_write_lifetime_5_minutes_assumed")
    );
    assert_eq!(snapshot["historical_estimate"]["original_snapshot_sha256"], hash(original.2.as_bytes()));
    for table in ["usage_observations", "usage_entries", "usage_source_entries"] {
        let row: (i64,Option<i64>,Option<i64>) = c.query_row(&format!("SELECT cost_nanos,write_5m,write_1h FROM {table} WHERE pricing_basis='historical_cache_write_estimate'"),[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!(row, (1_126_000, None, None));
    }
    assert_eq!(
        priced,
        c.query_row("SELECT cost_nanos,snapshot_json FROM usage_observations WHERE model='gpt-6.1-sol'", [], |r| Ok((
            r.get(0)?,
            r.get(1)?
        )))
        .unwrap()
    );
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM usage_observations WHERE pricing_basis='missing_tokens' AND cost_nanos IS NULL",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    drop(c);
    assert_eq!(estimate_cache(&f.destination, ORIGIN, true).unwrap()["updated"], 0);
    let store = Store::open_existing(&f.destination, 128).unwrap();
    let records = store
        .details(crate::usage::store::Query { model: Some("claude-opus-5-5".into()), ..Default::default() })
        .await
        .unwrap();
    let priced = records["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["pricing_basis"] == "historical_cache_write_estimate")
        .unwrap();
    assert_eq!(priced["pricing_snapshot"]["historical_estimate"]["method"], CACHE_ESTIMATE_METHOD);
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn cache_estimate_rejects_incompatible_evidence_and_rolls_back_the_batch() {
    let f = Fixture::new().await;
    f.add(1, true);
    f.add(2, true);
    f.run(true);
    let c = Connection::open(&f.destination).unwrap();
    c.execute("UPDATE usage_observations SET payload=json_set(payload,'$.parser_version','unverified-parser') WHERE id=(SELECT max(id) FROM usage_observations)",[]).unwrap();
    assert!(estimate_cache(&f.destination, ORIGIN, true).is_err());
    assert_eq!(
        c.query_row("SELECT count(*) FROM usage_observations WHERE cost_nanos IS NOT NULL", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        c.query_row("SELECT count(*) FROM usage_entries WHERE cost_nanos IS NOT NULL", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn cache_estimation_keeps_concurrent_native_capture_durable() {
    let f = Fixture::new().await;
    let c = Connection::open(&f.source).unwrap();
    c.execute("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<1500) INSERT INTO usage_events(id,timestamp,provider,model,executor_type,service_tier,response_service_tier,failed,generate,input_tokens,output_tokens,reasoning_tokens,cache_read_tokens,cache_creation_tokens,total_tokens) SELECT i,?1,'claude','claude-opus-5-5','ClaudeExecutor','auto','auto',0,1,200,30,10,80,30,230 FROM n",[(Utc::now()-chrono::Duration::days(1)).to_rfc3339()]).unwrap();
    drop(c);
    f.run(true);
    let store = Store::open_existing(&f.destination, 1024).unwrap();
    let destination = f.destination.clone();
    let estimator = tokio::task::spawn_blocking(move || estimate_cache(&destination, ORIGIN, true));
    for i in 0..200 {
        let mut o =
            Observation::new("proxy", format!("estimate-concurrent-{i}"), "anthropic", Utc::now().timestamp_millis());
        o.actual_model = Some("claude-opus-5-5".into());
        o.tokens = Tokens {
            input: Some(100),
            cache_read: Some(0),
            cache_write: Some(10),
            output: Some(20),
            ..Default::default()
        };
        assert!(store.enqueue(o));
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(estimator.await.unwrap().unwrap()["updated"], 1500);
    store.flush().await.unwrap();
    for key in ["dropped", "rejected", "writer_errors"] {
        assert_eq!(store.health()[key], 0);
    }
    assert_eq!(store.call(|c|Ok(c.query_row("SELECT count(*) FROM usage_observations WHERE origin_id='local' AND cost_nanos IS NULL AND pricing_basis='unknown_cache_write_ttl'",[],|r|r.get::<_,i64>(0))?)).await.unwrap(),200);
    store.shutdown().await.unwrap();
}
