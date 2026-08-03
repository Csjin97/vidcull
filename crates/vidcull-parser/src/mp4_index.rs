use std::path::Path;

use vidcull_core::{Error, Result};

use crate::cancel::Cancel;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keyframe {
    pub sample_number: u32,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gop {
    pub start_sample: u32,
    pub size: u32,
    pub start_timestamp_ms: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mp4Index {
    pub timescale: u32,
    pub sample_count: u32,
    pub keyframes: Vec<Keyframe>,
    pub gops: Vec<Gop>,
}

pub fn index_mp4<P: AsRef<Path>>(path: P) -> Result<Mp4Index> {
    index_mp4_cancellable(path, Cancel::default())
}

pub fn index_mp4_cancellable<P: AsRef<Path>>(path: P, cancel: Cancel<'_>) -> Result<Mp4Index> {
    let context = crate::mp4::read_mp4_tolerant_cancellable(path.as_ref(), cancel)?;
    index_mp4_from_context(&context)
}

pub(crate) fn index_mp4_from_context(context: &mp4parse::MediaContext) -> Result<Mp4Index> {
    let track = context
        .tracks
        .iter()
        .find(|t| matches!(t.track_type, mp4parse::TrackType::Video))
        .ok_or_else(|| Error::Parse("mp4: no video track found".into()))?;

    let timescale = track
        .timescale
        .as_ref()
        .map(|t| t.0)
        .ok_or_else(|| Error::Parse("mp4: video track has no mdhd timescale".into()))?;
    let timescale = u32::try_from(timescale)
        .map_err(|_| Error::Parse("mp4: timescale exceeds u32 range".into()))?;

    let time_to_sample_box = track
        .stts
        .as_ref()
        .ok_or_else(|| Error::Parse("mp4: video track has no stts (time-to-sample) box".into()))?;
    let stts_entries: Vec<(u32, u32)> = time_to_sample_box
        .samples
        .iter()
        .map(|s| (s.sample_count, s.sample_delta))
        .collect();
    let (sample_count, track_ticks) = stts_totals(&stts_entries)?;

    let sync_samples: Vec<u32> = match track.stss.as_ref() {
        Some(sync_sample_box) => {
            let samples: Vec<u32> = sync_sample_box.samples.iter().copied().collect();
            if samples.is_empty() {
                return Err(Error::Parse(
                    "mp4: stss present but contains zero entries".into(),
                ));
            }
            validate_stss(&samples, sample_count)?;
            samples
        }
        None => synthesise_all_sync(sample_count)?,
    };

    let keyframe_ticks = collect_keyframe_decode_ticks(&stts_entries, &sync_samples)?;
    let keyframes = keyframe_ticks
        .iter()
        .map(|&(sample, dt)| Keyframe {
            sample_number: sample,
            timestamp_ms: rescale_to_ms(dt, u64::from(timescale)),
        })
        .collect::<Vec<_>>();
    let gops = build_gops(&keyframe_ticks, sample_count, track_ticks, timescale);

    Ok(Mp4Index {
        timescale,
        sample_count,
        keyframes,
        gops,
    })
}

fn stts_totals(entries: &[(u32, u32)]) -> Result<(u32, u64)> {
    let mut total_samples: u32 = 0;
    let mut total_ticks: u64 = 0;
    for &(count, delta) in entries {
        total_samples = total_samples
            .checked_add(count)
            .ok_or_else(|| Error::Parse("mp4: stts sample count exceeds u32 range".into()))?;
        let segment_ticks = u64::from(count).saturating_mul(u64::from(delta));
        total_ticks = total_ticks.checked_add(segment_ticks).ok_or_else(|| {
            Error::Parse("mp4: stts cumulative duration exceeds u64 range".into())
        })?;
    }
    Ok((total_samples, total_ticks))
}

fn validate_stss(stss: &[u32], sample_count: u32) -> Result<()> {
    let first = stss.first().copied().unwrap_or(0);
    if first == 0 {
        return Err(Error::Parse(
            "mp4: stss sample number 0 is invalid (samples are 1-based)".into(),
        ));
    }
    for pair in stss.windows(2) {
        if pair[0] >= pair[1] {
            return Err(Error::Parse(
                "mp4: stss entries are not strictly ascending".into(),
            ));
        }
    }
    if let Some(&last) = stss.last() {
        if last > sample_count {
            return Err(Error::Parse(format!(
                "mp4: stss sample {last} exceeds total sample count {sample_count}"
            )));
        }
    }
    Ok(())
}

fn synthesise_all_sync(sample_count: u32) -> Result<Vec<u32>> {
    if sample_count == 0 {
        return Err(Error::Parse(
            "mp4: video track has no stss and zero samples (nothing to index)".into(),
        ));
    }
    Ok((1..=sample_count).collect())
}

fn collect_keyframe_decode_ticks(
    stts_entries: &[(u32, u32)],
    sync_samples: &[u32],
) -> Result<Vec<(u32, u64)>> {
    let mut out: Vec<(u32, u64)> = Vec::with_capacity(sync_samples.len());
    let mut next_sync_idx = 0usize;
    let mut current_sample: u32 = 0;
    let mut current_dt: u64 = 0;
    'outer: for &(count, delta) in stts_entries {
        for _ in 0..count {
            current_sample = current_sample.checked_add(1).ok_or_else(|| {
                Error::Parse("mp4: sample index overflow during stts walk".into())
            })?;
            if next_sync_idx < sync_samples.len() && sync_samples[next_sync_idx] == current_sample {
                out.push((current_sample, current_dt));
                next_sync_idx += 1;
                if next_sync_idx >= sync_samples.len() {
                    break 'outer;
                }
            }
            current_dt = current_dt
                .checked_add(u64::from(delta))
                .ok_or_else(|| Error::Parse("mp4: cumulative decode time overflowed u64".into()))?;
        }
    }
    if next_sync_idx < sync_samples.len() {
        return Err(Error::Parse(
            "mp4: stss references sample numbers not present in stts".into(),
        ));
    }
    Ok(out)
}

fn build_gops(
    keyframes: &[(u32, u64)],
    total_samples: u32,
    total_ticks: u64,
    timescale: u32,
) -> Vec<Gop> {
    let mut gops = Vec::with_capacity(keyframes.len());
    for (idx, &(start_sample, start_dt)) in keyframes.iter().enumerate() {
        let (next_sample, next_dt) = if idx + 1 < keyframes.len() {
            keyframes[idx + 1]
        } else {
            (total_samples.saturating_add(1), total_ticks)
        };
        let size = next_sample.saturating_sub(start_sample);
        let duration_ticks = next_dt.saturating_sub(start_dt);
        gops.push(Gop {
            start_sample,
            size,
            start_timestamp_ms: rescale_to_ms(start_dt, u64::from(timescale)),
            duration_ms: rescale_to_ms(duration_ticks, u64::from(timescale)),
        });
    }
    gops
}

fn rescale_to_ms(ticks: u64, timescale: u64) -> u64 {
    if timescale == 0 {
        return 0;
    }
    let t = u128::from(ticks);
    let ts = u128::from(timescale);
    let scaled = (t * 1000 + ts / 2) / ts;
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stts_totals_sums_count_and_duration() {
        let entries = vec![(30u32, 512u32), (15, 1024)];
        let (count, ticks) = stts_totals(&entries).unwrap();
        assert_eq!(count, 45);
        assert_eq!(ticks, 30 * 512 + 15 * 1024);
    }

    #[test]
    fn stts_totals_detects_sample_count_overflow() {
        let entries = vec![(u32::MAX, 1u32), (1, 1)];
        assert!(stts_totals(&entries).is_err());
    }

    #[test]
    fn validate_stss_rejects_zero_index() {
        assert!(validate_stss(&[0, 5], 10).is_err());
    }

    #[test]
    fn validate_stss_rejects_non_monotonic() {
        assert!(validate_stss(&[1, 5, 3], 10).is_err());
        assert!(validate_stss(&[1, 1], 10).is_err());
    }

    #[test]
    fn validate_stss_rejects_out_of_range() {
        assert!(validate_stss(&[1, 50], 30).is_err());
    }

    #[test]
    fn validate_stss_accepts_monotonic_in_range() {
        assert!(validate_stss(&[1, 5, 9, 30], 30).is_ok());
    }

    #[test]
    fn synthesise_all_sync_lists_every_sample_one_based() {
        assert_eq!(synthesise_all_sync(4).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(synthesise_all_sync(1).unwrap(), vec![1]);
    }

    #[test]
    fn synthesise_all_sync_rejects_empty_track() {
        assert!(synthesise_all_sync(0).is_err());
    }

    #[test]
    fn all_sync_walk_stamps_every_sample_from_stts() {
        let stts = vec![(4u32, 512u32)];
        let sync = synthesise_all_sync(4).unwrap();
        let out = collect_keyframe_decode_ticks(&stts, &sync).unwrap();
        assert_eq!(out, vec![(1, 0), (2, 512), (3, 1024), (4, 1536)]);
    }

    #[test]
    fn all_sync_gops_are_one_sample_each() {
        let kfs = vec![(1u32, 0u64), (2, 512), (3, 1024), (4, 1536)];
        let gops = build_gops(&kfs, 4, 4 * 512, 2048);
        assert_eq!(gops.len(), 4);
        assert!(gops.iter().all(|g| g.size == 1));
        assert_eq!(gops[0].duration_ms, 250);
        assert_eq!(gops[3].start_timestamp_ms, 750);
        assert_eq!(gops[3].duration_ms, 250);
    }

    #[test]
    fn keyframe_dt_walk_lockstep_with_single_keyframe_at_origin() {
        let time_to_sample = vec![(30u32, 512u32)];
        let sync_samples = vec![1u32];
        let out = collect_keyframe_decode_ticks(&time_to_sample, &sync_samples).unwrap();
        assert_eq!(out, vec![(1, 0)]);
    }

    #[test]
    fn keyframe_dt_walk_handles_multiple_keyframes() {
        let time_to_sample = vec![(30u32, 512u32)];
        let sync_samples = vec![1u32, 11, 21];
        let out = collect_keyframe_decode_ticks(&time_to_sample, &sync_samples).unwrap();
        assert_eq!(out, vec![(1, 0), (11, 5120), (21, 10240)]);
    }

    #[test]
    fn keyframe_dt_walk_crosses_stts_segment_boundary() {
        let time_to_sample = vec![(10u32, 1000u32), (10, 500)];
        let sync_samples = vec![1u32, 11];
        let out = collect_keyframe_decode_ticks(&time_to_sample, &sync_samples).unwrap();
        assert_eq!(out, vec![(1, 0), (11, 10_000)]);
    }

    #[test]
    fn rescale_to_ms_matches_existing_helper() {
        assert_eq!(rescale_to_ms(15_360, 15_360), 1000);
        assert_eq!(rescale_to_ms(0, 15_360), 0);
        assert_eq!(rescale_to_ms(7_680, 15_360), 500);
    }

    #[test]
    fn rescale_to_ms_handles_zero_timescale_gracefully() {
        assert_eq!(rescale_to_ms(1_000, 0), 0);
    }

    #[test]
    fn build_gops_emits_one_gop_for_single_keyframe() {
        let kfs = vec![(1u32, 0u64)];
        let gops = build_gops(&kfs, 30, 15_360, 15_360);
        assert_eq!(
            gops,
            vec![Gop {
                start_sample: 1,
                size: 30,
                start_timestamp_ms: 0,
                duration_ms: 1000,
            }]
        );
    }

    #[test]
    fn build_gops_splits_evenly_between_idrs() {
        let kfs = vec![(1u32, 0u64), (11, 5_120), (21, 10_240)];
        let gops = build_gops(&kfs, 30, 30 * 512, 15_360);
        assert_eq!(gops.len(), 3);
        assert_eq!(gops[0].size, 10);
        assert_eq!(gops[1].size, 10);
        assert_eq!(gops[2].size, 10);
        assert_eq!(gops[0].duration_ms, 333);
        assert_eq!(gops[2].start_timestamp_ms, 667);
    }
}
