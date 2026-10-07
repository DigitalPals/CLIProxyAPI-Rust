//! Bounded independent reads: dropping a caller cancels its SQLite work, not just
//! the response future. Rust aggregation loops cooperate through `check()`.
use super::{Store, database_failure};
use anyhow::{Context, Result};
use rusqlite::{Connection, ErrorCode, InterruptHandle};
use std::{
    cell::RefCell,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{sync::OwnedSemaphorePermit, task::AbortHandle};

const QUEUE_TIMEOUT: Duration = Duration::from_secs(2);
const EXECUTION_TIMEOUT: Duration = Duration::from_secs(10);
const CANCELLED: u8 = 1;
const EXPIRED: u8 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReadError {
    QueueTimeout,
    ExecutionTimeout,
    Cancelled,
}
impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::QueueTimeout => "Usage query queue deadline exceeded",
            Self::ExecutionTimeout => "Usage query execution deadline exceeded",
            Self::Cancelled => "Usage query cancelled",
        })
    }
}
impl std::error::Error for ReadError {}

#[derive(Clone, Copy)]
struct Budget {
    queue: Duration,
    execution: Duration,
}

struct Control {
    stopped: AtomicU8,
    deadline: Instant,
    interrupt: parking_lot::Mutex<Option<InterruptHandle>>,
    // A job can be queued in Tokio's blocking pool after admission. Keep its
    // permit here until it actually starts, so cancellation releases that slot
    // immediately even if all blocking threads are occupied.
    pending_permit: parking_lot::Mutex<Option<OwnedSemaphorePermit>>,
}
impl Control {
    fn check(&self) -> std::result::Result<(), ReadError> {
        match self.stopped.load(Ordering::Acquire) {
            CANCELLED => Err(ReadError::Cancelled),
            EXPIRED => Err(ReadError::ExecutionTimeout),
            _ if Instant::now() >= self.deadline => Err(ReadError::ExecutionTimeout),
            _ => Ok(()),
        }
    }
    fn stop(&self, reason: u8) {
        let _ = self.stopped.compare_exchange(0, reason, Ordering::AcqRel, Ordering::Acquire);
        self.pending_permit.lock().take();
        if let Some(handle) = self.interrupt.lock().as_ref() {
            handle.interrupt();
        }
    }
}

struct CancelOnDrop {
    control: Arc<Control>,
    abort: Option<AbortHandle>,
    complete: bool,
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if !self.complete {
            self.control.stop(CANCELLED);
            if let Some(abort) = &self.abort {
                // This prevents a queued blocking job from starting. Running
                // jobs instead stop through SQLite interruption/cooperative checks.
                abort.abort();
            }
        }
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<Control>>> = const { RefCell::new(None) };
}
struct Scope(Option<Arc<Control>>);
impl Scope {
    fn enter(control: Arc<Control>) -> Self {
        Self(CURRENT.with(|current| current.replace(Some(control))))
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.with(|current| current.replace(self.0.take()));
    }
}

/// Direct/reference calculations outside a managed reader have no deadline.
pub(super) fn check() -> Result<()> {
    CURRENT.with(|current| {
        if let Some(control) = current.borrow().as_ref() {
            control.check()?;
        }
        Ok(())
    })
}

// SQLite's default busy timeout can otherwise retain a cancelled read for five
// seconds without calling the progress handler. Check cancellation while waiting.
fn busy(attempt: i32) -> bool {
    if attempt >= 500 || check().is_err() {
        return false;
    }
    std::thread::sleep(Duration::from_millis(10));
    check().is_ok()
}

pub(super) async fn run<F, R>(store: &Store, f: F) -> Result<R>
where
    F: FnOnce(&mut Connection) -> Result<R> + Send + 'static,
    R: Send + 'static,
{
    run_with_budget(store, Budget { queue: QUEUE_TIMEOUT, execution: EXECUTION_TIMEOUT }, f).await
}

async fn run_with_budget<F, R>(store: &Store, budget: Budget, f: F) -> Result<R>
where
    F: FnOnce(&mut Connection) -> Result<R> + Send + 'static,
    R: Send + 'static,
{
    let permit = tokio::time::timeout(budget.queue, store.read_slots.clone().acquire_owned())
        .await
        .map_err(|_| ReadError::QueueTimeout)?
        .context("usage readers closed")?;
    let control = Arc::new(Control {
        stopped: AtomicU8::new(0),
        deadline: Instant::now() + budget.execution,
        interrupt: parking_lot::Mutex::new(None),
        pending_permit: parking_lot::Mutex::new(Some(permit)),
    });
    let mut cancellation = CancelOnDrop { control: control.clone(), abort: None, complete: false };
    let path = store.path.clone();
    let health = store.health.clone();
    let worker_control = control.clone();
    let worker = tokio::task::spawn_blocking(move || {
        let _permit = worker_control.pending_permit.lock().take();
        worker_control.check()?;
        let _scope = Scope::enter(worker_control.clone());
        let result = (|| {
            let mut conn = Connection::open_with_flags(
                path.as_path(),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            *worker_control.interrupt.lock() = Some(conn.get_interrupt_handle());
            let progress = worker_control.clone();
            conn.progress_handler(1000, Some(move || progress.check().is_err()));
            conn.busy_handler(Some(busy))?;
            #[cfg(test)]
            super::register_aggregates(&conn)?;
            worker_control.check()?;
            conn.execute_batch("PRAGMA query_only=ON; BEGIN;")?;
            worker_control.check()?;
            f(&mut conn)
        })();
        // Preserve genuine storage failures. Expected cancellation/timeout may
        // surface as SQLITE_INTERRUPT or SQLITE_BUSY (inside the busy handler).
        let result = match worker_control.check() {
            Err(reason) if result.as_ref().err().is_none_or(interrupted_or_busy) => Err(reason.into()),
            _ => result,
        };
        if let Err(error) = &result
            && database_failure(error)
        {
            health.fail(error);
        }
        result
    });
    cancellation.abort = Some(worker.abort_handle());
    match tokio::time::timeout_at(tokio::time::Instant::from_std(control.deadline), worker).await {
        Ok(result) => {
            cancellation.complete = true;
            result.context("usage query worker panicked")?
        }
        Err(_) => {
            control.stop(EXPIRED);
            Err(ReadError::ExecutionTimeout.into())
        }
    }
}

fn interrupted_or_busy(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|e| e.downcast_ref::<rusqlite::Error>())
        .any(|e| matches!(e.sqlite_error_code(), Some(ErrorCode::OperationInterrupted | ErrorCode::DatabaseBusy)))
}

#[cfg(test)]
#[path = "read_tests.rs"]
mod tests;
