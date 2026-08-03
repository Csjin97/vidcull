use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::Path;

use vidcull_core::{Error, Result};

use crate::ebml::{read_element_header, read_uint, skip_bytes};

const ID_EBML_HEADER: u32 = 0x1A45_DFA3;
const ID_SEGMENT: u32 = 0x1853_8067;
const ID_CUES: u32 = 0x1C53_BB6B;
const ID_CUE_POINT: u32 = 0xBB;
const ID_CUE_TIME: u32 = 0xB3;
const ID_CUE_TRACK_POSITIONS: u32 = 0xB7;
const ID_CUE_TRACK: u32 = 0xF7;
const ID_CUE_CLUSTER_POSITION: u32 = 0xF1;
const ID_CUE_RELATIVE_POSITION: u32 = 0xF0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keyframe {
    pub cue_index: u32,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gop {
    pub start_cue_index: u32,
    pub start_timestamp_ms: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MkvIndex {
    pub timestamp_scale_ns: u64,
    pub video_track_number: u64,
    pub codec_private: Option<Vec<u8>>,
    pub segment_data_start: u64,
    pub cue_positions: Vec<(u64, u64)>,
    pub segment_duration_ms: Option<u64>,
    pub keyframe_count: u32,
    pub keyframes: Vec<Keyframe>,
    pub gops: Vec<Gop>,
}

pub fn index_mkv<P: AsRef<Path>>(path: P) -> Result<MkvIndex> {
    let path = path.as_ref();

    let (timestamp_scale_ns, segment_duration_ms, video_track_number, codec_private) =
        read_segment_header(path)?;

    let mut reader = BufReader::with_capacity(crate::bounded::READ_BUF_CAPACITY, File::open(path)?);
    let (segment_data_start, cues) = walk_cues_for_track(&mut reader, video_track_number)?;

    if cues.is_empty() {
        return Err(Error::Unsupported(
            "mkv: Cues element absent or empty; live / un-indexed files belong on the \
             FFmpeg fallback path"
                .into(),
        ));
    }

    let keyframes: Vec<Keyframe> = cues
        .iter()
        .enumerate()
        .map(|(idx, cue)| {
            let cue_index = u32::try_from(idx).unwrap_or(u32::MAX);
            Keyframe {
                cue_index,
                timestamp_ms: rescale_to_ms(cue.timestamp_ticks, timestamp_scale_ns),
            }
        })
        .collect();
    let gops = build_gops(&keyframes, segment_duration_ms);

    let keyframe_count = u32::try_from(keyframes.len()).unwrap_or(u32::MAX);
    Ok(MkvIndex {
        timestamp_scale_ns,
        video_track_number,
        codec_private,
        segment_data_start,
        cue_positions: cues
            .iter()
            .map(|cue| (cue.cluster_position, cue.relative_position))
            .collect(),
        segment_duration_ms,
        keyframe_count,
        keyframes,
        gops,
    })
}

fn read_segment_header(path: &Path) -> Result<(u64, Option<u64>, u64, Option<Vec<u8>>)> {
    let reader = BufReader::with_capacity(crate::bounded::READ_BUF_CAPACITY, File::open(path)?);
    let mkv = matroska_demuxer::MatroskaFile::open(reader)
        .map_err(|e| Error::Parse(format!("matroska: {e}")))?;

    let info = mkv.info();
    let timestamp_scale_ns = info.timestamp_scale().get();
    let segment_duration_ms = info
        .duration()
        .and_then(|d| duration_ticks_to_ms(d, timestamp_scale_ns));

    let video = mkv
        .tracks()
        .iter()
        .find(|t| matches!(t.track_type(), matroska_demuxer::TrackType::Video))
        .ok_or_else(|| Error::Parse("mkv: no video track found".into()))?;
    let video_track_number = video.track_number().get();
    let codec_private = video.codec_private().map(<[u8]>::to_vec);

    Ok((
        timestamp_scale_ns,
        segment_duration_ms,
        video_track_number,
        codec_private,
    ))
}

const MAX_REPRESENTABLE_MS_F64: f64 = 9_223_372_036_854_775_808.0;

fn duration_ticks_to_ms(ticks: f64, timestamp_scale_ns: u64) -> Option<u64> {
    if !ticks.is_finite() || ticks < 0.0 {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let ns = ticks * timestamp_scale_ns as f64;
    let ms = (ns / 1_000_000.0).round();
    if !(0.0..MAX_REPRESENTABLE_MS_F64).contains(&ms) {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(ms as u64)
}

struct CueRecord {
    timestamp_ticks: u64,
    cluster_position: u64,
    relative_position: u64,
}

fn walk_cues_for_track<R: Read + Seek>(
    reader: &mut R,
    target_track: u64,
) -> Result<(u64, Vec<CueRecord>)> {
    let (id, size) = read_element_header(reader)?;
    if id != ID_EBML_HEADER {
        return Err(Error::Parse(format!(
            "ebml: expected EBML header (0x{ID_EBML_HEADER:08X}), got 0x{id:08X}"
        )));
    }
    skip_bytes(reader, size)?;

    let (id, segment_size) = read_element_header(reader)?;
    if id != ID_SEGMENT {
        return Err(Error::Parse(format!(
            "ebml: expected Segment (0x{ID_SEGMENT:08X}), got 0x{id:08X}"
        )));
    }
    let segment_data_start = reader.stream_position()?;
    let segment_end = if segment_size == u64::MAX {
        None
    } else {
        Some(segment_data_start.saturating_add(segment_size))
    };

    loop {
        if let Some(end) = segment_end {
            if reader.stream_position()? >= end {
                break;
            }
        }
        match read_element_header(reader) {
            Ok((child_id, child_size)) => {
                if child_id == ID_CUES {
                    return Ok((
                        segment_data_start,
                        parse_cues_master(reader, child_size, target_track)?,
                    ));
                }
                skip_bytes(reader, child_size)?;
            }
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
    }

    Ok((segment_data_start, Vec::new()))
}

fn parse_cues_master<R: Read + Seek>(
    reader: &mut R,
    cues_size: u64,
    target_track: u64,
) -> Result<Vec<CueRecord>> {
    if cues_size == u64::MAX {
        return Err(Error::Parse(
            "ebml: Cues element has unknown size; cannot bound walk".into(),
        ));
    }
    let cues_start = reader.stream_position()?;
    let cues_end = cues_start
        .checked_add(cues_size)
        .ok_or_else(|| Error::Parse("ebml: Cues size + offset overflows u64".into()))?;

    let mut cues = Vec::new();
    while reader.stream_position()? < cues_end {
        let (child_id, child_size) = read_element_header(reader)?;
        if child_id == ID_CUE_POINT {
            if let Some(cue) = parse_cue_point(reader, child_size, target_track)? {
                cues.push(cue);
            }
        } else {
            skip_bytes(reader, child_size)?;
        }
    }
    Ok(cues)
}

fn parse_cue_point<R: Read + Seek>(
    reader: &mut R,
    cue_point_size: u64,
    target_track: u64,
) -> Result<Option<CueRecord>> {
    if cue_point_size == u64::MAX {
        return Err(Error::Parse(
            "ebml: CuePoint has unknown size; cannot bound walk".into(),
        ));
    }
    let start = reader.stream_position()?;
    let end = start
        .checked_add(cue_point_size)
        .ok_or_else(|| Error::Parse("ebml: CuePoint size + offset overflows u64".into()))?;

    let mut cue_time: Option<u64> = None;
    let mut matched_positions = None;

    while reader.stream_position()? < end {
        let (child_id, child_size) = read_element_header(reader)?;
        match child_id {
            ID_CUE_TIME => {
                cue_time = Some(read_uint(reader, child_size)?);
            }
            ID_CUE_TRACK_POSITIONS => {
                if let Some(positions) =
                    parse_cue_track_positions(reader, child_size, target_track)?
                {
                    matched_positions = Some(positions);
                }
            }
            _ => skip_bytes(reader, child_size)?,
        }
    }

    if let Some((cluster_position, relative_position)) = matched_positions {
        let t = cue_time.ok_or_else(|| {
            Error::Parse("ebml: CuePoint missing required CueTime element".into())
        })?;
        return Ok(Some(CueRecord {
            timestamp_ticks: t,
            cluster_position,
            relative_position,
        }));
    }
    Ok(None)
}

fn parse_cue_track_positions<R: Read + Seek>(
    reader: &mut R,
    positions_size: u64,
    target_track: u64,
) -> Result<Option<(u64, u64)>> {
    if positions_size == u64::MAX {
        return Err(Error::Parse(
            "ebml: CueTrackPositions has unknown size".into(),
        ));
    }
    let start = reader.stream_position()?;
    let end = start.checked_add(positions_size).ok_or_else(|| {
        Error::Parse("ebml: CueTrackPositions size + offset overflows u64".into())
    })?;

    let mut track = None;
    let mut cluster_position = None;
    let mut relative_position = 0;
    while reader.stream_position()? < end {
        let (child_id, child_size) = read_element_header(reader)?;
        match child_id {
            ID_CUE_TRACK => track = Some(read_uint(reader, child_size)?),
            ID_CUE_CLUSTER_POSITION => {
                cluster_position = Some(read_uint(reader, child_size)?);
            }
            ID_CUE_RELATIVE_POSITION => relative_position = read_uint(reader, child_size)?,
            _ => skip_bytes(reader, child_size)?,
        }
    }
    if track != Some(target_track) {
        return Ok(None);
    }
    let cluster_position = cluster_position
        .ok_or_else(|| Error::Parse("ebml: CueTrackPositions missing CueClusterPosition".into()))?;
    Ok(Some((cluster_position, relative_position)))
}

fn rescale_to_ms(ticks: u64, timestamp_scale_ns: u64) -> u64 {
    if timestamp_scale_ns == 0 {
        return 0;
    }
    let ns = u128::from(ticks).saturating_mul(u128::from(timestamp_scale_ns));
    let ms = (ns + 500_000) / 1_000_000;
    u64::try_from(ms).unwrap_or(u64::MAX)
}

fn build_gops(keyframes: &[Keyframe], segment_duration_ms: Option<u64>) -> Vec<Gop> {
    let mut gops = Vec::with_capacity(keyframes.len());
    for (idx, kf) in keyframes.iter().enumerate() {
        let next_ts = if idx + 1 < keyframes.len() {
            keyframes[idx + 1].timestamp_ms
        } else {
            segment_duration_ms.unwrap_or(kf.timestamp_ms)
        };
        gops.push(Gop {
            start_cue_index: kf.cue_index,
            start_timestamp_ms: kf.timestamp_ms,
            duration_ms: next_ts.saturating_sub(kf.timestamp_ms),
        });
    }
    gops
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rescale_passes_through_when_ts_scale_is_one_ms() {
        assert_eq!(rescale_to_ms(0, 1_000_000), 0);
        assert_eq!(rescale_to_ms(1000, 1_000_000), 1000);
    }

    #[test]
    fn rescale_rounds_half_up() {
        assert_eq!(rescale_to_ms(1, 500_000), 1);
        assert_eq!(rescale_to_ms(1, 333_333), 0);
    }

    #[test]
    fn rescale_zero_timescale_returns_zero_not_div_by_zero() {
        assert_eq!(rescale_to_ms(1000, 0), 0);
    }

    #[test]
    fn build_gops_single_keyframe_spans_whole_segment() {
        let kfs = vec![Keyframe {
            cue_index: 0,
            timestamp_ms: 0,
        }];
        let gops = build_gops(&kfs, Some(1000));
        assert_eq!(
            gops,
            vec![Gop {
                start_cue_index: 0,
                start_timestamp_ms: 0,
                duration_ms: 1000,
            }]
        );
    }

    #[test]
    fn build_gops_splits_between_consecutive_idrs() {
        let kfs = vec![
            Keyframe {
                cue_index: 0,
                timestamp_ms: 0,
            },
            Keyframe {
                cue_index: 1,
                timestamp_ms: 500,
            },
            Keyframe {
                cue_index: 2,
                timestamp_ms: 800,
            },
        ];
        let gops = build_gops(&kfs, Some(1000));
        assert_eq!(gops.len(), 3);
        assert_eq!(gops[0].duration_ms, 500);
        assert_eq!(gops[1].duration_ms, 300);
        assert_eq!(gops[2].duration_ms, 200);
    }

    #[test]
    fn build_gops_handles_unknown_segment_duration_for_last_gop() {
        let kfs = vec![Keyframe {
            cue_index: 0,
            timestamp_ms: 500,
        }];
        let gops = build_gops(&kfs, None);
        assert_eq!(gops[0].duration_ms, 0);
    }

    #[test]
    fn duration_ticks_to_ms_handles_default_scale() {
        assert_eq!(duration_ticks_to_ms(1000.0, 1_000_000), Some(1000));
        assert_eq!(duration_ticks_to_ms(0.0, 1_000_000), Some(0));
    }

    #[test]
    fn duration_ticks_to_ms_rejects_non_finite() {
        assert_eq!(duration_ticks_to_ms(f64::NAN, 1_000_000), None);
        assert_eq!(duration_ticks_to_ms(f64::INFINITY, 1_000_000), None);
        assert_eq!(duration_ticks_to_ms(-1.0, 1_000_000), None);
    }
}
