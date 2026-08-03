use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use vidcull_core::Result;
use vidcull_db::open_in_memory;
use vidcull_fingerprint::format::encode_tier2;
use vidcull_fingerprint::{
    DEFAULT_BAR_LIMIT, GrayFrame, Tier2Builder, TimedFrame, trim_uniform_borders,
};
use vidcull_parser::fallback::{
    DecodeConcurrency, FfmpegBinaries, decode_sparse_with, probe_fallback,
};

const DEFAULT_SAMPLE_BUDGET: usize = 24;

const GRID_INTERVAL_MS: u64 = 2500;

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

struct Profile {
    name: String,
    frame_px: u64,
    duration_ms: u64,
    sampled_frames: usize,
    full_grid: u64,
    probe: Duration,
    decode_total: Duration,
    trim_total: Duration,
    phash_total: Duration,
    encode: Duration,
    db_write: Duration,
}

impl Profile {
    fn per_frame(&self, total: Duration) -> Duration {
        if self.sampled_frames == 0 {
            return Duration::ZERO;
        }
        total / u32::try_from(self.sampled_frames).unwrap_or(u32::MAX)
    }

    fn extrapolated_total(&self) -> Duration {
        let n = u32::try_from(self.full_grid).unwrap_or(u32::MAX);
        self.probe
            + (self.per_frame(self.decode_total)
                + self.per_frame(self.trim_total)
                + self.per_frame(self.phash_total))
                * n
            + self.encode
            + self.db_write
    }
}

fn profile_file(bins: &FfmpegBinaries, path: &Path, sample_budget: usize) -> Option<Profile> {
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );

    let t = Instant::now();
    let meta = match probe_fallback(bins, path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("  SKIP {name}: probe failed ({e})");
            return None;
        }
    };
    let probe = t.elapsed();
    let duration_ms = meta
        .duration
        .map_or(0, vidcull_core::VideoDuration::as_millis);
    if duration_ms == 0 || meta.resolution.is_empty() {
        eprintln!("  SKIP {name}: zero duration or empty resolution");
        return None;
    }
    let (width, height) = (meta.resolution.width, meta.resolution.height);
    let full_grid = duration_ms.div_ceil(GRID_INTERVAL_MS).max(1);

    let conc = DecodeConcurrency::new(1);
    let t = Instant::now();
    let frames = match decode_sparse_with(
        bins,
        path,
        duration_ms,
        width,
        height,
        sample_budget,
        &meta.codec,
        meta.fps_x1000,
        meta.has_b_frames,
        &conc,
    ) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("  SKIP {name}: decode failed ({e})");
            return None;
        }
    };
    let decode_total = t.elapsed();
    let sampled_frames = frames.len();
    if sampled_frames == 0 {
        eprintln!("  SKIP {name}: decoded 0 frames");
        return None;
    }

    let mut trim_total = Duration::ZERO;
    let mut phash_total = Duration::ZERO;
    let mut builder = Tier2Builder::new();
    for f in &frames {
        let t = Instant::now();
        let (w, h, px) = trim_uniform_borders(f.width, f.height, &f.pixels, DEFAULT_BAR_LIMIT);
        trim_total += t.elapsed();

        let t = Instant::now();
        builder.push(&TimedFrame {
            timestamp_ms: f.timestamp_ms,
            frame: GrayFrame {
                width: w,
                height: h,
                pixels: &px,
            },
        });
        phash_total += t.elapsed();
    }

    let t = Instant::now();
    let tier2 = builder.finish();
    let blob = encode_tier2(&tier2).unwrap_or_default();
    let encode = t.elapsed();

    let db_write = measure_db_write(&blob).unwrap_or(Duration::ZERO);

    Some(Profile {
        name,
        frame_px: u64::from(width) * u64::from(height),
        duration_ms,
        sampled_frames,
        full_grid,
        probe,
        decode_total,
        trim_total,
        phash_total,
        encode,
        db_write,
    })
}

fn measure_db_write(blob: &[u8]) -> Result<Duration> {
    use vidcull_core::types::NormalizedPath;
    use vidcull_db::repo::{FilesRepo, Fingerprint, FingerprintsRepo, NewFile};

    let db = open_in_memory()?;
    let conn = db.conn();
    let id = FilesRepo::new(conn).insert(&NewFile {
        path: NormalizedPath::new("/probe.mp4"),
        ..Default::default()
    })?;
    let fps = FingerprintsRepo::new(conn);
    fps.upsert(&Fingerprint {
        file_id: id,
        tier1_global: vec![0u8; 8],
        tier2_temporal: None,
        format_version: 1,
        created_at: 0,
    })?;

    let t = Instant::now();
    fps.set_partial(id, blob)?;
    Ok(t.elapsed())
}

#[allow(clippy::cast_precision_loss)]
fn print_table(profiles: &[Profile]) {
    println!();
    println!("== partial-clip fingerprint per-stage profile (per-frame, ms) ==");
    println!(
        "  {:<26} {:>9} {:>7} {:>8} {:>8} {:>8} | {:>9}",
        "file", "res(MP)", "decode", "trim", "phash", "decode%", "full(s)"
    );
    println!("  {}", "-".repeat(92));
    for p in profiles {
        let d = p.per_frame(p.decode_total);
        let tr = p.per_frame(p.trim_total);
        let ph = p.per_frame(p.phash_total);
        let cpu = tr + ph;
        let pct = if (d + cpu).as_secs_f64() > 0.0 {
            d.as_secs_f64() / (d + cpu).as_secs_f64() * 100.0
        } else {
            0.0
        };
        println!(
            "  {:<26} {:>9.2} {:>7.2} {:>8.3} {:>8.3} {:>7.2}% | {:>9.1}",
            truncate(&p.name, 26),
            p.frame_px as f64 / 1_000_000.0,
            ms(d),
            ms(tr),
            ms(ph),
            pct,
            p.extrapolated_total().as_secs_f64(),
        );
    }
    println!("  {}", "-".repeat(92));
    println!(
        "  decode/trim/phash = per-frame ms · decode% = decode share of per-frame CPU+IO ·\n  full(s) = extrapolated whole-file wall time (probe + per-frame×grid + encode + db)"
    );
    println!();
    println!("== one-off stages (per file, ms) + grid sizing ==");
    println!(
        "  {:<26} {:>8} {:>8} {:>9} {:>10} {:>9}",
        "file", "probe", "encode", "db_write", "dur(min)", "grid_pts"
    );
    println!("  {}", "-".repeat(80));
    for p in profiles {
        println!(
            "  {:<26} {:>8.1} {:>8.3} {:>9.3} {:>10.1} {:>9}",
            truncate(&p.name, 26),
            ms(p.probe),
            ms(p.encode),
            ms(p.db_write),
            p.duration_ms as f64 / 60_000.0,
            p.full_grid,
        );
    }
    println!("  {}", "-".repeat(80));
    println!(
        "  (sampled {} grid points/file; per-frame stages extrapolated to grid_pts.\n   wall time machine-dependent: reported, not goldened.)",
        profiles.first().map_or(0, |p| p.sampled_frames)
    );
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let head: String = s.chars().take(n.saturating_sub(1)).collect();
    format!("{head}…")
}

fn collect_videos(root: &Path) -> Vec<PathBuf> {
    const EXTS: &[&str] = &[
        "mp4", "mkv", "avi", "mov", "webm", "ts", "m4v", "wmv", "flv",
    ];
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase)
                .is_some_and(|e| EXTS.contains(&e.as_str()))
            {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn run() {
    let bins = match FfmpegBinaries::resolve() {
        Ok(b) => b,
        Err(e) => {
            println!("SKIP partial_stage_profile: ffmpeg not resolvable ({e})");
            return;
        }
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = args
        .first()
        .cloned()
        .unwrap_or_else(|| "fixtures/real".to_string());
    let sample_budget = args
        .get(1)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_SAMPLE_BUDGET);

    let root = Path::new(&root);
    let files = if root.is_dir() {
        collect_videos(root)
    } else {
        vec![root.to_path_buf()]
    };
    println!(
        "profiling {} file(s) under {} (sample budget {sample_budget} grid points)",
        files.len(),
        root.display()
    );

    let mut profiles = Vec::new();
    for f in &files {
        println!("  · {}", f.display());
        if let Some(p) = profile_file(&bins, f, sample_budget) {
            profiles.push(p);
        }
    }
    if profiles.is_empty() {
        println!("no files profiled (all skipped).");
        return;
    }
    print_table(&profiles);
}

fn main() {
    run();
}
