use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use vidcull_core::types::Codec;
use vidcull_core::{Error, Result};

use vidcull_parser::mkv_index::{Gop as MkvGop, Keyframe as MkvKeyframe, MkvIndex, index_mkv};
use vidcull_parser::mp4_index::{Gop as Mp4Gop, Keyframe as Mp4Keyframe, Mp4Index, index_mp4};
use vidcull_parser::sparse::{
    GrayscaleFrame, SparseDecoder, SparsePlan, SparseSample, SparseSampleSource, SparseStep,
    decode_sparse, plan_evenly_mkv, plan_evenly_mp4,
};
use vidcull_parser::sparse_mkv::MkvSampleSource;
use vidcull_parser::sparse_mp4::Mp4SampleSource;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn synthetic_mp4_index(keyframes: &[(u32, u64)]) -> Mp4Index {
    let kfs: Vec<Mp4Keyframe> = keyframes
        .iter()
        .map(|&(sample, ts)| Mp4Keyframe {
            sample_number: sample,
            timestamp_ms: ts,
        })
        .collect();
    let gops: Vec<Mp4Gop> = kfs
        .iter()
        .map(|kf| Mp4Gop {
            start_sample: kf.sample_number,
            size: 10,
            start_timestamp_ms: kf.timestamp_ms,
            duration_ms: 333,
        })
        .collect();
    Mp4Index {
        timescale: 15_360,
        sample_count: 300,
        keyframes: kfs,
        gops,
    }
}

fn synthetic_mkv_index(keyframes: &[u64]) -> MkvIndex {
    let kfs: Vec<MkvKeyframe> = keyframes
        .iter()
        .enumerate()
        .map(|(i, &ts)| MkvKeyframe {
            cue_index: u32::try_from(i).unwrap(),
            timestamp_ms: ts,
        })
        .collect();
    let gops: Vec<MkvGop> = kfs
        .iter()
        .map(|kf| MkvGop {
            start_cue_index: kf.cue_index,
            start_timestamp_ms: kf.timestamp_ms,
            duration_ms: 333,
        })
        .collect();
    let keyframe_count = u32::try_from(kfs.len()).unwrap();
    MkvIndex {
        timestamp_scale_ns: 1_000_000,
        video_track_number: 1,
        codec_private: None,
        segment_data_start: 0,
        cue_positions: keyframes.iter().map(|_| (0, 0)).collect(),
        segment_duration_ms: Some(10_000),
        keyframe_count,
        keyframes: kfs,
        gops,
    }
}

#[test]
fn planner_mp4_returns_empty_plan_when_budget_zero() {
    let idx = synthetic_mp4_index(&[(1, 0), (61, 2000), (121, 4000)]);
    let plan = plan_evenly_mp4(&idx, 0);
    assert!(plan.is_empty());
    assert_eq!(plan.len(), 0);
}

#[test]
fn planner_mp4_returns_empty_plan_when_index_empty() {
    let idx = synthetic_mp4_index(&[]);
    let plan = plan_evenly_mp4(&idx, 5);
    assert!(plan.is_empty());
}

#[test]
fn planner_mp4_passes_through_sample_numbers_and_timestamps() {
    let idx = synthetic_mp4_index(&[(1, 0), (61, 2500), (121, 5000), (181, 7500)]);
    let plan = plan_evenly_mp4(&idx, 4);
    assert_eq!(plan.len(), 4);
    assert_eq!(
        plan.steps,
        vec![
            SparseStep {
                timestamp_ms: 0,
                locator: 1
            },
            SparseStep {
                timestamp_ms: 2500,
                locator: 61
            },
            SparseStep {
                timestamp_ms: 5000,
                locator: 121
            },
            SparseStep {
                timestamp_ms: 7500,
                locator: 181
            },
        ]
    );
}

#[test]
fn planner_mp4_picks_only_idr_sample_numbers() {
    let idr_set = [1u32, 61, 121, 181];
    let idx = synthetic_mp4_index(&[(1, 0), (61, 2500), (121, 5000), (181, 7500)]);
    let plan = plan_evenly_mp4(&idx, 4);
    for step in &plan.steps {
        assert!(
            idr_set.contains(&step.locator),
            "planner picked locator {} which is not an IDR sample number (expected one of {:?})",
            step.locator,
            idr_set,
        );
    }
}

#[test]
fn planner_mp4_distributes_evenly_when_budget_under_population() {
    let idx = synthetic_mp4_index(&[
        (1, 0),
        (31, 1000),
        (61, 2000),
        (91, 3000),
        (121, 4000),
        (151, 5000),
        (181, 6000),
        (211, 7000),
        (241, 8000),
    ]);
    let plan = plan_evenly_mp4(&idx, 3);
    let locators: Vec<u32> = plan.steps.iter().map(|s| s.locator).collect();
    assert_eq!(locators, vec![1, 91, 181]);
}

#[test]
fn planner_mp4_anchors_on_first_idr_when_budget_is_one() {
    let idx = synthetic_mp4_index(&[(1, 0), (61, 2000), (121, 4000)]);
    let plan = plan_evenly_mp4(&idx, 1);
    assert_eq!(plan.steps.len(), 1);
    assert_eq!(plan.steps[0].locator, 1);
    assert_eq!(plan.steps[0].timestamp_ms, 0);
}

#[test]
fn planner_mp4_caps_at_keyframe_count_when_budget_exceeds() {
    let idx = synthetic_mp4_index(&[(1, 0), (61, 2500)]);
    let plan = plan_evenly_mp4(&idx, 10);
    assert_eq!(plan.steps.len(), 2);
}

#[test]
fn planner_mkv_uses_cue_index_as_locator() {
    let idx = synthetic_mkv_index(&[0, 2500, 5000, 7500]);
    let plan = plan_evenly_mkv(&idx, 4);
    assert_eq!(plan.len(), 4);
    let locators: Vec<u32> = plan.steps.iter().map(|s| s.locator).collect();
    assert_eq!(locators, vec![0, 1, 2, 3]);
    let timestamps: Vec<u64> = plan.steps.iter().map(|s| s.timestamp_ms).collect();
    assert_eq!(timestamps, vec![0, 2500, 5000, 7500]);
}

#[test]
fn planner_mkv_handles_empty_index() {
    let idx = synthetic_mkv_index(&[]);
    let plan = plan_evenly_mkv(&idx, 5);
    assert!(plan.is_empty());
}

type FetchLog = Rc<RefCell<Vec<u32>>>;
type DecodeLog = Rc<RefCell<Vec<(u64, Codec)>>>;

struct RecordingSource {
    fetched: FetchLog,
    fail_after: Option<usize>,
}

impl RecordingSource {
    fn new() -> (Self, FetchLog) {
        let fetched = Rc::new(RefCell::new(Vec::new()));
        let source = Self {
            fetched: Rc::clone(&fetched),
            fail_after: None,
        };
        (source, fetched)
    }

    fn failing_after(n: usize) -> (Self, FetchLog) {
        let (mut s, log) = Self::new();
        s.fail_after = Some(n);
        (s, log)
    }
}

impl SparseSampleSource for RecordingSource {
    fn fetch(&mut self, step: &SparseStep) -> Result<SparseSample> {
        let mut log = self.fetched.borrow_mut();
        if let Some(n) = self.fail_after {
            if log.len() >= n {
                return Err(Error::Parse(format!(
                    "synthetic source failure at fetch #{}",
                    log.len()
                )));
            }
        }
        log.push(step.locator);
        Ok(SparseSample {
            timestamp_ms: step.timestamp_ms,
            bytes: vec![u8::try_from(step.locator & 0xFF).unwrap(); 16],
        })
    }
}

struct RecordingDecoder {
    decoded: DecodeLog,
    fail_after: Option<usize>,
}

impl RecordingDecoder {
    fn new() -> (Self, DecodeLog) {
        let decoded = Rc::new(RefCell::new(Vec::new()));
        (
            Self {
                decoded: Rc::clone(&decoded),
                fail_after: None,
            },
            decoded,
        )
    }

    fn failing_after(n: usize) -> (Self, DecodeLog) {
        let (mut d, log) = Self::new();
        d.fail_after = Some(n);
        (d, log)
    }
}

impl SparseDecoder for RecordingDecoder {
    fn decode_idr(&mut self, sample: &SparseSample, codec: &Codec) -> Result<GrayscaleFrame> {
        let mut log = self.decoded.borrow_mut();
        if let Some(n) = self.fail_after {
            if log.len() >= n {
                return Err(Error::Parse(format!(
                    "synthetic decoder failure at decode #{}",
                    log.len()
                )));
            }
        }
        log.push((sample.timestamp_ms, codec.clone()));
        Ok(GrayscaleFrame {
            width: 32,
            height: 32,
            timestamp_ms: sample.timestamp_ms,
            pixels: vec![0u8; 32 * 32],
        })
    }
}

#[test]
fn driver_calls_source_then_decoder_exactly_once_per_step() {
    let plan = SparsePlan {
        steps: vec![
            SparseStep {
                timestamp_ms: 0,
                locator: 1,
            },
            SparseStep {
                timestamp_ms: 1000,
                locator: 31,
            },
            SparseStep {
                timestamp_ms: 2000,
                locator: 61,
            },
        ],
    };
    let (mut source, source_log) = RecordingSource::new();
    let (mut decoder, decoder_log) = RecordingDecoder::new();
    let frames = decode_sparse(&plan, &mut source, &mut decoder, &Codec::H264).unwrap();

    assert_eq!(frames.len(), 3);
    assert_eq!(*source_log.borrow(), vec![1u32, 31, 61]);
    let decoded_ts: Vec<u64> = decoder_log.borrow().iter().map(|(ts, _)| *ts).collect();
    assert_eq!(decoded_ts, vec![0, 1000, 2000]);
}

#[test]
fn driver_propagates_codec_argument_to_every_decoder_call() {
    let plan = SparsePlan {
        steps: vec![SparseStep {
            timestamp_ms: 0,
            locator: 1,
        }],
    };
    let (mut source, _) = RecordingSource::new();
    let (mut decoder, decoder_log) = RecordingDecoder::new();
    decode_sparse(&plan, &mut source, &mut decoder, &Codec::H265).unwrap();
    let codecs: Vec<Codec> = decoder_log
        .borrow()
        .iter()
        .map(|(_, c)| c.clone())
        .collect();
    assert_eq!(codecs, vec![Codec::H265]);
}

#[test]
fn driver_preserves_plan_order() {
    let plan = SparsePlan {
        steps: vec![
            SparseStep {
                timestamp_ms: 4000,
                locator: 121,
            },
            SparseStep {
                timestamp_ms: 1000,
                locator: 31,
            },
            SparseStep {
                timestamp_ms: 7000,
                locator: 211,
            },
        ],
    };
    let (mut source, source_log) = RecordingSource::new();
    let (mut decoder, _) = RecordingDecoder::new();
    decode_sparse(&plan, &mut source, &mut decoder, &Codec::H264).unwrap();
    assert_eq!(*source_log.borrow(), vec![121u32, 31, 211]);
}

#[test]
fn driver_short_circuits_on_source_error_and_skips_remaining() {
    let plan = SparsePlan {
        steps: vec![
            SparseStep {
                timestamp_ms: 0,
                locator: 1,
            },
            SparseStep {
                timestamp_ms: 1000,
                locator: 31,
            },
            SparseStep {
                timestamp_ms: 2000,
                locator: 61,
            },
        ],
    };
    let (mut source, source_log) = RecordingSource::failing_after(1);
    let (mut decoder, decoder_log) = RecordingDecoder::new();
    let err = decode_sparse(&plan, &mut source, &mut decoder, &Codec::H264).expect_err("must fail");
    assert!(matches!(err, Error::Parse(_)));
    assert_eq!(source_log.borrow().len(), 1);
    assert_eq!(decoder_log.borrow().len(), 1);
}

#[test]
fn driver_short_circuits_on_decoder_error() {
    let plan = SparsePlan {
        steps: vec![
            SparseStep {
                timestamp_ms: 0,
                locator: 1,
            },
            SparseStep {
                timestamp_ms: 1000,
                locator: 31,
            },
            SparseStep {
                timestamp_ms: 2000,
                locator: 61,
            },
        ],
    };
    let (mut source, source_log) = RecordingSource::new();
    let (mut decoder, decoder_log) = RecordingDecoder::failing_after(2);
    let err = decode_sparse(&plan, &mut source, &mut decoder, &Codec::H264).expect_err("must fail");
    assert!(matches!(err, Error::Parse(_)));
    assert_eq!(source_log.borrow().len(), 3);
    assert_eq!(decoder_log.borrow().len(), 2);
}

#[test]
fn driver_returns_empty_vec_for_empty_plan() {
    let plan = SparsePlan::default();
    let (mut source, source_log) = RecordingSource::new();
    let (mut decoder, decoder_log) = RecordingDecoder::new();
    let frames = decode_sparse(&plan, &mut source, &mut decoder, &Codec::H264).unwrap();
    assert!(frames.is_empty());
    assert!(source_log.borrow().is_empty());
    assert!(decoder_log.borrow().is_empty());
}

#[test]
fn mp4_source_reports_one_idr_for_single_keyframe_fixture() {
    let source = Mp4SampleSource::open(fixture("black_320x180_30fps_1s.mp4")).unwrap();
    assert_eq!(source.idr_count(), 1);
}

#[test]
fn mp4_source_extracts_avcc_framed_idr_payload() {
    let idx = index_mp4(fixture("black_320x180_30fps_1s.mp4")).unwrap();
    let plan = plan_evenly_mp4(&idx, 1);
    assert_eq!(plan.len(), 1);

    let mut source = Mp4SampleSource::open(fixture("black_320x180_30fps_1s.mp4")).unwrap();
    let sample = source.fetch(&plan.steps[0]).unwrap();

    assert_eq!(sample.timestamp_ms, 0);
    assert!(
        !sample.bytes.is_empty(),
        "IDR sample bytes must not be empty"
    );
    assert_avcc_nals_form_a_valid_partition(&sample.bytes);
    assert!(
        contains_h264_idr_slice(&sample.bytes),
        "expected at least one NAL of type 5 (IDR slice) in MP4 sample bytes"
    );
}

#[test]
fn mp4_source_rejects_locator_that_is_not_in_stss() {
    let mut source = Mp4SampleSource::open(fixture("black_320x180_30fps_1s.mp4")).unwrap();
    let bogus = SparseStep {
        timestamp_ms: 0,
        locator: 5,
    };
    let err = source
        .fetch(&bogus)
        .expect_err("locator 5 is a P-frame in this fixture");
    assert!(
        matches!(err, Error::Parse(_)),
        "non-IDR locator must surface as Parse, got {err:?}"
    );
}

#[test]
fn mp4_source_propagates_missing_file_as_io_error() {
    let err = Mp4SampleSource::open(PathBuf::from("/nonexistent/dir/missing.mp4"))
        .expect_err("missing file");
    assert!(matches!(err, Error::Io(_)), "expected Io, got {err:?}");
}

#[test]
fn mp4_source_fails_on_garbage_bytes_without_panic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("garbage.mp4");
    std::fs::write(&path, vec![0xFFu8; 4096]).unwrap();
    let err = Mp4SampleSource::open(path).expect_err("garbage must not parse");
    assert!(
        matches!(err, Error::Parse(_)),
        "garbage MP4 should surface as Parse, got {err:?}"
    );
}

#[test]
fn mkv_source_reports_one_idr_for_single_keyframe_fixture() {
    let source = MkvSampleSource::open(fixture("black_320x180_30fps_1s.mkv")).unwrap();
    assert_eq!(source.idr_count(), 1);
}

#[test]
fn mkv_source_reusing_indexed_track_is_byte_identical() {
    let path = fixture("black_320x180_30fps_1s.mkv");
    let idx = index_mkv(&path).unwrap();
    let step = &plan_evenly_mkv(&idx, 1).steps[0];
    let mut standalone = MkvSampleSource::open(&path).unwrap();
    let mut reused = MkvSampleSource::open_with_track(&path, idx.video_track_number).unwrap();

    assert_eq!(standalone.fetch(step).unwrap(), reused.fetch(step).unwrap());
}

#[test]
fn mkv_source_reusing_full_index_is_byte_identical() {
    let path = fixture("black_320x180_30fps_1s.mkv");
    let idx = index_mkv(&path).unwrap();
    let step = &plan_evenly_mkv(&idx, 1).steps[0];
    let mut standalone = MkvSampleSource::open(&path).unwrap();
    let mut reused = MkvSampleSource::open_with_index(&path, &idx).unwrap();

    assert_eq!(standalone.fetch(step).unwrap(), reused.fetch(step).unwrap());
}

#[test]
fn mkv_source_extracts_avcc_framed_idr_payload() {
    let idx = index_mkv(fixture("black_320x180_30fps_1s.mkv")).unwrap();
    let plan = plan_evenly_mkv(&idx, 1);
    assert_eq!(plan.len(), 1);

    let mut source = MkvSampleSource::open(fixture("black_320x180_30fps_1s.mkv")).unwrap();
    let sample = source.fetch(&plan.steps[0]).unwrap();

    assert_eq!(sample.timestamp_ms, 0);
    assert!(
        !sample.bytes.is_empty(),
        "IDR sample bytes must not be empty"
    );
    assert_avcc_nals_form_a_valid_partition(&sample.bytes);
    assert!(
        contains_h264_idr_slice(&sample.bytes),
        "expected at least one NAL of type 5 (IDR slice) in MKV sample bytes"
    );
}

#[test]
fn mkv_source_rejects_locator_out_of_range() {
    let mut source = MkvSampleSource::open(fixture("black_320x180_30fps_1s.mkv")).unwrap();
    let bogus = SparseStep {
        timestamp_ms: 0,
        locator: 99,
    };
    let err = source.fetch(&bogus).expect_err("only cue_index 0 exists");
    assert!(matches!(err, Error::Parse(_)), "got {err:?}");
}

#[test]
fn mkv_source_propagates_missing_file_as_io_error() {
    let err = MkvSampleSource::open(PathBuf::from("/nonexistent/dir/missing.mkv"))
        .expect_err("missing file");
    assert!(matches!(err, Error::Io(_)), "expected Io, got {err:?}");
}

fn assert_avcc_nals_form_a_valid_partition(bytes: &[u8]) {
    let mut pos = 0;
    while pos < bytes.len() {
        assert!(
            pos + 4 <= bytes.len(),
            "AVCC length prefix runs past end at pos {pos}"
        );
        let nal_len = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        assert!(nal_len > 0, "AVCC NAL has zero length at pos {pos}");
        assert!(
            pos + nal_len <= bytes.len(),
            "AVCC NAL length {nal_len} runs past end (have {} bytes left)",
            bytes.len() - pos
        );
        pos += nal_len;
    }
    assert_eq!(pos, bytes.len(), "AVCC NALs did not partition the sample");
}

fn contains_h264_idr_slice(bytes: &[u8]) -> bool {
    let mut pos = 0;
    while pos + 4 <= bytes.len() {
        let nal_len = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        if pos >= bytes.len() {
            return false;
        }
        let nal_type = bytes[pos] & 0x1F;
        if nal_type == 5 {
            return true;
        }
        pos += nal_len;
    }
    false
}
