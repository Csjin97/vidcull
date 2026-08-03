use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};

use vidcull_core::{Error, Result};

#[derive(Debug)]
struct ConcState {
    capacity: usize,
    in_use: usize,
    held_by_session: HashMap<u64, usize>,
    waiting_by_session: HashMap<u64, usize>,
}

#[derive(Debug)]
pub struct DecodeConcurrency {
    state: Mutex<ConcState>,
    cv: Condvar,
    next_session: AtomicU64,
    waiters: AtomicUsize,
}

impl DecodeConcurrency {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(ConcState {
                capacity: capacity.max(1),
                in_use: 0,
                held_by_session: HashMap::new(),
                waiting_by_session: HashMap::new(),
            }),
            cv: Condvar::new(),
            next_session: AtomicU64::new(0),
            waiters: AtomicUsize::new(0),
        }
    }

    #[must_use]
    pub fn serial() -> Self {
        Self::new(1)
    }

    pub fn set_capacity(&self, capacity: usize) {
        let mut guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        guard.capacity = capacity.max(1);
        self.cv.notify_all();
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .capacity
    }

    #[must_use]
    pub fn snapshot(&self) -> (usize, usize) {
        let g = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        (g.in_use, g.capacity)
    }

    #[must_use]
    pub fn waiters(&self) -> usize {
        self.waiters.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn new_session(&self) -> DecodeSession {
        DecodeSession {
            id: self.next_session.fetch_add(1, Ordering::Relaxed),
        }
    }

    pub fn acquire(&self) -> DecodePermit<'_> {
        let mut guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if guard.in_use < guard.capacity {
                guard.in_use += 1;
                return DecodePermit {
                    conc: self,
                    session: None,
                };
            }
            self.waiters.fetch_add(1, Ordering::Relaxed);
            guard = self.cv.wait(guard).unwrap_or_else(PoisonError::into_inner);
            self.waiters.fetch_sub(1, Ordering::Relaxed);
        }
    }

    pub fn acquire_fair(&self, session: &DecodeSession) -> DecodePermit<'_> {
        let mut guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if guard.in_use < guard.capacity {
                let held = guard.held_by_session.get(&session.id).copied().unwrap_or(0);
                let own_waiting = guard
                    .waiting_by_session
                    .get(&session.id)
                    .copied()
                    .unwrap_or(0);
                let total_waiting = self.waiters.load(Ordering::Relaxed);
                if should_yield_for_fair_share(held, guard.capacity, own_waiting, total_waiting) {
                    tracing::debug!(
                        stage = "decode_conc_fair_share",
                        session_id = session.id,
                        held,
                        capacity = guard.capacity,
                        own_waiting,
                        total_waiting,
                        "fallback decode fan-out yielding a free permit to a waiting \
                         sibling file",
                    );
                } else {
                    guard.in_use += 1;
                    *guard.held_by_session.entry(session.id).or_insert(0) += 1;
                    return DecodePermit {
                        conc: self,
                        session: Some(session.id),
                    };
                }
            }
            self.waiters.fetch_add(1, Ordering::Relaxed);
            *guard.waiting_by_session.entry(session.id).or_insert(0) += 1;
            guard = self.cv.wait(guard).unwrap_or_else(PoisonError::into_inner);
            self.waiters.fetch_sub(1, Ordering::Relaxed);
            if let Some(w) = guard.waiting_by_session.get_mut(&session.id) {
                *w = w.saturating_sub(1);
                if *w == 0 {
                    guard.waiting_by_session.remove(&session.id);
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct DecodeSession {
    id: u64,
}

fn should_yield_for_fair_share(
    held: usize,
    capacity: usize,
    own_waiting: usize,
    total_waiting: usize,
) -> bool {
    let fair_share = capacity.div_ceil(2).max(1);
    let waiting_other = total_waiting.saturating_sub(own_waiting);
    held >= fair_share && waiting_other > 0
}

pub struct DecodePermit<'a> {
    conc: &'a DecodeConcurrency,
    session: Option<u64>,
}

impl Drop for DecodePermit<'_> {
    fn drop(&mut self) {
        let mut guard = self
            .conc
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        guard.in_use = guard.in_use.saturating_sub(1);
        if let Some(id) = self.session {
            if let Some(held) = guard.held_by_session.get_mut(&id) {
                *held = held.saturating_sub(1);
                if *held == 0 {
                    guard.held_by_session.remove(&id);
                }
            }
        }
        self.conc.cv.notify_one();
    }
}

pub(crate) fn fan_out_indexed<T, R, F>(
    items: &[T],
    conc: &DecodeConcurrency,
    decode: F,
) -> Result<Vec<R>>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> Result<R> + Sync,
{
    let n = items.len();
    if n == 0 {
        return Ok(Vec::new());
    }
    let cells: Vec<Mutex<Option<Result<R>>>> = (0..n).map(|_| Mutex::new(None)).collect();
    let counter = AtomicUsize::new(0);
    let abort = AtomicBool::new(false);
    let workers = n.min(conc.capacity()).max(1);

    std::thread::scope(|s| {
        for _ in 0..workers {
            let counter = &counter;
            let abort = &abort;
            let cells = &cells;
            let decode = &decode;
            s.spawn(move || {
                loop {
                    if abort.load(Ordering::Relaxed) {
                        break;
                    }
                    let idx = counter.fetch_add(1, Ordering::Relaxed);
                    if idx >= n {
                        break;
                    }
                    let _permit = conc.acquire();
                    let result = decode(&items[idx]);
                    if result.is_err() {
                        abort.store(true, Ordering::Relaxed);
                    }
                    *cells[idx].lock().unwrap_or_else(PoisonError::into_inner) = Some(result);
                }
            });
        }
    });

    let mut out = Vec::with_capacity(n);
    for cell in &cells {
        match cell.lock().unwrap_or_else(PoisonError::into_inner).take() {
            Some(Ok(v)) => out.push(v),
            Some(Err(e)) => return Err(e),
            None => {
                return Err(Error::Decode(
                    "fan_out_indexed: worker did not fill expected cell".into(),
                ));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn new_clamps_zero_to_one() {
        let dc = DecodeConcurrency::new(0);
        assert_eq!(dc.capacity(), 1);
    }

    #[test]
    fn serial_is_capacity_one() {
        let dc = DecodeConcurrency::serial();
        assert_eq!(dc.capacity(), 1);
    }

    #[test]
    fn set_capacity_updates_and_clamps() {
        let dc = DecodeConcurrency::new(4);
        assert_eq!(dc.capacity(), 4);
        dc.set_capacity(8);
        assert_eq!(dc.capacity(), 8);
        dc.set_capacity(0);
        assert_eq!(dc.capacity(), 1);
    }

    #[test]
    fn acquire_and_drop_restore_count() {
        let dc = DecodeConcurrency::new(2);
        let p1 = dc.acquire();
        {
            let g = dc.state.lock().unwrap();
            assert_eq!(g.in_use, 1);
        }
        let p2 = dc.acquire();
        {
            let g = dc.state.lock().unwrap();
            assert_eq!(g.in_use, 2);
        }
        drop(p1);
        {
            let g = dc.state.lock().unwrap();
            assert_eq!(g.in_use, 1);
        }
        drop(p2);
        {
            let g = dc.state.lock().unwrap();
            assert_eq!(g.in_use, 0);
        }
    }

    #[test]
    fn cap_is_respected_under_concurrent_access() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        let capacity = 2usize;
        let dc = Arc::new(DecodeConcurrency::new(capacity));
        let observed_max = Arc::new(AtomicUsize::new(0));
        let in_flight = Arc::new(AtomicUsize::new(0));

        std::thread::scope(|s| {
            for _ in 0..8 {
                let dc = Arc::clone(&dc);
                let observed_max = Arc::clone(&observed_max);
                let in_flight = Arc::clone(&in_flight);
                s.spawn(move || {
                    let _permit = dc.acquire();
                    let current = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    let mut prev = observed_max.load(Ordering::Relaxed);
                    while current > prev {
                        match observed_max.compare_exchange(
                            prev,
                            current,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => break,
                            Err(actual) => prev = actual,
                        }
                    }
                    std::thread::sleep(Duration::from_millis(5));
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });

        let max = observed_max.load(Ordering::Relaxed);
        assert!(
            max <= capacity,
            "observed {max} concurrent permits > capacity {capacity}"
        );
    }

    #[test]
    fn fan_out_indexed_is_order_preserving_across_capacities() {
        let items: Vec<usize> = (0..64).collect();
        let expected: Vec<usize> = items.iter().map(|&x| x * 2 + 1).collect();
        for cap in [1usize, 2, 3, 8, 16, 64, 128] {
            let conc = DecodeConcurrency::new(cap);
            let got = fan_out_indexed(&items, &conc, |&x| -> Result<usize> { Ok(x * 2 + 1) })
                .expect("fan_out_indexed");
            assert_eq!(got, expected, "capacity {cap} changed the output order");
        }
    }

    #[test]
    fn fan_out_indexed_empty_is_ok_empty() {
        let conc = DecodeConcurrency::new(4);
        let items: Vec<usize> = Vec::new();
        let got =
            fan_out_indexed(&items, &conc, |&x| -> Result<usize> { Ok(x) }).expect("empty ok");
        assert!(got.is_empty());
    }

    #[test]
    fn fan_out_indexed_propagates_error() {
        let items: Vec<usize> = (0..32).collect();
        for cap in [1usize, 4, 32] {
            let conc = DecodeConcurrency::new(cap);
            let r = fan_out_indexed(&items, &conc, |&x| -> Result<usize> {
                if x == 13 {
                    Err(Error::Decode("boom".into()))
                } else {
                    Ok(x)
                }
            });
            assert!(
                matches!(r, Err(Error::Decode(_))),
                "cap {cap} swallowed the error"
            );
        }
    }

    #[test]
    fn set_capacity_wakes_blocked_waiters() {
        let dc = Arc::new(DecodeConcurrency::new(1));
        let _p1 = dc.acquire();

        let dc2 = Arc::clone(&dc);
        let handle = std::thread::spawn(move || {
            let _p = dc2.acquire();
        });

        dc.set_capacity(2);
        handle.join().expect("waiter thread panicked");
    }

    #[test]
    fn fair_share_pure_never_yields_without_a_waiter() {
        assert!(!should_yield_for_fair_share(4, 4, 0, 0));
        assert!(!should_yield_for_fair_share(100, 4, 0, 0));
    }

    #[test]
    fn fair_share_pure_never_yields_below_the_share() {
        assert!(!should_yield_for_fair_share(0, 4, 0, 5));
        assert!(!should_yield_for_fair_share(1, 4, 0, 5));
    }

    #[test]
    fn fair_share_pure_yields_at_the_share_with_an_other_session_waiter() {
        assert!(should_yield_for_fair_share(2, 4, 0, 1));
        assert!(should_yield_for_fair_share(3, 4, 0, 1));
        assert!(!should_yield_for_fair_share(1, 3, 0, 1));
        assert!(should_yield_for_fair_share(2, 3, 0, 1));
    }

    #[test]
    fn fair_share_pure_never_yields_when_all_waiters_are_own_session() {
        assert!(
            !should_yield_for_fair_share(2, 4, 1, 1),
            "own_waiting == total_waiting (no OTHER-session waiter) must never yield"
        );
        assert!(
            !should_yield_for_fair_share(3, 4, 2, 2),
            "same, with multiple own-session parked threads"
        );
    }

    #[test]
    fn fair_share_pure_yields_only_for_the_other_sessions_share_of_waiters() {
        assert!(should_yield_for_fair_share(2, 4, 1, 2));
        assert!(!should_yield_for_fair_share(2, 4, 2, 2));
    }

    #[test]
    fn acquire_fair_tracks_and_releases_per_session_held_count() {
        let dc = DecodeConcurrency::new(4);
        let session = dc.new_session();
        let p1 = dc.acquire_fair(&session);
        let p2 = dc.acquire_fair(&session);
        {
            let g = dc.state.lock().unwrap();
            assert_eq!(g.held_by_session.get(&session.id), Some(&2));
        }
        drop(p1);
        {
            let g = dc.state.lock().unwrap();
            assert_eq!(g.held_by_session.get(&session.id), Some(&1));
        }
        drop(p2);
        {
            let g = dc.state.lock().unwrap();
            assert!(
                !g.held_by_session.contains_key(&session.id),
                "session entry must be removed once its held count returns to 0"
            );
        }
    }

    #[test]
    fn acquire_fair_single_session_uses_full_capacity_when_alone() {
        let capacity = 6usize;
        let dc = DecodeConcurrency::new(capacity);
        let session = dc.new_session();
        let permits: Vec<_> = (0..capacity).map(|_| dc.acquire_fair(&session)).collect();
        assert_eq!(permits.len(), capacity);
        let (in_use, cap) = dc.snapshot();
        assert_eq!(in_use, capacity);
        assert_eq!(cap, capacity);
        assert_eq!(dc.waiters(), 0, "nobody ever blocked in the solo-file case");
    }

    #[test]
    fn acquire_fair_wide_session_yields_to_a_waiting_sibling_under_contention() {
        use std::sync::atomic::AtomicBool;
        use std::time::{Duration, Instant};

        let capacity = 2usize;
        let dc = Arc::new(DecodeConcurrency::new(capacity));
        let session_a = dc.new_session();
        let held_a1 = dc.acquire_fair(&session_a);
        let held_a2 = dc.acquire_fair(&session_a);

        let stop = Arc::new(AtomicBool::new(false));
        let b_served = Arc::new(AtomicBool::new(false));

        let dc_a = Arc::clone(&dc);
        let stop_a = Arc::clone(&stop);
        let session_a_for_thread = session_a;
        let a_thread = std::thread::spawn(move || {
            while !stop_a.load(Ordering::Relaxed) {
                let p = dc_a.acquire_fair(&session_a_for_thread);
                std::thread::sleep(Duration::from_millis(1));
                drop(p);
            }
        });

        let dc_b = Arc::clone(&dc);
        let b_served_flag = Arc::clone(&b_served);
        let b_thread = std::thread::spawn(move || {
            let session_b = dc_b.new_session();
            let _p = dc_b.acquire_fair(&session_b);
            b_served_flag.store(true, Ordering::Relaxed);
        });

        drop(held_a1);
        drop(held_a2);

        let deadline = Instant::now() + Duration::from_secs(5);
        while !b_served.load(Ordering::Relaxed) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::Relaxed);
        a_thread.join().expect("A thread panicked");
        b_thread.join().expect("B thread panicked");

        assert!(
            b_served.load(Ordering::Relaxed),
            "session B (a fresh sibling file) was starved by A's continuous \
             re-acquisition — the -2 fair-share yield did not engage"
        );
    }

    #[test]
    fn acquire_fair_survivor_restores_full_capacity_after_sibling_session_exits() {
        use std::time::{Duration, Instant};

        let (tx, rx) = std::sync::mpsc::channel::<usize>();

        std::thread::spawn(move || {
            let capacity = 4usize;
            let dc = DecodeConcurrency::new(capacity);

            let session_b = dc.new_session();
            let held_b1 = dc.acquire_fair(&session_b);
            let held_b2 = dc.acquire_fair(&session_b);

            let session_a = dc.new_session();
            let held_a1 = dc.acquire_fair(&session_a);
            let held_a2 = dc.acquire_fair(&session_a);

            let a_extra_served = std::sync::atomic::AtomicUsize::new(0);

            std::thread::scope(|scope| {
                for _ in 0..2 {
                    let dc_ref = &dc;
                    let session_a_ref = &session_a;
                    let served = &a_extra_served;
                    scope.spawn(move || {
                        let _p = dc_ref.acquire_fair(session_a_ref);
                        served.fetch_add(1, Ordering::Relaxed);
                        std::thread::sleep(Duration::from_millis(50));
                    });
                }

                let deadline = Instant::now() + Duration::from_secs(2);
                while dc.waiters() < 2 && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                assert_eq!(
                    dc.waiters(),
                    2,
                    "both extra A threads must be genuinely parked"
                );

                drop(held_b1);
                drop(held_b2);
            });

            let served = a_extra_served.load(Ordering::Relaxed);
            drop(held_a1);
            drop(held_a2);
            let _ = tx.send(served);
        });

        match rx.recv_timeout(Duration::from_secs(8)) {
            Ok(served) => assert_eq!(
                served, 2,
                "session A's own extra threads were starved by its own (self) \
                 waiters after sibling session B fully exited — the HIGH-1 \
                 self-throttle bug: held could never climb past fair_share \
                 because A kept mistaking its own parked threads for a waiting \
                 sibling file"
            ),
            Err(_) => panic!(
                "HIGH-1 self-throttle bug reproduced: session A's extra fan-out \
                 threads never both acquired a permit within 8s of sibling \
                 session B fully releasing its permits — A's own parked \
                 threads were mistaken for a genuinely-waiting sibling file, \
                 permanently capping A at held=2/4 (its fair share) even \
                 though B was long gone"
            ),
        }
    }
}
