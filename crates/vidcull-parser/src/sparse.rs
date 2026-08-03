use vidcull_core::Result;
use vidcull_core::types::Codec;

use crate::mkv_index::MkvIndex;
use crate::mp4_index::Mp4Index;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SparseSample {
    pub timestamp_ms: u64,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrayscaleFrame {
    pub width: u32,
    pub height: u32,
    pub timestamp_ms: u64,
    pub pixels: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SparseStep {
    pub timestamp_ms: u64,
    pub locator: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SparsePlan {
    pub steps: Vec<SparseStep>,
}

impl SparsePlan {
    #[must_use]
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

#[must_use]
pub fn plan_evenly_mp4(index: &Mp4Index, budget: usize) -> SparsePlan {
    if budget == 0 || index.keyframes.is_empty() {
        return SparsePlan::default();
    }
    let mut steps = Vec::new();
    let mut last_ts_ms = None;
    for kf in &index.keyframes {
        if steps.len() >= budget {
            break;
        }
        let keep = match last_ts_ms {
            None => true,
            Some(last_ts) => {
                kf.timestamp_ms.saturating_sub(last_ts) >= vidcull_core::SPARSE_GRID_INTERVAL_MS
            }
        };
        if keep {
            steps.push(SparseStep {
                timestamp_ms: kf.timestamp_ms,
                locator: kf.sample_number,
            });
            last_ts_ms = Some(kf.timestamp_ms);
        }
    }
    SparsePlan { steps }
}

#[must_use]
pub fn plan_evenly_mkv(index: &MkvIndex, budget: usize) -> SparsePlan {
    if budget == 0 || index.keyframes.is_empty() {
        return SparsePlan::default();
    }
    let mut steps = Vec::new();
    let mut last_ts_ms = None;
    for kf in &index.keyframes {
        if steps.len() >= budget {
            break;
        }
        let keep = match last_ts_ms {
            None => true,
            Some(last_ts) => {
                kf.timestamp_ms.saturating_sub(last_ts) >= vidcull_core::SPARSE_GRID_INTERVAL_MS
            }
        };
        if keep {
            steps.push(SparseStep {
                timestamp_ms: kf.timestamp_ms,
                locator: kf.cue_index,
            });
            last_ts_ms = Some(kf.timestamp_ms);
        }
    }
    SparsePlan { steps }
}

#[allow(dead_code)]
fn select_evenly(total: usize, budget: usize) -> impl Iterator<Item = usize> {
    let count = budget.min(total);
    (0..count).map(move |i| if count >= total { i } else { i * total / count })
}

pub trait SparseSampleSource {
    fn fetch(&mut self, step: &SparseStep) -> Result<SparseSample>;
}

pub trait SparseDecoder {
    fn decode_idr(&mut self, sample: &SparseSample, codec: &Codec) -> Result<GrayscaleFrame>;
}

pub fn decode_sparse_streaming<S, D, F>(
    plan: &SparsePlan,
    source: &mut S,
    decoder: &mut D,
    codec: &Codec,
    mut on_frame: F,
) -> Result<()>
where
    S: SparseSampleSource,
    D: SparseDecoder,
    F: FnMut(&GrayscaleFrame) -> Result<()>,
{
    for step in &plan.steps {
        let sample = source.fetch(step)?;
        let frame = decoder.decode_idr(&sample, codec)?;
        on_frame(&frame)?;
    }
    Ok(())
}

pub fn decode_sparse<S, D>(
    plan: &SparsePlan,
    source: &mut S,
    decoder: &mut D,
    codec: &Codec,
) -> Result<Vec<GrayscaleFrame>>
where
    S: SparseSampleSource,
    D: SparseDecoder,
{
    let mut out = Vec::with_capacity(plan.steps.len());
    for step in &plan.steps {
        let sample = source.fetch(step)?;
        out.push(decoder.decode_idr(&sample, codec)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct VecSampleSource(Vec<(u64, Vec<u8>)>);

    impl SparseSampleSource for VecSampleSource {
        fn fetch(&mut self, step: &SparseStep) -> vidcull_core::Result<SparseSample> {
            let (ts, bytes) = self.0[step.locator as usize].clone();
            Ok(SparseSample {
                timestamp_ms: ts,
                bytes,
            })
        }
    }

    struct ByteDecoder;

    impl SparseDecoder for ByteDecoder {
        fn decode_idr(
            &mut self,
            sample: &SparseSample,
            _codec: &vidcull_core::types::Codec,
        ) -> vidcull_core::Result<GrayscaleFrame> {
            let pixel = sample.bytes.first().copied().unwrap_or(0);
            Ok(GrayscaleFrame {
                width: 1,
                height: 1,
                timestamp_ms: sample.timestamp_ms,
                pixels: vec![pixel],
            })
        }
    }

    fn make_plan_and_source(entries: &[(u64, u8)]) -> (SparsePlan, VecSampleSource) {
        let steps = entries
            .iter()
            .enumerate()
            .map(|(i, &(ts, _))| SparseStep {
                timestamp_ms: ts,
                locator: u32::try_from(i).expect("test index fits u32"),
            })
            .collect();
        let backing = entries.iter().map(|&(ts, b)| (ts, vec![b])).collect();
        (SparsePlan { steps }, VecSampleSource(backing))
    }

    #[test]
    fn decode_sparse_streaming_is_byte_identical_to_decode_sparse() {
        use vidcull_core::types::Codec;

        let entries: &[(u64, u8)] = &[(0, 10), (2500, 20), (5000, 30), (7500, 40)];

        let (plan, mut src_vec) = make_plan_and_source(entries);
        let vec_frames = decode_sparse(&plan, &mut src_vec, &mut ByteDecoder, &Codec::H264)
            .expect("decode_sparse");

        let (_, mut src_stream) = make_plan_and_source(entries);
        let mut stream_frames: Vec<GrayscaleFrame> = Vec::new();
        decode_sparse_streaming(
            &plan,
            &mut src_stream,
            &mut ByteDecoder,
            &Codec::H264,
            |frame| {
                stream_frames.push(frame.clone());
                Ok(())
            },
        )
        .expect("decode_sparse_streaming");

        assert_eq!(
            vec_frames.len(),
            stream_frames.len(),
            "frame count must match"
        );
        for (i, (vf, sf)) in vec_frames.iter().zip(stream_frames.iter()).enumerate() {
            assert_eq!(
                vf, sf,
                "frame {i} differs between buffered and streaming paths"
            );
        }
    }

    #[test]
    fn decode_sparse_streaming_aborts_on_on_frame_error() {
        use vidcull_core::types::Codec;

        let entries: &[(u64, u8)] = &[(0, 1), (2500, 2), (5000, 3)];
        let (plan, mut src) = make_plan_and_source(entries);

        let mut call_count = 0usize;
        let result =
            decode_sparse_streaming(&plan, &mut src, &mut ByteDecoder, &Codec::H264, |_frame| {
                call_count += 1;
                if call_count == 2 {
                    Err(vidcull_core::Error::Decode("injected".into()))
                } else {
                    Ok(())
                }
            });

        assert!(result.is_err(), "must propagate on_frame error");
        assert_eq!(call_count, 2, "must not call on_frame after the error");
    }

    #[test]
    fn select_evenly_returns_nothing_when_budget_is_zero() {
        assert_eq!(
            select_evenly(10, 0).collect::<Vec<_>>(),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn select_evenly_returns_nothing_when_total_is_zero() {
        assert_eq!(select_evenly(0, 5).collect::<Vec<_>>(), Vec::<usize>::new());
    }

    #[test]
    fn select_evenly_returns_all_indices_when_budget_meets_population() {
        assert_eq!(select_evenly(5, 5).collect::<Vec<_>>(), vec![0, 1, 2, 3, 4]);
        assert_eq!(select_evenly(5, 8).collect::<Vec<_>>(), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn select_evenly_spreads_indices_across_population() {
        assert_eq!(select_evenly(8, 4).collect::<Vec<_>>(), vec![0, 2, 4, 6]);
        assert_eq!(select_evenly(8, 3).collect::<Vec<_>>(), vec![0, 2, 5]);
        assert_eq!(select_evenly(10, 3).collect::<Vec<_>>(), vec![0, 3, 6]);
    }

    #[test]
    fn select_evenly_always_anchors_on_first_index_when_budget_is_one() {
        assert_eq!(select_evenly(100, 1).collect::<Vec<_>>(), vec![0]);
    }
}
