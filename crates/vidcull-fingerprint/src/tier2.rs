use serde::{Deserialize, Serialize};

use vidcull_core::Result;
use vidcull_core::binary;

use crate::tier1::{GrayFrame, hamming_distance, phash_frames};

pub const TIER2_BUDGET_BYTES: usize = 20 * 1024;

pub const SEQUENCE_STABILITY_THRESHOLD: f64 = 0.75;

#[derive(Debug, Clone, Copy)]
pub struct TimedFrame<'a> {
    pub timestamp_ms: u64,
    pub frame: GrayFrame<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneHash {
    pub timestamp_ms: u64,
    pub phash: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Tier2Fingerprint {
    pub scenes: Vec<SceneHash>,
}

impl Tier2Fingerprint {
    #[must_use]
    pub fn len(&self) -> usize {
        self.scenes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.scenes.is_empty()
    }

    #[must_use]
    pub fn temporal_flow(&self) -> Vec<u32> {
        self.scenes
            .windows(2)
            .map(|w| hamming_distance(w[0].phash, w[1].phash))
            .collect()
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        binary::encode(self)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        binary::decode(bytes)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Tier2Builder {
    scenes: Vec<SceneHash>,
}

impl Tier2Builder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, tf: &TimedFrame<'_>) {
        if tf.frame.width == 0 || tf.frame.height == 0 {
            return;
        }
        let phash = phash_frames(std::slice::from_ref(&tf.frame));
        self.push_phash(tf.timestamp_ms, phash);
    }

    /// Adds an already-computed frame pHash. This is used by the combined
    /// Tier 1/Tier 2 streaming path to avoid downscaling the frame twice.
    pub fn push_phash(&mut self, timestamp_ms: u64, phash: u64) {
        let keep = match self.scenes.last() {
            None => true,
            Some(last_scene) => hamming_distance(last_scene.phash, phash) > 4,
        };
        if keep {
            self.scenes.push(SceneHash {
                timestamp_ms,
                phash,
            });
        }
    }

    #[must_use]
    pub fn finish(self) -> Tier2Fingerprint {
        Tier2Fingerprint {
            scenes: self.scenes,
        }
    }
}

#[must_use]
pub fn build_tier2(frames: &[TimedFrame<'_>]) -> Tier2Fingerprint {
    let mut builder = Tier2Builder::new();
    for tf in frames {
        builder.push(tf);
    }
    builder.finish()
}

#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn sequence_similarity(a: &Tier2Fingerprint, b: &Tier2Fingerprint) -> f64 {
    let (la, lb) = (a.scenes.len(), b.scenes.len());
    if la == 0 && lb == 0 {
        return 1.0;
    }
    if la == 0 || lb == 0 {
        return 0.0;
    }
    let overlap = la.min(lb);
    let mut acc = 0.0_f64;
    for i in 0..overlap {
        let hd = hamming_distance(a.scenes[i].phash, b.scenes[i].phash);
        acc += f64::from(64 - hd) / 64.0;
    }
    let mean = acc / overlap as f64;
    let length_ratio = overlap as f64 / la.max(lb) as f64;
    mean * length_ratio
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene(ts: u64, phash: u64) -> SceneHash {
        SceneHash {
            timestamp_ms: ts,
            phash,
        }
    }

    #[track_caller]
    fn assert_score(got: f64, want: f64) {
        assert!((got - want).abs() < 1e-12, "expected {want}, got {got}");
    }

    #[test]
    fn temporal_flow_of_short_sequences_is_empty() {
        assert!(Tier2Fingerprint::default().temporal_flow().is_empty());
        let one = Tier2Fingerprint {
            scenes: vec![scene(0, 0xFF)],
        };
        assert!(one.temporal_flow().is_empty());
    }

    #[test]
    fn temporal_flow_counts_transition_bits() {
        let fp = Tier2Fingerprint {
            scenes: vec![scene(0, 0b0000), scene(1, 0b0011), scene(2, 0b0111)],
        };
        assert_eq!(fp.temporal_flow(), vec![2, 1]);
    }

    #[test]
    fn identical_sequences_score_one() {
        let fp = Tier2Fingerprint {
            scenes: vec![scene(0, 0xDEAD), scene(1, 0xBEEF)],
        };
        assert_score(sequence_similarity(&fp, &fp), 1.0);
    }

    #[test]
    fn fully_opposite_hashes_score_zero() {
        let a = Tier2Fingerprint {
            scenes: vec![scene(0, 0)],
        };
        let b = Tier2Fingerprint {
            scenes: vec![scene(0, u64::MAX)],
        };
        assert_score(sequence_similarity(&a, &b), 0.0);
    }

    #[test]
    fn prefix_overlap_scales_by_length_ratio() {
        let full = Tier2Fingerprint {
            scenes: vec![scene(0, 0xAA), scene(1, 0xBB), scene(2, 0xCC)],
        };
        let prefix = Tier2Fingerprint {
            scenes: vec![scene(0, 0xAA), scene(1, 0xBB)],
        };
        let score = sequence_similarity(&full, &prefix);
        assert!((score - 2.0 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn empty_pair_is_identical_but_empty_versus_nonempty_is_not() {
        let empty = Tier2Fingerprint::default();
        let nonempty = Tier2Fingerprint {
            scenes: vec![scene(0, 1)],
        };
        assert_score(sequence_similarity(&empty, &empty), 1.0);
        assert_score(sequence_similarity(&empty, &nonempty), 0.0);
    }

    #[test]
    fn tier2_builder_is_byte_identical_to_build_tier2() {
        use crate::format::encode_tier2;

        fn splitmix64(state: &mut u64) -> u64 {
            *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = *state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        let mut state = 0x1234_5678_9ABC_DEF0_u64;
        let (width, height) = (64_u32, 64_u32);
        let px_len = (width * height) as usize;
        let random = |state: &mut u64| -> Vec<u8> {
            (0..px_len)
                .map(|_| (splitmix64(state) & 0xFF) as u8)
                .collect()
        };
        let pix_a = random(&mut state);
        let pix_b = random(&mut state);
        let pix_c = random(&mut state);
        let empty: Vec<u8> = Vec::new();

        let frames = [
            TimedFrame {
                timestamp_ms: 0,
                frame: GrayFrame {
                    width,
                    height,
                    pixels: &pix_a,
                },
            },
            TimedFrame {
                timestamp_ms: 1,
                frame: GrayFrame {
                    width,
                    height,
                    pixels: &pix_a,
                },
            },
            TimedFrame {
                timestamp_ms: 2,
                frame: GrayFrame {
                    width,
                    height,
                    pixels: &pix_b,
                },
            },
            TimedFrame {
                timestamp_ms: 3,
                frame: GrayFrame {
                    width: 0,
                    height,
                    pixels: &empty,
                },
            },
            TimedFrame {
                timestamp_ms: 4,
                frame: GrayFrame {
                    width,
                    height,
                    pixels: &pix_c,
                },
            },
        ];

        let batch = build_tier2(&frames);
        let mut builder = Tier2Builder::new();
        for tf in &frames {
            builder.push(tf);
        }
        let streamed = builder.finish();

        assert_eq!(
            batch, streamed,
            "incremental fold diverged from build_tier2"
        );
        assert_eq!(
            encode_tier2(&batch).expect("encode batch"),
            encode_tier2(&streamed).expect("encode streamed"),
            "encoded tier2 blob not byte-identical",
        );

        let kept: Vec<u64> = batch.scenes.iter().map(|s| s.timestamp_ms).collect();
        assert_eq!(
            kept,
            vec![0, 2, 4],
            "expected distinct frames kept; duplicate + zero-dim dropped",
        );
    }
}
