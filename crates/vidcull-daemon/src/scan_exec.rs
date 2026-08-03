/**
 * @file    `scan_exec.rs`
 * @brief   백그라운드 초기 폴더 스캔 실행기
 *
 * [변경 이력 (Changelog)]
 * - 2026-08-03 : 반복 스캔 요청을 최신 요청 하나로 병합해 작업 누적 방지
 */
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use crate::ShutdownToken;
use crate::watcher::enqueue_initial_scan_until;

struct ScanJob {
    roots: Vec<PathBuf>,
    exclude_rules: Vec<String>,
    generation: u64,
}

#[derive(Default)]
struct ScanProgress {
    active_jobs: AtomicUsize,
    discovered: AtomicU64,
}

#[derive(Clone)]
pub struct ScanExecutor {
    wake_tx: SyncSender<()>,
    pending: Arc<Mutex<Option<ScanJob>>>,
    generation: Arc<AtomicU64>,
    progress: Arc<ScanProgress>,
}

impl ScanExecutor {
    #[must_use]
    pub fn spawn(db_path: PathBuf, task_kind: String, shutdown: ShutdownToken) -> Self {
        let (wake_tx, wake_rx) = mpsc::sync_channel::<()>(1);
        let pending = Arc::new(Mutex::new(None::<ScanJob>));
        let thread_pending = Arc::clone(&pending);
        let generation = Arc::new(AtomicU64::new(0));
        let thread_generation = Arc::clone(&generation);
        let progress = Arc::new(ScanProgress::default());
        let thread_progress = Arc::clone(&progress);
        std::thread::Builder::new()
            .name("vidcull-scan".to_owned())
            .spawn(move || {
                let mut db = match vidcull_db::open_file(&db_path) {
                    Ok(db) => db,
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            "scan executor: could not open database; background scans disabled"
                        );
                        return;
                    }
                };
                while wake_rx.recv().is_ok() {
                    if shutdown.is_triggered() {
                        break;
                    }
                    let Some(job) = thread_pending.lock().ok().and_then(|mut slot| slot.take())
                    else {
                        continue;
                    };
                    thread_progress.discovered.store(0, Ordering::Relaxed);
                    thread_progress.active_jobs.fetch_add(1, Ordering::Relaxed);
                    let should_stop = || {
                        shutdown.is_triggered()
                            || thread_generation.load(Ordering::Acquire) != job.generation
                    };
                    let result = enqueue_initial_scan_until(
                        &mut db,
                        &job.roots,
                        &task_kind,
                        crate::unix_now(),
                        &job.exclude_rules,
                        &should_stop,
                        &thread_progress.discovered,
                    );
                    thread_progress.active_jobs.fetch_sub(1, Ordering::Relaxed);
                    let superseded = thread_generation.load(Ordering::Acquire) != job.generation;
                    match result {
                        Ok(count) if superseded => tracing::info!(
                            count,
                            folders = job.roots.len(),
                            "superseded folder scan stopped; latest request will continue"
                        ),
                        Ok(count) => tracing::info!(
                            count,
                            folders = job.roots.len(),
                            "enqueued scan of folders (background)"
                        ),
                        Err(err) => tracing::warn!(
                            error = %err,
                            "background folder scan failed; relying on the watcher"
                        ),
                    }
                }
            })
            .expect("spawn vidcull-scan executor thread");
        Self {
            wake_tx,
            pending,
            generation,
            progress,
        }
    }

    pub fn submit(&self, roots: Vec<PathBuf>, exclude_rules: Vec<String>) {
        if roots.is_empty() {
            return;
        }
        if let Ok(mut pending) = self.pending.lock() {
            let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
            *pending = Some(ScanJob {
                roots,
                exclude_rules,
                generation,
            });
        } else {
            tracing::warn!("scan executor queue is poisoned; scan request was not queued");
            return;
        }
        match self.wake_tx.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => {}
            Err(TrySendError::Disconnected(())) => {
                tracing::warn!("scan executor is unavailable; scan request was not queued");
            }
        }
    }

    #[must_use]
    pub fn is_scanning(&self) -> bool {
        self.progress.active_jobs.load(Ordering::Relaxed) > 0
    }

    #[must_use]
    pub fn discovered(&self) -> u64 {
        self.progress.discovered.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rapid_submissions_keep_only_the_latest_pending_scan() {
        let (wake_tx, _wake_rx) = mpsc::sync_channel(1);
        let executor = ScanExecutor {
            wake_tx,
            pending: Arc::new(Mutex::new(None)),
            generation: Arc::new(AtomicU64::new(0)),
            progress: Arc::new(ScanProgress::default()),
        };

        executor.submit(vec![PathBuf::from("old")], vec!["old-rule".to_owned()]);
        executor.submit(
            vec![PathBuf::from("latest")],
            vec!["latest-rule".to_owned()],
        );

        let pending = executor.pending.lock().expect("pending lock");
        let job = pending.as_ref().expect("latest job");
        assert_eq!(job.roots, vec![PathBuf::from("latest")]);
        assert_eq!(job.exclude_rules, vec!["latest-rule"]);
        assert_eq!(job.generation, 2);
        assert_eq!(executor.generation.load(Ordering::Acquire), 2);
    }
}
