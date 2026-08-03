use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use vidcull_core::types::{Codec, Resolution, VideoDuration};
use vidcull_core::{Error, Result};

use crate::cancel::{Cancel, CancelRead};
use crate::probe::{ContainerKind, VideoMetadata, fps_to_x1000, no_video_track};

pub fn read_mp4_tolerant(path: &Path) -> Result<mp4parse::MediaContext> {
    read_mp4_tolerant_cancellable(path, Cancel::default())
}

pub fn read_mp4_tolerant_cancellable(
    path: &Path,
    cancel: Cancel<'_>,
) -> Result<mp4parse::MediaContext> {
    let orig = match mp4parse::read_mp4(&mut BufReader::with_capacity(
        crate::bounded::READ_BUF_CAPACITY,
        CancelRead::new(File::open(path)?, cancel),
    )) {
        Ok(context) => return Ok(context),
        Err(e) => e,
    };
    if cancel.fired() {
        return Err(Error::Cancelled);
    }
    if let Some(boundary) = recognized_top_level_boundary(path)? {
        let mut capped = BufReader::with_capacity(
            crate::bounded::READ_BUF_CAPACITY,
            CancelRead::new(File::open(path)?, cancel),
        )
        .take(boundary);
        if let Ok(context) = mp4parse::read_mp4(&mut capped) {
            return Ok(context);
        }
        if cancel.fired() {
            return Err(Error::Cancelled);
        }
    }
    Err(Error::Parse(format!("mp4parse: {orig}")))
}

#[derive(Debug)]
pub enum PreParsedMp4 {
    NotAttempted,
    Parsed(Box<mp4parse::MediaContext>),
    MkvParsed(crate::probe::VideoMetadata),
    Failed,
    MkvFailed,
}

struct TeeRead<'s, R> {
    inner: R,
    sink: &'s mut dyn FnMut(&[u8]),
}

impl<R: Read> Read for TeeRead<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        (self.sink)(&buf[..n]);
        Ok(n)
    }
}

pub fn read_mp4_tolerant_hashing_cancellable(
    path: &Path,
    cancel: Cancel<'_>,
    sink: &mut dyn FnMut(&[u8]),
) -> Result<PreParsedMp4> {
    let mut tee = TeeRead {
        inner: CancelRead::new(File::open(path)?, cancel),
        sink,
    };
    let raw = mp4parse::read_mp4(&mut BufReader::with_capacity(
        crate::bounded::READ_BUF_CAPACITY,
        &mut tee,
    ));
    if cancel.fired() {
        return Err(Error::Cancelled);
    }
    let mut scratch = vec![0u8; 64 * 1024];
    loop {
        match tee.read(&mut scratch) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                if cancel.fired() {
                    return Err(Error::Cancelled);
                }
                return Err(e.into());
            }
        }
    }
    let orig = match raw {
        Ok(context) => return Ok(PreParsedMp4::Parsed(Box::new(context))),
        Err(e) => e,
    };
    if let Some(boundary) = recognized_top_level_boundary(path)? {
        let mut capped = BufReader::with_capacity(
            crate::bounded::READ_BUF_CAPACITY,
            CancelRead::new(File::open(path)?, cancel),
        )
        .take(boundary);
        if let Ok(context) = mp4parse::read_mp4(&mut capped) {
            return Ok(PreParsedMp4::Parsed(Box::new(context)));
        }
        if cancel.fired() {
            return Err(Error::Cancelled);
        }
    }
    tracing::debug!(reason = %orig, "fused hash+parse: mp4parse declined (hash completed)");
    Ok(PreParsedMp4::Failed)
}

fn recognized_top_level_boundary(path: &Path) -> Result<Option<u64>> {
    let mut file = File::open(path)?;
    let file_size = file.metadata()?.len();
    let mut box_start: u64 = 0;
    loop {
        file.seek(SeekFrom::Start(box_start))?;
        let mut header = [0u8; 8];
        if file.read_exact(&mut header).is_err() {
            return Ok(None);
        }
        let size32 = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        let (header_len, declared_total): (u64, u64) = match size32 {
            1 => {
                let mut ext = [0u8; 8];
                if file.read_exact(&mut ext).is_err() {
                    return Ok(None);
                }
                (16, u64::from_be_bytes(ext))
            }
            0 => return Ok(None),
            n => (8, u64::from(n)),
        };
        if declared_total < header_len {
            return Ok(None);
        }
        if box_start.saturating_add(declared_total) > file_size {
            return Ok(if box_start > 0 { Some(box_start) } else { None });
        }
        box_start = box_start.saturating_add(declared_total);
    }
}

pub fn probe_mp4(path: &Path, container: ContainerKind) -> Result<VideoMetadata> {
    probe_mp4_cancellable(path, container, Cancel::default())
}

pub fn probe_mp4_cancellable(
    path: &Path,
    container: ContainerKind,
    cancel: Cancel<'_>,
) -> Result<VideoMetadata> {
    probe_mp4_with_context_cancellable(path, container, cancel).map(|(metadata, _)| metadata)
}

pub(crate) fn probe_mp4_with_context_cancellable(
    path: &Path,
    container: ContainerKind,
    cancel: Cancel<'_>,
) -> Result<(VideoMetadata, mp4parse::MediaContext)> {
    let file_size_bytes = std::fs::metadata(path)?.len();

    let context = read_mp4_tolerant_cancellable(path, cancel)?;
    let metadata = probe_mp4_from_context(&context, path, container, file_size_bytes)?;
    Ok((metadata, context))
}

pub(crate) fn probe_mp4_from_context(
    context: &mp4parse::MediaContext,
    path: &Path,
    container: ContainerKind,
    file_size_bytes: u64,
) -> Result<VideoMetadata> {
    let video = context
        .tracks
        .iter()
        .find(|t| matches!(t.track_type, mp4parse::TrackType::Video))
        .ok_or_else(|| no_video_track("mp4"))?;

    let (codec, resolution) = match extract_codec_and_resolution(video) {
        Ok(cr) => cr,
        Err(original) => hevc_codec_and_resolution(path).map_err(|_| original)?,
    };
    let (duration_ms, frame_count) = extract_track_duration_and_samples(video);
    let duration = duration_ms.map(VideoDuration::from_millis);
    let fps_x1000 = compute_fps_x1000(duration_ms, frame_count);
    let bitrate_bps = compute_bitrate_bps(duration_ms, file_size_bytes);

    Ok(VideoMetadata {
        container,
        codec,
        resolution,
        duration,
        fps_x1000,
        has_b_frames: None,
        bitrate_bps,
        encoder_tags: None,
    })
}

pub fn extract_avcc(path: &Path) -> Result<Vec<u8>> {
    extract_avcc_cancellable(path, Cancel::default())
}

pub fn extract_avcc_cancellable(path: &Path, cancel: Cancel<'_>) -> Result<Vec<u8>> {
    let context = read_mp4_tolerant_cancellable(path, cancel)?;
    extract_avcc_from_context(&context)
}

pub(crate) fn extract_avcc_from_context(context: &mp4parse::MediaContext) -> Result<Vec<u8>> {
    let track = context
        .tracks
        .iter()
        .find(|t| matches!(t.track_type, mp4parse::TrackType::Video))
        .ok_or_else(|| Error::Parse("mp4: no video track found".into()))?;
    let stsd = track
        .stsd
        .as_ref()
        .ok_or_else(|| Error::Parse("mp4: video track has no sample description (stsd)".into()))?;
    let entry = stsd
        .descriptions
        .first()
        .ok_or_else(|| Error::Parse("mp4: stsd has no sample entries".into()))?;
    let mp4parse::SampleEntry::Video(video) = entry else {
        return Err(Error::Parse(
            "mp4: first sample entry of video track is not a video entry".into(),
        ));
    };
    match &video.codec_specific {
        mp4parse::VideoCodecSpecific::AVCConfig(avcc) => Ok(avcc.iter().copied().collect()),
        _ => Err(Error::Unsupported(
            "mp4: video sample entry carries no avcC (not H.264); native decode not applicable"
                .into(),
        )),
    }
}

pub fn extract_hvcc(path: &Path) -> Result<Vec<u8>> {
    let moov = read_moov_body(path)?;
    for (fourcc, trak) in iso_boxes(&moov) {
        if &fourcc != b"trak" {
            continue;
        }
        let Some(mdia) = find_box(trak, *b"mdia") else {
            continue;
        };
        let Some(minf) = find_box(mdia, *b"minf") else {
            continue;
        };
        let Some(stbl) = find_box(minf, *b"stbl") else {
            continue;
        };
        let Some(stsd) = find_box(stbl, *b"stsd") else {
            continue;
        };
        if stsd.len() < 8 {
            continue;
        }
        for (efourcc, entry) in iso_boxes(&stsd[8..]) {
            if &efourcc != b"hev1" && &efourcc != b"hvc1" {
                continue;
            }
            if entry.len() < 78 {
                continue;
            }
            if let Some(hvcc) = find_box(&entry[78..], *b"hvcC") {
                return Ok(hvcc.to_vec());
            }
        }
    }
    Err(Error::Unsupported(
        "mp4: no hvcC box in any video sample entry (not HEVC); native decode not applicable"
            .into(),
    ))
}

fn hevc_codec_and_resolution(path: &Path) -> Result<(Codec, Resolution)> {
    let hvcc = extract_hvcc(path)?;
    let config = crate::hevc::parse_hvcc(&hvcc)?;
    let resolution = Resolution::new(config.sps.cropped_width(), config.sps.cropped_height());
    Ok((Codec::H265, resolution))
}

fn read_moov_body(path: &Path) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    loop {
        let mut header = [0u8; 8];
        if file.read_exact(&mut header).is_err() {
            return Err(Error::Parse(
                "mp4: reached end of file without a moov box".into(),
            ));
        }
        let size32 = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        let fourcc = [header[4], header[5], header[6], header[7]];

        let (header_len, total): (u64, u64) = match size32 {
            1 => {
                let mut ext = [0u8; 8];
                file.read_exact(&mut ext)
                    .map_err(|_| Error::Parse("mp4: truncated 64-bit box size".into()))?;
                (16, u64::from_be_bytes(ext))
            }
            0 => {
                let body_start = file.stream_position()?;
                let end = file.seek(SeekFrom::End(0))?;
                file.seek(SeekFrom::Start(body_start))?;
                (8, end.saturating_sub(body_start).saturating_add(8))
            }
            n => (8, u64::from(n)),
        };
        if total < header_len {
            return Err(Error::Parse("mp4: box size smaller than its header".into()));
        }
        let body_len = total - header_len;

        if &fourcc == b"moov" {
            let body = crate::bounded::read_exact_bounded(&mut file, body_len, "mp4: moov box")?;
            return Ok(body);
        }
        let skip = i64::try_from(body_len)
            .map_err(|_| Error::Parse("mp4: box too large to skip".into()))?;
        file.seek(SeekFrom::Current(skip))?;
    }
}

fn iso_boxes(body: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 8 <= body.len() {
        let size32 = u32::from_be_bytes([body[off], body[off + 1], body[off + 2], body[off + 3]]);
        let fourcc = [body[off + 4], body[off + 5], body[off + 6], body[off + 7]];
        let (header_len, total) = match size32 {
            1 => {
                if off + 16 > body.len() {
                    break;
                }
                let large = u64::from_be_bytes([
                    body[off + 8],
                    body[off + 9],
                    body[off + 10],
                    body[off + 11],
                    body[off + 12],
                    body[off + 13],
                    body[off + 14],
                    body[off + 15],
                ]);
                (16usize, usize::try_from(large).unwrap_or(usize::MAX))
            }
            0 => (8usize, body.len() - off),
            n => (8usize, usize::try_from(n).unwrap_or(usize::MAX)),
        };
        if total < header_len || off + total > body.len() {
            break;
        }
        out.push((fourcc, &body[off + header_len..off + total]));
        off += total;
    }
    out
}

fn find_box(body: &[u8], fourcc: [u8; 4]) -> Option<&[u8]> {
    iso_boxes(body)
        .into_iter()
        .find(|(f, _)| *f == fourcc)
        .map(|(_, b)| b)
}

fn extract_codec_and_resolution(track: &mp4parse::Track) -> Result<(Codec, Resolution)> {
    let stsd = track
        .stsd
        .as_ref()
        .ok_or_else(|| Error::Parse("mp4: video track has no sample description (stsd)".into()))?;
    let entry = stsd
        .descriptions
        .first()
        .ok_or_else(|| Error::Parse("mp4: stsd has no sample entries".into()))?;

    match entry {
        mp4parse::SampleEntry::Video(v) => {
            let codec = codec_from_mp4parse(v.codec_type)?;
            let resolution = Resolution::new(u32::from(v.width), u32::from(v.height));
            Ok((codec, resolution))
        }
        mp4parse::SampleEntry::Audio(_) => Err(Error::Parse(
            "mp4: first sample entry of video track is audio (corrupted moov)".into(),
        )),
        mp4parse::SampleEntry::Unknown => Err(Error::Parse(
            "mp4: sample entry is an unrecognised box type".into(),
        )),
    }
}

fn codec_from_mp4parse(ct: mp4parse::CodecType) -> Result<Codec> {
    Ok(match ct {
        mp4parse::CodecType::H264 => Codec::H264,
        mp4parse::CodecType::AV1 => Codec::Av1,
        mp4parse::CodecType::VP9 => Codec::Vp9,
        mp4parse::CodecType::VP8 => Codec::Other("vp8".into()),
        mp4parse::CodecType::MP4V => Codec::Other("mpeg4".into()),
        mp4parse::CodecType::H263 => Codec::Other("h263".into()),
        mp4parse::CodecType::EncryptedVideo => {
            return Err(Error::Unsupported(
                "mp4: encrypted video track is not on the fast path".into(),
            ));
        }
        mp4parse::CodecType::Unknown => {
            return Err(Error::Unsupported(
                "mp4: unrecognised video codec (likely HEVC — fall back to FFmpeg)".into(),
            ));
        }
        other => {
            return Err(Error::Parse(format!(
                "mp4: video sample entry declared audio codec {other:?}"
            )));
        }
    })
}

fn extract_track_duration_and_samples(track: &mp4parse::Track) -> (Option<u64>, Option<u64>) {
    let timescale = track.timescale.as_ref().map(|t| t.0);
    let scaled_duration = track.duration.as_ref().map(|d| d.0);
    let duration_ms = match (timescale, scaled_duration) {
        (Some(ts), Some(d)) if ts > 0 => Some(rescale_to_ms(d, ts)),
        _ => None,
    };

    let sample_count = track.stts.as_ref().map(|stts| {
        stts.samples
            .iter()
            .map(|s| u64::from(s.sample_count))
            .sum::<u64>()
    });

    (duration_ms, sample_count)
}

fn rescale_to_ms(value: u64, timescale: u64) -> u64 {
    let v = u128::from(value);
    let ts = u128::from(timescale);
    let scaled = (v * 1000 + ts / 2) / ts;
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

fn compute_fps_x1000(duration_ms: Option<u64>, frames: Option<u64>) -> Option<u32> {
    let (Some(ms), Some(n)) = (duration_ms, frames) else {
        return None;
    };
    if ms == 0 || n == 0 {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let fps = n as f64 * 1000.0 / ms as f64;
    fps_to_x1000(fps)
}

fn compute_bitrate_bps(duration_ms: Option<u64>, file_size: u64) -> Option<u64> {
    let ms = duration_ms?;
    if ms == 0 {
        return None;
    }
    let bits = u128::from(file_size).saturating_mul(8);
    let bps = bits.saturating_mul(1000) / u128::from(ms);
    u64::try_from(bps).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rescale_to_ms_round_trips_exact_values() {
        assert_eq!(rescale_to_ms(15_360, 15_360), 1000);
        assert_eq!(rescale_to_ms(0, 1000), 0);
    }

    #[test]
    fn rescale_to_ms_rounds_half_up() {
        assert_eq!(rescale_to_ms(1, 2000), 1);
        assert_eq!(rescale_to_ms(1, 3000), 0);
    }

    #[test]
    fn bitrate_uses_file_size_and_duration() {
        assert_eq!(compute_bitrate_bps(Some(1000), 2802), Some(22_416));
        assert_eq!(compute_bitrate_bps(None, 2802), None);
        assert_eq!(compute_bitrate_bps(Some(0), 2802), None);
    }

    #[test]
    fn fps_is_none_when_either_input_missing() {
        assert_eq!(compute_fps_x1000(None, Some(30)), None);
        assert_eq!(compute_fps_x1000(Some(1000), None), None);
        assert_eq!(compute_fps_x1000(Some(0), Some(30)), None);
        assert_eq!(compute_fps_x1000(Some(1000), Some(0)), None);
    }

    #[test]
    fn fps_30fps_in_one_second_rounds_to_30_000() {
        assert_eq!(compute_fps_x1000(Some(1000), Some(30)), Some(30_000));
    }

    #[test]
    fn read_moov_body_rejects_oversized_declared_length() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let total: u32 = 8 + 1_000_000;
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(&total.to_be_bytes()).unwrap();
        file.write_all(b"moov").unwrap();
        file.flush().unwrap();

        let err = read_moov_body(file.path()).unwrap_err();
        assert!(matches!(err, Error::Parse(_)));
    }

    #[test]
    fn read_moov_body_accepts_well_formed_small_box() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let total: u32 = 8 + 4;
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(&total.to_be_bytes()).unwrap();
        file.write_all(b"moov").unwrap();
        file.write_all(&[1, 2, 3, 4]).unwrap();
        file.flush().unwrap();

        let body = read_moov_body(file.path()).unwrap();
        assert_eq!(body, vec![1, 2, 3, 4]);
    }

    use std::io::Write;
    use std::path::PathBuf;
    use tempfile::NamedTempFile;

    fn box_header(size: u32, fourcc: &[u8]) -> Vec<u8> {
        let mut v = size.to_be_bytes().to_vec();
        v.extend_from_slice(fourcc);
        v
    }

    fn write_tmp(bytes: &[u8]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    #[test]
    fn boundary_trims_at_first_overshoot_after_prior_boxes() {
        let mut bytes = box_header(16, b"ftyp");
        bytes.extend_from_slice(&[0u8; 8]);
        bytes.extend_from_slice(&box_header(500_000_000, b"junk"));
        let f = write_tmp(&bytes);
        assert_eq!(recognized_top_level_boundary(f.path()).unwrap(), Some(16));
    }

    #[test]
    fn boundary_none_when_overshoot_is_the_first_box() {
        let f = write_tmp(&vec![0xFFu8; 4096]);
        assert_eq!(recognized_top_level_boundary(f.path()).unwrap(), None);
    }

    #[test]
    fn boundary_none_for_fit_declaring_trailing_box() {
        let mut bytes = box_header(16, b"ftyp");
        bytes.extend_from_slice(&[0u8; 8]);
        bytes.extend_from_slice(&box_header(8, b"junk"));
        let f = write_tmp(&bytes);
        assert_eq!(recognized_top_level_boundary(f.path()).unwrap(), None);
    }

    #[test]
    fn boundary_none_when_size_zero_box_reached_before_overshoot() {
        let mut bytes = box_header(16, b"ftyp");
        bytes.extend_from_slice(&[0u8; 8]);
        bytes.extend_from_slice(&box_header(0, b"mdat"));
        bytes.extend_from_slice(&[0u8; 32]);
        let f = write_tmp(&bytes);
        assert_eq!(recognized_top_level_boundary(f.path()).unwrap(), None);
    }

    #[test]
    fn boundary_none_when_declared_size_below_header() {
        let mut bytes = box_header(16, b"ftyp");
        bytes.extend_from_slice(&[0u8; 8]);
        bytes.extend_from_slice(&box_header(4, b"junk"));
        let f = write_tmp(&bytes);
        assert_eq!(recognized_top_level_boundary(f.path()).unwrap(), None);
    }

    #[test]
    fn boundary_none_for_clean_walk_to_eof() {
        let mut bytes = box_header(16, b"ftyp");
        bytes.extend_from_slice(&[0u8; 8]);
        bytes.extend_from_slice(&box_header(8, b"free"));
        let f = write_tmp(&bytes);
        assert_eq!(recognized_top_level_boundary(f.path()).unwrap(), None);
    }

    #[test]
    fn tolerant_read_matches_raw_on_clean_file_no_op() {
        let path = fixture("black_320x180_30fps_1s.mp4");
        let raw = mp4parse::read_mp4(&mut BufReader::new(File::open(&path).unwrap())).unwrap();
        let tol = read_mp4_tolerant(&path).unwrap();
        assert_eq!(tol.tracks.len(), raw.tracks.len());
        let raw_v = &raw.tracks[0];
        let tol_v = &tol.tracks[0];
        assert_eq!(
            format!("{:?}", tol_v.track_type),
            format!("{:?}", raw_v.track_type)
        );
        assert_eq!(
            tol_v.stsd.as_ref().unwrap().descriptions.len(),
            raw_v.stsd.as_ref().unwrap().descriptions.len()
        );
    }

    #[test]
    fn tolerant_read_recovers_overshoot_trailing_garbage() {
        let mut bytes = std::fs::read(fixture("black_320x180_30fps_1s.mp4")).unwrap();
        let clean_tracks =
            mp4parse::read_mp4(&mut BufReader::new(std::io::Cursor::new(bytes.clone())))
                .unwrap()
                .tracks
                .len();
        bytes.extend_from_slice(&box_header(500_000_000, b"junk"));

        assert!(
            mp4parse::read_mp4(&mut BufReader::new(std::io::Cursor::new(bytes.clone()))).is_err(),
            "crafted overshoot fixture must reproduce the pre-fix raw read_mp4 failure"
        );

        let f = write_tmp(&bytes);
        let ctx = read_mp4_tolerant(f.path()).expect("tolerant reader recovers the clip");
        assert_eq!(ctx.tracks.len(), clean_tracks);
    }

    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    fn fired_cancel(flag: &AtomicBool) -> Cancel<'_> {
        flag.store(true, Ordering::Relaxed);
        Cancel {
            pause: Some(flag),
            removal: None,
        }
    }

    fn large_padded_bytes(mdat_len: usize) -> Vec<u8> {
        let mut bytes = box_header(16, b"ftyp");
        bytes.extend_from_slice(&[0u8; 8]);
        let total = u32::try_from(8 + mdat_len).unwrap();
        bytes.extend_from_slice(&box_header(total, b"mdat"));
        bytes.extend(std::iter::repeat_n(0xABu8, mdat_len));
        bytes
    }

    struct CountingReader<R> {
        inner: R,
        read_bytes: std::sync::Arc<AtomicU64>,
    }

    impl<R: Read> Read for CountingReader<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read_bytes.fetch_add(n as u64, Ordering::Relaxed);
            Ok(n)
        }
    }

    #[test]
    fn pre_fired_cancel_reads_zero_bytes_before_erroring() {
        let bytes = large_padded_bytes(1024 * 1024);
        let f = write_tmp(&bytes);
        let flag = AtomicBool::new(false);
        let cancel = fired_cancel(&flag);

        let counted = std::sync::Arc::new(AtomicU64::new(0));
        let file = File::open(f.path()).unwrap();
        let counting = CountingReader {
            inner: file,
            read_bytes: counted.clone(),
        };
        let mut wrapped = BufReader::with_capacity(
            crate::bounded::READ_BUF_CAPACITY,
            CancelRead::new(counting, cancel),
        );
        let result = mp4parse::read_mp4(&mut wrapped);
        assert!(result.is_err(), "a pre-fired cancel must not yield Ok");
        assert_eq!(
            counted.load(Ordering::Relaxed),
            0,
            "no bytes should reach the underlying file once cancel has fired"
        );
    }

    #[test]
    fn read_mp4_tolerant_cancellable_pre_fired_returns_cancelled_not_parse() {
        let bytes = large_padded_bytes(64 * 1024);
        let f = write_tmp(&bytes);
        let flag = AtomicBool::new(false);
        let cancel = fired_cancel(&flag);

        let err = read_mp4_tolerant_cancellable(f.path(), cancel)
            .expect_err("pre-fired cancel must not succeed");
        assert!(
            matches!(err, Error::Cancelled),
            "expected Error::Cancelled, got {err:?}"
        );
    }

    static MID_READ_FLAG: AtomicBool = AtomicBool::new(false);

    #[test]
    fn read_mp4_tolerant_cancellable_mid_read_fire_short_circuits() {
        const PAD: u64 = 1024 * 1024 * 1024;
        let mut bytes = box_header(16, b"ftyp");
        bytes.extend_from_slice(&[0u8; 8]);
        bytes.extend_from_slice(&box_header(u32::try_from(8 + PAD).unwrap(), b"mdat"));
        let f = write_tmp(&bytes);
        std::fs::OpenOptions::new()
            .write(true)
            .open(f.path())
            .unwrap()
            .set_len(bytes.len() as u64 + PAD)
            .unwrap();
        MID_READ_FLAG.store(false, Ordering::Relaxed);
        let cancel = Cancel {
            pause: Some(&MID_READ_FLAG),
            removal: None,
        };

        let handle = std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(5));
            MID_READ_FLAG.store(true, Ordering::Relaxed);
        });
        let err = read_mp4_tolerant_cancellable(f.path(), cancel)
            .expect_err("mid-read cancel must not succeed");
        handle.join().unwrap();
        assert!(
            matches!(err, Error::Cancelled),
            "expected Error::Cancelled, got {err:?}"
        );
    }

    #[test]
    fn read_mp4_tolerant_cancellable_never_fired_matches_non_cancellable() {
        let path = fixture("black_320x180_30fps_1s.mp4");
        let plain = read_mp4_tolerant(&path).unwrap();
        let cancellable = read_mp4_tolerant_cancellable(&path, Cancel::default()).unwrap();
        assert_eq!(cancellable.tracks.len(), plain.tracks.len());
        assert_eq!(
            format!("{:?}", cancellable.tracks[0].track_type),
            format!("{:?}", plain.tracks[0].track_type)
        );
    }

    fn run_fused(path: &Path) -> (PreParsedMp4, u64, u64) {
        use crate::cancel::ThreadReadCounter;
        let mut teed = 0u64;
        crate::cancel::THREAD_READ_COUNTER.with(ThreadReadCounter::start);
        let outcome = read_mp4_tolerant_hashing_cancellable(path, Cancel::default(), &mut |b| {
            teed += b.len() as u64;
        })
        .expect("fused pass must not error on readable files");
        let counted = crate::cancel::THREAD_READ_COUNTER.with(ThreadReadCounter::stop);
        (outcome, teed, counted)
    }

    #[test]
    fn fused_clean_file_single_pass_hash_sees_every_byte_once() {
        let path = fixture("black_320x180_30fps_1s.mp4");
        let size = std::fs::metadata(&path).unwrap().len();
        let (outcome, teed, counted) = run_fused(&path);
        let PreParsedMp4::Parsed(context) = outcome else {
            panic!("clean fixture must parse in the fused pass, got {outcome:?}");
        };
        let plain = read_mp4_tolerant(&path).unwrap();
        assert_eq!(
            context.tracks.len(),
            plain.tracks.len(),
            "§J: same parse as the split path"
        );
        assert_eq!(teed, size, "hash side must see every byte exactly once");
        assert_eq!(
            counted, size,
            "clean file must be read exactly once (no retry pass)"
        );
    }

    #[test]
    fn fused_trailing_garbage_reads_at_most_2_4x_and_hashes_whole_file() {
        let mut bytes = std::fs::read(fixture("black_320x180_30fps_1s.mp4")).unwrap();
        let clean_tracks =
            mp4parse::read_mp4(&mut BufReader::new(std::io::Cursor::new(bytes.clone())))
                .unwrap()
                .tracks
                .len();
        bytes.extend_from_slice(&box_header(500_000_000, b"junk"));
        let f = write_tmp(&bytes);
        let total = bytes.len() as u64;

        let (outcome, teed, counted) = run_fused(f.path());
        let PreParsedMp4::Parsed(context) = outcome else {
            panic!("trim retry must recover the clip, got {outcome:?}");
        };
        assert_eq!(context.tracks.len(), clean_tracks);
        assert_eq!(
            teed, total,
            "hash must cover the ORIGINAL bytes incl. the garbage tail"
        );
        let max = total * 24 / 10;
        assert!(
            counted <= max,
            "garbage-file fused reads {counted} exceed 2.4x ceiling {max} (file {total})"
        );
    }

    #[test]
    fn fused_unparseable_bytes_still_tee_whole_file_and_report_failed() {
        let bytes = vec![0xABu8; 96 * 1024];
        let f = write_tmp(&bytes);
        let (outcome, teed, _counted) = run_fused(f.path());
        assert!(matches!(outcome, PreParsedMp4::Failed), "got {outcome:?}");
        assert_eq!(
            teed,
            bytes.len() as u64,
            "hash must complete despite the parse failure"
        );
    }

    #[test]
    fn fused_empty_file_hashes_zero_bytes_and_reports_failed() {
        let f = write_tmp(&[]);
        let (outcome, teed, _counted) = run_fused(f.path());
        assert!(matches!(outcome, PreParsedMp4::Failed), "got {outcome:?}");
        assert_eq!(teed, 0);
    }

    #[test]
    fn fused_pre_fired_cancel_returns_cancelled_before_any_read() {
        let bytes = large_padded_bytes(1024 * 1024);
        let f = write_tmp(&bytes);
        let flag = AtomicBool::new(false);
        let cancel = fired_cancel(&flag);
        let mut teed = 0u64;
        let err = read_mp4_tolerant_hashing_cancellable(f.path(), cancel, &mut |b| {
            teed += b.len() as u64;
        })
        .expect_err("pre-fired cancel must not succeed");
        assert!(
            matches!(err, Error::Cancelled),
            "expected Cancelled, got {err:?}"
        );
        assert_eq!(teed, 0, "no byte may reach the hash after a fired cancel");
    }

    #[test]
    fn extract_avcc_cancellable_never_fired_matches_non_cancellable() {
        let path = fixture("black_320x180_30fps_1s.mp4");
        let plain = extract_avcc(&path).unwrap();
        let cancellable = extract_avcc_cancellable(&path, Cancel::default()).unwrap();
        assert_eq!(cancellable, plain);
    }

    #[test]
    fn extract_avcc_cancellable_pre_fired_returns_cancelled() {
        let path = fixture("black_320x180_30fps_1s.mp4");
        let flag = AtomicBool::new(false);
        let cancel = fired_cancel(&flag);
        let err = extract_avcc_cancellable(&path, cancel).expect_err("must not succeed");
        assert!(
            matches!(err, Error::Cancelled),
            "expected Error::Cancelled, got {err:?}"
        );
    }

    #[test]
    fn probe_mp4_cancellable_never_fired_matches_non_cancellable() {
        let path = fixture("black_320x180_30fps_1s.mp4");
        let plain = probe_mp4(&path, ContainerKind::Mp4).unwrap();
        let cancellable =
            probe_mp4_cancellable(&path, ContainerKind::Mp4, Cancel::default()).unwrap();
        assert_eq!(cancellable, plain);
    }

    #[test]
    fn probe_mp4_cancellable_pre_fired_returns_cancelled() {
        let path = fixture("black_320x180_30fps_1s.mp4");
        let flag = AtomicBool::new(false);
        let cancel = fired_cancel(&flag);
        let err =
            probe_mp4_cancellable(&path, ContainerKind::Mp4, cancel).expect_err("must not succeed");
        assert!(
            matches!(err, Error::Cancelled),
            "expected Error::Cancelled, got {err:?}"
        );
    }
}
