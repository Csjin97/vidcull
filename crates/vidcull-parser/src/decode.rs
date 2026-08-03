use std::path::Path;

use vidcull_core::types::Codec;
use vidcull_core::{Error, Result};

use crate::cancel::Cancel;
use crate::fallback::concurrency::{DecodeConcurrency, fan_out_indexed};
use crate::fallback::{self, DecodePath, FfmpegBinaries};
use crate::h264::NativeH264Decoder;
use crate::hevc::NativeH265Decoder;
use crate::mp4_index::{Keyframe, Mp4Index};
use crate::probe::{ContainerKind, VideoMetadata};
use crate::sparse::{GrayscaleFrame, SparseDecoder, SparseSampleSource, SparseStep};
use crate::sparse_mkv::MkvSampleSource;
use crate::sparse_mp4::Mp4SampleSource;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedVideo {
    pub metadata: VideoMetadata,
    pub frames: Vec<GrayscaleFrame>,
    pub decode_path: DecodePath,
}

pub fn probe_and_decode_sparse(
    bins: &FfmpegBinaries,
    path: &Path,
    budget: usize,
) -> Result<DecodedVideo> {
    probe_and_decode_sparse_impl(
        bins,
        path,
        budget,
        budget,
        false,
        &crate::fallback::DecodeConcurrency::serial(),
    )
}

pub fn probe_and_decode_sparse_budgets(
    bins: &FfmpegBinaries,
    path: &Path,
    native_budget: usize,
    fallback_budget: usize,
    conc: &crate::fallback::DecodeConcurrency,
) -> Result<DecodedVideo> {
    probe_and_decode_sparse_impl(bins, path, native_budget, fallback_budget, true, conc)
}

fn probe_and_decode_sparse_impl(
    bins: &FfmpegBinaries,
    path: &Path,
    native_budget: usize,
    fallback_budget: usize,
    strided_fallback: bool,
    conc: &crate::fallback::DecodeConcurrency,
) -> Result<DecodedVideo> {
    let budget = native_budget;
    let metadata = probe_for_decode(bins, path, Cancel::default())?;

    let duration_ms = usable_duration_ms(&metadata).ok_or_else(|| {
        Error::Unsupported(format!(
            "decode: probe reported no usable duration for {}",
            path.display()
        ))
    })?;

    if matches!(metadata.codec, Codec::H264) {
        if let Some(frames) = try_native_h264(path, &metadata.container, duration_ms, budget, conc)
        {
            return Ok(DecodedVideo {
                metadata,
                frames,
                decode_path: DecodePath::Native,
            });
        }
    }

    if matches!(metadata.codec, Codec::H265) {
        if let Some(frames) = try_native_h265(path, &metadata.container, duration_ms, budget, conc)
        {
            return Ok(DecodedVideo {
                metadata,
                frames,
                decode_path: DecodePath::Native,
            });
        }
    }

    let frames = if strided_fallback {
        fallback::decode_sparse_strided_with(
            bins,
            path,
            duration_ms,
            metadata.resolution.width,
            metadata.resolution.height,
            fallback_budget,
            &metadata.codec,
            metadata.fps_x1000,
            metadata.has_b_frames,
            conc,
        )?
    } else {
        fallback::decode_sparse_with(
            bins,
            path,
            duration_ms,
            metadata.resolution.width,
            metadata.resolution.height,
            fallback_budget,
            &metadata.codec,
            metadata.fps_x1000,
            metadata.has_b_frames,
            conc,
        )?
    };

    Ok(DecodedVideo {
        metadata,
        frames,
        decode_path: DecodePath::Fallback,
    })
}

fn try_native_h264(
    path: &Path,
    container: &ContainerKind,
    duration_ms: u64,
    budget: usize,
    conc: &DecodeConcurrency,
) -> Option<Vec<GrayscaleFrame>> {
    match container {
        ContainerKind::Mp4 | ContainerKind::Mov | ContainerKind::ThreeGp => {
            try_native_h264_mp4(path, duration_ms, budget, conc)
        }
        ContainerKind::Mkv | ContainerKind::WebM => {
            try_native_h264_mkv(path, duration_ms, budget, conc)
        }
        ContainerKind::UnsupportedFastPath(_) => None,
    }
}

fn try_native_h264_mp4(
    path: &Path,
    duration_ms: u64,
    budget: usize,
    conc: &DecodeConcurrency,
) -> Option<Vec<GrayscaleFrame>> {
    let avcc = match crate::mp4::extract_avcc(path) {
        Ok(a) => a,
        Err(e) => {
            tracing::info!(step = "extract_avcc", reason = %e, "h264/mp4 native path declined");
            return None;
        }
    };
    let index = match crate::mp4_index::index_mp4(path) {
        Ok(i) => i,
        Err(e) => {
            tracing::info!(step = "index_mp4", reason = %e, "h264/mp4 native path declined");
            return None;
        }
    };
    let decoder = match NativeH264Decoder::from_avcc(&avcc) {
        Ok(d) => d,
        Err(e) => {
            tracing::info!(step = "from_avcc", reason = %e, "h264/mp4 native path declined");
            return None;
        }
    };
    let mut source = match Mp4SampleSource::open(path) {
        Ok(s) => s,
        Err(e) => {
            tracing::info!(step = "open_source", reason = %e, "h264/mp4 native path declined");
            return None;
        }
    };

    let targets = fallback::plan_fallback_timestamps(duration_ms, budget);
    let mut samples = Vec::with_capacity(targets.len());
    for target_ms in targets {
        let keyframe = nearest_preceding_idr(&index, target_ms)?;
        let step = SparseStep {
            timestamp_ms: target_ms,
            locator: keyframe.sample_number,
        };
        samples.push(source.fetch(&step).ok()?);
    }
    fan_out_indexed(&samples, conc, |sample| {
        decoder.clone().decode_idr(sample, &Codec::H264)
    })
    .ok()
}

fn nearest_preceding_idr(index: &Mp4Index, target_ms: u64) -> Option<&Keyframe> {
    index
        .keyframes
        .iter()
        .take_while(|kf| kf.timestamp_ms <= target_ms)
        .last()
}

fn try_native_h264_mkv(
    path: &Path,
    duration_ms: u64,
    budget: usize,
    conc: &DecodeConcurrency,
) -> Option<Vec<GrayscaleFrame>> {
    let index = match crate::mkv_index::index_mkv(path) {
        Ok(i) => i,
        Err(e) => {
            tracing::info!(step = "index_mkv", reason = %e, "h264/mkv native path declined");
            return None;
        }
    };
    let codec_private = index.codec_private.as_deref()?;
    let decoder = match NativeH264Decoder::from_avcc(&codec_private) {
        Ok(d) => d,
        Err(e) => {
            tracing::info!(step = "from_avcc", reason = %e, "h264/mkv native path declined");
            return None;
        }
    };
    let mut source = match MkvSampleSource::open_with_index(path, &index) {
        Ok(s) => s,
        Err(e) => {
            tracing::info!(step = "open_source", reason = %e, "h264/mkv native path declined");
            return None;
        }
    };

    let targets = fallback::plan_fallback_timestamps(duration_ms, budget);
    let mut samples = Vec::with_capacity(targets.len());
    for target_ms in targets {
        let keyframe = nearest_preceding_idr_mkv(&index, target_ms)?;
        let step = SparseStep {
            timestamp_ms: target_ms,
            locator: keyframe.cue_index,
        };
        samples.push(source.fetch(&step).ok()?);
    }
    fan_out_indexed(&samples, conc, |sample| {
        decoder.clone().decode_idr(sample, &Codec::H264)
    })
    .ok()
}

fn nearest_preceding_idr_mkv(
    index: &crate::mkv_index::MkvIndex,
    target_ms: u64,
) -> Option<&crate::mkv_index::Keyframe> {
    index
        .keyframes
        .iter()
        .take_while(|kf| kf.timestamp_ms <= target_ms)
        .last()
}

fn try_native_h265(
    path: &Path,
    container: &ContainerKind,
    duration_ms: u64,
    budget: usize,
    conc: &DecodeConcurrency,
) -> Option<Vec<GrayscaleFrame>> {
    match container {
        ContainerKind::Mp4 | ContainerKind::Mov | ContainerKind::ThreeGp => {
            try_native_h265_mp4(path, duration_ms, budget, conc)
        }
        ContainerKind::Mkv | ContainerKind::WebM => {
            try_native_h265_mkv(path, duration_ms, budget, conc)
        }
        ContainerKind::UnsupportedFastPath(_) => None,
    }
}

fn try_native_h265_mp4(
    path: &Path,
    duration_ms: u64,
    budget: usize,
    conc: &DecodeConcurrency,
) -> Option<Vec<GrayscaleFrame>> {
    let hvcc = match crate::mp4::extract_hvcc(path) {
        Ok(h) => h,
        Err(e) => {
            tracing::info!(step = "extract_hvcc", reason = %e, "h265/mp4 native path declined");
            return None;
        }
    };
    let index = match crate::mp4_index::index_mp4(path) {
        Ok(i) => i,
        Err(e) => {
            tracing::info!(step = "index_mp4", reason = %e, "h265/mp4 native path declined");
            return None;
        }
    };
    let decoder = match NativeH265Decoder::from_hvcc(&hvcc) {
        Ok(d) => d,
        Err(e) => {
            tracing::info!(step = "from_hvcc", reason = %e, "h265/mp4 native path declined");
            return None;
        }
    };
    let mut source = match Mp4SampleSource::open(path) {
        Ok(s) => s,
        Err(e) => {
            tracing::info!(step = "open_source", reason = %e, "h265/mp4 native path declined");
            return None;
        }
    };

    let targets = fallback::plan_fallback_timestamps(duration_ms, budget);
    let mut samples = Vec::with_capacity(targets.len());
    for target_ms in targets {
        let keyframe = nearest_preceding_idr(&index, target_ms)?;
        let step = SparseStep {
            timestamp_ms: target_ms,
            locator: keyframe.sample_number,
        };
        samples.push(source.fetch(&step).ok()?);
    }
    fan_out_indexed(&samples, conc, |sample| {
        decoder.clone().decode_idr(sample, &Codec::H265)
    })
    .ok()
}

fn try_native_h265_mkv(
    path: &Path,
    duration_ms: u64,
    budget: usize,
    conc: &DecodeConcurrency,
) -> Option<Vec<GrayscaleFrame>> {
    let index = match crate::mkv_index::index_mkv(path) {
        Ok(i) => i,
        Err(e) => {
            tracing::info!(step = "index_mkv", reason = %e, "h265/mkv native path declined");
            return None;
        }
    };
    let hvcc = index.codec_private.as_deref()?;
    let decoder = match NativeH265Decoder::from_hvcc(&hvcc) {
        Ok(d) => d,
        Err(e) => {
            tracing::info!(step = "from_hvcc", reason = %e, "h265/mkv native path declined");
            return None;
        }
    };
    let mut source = match MkvSampleSource::open_with_index(path, &index) {
        Ok(s) => s,
        Err(e) => {
            tracing::info!(step = "open_source", reason = %e, "h265/mkv native path declined");
            return None;
        }
    };

    let targets = fallback::plan_fallback_timestamps(duration_ms, budget);
    let mut samples = Vec::with_capacity(targets.len());
    for target_ms in targets {
        let keyframe = nearest_preceding_idr_mkv(&index, target_ms)?;
        let step = SparseStep {
            timestamp_ms: target_ms,
            locator: keyframe.cue_index,
        };
        samples.push(source.fetch(&step).ok()?);
    }
    fan_out_indexed(&samples, conc, |sample| {
        decoder.clone().decode_idr(sample, &Codec::H265)
    })
    .ok()
}

fn usable_duration_ms(metadata: &VideoMetadata) -> Option<u64> {
    metadata
        .duration
        .map(vidcull_core::types::VideoDuration::as_millis)
        .filter(|ms| *ms > 0)
}

fn probe_for_decode(
    bins: &FfmpegBinaries,
    path: &Path,
    cancel: Cancel<'_>,
) -> Result<VideoMetadata> {
    probe_for_decode_with_context(bins, path, cancel, crate::mp4::PreParsedMp4::NotAttempted)
        .map(|(metadata, _)| metadata)
}

fn probe_for_decode_with_context(
    bins: &FfmpegBinaries,
    path: &Path,
    cancel: Cancel<'_>,
    pre_parsed: crate::mp4::PreParsedMp4,
) -> Result<(VideoMetadata, Option<mp4parse::MediaContext>)> {
    let native = match crate::probe::probe_with_context_cancellable(path, cancel, pre_parsed) {
        Ok((metadata, context)) => {
            if !metadata.resolution.is_empty() && usable_duration_ms(&metadata).is_some() {
                return Ok((metadata, context));
            }
            Some(metadata)
        }
        Err(err) if fallback::should_probe_fallback(&err) => None,
        Err(err) => return Err(err),
    };

    match fallback::probe_fallback_cancellable(bins, path, cancel) {
        Ok(metadata) if !metadata.resolution.is_empty() => Ok((metadata, None)),
        Ok(weak) => Ok((
            native.filter(|m| !m.resolution.is_empty()).unwrap_or(weak),
            None,
        )),
        Err(err) => native.map(|m| (m, None)).ok_or(err),
    }
}

pub fn probe_and_decode_sparse_budgets_streaming<F>(
    bins: &FfmpegBinaries,
    path: &Path,
    native_budget: usize,
    fallback_budget: usize,
    conc: &crate::fallback::DecodeConcurrency,
    cancel: Cancel<'_>,
    on_frame: F,
) -> Result<(VideoMetadata, DecodePath)>
where
    F: FnMut(&GrayscaleFrame) -> Result<()>,
{
    probe_and_decode_sparse_impl_streaming(
        bins,
        path,
        native_budget,
        fallback_budget,
        true,
        conc,
        crate::mp4::PreParsedMp4::NotAttempted,
        cancel,
        on_frame,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn probe_and_decode_sparse_budgets_streaming_preparsed<F>(
    bins: &FfmpegBinaries,
    path: &Path,
    native_budget: usize,
    fallback_budget: usize,
    conc: &crate::fallback::DecodeConcurrency,
    pre_parsed: crate::mp4::PreParsedMp4,
    cancel: Cancel<'_>,
    on_frame: F,
) -> Result<(VideoMetadata, DecodePath)>
where
    F: FnMut(&GrayscaleFrame) -> Result<()>,
{
    probe_and_decode_sparse_impl_streaming(
        bins,
        path,
        native_budget,
        fallback_budget,
        true,
        conc,
        pre_parsed,
        cancel,
        on_frame,
    )
}

#[allow(clippy::too_many_arguments)]
fn probe_and_decode_sparse_impl_streaming<F>(
    bins: &FfmpegBinaries,
    path: &Path,
    native_budget: usize,
    fallback_budget: usize,
    strided_fallback: bool,
    conc: &crate::fallback::DecodeConcurrency,
    pre_parsed: crate::mp4::PreParsedMp4,
    cancel: Cancel<'_>,
    mut on_frame: F,
) -> Result<(VideoMetadata, DecodePath)>
where
    F: FnMut(&GrayscaleFrame) -> Result<()>,
{
    if cancel.fired() {
        return Err(Error::Cancelled);
    }
    let probe_start = std::time::Instant::now();
    let (metadata, parsed_mp4) = probe_for_decode_with_context(bins, path, cancel, pre_parsed)?;
    let probe_ms = u64::try_from(probe_start.elapsed().as_millis()).unwrap_or(u64::MAX);
    tracing::debug!(probe_ms, "probe complete");
    if probe_ms >= 2_000 {
        tracing::warn!(probe_ms, "slow probe (native parse + ffprobe fallback)");
    }

    let duration_ms = usable_duration_ms(&metadata).ok_or_else(|| {
        Error::Unsupported(format!(
            "decode: probe reported no usable duration for {}",
            path.display()
        ))
    })?;

    let frame_bytes = metadata.resolution.width as usize * metadata.resolution.height as usize;

    if matches!(metadata.codec, Codec::H264)
        && try_native_h264_streaming(
            path,
            &metadata.container,
            duration_ms,
            native_budget,
            conc,
            frame_bytes,
            cancel,
            parsed_mp4.as_ref(),
            &mut on_frame,
        )?
    {
        return Ok((metadata, DecodePath::Native));
    }

    if matches!(metadata.codec, Codec::H265)
        && try_native_h265_streaming(
            path,
            &metadata.container,
            duration_ms,
            native_budget,
            conc,
            frame_bytes,
            cancel,
            parsed_mp4.as_ref(),
            &mut on_frame,
        )?
    {
        return Ok((metadata, DecodePath::Native));
    }

    if strided_fallback {
        fallback::decode_sparse_strided_with_streaming(
            bins,
            path,
            duration_ms,
            metadata.resolution.width,
            metadata.resolution.height,
            fallback_budget,
            &metadata.codec,
            metadata.fps_x1000,
            metadata.has_b_frames,
            conc,
            cancel,
            &mut on_frame,
        )?;
    } else {
        fallback::decode_sparse_with_streaming(
            bins,
            path,
            duration_ms,
            metadata.resolution.width,
            metadata.resolution.height,
            fallback_budget,
            &metadata.codec,
            metadata.fps_x1000,
            metadata.has_b_frames,
            conc,
            cancel,
            &mut on_frame,
        )?;
    }

    Ok((metadata, DecodePath::Fallback))
}

#[allow(clippy::too_many_arguments)]
fn try_native_h264_streaming<F>(
    path: &Path,
    container: &ContainerKind,
    duration_ms: u64,
    budget: usize,
    conc: &DecodeConcurrency,
    frame_bytes: usize,
    cancel: Cancel<'_>,
    parsed_mp4: Option<&mp4parse::MediaContext>,
    on_frame: &mut F,
) -> Result<bool>
where
    F: FnMut(&GrayscaleFrame) -> Result<()>,
{
    match container {
        ContainerKind::Mp4 | ContainerKind::Mov | ContainerKind::ThreeGp => {
            try_native_h264_mp4_streaming(
                path,
                duration_ms,
                budget,
                conc,
                frame_bytes,
                cancel,
                parsed_mp4,
                on_frame,
            )
        }
        ContainerKind::Mkv | ContainerKind::WebM => try_native_h264_mkv_streaming(
            path,
            duration_ms,
            budget,
            conc,
            frame_bytes,
            cancel,
            on_frame,
        ),
        ContainerKind::UnsupportedFastPath(_) => Ok(false),
    }
}

#[allow(clippy::too_many_arguments)]
fn try_native_h265_streaming<F>(
    path: &Path,
    container: &ContainerKind,
    duration_ms: u64,
    budget: usize,
    conc: &DecodeConcurrency,
    frame_bytes: usize,
    cancel: Cancel<'_>,
    parsed_mp4: Option<&mp4parse::MediaContext>,
    on_frame: &mut F,
) -> Result<bool>
where
    F: FnMut(&GrayscaleFrame) -> Result<()>,
{
    match container {
        ContainerKind::Mp4 | ContainerKind::Mov | ContainerKind::ThreeGp => {
            try_native_h265_mp4_streaming(
                path,
                duration_ms,
                budget,
                conc,
                frame_bytes,
                cancel,
                parsed_mp4,
                on_frame,
            )
        }
        ContainerKind::Mkv | ContainerKind::WebM => try_native_h265_mkv_streaming(
            path,
            duration_ms,
            budget,
            conc,
            frame_bytes,
            cancel,
            on_frame,
        ),
        ContainerKind::UnsupportedFastPath(_) => Ok(false),
    }
}

const NATIVE_FANOUT_MEM_BUDGET_BYTES: usize = 96 * 1024 * 1024;

fn native_fanout_chunk_len(conc_cap: usize, frame_bytes: usize) -> usize {
    use std::sync::OnceLock;
    static MAX: OnceLock<Option<usize>> = OnceLock::new();
    let cap = conc_cap.max(1);
    if let Some(limit) = *MAX.get_or_init(|| {
        std::env::var("VIDCULL_NATIVE_FANOUT_MAX")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&n| n >= 1)
    }) {
        cap.min(limit)
    } else {
        let mem_cap = (NATIVE_FANOUT_MEM_BUDGET_BYTES / frame_bytes.max(1)).max(1);
        cap.min(mem_cap)
    }
}

fn stream_native_chunks<S, Resolve, Dec, On>(
    targets: &[u64],
    chunk_len: usize,
    conc: &DecodeConcurrency,
    cancel: Cancel<'_>,
    mut resolve_fetch: Resolve,
    decode: Dec,
    on_frame: &mut On,
) -> Result<bool>
where
    S: Send + Sync,
    Resolve: FnMut(u64) -> Option<S>,
    Dec: Fn(&S) -> Result<GrayscaleFrame> + Sync,
    On: FnMut(&GrayscaleFrame) -> Result<()>,
{
    let mut delivered = false;
    for chunk in targets.chunks(chunk_len.max(1)) {
        if cancel.fired() {
            return Err(Error::Cancelled);
        }
        let mut samples = Vec::with_capacity(chunk.len());
        for &target_ms in chunk {
            if cancel.fired() {
                return Err(Error::Cancelled);
            }
            match resolve_fetch(target_ms) {
                None => {
                    if delivered {
                        return Err(Error::Decode(
                            "native stream: recoverable IDR/fetch failure after frames \
                             delivered; cannot fall back without double-folding"
                                .into(),
                        ));
                    }
                    return Ok(false);
                }
                Some(s) => samples.push(s),
            }
        }
        let frames = match fan_out_indexed(&samples, conc, &decode) {
            Ok(f) => f,
            Err(e) => {
                if delivered || matches!(e, Error::Cancelled) {
                    return Err(e);
                }
                return Ok(false);
            }
        };
        for f in &frames {
            on_frame(f)?;
            delivered = true;
        }
    }
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn try_native_h264_mp4_streaming<F>(
    path: &Path,
    duration_ms: u64,
    budget: usize,
    conc: &DecodeConcurrency,
    frame_bytes: usize,
    cancel: Cancel<'_>,
    parsed_mp4: Option<&mp4parse::MediaContext>,
    on_frame: &mut F,
) -> Result<bool>
where
    F: FnMut(&GrayscaleFrame) -> Result<()>,
{
    let owned_context;
    let context = match parsed_mp4 {
        Some(ctx) => ctx,
        None => match crate::mp4::read_mp4_tolerant_cancellable(path, cancel) {
            Ok(ctx) => {
                owned_context = ctx;
                &owned_context
            }
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(e) => {
                tracing::info!(step = "extract_avcc", reason = %e,
                        "h264/mp4 native path declined");
                return Ok(false);
            }
        },
    };
    let avcc = match crate::mp4::extract_avcc_from_context(context) {
        Ok(a) => a,
        Err(e) => {
            tracing::info!(step = "extract_avcc", reason = %e, "h264/mp4 native path declined");
            return Ok(false);
        }
    };
    let index = match crate::mp4_index::index_mp4_from_context(context) {
        Ok(i) => i,
        Err(e) => {
            tracing::info!(step = "index_mp4", reason = %e, "h264/mp4 native path declined");
            return Ok(false);
        }
    };
    let decoder = match NativeH264Decoder::from_avcc(&avcc) {
        Ok(d) => d,
        Err(e) => {
            tracing::info!(step = "from_avcc", reason = %e, "h264/mp4 native path declined");
            return Ok(false);
        }
    };
    let mut source = match Mp4SampleSource::from_context(context, path) {
        Ok(s) => s,
        Err(e) => {
            tracing::info!(step = "open_source", reason = %e, "h264/mp4 native path declined");
            return Ok(false);
        }
    };

    let targets = fallback::plan_fallback_timestamps(duration_ms, budget);
    let chunk_len = native_fanout_chunk_len(conc.capacity(), frame_bytes);
    stream_native_chunks(
        &targets,
        chunk_len,
        conc,
        cancel,
        |target_ms| {
            let keyframe = nearest_preceding_idr(&index, target_ms)?;
            let step = SparseStep {
                timestamp_ms: target_ms,
                locator: keyframe.sample_number,
            };
            source.fetch(&step).ok()
        },
        |sample| {
            if cancel.fired() {
                return Err(Error::Cancelled);
            }
            decoder.clone().decode_idr(sample, &Codec::H264)
        },
        on_frame,
    )
}

fn try_native_h264_mkv_streaming<F>(
    path: &Path,
    duration_ms: u64,
    budget: usize,
    conc: &DecodeConcurrency,
    frame_bytes: usize,
    cancel: Cancel<'_>,
    on_frame: &mut F,
) -> Result<bool>
where
    F: FnMut(&GrayscaleFrame) -> Result<()>,
{
    let index = match crate::mkv_index::index_mkv(path) {
        Ok(i) => i,
        Err(e) => {
            tracing::info!(step = "index_mkv", reason = %e, "h264/mkv native path declined");
            return Ok(false);
        }
    };
    let Some(codec_private) = index.codec_private.as_deref() else {
        tracing::info!(step = "codec_private", "h264/mkv native path declined");
        return Ok(false);
    };
    let decoder = match NativeH264Decoder::from_avcc(&codec_private) {
        Ok(d) => d,
        Err(e) => {
            tracing::info!(step = "from_avcc", reason = %e, "h264/mkv native path declined");
            return Ok(false);
        }
    };
    let mut source = match MkvSampleSource::open_with_index(path, &index) {
        Ok(s) => s,
        Err(e) => {
            tracing::info!(step = "open_source", reason = %e, "h264/mkv native path declined");
            return Ok(false);
        }
    };

    let targets = fallback::plan_fallback_timestamps(duration_ms, budget);
    let chunk_len = native_fanout_chunk_len(conc.capacity(), frame_bytes);
    stream_native_chunks(
        &targets,
        chunk_len,
        conc,
        cancel,
        |target_ms| {
            let keyframe = nearest_preceding_idr_mkv(&index, target_ms)?;
            let step = SparseStep {
                timestamp_ms: target_ms,
                locator: keyframe.cue_index,
            };
            source.fetch(&step).ok()
        },
        |sample| {
            if cancel.fired() {
                return Err(Error::Cancelled);
            }
            decoder.clone().decode_idr(sample, &Codec::H264)
        },
        on_frame,
    )
}

#[allow(clippy::too_many_arguments)]
fn try_native_h265_mp4_streaming<F>(
    path: &Path,
    duration_ms: u64,
    budget: usize,
    conc: &DecodeConcurrency,
    frame_bytes: usize,
    cancel: Cancel<'_>,
    parsed_mp4: Option<&mp4parse::MediaContext>,
    on_frame: &mut F,
) -> Result<bool>
where
    F: FnMut(&GrayscaleFrame) -> Result<()>,
{
    let hvcc = match crate::mp4::extract_hvcc(path) {
        Ok(h) => h,
        Err(e) => {
            tracing::info!(step = "extract_hvcc", reason = %e, "h265/mp4 native path declined");
            return Ok(false);
        }
    };
    let owned_context;
    let context = match parsed_mp4 {
        Some(ctx) => ctx,
        None => match crate::mp4::read_mp4_tolerant_cancellable(path, cancel) {
            Ok(ctx) => {
                owned_context = ctx;
                &owned_context
            }
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(e) => {
                tracing::info!(step = "index_mp4", reason = %e,
                        "h265/mp4 native path declined");
                return Ok(false);
            }
        },
    };
    let index = match crate::mp4_index::index_mp4_from_context(context) {
        Ok(i) => i,
        Err(e) => {
            tracing::info!(step = "index_mp4", reason = %e, "h265/mp4 native path declined");
            return Ok(false);
        }
    };
    let decoder = match NativeH265Decoder::from_hvcc(&hvcc) {
        Ok(d) => d,
        Err(e) => {
            tracing::info!(step = "from_hvcc", reason = %e, "h265/mp4 native path declined");
            return Ok(false);
        }
    };
    let mut source = match Mp4SampleSource::from_context(context, path) {
        Ok(s) => s,
        Err(e) => {
            tracing::info!(step = "open_source", reason = %e, "h265/mp4 native path declined");
            return Ok(false);
        }
    };

    let targets = fallback::plan_fallback_timestamps(duration_ms, budget);
    let chunk_len = native_fanout_chunk_len(conc.capacity(), frame_bytes);
    stream_native_chunks(
        &targets,
        chunk_len,
        conc,
        cancel,
        |target_ms| {
            let keyframe = nearest_preceding_idr(&index, target_ms)?;
            let step = SparseStep {
                timestamp_ms: target_ms,
                locator: keyframe.sample_number,
            };
            source.fetch(&step).ok()
        },
        |sample| {
            if cancel.fired() {
                return Err(Error::Cancelled);
            }
            decoder.clone().decode_idr(sample, &Codec::H265)
        },
        on_frame,
    )
}

fn try_native_h265_mkv_streaming<F>(
    path: &Path,
    duration_ms: u64,
    budget: usize,
    conc: &DecodeConcurrency,
    frame_bytes: usize,
    cancel: Cancel<'_>,
    on_frame: &mut F,
) -> Result<bool>
where
    F: FnMut(&GrayscaleFrame) -> Result<()>,
{
    let index = match crate::mkv_index::index_mkv(path) {
        Ok(i) => i,
        Err(e) => {
            tracing::info!(step = "index_mkv", reason = %e, "h265/mkv native path declined");
            return Ok(false);
        }
    };
    let Some(hvcc) = index.codec_private.as_deref() else {
        tracing::info!(step = "codec_private", "h265/mkv native path declined");
        return Ok(false);
    };
    let decoder = match NativeH265Decoder::from_hvcc(&hvcc) {
        Ok(d) => d,
        Err(e) => {
            tracing::info!(step = "from_hvcc", reason = %e, "h265/mkv native path declined");
            return Ok(false);
        }
    };
    let mut source = match MkvSampleSource::open_with_index(path, &index) {
        Ok(s) => s,
        Err(e) => {
            tracing::info!(step = "open_source", reason = %e, "h265/mkv native path declined");
            return Ok(false);
        }
    };

    let targets = fallback::plan_fallback_timestamps(duration_ms, budget);
    let chunk_len = native_fanout_chunk_len(conc.capacity(), frame_bytes);
    stream_native_chunks(
        &targets,
        chunk_len,
        conc,
        cancel,
        |target_ms| {
            let keyframe = nearest_preceding_idr_mkv(&index, target_ms)?;
            let step = SparseStep {
                timestamp_ms: target_ms,
                locator: keyframe.cue_index,
            };
            source.fetch(&step).ok()
        },
        |sample| {
            if cancel.fired() {
                return Err(Error::Cancelled);
            }
            decoder.clone().decode_idr(sample, &Codec::H265)
        },
        on_frame,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn missing_file_is_an_error_not_a_panic() {
        let bins = FfmpegBinaries::new(PathBuf::from("ffmpeg"), PathBuf::from("ffprobe"));
        let err = probe_and_decode_sparse(&bins, Path::new("/no/such/clip.mp4"), 4)
            .expect_err("missing file must error");
        assert!(matches!(err, Error::Io(_)), "got {err:?}");
    }

    fn index_with_keyframes(kfs: &[(u32, u64)]) -> Mp4Index {
        Mp4Index {
            timescale: 1000,
            sample_count: u32::try_from(kfs.len()).expect("test keyframe count fits u32"),
            keyframes: kfs
                .iter()
                .map(|&(sample_number, timestamp_ms)| Keyframe {
                    sample_number,
                    timestamp_ms,
                })
                .collect(),
            gops: Vec::new(),
        }
    }

    #[test]
    fn nearest_preceding_idr_lands_on_keyframe_at_or_before_target() {
        let index = index_with_keyframes(&[(1, 0), (91, 3000), (181, 6000)]);

        assert_eq!(nearest_preceding_idr(&index, 0).unwrap().sample_number, 1);
        assert_eq!(
            nearest_preceding_idr(&index, 2500).unwrap().sample_number,
            1
        );
        assert_eq!(
            nearest_preceding_idr(&index, 3000).unwrap().sample_number,
            91
        );
        assert_eq!(
            nearest_preceding_idr(&index, 99_999).unwrap().sample_number,
            181
        );
    }

    #[test]
    fn nearest_preceding_idr_is_none_when_first_keyframe_is_after_target() {
        let index = index_with_keyframes(&[(1, 40), (90, 3040)]);
        assert!(nearest_preceding_idr(&index, 0).is_none());
    }

    #[test]
    fn nearest_preceding_idr_handles_empty_index() {
        let index = index_with_keyframes(&[]);
        assert!(nearest_preceding_idr(&index, 0).is_none());
    }

    #[test]
    fn native_fanout_chunk_len_ordinary_frame_reaches_full_capacity() {
        let frame_1080p = 1920 * 1080;
        assert_eq!(native_fanout_chunk_len(32, frame_1080p), 32);
        assert_eq!(native_fanout_chunk_len(8, frame_1080p), 8);
        assert_eq!(
            native_fanout_chunk_len(16, frame_1080p),
            16,
            "a single large 1080p-class file must fan out past the old fixed-8 ceiling"
        );
    }

    #[test]
    fn native_fanout_chunk_len_8k_frame_stays_memory_bound_at_two() {
        let frame_8k = 33 * 1024 * 1024;
        assert_eq!(native_fanout_chunk_len(32, frame_8k), 2);
        assert_eq!(native_fanout_chunk_len(4, frame_8k), 2);
        assert_eq!(native_fanout_chunk_len(1, frame_8k), 1);
    }

    #[test]
    fn native_fanout_chunk_len_never_exceeds_capacity() {
        for cap in [1usize, 2, 4, 8, 16, 32] {
            for frame_bytes in [4usize, 2_073_600, 33 * 1024 * 1024] {
                assert!(
                    native_fanout_chunk_len(cap, frame_bytes) <= cap,
                    "cap={cap} frame_bytes={frame_bytes} exceeded capacity"
                );
            }
        }
    }

    fn make_frame(value: u64) -> GrayscaleFrame {
        let lo = (value & 0xff) as u8;
        let hi = ((value >> 8) & 0xff) as u8;
        GrayscaleFrame {
            width: 2,
            height: 2,
            timestamp_ms: value,
            pixels: vec![lo, hi, lo.wrapping_add(1), hi.wrapping_add(1)],
        }
    }

    #[test]
    fn stream_native_chunks_order_and_cap_invariance() {
        let targets: Vec<u64> = (0u64..50).map(|i| i * 2500).collect();
        let expected: Vec<GrayscaleFrame> = targets.iter().map(|&t| make_frame(t)).collect();

        let mut per_cap: Vec<Vec<GrayscaleFrame>> = Vec::new();

        for cap in [1usize, 2, 4, 8, 16] {
            let conc = DecodeConcurrency::new(cap);
            let chunk_len = native_fanout_chunk_len(conc.capacity(), 4);
            let mut got: Vec<GrayscaleFrame> = Vec::new();
            let result = stream_native_chunks(
                &targets,
                chunk_len,
                &conc,
                Cancel::default(),
                Some,
                |s: &u64| Ok(make_frame(*s)),
                &mut |f| {
                    got.push(f.clone());
                    Ok(())
                },
            );
            assert!(
                matches!(result, Ok(true)),
                "cap={cap}: expected Ok(true), got {result:?}"
            );
            assert_eq!(got.len(), targets.len(), "cap={cap}: frame count mismatch");
            assert_eq!(
                got, expected,
                "cap={cap}: frame sequence differs from expected"
            );
            per_cap.push(got);
        }

        let baseline = &per_cap[0];
        for (idx, seq) in per_cap.iter().enumerate().skip(1) {
            assert_eq!(
                seq, baseline,
                "cap index {idx}: sequence differs from cap=1 baseline (§J violated)"
            );
        }
    }

    #[test]
    fn stream_native_chunks_cancel_on_first_idr_aborts_promptly() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let targets: Vec<u64> = (0u64..200).map(|i| i * 2500).collect();
        let conc = DecodeConcurrency::serial();
        let chunk_len = native_fanout_chunk_len(conc.capacity(), 4);
        let removal = AtomicBool::new(false);
        let cancel = Cancel {
            pause: None,
            removal: Some(&removal),
        };
        let decode_calls = AtomicUsize::new(0);
        let mut delivered: Vec<GrayscaleFrame> = Vec::new();

        let result = stream_native_chunks(
            &targets,
            chunk_len,
            &conc,
            cancel,
            Some,
            |s: &u64| {
                if cancel.fired() {
                    return Err(Error::Cancelled);
                }
                decode_calls.fetch_add(1, Ordering::Relaxed);
                removal.store(true, Ordering::Relaxed);
                Ok(make_frame(*s))
            },
            &mut |f| {
                delivered.push(f.clone());
                Ok(())
            },
        );

        assert!(
            matches!(result, Err(Error::Cancelled)),
            "first-IDR cancel must abort with Cancelled (never Ok(false)), got {result:?}",
        );
        assert_eq!(
            decode_calls.load(Ordering::Relaxed),
            1,
            "cancel must land after ~1 decode_idr, not after the 200-point grid",
        );
    }

    #[test]
    fn stream_native_chunks_decode_cancelled_pre_delivery_propagates_not_fallback() {
        let targets: Vec<u64> = (0u64..10).map(|i| i * 2500).collect();
        let conc = DecodeConcurrency::new(4);
        let chunk_len = native_fanout_chunk_len(conc.capacity(), 4);
        let mut delivered: Vec<GrayscaleFrame> = Vec::new();
        let result = stream_native_chunks(
            &targets,
            chunk_len,
            &conc,
            Cancel::default(),
            Some,
            |_s: &u64| Err(Error::Cancelled),
            &mut |f| {
                delivered.push(f.clone());
                Ok(())
            },
        );
        assert!(
            matches!(result, Err(Error::Cancelled)),
            "a pre-delivery decode Cancelled must propagate (not Ok(false)→ffmpeg), got {result:?}",
        );
        assert!(
            delivered.is_empty(),
            "no frames delivered on a pre-delivery cancel"
        );
    }

    #[test]
    fn stream_native_chunks_fallback_pre_delivery_resolve_none() {
        let targets: Vec<u64> = (0u64..50).map(|i| i * 2500).collect();
        let first = targets[0];
        let conc = DecodeConcurrency::new(4);
        let chunk_len = native_fanout_chunk_len(conc.capacity(), 4);
        let mut delivered: Vec<GrayscaleFrame> = Vec::new();
        let result = stream_native_chunks(
            &targets,
            chunk_len,
            &conc,
            Cancel::default(),
            |t| if t == first { None } else { Some(t) },
            |s: &u64| Ok(make_frame(*s)),
            &mut |f| {
                delivered.push(f.clone());
                Ok(())
            },
        );
        assert!(
            matches!(result, Ok(false)),
            "pre-delivery None must yield Ok(false), got {result:?}"
        );
        assert!(
            delivered.is_empty(),
            "no frames must be delivered when first resolve returns None"
        );
    }

    #[test]
    fn stream_native_chunks_fallback_post_delivery_resolve_none_is_err() {
        let targets: Vec<u64> = (0u64..50).map(|i| i * 2500).collect();
        let bad_target = targets[3];
        let conc = DecodeConcurrency::new(1);
        let chunk_len = native_fanout_chunk_len(conc.capacity(), 4);
        let mut delivered: Vec<GrayscaleFrame> = Vec::new();
        let result = stream_native_chunks(
            &targets,
            chunk_len,
            &conc,
            Cancel::default(),
            |t| if t == bad_target { None } else { Some(t) },
            |s: &u64| Ok(make_frame(*s)),
            &mut |f| {
                delivered.push(f.clone());
                Ok(())
            },
        );
        assert!(
            result.is_err(),
            "post-delivery None must yield Err, got {result:?}"
        );
        assert_eq!(
            delivered.len(),
            3,
            "expected frames for targets[0..3] to be delivered before error, got {}",
            delivered.len()
        );
        let expected_pre: Vec<GrayscaleFrame> =
            targets[..3].iter().map(|&t| make_frame(t)).collect();
        assert_eq!(
            delivered, expected_pre,
            "delivered frames before error must match targets[0..3]"
        );
    }

    #[test]
    fn stream_native_chunks_fallback_pre_delivery_decode_error() {
        let targets: Vec<u64> = (0u64..10).map(|i| i * 2500).collect();
        let conc = DecodeConcurrency::new(4);
        let chunk_len = native_fanout_chunk_len(conc.capacity(), 4);
        let mut delivered: Vec<GrayscaleFrame> = Vec::new();
        let result = stream_native_chunks(
            &targets,
            chunk_len,
            &conc,
            Cancel::default(),
            Some,
            |_s: &u64| Err(Error::Decode("boom".into())),
            &mut |f| {
                delivered.push(f.clone());
                Ok(())
            },
        );
        assert!(
            matches!(result, Ok(false)),
            "pre-delivery decode error must yield Ok(false), got {result:?}"
        );
        assert!(
            delivered.is_empty(),
            "no frames must be delivered when first chunk decode errors"
        );
    }

    #[test]
    fn stream_native_chunks_fallback_post_delivery_decode_error_is_err() {
        let targets: Vec<u64> = (0u64..10).map(|i| i * 2500).collect();
        let bad_val = targets[3];
        let conc = DecodeConcurrency::new(1);
        let chunk_len = native_fanout_chunk_len(conc.capacity(), 4);
        let mut delivered: Vec<GrayscaleFrame> = Vec::new();
        let result = stream_native_chunks(
            &targets,
            chunk_len,
            &conc,
            Cancel::default(),
            Some,
            move |s: &u64| {
                if *s == bad_val {
                    Err(Error::Decode("boom".into()))
                } else {
                    Ok(make_frame(*s))
                }
            },
            &mut |f| {
                delivered.push(f.clone());
                Ok(())
            },
        );
        assert!(
            result.is_err(),
            "post-delivery decode error must yield Err, got {result:?}"
        );
        assert_eq!(
            delivered.len(),
            3,
            "expected 3 frames before decode error, got {}",
            delivered.len()
        );
    }

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    fn box_header(size: u32, fourcc: &[u8]) -> Vec<u8> {
        let mut v = size.to_be_bytes().to_vec();
        v.extend_from_slice(fourcc);
        v
    }

    fn with_overshoot_garbage(base: &[u8]) -> Vec<u8> {
        let mut v = base.to_vec();
        v.extend_from_slice(&box_header(500_000_000, b"junk"));
        v
    }

    fn write_clip(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clip.mp4");
        std::fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    fn measure_native_index_read_bytes(path: &Path) -> u64 {
        let bins = FfmpegBinaries::new(
            PathBuf::from("/nonexistent/ffmpeg"),
            PathBuf::from("/nonexistent/ffprobe"),
        );
        crate::cancel::THREAD_READ_COUNTER.with(crate::cancel::ThreadReadCounter::start);
        let mut frames = Vec::new();
        let (_metadata, decode_path) = probe_and_decode_sparse_budgets_streaming(
            &bins,
            path,
            4,
            4,
            &DecodeConcurrency::serial(),
            Cancel::default(),
            |f| {
                frames.push(f.clone());
                Ok(())
            },
        )
        .unwrap_or_else(|e| {
            panic!(
                "probe_and_decode_sparse_budgets_streaming({}): {e:?}",
                path.display()
            )
        });
        let bytes = crate::cancel::THREAD_READ_COUNTER.with(crate::cancel::ThreadReadCounter::stop);
        assert_eq!(
            decode_path,
            DecodePath::Native,
            "expected the native path so the measurement covers the mp4parse chain"
        );
        assert!(
            !frames.is_empty(),
            "native decode must deliver at least one frame"
        );
        bytes
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn native_index_read_amplification_stays_near_one_parse_clean_file() {
        let path = fixture("h264-native-e2e/testsrc2_160_90.mp4");
        let file_size = std::fs::metadata(&path).unwrap().len();

        let total_read = measure_native_index_read_bytes(&path);

        let ratio = total_read as f64 / file_size as f64;
        assert!(
            ratio <= 1.2,
            "clean-file read amplification regressed: {total_read} bytes read for a \
             {file_size}-byte file (ratio {ratio:.3}, want <= 1.2x — the shared-parse target)"
        );
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn native_index_read_amplification_stays_bounded_trailing_garbage_file() {
        let base = std::fs::read(fixture("h264-native-e2e/testsrc2_160_90.mp4")).unwrap();
        let craft = with_overshoot_garbage(&base);
        let file_size = craft.len() as u64;
        let (_dir, path) = write_clip(&craft);

        let total_read = measure_native_index_read_bytes(&path);

        let ratio = total_read as f64 / file_size as f64;
        assert!(
            ratio <= 2.4,
            "trailing-garbage read amplification regressed: {total_read} bytes read for a \
             {file_size}-byte crafted file (ratio {ratio:.3}, want <= 2.4x — the \
             shared-parse target for the raw+trimmed retry)"
        );
    }
}
