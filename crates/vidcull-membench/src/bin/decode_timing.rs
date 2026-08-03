use std::process::ExitCode;
use std::time::{Duration, Instant};

use vidcull_core::Result;
use vidcull_core::types::Codec;
use vidcull_membench::timing::{DecodeTiming, SpawnDecompose, ratio_of};
use vidcull_parser::fallback::{
    DecodeConcurrency, FfmpegBinaries, decode_batch_head, decode_frame_at, decode_sparse,
};
use vidcull_parser::{probe_and_decode_sparse, probe_and_decode_sparse_budgets};
use vidcull_synth::{Recipe, render_recipe, render_source};

const SPARSE_BUDGET: usize = 12;
const ONE_HOUR_REFERENCE: Duration = Duration::from_secs(3);
const TIMED_RUNS: usize = 3;

const S1_BUDGETS: [usize; 4] = [12, 100, 500, 1440];
const S1_PERFRAME_MAX_MEASURED: usize = 100;

fn secs_ms(d: Duration) -> String {
    format!(
        "{:.3}s ({:.1} ms)",
        d.as_secs_f64(),
        d.as_secs_f64() * 1000.0
    )
}

fn ms(d: Duration) -> String {
    format!("{:.1}ms", d.as_secs_f64() * 1000.0)
}

fn best_of(runs: usize, mut f: impl FnMut() -> Result<()>) -> Result<Duration> {
    f()?;
    let mut best = Duration::MAX;
    for _ in 0..runs {
        let start = Instant::now();
        f()?;
        best = best.min(start.elapsed());
    }
    Ok(best)
}

fn measure_sparse(bins: &FfmpegBinaries, path: &std::path::Path) -> Result<DecodeTiming> {
    let decoded = probe_and_decode_sparse(bins, path, SPARSE_BUDGET)?;
    let frames = decoded.frames.len();
    let total = best_of(TIMED_RUNS, || {
        let d = probe_and_decode_sparse(bins, path, SPARSE_BUDGET)?;
        std::hint::black_box(&d);
        Ok(())
    })?;
    Ok(DecodeTiming { frames, total })
}

fn measure_single_frame(
    bins: &FfmpegBinaries,
    path: &std::path::Path,
    ts_ms: u64,
    width: u32,
    height: u32,
) -> Result<Duration> {
    best_of(5, || {
        let frame = decode_frame_at(bins, path, ts_ms, width, height)?;
        std::hint::black_box(&frame);
        Ok(())
    })
}

fn run_decode_timing(bins: &FfmpegBinaries, dir: &std::path::Path) -> Result<()> {
    println!("== §A sparse decode timing (12 I-frames) ==");
    let short = render_source(bins, dir, "h264_30s", "testsrc", 30_000, 320, 180, 30, 30)?;
    let long = render_source(bins, dir, "h264_120s", "testsrc", 120_000, 320, 180, 30, 30)?;

    let t_short = measure_sparse(bins, &short)?;
    let t_long = measure_sparse(bins, &long)?;

    println!(
        "  30s  clip: {} frames in {}  → {}/frame",
        t_short.frames,
        secs_ms(t_short.total),
        secs_ms(t_short.per_frame()),
    );
    println!(
        "  120s clip: {} frames in {}  → {}/frame",
        t_long.frames,
        secs_ms(t_long.total),
        secs_ms(t_long.per_frame()),
    );

    let independence = ratio_of(t_short.per_frame(), t_long.per_frame());
    println!(
        "  per-frame 120s/30s ratio: {independence:.2}× (≈1.0 ⇒ input-seek cost is duration-independent ⇒ extrapolation valid)"
    );

    let one_hour = t_long.extrapolate(SPARSE_BUDGET);
    let verdict = if one_hour <= ONE_HOUR_REFERENCE {
        "within"
    } else {
        "OVER"
    };
    println!(
        "  ⇒ 1-hour / 12-I-frame proxy: {}  [{verdict} the {} reference]",
        secs_ms(one_hour),
        secs_ms(ONE_HOUR_REFERENCE),
    );
    println!("  (wall time machine-dependent: reported, not goldened — verdict is informational)");
    Ok(())
}

fn run_file_timing(
    bins: &FfmpegBinaries,
    path: &std::path::Path,
    budget: usize,
    capacity: usize,
) -> Result<()> {
    println!("== real-file native decode: serial vs parallel ==");
    println!("  file:      {}", path.display());
    let serial = DecodeConcurrency::new(1);
    let parallel = DecodeConcurrency::new(capacity);

    let seq = probe_and_decode_sparse_budgets(bins, path, budget, budget, &serial)?;
    let par = probe_and_decode_sparse_budgets(bins, path, budget, budget, &parallel)?;
    let frames = seq.frames.len();
    println!(
        "  routing:   {:?} ({frames} frames, budget {budget}, capacity {capacity})",
        seq.decode_path
    );
    assert_eq!(
        frames,
        par.frames.len(),
        "frame count differs serial vs parallel"
    );
    for (i, (s, p)) in seq.frames.iter().zip(par.frames.iter()).enumerate() {
        assert_eq!(
            s.timestamp_ms, p.timestamp_ms,
            "frame {i} timestamp differs"
        );
        assert_eq!(
            s.pixels, p.pixels,
            "frame {i} pixels differ — parallel decode is NOT bit-exact"
        );
    }
    println!("  bit-exact: serial == parallel \u{2713} ({frames} frames identical)");
    if frames == 0 {
        println!("  (no frames decoded — nothing to time)");
        return Ok(());
    }

    let t_seq = best_of(TIMED_RUNS, || {
        let d = probe_and_decode_sparse_budgets(bins, path, budget, budget, &serial)?;
        std::hint::black_box(&d);
        Ok(())
    })?;
    let t_par = best_of(TIMED_RUNS, || {
        let d = probe_and_decode_sparse_budgets(bins, path, budget, budget, &parallel)?;
        std::hint::black_box(&d);
        Ok(())
    })?;
    let n = u32::try_from(frames).unwrap_or(u32::MAX);
    println!(
        "  serial   (cap 1):    {}  → {}/frame",
        secs_ms(t_seq),
        secs_ms(t_seq / n),
    );
    println!(
        "  parallel (cap {capacity}):    {}  → {}/frame",
        secs_ms(t_par),
        secs_ms(t_par / n),
    );
    println!(
        "  ⇒ speedup {:.2}× (incl. serial probe); decode delta {}",
        ratio_of(t_par, t_seq),
        secs_ms(t_seq.saturating_sub(t_par)),
    );
    println!("  (wall time machine-dependent: reported, not goldened)");
    Ok(())
}

fn run_native_comparison(bins: &FfmpegBinaries, dir: &std::path::Path) -> Result<()> {
    println!();
    println!("== §N6 native vs ffmpeg sparse decode (same H.264 clip) ==");
    let clip_ms = 30_000u64;
    let (w, h) = (320u32, 180u32);
    let clip = render_source(bins, dir, "n6_h264", "testsrc", clip_ms, w, h, 30, 30)?;

    let probed = probe_and_decode_sparse(bins, &clip, SPARSE_BUDGET)?;
    println!(
        "  routing: {:?} ({} frames) — native path {}",
        probed.decode_path,
        probed.frames.len(),
        if probed.decode_path == vidcull_parser::fallback::DecodePath::Native {
            "active"
        } else {
            "INACTIVE — fell back; numbers below are not a native comparison"
        }
    );

    let native = best_of(TIMED_RUNS, || {
        let d = probe_and_decode_sparse(bins, &clip, SPARSE_BUDGET)?;
        std::hint::black_box(&d);
        Ok(())
    })?;
    let ffmpeg = best_of(TIMED_RUNS, || {
        let f = decode_sparse(bins, &clip, clip_ms, w, h, SPARSE_BUDGET)?;
        std::hint::black_box(&f);
        Ok(())
    })?;

    let n = u32::try_from(SPARSE_BUDGET).unwrap_or(u32::MAX);
    println!(
        "  native (in-process):   {}  → {}/frame",
        secs_ms(native),
        secs_ms(native / n),
    );
    println!(
        "  ffmpeg (per-frame spawn): {}  → {}/frame",
        secs_ms(ffmpeg),
        secs_ms(ffmpeg / n),
    );
    println!(
        "  ⇒ native speedup: {:.1}× over ffmpeg ({SPARSE_BUDGET} I-frames; lower bound — native incl. probe)",
        ratio_of(native, ffmpeg),
    );
    println!("  (wall time machine-dependent: reported, not goldened)");
    Ok(())
}

fn run_fallback_overhead(bins: &FfmpegBinaries, dir: &std::path::Path) -> Result<()> {
    println!();
    println!("== §F4 fallback-codec decode overhead (single frame) ==");
    let h264 = render_source(bins, dir, "fp_native", "testsrc", 6_000, 320, 180, 30, 30)?;
    let vp9 = render_recipe(
        bins,
        &Recipe::reencode(h264.clone(), Codec::Vp9).with_clip(0, 5_000),
        0x0F4_u64,
        dir,
    )?;

    let ts_ms = 2_000;
    let native = measure_single_frame(bins, &h264, ts_ms, 320, 180)?;
    let fallback = measure_single_frame(bins, &vp9, ts_ms, 320, 180)?;
    let overhead = ratio_of(native, fallback);

    println!("  native  (H.264 / fast path): {}/frame", secs_ms(native));
    println!("  fallback (VP9 / ffmpeg):     {}/frame", secs_ms(fallback));
    println!(
        "  fallback/native per-frame overhead: {overhead:.2}× (shared subprocess-spawn cost; codec decode delta is the excess over 1.0×)"
    );
    println!(
        "  note: AV1 is the other fallback codec; libaom-av1 is far slower to *encode* so it is not live-rendered here — its *decode* cost is ≥ VP9's, so this is a lower bound on fallback-decode overhead."
    );
    println!(
        "  GATED: the fallback *entry rate* distribution depends on a real-corpus codec mix (synthetic data cannot model it) — measure via `FallbackMetrics` over a 100k+ live corpus (§G / real corpus)."
    );
    Ok(())
}

fn measure_batch(
    bins: &FfmpegBinaries,
    path: &std::path::Path,
    width: u32,
    height: u32,
    count: usize,
) -> Result<DecodeTiming> {
    let frames = decode_batch_head(bins, path, width, height, count)?.len();
    let total = best_of(TIMED_RUNS, || {
        let f = decode_batch_head(bins, path, width, height, count)?;
        std::hint::black_box(&f);
        Ok(())
    })?;
    Ok(DecodeTiming { frames, total })
}

fn measure_perframe(
    bins: &FfmpegBinaries,
    path: &std::path::Path,
    clip_ms: u64,
    width: u32,
    height: u32,
    count: usize,
) -> Result<DecodeTiming> {
    let span = clip_ms.saturating_sub(1);
    let timestamps: Vec<u64> = (0..count)
        .map(|i| i as u64 * span / count.max(1) as u64)
        .collect();
    let total = best_of(TIMED_RUNS, || {
        for &ts in &timestamps {
            let f = decode_frame_at(bins, path, ts, width, height)?;
            std::hint::black_box(&f);
        }
        Ok(())
    })?;
    Ok(DecodeTiming {
        frames: count,
        total,
    })
}

fn run_spawn_decomposition(bins: &FfmpegBinaries, dir: &std::path::Path) -> Result<()> {
    println!();
    println!("== §S1 ffmpeg spawn vs decode decomposition ==");
    let clip_ms = 50_000u64;
    let (w, h) = (320u32, 180u32);
    let clip = render_source(bins, dir, "s1_allintra", "testsrc", clip_ms, w, h, 30, 1)?;

    let mut batch_small: Option<DecodeTiming> = None;
    let mut batch_large: Option<DecodeTiming> = None;
    let mut perframe_pf: Option<Duration> = None;

    println!("  budget |   batch(1 spawn) |  batch/f |  per-frame(N) |   pf/f  | pf");
    println!("  -------+------------------+----------+---------------+---------+------");
    for &b in &S1_BUDGETS {
        let batch = measure_batch(bins, &clip, w, h, b)?;
        if batch_small.is_none() {
            batch_small = Some(batch);
        }
        batch_large = Some(batch);

        let (pf_total, pf_tag) = if b <= S1_PERFRAME_MAX_MEASURED {
            let pf = measure_perframe(bins, &clip, clip_ms, w, h, b)?;
            perframe_pf = Some(pf.per_frame());
            (pf.total, "meas")
        } else {
            let pf = perframe_pf.expect("a budget <= S1_PERFRAME_MAX_MEASURED runs first");
            (pf * u32::try_from(b).unwrap_or(u32::MAX), "extrap")
        };

        println!(
            "  {b:>5}  | {:>15} | {:>7} | {:>13} | {:>7} | {pf_tag}",
            ms(batch.total),
            ms(batch.per_frame()),
            ms(pf_total),
            ms(pf_total / u32::try_from(b).unwrap_or(u32::MAX)),
        );
    }

    let decomp = SpawnDecompose::from_batch_pair(
        batch_small.expect("at least one budget measured"),
        batch_large.expect("at least one budget measured"),
    );
    let pf = perframe_pf.expect("at least one per-frame budget measured");
    let tax = decomp.spawn_seek_fixed(pf);

    println!();
    println!(
        "  decode marginal (batch slope):     {}/frame",
        secs_ms(decomp.decode_marginal)
    );
    println!(
        "  spawn+open    (batch intercept):   {}",
        secs_ms(decomp.spawn_once)
    );
    println!("  per-frame path amortized cost:     {}/frame", secs_ms(pf));
    println!(
        "  ⇒ spawn+seek tax a native decoder removes: {}/frame ({:.1}× the decode work)",
        secs_ms(tax),
        ratio_of(decomp.decode_marginal, tax),
    );
    println!(
        "  ⇒ native must beat {}/frame; {} of that is decode work it must still do",
        secs_ms(pf),
        secs_ms(decomp.decode_marginal),
    );
    println!(
        "  (wall time machine-dependent: reported, not goldened — this is the spike's GO/NO-GO baseline)"
    );
    Ok(())
}

fn run() -> Result<()> {
    let bins = match FfmpegBinaries::resolve() {
        Ok(bins) => bins,
        Err(e) => {
            println!(
                "SKIP decode_timing: ffmpeg/ffprobe not resolvable ({e}); set VIDCULL_FFMPEG_DIR or install on PATH"
            );
            return Ok(());
        }
    };
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.first().is_some_and(|s| s == "file") {
        let Some(path) = args.get(1).filter(|s| !s.is_empty()) else {
            eprintln!("usage: decode_timing file <path> [budget]");
            return Ok(());
        };
        let budget = args
            .get(2)
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(SPARSE_BUDGET);
        let capacity = args
            .get(3)
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(std::num::NonZero::get)
                    .unwrap_or(4)
            });
        return run_file_timing(&bins, std::path::Path::new(path), budget, capacity);
    }

    let want = |name: &str| args.is_empty() || args.iter().any(|a| a == name || a == "all");

    let dir = tempfile::tempdir().expect("tempdir");
    if want("a") {
        run_decode_timing(&bins, dir.path())?;
    }
    if want("n6") {
        run_native_comparison(&bins, dir.path())?;
    }
    if want("f4") {
        run_fallback_overhead(&bins, dir.path())?;
    }
    if want("s1") {
        run_spawn_decomposition(&bins, dir.path())?;
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("decode_timing failed: {e}");
            ExitCode::FAILURE
        }
    }
}
