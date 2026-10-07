//! A single writer serializes assignment snapshots, coalescing request-side wakeups.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;

use chrono::Utc;
use parking_lot::Mutex;
use tokio::sync::oneshot;

use super::{Binding, Registry};

enum Command {
    Wake,
    Flush(oneshot::Sender<()>),
    Barrier(oneshot::Sender<()>),
}

pub(super) struct Persistence {
    commands: mpsc::Sender<Command>,
    pending: Arc<AtomicBool>,
    outstanding: Arc<AtomicUsize>,
}

impl Persistence {
    pub(super) fn new(registry: Arc<Mutex<Registry>>, path: PathBuf) -> Option<Self> {
        Self::start(registry, path, write_assignments)
    }

    fn start(
        registry: Arc<Mutex<Registry>>,
        path: PathBuf,
        mut write: impl FnMut(&Path, &HashMap<String, Binding>) -> anyhow::Result<()> + Send + 'static,
    ) -> Option<Self> {
        let (commands, receive) = mpsc::channel();
        let pending = Arc::new(AtomicBool::new(false));
        let queued = pending.clone();
        let outstanding = Arc::new(AtomicUsize::new(0));
        let active = outstanding.clone();
        let worker = std::thread::Builder::new().name("session-writer".into()).spawn(move || {
            // The worker owns no sender: dropping Sessions closes the channel, drains
            // queued writes, and exits the thread without a reference cycle.
            while let Ok(first) = receive.recv() {
                let mut barriers = Vec::new();
                let mut requested = 0;
                for command in std::iter::once(first).chain(receive.try_iter().take(1023)) {
                    match command {
                        Command::Wake => {
                            queued.store(false, Ordering::Release);
                            requested += 1;
                        }
                        Command::Flush(barrier) => {
                            requested += 1;
                            barriers.push(barrier);
                        }
                        Command::Barrier(barrier) => barriers.push(barrier),
                    }
                }
                let snapshot = {
                    let mut registry = registry.lock();
                    if requested > 0 && std::mem::take(&mut registry.dirty) {
                        registry.last_flush = Utc::now().timestamp();
                        Some(registry.bindings.clone())
                    } else {
                        None
                    }
                };
                if let Some(bindings) = snapshot
                    && let Err(e) = write(&path, &bindings)
                {
                    // Keep failed snapshots eligible for the next periodic/manual
                    // flush, including changes made while this write was in flight.
                    registry.lock().dirty = true;
                    tracing::warn!(path = %path.display(), "session assignments could not be persisted: {e}");
                }
                active.fetch_sub(requested, Ordering::AcqRel);
                for barrier in barriers {
                    let _ = barrier.send(());
                }
            }
        });
        match worker {
            Ok(_) => Some(Self { commands, pending, outstanding }),
            Err(e) => {
                tracing::warn!("session persistence worker could not start: {e}");
                None
            }
        }
    }

    pub(super) fn schedule(&self) {
        if !self.pending.swap(true, Ordering::AcqRel) {
            self.outstanding.fetch_add(1, Ordering::AcqRel);
            if self.commands.send(Command::Wake).is_err() {
                self.pending.store(false, Ordering::Release);
                self.outstanding.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    pub(super) fn flush(&self) -> oneshot::Receiver<()> {
        let (send, receive) = oneshot::channel();
        self.outstanding.fetch_add(1, Ordering::AcqRel);
        if self.commands.send(Command::Flush(send)).is_err() {
            self.outstanding.fetch_sub(1, Ordering::AcqRel);
        }
        receive
    }

    /// A reused assignment may still have its initial write in flight. Waiting
    /// for that write must not turn a timestamp touch into another disk write.
    pub(super) fn pending_write(&self) -> Option<oneshot::Receiver<()>> {
        if self.outstanding.load(Ordering::Acquire) == 0 {
            return None;
        }
        let (send, receive) = oneshot::channel();
        let _ = self.commands.send(Command::Barrier(send));
        Some(receive)
    }
}

fn write_assignments(path: &Path, bindings: &HashMap<String, Binding>) -> anyhow::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> anyhow::Result<()> {
        let mut file = options.open(&temporary)?;
        file.write_all(&serde_json::to_vec(bindings)?)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        #[cfg(unix)]
        if let Some(parent) = path.parent() {
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn slow_write_releases_registry_and_flush_waits_for_latest_coalesced_snapshot() {
        let registry = Arc::new(Mutex::new(Registry::default()));
        let (entered, mut started) = tokio::sync::mpsc::unbounded_channel();
        let (release, gated) = mpsc::channel();
        let writes = Arc::new(Mutex::new(Vec::new()));
        let captured = writes.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let worker = Persistence::start(registry.clone(), PathBuf::from("unused"), move |_, bindings| {
            if count.fetch_add(1, Ordering::SeqCst) == 0 {
                entered.send(()).unwrap();
                gated.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            captured.lock().push(bindings["task"].account.clone());
            Ok(())
        })
        .unwrap();
        {
            let mut state = registry.lock();
            state
                .bindings
                .insert("task".into(), Binding { session: "owner".into(), account: "first".into(), last_seen: 0 });
            state.dirty = true;
        }
        worker.schedule();
        tokio::time::timeout(Duration::from_secs(2), started.recv()).await.unwrap().unwrap();
        // The persistence callback is blocked, but request-side selection can lock
        // and update the registry. Many wakeups collapse into one latest snapshot.
        for i in 0..100 {
            let mut state = registry.try_lock().expect("disk write must not hold the registry mutex");
            state.bindings.get_mut("task").unwrap().account = format!("replacement-{i}");
            state.dirty = true;
            drop(state);
            worker.schedule();
        }
        let mut barrier = worker.flush();
        assert!(matches!(barrier.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), barrier).await.unwrap().unwrap();
        assert_eq!(*writes.lock(), ["first", "replacement-99"]);
        assert!(!registry.lock().dirty);
        let weak = Arc::downgrade(&registry);
        drop(registry);
        drop(worker);
        // A sender is not retained by the worker, so its registry ownership ends.
        tokio::time::timeout(Duration::from_secs(2), async {
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn failed_write_remains_dirty_and_can_be_retried() {
        let registry = Arc::new(Mutex::new(Registry { dirty: true, ..Default::default() }));
        let mut attempts = 0;
        let worker = Persistence::start(registry.clone(), PathBuf::from("unused"), move |_, _| {
            attempts += 1;
            anyhow::ensure!(attempts > 1, "injected write failure");
            Ok(())
        })
        .unwrap();
        worker.flush().await.unwrap();
        assert!(registry.lock().dirty);
        worker.flush().await.unwrap();
        assert!(!registry.lock().dirty);
    }

    #[tokio::test]
    async fn reusing_a_pending_assignment_waits_without_flushing_activity_timestamps() {
        let registry = Arc::new(Mutex::new(Registry { dirty: true, ..Default::default() }));
        let (entered, started) = oneshot::channel();
        let (release, gate) = mpsc::channel();
        let mut entered = Some(entered);
        let writes = Arc::new(AtomicUsize::new(0));
        let count = writes.clone();
        let worker = Persistence::start(registry.clone(), PathBuf::from("unused"), move |_, _| {
            count.fetch_add(1, Ordering::SeqCst);
            if let Some(entered) = entered.take() {
                entered.send(()).unwrap();
                gate.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            Ok(())
        })
        .unwrap();
        worker.schedule();
        tokio::time::timeout(Duration::from_secs(2), started).await.unwrap().unwrap();
        registry.lock().dirty = true; // last_seen changed during the initial write
        let mut pending = worker.pending_write().expect("initial assignment is still being written");
        assert!(matches!(pending.try_recv(), Err(oneshot::error::TryRecvError::Empty)));
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), pending).await.unwrap().unwrap();
        assert_eq!(writes.load(Ordering::SeqCst), 1);
        assert!(registry.lock().dirty, "timestamp updates wait for the periodic flush");
        assert!(worker.pending_write().is_none());
    }
}
