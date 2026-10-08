use super::*;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

fn old_layout() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    migrate(&mut conn).unwrap();
    conn.execute_batch(
        "INSERT INTO usage_observations(source,origin_id,source_event_id,fingerprint,canonical_key,association_key,
           trust_rank,event_at_ms,ingested_at_ms,provider,completeness,known_fields,pricing_basis,catalogue_version,snapshot_json,payload)
         VALUES('proxy','local','test','fingerprint','canonical','association',0,1,1,'openai','missing',0,'unknown','test','{}','preserve');",
    )
    .unwrap();
    conn
}

fn columns(conn: &Connection) -> Vec<String> {
    conn.prepare("PRAGMA index_info(usage_event_time)")
        .unwrap()
        .query_map([], |row| row.get(2))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

#[test]
fn covering_upgrade_preserves_rows_schema_version_and_existing_query_contract() {
    let mut conn = old_layout();
    upgrade_dashboard_index(&mut conn).unwrap();
    assert_eq!(columns(&conn), ["event_at_ms", "provider", "model", "account_id", "client_id", "origin_id", "source"]);
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0)).unwrap();
    assert_eq!(version, 3);
    let payload: String = conn
        .query_row(
            "SELECT payload FROM usage_observations INDEXED BY usage_event_time WHERE event_at_ms>=0 AND event_at_ms<2",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(payload, "preserve");
    let schema_before: i64 = conn.pragma_query_value(None, "schema_version", |row| row.get(0)).unwrap();
    upgrade_dashboard_index(&mut conn).unwrap();
    let schema_after: i64 = conn.pragma_query_value(None, "schema_version", |row| row.get(0)).unwrap();
    assert_eq!(schema_before, schema_after, "an already upgraded index must not be rebuilt");
    let plan: String = conn
        .query_row(
            "EXPLAIN QUERY PLAN SELECT DISTINCT provider,model,account_id,COALESCE(client_id,CASE WHEN origin_id LIKE 'collector:%' THEN origin_id END),source FROM usage_observations INDEXED BY usage_event_time WHERE event_at_ms>=0 AND event_at_ms<2",
            [],
            |row| row.get(3),
        )
        .unwrap();
    assert!(plan.contains("COVERING INDEX usage_event_time"), "{plan}");
}

#[test]
fn failed_covering_build_restores_the_old_index_and_journal() {
    let mut conn = old_layout();
    conn.authorizer(Some(|ctx: AuthContext<'_>| match ctx.action {
        AuthAction::CreateIndex { index_name: "usage_event_time", .. } => Authorization::Deny,
        _ => Authorization::Allow,
    }));
    assert!(upgrade_dashboard_index(&mut conn).is_err());
    conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    assert_eq!(columns(&conn), ["event_at_ms"]);
    let payload: String = conn
        .query_row("SELECT payload FROM usage_observations INDEXED BY usage_event_time", [], |row| row.get(0))
        .unwrap();
    assert_eq!(payload, "preserve");
    upgrade_dashboard_index(&mut conn).unwrap();
    assert_eq!(columns(&conn).len(), 7);
}

#[test]
fn unexpected_index_shapes_are_rejected_without_replacing_them() {
    for sql in [
        "CREATE INDEX usage_event_time ON usage_observations(provider,event_at_ms)",
        "CREATE INDEX usage_event_time ON usage_observations(event_at_ms) WHERE source='proxy'",
    ] {
        let mut conn = old_layout();
        conn.execute_batch(&format!("DROP INDEX usage_event_time; {sql};")).unwrap();
        let before: String = conn
            .query_row("SELECT sql FROM sqlite_master WHERE name='usage_event_time'", [], |row| row.get(0))
            .unwrap();
        assert!(upgrade_dashboard_index(&mut conn).is_err());
        let after: String = conn
            .query_row("SELECT sql FROM sqlite_master WHERE name='usage_event_time'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(before, after);
    }
}

#[tokio::test]
async fn only_server_startup_upgrades_the_index_while_collectors_keep_their_compact_layout() {
    let dir = std::env::temp_dir().join(format!("fusebox-index-layout-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let collector_path = dir.join("outbox.sqlite3");
    for _ in 0..2 {
        let collector = Store::open_collector(&collector_path, 512 * 1024).unwrap();
        assert_eq!(collector.call(|conn| Ok(columns(conn))).await.unwrap(), ["event_at_ms"]);
        collector.shutdown().await.unwrap();
    }
    let server = Store::open(&dir.join("server.sqlite3"), 8, 90, None).unwrap();
    assert_eq!(server.call(|conn| Ok(columns(conn).len())).await.unwrap(), 7);
    server.shutdown().await.unwrap();
    drop(server);
    // Windows keeps the file busy until the last reader thread lets go.
    let _ = std::fs::remove_dir_all(dir);
}
