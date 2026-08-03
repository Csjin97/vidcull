use std::collections::VecDeque;
use std::fmt::{self, Write as _};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use vidcull_ipc::{LogLevel, LogRecord};

pub const DEFAULT_CAPACITY: usize = 1024;

#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<Mutex<VecDeque<LogRecord>>>,
    capacity: usize,
}

impl LogBuffer {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(capacity))),
            capacity,
        }
    }

    pub fn push(&self, record: LogRecord) {
        let mut queue = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if queue.len() >= self.capacity {
            queue.pop_front();
        }
        queue.push_back(record);
    }

    #[must_use]
    pub fn snapshot(&self, max: usize) -> Vec<LogRecord> {
        let queue = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let take = queue.len().min(max);
        let skip = queue.len() - take;
        queue.iter().skip(skip).cloned().collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[must_use]
    pub fn layer(&self) -> LogBufferLayer {
        LogBufferLayer {
            buffer: self.clone(),
        }
    }
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

pub struct LogBufferLayer {
    buffer: LogBuffer,
}

impl<S: Subscriber> Layer<S> for LogBufferLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        self.buffer.push(LogRecord {
            timestamp_ms: now_ms(),
            level: level_to_ipc(*metadata.level()),
            target: metadata.target().to_owned(),
            message: visitor.finish(),
        });
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: String,
}

impl MessageVisitor {
    fn finish(mut self) -> String {
        if !self.fields.is_empty() {
            if self.message.is_empty() {
                self.message = self.fields.trim_start().to_owned();
            } else {
                self.message.push_str(&self.fields);
            }
        }
        self.message
    }
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }
}

fn level_to_ipc(level: Level) -> LogLevel {
    match level {
        Level::ERROR => LogLevel::Error,
        Level::WARN => LogLevel::Warn,
        Level::INFO => LogLevel::Info,
        Level::DEBUG => LogLevel::Debug,
        Level::TRACE => LogLevel::Trace,
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(message: &str) -> LogRecord {
        LogRecord {
            timestamp_ms: 0,
            level: LogLevel::Info,
            target: "test".to_owned(),
            message: message.to_owned(),
        }
    }

    #[test]
    fn snapshot_returns_oldest_first_bounded_by_max() {
        let buf = LogBuffer::new(8);
        for i in 0..5 {
            buf.push(rec(&format!("m{i}")));
        }
        let all = buf.snapshot(10);
        let msgs: Vec<_> = all.iter().map(|r| r.message.as_str()).collect();
        assert_eq!(msgs, ["m0", "m1", "m2", "m3", "m4"]);
        let tail = buf.snapshot(2);
        let tail_msgs: Vec<_> = tail.iter().map(|r| r.message.as_str()).collect();
        assert_eq!(tail_msgs, ["m3", "m4"]);
    }

    #[test]
    fn ring_evicts_oldest_at_capacity() {
        let buf = LogBuffer::new(3);
        for i in 0..6 {
            buf.push(rec(&format!("m{i}")));
        }
        assert_eq!(buf.len(), 3);
        let msgs: Vec<_> = buf.snapshot(10).iter().map(|r| r.message.clone()).collect();
        assert_eq!(msgs, ["m3", "m4", "m5"]);
    }

    #[test]
    fn empty_buffer_snapshots_empty() {
        let buf = LogBuffer::default();
        assert!(buf.is_empty());
        assert!(buf.snapshot(100).is_empty());
        assert!(buf.snapshot(0).is_empty());
    }

    #[test]
    fn zero_capacity_is_clamped_to_one() {
        let buf = LogBuffer::new(0);
        buf.push(rec("a"));
        buf.push(rec("b"));
        let msgs: Vec<_> = buf.snapshot(10).iter().map(|r| r.message.clone()).collect();
        assert_eq!(msgs, ["b"]);
    }

    #[test]
    fn visitor_joins_message_and_fields() {
        let v = MessageVisitor {
            message: "started".to_owned(),
            fields: " id=7".to_owned(),
        };
        assert_eq!(v.finish(), "started id=7");

        let only_fields = MessageVisitor {
            message: String::new(),
            fields: " error=boom".to_owned(),
        };
        assert_eq!(only_fields.finish(), "error=boom");
    }

    #[test]
    fn layer_captures_real_tracing_events() {
        use tracing_subscriber::prelude::*;
        let buf = LogBuffer::new(16);
        let subscriber = tracing_subscriber::registry().with(buf.layer());
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(id = 7, "started");
            tracing::warn!("careful");
        });
        let records = buf.snapshot(10);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].level, LogLevel::Info);
        assert!(records[0].message.contains("started"), "{:?}", records[0]);
        assert!(records[0].message.contains("id=7"), "{:?}", records[0]);
        assert_eq!(records[1].level, LogLevel::Warn);
        assert_eq!(records[1].message, "careful");
    }
}
