use std::time::Instant;

use vidcull_core::types::FileId;
use vidcull_matcher::near::{LshIndex, LshParams};

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[test]
fn query_against_ten_thousand_corpus_is_fast_and_correct() {
    const CORPUS: i64 = 10_000;
    let params = LshParams::default();

    let mut state = 0xABCD_1234_5678_9F01u64;
    let mut items: Vec<(FileId, u64)> = (1..=CORPUS)
        .map(|i| (FileId(i), splitmix64(&mut state) | 1))
        .collect();

    let query = 0x0123_4567_89AB_CDEFu64;
    let planted = FileId(CORPUS + 1);
    items.push((planted, query ^ 0b111));

    let index = LshIndex::build(items, params);
    assert_eq!(
        index.len(),
        usize::try_from(CORPUS).expect("corpus size fits usize") + 1
    );

    let start = Instant::now();
    let hits = index.query(query);
    let elapsed = start.elapsed();

    assert!(
        hits.iter().any(|m| m.file_id == planted),
        "planted near-duplicate must be found",
    );
    assert!(
        elapsed.as_millis() < 200,
        "10k-corpus query took {elapsed:?}, expected < 200ms",
    );
}
