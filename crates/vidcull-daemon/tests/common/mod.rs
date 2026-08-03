#![allow(dead_code)]

use std::time::Duration;

use vidcull_core::{Error, Result};
use vidcull_daemon::{DaemonConfig, ShutdownToken, TaskHandler};
use vidcull_db::Database;
use vidcull_db::repo::{NewTask, Task, TaskQueueRepo};

pub const FAKE_NOW: i64 = 1_700_000_000;

pub const KIND: &str = "scan";

#[must_use]
pub fn config() -> DaemonConfig {
    DaemonConfig {
        kind: KIND.to_owned(),
        poll_interval: Duration::from_millis(5),
        ..DaemonConfig::default()
    }
}

pub fn enqueue_scan(db: &Database, n: usize) -> Vec<i64> {
    let repo = TaskQueueRepo::new(db.conn());
    (0..n)
        .map(|_| {
            repo.enqueue(&NewTask {
                kind: KIND.to_owned(),
                priority: 0,
                payload: None,
                enqueued_at: FAKE_NOW,
                size_bytes: 0,
            })
            .expect("enqueue")
        })
        .collect()
}

pub struct RecordingHandler {
    pub seen: Vec<i64>,
    stop_after: usize,
    token: ShutdownToken,
    fail_ids: Vec<i64>,
}

impl RecordingHandler {
    #[must_use]
    pub fn new(token: ShutdownToken) -> Self {
        Self {
            seen: Vec::new(),
            stop_after: usize::MAX,
            token,
            fail_ids: Vec::new(),
        }
    }

    #[must_use]
    pub fn stop_after(mut self, n: usize) -> Self {
        self.stop_after = n;
        self
    }

    #[must_use]
    pub fn failing(mut self, ids: Vec<i64>) -> Self {
        self.fail_ids = ids;
        self
    }
}

impl TaskHandler for RecordingHandler {
    fn handle(&mut self, task: &Task) -> Result<()> {
        self.seen.push(task.id);
        if self.seen.len() >= self.stop_after {
            self.token.trigger();
        }
        if self.fail_ids.contains(&task.id) {
            return Err(Error::Decode(format!(
                "synthetic failure for task {}",
                task.id
            )));
        }
        Ok(())
    }
}
