
CREATE INDEX idx_task_queue_dequeue
    ON task_queue(state, kind, priority DESC, enqueued_at);

CREATE INDEX idx_task_queue_active_payload
    ON task_queue(kind, payload) WHERE state IN ('PENDING', 'RUNNING');

CREATE INDEX idx_dup_groups_trust
    ON duplicate_groups(trust_level, id);
