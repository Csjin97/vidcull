use std::fmt;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use vidcull_core::{Error, Result};

use crate::ebml::{KeepLengthMarker, read_element_header, read_uint, read_vint, skip_bytes};
use crate::mkv_index::MkvIndex;
use crate::sparse::{SparseSample, SparseSampleSource, SparseStep};

const ID_EBML_HEADER: u32 = 0x1A45_DFA3;
const ID_SEGMENT: u32 = 0x1853_8067;
const ID_CUES: u32 = 0x1C53_BB6B;
const ID_CUE_POINT: u32 = 0xBB;
const ID_CUE_TRACK_POSITIONS: u32 = 0xB7;
const ID_CUE_TRACK: u32 = 0xF7;
const ID_CUE_CLUSTER_POSITION: u32 = 0xF1;
const ID_CUE_RELATIVE_POSITION: u32 = 0xF0;
const ID_CLUSTER: u32 = 0x1F43_B675;
const ID_SIMPLE_BLOCK: u32 = 0xA3;
const ID_BLOCK_GROUP: u32 = 0xA0;
const ID_BLOCK: u32 = 0xA1;

pub struct MkvSampleSource {
    file: BufReader<File>,
    segment_data_start: u64,
    video_track_number: u64,
    cue_positions: Vec<(u64, u64)>,
}

impl MkvSampleSource {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();

        let video_track_number = {
            let reader =
                BufReader::with_capacity(crate::bounded::READ_BUF_CAPACITY, File::open(path)?);
            let mkv = matroska_demuxer::MatroskaFile::open(reader)
                .map_err(|e| Error::Parse(format!("matroska: {e}")))?;
            let video = mkv
                .tracks()
                .iter()
                .find(|t| matches!(t.track_type(), matroska_demuxer::TrackType::Video))
                .ok_or_else(|| Error::Parse("mkv: no video track found".into()))?;
            video.track_number().get()
        };

        Self::open_with_track(path, video_track_number)
    }

    /// Opens the sparse source using track metadata already obtained by the MKV indexer.
    pub fn open_with_track<P: AsRef<Path>>(path: P, video_track_number: u64) -> Result<Self> {
        let path = path.as_ref();
        let mut reader =
            BufReader::with_capacity(crate::bounded::READ_BUF_CAPACITY, File::open(path)?);
        let segment_data_start = enter_segment(&mut reader)?;
        let cue_positions = collect_cue_positions(&mut reader, video_track_number)?;
        if cue_positions.is_empty() {
            return Err(Error::Unsupported(
                "mkv: Cues element absent or empty; live / un-indexed files belong on the \
                 FFmpeg fallback path"
                    .into(),
            ));
        }

        Ok(Self {
            file: reader,
            segment_data_start,
            video_track_number,
            cue_positions,
        })
    }

    /// Opens directly from the indexer's Cue table without walking the MKV a second time.
    pub fn open_with_index<P: AsRef<Path>>(path: P, index: &MkvIndex) -> Result<Self> {
        if index.cue_positions.is_empty() {
            return Err(Error::Unsupported("mkv: indexed Cue table is empty".into()));
        }
        Ok(Self {
            file: BufReader::with_capacity(
                crate::bounded::READ_BUF_CAPACITY,
                File::open(path.as_ref())?,
            ),
            segment_data_start: index.segment_data_start,
            video_track_number: index.video_track_number,
            cue_positions: index.cue_positions.clone(),
        })
    }

    #[must_use]
    pub fn idr_count(&self) -> usize {
        self.cue_positions.len()
    }
}

impl fmt::Debug for MkvSampleSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MkvSampleSource")
            .field("segment_data_start", &self.segment_data_start)
            .field("video_track_number", &self.video_track_number)
            .field("idr_count", &self.cue_positions.len())
            .finish_non_exhaustive()
    }
}

impl SparseSampleSource for MkvSampleSource {
    fn fetch(&mut self, step: &SparseStep) -> Result<SparseSample> {
        let idx = step.locator as usize;
        let &(cluster_position, relative_position) =
            self.cue_positions.get(idx).ok_or_else(|| {
                Error::Parse(format!(
                    "mkv: locator {} is out of range (have {} cues)",
                    step.locator,
                    self.cue_positions.len()
                ))
            })?;

        let cluster_absolute = self
            .segment_data_start
            .checked_add(cluster_position)
            .ok_or_else(|| Error::Parse("mkv: cluster offset overflow".into()))?;
        self.file.seek(SeekFrom::Start(cluster_absolute))?;
        let (cluster_id, _cluster_size) = read_element_header(&mut self.file)?;
        if cluster_id != ID_CLUSTER {
            return Err(Error::Parse(format!(
                "mkv: expected Cluster (0x{ID_CLUSTER:08X}) at CueClusterPosition, got 0x{cluster_id:08X}"
            )));
        }
        let cluster_data_start = self.file.stream_position()?;

        let block_header_at = cluster_data_start
            .checked_add(relative_position)
            .ok_or_else(|| Error::Parse("mkv: block relative offset overflow".into()))?;
        self.file.seek(SeekFrom::Start(block_header_at))?;
        let (block_id, block_size) = read_element_header(&mut self.file)?;

        let payload = match block_id {
            ID_SIMPLE_BLOCK => {
                read_block_payload(&mut self.file, block_size, self.video_track_number)?
            }
            ID_BLOCK_GROUP => {
                read_block_group_payload(&mut self.file, block_size, self.video_track_number)?
            }
            other => {
                return Err(Error::Parse(format!(
                    "mkv: expected SimpleBlock (0x{ID_SIMPLE_BLOCK:02X}) or BlockGroup \
                     (0x{ID_BLOCK_GROUP:02X}) at relative position, got 0x{other:02X}"
                )));
            }
        };

        Ok(SparseSample {
            timestamp_ms: step.timestamp_ms,
            bytes: payload,
        })
    }
}

fn enter_segment<R: Read + Seek>(reader: &mut R) -> Result<u64> {
    let (id, size) = read_element_header(reader)?;
    if id != ID_EBML_HEADER {
        return Err(Error::Parse(format!(
            "ebml: expected EBML header (0x{ID_EBML_HEADER:08X}), got 0x{id:08X}"
        )));
    }
    skip_bytes(reader, size)?;
    let (id, _segment_size) = read_element_header(reader)?;
    if id != ID_SEGMENT {
        return Err(Error::Parse(format!(
            "ebml: expected Segment (0x{ID_SEGMENT:08X}), got 0x{id:08X}"
        )));
    }
    let segment_data_start = reader.stream_position()?;
    Ok(segment_data_start)
}

fn collect_cue_positions<R: Read + Seek>(
    reader: &mut R,
    target_track: u64,
) -> Result<Vec<(u64, u64)>> {
    loop {
        match read_element_header(reader) {
            Ok((child_id, child_size)) => {
                if child_id == ID_CUES {
                    return parse_cues_master(reader, child_size, target_track);
                }
                skip_bytes(reader, child_size)?;
            }
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
    }
    Ok(Vec::new())
}

fn parse_cues_master<R: Read + Seek>(
    reader: &mut R,
    cues_size: u64,
    target_track: u64,
) -> Result<Vec<(u64, u64)>> {
    if cues_size == u64::MAX {
        return Err(Error::Parse(
            "ebml: Cues element has unknown size; cannot bound walk".into(),
        ));
    }
    let cues_start = reader.stream_position()?;
    let cues_end = cues_start
        .checked_add(cues_size)
        .ok_or_else(|| Error::Parse("ebml: Cues size + offset overflows u64".into()))?;

    let mut out = Vec::new();
    while reader.stream_position()? < cues_end {
        let (child_id, child_size) = read_element_header(reader)?;
        if child_id == ID_CUE_POINT {
            if let Some(pos) = parse_cue_point(reader, child_size, target_track)? {
                out.push(pos);
            }
        } else {
            skip_bytes(reader, child_size)?;
        }
    }
    Ok(out)
}

fn parse_cue_point<R: Read + Seek>(
    reader: &mut R,
    cue_point_size: u64,
    target_track: u64,
) -> Result<Option<(u64, u64)>> {
    if cue_point_size == u64::MAX {
        return Err(Error::Parse(
            "ebml: CuePoint has unknown size; cannot bound walk".into(),
        ));
    }
    let start = reader.stream_position()?;
    let end = start
        .checked_add(cue_point_size)
        .ok_or_else(|| Error::Parse("ebml: CuePoint size + offset overflows u64".into()))?;

    let mut matched_positions: Option<(u64, u64)> = None;

    while reader.stream_position()? < end {
        let (child_id, child_size) = read_element_header(reader)?;
        if child_id == ID_CUE_TRACK_POSITIONS {
            if let Some(pos) = parse_cue_track_positions(reader, child_size, target_track)? {
                matched_positions = Some(pos);
            }
        } else {
            skip_bytes(reader, child_size)?;
        }
    }
    Ok(matched_positions)
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

    let mut track: Option<u64> = None;
    let mut cluster_pos: Option<u64> = None;
    let mut relative_pos: u64 = 0;

    while reader.stream_position()? < end {
        let (child_id, child_size) = read_element_header(reader)?;
        match child_id {
            ID_CUE_TRACK => track = Some(read_uint(reader, child_size)?),
            ID_CUE_CLUSTER_POSITION => cluster_pos = Some(read_uint(reader, child_size)?),
            ID_CUE_RELATIVE_POSITION => relative_pos = read_uint(reader, child_size)?,
            _ => skip_bytes(reader, child_size)?,
        }
    }

    let track =
        track.ok_or_else(|| Error::Parse("ebml: CueTrackPositions missing CueTrack".into()))?;
    if track != target_track {
        return Ok(None);
    }
    let cluster_pos = cluster_pos
        .ok_or_else(|| Error::Parse("ebml: CueTrackPositions missing CueClusterPosition".into()))?;
    Ok(Some((cluster_pos, relative_pos)))
}

fn read_block_payload<R: Read + Seek>(
    reader: &mut R,
    block_size: u64,
    expected_track: u64,
) -> Result<Vec<u8>> {
    let header_start = reader.stream_position()?;
    let track_num = read_vint(reader, KeepLengthMarker::Strip)?;
    if track_num != expected_track {
        return Err(Error::Parse(format!(
            "mkv: block track {track_num} does not match video track {expected_track}"
        )));
    }
    let mut ts_bytes = [0u8; 2];
    reader.read_exact(&mut ts_bytes)?;
    let _block_ts = i16::from_be_bytes(ts_bytes);
    let mut flags = [0u8; 1];
    reader.read_exact(&mut flags)?;
    let lacing = (flags[0] >> 1) & 0x03;
    if lacing != 0 {
        return Err(Error::Unsupported(format!(
            "mkv: laced blocks (lacing mode {lacing}) are not on the fast path; H.264/H.265 \
             never use lacing"
        )));
    }
    let header_end = reader.stream_position()?;
    let header_size = header_end - header_start;
    if header_size > block_size {
        return Err(Error::Parse(format!(
            "mkv: block header size {header_size} exceeds block size {block_size}"
        )));
    }
    let frame_size_u64 = block_size - header_size;
    let frame = crate::bounded::read_exact_bounded(reader, frame_size_u64, "mkv: block frame")?;
    Ok(frame)
}

fn read_block_group_payload<R: Read + Seek>(
    reader: &mut R,
    group_size: u64,
    expected_track: u64,
) -> Result<Vec<u8>> {
    if group_size == u64::MAX {
        return Err(Error::Parse("mkv: BlockGroup has unknown size".into()));
    }
    let start = reader.stream_position()?;
    let end = start
        .checked_add(group_size)
        .ok_or_else(|| Error::Parse("mkv: BlockGroup size + offset overflows u64".into()))?;

    while reader.stream_position()? < end {
        let (child_id, child_size) = read_element_header(reader)?;
        if child_id == ID_BLOCK {
            return read_block_payload(reader, child_size, expected_track);
        }
        skip_bytes(reader, child_size)?;
    }
    Err(Error::Parse(
        "mkv: BlockGroup contained no Block element".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_parse_cue_track_positions_matching() {
        let data = vec![0xF7, 0x81, 0x02, 0xF1, 0x81, 0x64, 0xF0, 0x81, 0x0A];
        let mut cursor = Cursor::new(data);
        let res = parse_cue_track_positions(&mut cursor, 9, 2).unwrap();
        assert!(res.is_some());
        let (cluster, relative) = res.unwrap();
        assert_eq!(cluster, 100);
        assert_eq!(relative, 10);
    }

    #[test]
    fn test_parse_cue_track_positions_mismatch() {
        let data = vec![0xF7, 0x81, 0x03, 0xF1, 0x81, 0x64];
        let mut cursor = Cursor::new(data);
        let res = parse_cue_track_positions(&mut cursor, 6, 2).unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn test_parse_cue_track_positions_missing_fields() {
        let data = vec![0xF7, 0x81, 0x02];
        let mut cursor = Cursor::new(data);
        let res = parse_cue_track_positions(&mut cursor, 3, 2);
        assert!(res.is_err());
    }

    #[test]
    fn test_read_block_payload_unlaced() {
        let data = vec![0x82, 0x00, 0x00, 0x00, 0xDE, 0xAD];
        let mut cursor = Cursor::new(data);
        let payload = read_block_payload(&mut cursor, 6, 2).unwrap();
        assert_eq!(payload, vec![0xDE, 0xAD]);
    }

    #[test]
    fn test_read_block_payload_lacing_rejected() {
        let data = vec![0x82, 0x00, 0x00, 0x02, 0xDE, 0xAD];
        let mut cursor = Cursor::new(data);
        let res = read_block_payload(&mut cursor, 6, 2);
        assert!(res.is_err());
    }

    #[test]
    fn test_read_block_payload_mismatched_track() {
        let data = vec![0x83, 0x00, 0x00, 0x00, 0xDE, 0xAD];
        let mut cursor = Cursor::new(data);
        let res = read_block_payload(&mut cursor, 6, 2);
        assert!(res.is_err());
    }

    #[test]
    fn test_read_block_group_payload_success() {
        let data = vec![0xA1, 0x86, 0x82, 0x00, 0x00, 0x00, 0xDE, 0xAD];
        let mut cursor = Cursor::new(data);
        let payload = read_block_group_payload(&mut cursor, 8, 2).unwrap();
        assert_eq!(payload, vec![0xDE, 0xAD]);
    }

    #[test]
    fn test_read_block_group_payload_no_block() {
        let data = vec![0xBF, 0x81, 0x00];
        let mut cursor = Cursor::new(data);
        let res = read_block_group_payload(&mut cursor, 3, 2);
        assert!(res.is_err());
    }
}
