use vidcull_core::types::FileId;
use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};
use vidcull_matcher::partial::{AnchorParams, plan_partial_clips};

const RECALL_FLOOR: f64 = 0.95;

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn flip_low_bits(h: u64, n: u32) -> u64 {
    if n == 0 {
        return h;
    }
    let mask = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
    h ^ mask
}

fn source_seq(seed: u64, n: usize) -> Tier2Fingerprint {
    let mut state = seed;
    let scenes = (0..n)
        .map(|i| SceneHash {
            timestamp_ms: i as u64 * 1000,
            phash: splitmix64(&mut state) | 1,
        })
        .collect();
    Tier2Fingerprint { scenes }
}

fn clip_of(source: &Tier2Fingerprint, start: usize, len: usize, perturb: u32) -> Tier2Fingerprint {
    let scenes = source.scenes[start..start + len]
        .iter()
        .enumerate()
        .map(|(i, s)| SceneHash {
            timestamp_ms: i as u64 * 1000,
            phash: flip_low_bits(s.phash, perturb),
        })
        .collect();
    Tier2Fingerprint { scenes }
}

#[test]
#[allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]
fn synthetic_clip_dataset_clears_recall_floor() {
    const SOURCES: usize = 40;
    const SOURCE_LEN: usize = 30;
    const CLIPS_PER_SOURCE: usize = 4;
    const CLIP_LEN: usize = 6;

    let mut corpus: Vec<(FileId, Tier2Fingerprint)> = Vec::new();
    let mut sources: Vec<Tier2Fingerprint> = Vec::with_capacity(SOURCES);
    for s in 0..SOURCES {
        let seq = source_seq(0x00C0_FFEE + s as u64 * 7, SOURCE_LEN);
        corpus.push((FileId(s as i64 + 1), seq.clone()));
        sources.push(seq);
    }

    let mut planted: Vec<(FileId, FileId)> = Vec::new();
    let mut next_clip_id = 10_000i64;
    let mut state = 0x5EED_1234u64;
    for (s, source) in sources.iter().enumerate() {
        for _ in 0..CLIPS_PER_SOURCE {
            let start = (splitmix64(&mut state) as usize) % (SOURCE_LEN - CLIP_LEN + 1);
            let perturb = 2 + (splitmix64(&mut state) % 4) as u32;
            let clip = clip_of(source, start, CLIP_LEN, perturb);
            let clip_id = FileId(next_clip_id);
            next_clip_id += 1;
            corpus.push((clip_id, clip));
            planted.push((clip_id, FileId(s as i64 + 1)));
        }
    }

    let plan = plan_partial_clips(corpus, AnchorParams::default());

    let mut matched_source: std::collections::BTreeMap<FileId, Vec<FileId>> =
        std::collections::BTreeMap::new();
    for m in &plan.matches {
        matched_source
            .entry(m.clip)
            .or_default()
            .push(m.alignment.source);
    }

    let mut recalled = 0usize;
    let mut wrong_source = 0usize;
    for (clip_id, true_source) in &planted {
        if let Some(sources_hit) = matched_source.get(clip_id) {
            if sources_hit.contains(true_source) {
                recalled += 1;
            }
            if sources_hit.iter().any(|s| s != true_source) {
                wrong_source += 1;
            }
        }
    }

    let total = planted.len();
    let recall = recalled as f64 / total as f64;
    assert!(
        recall >= RECALL_FLOOR,
        "recall {recall:.3} ({recalled}/{total}) below floor {RECALL_FLOOR}",
    );
    assert_eq!(
        wrong_source, 0,
        "no clip may be matched to the wrong source (false positives are worse)",
    );
}
