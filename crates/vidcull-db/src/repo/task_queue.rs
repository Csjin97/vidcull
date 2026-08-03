/**
 * @file    `task_queue.rs`
 * @brief   영속 인덱싱 작업 큐 저장소
 *
 * [변경 이력 (Changelog)]
 * - 2026-08-03 : 대용량 스캔용 작업 ID 워터마크와 증분 payload 조회 추가
 */
// 2026-08-03: 대용량 작업 큐 진행 조회를 위한 상태별 스트리밍 순회 추가.
use rusqlite::{Connection, OptionalExtension, Row, params};
use vidcull_core::{Error, Result};

use crate::connection::map_err;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskState {
    Pending,
    Running,
    Done,
    Failed,
}

impl TaskState {
    pub(super) fn as_text(self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Running => "RUNNING",
            Self::Done => "DONE",
            Self::Failed => "FAILED",
        }
    }

    pub(super) fn from_text(s: &str) -> Result<Self> {
        match s {
            "PENDING" => Ok(Self::Pending),
            "RUNNING" => Ok(Self::Running),
            "DONE" => Ok(Self::Done),
            "FAILED" => Ok(Self::Failed),
            other => Err(Error::Database(format!(
                "unknown task state `{other}`; expected PENDING/RUNNING/DONE/FAILED",
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTask {
    pub kind: String,
    pub priority: i32,
    pub payload: Option<Vec<u8>>,
    pub enqueued_at: i64,
    pub size_bytes: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: i64,
    pub kind: String,
    pub state: TaskState,
    pub priority: i32,
    pub payload: Option<Vec<u8>>,
    pub attempts: i32,
    pub enqueued_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub last_error: Option<String>,
}

pub struct TaskQueueRepo<'a> {
    conn: &'a Connection,
}

impl<'a> TaskQueueRepo<'a> {
    #[must_use]
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    pub fn enqueue(&self, task: &NewTask) -> Result<i64> {
        self.conn
            .prepare_cached(
                "INSERT INTO task_queue (kind, state, priority, payload, enqueued_at, size_bytes) \
                 VALUES (?1, 'PENDING', ?2, ?3, ?4, ?5)",
            )
            .map_err(map_err)?
            .execute(params![
                task.kind,
                task.priority,
                task.payload,
                task.enqueued_at,
                task.size_bytes
            ])
            .map_err(map_err)?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn has_failed_with_size(&self, payload: &[u8], size_bytes: i64) -> Result<bool> {
        let count: i64 = self
            .conn
            .prepare_cached(
                "SELECT COUNT(1) FROM task_queue \
                 WHERE payload = ?1 AND state = 'FAILED' AND size_bytes = ?2",
            )
            .map_err(map_err)?
            .query_row(params![payload, size_bytes], |row| row.get(0))
            .map_err(map_err)?;
        Ok(count > 0)
    }

    pub fn count_failed_by_payload(&self, kind: &str, payload: &[u8]) -> Result<i64> {
        self.conn
            .prepare_cached(
                "SELECT COUNT(1) FROM task_queue \
                 WHERE state = 'FAILED' AND kind = ?1 AND payload = ?2",
            )
            .map_err(map_err)?
            .query_row(params![kind, payload], |row| row.get(0))
            .map_err(map_err)
    }

    pub fn list_active_payloads(&self, kind: &str) -> Result<Vec<(i64, Vec<u8>)>> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT id, payload FROM task_queue \
                 WHERE kind = ?1 AND payload IS NOT NULL \
                   AND state IN ('PENDING', 'RUNNING')",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map(params![kind], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<(i64, Vec<u8>)>>>()
            .map_err(map_err)?;
        Ok(rows)
    }

    pub fn max_id(&self, kind: &str) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT COALESCE(MAX(id), 0) FROM task_queue WHERE kind = ?1",
                params![kind],
                |row| row.get(0),
            )
            .map_err(map_err)
    }

    pub fn list_payloads_after(
        &self,
        kind: &str,
        after_id: i64,
    ) -> Result<Vec<(i64, Vec<u8>, String)>> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT id, payload, state FROM task_queue \
                 WHERE kind = ?1 AND id > ?2 AND payload IS NOT NULL ORDER BY id ASC",
            )
            .map_err(map_err)?;
        stmt.query_map(params![kind, after_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .map_err(map_err)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(map_err)
    }

    pub fn exists_active_by_payload(&self, kind: &str, payload: &[u8]) -> Result<bool> {
        let count: i64 = self
            .conn
            .prepare_cached(
                "SELECT COUNT(1) FROM task_queue WHERE kind = ?1 AND payload = ?2 AND state IN ('PENDING', 'RUNNING')",
            )
            .map_err(map_err)?
            .query_row(params![kind, payload], |row| row.get(0))
            .map_err(map_err)?;
        Ok(count > 0)
    }

    pub fn exists(&self, id: i64) -> Result<bool> {
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM task_queue WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(map_err)?;
        Ok(found.is_some())
    }

    pub fn get(&self, id: i64) -> Result<Option<Task>> {
        self.conn
            .query_row(SELECT_ALL_BY_ID, params![id], row_to_task)
            .optional()
            .map_err(map_err)
            .and_then(Option::transpose)
    }

    pub fn dequeue_next(&self, kind: &str, now_unix: i64) -> Result<Option<Task>> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "UPDATE task_queue SET \
                    state = 'RUNNING', \
                    started_at = ?1, \
                    attempts = attempts + 1 \
                 WHERE id = (\
                    SELECT id FROM task_queue \
                    WHERE state = 'PENDING' AND kind = ?2 AND enqueued_at <= ?1 \
                    ORDER BY priority DESC, enqueued_at ASC, id ASC \
                    LIMIT 1\
                 ) \
                 RETURNING id, kind, state, priority, payload, attempts, \
                           enqueued_at, started_at, finished_at, last_error",
            )
            .map_err(map_err)?;
        let result = stmt
            .query_row(params![now_unix, kind], row_to_task)
            .optional()
            .map_err(map_err)?;
        result.transpose()
    }

    pub fn dequeue_next_at_priority(
        &self,
        kind: &str,
        priority: i32,
        now_unix: i64,
    ) -> Result<Option<Task>> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "UPDATE task_queue SET \
                    state = 'RUNNING', \
                    started_at = ?1, \
                    attempts = attempts + 1 \
                 WHERE id = (\
                    SELECT id FROM task_queue \
                    WHERE state = 'PENDING' AND kind = ?2 AND priority = ?3 \
                      AND enqueued_at <= ?1 \
                    ORDER BY enqueued_at ASC, id ASC \
                    LIMIT 1\
                 ) \
                 RETURNING id, kind, state, priority, payload, attempts, \
                           enqueued_at, started_at, finished_at, last_error",
            )
            .map_err(map_err)?;
        let result = stmt
            .query_row(params![now_unix, kind, priority], row_to_task)
            .optional()
            .map_err(map_err)?;
        result.transpose()
    }

    pub fn dequeue_next_above_priority(
        &self,
        kind: &str,
        min_priority_exclusive: i32,
        now_unix: i64,
    ) -> Result<Option<Task>> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "UPDATE task_queue SET \
                    state = 'RUNNING', \
                    started_at = ?1, \
                    attempts = attempts + 1 \
                 WHERE id = (\
                    SELECT id FROM task_queue \
                    WHERE state = 'PENDING' AND kind = ?2 AND priority > ?3 \
                      AND enqueued_at <= ?1 \
                    ORDER BY priority DESC, enqueued_at ASC, id ASC \
                    LIMIT 1\
                 ) \
                 RETURNING id, kind, state, priority, payload, attempts, \
                           enqueued_at, started_at, finished_at, last_error",
            )
            .map_err(map_err)?;
        let result = stmt
            .query_row(params![now_unix, kind, min_priority_exclusive], row_to_task)
            .optional()
            .map_err(map_err)?;
        result.transpose()
    }

    pub fn mark_done(&self, id: i64, finished_at: i64) -> Result<()> {
        self.conn
            .prepare_cached("UPDATE task_queue SET state = 'DONE', finished_at = ?1 WHERE id = ?2")
            .map_err(map_err)?
            .execute(params![finished_at, id])
            .map_err(map_err)?;
        Ok(())
    }

    pub fn mark_failed(&self, id: i64, finished_at: i64, message: &str) -> Result<()> {
        self.conn
            .prepare_cached(
                "UPDATE task_queue SET state = 'FAILED', finished_at = ?1, last_error = ?2 \
                 WHERE id = ?3",
            )
            .map_err(map_err)?
            .execute(params![finished_at, message, id])
            .map_err(map_err)?;
        Ok(())
    }

    pub fn delete(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM task_queue WHERE id = ?1", params![id])
            .map_err(map_err)?;
        Ok(())
    }

    pub fn delete_if_pending(&self, id: i64) -> Result<bool> {
        let affected = self
            .conn
            .execute(
                "DELETE FROM task_queue WHERE id = ?1 AND state = ?2",
                params![id, TaskState::Pending.as_text()],
            )
            .map_err(map_err)?;
        Ok(affected > 0)
    }

    pub fn requeue_running(&self) -> Result<usize> {
        let recovered = self
            .conn
            .execute(
                "UPDATE task_queue SET state = 'PENDING', started_at = NULL \
                 WHERE state = 'RUNNING'",
                [],
            )
            .map_err(map_err)?;
        Ok(recovered)
    }

    pub fn future_enqueued_pending(&self, now: i64) -> Result<Vec<(i64, i32)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT enqueued_at, attempts FROM task_queue \
                 WHERE state = 'PENDING' AND enqueued_at > ?1 \
                 ORDER BY enqueued_at ASC, id ASC",
            )
            .map_err(map_err)?;
        let rows = stmt
            .query_map(params![now], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i32>(1)?))
            })
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<(i64, i32)>>>()
            .map_err(map_err)?;
        Ok(rows)
    }

    pub fn reset_future_enqueued_pending(&self, now: i64) -> Result<usize> {
        let reset = self
            .conn
            .execute(
                "UPDATE task_queue SET enqueued_at = ?1 \
                 WHERE state = 'PENDING' AND enqueued_at > ?1",
                params![now],
            )
            .map_err(map_err)?;
        Ok(reset)
    }

    pub fn count_by_state(&self, state: TaskState) -> Result<u64> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM task_queue WHERE state = ?1",
                params![state.as_text()],
                |row| row.get(0),
            )
            .map_err(map_err)?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    pub fn count_distinct_files_by_state(&self, state: TaskState) -> Result<u64> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT \
                   (SELECT COUNT(DISTINCT payload) FROM task_queue \
                      WHERE state = ?1 AND priority >= 0 AND payload IS NOT NULL) \
                 + (SELECT COUNT(*) FROM task_queue \
                      WHERE state = ?1 AND priority >= 0 AND payload IS NULL)",
                params![state.as_text()],
                |row| row.get(0),
            )
            .map_err(map_err)?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    pub fn count_distinct_files_at_priority(&self, priority: i32, state: TaskState) -> Result<u64> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT \
                   (SELECT COUNT(DISTINCT payload) FROM task_queue \
                      WHERE state = ?1 AND priority = ?2 AND payload IS NOT NULL) \
                 + (SELECT COUNT(*) FROM task_queue \
                      WHERE state = ?1 AND priority = ?2 AND payload IS NULL)",
                params![state.as_text(), priority],
                |row| row.get(0),
            )
            .map_err(map_err)?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    pub fn count_pending_min_priority(&self, min_priority: i32) -> Result<u64> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM task_queue \
                 WHERE state = ?1 AND priority >= ?2",
                params![TaskState::Pending.as_text(), min_priority],
                |row| row.get(0),
            )
            .map_err(map_err)?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    pub fn count_pending_by_kind(&self, kind: &str) -> Result<u64> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM task_queue WHERE state = ?1 AND kind = ?2",
                params![TaskState::Pending.as_text(), kind],
                |row| row.get(0),
            )
            .map_err(map_err)?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    pub fn min_pending_enqueued_at(&self, kind: &str) -> Result<Option<i64>> {
        self.conn
            .query_row(
                "SELECT MIN(enqueued_at) FROM task_queue WHERE state = ?1 AND kind = ?2",
                params![TaskState::Pending.as_text(), kind],
                |row| row.get(0),
            )
            .map_err(map_err)
    }

    pub fn sum_outstanding_size_bytes(&self) -> Result<u64> {
        let total: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(SUM(size_bytes), 0) FROM task_queue \
                 WHERE state IN ('PENDING', 'RUNNING')",
                [],
                |row| row.get(0),
            )
            .map_err(map_err)?;
        Ok(u64::try_from(total).unwrap_or(0))
    }

    pub fn list_by_state(&self, state: TaskState) -> Result<Vec<Task>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, kind, state, priority, payload, attempts, \
                        enqueued_at, started_at, finished_at, last_error \
                 FROM task_queue WHERE state = ?1 ORDER BY id ASC",
            )
            .map_err(map_err)?;
        let raw = stmt
            .query_map(params![state.as_text()], row_to_task)
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<Result<Task>>>>()
            .map_err(map_err)?;
        raw.into_iter().collect()
    }

    pub fn visit_by_state(
        &self,
        state: TaskState,
        mut visit: impl FnMut(&Task) -> Result<()>,
    ) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, kind, state, priority, payload, attempts, \
                        enqueued_at, started_at, finished_at, last_error \
                 FROM task_queue WHERE state = ?1 ORDER BY id ASC",
            )
            .map_err(map_err)?;
        let mut rows = stmt.query(params![state.as_text()]).map_err(map_err)?;
        while let Some(row) = rows.next().map_err(map_err)? {
            let task = row_to_task(row).map_err(map_err)??;
            visit(&task)?;
        }
        Ok(())
    }

    pub fn list_by_priority_state(&self, priority: i32, state: TaskState) -> Result<Vec<Task>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, kind, state, priority, payload, attempts, \
                        enqueued_at, started_at, finished_at, last_error \
                 FROM task_queue WHERE state = ?1 AND priority = ?2 ORDER BY id ASC",
            )
            .map_err(map_err)?;
        let raw = stmt
            .query_map(params![state.as_text(), priority], row_to_task)
            .map_err(map_err)?
            .collect::<rusqlite::Result<Vec<Result<Task>>>>()
            .map_err(map_err)?;
        raw.into_iter().collect()
    }

    pub fn requeue_busy_task(&self, id: i64, enqueued_at: i64, attempts: i32) -> Result<()> {
        self.conn
            .prepare_cached(
                "UPDATE task_queue SET \
                    state = 'PENDING', \
                    started_at = NULL, \
                    enqueued_at = ?1, \
                    attempts = ?2 \
                 WHERE id = ?3",
            )
            .map_err(map_err)?
            .execute(params![enqueued_at, attempts, id])
            .map_err(map_err)?;
        Ok(())
    }
}

const SELECT_ALL_BY_ID: &str = "SELECT id, kind, state, priority, payload, attempts, \
                                       enqueued_at, started_at, finished_at, last_error \
                                FROM task_queue WHERE id = ?1";

fn row_to_task(row: &Row<'_>) -> rusqlite::Result<Result<Task>> {
    let id: i64 = row.get("id")?;
    let kind: String = row.get("kind")?;
    let state_text: String = row.get("state")?;
    let priority: i32 = row.get("priority")?;
    let payload: Option<Vec<u8>> = row.get("payload")?;
    let attempts: i32 = row.get("attempts")?;
    let enqueued_at: i64 = row.get("enqueued_at")?;
    let started_at: Option<i64> = row.get("started_at")?;
    let finished_at: Option<i64> = row.get("finished_at")?;
    let last_error: Option<String> = row.get("last_error")?;

    Ok(TaskState::from_text(&state_text).map(|state| Task {
        id,
        kind,
        state,
        priority,
        payload,
        attempts,
        enqueued_at,
        started_at,
        finished_at,
        last_error,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_state_text_round_trip() {
        for state in &[
            TaskState::Pending,
            TaskState::Running,
            TaskState::Done,
            TaskState::Failed,
        ] {
            let text = state.as_text();
            let parsed = TaskState::from_text(text).unwrap();
            assert_eq!(*state, parsed);
        }
        assert!(TaskState::from_text("UNKNOWN").is_err());
    }

    const NOW: i64 = 1_700_000_000;

    fn pending_at(repo: &TaskQueueRepo<'_>, enqueued_at: i64) -> i64 {
        repo.enqueue(&NewTask {
            kind: "scan".into(),
            priority: 0,
            payload: None,
            enqueued_at,
            size_bytes: 0,
        })
        .expect("enqueue")
    }

    #[test]
    fn count_pending_by_kind_is_zero_for_empty_queue() {
        let db = crate::open_in_memory().expect("open in-memory db");
        let repo = TaskQueueRepo::new(db.conn());
        assert_eq!(repo.count_pending_by_kind("scan").expect("count"), 0);
    }

    #[test]
    fn count_pending_by_kind_counts_future_stamped_rows() {
        let db = crate::open_in_memory().expect("open in-memory db");
        let repo = TaskQueueRepo::new(db.conn());
        pending_at(&repo, NOW + 3);
        pending_at(&repo, NOW - 5);
        assert_eq!(repo.count_pending_by_kind("scan").expect("count"), 2);
    }

    #[test]
    fn count_pending_by_kind_is_scoped_to_the_given_kind() {
        let db = crate::open_in_memory().expect("open in-memory db");
        let repo = TaskQueueRepo::new(db.conn());
        pending_at(&repo, NOW);
        repo.enqueue(&NewTask {
            kind: "other".into(),
            priority: 0,
            payload: None,
            enqueued_at: NOW,
            size_bytes: 0,
        })
        .expect("enqueue other-kind task");
        assert_eq!(repo.count_pending_by_kind("scan").expect("count"), 1);
        assert_eq!(repo.count_pending_by_kind("other").expect("count"), 1);
        assert_eq!(repo.count_pending_by_kind("nonexistent").expect("count"), 0);
    }

    #[test]
    fn count_pending_by_kind_excludes_running_done_and_failed() {
        let db = crate::open_in_memory().expect("open in-memory db");
        let repo = TaskQueueRepo::new(db.conn());
        let running_id = pending_at(&repo, NOW - 1);
        repo.dequeue_next("scan", NOW).expect("claim to RUNNING");
        let done_id = pending_at(&repo, NOW - 1);
        repo.dequeue_next("scan", NOW).expect("claim");
        repo.mark_done(done_id, NOW).expect("mark done");
        let failed_id = pending_at(&repo, NOW - 1);
        repo.dequeue_next("scan", NOW).expect("claim");
        repo.mark_failed(failed_id, NOW, "boom")
            .expect("mark failed");
        let _ = running_id;

        pending_at(&repo, NOW - 1);
        assert_eq!(repo.count_pending_by_kind("scan").expect("count"), 1);
    }

    fn pending_prio(repo: &TaskQueueRepo<'_>, priority: i32) -> i64 {
        repo.enqueue(&NewTask {
            kind: "scan".into(),
            priority,
            payload: None,
            enqueued_at: NOW,
            size_bytes: 0,
        })
        .expect("enqueue")
    }

    const PARTIAL_PRIORITY: i32 = -200;

    #[test]
    fn dequeue_above_priority_skips_the_partial_band_but_claims_the_rest() {
        let db = crate::open_in_memory().expect("open in-memory db");
        let repo = TaskQueueRepo::new(db.conn());
        let partial = pending_prio(&repo, PARTIAL_PRIORITY);
        let fresh = pending_prio(&repo, 0);

        let claimed = repo
            .dequeue_next_above_priority("scan", PARTIAL_PRIORITY, NOW)
            .expect("dequeue above")
            .expect("a claimable non-partial task");
        assert_eq!(
            claimed.id, fresh,
            "must claim the priority-0 task, not the partial"
        );

        assert!(
            repo.dequeue_next_above_priority("scan", PARTIAL_PRIORITY, NOW)
                .expect("dequeue above")
                .is_none(),
            "the partial band must stay unclaimed by the fenced dequeue"
        );

        let plain = repo
            .dequeue_next("scan", NOW)
            .expect("dequeue")
            .expect("the partial is still claimable normally");
        assert_eq!(plain.id, partial);
        assert_eq!(
            plain.attempts, 1,
            "skipped partial never had attempts incremented before this claim"
        );
    }

    #[test]
    fn dequeue_above_priority_is_strict_and_returns_none_for_partial_only_queue() {
        let db = crate::open_in_memory().expect("open in-memory db");
        let repo = TaskQueueRepo::new(db.conn());
        pending_prio(&repo, PARTIAL_PRIORITY);
        pending_prio(&repo, PARTIAL_PRIORITY - 1);
        assert!(
            repo.dequeue_next_above_priority("scan", PARTIAL_PRIORITY, NOW)
                .expect("dequeue above")
                .is_none(),
            "a partial-only queue yields None (worker then sleeps, reclaiming a freed slot next poll)"
        );
    }

    #[test]
    fn dequeue_above_priority_preserves_priority_order_within_admitted_bands() {
        let db = crate::open_in_memory().expect("open in-memory db");
        let repo = TaskQueueRepo::new(db.conn());
        pending_prio(&repo, PARTIAL_PRIORITY);
        let densify = pending_prio(&repo, -100);
        let fresh = pending_prio(&repo, 0);

        let first = repo
            .dequeue_next_above_priority("scan", PARTIAL_PRIORITY, NOW)
            .expect("dequeue above")
            .expect("task");
        assert_eq!(first.id, fresh, "highest admitted priority first");
        let second = repo
            .dequeue_next_above_priority("scan", PARTIAL_PRIORITY, NOW)
            .expect("dequeue above")
            .expect("task");
        assert_eq!(second.id, densify, "then the densify band");
    }

    #[test]
    fn dequeue_above_priority_ignores_future_stamped_rows() {
        let db = crate::open_in_memory().expect("open in-memory db");
        let repo = TaskQueueRepo::new(db.conn());
        repo.enqueue(&NewTask {
            kind: "scan".into(),
            priority: 0,
            payload: None,
            enqueued_at: NOW + 3,
            size_bytes: 0,
        })
        .expect("enqueue");
        assert!(
            repo.dequeue_next_above_priority("scan", PARTIAL_PRIORITY, NOW)
                .expect("dequeue above")
                .is_none(),
            "enqueued_at > now must not be claimed (same due-time filter as dequeue_next)"
        );
    }

    #[test]
    fn min_pending_enqueued_at_is_none_for_empty_queue() {
        let db = crate::open_in_memory().expect("open in-memory db");
        let repo = TaskQueueRepo::new(db.conn());
        assert_eq!(repo.min_pending_enqueued_at("scan").expect("min"), None);
    }

    #[test]
    fn min_pending_enqueued_at_picks_the_earliest_including_future_stamped() {
        let db = crate::open_in_memory().expect("open in-memory db");
        let repo = TaskQueueRepo::new(db.conn());
        pending_at(&repo, NOW + 30);
        pending_at(&repo, NOW + 1);
        pending_at(&repo, NOW - 5);
        assert_eq!(
            repo.min_pending_enqueued_at("scan").expect("min"),
            Some(NOW - 5)
        );
    }

    #[test]
    fn min_pending_enqueued_at_is_scoped_to_the_given_kind() {
        let db = crate::open_in_memory().expect("open in-memory db");
        let repo = TaskQueueRepo::new(db.conn());
        pending_at(&repo, NOW + 10);
        repo.enqueue(&NewTask {
            kind: "other".into(),
            priority: 0,
            payload: None,
            enqueued_at: NOW - 100,
            size_bytes: 0,
        })
        .expect("enqueue other-kind task");
        assert_eq!(
            repo.min_pending_enqueued_at("scan").expect("min"),
            Some(NOW + 10),
            "must not be pulled down by a far-earlier row of a different kind"
        );
        assert_eq!(
            repo.min_pending_enqueued_at("nonexistent").expect("min"),
            None
        );
    }

    #[test]
    fn min_pending_enqueued_at_excludes_running_done_and_failed() {
        let db = crate::open_in_memory().expect("open in-memory db");
        let repo = TaskQueueRepo::new(db.conn());
        let done_id = pending_at(&repo, NOW - 100);
        repo.dequeue_next("scan", NOW).expect("claim");
        repo.mark_done(done_id, NOW).expect("mark done");

        pending_at(&repo, NOW + 5);
        assert_eq!(
            repo.min_pending_enqueued_at("scan").expect("min"),
            Some(NOW + 5),
            "a DONE row's earlier enqueued_at must not leak into the PENDING minimum"
        );
    }

    #[test]
    fn reset_future_enqueued_pending_makes_future_task_immediately_claimable() {
        let db = crate::open_in_memory().expect("open in-memory db");
        let repo = TaskQueueRepo::new(db.conn());
        pending_at(&repo, NOW + 30);
        assert!(
            repo.dequeue_next("scan", NOW).expect("dq").is_none(),
            "future-enqueued task is not claimable before reset"
        );
        assert_eq!(repo.reset_future_enqueued_pending(NOW).expect("reset"), 1);
        let claimed = repo
            .dequeue_next("scan", NOW)
            .expect("dq")
            .expect("claimed after reset");
        assert_eq!(claimed.enqueued_at, NOW);
    }

    #[test]
    fn reset_future_enqueued_pending_respects_boundaries() {
        let db = crate::open_in_memory().expect("open in-memory db");
        let repo = TaskQueueRepo::new(db.conn());
        let at_now = pending_at(&repo, NOW);
        let future = pending_at(&repo, NOW + 1);
        let past = pending_at(&repo, NOW - 1);

        assert_eq!(repo.reset_future_enqueued_pending(NOW).expect("reset"), 1);
        assert_eq!(repo.get(at_now).expect("get").unwrap().enqueued_at, NOW);
        assert_eq!(repo.get(future).expect("get").unwrap().enqueued_at, NOW);
        assert_eq!(repo.get(past).expect("get").unwrap().enqueued_at, NOW - 1);
    }

    #[test]
    fn future_enqueued_pending_reports_delta_inputs_only() {
        let db = crate::open_in_memory().expect("open in-memory db");
        let repo = TaskQueueRepo::new(db.conn());
        let id = pending_at(&repo, NOW + 30);
        repo.requeue_busy_task(id, NOW + 30, 4).expect("requeue");
        pending_at(&repo, NOW - 5);

        assert_eq!(
            repo.future_enqueued_pending(NOW).expect("diag"),
            vec![(NOW + 30, 4)]
        );
    }

    #[test]
    fn recover_composition_requeues_running_and_resets_future_pending() {
        let db = crate::open_in_memory().expect("open in-memory db");
        let repo = TaskQueueRepo::new(db.conn());
        let running_id = pending_at(&repo, NOW - 10);
        repo.dequeue_next("scan", NOW).expect("dq").expect("claim");
        let future_id = pending_at(&repo, NOW + 30);
        assert_eq!(repo.requeue_running().expect("requeue_running"), 1);
        assert_eq!(repo.reset_future_enqueued_pending(NOW).expect("reset"), 1);
        let mut claimed = vec![
            repo.dequeue_next("scan", NOW).expect("dq").expect("c1").id,
            repo.dequeue_next("scan", NOW).expect("dq").expect("c2").id,
        ];
        claimed.sort_unstable();
        let mut expected = vec![running_id, future_id];
        expected.sort_unstable();
        assert_eq!(claimed, expected);
    }

    #[test]
    fn delete_if_pending_only_removes_pending_rows() {
        let db = crate::open_in_memory().expect("open in-memory db");
        let repo = TaskQueueRepo::new(db.conn());

        let pending = pending_at(&repo, NOW);
        assert!(
            repo.delete_if_pending(pending)
                .expect("delete pending returns true"),
            "a PENDING row is removed and reported",
        );
        assert!(
            repo.get(pending).expect("get").is_none(),
            "the PENDING row was deleted"
        );

        let running = pending_at(&repo, NOW);
        repo.dequeue_next("scan", NOW)
            .expect("dq")
            .expect("claim → RUNNING");
        assert!(
            !repo
                .delete_if_pending(running)
                .expect("delete RUNNING is a no-op"),
            "delete_if_pending must not touch a RUNNING task",
        );
        let row = repo
            .get(running)
            .expect("get")
            .expect("RUNNING row still present");
        assert_eq!(
            row.state,
            TaskState::Running,
            "the RUNNING task is left intact"
        );

        assert!(
            !repo
                .delete_if_pending(999_999)
                .expect("delete missing id returns false"),
            "a non-existent id reports no deletion",
        );
    }
}
