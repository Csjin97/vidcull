use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use tempfile::tempdir;
use vidcull_db::repo::{NewTask, TaskQueueRepo, TaskState};
use vidcull_db::{Database, open_file};

const KIND: &str = "scan";
const NOW: i64 = 1_700_000_000;

fn new_task() -> NewTask {
    NewTask {
        kind: KIND.to_owned(),
        priority: 0,
        payload: None,
        enqueued_at: NOW,
        size_bytes: 0,
    }
}

fn enqueue_n(db: &Database, n: usize) {
    let repo = TaskQueueRepo::new(db.conn());
    for _ in 0..n {
        repo.enqueue(&new_task()).expect("enqueue");
    }
}

fn count(db: &Database, state: TaskState) -> u64 {
    TaskQueueRepo::new(db.conn())
        .count_by_state(state)
        .expect("count_by_state")
}

#[test]
fn concurrent_workers_claim_each_task_exactly_once() {
    const TASKS: usize = 300;
    const WORKERS: usize = 8;

    let dir = tempdir().expect("tempdir");
    let path = Arc::new(dir.path().join("av.db"));

    {
        let db = open_file(path.as_ref()).expect("seed open");
        enqueue_n(&db, TASKS);
    }

    let mut handles = Vec::new();
    for _ in 0..WORKERS {
        let path = Arc::clone(&path);
        handles.push(thread::spawn(move || {
            let db = open_file(path.as_ref()).expect("worker open");
            let repo = TaskQueueRepo::new(db.conn());
            let mut claimed = Vec::new();
            while let Some(task) = repo.dequeue_next(KIND, NOW).expect("dequeue") {
                claimed.push(task.id);
                repo.mark_done(task.id, NOW).expect("mark done");
            }
            claimed
        }));
    }

    let mut all = Vec::new();
    for h in handles {
        all.extend(h.join().expect("worker thread"));
    }

    let unique: BTreeSet<i64> = all.iter().copied().collect();
    assert_eq!(all.len(), TASKS, "every task was claimed (none lost)");
    assert_eq!(
        unique.len(),
        TASKS,
        "no task was claimed by two workers (atomic dequeue)",
    );

    let db = open_file(path.as_ref()).expect("verify open");
    assert_eq!(count(&db, TaskState::Done), TASKS as u64);
    assert_eq!(count(&db, TaskState::Pending), 0);
    assert_eq!(count(&db, TaskState::Running), 0);
}

#[test]
fn concurrent_producer_and_consumers_lose_no_task() {
    const TASKS: usize = 300;
    const CONSUMERS: usize = 6;

    let dir = tempdir().expect("tempdir");
    let path = Arc::new(dir.path().join("av.db"));
    drop(open_file(path.as_ref()).expect("init schema"));

    let producer_done = Arc::new(AtomicBool::new(false));

    let producer = {
        let path = Arc::clone(&path);
        let producer_done = Arc::clone(&producer_done);
        thread::spawn(move || {
            let db = open_file(path.as_ref()).expect("producer open");
            let repo = TaskQueueRepo::new(db.conn());
            for _ in 0..TASKS {
                repo.enqueue(&new_task()).expect("enqueue");
                thread::yield_now();
            }
            producer_done.store(true, Ordering::SeqCst);
        })
    };

    let mut handles = Vec::new();
    for _ in 0..CONSUMERS {
        let path = Arc::clone(&path);
        let producer_done = Arc::clone(&producer_done);
        handles.push(thread::spawn(move || {
            let db = open_file(path.as_ref()).expect("consumer open");
            let repo = TaskQueueRepo::new(db.conn());
            let mut claimed = Vec::new();
            loop {
                match repo.dequeue_next(KIND, NOW).expect("dequeue") {
                    Some(task) => {
                        repo.mark_done(task.id, NOW).expect("mark done");
                        claimed.push(task.id);
                    }
                    None if producer_done.load(Ordering::SeqCst) => {
                        match repo.dequeue_next(KIND, NOW).expect("dequeue") {
                            Some(task) => {
                                repo.mark_done(task.id, NOW).expect("mark done");
                                claimed.push(task.id);
                            }
                            None => break,
                        }
                    }
                    None => thread::yield_now(),
                }
            }
            claimed
        }));
    }

    producer.join().expect("producer thread");
    let mut all = Vec::new();
    for h in handles {
        all.extend(h.join().expect("consumer thread"));
    }

    let unique: BTreeSet<i64> = all.iter().copied().collect();
    assert_eq!(all.len(), TASKS, "no task processed twice and none lost");
    assert_eq!(
        unique.len(),
        TASKS,
        "every produced task consumed exactly once"
    );

    let db = open_file(path.as_ref()).expect("verify open");
    assert_eq!(count(&db, TaskState::Done), TASKS as u64);
    assert_eq!(count(&db, TaskState::Pending), 0);
}

#[test]
fn concurrent_writers_do_not_surface_sqlite_busy() {
    const THREADS: usize = 8;
    const OPS: usize = 60;

    let dir = tempdir().expect("tempdir");
    let path = Arc::new(dir.path().join("av.db"));
    drop(open_file(path.as_ref()).expect("init schema"));

    let mut handles = Vec::new();
    for _ in 0..THREADS {
        let path = Arc::clone(&path);
        handles.push(thread::spawn(move || {
            let db = open_file(path.as_ref()).expect("open");
            let repo = TaskQueueRepo::new(db.conn());
            for _ in 0..OPS {
                repo.enqueue(&new_task()).expect("enqueue under contention");
                if let Some(task) = repo
                    .dequeue_next(KIND, NOW)
                    .expect("dequeue under contention")
                {
                    repo.mark_done(task.id, NOW)
                        .expect("mark done under contention");
                }
            }
        }));
    }
    for h in handles {
        h.join()
            .expect("writer thread survived (no SQLITE_BUSY surfaced)");
    }

    let db = open_file(path.as_ref()).expect("verify open");
    let done = count(&db, TaskState::Done);
    let pending = count(&db, TaskState::Pending);
    assert_eq!(
        done + pending,
        (THREADS * OPS) as u64,
        "every enqueued task is accounted for (no lost write under contention)",
    );
    assert_eq!(
        count(&db, TaskState::Running),
        0,
        "every claimed task was completed in the same iteration",
    );
}

#[test]
fn crash_with_multiple_running_tasks_recovers_all() {
    const TASKS: usize = 10;
    const INFLIGHT: usize = 4;

    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("av.db");

    {
        let db = open_file(&path).expect("seed open");
        enqueue_n(&db, TASKS);
    }

    {
        let mut crashed_workers = Vec::new();
        let mut claimed = BTreeSet::new();
        for _ in 0..INFLIGHT {
            let db = open_file(&path).expect("worker open");
            let task = TaskQueueRepo::new(db.conn())
                .dequeue_next(KIND, NOW)
                .expect("dequeue")
                .expect("a task to claim");
            assert_eq!(task.state, TaskState::Running);
            assert!(
                claimed.insert(task.id),
                "each worker claimed a distinct task"
            );
            crashed_workers.push(db);
        }
    }

    let db = open_file(&path).expect("restart open");
    assert_eq!(
        count(&db, TaskState::Running),
        INFLIGHT as u64,
        "stale RUNNING rows survived the crash",
    );

    let recovered = TaskQueueRepo::new(db.conn())
        .requeue_running()
        .expect("requeue_running");
    assert_eq!(recovered, INFLIGHT, "every in-flight task was recovered");
    assert_eq!(count(&db, TaskState::Running), 0);
    assert_eq!(count(&db, TaskState::Pending), TASKS as u64);

    let pending = TaskQueueRepo::new(db.conn())
        .list_by_state(TaskState::Pending)
        .expect("list pending");
    let attempted = pending.iter().filter(|t| t.attempts >= 1).count();
    assert_eq!(
        attempted, INFLIGHT,
        "the recovered tasks preserved their attempt counter",
    );
}
