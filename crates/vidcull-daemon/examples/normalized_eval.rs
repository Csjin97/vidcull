#![allow(clippy::cast_possible_wrap)]

use std::path::Path;
use std::path::PathBuf;

use vidcull_core::FileId;
use vidcull_fingerprint::tier2::Tier2Fingerprint;
use vidcull_fingerprint::{
    DEFAULT_BAR_LIMIT, GrayFrame, TimedFrame, build_tier2, hamming_distance, trim_uniform_borders,
};
use vidcull_matcher::partial::{AnchorParams, plan_partial_clips};
use vidcull_parser::fallback::{FfmpegBinaries, decode_sparse, probe_fallback};
use vidcull_parser::probe_and_decode_sparse;
use vidcull_parser::sparse::GrayscaleFrame;

struct Built {
    raw: Tier2Fingerprint,
    norm: Tier2Fingerprint,
    decoded: usize,
}

#[allow(dead_code)]
struct FileModes {
    id: FileId,
    native_norm: Tier2Fingerprint,
    fa_norm: Tier2Fingerprint,
    native_scenes: usize,
    fa_scenes: usize,
    name: String,
}

fn decode_frame_accurate(bins: &FfmpegBinaries, path: &Path) -> Vec<GrayscaleFrame> {
    let meta = probe_fallback(bins, path).expect("ffprobe");
    let dur = meta
        .duration
        .map_or(0, vidcull_core::VideoDuration::as_millis);
    decode_sparse(
        bins,
        path,
        dur,
        meta.resolution.width,
        meta.resolution.height,
        10_000,
    )
    .expect("frame-accurate decode")
}

fn cache_paths(path: &Path, frame_accurate: bool) -> (PathBuf, PathBuf) {
    let stem = path.file_name().and_then(|s| s.to_str()).unwrap_or("clip");
    let mode = if frame_accurate { "ffmpeg" } else { "native" };
    let dir = std::env::temp_dir().join("vidcull_eval_cache");
    let _ = std::fs::create_dir_all(&dir);
    (
        dir.join(format!("{stem}.{mode}.raw.t2")),
        dir.join(format!("{stem}.{mode}.norm.t2")),
    )
}

fn build(bins: &FfmpegBinaries, path: &Path, frame_accurate: bool) -> Built {
    let (raw_cache, norm_cache) = cache_paths(path, frame_accurate);
    if let (Ok(r), Ok(n)) = (std::fs::read(&raw_cache), std::fs::read(&norm_cache)) {
        if let (Ok(raw), Ok(norm)) = (
            Tier2Fingerprint::from_bytes(&r),
            Tier2Fingerprint::from_bytes(&n),
        ) {
            let decoded = norm.scenes.len();
            return Built { raw, norm, decoded };
        }
    }
    let frames: Vec<GrayscaleFrame> = if frame_accurate {
        decode_frame_accurate(bins, path)
    } else {
        probe_and_decode_sparse(bins, path, 10_000)
            .expect("decode")
            .frames
    };
    let raw_timed: Vec<TimedFrame> = frames
        .iter()
        .map(|f| TimedFrame {
            timestamp_ms: f.timestamp_ms,
            frame: GrayFrame {
                width: f.width,
                height: f.height,
                pixels: &f.pixels,
            },
        })
        .collect();
    let raw = build_tier2(&raw_timed);
    let trimmed: Vec<(u64, u32, u32, Vec<u8>)> = frames
        .iter()
        .map(|f| {
            let (w, h, px) = trim_uniform_borders(f.width, f.height, &f.pixels, DEFAULT_BAR_LIMIT);
            (f.timestamp_ms, w, h, px)
        })
        .collect();
    let norm_timed: Vec<TimedFrame> = trimmed
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
    let norm = build_tier2(&norm_timed);
    if let (Ok(rb), Ok(nb)) = (raw.to_bytes(), norm.to_bytes()) {
        let _ = std::fs::write(&raw_cache, rb);
        let _ = std::fs::write(&norm_cache, nb);
    }
    Built {
        raw,
        norm,
        decoded: frames.len(),
    }
}

fn run(label: &str, corpus: &[(FileId, Tier2Fingerprint)], params: AnchorParams) {
    let plan = plan_partial_clips(corpus.to_vec(), params);
    print!("  {label}: ");
    if plan.matches.is_empty() {
        println!("(no matches)");
        return;
    }
    println!();
    for m in &plan.matches {
        let a = &m.alignment;
        println!(
            "    clip {} ⊂ source {}  matched {}/{}  cov={}  [{}..{}ms]",
            m.clip.0,
            a.source.0,
            a.matched_scenes,
            a.clip_scenes,
            a.coverage_x1000,
            a.start_ms,
            a.end_ms
        );
    }
}

fn gate_run(
    label: &str,
    corpus: Vec<(FileId, Tier2Fingerprint)>,
    params: AnchorParams,
) -> Vec<(FileId, FileId, usize)> {
    let plan = plan_partial_clips(corpus, params);
    println!("\n--- {label} ---");
    if plan.matches.is_empty() {
        println!("  (no matches)");
    }
    for m in &plan.matches {
        let a = &m.alignment;
        println!(
            "  clip {} ⊂ source {}  matched {}/{}  [{}..{}ms]",
            m.clip.0, a.source.0, a.matched_scenes, a.clip_scenes, a.start_ms, a.end_ms
        );
    }
    plan.matches
        .into_iter()
        .map(|m| (m.clip, m.alignment.source, m.alignment.matched_scenes))
        .collect()
}

#[allow(clippy::too_many_lines, clippy::similar_names)]
fn run_gate(bins: &FfmpegBinaries, paths: &[String]) {
    println!("=== Phase-0 Recall Gate ===");
    println!(
        "Building fingerprints (native + frame-accurate) for {} files …",
        paths.len()
    );

    let mut files: Vec<FileModes> = Vec::new();
    for (i, p) in paths.iter().enumerate() {
        let id = FileId(i as i64 + 1);
        let name = p
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(p.as_str())
            .to_string();
        println!("\n[{}] {} …", id.0, name);
        let nat = build(bins, Path::new(p), false);
        let fa = build(bins, Path::new(p), true);
        println!(
            "  native  decoded={} raw={} norm={}",
            nat.decoded,
            nat.raw.scenes.len(),
            nat.norm.scenes.len()
        );
        println!(
            "  fa      decoded={} raw={} norm={}",
            fa.decoded,
            fa.raw.scenes.len(),
            fa.norm.scenes.len()
        );
        files.push(FileModes {
            id,
            native_scenes: nat.norm.scenes.len(),
            fa_scenes: fa.norm.scenes.len(),
            native_norm: nat.norm,
            fa_norm: fa.norm,
            name,
        });
    }

    let params = AnchorParams::new(AnchorParams::DEFAULT_BANDS, 6, 1000, 3)
        .expect("params")
        .with_min_matched(3);

    let ff_corpus: Vec<(FileId, Tier2Fingerprint)> =
        files.iter().map(|f| (f.id, f.fa_norm.clone())).collect();
    let ff_results = gate_run(
        "BASELINE frame-accurate / frame-accurate (FF)",
        ff_corpus,
        params,
    );

    if ff_results.is_empty() {
        println!("\nWARNING: baseline found 0 TP pairs — gate cannot measure recall loss.");
        println!("VERDICT: HALT (baseline 0 TP)");
        return;
    }

    let ff_source_ids: Vec<FileId> = {
        let mut v: Vec<FileId> = ff_results.iter().map(|&(_, s, _)| s).collect();
        v.sort_unstable_by_key(|id| id.0);
        v.dedup();
        v
    };

    println!("\nBaseline TP pairs ({}):", ff_results.len());
    for &(c, s, m) in &ff_results {
        let c_name = files
            .iter()
            .find(|f| f.id == c)
            .map_or("?", |f| f.name.as_str());
        let s_name = files
            .iter()
            .find(|f| f.id == s)
            .map_or("?", |f| f.name.as_str());
        println!(
            "  clip {}({}) ⊂ source {}({})  ff_matched={}",
            c.0, c_name, s.0, s_name, m
        );
    }

    let nn_corpus: Vec<(FileId, Tier2Fingerprint)> = files
        .iter()
        .map(|f| (f.id, f.native_norm.clone()))
        .collect();
    let nn_results = gate_run("CASE 1  native / native  (NN)", nn_corpus, params);

    println!("\nCase-2 corpus modes (clips→native, sources→fa):");
    let nf_corpus: Vec<(FileId, Tier2Fingerprint)> = files
        .iter()
        .map(|f| {
            let is_source = ff_source_ids.contains(&f.id);
            let mode = if is_source { "fa" } else { "native" };
            println!("  [{}] {} → {}", f.id.0, f.name, mode);
            let fp = if is_source {
                f.fa_norm.clone()
            } else {
                f.native_norm.clone()
            };
            (f.id, fp)
        })
        .collect();
    let nf_results = gate_run("CASE 2  native_clip / fa_source  (NF)", nf_corpus, params);

    println!("\nCase-3 corpus modes (clips→fa, sources→native):");
    let fn_corpus: Vec<(FileId, Tier2Fingerprint)> = files
        .iter()
        .map(|f| {
            let is_source = ff_source_ids.contains(&f.id);
            let mode = if is_source { "native" } else { "fa" };
            println!("  [{}] {} → {}", f.id.0, f.name, mode);
            let fp = if is_source {
                f.native_norm.clone()
            } else {
                f.fa_norm.clone()
            };
            (f.id, fp)
        })
        .collect();
    let fn_results = gate_run("CASE 3  fa_clip / native_source  (FN)", fn_corpus, params);

    println!("\n=== GATE SUMMARY ===");

    let find_match =
        |results: &[(FileId, FileId, usize)], clip: FileId, src: FileId| -> Option<usize> {
            results
                .iter()
                .find(|&&(c, s, _)| c == clip && s == src)
                .map(|&(_, _, m)| m)
        };

    let has_pair = |results: &[(FileId, FileId, usize)], clip: FileId, src: FileId| -> bool {
        results.iter().any(|&(c, s, _)| c == clip && s == src)
    };

    let mut case1_pass = true;
    let mut case2_pass = true;
    let mut case3_pass = true;

    for &(clip, src, ff_m) in &ff_results {
        match find_match(&nn_results, clip, src) {
            None => {
                println!(
                    "  Case1 FAIL: clip {} ⊂ source {} — NOT FOUND by native \
                     (ff_matched={})",
                    clip.0, src.0, ff_m
                );
                case1_pass = false;
            }
            Some(n) => {
                let verdict = if n >= ff_m {
                    "OK  "
                } else if n >= 3 {
                    "WARN"
                } else {
                    case1_pass = false;
                    "FAIL"
                };
                println!(
                    "  Case1 {}: clip {} ⊂ source {}  native={} fa={} (margin {})",
                    verdict,
                    clip.0,
                    src.0,
                    n,
                    ff_m,
                    (n as i64) - (ff_m as i64)
                );
            }
        }

        match find_match(&nf_results, clip, src) {
            None => {
                println!(
                    "  Case2 FAIL: clip {} ⊂ source {} — NOT FOUND in NF",
                    clip.0, src.0
                );
                case2_pass = false;
            }
            Some(n) if n < 3 => {
                println!(
                    "  Case2 FAIL: clip {} ⊂ source {}  matched={} < 3",
                    clip.0, src.0, n
                );
                case2_pass = false;
            }
            Some(n) => {
                println!(
                    "  Case2 OK:   clip {} ⊂ source {}  matched={}",
                    clip.0, src.0, n
                );
            }
        }

        match find_match(&fn_results, clip, src) {
            None => {
                println!(
                    "  Case3 FAIL: clip {} ⊂ source {} — NOT FOUND in FN",
                    clip.0, src.0
                );
                case3_pass = false;
            }
            Some(n) if n < 3 => {
                println!(
                    "  Case3 FAIL: clip {} ⊂ source {}  matched={} < 3",
                    clip.0, src.0, n
                );
                case3_pass = false;
            }
            Some(n) => {
                println!(
                    "  Case3 OK:   clip {} ⊂ source {}  matched={}",
                    clip.0, src.0, n
                );
            }
        }
    }

    let mut fp_count = 0usize;
    for (plan_name, results) in [
        ("NN", &nn_results),
        ("NF", &nf_results),
        ("FN", &fn_results),
    ] {
        for &(c, s, _) in results {
            if !has_pair(&ff_results, c, s) {
                println!(
                    "  FP {}: clip {} ⊂ source {} — new match absent from baseline",
                    plan_name, c.0, s.0
                );
                fp_count += 1;
            }
        }
    }
    if fp_count == 0 {
        println!("  Controls: 0 false positives vs baseline (separation OK)");
    }

    println!();
    println!(
        "Case 1 (NN  native/native):       {}",
        if case1_pass { "PASS" } else { "FAIL" }
    );
    println!(
        "Case 2 (NF  native_clip/fa_src):  {}",
        if case2_pass { "PASS" } else { "FAIL" }
    );
    println!(
        "Case 3 (FN  fa_clip/native_src):  {}",
        if case3_pass { "PASS" } else { "FAIL" }
    );
    println!(
        "Controls (no new false positives): {}",
        if fp_count == 0 { "PASS" } else { "FAIL" }
    );

    if case1_pass && case2_pass && case3_pass && fp_count == 0 {
        println!("\nVERDICT: GO");
    } else {
        println!("\nVERDICT: HALT");
    }
}

#[allow(clippy::too_many_lines)]
fn run_phash_survival(bins: &FfmpegBinaries, paths: &[String]) {
    if paths.len() < 2 {
        eprintln!("usage: normalized_eval --phash-survival <clip> <source>");
        std::process::exit(2);
    }
    let clip_path = &paths[0];
    let source_path = &paths[1];

    println!("=== pHash Survival Check ===");
    println!("clip:   {clip_path}");
    println!("source: {source_path}");
    println!();

    let clip_name = clip_path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(clip_path.as_str());
    let src_name = source_path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(source_path.as_str());

    println!("Decoding clip   ({clip_name}) — native IDR + trim …");
    let clip_built = build(bins, Path::new(clip_path), false);
    println!(
        "  decoded={} scenes={}",
        clip_built.decoded,
        clip_built.norm.scenes.len()
    );

    println!("Decoding source ({src_name}) — native IDR + trim …");
    let src_built = build(bins, Path::new(source_path), false);
    println!(
        "  decoded={} scenes={}",
        src_built.decoded,
        src_built.norm.scenes.len()
    );

    let clip_scenes = &clip_built.norm.scenes;
    let src_scenes = &src_built.norm.scenes;

    println!();
    if clip_scenes.is_empty() || src_scenes.is_empty() {
        println!("ERROR: empty scene list — cannot measure.");
        std::process::exit(1);
    }

    println!(
        "{:<6}  {:>12}  {:>8}  {:>14}  {:>12}",
        "ci", "clip_ts_ms", "min_dist", "nearest_src_ms", "offset_ms"
    );
    println!("{}", "-".repeat(60));

    let thresholds: [u32; 4] = [4, 6, 8, 10];
    let mut counts = [0usize; 4];
    let mut offsets_at_6: Vec<i64> = Vec::new();

    for (ci, cs) in clip_scenes.iter().enumerate() {
        let mut min_dist = u32::MAX;
        let mut nearest_src_ms = 0u64;
        for ss in src_scenes {
            let d = hamming_distance(cs.phash, ss.phash);
            if d < min_dist {
                min_dist = d;
                nearest_src_ms = ss.timestamp_ms;
            }
        }
        let offset_ms = nearest_src_ms as i64 - cs.timestamp_ms as i64;
        println!(
            "{:<6}  {:>12}  {:>8}  {:>14}  {:>12}",
            ci, cs.timestamp_ms, min_dist, nearest_src_ms, offset_ms
        );
        for (ti, &t) in thresholds.iter().enumerate() {
            if min_dist <= t {
                counts[ti] += 1;
            }
        }
        if min_dist <= 6 {
            offsets_at_6.push(offset_ms);
        }
    }

    println!();
    println!("=== Threshold Summary ===");
    println!("  production config: max_distance=6, min_matched=3 (PASS requires ≥ 3 at d≤6)");
    println!("{:>10}  {:>10}", "max_dist", "matched");
    for (ti, &t) in thresholds.iter().enumerate() {
        let mark = if t == 6 { " <-- production" } else { "" };
        println!("{:>10}  {:>6}/{}{}", t, counts[ti], clip_scenes.len(), mark);
    }

    println!();
    println!("=== Timestamp Offsets (src_ts − clip_ts) for matches at d ≤ 6 ===");
    if offsets_at_6.is_empty() {
        println!("  (no matches within distance 6)");
    } else {
        for (i, off) in offsets_at_6.iter().enumerate() {
            println!("  match {i}: {off:+}ms");
        }
        let min_off = offsets_at_6.iter().copied().min().unwrap_or(0);
        let max_off = offsets_at_6.iter().copied().max().unwrap_or(0);
        let spread = max_off - min_off;
        println!("  offset range: {min_off:+}ms .. {max_off:+}ms  (spread {spread}ms)");
        if spread < 5_000 {
            println!("  -> offsets CLUSTER (spread < 5 s) — Hough would find consensus");
        } else {
            println!("  -> offsets SPREAD  (spread >= 5 s) — Hough alignment uncertain");
        }
    }

    let matched_at_6 = counts[1];
    let prod_min_matched: usize = 3;

    println!();
    println!("=== VERDICT ===");
    if matched_at_6 >= prod_min_matched {
        println!(
            "PASS — {}/{} clip scenes phash-match a source scene at d≤6 (need ≥{})",
            matched_at_6,
            clip_scenes.len(),
            prod_min_matched
        );
        println!(
            "Timestamp-offset alignment rewrite IS worth building \
             (signal survives to matching layer)."
        );
    } else {
        println!(
            "FAIL — {}/{} clip scenes phash-match a source scene at d≤6 (need ≥{})",
            matched_at_6,
            clip_scenes.len(),
            prod_min_matched
        );
        println!(
            "Native pHash signal is destroyed before alignment — \
             alignment rewrite CANNOT recover the pair (2nd HALT)."
        );
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let gate_mode = args.first().map(String::as_str) == Some("--gate");
    if gate_mode {
        args.remove(0);
    }
    let survival_mode = args.first().map(String::as_str) == Some("--phash-survival");
    if survival_mode {
        args.remove(0);
    }
    let frame_accurate = args.first().map(String::as_str) == Some("--ffmpeg");
    if frame_accurate {
        args.remove(0);
    }
    let paths = args;
    if paths.is_empty() {
        eprintln!(
            "usage: normalized_eval [--gate] [--phash-survival] [--ffmpeg] <file1> <file2> ..."
        );
        std::process::exit(2);
    }

    let bins = FfmpegBinaries::new(PathBuf::from("ffmpeg"), PathBuf::from("ffprobe"));

    if gate_mode {
        run_gate(&bins, &paths);
        return;
    }

    if survival_mode {
        run_phash_survival(&bins, &paths);
        return;
    }

    println!(
        "decode: {}",
        if frame_accurate {
            "FRAME-ACCURATE (ffmpeg -ss)"
        } else {
            "NATIVE (preceding IDR)"
        }
    );

    let mut raw_corpus = Vec::new();
    let mut norm_corpus = Vec::new();
    for (i, p) in paths.iter().enumerate() {
        let id = FileId(i as i64 + 1);
        let b = build(&bins, Path::new(p), frame_accurate);
        let name = p.rsplit(['/', '\\']).next().unwrap_or(p);
        println!(
            "file {} decoded={} raw_scenes={} norm_scenes={}  {}",
            id.0,
            b.decoded,
            b.raw.scenes.len(),
            b.norm.scenes.len(),
            name
        );
        raw_corpus.push((id, b.raw));
        norm_corpus.push((id, b.norm));
    }
    let default = AnchorParams::default();
    let bands = AnchorParams::DEFAULT_BANDS;
    let mg = |maxd: u32, m: usize| {
        AnchorParams::new(bands, maxd, 1000, 3)
            .expect("params")
            .with_min_matched(m)
    };

    println!("\n===== RAW (full frame) =====");
    run("default(cov600,d6)", &raw_corpus, default);
    run("matched-gate(d6,m3)", &raw_corpus, mg(6, 3));
    run("matched-gate(d8,m3)", &raw_corpus, mg(8, 3));
    println!("\n===== NORMALIZED (active-region trim, ) =====");
    run("default(cov600,d6)", &norm_corpus, default);
    run("matched-gate(d6,m3)", &norm_corpus, mg(6, 3));
    run("matched-gate(d6,m4)", &norm_corpus, mg(6, 4));
    run("matched-gate(d8,m3)", &norm_corpus, mg(8, 3));
    run("matched-gate(d8,m4)", &norm_corpus, mg(8, 4));
}
