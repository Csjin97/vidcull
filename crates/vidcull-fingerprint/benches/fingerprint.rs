#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
#![allow(missing_docs)]

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use vidcull_core::{Codec, VideoDuration};
use vidcull_fingerprint::format::{decode_tier2, encode_tier2};
use vidcull_fingerprint::tier1::phash_frames;
use vidcull_fingerprint::{
    GrayFrame, SceneHash, Tier1Builder, Tier2Builder, Tier2Fingerprint, TimedFrame, build_tier1,
    build_tier2, dct_energy, hamming_distance, hamming_distance_batch, sequence_similarity,
};

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn synth_pixels(width: u32, height: u32, seed: u64) -> Vec<u8> {
    let n = width as usize * height as usize;
    let mut state = seed;
    let mut px = vec![0u8; n];
    for p in &mut px {
        *p = (splitmix64(&mut state) & 0xFF) as u8;
    }
    px
}

fn synth_tier2(scenes: usize, seed: u64) -> Tier2Fingerprint {
    let mut state = seed;
    Tier2Fingerprint {
        scenes: (0..scenes)
            .map(|i| SceneHash {
                timestamp_ms: i as u64 * 1000,
                phash: splitmix64(&mut state),
            })
            .collect(),
    }
}

fn bench_phash(c: &mut Criterion) {
    let (w, h) = (1280u32, 720u32);
    let mut group = c.benchmark_group("phash_frames");
    for count in [1usize, 8] {
        let buffers: Vec<Vec<u8>> = (0..count)
            .map(|i| synth_pixels(w, h, 0x00A1_1CE5 + i as u64))
            .collect();
        let frames: Vec<GrayFrame<'_>> = buffers
            .iter()
            .map(|b| GrayFrame {
                width: w,
                height: h,
                pixels: b,
            })
            .collect();
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &frames, |b, frames| {
            b.iter(|| phash_frames(black_box(frames)));
        });
    }
    group.finish();
}

fn bench_build_tier1(c: &mut Criterion) {
    let (w, h) = (1280u32, 720u32);
    let buffers: Vec<Vec<u8>> = (0..8)
        .map(|i| synth_pixels(w, h, 0x00B0_0B00 + i as u64))
        .collect();
    let frames: Vec<GrayFrame<'_>> = buffers
        .iter()
        .map(|b| GrayFrame {
            width: w,
            height: h,
            pixels: b,
        })
        .collect();
    let gop: Vec<u64> = (0..32).map(|i| 400 + (i % 5) * 50).collect();
    c.bench_function("build_tier1", |b| {
        b.iter(|| {
            build_tier1(
                black_box(VideoDuration::from_millis(3_600_000)),
                black_box(Codec::H264),
                black_box(&gop),
                black_box(&frames),
            )
        });
    });
}

fn bench_build_tier2(c: &mut Criterion) {
    let (w, h) = (640u32, 360u32);
    let mut group = c.benchmark_group("build_tier2");
    for scenes in [60usize, 240] {
        let buffers: Vec<Vec<u8>> = (0..scenes)
            .map(|i| synth_pixels(w, h, 0x00C0_FFEE + i as u64))
            .collect();
        let timed: Vec<TimedFrame<'_>> = buffers
            .iter()
            .enumerate()
            .map(|(i, b)| TimedFrame {
                timestamp_ms: i as u64 * 1000,
                frame: GrayFrame {
                    width: w,
                    height: h,
                    pixels: b,
                },
            })
            .collect();
        group.throughput(Throughput::Elements(scenes as u64));
        group.bench_with_input(BenchmarkId::from_parameter(scenes), &timed, |b, timed| {
            b.iter(|| build_tier2(black_box(timed)));
        });
    }
    group.finish();
}

fn bench_combined_tier1_tier2(c: &mut Criterion) {
    let (w, h) = (1280u32, 720u32);
    let buffers: Vec<Vec<u8>> = (0..8)
        .map(|i| synth_pixels(w, h, 0x00D0_0D00 + i as u64))
        .collect();
    let frames: Vec<GrayFrame<'_>> = buffers
        .iter()
        .map(|pixels| GrayFrame {
            width: w,
            height: h,
            pixels,
        })
        .collect();

    let mut group = c.benchmark_group("tier1_tier2_streaming");
    group.throughput(Throughput::Elements(frames.len() as u64));
    group.bench_function("separate_downscale", |b| {
        b.iter(|| {
            let mut tier1 = Tier1Builder::new();
            let mut tier2 = Tier2Builder::new();
            let mut energy = 0.0;
            for (i, frame) in black_box(&frames).iter().enumerate() {
                tier1.push(frame);
                tier2.push(&TimedFrame {
                    timestamp_ms: i as u64 * 2500,
                    frame: *frame,
                });
                energy += dct_energy(frame);
            }
            black_box((tier1.finish(), tier2.finish(), energy))
        });
    });
    group.bench_function("shared_downscale", |b| {
        b.iter(|| {
            let mut tier1 = Tier1Builder::new();
            let mut tier2 = Tier2Builder::new();
            let mut energy = 0.0;
            for (i, frame) in black_box(&frames).iter().enumerate() {
                if let Some((phash, frame_energy)) = tier1.push_and_analyze(frame) {
                    tier2.push_phash(i as u64 * 2500, phash);
                    energy += frame_energy;
                }
            }
            black_box((tier1.finish(), tier2.finish(), energy))
        });
    });
    group.finish();
}

fn bench_sequence_similarity(c: &mut Criterion) {
    let a = synth_tier2(600, 1);
    let b = synth_tier2(600, 2);
    c.bench_function("sequence_similarity/600", |bn| {
        bn.iter(|| sequence_similarity(black_box(&a), black_box(&b)));
    });
}

fn bench_serialization(c: &mut Criterion) {
    let fp = synth_tier2(600, 7);
    let bytes = encode_tier2(&fp).expect("encode");
    let mut group = c.benchmark_group("tier2_serialization");
    group.bench_function("encode/600", |b| {
        b.iter(|| encode_tier2(black_box(&fp)).expect("encode"));
    });
    group.bench_function("decode/600", |b| {
        b.iter(|| decode_tier2(black_box(&bytes)).expect("decode"));
    });
    group.finish();
}

fn bench_hamming(c: &mut Criterion) {
    c.bench_function("hamming_distance", |b| {
        b.iter(|| {
            hamming_distance(
                black_box(0x0123_4567_89AB_CDEF),
                black_box(0xFEDC_BA98_7654_3210),
            )
        });
    });
}

fn bench_hamming_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("hamming_corpus");
    for n in [256usize, 4096] {
        let mut state = 0x00B7_51D0_0000_0001;
        let hashes: Vec<u64> = (0..n).map(|_| splitmix64(&mut state)).collect();
        let query = splitmix64(&mut state);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::new("scalar_loop", n), &hashes, |b, hashes| {
            b.iter(|| {
                let mut acc = 0u32;
                for &h in hashes {
                    acc += hamming_distance(black_box(query), black_box(h));
                }
                acc
            });
        });
        group.bench_with_input(BenchmarkId::new("simd_batch", n), &hashes, |b, hashes| {
            b.iter(|| hamming_distance_batch(black_box(query), black_box(hashes)));
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_phash,
    bench_build_tier1,
    bench_build_tier2,
    bench_combined_tier1_tier2,
    bench_sequence_similarity,
    bench_serialization,
    bench_hamming,
    bench_hamming_batch
);
criterion_main!(benches);
