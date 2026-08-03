
CREATE INDEX idx_task_queue_failed_payload
    ON task_queue(payload, size_bytes) WHERE state = 'FAILED';
