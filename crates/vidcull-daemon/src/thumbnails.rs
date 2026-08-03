use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, PoisonError};

use vidcull_core::types::Blake3Hash;
use vidcull_parser::fallback::FfmpegBinaries;
use vidcull_thumb::{GrayView, ThumbnailCache, ThumbnailOptions, encode_thumbnail, to_data_uri};

/// Ceiling on how far thumbnail-decode concurrency auto-scales with core
/// count, so a many-core machine doesn't spawn an excessive number of
/// simultaneous ffmpeg processes just for preview generation.
const THUMB_DECODE_CONCURRENCY_CEILING: usize = 16;

pub const THUMB_DECODE_MAX_ENV: &str = "VIDCULL_THUMB_DECODE_MAX";

/// Was a bare constant (3) regardless of core count — the same "fixed cap
/// that never rescales" bug as the old seq_read_gate. Thumbnail decode is one
/// ffmpeg spawn per request (bridge.rs on-demand previews, or background
/// prewarming), so it benefits from tracking available cores like the
/// decode/seq-read gates do; `VIDCULL_THUMB_DECODE_MAX` still lets it be
/// pinned to a fixed value if needed.
fn thumb_decode_concurrency() -> usize {
    thumb_decode_concurrency_from(std::env::var(THUMB_DECODE_MAX_ENV).ok().as_deref())
}

fn thumb_decode_concurrency_from(raw: Option<&str>) -> usize {
    let cores = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);
    let default = cores.clamp(1, THUMB_DECODE_CONCURRENCY_CEILING);
    match raw.map(str::trim).and_then(|v| v.parse::<usize>().ok()) {
        Some(0) | None => default,
        Some(n) => n,
    }
}

struct ThumbConcurrency {
    state: Mutex<usize>,
    cap: usize,
    cv: Condvar,
}

impl ThumbConcurrency {
    fn new(cap: usize) -> Self {
        Self {
            state: Mutex::new(0),
            cap: cap.max(1),
            cv: Condvar::new(),
        }
    }

    fn acquire(&self) -> ThumbPermit<'_> {
        let mut guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        while *guard >= self.cap {
            guard = self.cv.wait(guard).unwrap_or_else(PoisonError::into_inner);
        }
        *guard += 1;
        ThumbPermit { sem: self }
    }
}

struct ThumbPermit<'a> {
    sem: &'a ThumbConcurrency,
}

impl Drop for ThumbPermit<'_> {
    fn drop(&mut self) {
        let mut guard = self
            .sem
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *guard = guard.saturating_sub(1);
        self.sem.cv.notify_one();
    }
}

pub const THUMB_DIR_ENV: &str = "VIDCULL_THUMB_DIR";

pub const THUMB_HWACCEL_ENV: &str = "VIDCULL_THUMB_HWACCEL";

#[must_use]
pub fn thumb_hwaccel_enabled() -> bool {
    match std::env::var(THUMB_HWACCEL_ENV) {
        Ok(val) => parse_hwaccel_val(&val),
        Err(_) => false,
    }
}

#[must_use]
fn parse_hwaccel_val(val: &str) -> bool {
    matches!(val.to_ascii_lowercase().trim(), "1" | "true" | "yes" | "on")
}

#[must_use]
pub fn cache_dir() -> PathBuf {
    if let Some(explicit) = std::env::var_os(THUMB_DIR_ENV) {
        PathBuf::from(explicit)
    } else {
        crate::settings::data_dir().join("thumbs")
    }
}

pub struct ThumbnailProvider {
    cache: ThumbnailCache,
    ffmpeg: Option<FfmpegBinaries>,
    options: ThumbnailOptions,
    concurrency: ThumbConcurrency,
}

impl ThumbnailProvider {
    #[must_use]
    pub fn new(cache_root: PathBuf, ffmpeg: Option<FfmpegBinaries>) -> Self {
        if let Some(ref bins) = ffmpeg {
            tracing::info!(
                ffmpeg = %crate::redact::redact_fs_path(bins.ffmpeg()),
                cache_root = %crate::redact::redact_fs_path(&cache_root),
                "thumbnail provider initialized with ffmpeg decoder"
            );
        } else {
            tracing::warn!(
                cache_root = %crate::redact::redact_fs_path(&cache_root),
                "thumbnail provider initialized WITHOUT ffmpeg decoder (previews only on cache hit)"
            );
        }
        Self {
            cache: ThumbnailCache::new(cache_root),
            ffmpeg,
            options: ThumbnailOptions::default(),
            concurrency: ThumbConcurrency::new(thumb_decode_concurrency()),
        }
    }

    #[must_use]
    pub fn data_uri(&self, path: &Path, content_hash: Option<&Blake3Hash>) -> Option<String> {
        let Some(content_hash) = content_hash else {
            tracing::warn!(
                path = %crate::redact::redact_fs_path(path),
                "cannot generate thumbnail: missing content hash (file might not be indexed/hashed yet)"
            );
            return None;
        };
        let hex = content_hash.to_hex();
        match self
            .cache
            .load_or_store(&hex, 0, || self.decode_and_encode(path))
        {
            Ok(cached) => Some(to_data_uri(&cached.bytes)),
            Err(err) => {
                tracing::warn!(path = %crate::redact::redact_fs_path(path), error = %err, "failed to generate thumbnail for file");
                None
            }
        }
    }

    pub fn store_decoded_frame(
        &self,
        content_hash: &Blake3Hash,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> vidcull_core::Result<()> {
        if width == 0 || height == 0 {
            return Ok(());
        }
        let hex = content_hash.to_hex();
        self.cache.load_or_store(&hex, 0, || {
            encode_thumbnail(
                GrayView {
                    width,
                    height,
                    pixels,
                },
                self.options,
            )
        })?;
        Ok(())
    }

    fn decode_and_encode(&self, path: &Path) -> vidcull_core::Result<Vec<u8>> {
        let _permit = self.concurrency.acquire();
        let bins = self.ffmpeg.as_ref().ok_or_else(|| {
            vidcull_core::Error::Unsupported(
                "thumbnail: no ffmpeg backend available to decode a preview".to_owned(),
            )
        })?;

        let metadata = match vidcull_parser::probe(path) {
            Ok(m) if !m.resolution.is_empty() && m.duration.is_some() => m,
            _ => vidcull_parser::fallback::probe_fallback(bins, path)?,
        };

        if metadata.resolution.width == 0 || metadata.resolution.height == 0 {
            return Err(vidcull_core::Error::Decode(format!(
                "thumbnail: probe reported empty resolution for {}",
                crate::redact::redact_fs_path(path)
            )));
        }

        let duration_ms = metadata
            .duration
            .map_or(0, vidcull_core::VideoDuration::as_millis);
        let target_ts_ms = if duration_ms > 0 {
            (duration_ms / 10).min(5000)
        } else {
            0
        };

        let hwaccel = thumb_hwaccel_enabled();
        let frame = if hwaccel {
            match vidcull_parser::fallback::decode_thumb_frame_at(
                bins,
                path,
                target_ts_ms,
                metadata.resolution.width,
                metadata.resolution.height,
                true,
            ) {
                Ok(f) => f,
                Err(hw_err) => {
                    tracing::warn!(
                        path = %crate::redact::redact_fs_path(path),
                        error = %hw_err,
                        "hwaccel thumbnail decode failed, retrying with software decoder"
                    );
                    vidcull_parser::fallback::decode_thumb_frame_at(
                        bins,
                        path,
                        target_ts_ms,
                        metadata.resolution.width,
                        metadata.resolution.height,
                        false,
                    )?
                }
            }
        } else {
            vidcull_parser::fallback::decode_thumb_frame_at(
                bins,
                path,
                target_ts_ms,
                metadata.resolution.width,
                metadata.resolution.height,
                false,
            )?
        };

        encode_thumbnail(
            GrayView {
                width: frame.width,
                height: frame.height,
                pixels: &frame.pixels,
            },
            self.options,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hwaccel_val_off_for_empty_string() {
        assert!(!parse_hwaccel_val(""), "empty string must be off");
    }

    #[test]
    fn parse_hwaccel_val_on_for_truthy_values() {
        for val in ["1", "true", "True", "TRUE", "yes", "Yes", "on", "ON"] {
            assert!(parse_hwaccel_val(val), "expected ON for {val:?}");
        }
    }

    #[test]
    fn parse_hwaccel_val_off_for_falsy_values() {
        for val in ["0", "false", "False", "no", "off", "random", "2", "enabled"] {
            assert!(!parse_hwaccel_val(val), "expected OFF for {val:?}");
        }
    }

    #[test]
    fn parse_hwaccel_val_trims_whitespace() {
        assert!(parse_hwaccel_val("  1  "));
        assert!(parse_hwaccel_val("\ttrue\n"));
        assert!(!parse_hwaccel_val("  0  "));
    }

    #[test]
    fn sw_thumb_decode_args_have_no_hwaccel() {
        let args =
            vidcull_parser::fallback::thumb_decode_args(0, std::path::Path::new("/x.mp4"), false);
        let rendered: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            !rendered.iter().any(|a| a == "-hwaccel"),
            "SW thumb decode args must not contain -hwaccel: {rendered:?}"
        );
    }

    #[test]
    fn thumb_decode_concurrency_env_overrides_the_core_based_default() {
        assert_eq!(thumb_decode_concurrency_from(Some("2")), 2);
        assert_eq!(thumb_decode_concurrency_from(Some(" 5 ")), 5);
    }

    #[test]
    fn thumb_decode_concurrency_default_is_core_based_and_bounded() {
        let default = thumb_decode_concurrency_from(None);
        assert!(default >= 1, "must never be zero");
        assert!(
            default <= THUMB_DECODE_CONCURRENCY_CEILING,
            "must not exceed the ceiling regardless of core count"
        );
        assert_eq!(
            thumb_decode_concurrency_from(Some("0")),
            default,
            "0 means \"use the core-based default\", same as the seq-read gate convention"
        );
        assert_eq!(
            thumb_decode_concurrency_from(Some("garbage")),
            default,
            "unparsable override falls back to the default"
        );
    }

    #[test]
    fn thumb_concurrency_caps_simultaneous_permits() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        let cap = 3usize;
        let sem = Arc::new(ThumbConcurrency::new(cap));
        let observed_max = Arc::new(AtomicUsize::new(0));
        let in_flight = Arc::new(AtomicUsize::new(0));

        std::thread::scope(|s| {
            for _ in 0..8 {
                let sem = Arc::clone(&sem);
                let observed_max = Arc::clone(&observed_max);
                let in_flight = Arc::clone(&in_flight);
                s.spawn(move || {
                    let _permit = sem.acquire();
                    let current = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    let mut prev = observed_max.load(Ordering::Relaxed);
                    while current > prev {
                        match observed_max.compare_exchange(
                            prev,
                            current,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => break,
                            Err(actual) => prev = actual,
                        }
                    }
                    std::thread::sleep(Duration::from_millis(5));
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });

        let max = observed_max.load(Ordering::Relaxed);
        assert!(
            max <= cap,
            "observed {max} concurrent thumbnail-decode permits > cap {cap}"
        );
        assert!(max >= 1, "no permits were ever observed in flight");
    }

    #[test]
    fn hw_thumb_decode_args_contain_hwaccel_auto() {
        let args =
            vidcull_parser::fallback::thumb_decode_args(0, std::path::Path::new("/x.mp4"), true);
        let rendered: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let hw_pos = rendered
            .iter()
            .position(|a| a == "-hwaccel")
            .expect("-hwaccel must be present in HW thumb args");
        assert_eq!(rendered[hw_pos + 1], "auto");
        let i_pos = rendered.iter().position(|a| a == "-i").expect("-i");
        assert!(hw_pos < i_pos, "-hwaccel must precede -i");
    }
}
