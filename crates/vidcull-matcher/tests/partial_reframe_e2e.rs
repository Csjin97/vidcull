use std::path::Path;

use vidcull_core::Result;
use vidcull_core::types::{Codec, FileId, NormalizedPath};
use vidcull_db::repo::{
    DuplicateGroupsRepo, FilesRepo, Fingerprint, FingerprintsRepo, NewFile, TrustLevel,
};
use vidcull_db::{Database, open_in_memory};
use vidcull_fingerprint::format::encode_tier2;
use vidcull_fingerprint::format::{self, FORMAT_VERSION};
use vidcull_fingerprint::tier1::{GopSignature, Tier1Fingerprint};
use vidcull_fingerprint::{
    DEFAULT_BAR_LIMIT, GrayFrame, Tier2Builder, TimedFrame, build_tier2, trim_uniform_borders,
};
use vidcull_matcher::partial::durable::rebuild_partial_clip_groups_from_fingerprints;
use vidcull_matcher::partial::partial_clip_params;
use vidcull_parser::Cancel;
use vidcull_parser::fallback::{
    DecodeConcurrency, FfmpegBinaries, decode_sparse_with, decode_sparse_with_streaming,
    probe_fallback,
};
use vidcull_synth::render_source;

const CLIP_W: u32 = 540;
const CLIP_H: u32 = 720;
const COMP_W: u32 = 1280;
const COMP_H: u32 = 720;

const FPS: u32 = 30;
const GOP: u32 = 30;

const CLIP_MS: u64 = 16_000;
const CLIP_START_IN_COMP_MS: u64 = 20_000;
const COMP_MS: u64 = 60_000;

const SCENES_PER_SEC: u64 = 4;

fn binaries_or_skip(test: &str) -> Option<FfmpegBinaries> {
    match FfmpegBinaries::resolve() {
        Ok(bins) => Some(bins),
        Err(e) => {
            eprintln!("SKIP {test}: ffmpeg not resolvable ({e})");
            None
        }
    }
}

fn partial_fp(bins: &FfmpegBinaries, path: &Path) -> Option<Vec<u8>> {
    fingerprint(bins, path, true)
}

fn fingerprint_no_trim(bins: &FfmpegBinaries, path: &Path) -> Option<Vec<u8>> {
    fingerprint(bins, path, false)
}

fn fingerprint(bins: &FfmpegBinaries, path: &Path, trim: bool) -> Option<Vec<u8>> {
    let meta = probe_fallback(bins, path)
        .unwrap_or_else(|e| panic!("probe {} failed: {e}", path.display()));
    let dur = meta
        .duration
        .map_or(0, vidcull_core::VideoDuration::as_millis);
    if dur == 0 || meta.resolution.is_empty() {
        return None;
    }
    let budget = usize::try_from((dur * SCENES_PER_SEC) / 1000)
        .unwrap_or(0)
        .max(1);
    let frames = decode_sparse_with(
        bins,
        path,
        dur,
        meta.resolution.width,
        meta.resolution.height,
        budget,
        &meta.codec,
        meta.fps_x1000,
        meta.has_b_frames,
        &DecodeConcurrency::serial(),
    )
    .unwrap_or_else(|e| panic!("decode {} failed: {e}", path.display()));

    let prepared: Vec<(u64, u32, u32, Vec<u8>)> = frames
        .iter()
        .map(|f| {
            if trim {
                let (w, h, px) =
                    trim_uniform_borders(f.width, f.height, &f.pixels, DEFAULT_BAR_LIMIT);
                (f.timestamp_ms, w, h, px)
            } else {
                (f.timestamp_ms, f.width, f.height, f.pixels.clone())
            }
        })
        .collect();
    let timed: Vec<TimedFrame<'_>> = prepared
        .iter()
        .map(|(ts, w, h, px)| TimedFrame {
            timestamp_ms: *ts,
            frame: GrayFrame {
                width: *w,
                height: *h,
                pixels: px,
            },
        })
        .collect();
    let tier2 = build_tier2(&timed);
    if tier2.is_empty() {
        return None;
    }
    Some(encode_tier2(&tier2).expect("encode partial tier2"))
}

fn partial_fp_streaming(bins: &FfmpegBinaries, path: &Path) -> Option<Vec<u8>> {
    let meta = probe_fallback(bins, path)
        .unwrap_or_else(|e| panic!("probe {} failed: {e}", path.display()));
    let dur = meta
        .duration
        .map_or(0, vidcull_core::VideoDuration::as_millis);
    if dur == 0 || meta.resolution.is_empty() {
        return None;
    }
    let budget = usize::try_from((dur * SCENES_PER_SEC) / 1000)
        .unwrap_or(0)
        .max(1);
    let mut builder = Tier2Builder::new();
    decode_sparse_with_streaming(
        bins,
        path,
        dur,
        meta.resolution.width,
        meta.resolution.height,
        budget,
        &meta.codec,
        meta.fps_x1000,
        meta.has_b_frames,
        &DecodeConcurrency::serial(),
        Cancel::default(),
        |frame| {
            let (w, h, px) =
                trim_uniform_borders(frame.width, frame.height, &frame.pixels, DEFAULT_BAR_LIMIT);
            builder.push(&TimedFrame {
                timestamp_ms: frame.timestamp_ms,
                frame: GrayFrame {
                    width: w,
                    height: h,
                    pixels: &px,
                },
            });
            Ok(())
        },
    )
    .unwrap_or_else(|e| panic!("stream decode {} failed: {e}", path.display()));
    let tier2 = builder.finish();
    if tier2.is_empty() {
        return None;
    }
    Some(encode_tier2(&tier2).expect("encode partial tier2"))
}

#[test]
fn streaming_partial_fingerprint_is_byte_identical_to_buffered() {
    let Some(bins) =
        binaries_or_skip("streaming_partial_fingerprint_is_byte_identical_to_buffered")
    else {
        return;
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    let clip = render_clip(&bins, dir);
    let comp = render_compilation(&bins, dir);

    for (label, path) in [("clip", &clip), ("compilation", &comp)] {
        let buffered = partial_fp(&bins, path);
        let streamed = partial_fp_streaming(&bins, path);
        assert!(
            buffered.is_some(),
            "[{label}] buffered partial fp should be Some (fixture yields scenes)"
        );
        assert_eq!(
            buffered, streamed,
            "[{label}] streaming partial blob diverged from buffered build (§J broken)"
        );
    }
}

fn render_clip(bins: &FfmpegBinaries, dir: &Path) -> std::path::PathBuf {
    render_source(
        bins, dir, "clip", "testsrc2", CLIP_MS, CLIP_W, CLIP_H, FPS, GOP,
    )
    .expect("render portrait clip")
}

fn render_compilation(bins: &FfmpegBinaries, dir: &Path) -> std::path::PathBuf {
    let out = dir.join("compilation.mp4");
    let lead_s = seconds(CLIP_START_IN_COMP_MS);
    let trail_s = seconds(COMP_MS - CLIP_START_IN_COMP_MS - CLIP_MS);
    let total_s = seconds(COMP_MS);
    let pad_x = (COMP_W - CLIP_W) / 2;
    let filter = format!(
        "[0:v]trim=0:{lead_s},setpts=PTS-STARTPTS,format=yuv420p[a];\
         [1:v]pad={COMP_W}:{COMP_H}:{pad_x}:0:color=black,setpts=PTS-STARTPTS,format=yuv420p[b];\
         [2:v]trim=0:{trail_s},setpts=PTS-STARTPTS,format=yuv420p[c];\
         [a][b][c]concat=n=3:v=1:a=0[out]"
    );
    let status = std::process::Command::new(bins.ffmpeg())
        .args([
            "-v",
            "error",
            "-hide_banner",
            "-nostdin",
            "-y",
            "-fflags",
            "+bitexact",
        ])
        .args(["-f", "lavfi", "-i"])
        .arg(format!("mandelbrot=size={COMP_W}x{COMP_H}:rate={FPS}"))
        .args(["-f", "lavfi", "-i"])
        .arg(format!("testsrc2=size={CLIP_W}x{CLIP_H}:rate={FPS}"))
        .args(["-f", "lavfi", "-i"])
        .arg(format!("life=size={COMP_W}x{COMP_H}:rate={FPS}:mold=10"))
        .args(["-filter_complex", &filter])
        .args(["-map", "[out]", "-t", &total_s])
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
        ])
        .args([
            "-r",
            &FPS.to_string(),
            "-g",
            &GOP.to_string(),
            "-keyint_min",
            &GOP.to_string(),
        ])
        .args([
            "-sc_threshold",
            "0",
            "-an",
            "-map_metadata",
            "-1",
            "-bitexact",
        ])
        .arg(&out)
        .status()
        .expect("spawn ffmpeg for compilation");
    assert!(status.success(), "compilation render failed: {status}");
    out
}

fn render_negative_control(bins: &FfmpegBinaries, dir: &Path) -> std::path::PathBuf {
    render_source(
        bins,
        dir,
        "control",
        "mandelbrot",
        COMP_MS,
        COMP_W,
        COMP_H,
        FPS,
        GOP,
    )
    .expect("render negative control")
}

fn seconds(ms: u64) -> String {
    format!("{}.{:03}", ms / 1000, ms % 1000)
}

const T0: i64 = 1_700_000_000;
const MTIME: i64 = 1_700_000_000_000_000_000;

fn seed_file(db: &Database, path: &str, duration_ms: u64) -> FileId {
    let new_file = NewFile {
        path: NormalizedPath::new(path),
        size_bytes: 1024,
        mtime_ns: MTIME,
        inode: None,
        content_hash: None,
        codec: Some(Codec::H264),
        container: None,
        duration: Some(vidcull_core::VideoDuration::from_millis(duration_ms)),
        fps_x1000: Some(i32::try_from(FPS * 1000).expect("fps fits i32")),
        bitrate_bps: None,
        resolution: None,
        first_seen_at: T0,
        last_seen_at: T0,
        ..Default::default()
    };
    FilesRepo::new(db.conn())
        .insert(&new_file)
        .expect("insert file")
}

fn seed_with_partial(db: &Database, path: &str, duration_ms: u64, blob: &[u8]) -> FileId {
    let id = seed_file(db, path, duration_ms);
    let t1 = Tier1Fingerprint {
        duration_ms,
        codec: Codec::H264,
        gop: GopSignature::from_durations(&[]),
        global_phash: 0,
    };
    let repo = FingerprintsRepo::new(db.conn());
    repo.upsert(&Fingerprint {
        file_id: id,
        tier1_global: format::encode_tier1(&t1).expect("encode tier1"),
        tier2_temporal: None,
        format_version: u32::from(FORMAT_VERSION),
        created_at: T0,
    })
    .expect("upsert fingerprint");
    repo.set_partial(id, blob).expect("set partial");
    id
}

fn possible_groups(db: &Database) -> Vec<Vec<i64>> {
    let repo = DuplicateGroupsRepo::new(db.conn());
    let mut out = Vec::new();
    for gid in 1..=512 {
        if let Some(group) = repo.get(gid).expect("get group") {
            if group.trust_level == TrustLevel::Possible {
                let mut m: Vec<i64> = repo
                    .list_members(gid)
                    .expect("members")
                    .into_iter()
                    .map(|f| f.0)
                    .collect();
                m.sort_unstable();
                out.push(m);
            }
        }
    }
    out.sort();
    out
}

fn grouped_together(db: &Database, a: FileId, b: FileId) -> bool {
    let pair = {
        let mut v = vec![a.0, b.0];
        v.sort_unstable();
        v
    };
    possible_groups(db).contains(&pair)
}

#[test]
fn reframed_portrait_clip_is_grouped_with_its_pillarboxed_compilation() -> Result<()> {
    let Some(bins) = binaries_or_skip("reframed_portrait_clip_is_grouped") else {
        return Ok(());
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();

    let clip_path = render_clip(&bins, dir);
    let comp_path = render_compilation(&bins, dir);
    let control_path = render_negative_control(&bins, dir);

    let clip_blob = partial_fp(&bins, &clip_path).expect("clip yields a partial fingerprint");
    let comp_blob =
        partial_fp(&bins, &comp_path).expect("compilation yields a partial fingerprint");
    let control_blob =
        partial_fp(&bins, &control_path).expect("control yields a partial fingerprint");

    let clip_scenes = format::decode_tier2(&clip_blob)?.scenes.len();
    let comp_scenes = format::decode_tier2(&comp_blob)?.scenes.len();
    eprintln!("[reframe] clip_scenes={clip_scenes} comp_scenes={comp_scenes}");
    assert!(
        clip_scenes >= partial_clip_params().min_scenes(),
        "clip must have at least min_scenes ({}) informative scenes, got {clip_scenes}",
        partial_clip_params().min_scenes()
    );
    assert!(
        comp_scenes > clip_scenes,
        "compilation must be strictly longer than the clip (source-longer gate); \
         clip={clip_scenes} comp={comp_scenes}"
    );

    let mut db = open_in_memory()?;
    let clip_id = seed_with_partial(&db, "/v/clip.mp4", CLIP_MS, &clip_blob);
    let comp_id = seed_with_partial(&db, "/v/compilation.mp4", COMP_MS, &comp_blob);
    let control_id = seed_with_partial(&db, "/v/control.mp4", COMP_MS, &control_blob);

    let out = rebuild_partial_clip_groups_from_fingerprints(&mut db, partial_clip_params(), T0)?;
    eprintln!(
        "[reframe] groups_created={} possible_groups={:?}",
        out.groups_created,
        possible_groups(&db)
    );

    assert!(
        grouped_together(&db, clip_id, comp_id),
        "reframed clip must be grouped with the compilation it is pillarboxed into \
         (clip_id={clip_id:?} comp_id={comp_id:?}); possible_groups={:?}",
        possible_groups(&db)
    );

    assert!(
        !grouped_together(&db, clip_id, control_id),
        "clip must NOT be grouped with the unrelated negative control"
    );
    assert!(
        !grouped_together(&db, comp_id, control_id),
        "compilation must NOT be grouped with the unrelated negative control"
    );

    let clip_raw = fingerprint_no_trim(&bins, &clip_path).expect("clip raw fp");
    let comp_raw = fingerprint_no_trim(&bins, &comp_path).expect("comp raw fp");
    let mut db_raw = open_in_memory()?;
    let clip_raw_id = seed_with_partial(&db_raw, "/v/clip.mp4", CLIP_MS, &clip_raw);
    let comp_raw_id = seed_with_partial(&db_raw, "/v/compilation.mp4", COMP_MS, &comp_raw);
    rebuild_partial_clip_groups_from_fingerprints(&mut db_raw, partial_clip_params(), T0)?;
    eprintln!(
        "[reframe] no-trim possible_groups={:?}",
        possible_groups(&db_raw)
    );
    assert!(
        !grouped_together(&db_raw, clip_raw_id, comp_raw_id),
        "without active-region trim the pillarboxed clip must NOT match — the trim \
         is the operative step; a match here means the test is not actually \
         exercising normalization"
    );

    Ok(())
}

fn is_confirmed_full_dup(db: &Database, file_id: FileId) -> bool {
    DuplicateGroupsRepo::new(db.conn())
        .find_groups_containing(file_id)
        .expect("groups containing")
        .iter()
        .any(|g| matches!(g.trust_level, TrustLevel::Exact))
}

#[test]
fn near_dup_source_gate_allows_reframe_detection() -> Result<()> {
    let Some(bins) = binaries_or_skip("near_dup_source_gate") else {
        return Ok(());
    };
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();

    let clip_path = render_clip(&bins, dir);
    let comp_path = render_compilation(&bins, dir);
    let clip_blob = partial_fp(&bins, &clip_path).expect("clip partial fp");
    let comp_blob = partial_fp(&bins, &comp_path).expect("comp partial fp");

    let mut db = open_in_memory()?;
    let clip_id = seed_with_partial(&db, "/v/clip.mp4", CLIP_MS, &clip_blob);
    let comp_id = seed_with_partial(&db, "/v/compilation.mp4", COMP_MS, &comp_blob);
    let sibling_id = seed_with_partial(&db, "/v/compilation_nearvariant.mp4", COMP_MS, &comp_blob);

    {
        let groups = DuplicateGroupsRepo::new(db.conn());
        let gid = groups
            .create(TrustLevel::VeryLikely, T0)
            .expect("create vl group");
        groups.add_member(gid, comp_id).expect("add comp");
        groups.add_member(gid, sibling_id).expect("add sibling");
    }

    let comp_gated = is_confirmed_full_dup(&db, comp_id);
    let clip_gated = is_confirmed_full_dup(&db, clip_id);
    eprintln!(
        "[gate] compilation confirmed_full_dup={comp_gated}, clip confirmed_full_dup={clip_gated}"
    );
    assert!(
        !comp_gated,
        "a VERY_LIKELY compilation must NOT be a confirmed full-dup: it can still contain a reframed sub-clip, so it must keep its partial"
    );
    assert!(
        !clip_gated,
        "the clip is not in any whole-file group, so it is not gated"
    );

    rebuild_partial_clip_groups_from_fingerprints(&mut db, partial_clip_params(), T0)?;

    assert!(
        grouped_together(&db, clip_id, comp_id),
        "a reframed clip must be grouped with its compilation even when that \
         compilation is a VERY_LIKELY near-dup of a sibling (clip_id={clip_id:?} \
         comp_id={comp_id:?}); possible_groups={:?}",
        possible_groups(&db)
    );

    Ok(())
}
