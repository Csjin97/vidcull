use std::io::{self, Read};
use std::sync::atomic::AtomicBool;

#[derive(Clone, Copy, Default)]
pub struct Cancel<'a> {
    pub pause: Option<&'a AtomicBool>,
    pub removal: Option<&'a AtomicBool>,
}

impl Cancel<'_> {
    #[must_use]
    pub fn fired(&self) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        self.pause.is_some_and(|c| c.load(Relaxed)) || self.removal.is_some_and(|c| c.load(Relaxed))
    }
}

pub const CANCEL_READ_MARKER: io::ErrorKind = io::ErrorKind::Other;

pub struct CancelRead<'a, R> {
    inner: R,
    cancel: Cancel<'a>,
}

impl<'a, R> CancelRead<'a, R> {
    pub fn new(inner: R, cancel: Cancel<'a>) -> Self {
        Self { inner, cancel }
    }
}

impl<R: Read> Read for CancelRead<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.fired() {
            return Err(io::Error::new(
                CANCEL_READ_MARKER,
                "in-process parse cancelled",
            ));
        }
        let n = self.inner.read(buf)?;
        THREAD_READ_COUNTER.with(|c| c.record(n));
        Ok(n)
    }
}

pub(crate) struct ThreadReadCounter {
    active: std::cell::Cell<bool>,
    bytes: std::cell::Cell<u64>,
}

impl ThreadReadCounter {
    #[cfg(test)]
    pub(crate) fn start(&self) {
        self.bytes.set(0);
        self.active.set(true);
    }

    #[cfg(test)]
    pub(crate) fn stop(&self) -> u64 {
        self.active.set(false);
        self.bytes.get()
    }

    fn record(&self, n: usize) {
        if self.active.get() {
            self.bytes.set(self.bytes.get() + n as u64);
        }
    }
}

thread_local! {
    pub(crate) static THREAD_READ_COUNTER: ThreadReadCounter = const {
        ThreadReadCounter {
            active: std::cell::Cell::new(false),
            bytes: std::cell::Cell::new(0),
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn fired() -> Cancel<'static> {
        static FLAG: AtomicBool = AtomicBool::new(true);
        FLAG.store(true, Ordering::Relaxed);
        Cancel {
            pause: Some(&FLAG),
            removal: None,
        }
    }

    #[test]
    fn pre_fired_cancel_reads_zero_bytes() {
        let mut src = std::io::repeat(0u8).take(1024 * 1024);
        let mut reader = CancelRead::new(&mut src, fired());
        let mut buf = [0u8; 4096];
        let err = reader
            .read(&mut buf)
            .expect_err("pre-fired cancel must error");
        assert_eq!(err.kind(), CANCEL_READ_MARKER);
    }

    #[test]
    fn never_fired_cancel_delegates_every_byte() {
        let data = vec![7u8; 4096];
        let mut src = std::io::Cursor::new(data.clone());
        let mut reader = CancelRead::new(&mut src, Cancel::default());
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn mid_stream_fire_short_circuits_remaining_reads() {
        static FLAG: AtomicBool = AtomicBool::new(false);
        FLAG.store(false, Ordering::Relaxed);
        let cancel = Cancel {
            pause: Some(&FLAG),
            removal: None,
        };
        let data = vec![9u8; 4096];
        let mut src = std::io::Cursor::new(data);
        let mut reader = CancelRead::new(&mut src, cancel);

        let mut buf = [0u8; 100];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(n, 100, "first read must succeed before cancel fires");

        FLAG.store(true, Ordering::Relaxed);
        let err = reader
            .read(&mut buf)
            .expect_err("read after cancel fires must error, not return more bytes");
        assert_eq!(err.kind(), CANCEL_READ_MARKER);
    }
}
