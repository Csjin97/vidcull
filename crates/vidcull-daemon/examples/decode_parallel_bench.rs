use std::path::PathBuf;
use std::time::Instant;

use vidcull_parser::fallback::{
    DecodeConcurrency, FfmpegBinaries, decode_sparse, decode_sparse_with, fallback_spawn_plan,
    probe_fallback,
};

#[allow(clippy::too_many_lines)]
fn main() {
    let mut args = std::env::args_os().skip(1);

    let video_path: PathBuf = if let Some(p) = args.next() {
        PathBuf::from(p)
    } else {
        eprintln!("Usage: decode_parallel_bench <video_path> [capacity]");
        std::process::exit(1);
    };

    let capacity: usize = args
        .next()
        .and_then(|s| s.to_string_lossy().parse::<usize>().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(std::num::NonZero::get)
                .unwrap_or(4)
        });

    let bins = FfmpegBinaries::new("ffmpeg".into(), "ffprobe".into());

    let meta = probe_fallback(&bins, &video_path).unwrap_or_else(|e| {
        eprintln!("probe_fallback failed: {e}");
        std::process::exit(1);
    });

    let dur = meta
        .duration
        .map_or(0, vidcull_core::VideoDuration::as_millis);
    let w = meta.resolution.width;
    let h = meta.resolution.height;

    if dur == 0 || w == 0 || h == 0 {
        eprintln!("probe returned unusable metadata: dur={dur}ms {w}x{h} — cannot decode");
        std::process::exit(1);
    }

    let budget = 10_000usize;

    let frame_px = u64::from(w) * u64::from(h);
    let (grid_points, planned_spawns) = fallback_spawn_plan(
        &meta.container,
        dur,
        budget,
        &meta.codec,
        meta.fps_x1000,
        meta.has_b_frames,
        frame_px,
    );
    let batch_eligible = planned_spawns < grid_points;

    println!("File       : {}", video_path.display());
    println!("Resolution : {w}x{h}  duration: {dur} ms");
    println!(
        "Codec/fps  : {:?} / {}",
        meta.codec,
        meta.fps_x1000.map_or_else(
            || "unknown".to_string(),
            |f| format!("{:.3} fps", f64::from(f) / 1000.0),
        ),
    );
    println!("Budget     : {budget} grid frames");
    println!("Capacity   : {capacity} concurrent ffmpeg spawns");
    println!();

    let t0 = Instant::now();
    let seq_frames = decode_sparse(&bins, &video_path, dur, w, h, budget).unwrap_or_else(|e| {
        eprintln!("sequential decode_sparse failed: {e}");
        std::process::exit(1);
    });
    let seq_elapsed = t0.elapsed();

    let conc = DecodeConcurrency::new(capacity);
    let t1 = Instant::now();
    let par_frames = decode_sparse_with(
        &bins,
        &video_path,
        dur,
        w,
        h,
        budget,
        &meta.codec,
        meta.fps_x1000,
        meta.has_b_frames,
        &conc,
    )
    .unwrap_or_else(|e| {
        eprintln!("parallel decode_sparse_with failed: {e}");
        std::process::exit(1);
    });
    let par_elapsed = t1.elapsed();

    assert_eq!(
        seq_frames.len(),
        par_frames.len(),
        "frame count mismatch: sequential={} parallel={}",
        seq_frames.len(),
        par_frames.len(),
    );

    for (i, (s, p)) in seq_frames.iter().zip(par_frames.iter()).enumerate() {
        assert_eq!(
            s.timestamp_ms, p.timestamp_ms,
            "frame[{i}] timestamp mismatch: seq={} par={}",
            s.timestamp_ms, p.timestamp_ms,
        );
        assert_eq!(
            s.pixels,
            p.pixels,
            "frame[{i}] pixel mismatch at ts={}ms (first differing byte index: {})",
            s.timestamp_ms,
            s.pixels
                .iter()
                .zip(p.pixels.iter())
                .position(|(a, b)| a != b)
                .unwrap_or(0),
        );
    }

    let speedup = seq_elapsed.as_secs_f64() / par_elapsed.as_secs_f64().max(1e-9);

    println!("Frames decoded : {}", seq_frames.len());
    println!("Grid points    : {grid_points}");
    if batch_eligible {
        #[allow(clippy::cast_precision_loss)]
        let reduction = grid_points as f64 / (planned_spawns.max(1) as f64);
        println!(
            "ffmpeg spawns  : {planned_spawns} (batch windows)  vs  {grid_points} (per-frame)  =>  {reduction:.1}x fewer spawns"
        );
    } else {
        println!("ffmpeg spawns  : {grid_points} (per-frame path — not batch-eligible)");
    }
    println!("Sequential time: {:.3} s", seq_elapsed.as_secs_f64());
    println!(
        "Parallel time  : {:.3} s  (capacity={})",
        par_elapsed.as_secs_f64(),
        capacity
    );
    println!("Speedup        : {speedup:.2}x");
    println!();
    println!("OK — sequential and parallel frames are identical.");
}
