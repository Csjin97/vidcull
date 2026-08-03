use std::process::ExitCode;

use vidcull_matcher::partial::DEFAULT_SHARD_SOURCES;
use vidcull_membench::{
    CountingAllocator, MemReport, measure_anchor, measure_anchor_scoped, measure_lsh,
};

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

const LSH_100K_PEAK_BUDGET: usize = 48 * 1024 * 1024;

const ANCHOR_10K_PEAK_BUDGET: usize = 512 * 1024 * 1024;

const ANCHOR_SCOPED_100K_PEAK_BUDGET: usize = 512 * 1024 * 1024;

fn mib(bytes: usize) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    {
        bytes as f64 / (1024.0 * 1024.0)
    }
}

fn print_report(label: &str, report: MemReport) {
    println!(
        "{label:<22} n={n:>7}  retained={ret:>8.1} MiB  peak={peak:>8.1} MiB  \
         (retained {rpe:>6.1} B/elem, peak {ppe:>6.1} B/elem)",
        n = report.elements,
        ret = mib(report.retained_bytes),
        peak = mib(report.peak_bytes),
        rpe = report.retained_per_element(),
        ppe = report.peak_per_element(),
    );
}

fn parse_arg(args: &[String], idx: usize, default: usize) -> usize {
    args.get(idx)
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let lsh_n = parse_arg(&args, 1, 100_000);
    let anchor_videos = parse_arg(&args, 2, 10_000);
    let anchor_scenes = parse_arg(&args, 3, 60);

    println!("== §B grouping-pass peak-memory probe (deterministic allocated bytes) ==");

    let lsh = measure_lsh(lsh_n, 0x5EED_0001);
    print_report("lsh_build", lsh);

    let anchor = measure_anchor(anchor_videos, anchor_scenes, 0x0A0C_0001);
    print_report("anchor_build", anchor);

    let anchor_100k_peak = anchor.peak_per_element() * 100_000.0;
    println!(
        "anchor_build (100k extrapolated)            peak≈{:>8.1} MiB  \
         ({:.1} B/video × 100k) — see §B: partial-clip index must be scoped/sharded",
        anchor_100k_peak / (1024.0 * 1024.0),
        anchor.peak_per_element(),
    );

    let scoped = measure_anchor_scoped(100_000, 60, DEFAULT_SHARD_SOURCES, 0x0A0C_0001);
    println!(
        "anchor_build_scoped (100k, shard={DEFAULT_SHARD_SOURCES})       peak={:>8.1} MiB  \
         retained={:.1} MiB — B-fix-2: bounded to one shard, not ≈2 GiB",
        mib(scoped.peak_bytes),
        mib(scoped.retained_bytes),
    );

    let mut ok = true;
    if lsh.elements >= 100_000 && lsh.peak_bytes > LSH_100K_PEAK_BUDGET {
        eprintln!(
            "FAIL: LSH peak {:.1} MiB exceeds budget {:.1} MiB",
            mib(lsh.peak_bytes),
            mib(LSH_100K_PEAK_BUDGET),
        );
        ok = false;
    }
    if anchor.elements >= 10_000 && anchor.peak_bytes > ANCHOR_10K_PEAK_BUDGET {
        eprintln!(
            "FAIL: anchor peak {:.1} MiB exceeds budget {:.1} MiB",
            mib(anchor.peak_bytes),
            mib(ANCHOR_10K_PEAK_BUDGET),
        );
        ok = false;
    }
    if scoped.peak_bytes > ANCHOR_SCOPED_100K_PEAK_BUDGET {
        eprintln!(
            "FAIL: scoped anchor peak {:.1} MiB exceeds budget {:.1} MiB (sharding regressed?)",
            mib(scoped.peak_bytes),
            mib(ANCHOR_SCOPED_100K_PEAK_BUDGET),
        );
        ok = false;
    }

    if ok {
        println!("PASS: all probed indexes within documented peak budgets");
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
