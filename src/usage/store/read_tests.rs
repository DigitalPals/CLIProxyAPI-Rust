use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::oneshot;

const LONG_SQL: &str =
    "WITH RECURSIVE x(v) AS (VALUES(0) UNION ALL SELECT v+1 FROM x WHERE v<1000000000) SELECT sum(v) FROM x";
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);

fn store() -> Store {
    let path = std::env::temp_dir().join(format!("fusebox-reader-{}.sqlite", uuid::Uuid::new_v4()));
    Store::open(&path, 8, 90, None).unwrap()
}

fn budget(execution: Duration) -> Budget {
    Budget { queue: Duration::from_secs(2), execution }
}

async fn permits(store: &Store, expected: usize) {
    tokio::time::timeout(CLEANUP_TIMEOUT, async {
        while store.read_slots.available_permits() != expected {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("reader permits did not recover promptly");
}

async fn recovered(store: &Store) {
    permits(store, 4).await;
    let value: i64 =
        run_with_budget(store, budget(CLEANUP_TIMEOUT), |conn| Ok(conn.query_row("SELECT 42", [], |row| row.get(0))?))
            .await
            .unwrap();
    assert_eq!(value, 42);
    let health = store.health();
    assert_eq!(health["state"], "healthy");
    assert_eq!(health["writer_errors"], 0);
    assert_eq!(health["dropped"], 0);
    assert!(health["message"].is_null());
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn execution_deadline_interrupts_sql_and_preserves_writer_health() {
    let store = store();
    let (finished, sql_result) = oneshot::channel();
    let error = run_with_budget(&store, budget(Duration::from_millis(250)), move |conn| {
        let result = conn.query_row::<i64, _, _>(LONG_SQL, [], |row| row.get(0));
        let _ = finished.send(result.as_ref().err().and_then(rusqlite::Error::sqlite_error_code));
        Ok(result?)
    })
    .await
    .unwrap_err();
    assert_eq!(error.downcast_ref::<ReadError>(), Some(&ReadError::ExecutionTimeout));
    assert_eq!(
        tokio::time::timeout(CLEANUP_TIMEOUT, sql_result).await.unwrap().unwrap(),
        Some(ErrorCode::OperationInterrupted)
    );
    recovered(&store).await;
}

#[tokio::test]
async fn execution_deadline_stops_sqlite_lock_wait_without_degrading_health() {
    // A rollback-journal collector permits an exclusive lock, unlike a WAL
    // writer. Keep it held until the managed reader actually stops waiting.
    let path = std::env::temp_dir().join(format!("fusebox-reader-lock-{}.sqlite", uuid::Uuid::new_v4()));
    let store = Store::open_collector(&path, 1024 * 1024).unwrap();
    let locked = Connection::open(&path).unwrap();
    locked.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let (finished, sql_result) = oneshot::channel();
    let error = run_with_budget(&store, budget(Duration::from_millis(250)), move |conn| {
        let result = conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM usage_meta", [], |row| row.get(0));
        let _ = finished.send(result.as_ref().err().and_then(rusqlite::Error::sqlite_error_code));
        Ok(result?)
    })
    .await
    .unwrap_err();
    assert_eq!(error.downcast_ref::<ReadError>(), Some(&ReadError::ExecutionTimeout));
    assert_eq!(
        tokio::time::timeout(CLEANUP_TIMEOUT, sql_result).await.unwrap().unwrap(),
        Some(ErrorCode::DatabaseBusy)
    );
    permits(&store, 4).await;
    locked.execute_batch("ROLLBACK").unwrap();
    recovered(&store).await;
}

#[tokio::test]
async fn dropping_caller_interrupts_running_sql_and_recovers_its_slot() {
    let store = store();
    let reader = store.clone();
    let (started, running) = oneshot::channel();
    let (finished, sql_result) = oneshot::channel();
    let caller = tokio::spawn(async move {
        run_with_budget(&reader, budget(Duration::from_secs(10)), move |conn| {
            let _ = started.send(());
            let result = conn.query_row::<i64, _, _>(LONG_SQL, [], |row| row.get(0));
            let _ = finished.send(result.as_ref().err().and_then(rusqlite::Error::sqlite_error_code));
            Ok(result?)
        })
        .await
    });
    tokio::time::timeout(CLEANUP_TIMEOUT, running).await.unwrap().unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    assert_eq!(
        tokio::time::timeout(CLEANUP_TIMEOUT, sql_result).await.unwrap().unwrap(),
        Some(ErrorCode::OperationInterrupted)
    );
    recovered(&store).await;
}

#[tokio::test]
async fn queue_deadline_never_runs_rejected_work_or_degrades_health() {
    let store = store();
    let held = store.read_slots.clone().acquire_many_owned(4).await.unwrap();
    let invoked = Arc::new(AtomicBool::new(false));
    let in_worker = invoked.clone();
    let error =
        run_with_budget(&store, Budget { queue: Duration::from_millis(50), execution: CLEANUP_TIMEOUT }, move |_| {
            in_worker.store(true, Ordering::Relaxed);
            Ok(())
        })
        .await
        .unwrap_err();
    assert_eq!(error.downcast_ref::<ReadError>(), Some(&ReadError::QueueTimeout));
    assert!(!invoked.load(Ordering::Relaxed));
    assert_eq!(store.read_slots.available_permits(), 0);
    drop(held);
    recovered(&store).await;
}

#[tokio::test]
async fn execution_deadline_stops_cooperative_rust_aggregation() {
    let store = store();
    let (finished, rust_result) = oneshot::channel();
    let error = run_with_budget(&store, budget(Duration::from_millis(250)), move |_| {
        loop {
            if let Err(error) = check() {
                let _ = finished.send(error.downcast_ref::<ReadError>().copied());
                break Err::<(), _>(error);
            }
            std::hint::spin_loop();
        }
    })
    .await
    .unwrap_err();
    assert_eq!(error.downcast_ref::<ReadError>(), Some(&ReadError::ExecutionTimeout));
    assert_eq!(
        tokio::time::timeout(CLEANUP_TIMEOUT, rust_result).await.unwrap().unwrap(),
        Some(ReadError::ExecutionTimeout)
    );
    recovered(&store).await;
}

#[test]
fn blocking_pool_queue_does_not_retain_cancelled_or_expired_reader_slots() {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().max_blocking_threads(1).build().unwrap();
    runtime.block_on(async {
        let store = store();
        let (release, blocked) = std::sync::mpsc::channel();
        let (started, running) = oneshot::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            let _ = started.send(());
            let _ = blocked.recv();
        });
        running.await.unwrap();
        let invoked = Arc::new(AtomicBool::new(false));
        let in_worker = invoked.clone();
        let reader = store.clone();
        let caller = tokio::spawn(async move {
            run_with_budget(&reader, budget(Duration::from_secs(10)), move |_| {
                in_worker.store(true, Ordering::Relaxed);
                Ok(())
            })
            .await
        });
        permits(&store, 3).await;
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        permits(&store, 4).await;
        assert!(!invoked.load(Ordering::Relaxed));

        // The execution budget includes waiting for a blocking-pool thread.
        let in_worker = invoked.clone();
        let error = run_with_budget(&store, budget(Duration::from_millis(50)), move |_| {
            in_worker.store(true, Ordering::Relaxed);
            Ok(())
        })
        .await
        .unwrap_err();
        assert_eq!(error.downcast_ref::<ReadError>(), Some(&ReadError::ExecutionTimeout));
        permits(&store, 4).await;
        assert!(!invoked.load(Ordering::Relaxed));

        release.send(()).unwrap();
        blocker.await.unwrap();
        recovered(&store).await;
        assert!(!invoked.load(Ordering::Relaxed));
    });
}
