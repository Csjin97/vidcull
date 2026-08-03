#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
#![allow(missing_docs)]

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use vidcull_core::{Codec, FileId, Resolution, types::BestCopyMode};
use vidcull_db::repo::TrustLevel;
use vidcull_fingerprint::{SceneHash, Tier2Fingerprint};
use vidcull_matcher::cluster::{GroupMembership, cluster_components};
use vidcull_matcher::near::{LshIndex, LshParams};
use vidcull_matcher::partial::{AnchorIndex, AnchorParams};
use vidcull_matcher::ranking::{score_quality, select_best};

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn synth_phashes(n: usize, seed: u64) -> Vec<(FileId, u64)> {
    let mut state = seed;
    (0..n)
        .map(|i| (FileId(i as i64 + 1), splitmix64(&mut state) | 1))
        .collect()
}

fn synth_corpus(videos: usize, scenes: usize, seed: u64) -> Vec<(FileId, Tier2Fingerprint)> {
    let mut state = seed;
    (0..videos)
        .map(|v| {
            let fp = Tier2Fingerprint {
                scenes: (0..scenes)
                    .map(|s| SceneHash {
                        timestamp_ms: s as u64 * 500,
                        phash: splitmix64(&mut state) | 1,
                    })
                    .collect(),
            };
            (FileId(v as i64 + 1), fp)
        })
        .collect()
}

fn anchor_params() -> AnchorParams {
    AnchorParams::new(16, 10, 500, 4).expect("valid anchor params")
}

fn bench_lsh_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("lsh_build");
    for n in [1_000usize, 10_000] {
        let items = synth_phashes(n, 0x5EED_0001);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &items, |b, items| {
            b.iter(|| LshIndex::build(black_box(items.iter().copied()), LshParams::default()));
        });
    }
    group.finish();
}

fn bench_lsh_query(c: &mut Criterion) {
    let items = synth_phashes(10_000, 0x5EED_0001);
    let index = LshIndex::build(items.iter().copied(), LshParams::default());
    let mut state = 0x5EED_0002u64;
    let queries: Vec<u64> = (0..256).map(|_| splitmix64(&mut state) | 1).collect();
    let mut cycle = queries.iter().copied().cycle();
    c.bench_function("lsh_query/10000", |b| {
        b.iter(|| black_box(index.query(black_box(cycle.next().expect("cycle is infinite")))));
    });
}

fn bench_anchor_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("anchor_build");
    for videos in [500usize, 2_000] {
        let corpus = synth_corpus(videos, 60, 0x0A0C_0001);
        group.throughput(Throughput::Elements(videos as u64));
        group.bench_with_input(BenchmarkId::from_parameter(videos), &corpus, |b, corpus| {
            b.iter(|| AnchorIndex::build(black_box(corpus.iter().cloned()), anchor_params()));
        });
    }
    group.finish();
}

fn bench_anchor_search(c: &mut Criterion) {
    let corpus = synth_corpus(1_000, 60, 0x0A0C_0001);
    let index = AnchorIndex::build(corpus.iter().cloned(), anchor_params());
    let clip: Vec<SceneHash> = corpus[3].1.scenes[10..30].to_vec();
    c.bench_function("anchor_search/1000x60", |b| {
        b.iter(|| black_box(index.search(black_box(&clip), None)));
    });
}

fn bench_cluster_components(c: &mut Criterion) {
    let groups: Vec<GroupMembership> = (0..5_000)
        .map(|g| GroupMembership {
            group_id: g as i64 + 1,
            trust: TrustLevel::Exact,
            members: vec![FileId(g as i64 * 2 + 1), FileId(g as i64 * 2 + 2)],
            non_transitive: false,
        })
        .collect();
    c.bench_function("cluster_components/5000", |b| {
        b.iter(|| cluster_components(black_box(&groups)));
    });
}

fn bench_ranking(c: &mut Criterion) {
    let candidates: Vec<(FileId, _)> = (0..1_000)
        .map(|i| {
            let q = score_quality(
                Some(Resolution::new(1920, 1080)),
                Some(5_000_000 + i as i64),
                Some(&Codec::H265),
                None,
                1_000_000 + i as i64,
                None,
                None,
                None,
                None,
                BestCopyMode::SpaceSaving,
            );
            (FileId(i as i64 + 1), q)
        })
        .collect();
    c.bench_function("score_quality/single", |b| {
        b.iter(|| {
            score_quality(
                black_box(Some(Resolution::new(1920, 1080))),
                black_box(Some(8_000_000)),
                black_box(Some(&Codec::H265)),
                None,
                black_box(2_000_000),
                None,
                None,
                None,
                None,
                BestCopyMode::SpaceSaving,
            )
        });
    });
    c.bench_function("select_best/1000", |b| {
        b.iter(|| select_best(black_box(candidates.iter().copied())));
    });
}

criterion_group!(
    benches,
    bench_lsh_build,
    bench_lsh_query,
    bench_anchor_build,
    bench_anchor_search,
    bench_cluster_components,
    bench_ranking
);
criterion_main!(benches);
