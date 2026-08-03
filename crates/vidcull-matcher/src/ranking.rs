use std::cmp::Ordering;

use vidcull_core::Result;
use vidcull_core::types::{BestCopyMode, Codec, FileId, Resolution};
use vidcull_db::Database;
#[cfg(test)]
use vidcull_db::repo::FilesRepo;
use vidcull_db::repo::{DaemonSettingsRepo, DuplicateGroup, DuplicateGroupsRepo};

#[must_use]
pub fn codec_efficiency_x100(codec: Option<&Codec>, mode: BestCopyMode) -> u64 {
    match mode {
        BestCopyMode::Archival
        | BestCopyMode::MinSize
        | BestCopyMode::Compatible
        | BestCopyMode::MaxResolution => 100,
        BestCopyMode::SpaceSaving | BestCopyMode::MaxQuality => match codec {
            Some(Codec::Av1) => 350,
            Some(Codec::H265) => 300,
            Some(Codec::Vp9) => 260,
            Some(Codec::H264) => 200,
            Some(Codec::Mpeg2 | Codec::Other(_)) | None => 100,
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct QualityScore {
    pub pixels: u64,
    pub encoder_score: u32,
    pub laplacian_variance: u64,
    pub dct_energy: u64,
    pub bpp_scaled: u64,
    pub effective_bitrate: u64,
    pub size_bytes: u64,
}

impl QualityScore {
    #[must_use]
    pub fn scalar(self) -> u64 {
        self.pixels
            .saturating_mul(10_000_000)
            .saturating_add(u64::from(self.encoder_score).saturating_mul(1_000_000))
            .saturating_add(self.laplacian_variance.saturating_mul(100))
            .saturating_add(self.dct_energy.saturating_mul(10))
            .saturating_add(self.bpp_scaled / 1000)
            .saturating_add(self.effective_bitrate / 1000)
            .saturating_add(self.size_bytes / 1_000_000)
    }
}

#[must_use]
#[allow(
    clippy::too_many_arguments,
    clippy::bool_to_int_with_if,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn score_quality(
    resolution: Option<Resolution>,
    bitrate_bps: Option<i64>,
    codec: Option<&Codec>,
    container: Option<&str>,
    size_bytes: i64,
    laplacian_variance: Option<f64>,
    dct_energy: Option<f64>,
    bpp: Option<f64>,
    encoder_tags: Option<&str>,
    mode: BestCopyMode,
) -> QualityScore {
    let pixels = if mode == BestCopyMode::MinSize {
        0
    } else {
        resolution
            .filter(|r| !r.is_empty())
            .map_or(0, Resolution::pixels)
    };

    let encoder_score = if mode == BestCopyMode::MinSize {
        0
    } else if mode == BestCopyMode::MaxQuality || mode == BestCopyMode::MaxResolution {
        1
    } else if mode == BestCopyMode::Compatible {
        let mut score = 0;
        if let Some(Codec::H264) = codec {
            score += 5;
        }
        if let Some(cont) = container {
            if cont.to_lowercase().contains("mp4") {
                score += 5;
            }
        }
        score
    } else if let Some(tags) = encoder_tags {
        let tags_lc = tags.to_lowercase();
        let re_encode_keywords = [
            "handbrake",
            "svt-av1",
            "svtav1",
            "x264",
            "x265",
            "xvid",
            "divx",
            "lavc",
            "ffmpeg",
        ];
        if re_encode_keywords.iter().any(|&k| tags_lc.contains(k)) {
            0
        } else {
            1
        }
    } else {
        1
    };

    let laplacian_variance = if mode == BestCopyMode::MinSize {
        0
    } else {
        laplacian_variance.map_or(0, |v| (v * 1000.0).max(0.0) as u64)
    };

    let dct_energy = if mode == BestCopyMode::MinSize {
        0
    } else {
        dct_energy.map_or(0, |v| (v * 1000.0).max(0.0) as u64)
    };

    let bpp_scaled = if mode == BestCopyMode::MinSize {
        0
    } else {
        bpp.map_or(0, |v| (v * 1_000_000.0).max(0.0) as u64)
    };

    let effective_bitrate = if mode == BestCopyMode::MinSize {
        0
    } else {
        let raw_bitrate = bitrate_bps.and_then(|b| u64::try_from(b).ok()).unwrap_or(0);
        raw_bitrate.saturating_mul(codec_efficiency_x100(codec, mode))
    };

    let raw_size = u64::try_from(size_bytes).unwrap_or(0);
    let size_bytes = if mode == BestCopyMode::MinSize {
        u64::MAX.saturating_sub(raw_size)
    } else {
        raw_size
    };

    QualityScore {
        pixels,
        encoder_score,
        laplacian_variance,
        dct_energy,
        bpp_scaled,
        effective_bitrate,
        size_bytes,
    }
}

#[must_use]
pub fn select_best<I>(candidates: I) -> Option<FileId>
where
    I: IntoIterator<Item = (FileId, QualityScore)>,
{
    candidates
        .into_iter()
        .reduce(|best, cur| match cur.1.cmp(&best.1) {
            Ordering::Greater => cur,
            Ordering::Less => best,
            Ordering::Equal => {
                if cur.0 < best.0 {
                    cur
                } else {
                    best
                }
            }
        })
        .map(|(id, _)| id)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BestCopyOutcome {
    pub groups_updated: usize,
    pub groups_unchanged: usize,
    pub groups_without_active_members: usize,
}

pub fn assign_best_copies(db: &mut Database, now_unix_s: i64) -> Result<BestCopyOutcome> {
    assign_best_copies_joined(db, now_unix_s)
}

#[cfg(test)]
thread_local! {
    static READ_QUERY_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn bump_read_query_count() {
    READ_QUERY_COUNT.with(|c| c.set(c.get() + 1));
}

fn load_best_copy_mode(settings_repo: &DaemonSettingsRepo<'_>) -> Result<BestCopyMode> {
    #[cfg(test)]
    bump_read_query_count();
    Ok(settings_repo
        .load()?
        .and_then(|bytes| postcard::from_bytes::<vidcull_ipc::DaemonSettings>(&bytes).ok())
        .map(|s| s.best_copy_mode)
        .unwrap_or_default())
}

fn finalize_group(
    groups: &DuplicateGroupsRepo<'_>,
    outcome: &mut BestCopyOutcome,
    group: &DuplicateGroup,
    scored: Vec<(FileId, QualityScore)>,
    now_unix_s: i64,
) -> Result<()> {
    let best = select_best(scored);
    if best.is_none() {
        outcome.groups_without_active_members += 1;
    }
    if best == group.best_file_id {
        outcome.groups_unchanged += 1;
    } else {
        groups.set_best(group.id, best, now_unix_s)?;
        outcome.groups_updated += 1;
    }
    Ok(())
}

#[cfg(test)]
fn assign_best_copies_legacy(db: &mut Database, now_unix_s: i64) -> Result<BestCopyOutcome> {
    db.transaction(|conn| {
        let groups = DuplicateGroupsRepo::new(conn);
        let files = FilesRepo::new(conn);
        let settings_repo = DaemonSettingsRepo::new(conn);
        let mode = load_best_copy_mode(&settings_repo)?;

        let mut outcome = BestCopyOutcome::default();
        for group in groups.list_all()? {
            let mut scored: Vec<(FileId, QualityScore)> = Vec::new();
            for member in groups.list_members(group.id)? {
                let Some(record) = files.get(member)? else {
                    continue;
                };
                if record.deleted_at.is_some() {
                    continue;
                }
                scored.push((
                    member,
                    score_quality(
                        record.resolution,
                        record.bitrate_bps,
                        record.codec.as_ref(),
                        record.container.as_deref(),
                        record.size_bytes,
                        record.laplacian_variance,
                        record.dct_energy,
                        record.bpp,
                        record.encoder_tags.as_deref(),
                        mode,
                    ),
                ));
            }
            finalize_group(&groups, &mut outcome, &group, scored, now_unix_s)?;
        }
        Ok(outcome)
    })
}

fn assign_best_copies_joined(db: &mut Database, now_unix_s: i64) -> Result<BestCopyOutcome> {
    db.transaction(|conn| {
        let groups = DuplicateGroupsRepo::new(conn);
        let settings_repo = DaemonSettingsRepo::new(conn);
        let mode = load_best_copy_mode(&settings_repo)?;

        #[cfg(test)]
        bump_read_query_count();
        let rows = groups.list_groups_with_member_records()?;

        let mut outcome = BestCopyOutcome::default();
        for (group, members) in rows {
            let scored: Vec<(FileId, QualityScore)> = members
                .into_iter()
                .map(|record| {
                    let score = score_quality(
                        record.resolution,
                        record.bitrate_bps,
                        record.codec.as_ref(),
                        record.container.as_deref(),
                        record.size_bytes,
                        record.laplacian_variance,
                        record.dct_energy,
                        record.bpp,
                        record.encoder_tags.as_deref(),
                        mode,
                    );
                    (record.file_id, score)
                })
                .collect();
            finalize_group(&groups, &mut outcome, &group, scored, now_unix_s)?;
        }
        Ok(outcome)
    })
}

#[cfg(test)]
mod tests {
    use vidcull_core::types::NormalizedPath;
    use vidcull_db::open_in_memory;
    use vidcull_db::repo::{NewFile, TrustLevel};

    use super::*;

    #[expect(clippy::unnecessary_wraps, reason = "ergonomic test constructor")]
    fn res(w: u32, h: u32) -> Option<Resolution> {
        Some(Resolution::new(w, h))
    }

    #[test]
    fn codec_efficiency_ranks_modern_codecs_higher() {
        assert!(
            codec_efficiency_x100(Some(&Codec::Av1), BestCopyMode::SpaceSaving)
                > codec_efficiency_x100(Some(&Codec::H265), BestCopyMode::SpaceSaving)
        );
        assert!(
            codec_efficiency_x100(Some(&Codec::H265), BestCopyMode::SpaceSaving)
                > codec_efficiency_x100(Some(&Codec::H264), BestCopyMode::SpaceSaving)
        );
        assert!(
            codec_efficiency_x100(Some(&Codec::H264), BestCopyMode::SpaceSaving)
                > codec_efficiency_x100(Some(&Codec::Mpeg2), BestCopyMode::SpaceSaving)
        );
    }

    #[test]
    fn unknown_codec_gets_conservative_baseline() {
        let baseline = codec_efficiency_x100(Some(&Codec::Mpeg2), BestCopyMode::SpaceSaving);
        assert_eq!(
            codec_efficiency_x100(None, BestCopyMode::SpaceSaving),
            baseline
        );
        assert_eq!(
            codec_efficiency_x100(
                Some(&Codec::Other("prores".into())),
                BestCopyMode::SpaceSaving
            ),
            baseline,
        );
    }

    fn score_test(
        resolution: Option<Resolution>,
        bitrate_bps: Option<i64>,
        codec: Option<&Codec>,
        size_bytes: i64,
    ) -> QualityScore {
        score_quality(
            resolution,
            bitrate_bps,
            codec,
            None,
            size_bytes,
            None,
            None,
            None,
            None,
            BestCopyMode::SpaceSaving,
        )
    }

    #[test]
    fn higher_resolution_dominates_bitrate() {
        let uhd = score_test(res(3840, 2160), Some(1_000_000), Some(&Codec::H264), 10);
        let sd = score_test(res(640, 480), Some(99_000_000), Some(&Codec::H264), 10);
        assert!(uhd > sd);
    }

    #[test]
    fn at_equal_resolution_effective_bitrate_decides() {
        let h264 = score_test(res(1920, 1080), Some(8_000_000), Some(&Codec::H264), 100);
        let h265_low = score_test(res(1920, 1080), Some(3_000_000), Some(&Codec::H265), 100);
        let h265_high = score_test(res(1920, 1080), Some(6_000_000), Some(&Codec::H265), 100);
        assert!(h264 > h265_low);
        assert!(h265_high > h264);
    }

    #[test]
    fn known_metadata_outranks_missing() {
        let known = score_test(res(1280, 720), Some(2_000_000), Some(&Codec::H264), 500);
        let no_res = score_test(None, Some(2_000_000), Some(&Codec::H264), 500);
        let no_bitrate = score_test(res(1280, 720), None, Some(&Codec::H264), 500);
        assert!(known > no_res, "known resolution beats unknown");
        assert!(known > no_bitrate, "known bitrate beats unknown");
    }

    #[test]
    fn empty_resolution_scores_zero_pixels() {
        let zero = score_test(
            Some(Resolution::new(0, 1080)),
            Some(1),
            Some(&Codec::H264),
            1,
        );
        assert_eq!(zero.pixels, 0);
    }

    #[test]
    fn filesize_breaks_ties_when_res_and_bitrate_match() {
        let big = score_test(
            res(1920, 1080),
            Some(5_000_000),
            Some(&Codec::H264),
            2_000_000,
        );
        let small = score_test(
            res(1920, 1080),
            Some(5_000_000),
            Some(&Codec::H264),
            1_000_000,
        );
        assert!(big > small);
    }

    #[test]
    fn select_best_picks_highest_score() {
        let best = select_best([
            (
                FileId(1),
                score_test(res(640, 480), Some(1_000_000), Some(&Codec::H264), 10),
            ),
            (
                FileId(2),
                score_test(res(1920, 1080), Some(1_000_000), Some(&Codec::H264), 10),
            ),
            (
                FileId(3),
                score_test(res(1280, 720), Some(1_000_000), Some(&Codec::H264), 10),
            ),
        ]);
        assert_eq!(best, Some(FileId(2)));
    }

    #[test]
    fn select_best_breaks_exact_ties_on_smallest_id() {
        let s = score_test(res(1920, 1080), Some(5_000_000), Some(&Codec::H264), 100);
        let best = select_best([(FileId(9), s), (FileId(4), s), (FileId(7), s)]);
        assert_eq!(best, Some(FileId(4)));
    }

    #[test]
    fn select_best_on_empty_is_none() {
        assert_eq!(select_best(std::iter::empty()), None);
    }

    #[test]
    fn scalar_is_monotonic_with_ord() {
        let low = score_test(res(640, 480), Some(1_000_000), Some(&Codec::H264), 10);
        let high = score_test(res(3840, 2160), Some(1_000_000), Some(&Codec::H264), 10);
        assert!(high > low);
        assert!(high.scalar() > low.scalar());
    }

    #[test]
    fn original_tags_outrank_reencoded() {
        let clean = score_quality(
            res(1920, 1080),
            Some(5_000_000),
            Some(&Codec::H264),
            None,
            100,
            None,
            None,
            None,
            None,
            BestCopyMode::SpaceSaving,
        );
        let reencoded = score_quality(
            res(1920, 1080),
            Some(5_000_000),
            Some(&Codec::H264),
            None,
            100,
            None,
            None,
            None,
            Some("encoder:handbrake"),
            BestCopyMode::SpaceSaving,
        );
        assert!(clean > reencoded);
    }

    #[test]
    fn detail_metrics_rank_sharper_first() {
        let sharp = score_quality(
            res(1920, 1080),
            Some(5_000_000),
            Some(&Codec::H264),
            None,
            100,
            Some(120.0),
            Some(45.0),
            None,
            None,
            BestCopyMode::SpaceSaving,
        );
        let blur = score_quality(
            res(1920, 1080),
            Some(5_000_000),
            Some(&Codec::H264),
            None,
            100,
            Some(20.0),
            Some(45.0),
            None,
            None,
            BestCopyMode::SpaceSaving,
        );
        assert!(sharp > blur);

        let high_dct = score_quality(
            res(1920, 1080),
            Some(5_000_000),
            Some(&Codec::H264),
            None,
            100,
            Some(50.0),
            Some(300.0),
            None,
            None,
            BestCopyMode::SpaceSaving,
        );
        let low_dct = score_quality(
            res(1920, 1080),
            Some(5_000_000),
            Some(&Codec::H264),
            None,
            100,
            Some(50.0),
            Some(50.0),
            None,
            None,
            BestCopyMode::SpaceSaving,
        );
        assert!(high_dct > low_dct);
    }

    #[test]
    fn bpp_penalizes_over_compression() {
        let normal_bpp = score_quality(
            res(1920, 1080),
            Some(5_000_000),
            Some(&Codec::H264),
            None,
            100,
            Some(50.0),
            Some(50.0),
            Some(0.12),
            None,
            BestCopyMode::SpaceSaving,
        );
        let low_bpp = score_quality(
            res(1920, 1080),
            Some(5_000_000),
            Some(&Codec::H264),
            None,
            100,
            Some(50.0),
            Some(50.0),
            Some(0.02),
            None,
            BestCopyMode::SpaceSaving,
        );
        assert!(normal_bpp > low_bpp);
    }

    #[test]
    fn archival_mode_ignores_codec_efficiency_boost() {
        let h264 = score_quality(
            res(1920, 1080),
            Some(8_000_000),
            Some(&Codec::H264),
            None,
            100,
            None,
            None,
            None,
            None,
            BestCopyMode::Archival,
        );
        let h265 = score_quality(
            res(1920, 1080),
            Some(6_000_000),
            Some(&Codec::H265),
            None,
            100,
            None,
            None,
            None,
            None,
            BestCopyMode::Archival,
        );
        assert!(h264 > h265);
    }

    #[test]
    fn max_quality_mode_ignores_encoder_tags_penalty() {
        let clean = score_quality(
            res(1920, 1080),
            Some(5_000_000),
            Some(&Codec::H264),
            None,
            100,
            None,
            None,
            None,
            None,
            BestCopyMode::MaxQuality,
        );
        let reencoded = score_quality(
            res(1920, 1080),
            Some(5_000_000),
            Some(&Codec::H264),
            None,
            100,
            None,
            None,
            None,
            Some("encoder:handbrake"),
            BestCopyMode::MaxQuality,
        );
        assert_eq!(clean, reencoded);
    }

    #[test]
    fn min_size_mode_picks_smallest_file() {
        let big = score_quality(
            res(3840, 2160),
            Some(15_000_000),
            Some(&Codec::H265),
            None,
            2_000_000,
            Some(100.0),
            Some(100.0),
            None,
            None,
            BestCopyMode::MinSize,
        );
        let small = score_quality(
            res(640, 480),
            Some(500_000),
            Some(&Codec::H264),
            None,
            100_000,
            Some(10.0),
            Some(10.0),
            None,
            None,
            BestCopyMode::MinSize,
        );
        assert!(small > big);
    }

    #[test]
    fn compatible_mode_prefers_h264_and_mp4() {
        let h264_mp4 = score_quality(
            res(1920, 1080),
            Some(5_000_000),
            Some(&Codec::H264),
            Some("mp4"),
            100,
            None,
            None,
            None,
            None,
            BestCopyMode::Compatible,
        );
        let h264_mkv = score_quality(
            res(1920, 1080),
            Some(5_000_000),
            Some(&Codec::H264),
            Some("mkv"),
            100,
            None,
            None,
            None,
            None,
            BestCopyMode::Compatible,
        );
        let h265_mp4 = score_quality(
            res(1920, 1080),
            Some(5_000_000),
            Some(&Codec::H265),
            Some("mp4"),
            100,
            None,
            None,
            None,
            None,
            BestCopyMode::Compatible,
        );
        assert!(h264_mp4 > h264_mkv);
        assert!(h264_mp4 > h265_mp4);
    }

    fn seed_file(
        db: &Database,
        path: &str,
        resolution: Option<Resolution>,
        bitrate_bps: Option<i64>,
        codec: Option<Codec>,
        size_bytes: i64,
    ) -> FileId {
        let new_file = NewFile {
            path: NormalizedPath::new(path),
            size_bytes,
            mtime_ns: 0,
            codec,
            bitrate_bps,
            resolution,
            first_seen_at: 0,
            last_seen_at: 0,
            ..Default::default()
        };
        FilesRepo::new(db.conn()).insert(&new_file).unwrap()
    }

    fn insert_orphan_member(db: &Database, group_id: i64, file_id: i64) {
        db.conn()
            .execute_batch("PRAGMA foreign_keys = OFF;")
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO duplicate_group_members (group_id, file_id) VALUES (?1, ?2)",
                [group_id, file_id],
            )
            .unwrap();
        db.conn()
            .execute_batch("PRAGMA foreign_keys = ON;")
            .unwrap();
    }

    fn force_best_file_id(db: &Database, group_id: i64, file_id: i64) {
        db.conn()
            .execute_batch("PRAGMA foreign_keys = OFF;")
            .unwrap();
        db.conn()
            .execute(
                "UPDATE duplicate_groups SET best_file_id = ?1 WHERE id = ?2",
                [file_id, group_id],
            )
            .unwrap();
        db.conn()
            .execute_batch("PRAGMA foreign_keys = ON;")
            .unwrap();
    }

    struct FixtureIds {
        all_orphan: i64,
        all_deleted: i64,
        memberless: i64,
        mixed: i64,
        mixed_live: FileId,
        healthy: i64,
        healthy_best: FileId,
    }

    fn seed_equivalence_fixture(db: &Database) -> FixtureIds {
        let groups = DuplicateGroupsRepo::new(db.conn());

        let all_orphan = groups.create(TrustLevel::Exact, 0).unwrap();
        insert_orphan_member(db, all_orphan, 9_201);
        insert_orphan_member(db, all_orphan, 9_202);
        force_best_file_id(db, all_orphan, 9_201);

        let all_deleted = groups.create(TrustLevel::Exact, 0).unwrap();
        let d1 = seed_file(
            db,
            "eq_deleted1.mp4",
            res(1920, 1080),
            Some(5_000_000),
            Some(Codec::H264),
            100,
        );
        let d2 = seed_file(
            db,
            "eq_deleted2.mp4",
            res(1280, 720),
            Some(2_000_000),
            Some(Codec::H264),
            60,
        );
        groups.add_member(all_deleted, d1).unwrap();
        groups.add_member(all_deleted, d2).unwrap();
        groups.set_best(all_deleted, Some(d1), 0).unwrap();
        FilesRepo::new(db.conn()).mark_deleted(d1, 1).unwrap();
        FilesRepo::new(db.conn()).mark_deleted(d2, 1).unwrap();

        let memberless = groups.create(TrustLevel::Possible, 0).unwrap();

        let mixed = groups.create(TrustLevel::VeryLikely, 0).unwrap();
        let mixed_live = seed_file(
            db,
            "eq_mixed_live.mp4",
            res(1280, 720),
            Some(3_000_000),
            Some(Codec::H264),
            40,
        );
        let mixed_gone = seed_file(
            db,
            "eq_mixed_gone.mp4",
            res(3840, 2160),
            Some(9_000_000),
            Some(Codec::H265),
            80,
        );
        groups.add_member(mixed, mixed_live).unwrap();
        groups.add_member(mixed, mixed_gone).unwrap();
        FilesRepo::new(db.conn())
            .mark_deleted(mixed_gone, 1)
            .unwrap();
        insert_orphan_member(db, mixed, 9_203);

        let healthy = groups.create(TrustLevel::Exact, 0).unwrap();
        let healthy_worse = seed_file(
            db,
            "eq_healthy_sd.mp4",
            res(640, 480),
            Some(1_000_000),
            Some(Codec::H264),
            10,
        );
        let healthy_best = seed_file(
            db,
            "eq_healthy_hd.mp4",
            res(1920, 1080),
            Some(5_000_000),
            Some(Codec::H264),
            100,
        );
        groups.add_member(healthy, healthy_worse).unwrap();
        groups.add_member(healthy, healthy_best).unwrap();

        FixtureIds {
            all_orphan,
            all_deleted,
            memberless,
            mixed,
            mixed_live,
            healthy,
            healthy_best,
        }
    }

    fn best_of(db: &Database, gid: i64) -> Option<FileId> {
        DuplicateGroupsRepo::new(db.conn())
            .get(gid)
            .unwrap()
            .unwrap()
            .best_file_id
    }

    #[test]
    fn legacy_and_joined_paths_produce_identical_outcomes_across_fixtures() {
        let mut legacy_db = open_in_memory().unwrap();
        let mut joined_db = open_in_memory().unwrap();
        let legacy_ids = seed_equivalence_fixture(&legacy_db);
        let joined_ids = seed_equivalence_fixture(&joined_db);

        let legacy_out = assign_best_copies_legacy(&mut legacy_db, 1_000).unwrap();
        let joined_out = assign_best_copies_joined(&mut joined_db, 1_000).unwrap();

        assert_eq!(legacy_out, joined_out, "BestCopyOutcome counters diverge");
        assert_eq!(legacy_out.groups_updated, 4);
        assert_eq!(legacy_out.groups_unchanged, 1);
        assert_eq!(legacy_out.groups_without_active_members, 3);

        assert_eq!(best_of(&legacy_db, legacy_ids.all_orphan), None);
        assert_eq!(best_of(&joined_db, joined_ids.all_orphan), None);
        assert_eq!(best_of(&legacy_db, legacy_ids.all_deleted), None);
        assert_eq!(best_of(&joined_db, joined_ids.all_deleted), None);

        assert_eq!(best_of(&legacy_db, legacy_ids.memberless), None);
        assert_eq!(best_of(&joined_db, joined_ids.memberless), None);

        assert_eq!(
            best_of(&legacy_db, legacy_ids.mixed),
            Some(legacy_ids.mixed_live)
        );
        assert_eq!(
            best_of(&joined_db, joined_ids.mixed),
            Some(joined_ids.mixed_live)
        );

        assert_eq!(
            best_of(&legacy_db, legacy_ids.healthy),
            Some(legacy_ids.healthy_best)
        );
        assert_eq!(
            best_of(&joined_db, joined_ids.healthy),
            Some(joined_ids.healthy_best)
        );
    }

    #[test]
    fn joined_driver_makes_exactly_two_read_calls() {
        let mut db = open_in_memory().unwrap();
        let a = seed_file(
            &db,
            "ac5_a.mp4",
            res(1280, 720),
            Some(2_000_000),
            Some(Codec::H264),
            10,
        );
        let b = seed_file(
            &db,
            "ac5_b.mp4",
            res(1920, 1080),
            Some(4_000_000),
            Some(Codec::H264),
            20,
        );
        let groups = DuplicateGroupsRepo::new(db.conn());
        let gid = groups.create(TrustLevel::Exact, 0).unwrap();
        groups.add_member(gid, a).unwrap();
        groups.add_member(gid, b).unwrap();

        READ_QUERY_COUNT.with(|c| c.set(0));
        assign_best_copies_joined(&mut db, 1_000).unwrap();
        let count = READ_QUERY_COUNT.with(std::cell::Cell::get);

        assert_eq!(
            count, 2,
            "expected exactly 2 read calls (settings load + JOIN), got {count}",
        );
    }
}
