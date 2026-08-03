use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use vidcull_core::types::{Codec, Resolution, VideoDuration};
use vidcull_core::{Error, Result};

use crate::Cancel;
use crate::probe::{ContainerKind, VideoMetadata, fps_to_x1000, no_video_track};

pub fn probe_mkv(path: &Path, container: ContainerKind) -> Result<VideoMetadata> {
    let file = File::open(path)?;
    let file_size_bytes = file.metadata()?.len();
    let reader = BufReader::with_capacity(crate::bounded::READ_BUF_CAPACITY, file);

    probe_mkv_reader(reader, container, file_size_bytes)
}

pub fn probe_mkv_hashing_cancellable(
    path: &Path,
    container: ContainerKind,
    cancel: Cancel<'_>,
    sink: &mut dyn FnMut(&[u8]),
) -> Result<Option<VideoMetadata>> {
    let file = File::open(path)?;
    let file_size_bytes = file.metadata()?.len();
    let mut reader = HashingSeekReader::new(file, file_size_bytes, cancel, sink);
    let parsed = probe_mkv_reader(&mut reader, container, file_size_bytes);
    if let Err(err) = reader.finish() {
        if cancel.fired() {
            return Err(Error::Cancelled);
        }
        return Err(err);
    }
    if cancel.fired() {
        return Err(Error::Cancelled);
    }
    match parsed {
        Ok(metadata) => Ok(Some(metadata)),
        Err(Error::Io(err)) => Err(Error::Io(err)),
        Err(_) => Ok(None),
    }
}

fn probe_mkv_reader<R: Read + Seek>(
    reader: R,
    container: ContainerKind,
    file_size_bytes: u64,
) -> Result<VideoMetadata> {
    let mkv = matroska_demuxer::MatroskaFile::open(reader)
        .map_err(|e| Error::Parse(format!("matroska: {e}")))?;

    let info = mkv.info();
    let timestamp_scale = info.timestamp_scale().get();
    let duration_ms = info
        .duration()
        .and_then(|d| ticks_to_ms(d, timestamp_scale));

    let video = mkv
        .tracks()
        .iter()
        .find(|t| matches!(t.track_type(), matroska_demuxer::TrackType::Video))
        .ok_or_else(|| no_video_track("mkv"))?;

    let codec = codec_from_codec_id(video.codec_id())?;
    let resolution = resolution_from_track(video)?;
    let fps_x1000 = video.default_duration().and_then(|ns_per_frame| {
        #[allow(clippy::cast_precision_loss)]
        let ns = ns_per_frame.get() as f64;
        fps_to_x1000(1_000_000_000.0 / ns)
    });
    let bitrate_bps = compute_bitrate_bps(duration_ms, file_size_bytes);

    Ok(VideoMetadata {
        container,
        codec,
        resolution,
        duration: duration_ms.map(VideoDuration::from_millis),
        fps_x1000,
        has_b_frames: None,
        bitrate_bps,
        encoder_tags: None,
    })
}

struct HashingSeekReader<'a> {
    file: File,
    pos: u64,
    hashed_until: u64,
    file_len: u64,
    physical_bytes_read: u64,
    cancel: Cancel<'a>,
    sink: &'a mut dyn FnMut(&[u8]),
}

impl<'a> HashingSeekReader<'a> {
    fn new(file: File, file_len: u64, cancel: Cancel<'a>, sink: &'a mut dyn FnMut(&[u8])) -> Self {
        Self {
            file,
            pos: 0,
            hashed_until: 0,
            file_len,
            physical_bytes_read: 0,
            cancel,
            sink,
        }
    }

    fn finish(&mut self) -> Result<()> {
        self.hash_through(self.file_len).map_err(Error::Io)
    }

    fn hash_through(&mut self, target: u64) -> std::io::Result<()> {
        let target = target.min(self.file_len);
        if target <= self.hashed_until {
            return Ok(());
        }
        let restore = self.pos;
        self.file.seek(SeekFrom::Start(self.hashed_until))?;
        let mut buf = [0u8; 64 * 1024];
        while self.hashed_until < target {
            if self.cancel.fired() {
                return Err(std::io::Error::new(
                    crate::cancel::CANCEL_READ_MARKER,
                    "MKV fused hash cancelled",
                ));
            }
            let want = usize::try_from((target - self.hashed_until).min(buf.len() as u64))
                .unwrap_or(buf.len());
            let n = self.file.read(&mut buf[..want])?;
            if n == 0 {
                break;
            }
            (self.sink)(&buf[..n]);
            self.physical_bytes_read = self.physical_bytes_read.saturating_add(n as u64);
            self.hashed_until += n as u64;
        }
        self.file.seek(SeekFrom::Start(restore))?;
        Ok(())
    }
}

impl Read for HashingSeekReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.cancel.fired() {
            return Err(std::io::Error::new(
                crate::cancel::CANCEL_READ_MARKER,
                "MKV fused hash cancelled",
            ));
        }
        if self.pos > self.hashed_until {
            self.hash_through(self.pos)?;
        }
        let start = self.pos;
        let n = self.file.read(buf)?;
        self.physical_bytes_read = self.physical_bytes_read.saturating_add(n as u64);
        self.pos += n as u64;
        if self.pos > self.hashed_until {
            let already_hashed = usize::try_from(self.hashed_until.saturating_sub(start))
                .unwrap_or(usize::MAX)
                .min(n);
            (self.sink)(&buf[already_hashed..n]);
            self.hashed_until = self.pos;
        }
        Ok(n)
    }
}

impl Seek for HashingSeekReader<'_> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(value) => value,
            SeekFrom::Current(delta) => self.pos.checked_add_signed(delta).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek before start")
            })?,
            SeekFrom::End(delta) => self.file_len.checked_add_signed(delta).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek before start")
            })?,
        };
        self.hash_through(target)?;
        self.pos = self.file.seek(SeekFrom::Start(target))?;
        Ok(self.pos)
    }
}

pub fn extract_codec_private(path: &Path) -> Result<Vec<u8>> {
    let reader = BufReader::with_capacity(crate::bounded::READ_BUF_CAPACITY, File::open(path)?);
    let mkv = matroska_demuxer::MatroskaFile::open(reader)
        .map_err(|e| Error::Parse(format!("matroska: {e}")))?;
    let video = mkv
        .tracks()
        .iter()
        .find(|t| matches!(t.track_type(), matroska_demuxer::TrackType::Video))
        .ok_or_else(|| no_video_track("mkv"))?;
    if !matches!(
        codec_from_codec_id(video.codec_id())?,
        Codec::H264 | Codec::H265
    ) {
        return Err(Error::Unsupported(
            "mkv: video track is neither H.264 nor H.265; native decode not applicable".into(),
        ));
    }
    video.codec_private().map(<[u8]>::to_vec).ok_or_else(|| {
        Error::Unsupported(
            "mkv: H.264/H.265 track carries no CodecPrivate (avcC/hvcC); native decode not \
             applicable"
                .into(),
        )
    })
}

fn codec_from_codec_id(id: &str) -> Result<Codec> {
    Ok(match id {
        "V_MPEG4/ISO/AVC" => Codec::H264,
        "V_MPEGH/ISO/HEVC" => Codec::H265,
        "V_AV1" => Codec::Av1,
        "V_VP9" => Codec::Vp9,
        "V_VP8" => Codec::Other("vp8".into()),
        "V_MPEG2" => Codec::Mpeg2,
        other if other.starts_with("V_") => {
            Codec::Other(other.trim_start_matches("V_").to_ascii_lowercase())
        }
        other => {
            return Err(Error::Parse(format!(
                "mkv: track CodecID `{other}` is not a video codec"
            )));
        }
    })
}

fn resolution_from_track(track: &matroska_demuxer::TrackEntry) -> Result<Resolution> {
    let video = track
        .video()
        .ok_or_else(|| Error::Parse("mkv: video track lacks Video element".into()))?;
    let width = u32::try_from(video.pixel_width().get())
        .map_err(|_| Error::Parse("mkv: PixelWidth exceeds u32 range".into()))?;
    let height = u32::try_from(video.pixel_height().get())
        .map_err(|_| Error::Parse("mkv: PixelHeight exceeds u32 range".into()))?;
    Ok(Resolution::new(width, height))
}

fn ticks_to_ms(ticks: f64, timestamp_scale_ns: u64) -> Option<u64> {
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

const MAX_REPRESENTABLE_MS_F64: f64 = 9_223_372_036_854_775_808.0;

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
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    #[test]
    fn ticks_to_ms_handles_standard_timestamp_scale() {
        assert_eq!(ticks_to_ms(1000.0, 1_000_000), Some(1000));
        assert_eq!(ticks_to_ms(0.0, 1_000_000), Some(0));
    }

    #[test]
    fn ticks_to_ms_rejects_invalid_inputs() {
        assert_eq!(ticks_to_ms(f64::NAN, 1_000_000), None);
        assert_eq!(ticks_to_ms(-1.0, 1_000_000), None);
    }

    #[test]
    fn codec_id_mapping_covers_known_video_codecs() {
        assert_eq!(codec_from_codec_id("V_MPEG4/ISO/AVC").unwrap(), Codec::H264);
        assert_eq!(
            codec_from_codec_id("V_MPEGH/ISO/HEVC").unwrap(),
            Codec::H265
        );
        assert_eq!(codec_from_codec_id("V_AV1").unwrap(), Codec::Av1);
        assert_eq!(codec_from_codec_id("V_VP9").unwrap(), Codec::Vp9);
        assert_eq!(codec_from_codec_id("V_MPEG2").unwrap(), Codec::Mpeg2);
    }

    #[test]
    fn codec_id_unknown_video_prefix_maps_to_other() {
        assert_eq!(
            codec_from_codec_id("V_PRORES").unwrap(),
            Codec::Other("prores".into())
        );
    }

    #[test]
    fn codec_id_audio_prefix_is_rejected() {
        let err = codec_from_codec_id("A_AAC").expect_err("audio track must not be accepted here");
        assert!(matches!(err, Error::Parse(_)));
    }

    #[test]
    fn fused_mkv_probe_hashes_every_byte_in_original_order() {
        for name in [
            "black_320x180_30fps_1s.mkv",
            "h264-native-e2e/testsrc2_160_90.mkv",
            "hevc-native-e2e/clip.mkv",
        ] {
            let path = fixture(name);
            let expected = vidcull_fingerprint::hash_file(&path).expect("standalone hash");
            let mut hasher = blake3::Hasher::new();
            let metadata = probe_mkv_hashing_cancellable(
                &path,
                ContainerKind::Mkv,
                Cancel::default(),
                &mut |bytes| {
                    hasher.update(bytes);
                },
            )
            .expect("fused probe")
            .expect("fixture must parse");
            assert_eq!(hasher.finalize().as_bytes(), expected.as_bytes(), "{name}");
            assert!(!metadata.resolution.is_empty(), "{name}");
        }
    }

    #[test]
    fn fused_webm_probe_hashes_every_byte_in_original_order() {
        let path = fixture("vp9_320x180_1s.webm");
        let expected = vidcull_fingerprint::hash_file(&path).expect("standalone hash");
        let mut hasher = blake3::Hasher::new();
        let metadata = probe_mkv_hashing_cancellable(
            &path,
            ContainerKind::WebM,
            Cancel::default(),
            &mut |bytes| {
                hasher.update(bytes);
            },
        )
        .expect("fused probe")
        .expect("fixture must parse");
        assert_eq!(hasher.finalize().as_bytes(), expected.as_bytes());
        assert_eq!(metadata.container, ContainerKind::WebM);
    }

    #[test]
    fn fused_corrupt_mkv_still_hashes_the_complete_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("corrupt.mkv");
        let bytes: Vec<u8> = (0..200_000).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &bytes).expect("write");
        let expected = blake3::hash(&bytes);
        let mut hasher = blake3::Hasher::new();
        let metadata = probe_mkv_hashing_cancellable(
            &path,
            ContainerKind::Mkv,
            Cancel::default(),
            &mut |chunk| {
                hasher.update(chunk);
            },
        )
        .expect("readable corrupt file must complete hashing");
        assert!(metadata.is_none());
        assert_eq!(hasher.finalize(), expected);
    }

    #[test]
    fn fused_mkv_probe_honors_pre_fired_cancellation_without_hashing() {
        let path = fixture("black_320x180_30fps_1s.mkv");
        let flag = AtomicBool::new(true);
        let cancel = Cancel {
            pause: Some(&flag),
            removal: None,
        };
        let mut hashed = 0usize;
        let err = probe_mkv_hashing_cancellable(&path, ContainerKind::Mkv, cancel, &mut |bytes| {
            hashed += bytes.len()
        })
        .expect_err("pre-fired cancellation must fail");
        assert!(matches!(err, Error::Cancelled));
        assert_eq!(hashed, 0);
    }

    #[test]
    fn fused_mkv_probe_read_amplification_is_bounded() {
        let path = fixture("h264-native-e2e/testsrc2_160_90.mkv");
        let file = File::open(&path).expect("open");
        let file_len = file.metadata().expect("metadata").len();
        let mut hashed = 0u64;
        let mut sink = |bytes: &[u8]| hashed += bytes.len() as u64;
        let mut reader = HashingSeekReader::new(file, file_len, Cancel::default(), &mut sink);
        probe_mkv_reader(&mut reader, ContainerKind::Mkv, file_len).expect("probe");
        reader.finish().expect("finish hash");
        let physical = reader.physical_bytes_read;
        drop(reader);
        eprintln!("fused MKV physical read bytes: {physical}/{file_len}");
        assert_eq!(
            hashed, file_len,
            "logical hash stream must cover the file once"
        );
        assert!(
            physical <= file_len.saturating_mul(2),
            "fused MKV read amplification too high: {physical}/{file_len}"
        );
    }
}
