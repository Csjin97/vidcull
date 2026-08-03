#![allow(missing_docs)]

pub mod timing;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use vidcull_core::FileId;
use vidcull_fingerprint::{SceneHash, Tier2Fingerprint};
use vidcull_matcher::near::{LshIndex, LshParams};
use vidcull_matcher::partial::{AnchorIndex, AnchorParams};

static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

pub struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let live = ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        ALLOCATED.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[must_use]
pub fn current_allocated() -> usize {
    ALLOCATED.load(Ordering::Relaxed)
}

#[must_use]
pub fn peak_allocated() -> usize {
    PEAK.load(Ordering::Relaxed)
}

pub fn reset_peak() {
    PEAK.store(ALLOCATED.load(Ordering::Relaxed), Ordering::Relaxed);
}

#[derive(Debug, Clone, Copy)]
pub struct MemReport {
    pub elements: usize,
    pub retained_bytes: usize,
    pub peak_bytes: usize,
}

impl MemReport {
    #[must_use]
    pub fn retained_per_element(&self) -> f64 {
        if self.elements == 0 {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)]
        {
            self.retained_bytes as f64 / self.elements as f64
        }
    }

    #[must_use]
    pub fn peak_per_element(&self) -> f64 {
        if self.elements == 0 {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)]
        {
            self.peak_bytes as f64 / self.elements as f64
        }
    }
}

#[must_use]
pub fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[must_use]
pub fn synth_phashes(n: usize, seed: u64) -> Vec<(FileId, u64)> {
    let mut state = seed;
    (0..n)
        .map(|i| {
            let id = i64::try_from(i).unwrap_or(i64::MAX).saturating_add(1);
            (FileId(id), splitmix64(&mut state) | 1)
        })
        .collect()
}

#[must_use]
pub fn synth_corpus(videos: usize, scenes: usize, seed: u64) -> Vec<(FileId, Tier2Fingerprint)> {
    let mut state = seed;
    (0..videos)
        .map(|v| {
            let id = i64::try_from(v).unwrap_or(i64::MAX).saturating_add(1);
            let fp = Tier2Fingerprint {
                scenes: (0..scenes)
                    .map(|s| SceneHash {
                        timestamp_ms: u64::try_from(s).unwrap_or(0) * 500,
                        phash: splitmix64(&mut state) | 1,
                    })
                    .collect(),
            };
            (FileId(id), fp)
        })
        .collect()
}

#[must_use]
pub fn measure_lsh(n: usize, seed: u64) -> MemReport {
    let items = synth_phashes(n, seed);
    let before = current_allocated();
    reset_peak();
    let index = LshIndex::build(items.iter().copied(), LshParams::default());
    let retained = current_allocated().saturating_sub(before);
    let peak = peak_allocated().saturating_sub(before);
    std::hint::black_box(&index);
    std::hint::black_box(&items);
    MemReport {
        elements: n,
        retained_bytes: retained,
        peak_bytes: peak,
    }
}

#[must_use]
pub fn measure_anchor(videos: usize, scenes: usize, seed: u64) -> MemReport {
    let corpus = synth_corpus(videos, scenes, seed);
    let before = current_allocated();
    reset_peak();
    let index = AnchorIndex::build(corpus.iter().cloned(), AnchorParams::default());
    let retained = current_allocated().saturating_sub(before);
    let peak = peak_allocated().saturating_sub(before);
    std::hint::black_box(&index);
    std::hint::black_box(&corpus);
    MemReport {
        elements: videos,
        retained_bytes: retained,
        peak_bytes: peak,
    }
}

#[must_use]
pub fn measure_anchor_scoped(
    videos: usize,
    scenes: usize,
    shard_sources: usize,
    seed: u64,
) -> MemReport {
    let corpus = synth_corpus(videos, scenes, seed);
    let shard_sources = shard_sources.max(1);
    let before = current_allocated();
    reset_peak();
    for shard in corpus.chunks(shard_sources) {
        let index = AnchorIndex::build(shard.iter().cloned(), AnchorParams::default());
        std::hint::black_box(&index);
    }
    let retained = current_allocated().saturating_sub(before);
    let peak = peak_allocated().saturating_sub(before);
    std::hint::black_box(&corpus);
    MemReport {
        elements: videos,
        retained_bytes: retained,
        peak_bytes: peak,
    }
}

#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

#[cfg(test)]
mod tests {
    use super::*;

    #[global_allocator]
    static GLOBAL: CountingAllocator = CountingAllocator;

    #[test]
    fn allocator_tracks_live_bytes() {
        let _guard = TEST_LOCK.lock().unwrap();
        let before = current_allocated();
        let v: Vec<u64> = (0..4096).collect();
        let after = current_allocated();
        assert!(
            after >= before + 4096 * std::mem::size_of::<u64>(),
            "live bytes must rise by at least the Vec's payload"
        );
        drop(v);
        let freed = current_allocated();
        assert!(freed < after, "dropping the Vec must release bytes");
    }

    #[test]
    fn peak_is_at_least_retained() {
        let _guard = TEST_LOCK.lock().unwrap();
        let report = measure_lsh(5_000, 0x5EED_0001);
        assert!(report.retained_bytes > 0);
        assert!(
            report.peak_bytes >= report.retained_bytes,
            "peak {} must dominate retained {}",
            report.peak_bytes,
            report.retained_bytes
        );
    }

    #[test]
    fn lsh_retained_scales_linearly() {
        let _guard = TEST_LOCK.lock().unwrap();
        let small = measure_lsh(5_000, 0x5EED_0001);
        let large = measure_lsh(10_000, 0x5EED_0001);
        #[allow(clippy::cast_precision_loss)]
        let ratio = large.retained_bytes as f64 / small.retained_bytes as f64;
        assert!(
            (1.5..=2.5).contains(&ratio),
            "retained scaling {ratio:.2}× outside [1.5, 2.5] (small={}, large={})",
            small.retained_bytes,
            large.retained_bytes
        );
    }

    #[test]
    fn lsh_retained_per_element_is_bounded() {
        let _guard = TEST_LOCK.lock().unwrap();
        let report = measure_lsh(10_000, 0x5EED_0001);
        let per = report.retained_per_element();
        assert!(
            per < 2048.0,
            "retained {per:.1} bytes/element exceeds the 2 KiB sanity ceiling"
        );
    }

    #[test]
    fn scoped_anchor_peak_is_bounded_by_one_shard() {
        let _guard = TEST_LOCK.lock().unwrap();
        let full = measure_anchor(3_000, 60, 0x0A0C_0002);
        let one_shard = measure_anchor(1_000, 60, 0x0A0C_0002);
        let scoped = measure_anchor_scoped(3_000, 60, 1_000, 0x0A0C_0002);

        assert!(
            scoped.peak_bytes < full.peak_bytes,
            "scoped peak {} must beat the full-corpus index peak {}",
            scoped.peak_bytes,
            full.peak_bytes,
        );
        assert!(
            scoped.peak_bytes <= one_shard.peak_bytes * 3 / 2,
            "scoped peak {} exceeds 1.5× one-shard index {}",
            scoped.peak_bytes,
            one_shard.peak_bytes,
        );
        assert!(
            scoped.retained_bytes < one_shard.peak_bytes / 4,
            "scoped retained {} should be ~0 (shards dropped), not held",
            scoped.retained_bytes,
        );
    }

    #[test]
    fn corpus_generation_is_deterministic() {
        assert_eq!(synth_phashes(64, 7), synth_phashes(64, 7));
        let a = synth_corpus(8, 16, 3);
        let b = synth_corpus(8, 16, 3);
        assert_eq!(a.len(), b.len());
        for ((ida, fa), (idb, fb)) in a.iter().zip(&b) {
            assert_eq!(ida, idb);
            assert_eq!(fa.scenes, fb.scenes);
        }
    }
}
