use vidcull_core::SPARSE_GRID_INTERVAL_MS;
use vidcull_core::types::FileId;
use vidcull_fingerprint::tier2::{SceneHash, Tier2Fingerprint};
use vidcull_matcher::whole::{WholeFileCandidate, WholeFileParams, scan_whole_file_candidates};

const GRID_MS: u64 = SPARSE_GRID_INTERVAL_MS;

const N_BASE: usize = 4000;

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn scene(ts: u64, phash: u64) -> SceneHash {
    SceneHash {
        timestamp_ms: ts,
        phash,
    }
}

fn fp(scenes: Vec<SceneHash>) -> Tier2Fingerprint {
    Tier2Fingerprint { scenes }
}

fn measure_all(a: Vec<SceneHash>, b: Vec<SceneHash>) -> Vec<WholeFileCandidate> {
    let corpus = vec![(FileId(1), fp(a)), (FileId(2), fp(b))];
    scan_whole_file_candidates(&corpus, WholeFileParams::default())
}

#[track_caller]
fn measure_one(a: Vec<SceneHash>, b: Vec<SceneHash>) -> WholeFileCandidate {
    let out = measure_all(a, b);
    assert_eq!(
        out.len(),
        1,
        "a deliberately-matched pair must yield exactly one candidate"
    );
    out.into_iter().next().unwrap()
}

fn spread_mask(n: usize, density: f64) -> Vec<bool> {
    let mut acc = 0.0_f64;
    (0..n)
        .map(|_| {
            acc += density;
            if acc >= 1.0 {
                acc -= 1.0;
                true
            } else {
                false
            }
        })
        .collect()
}

fn fmt_bool(passes: bool) -> &'static str {
    if passes { "PASS" } else { "FAIL" }
}

fn describe(label: &str, c: &WholeFileCandidate) -> String {
    format!(
        "{label:<38} scene_ratio={:.4} span_a={:.4} span_b={:.4} cov_ab={:.4} \
cov_ba={:.4} offset_ab={:>7}ms offset_ba={:>7}ms consist_ab={:.3} consist_ba={:.3} gate={}",
        c.scene_ratio,
        c.span_coverage_a,
        c.span_coverage_b,
        c.coverage_ab,
        c.coverage_ba,
        c.offset_ab_ms,
        c.offset_ba_ms,
        c.offset_consistency_ab,
        c.offset_consistency_ba,
        fmt_bool(c.passes_gate),
    )
}

fn density_reencode_pair(
    n: usize,
    density: f64,
    offset_ms: u64,
    seed: u64,
) -> (Vec<SceneHash>, Vec<SceneHash>) {
    let mut st = seed;
    let a: Vec<SceneHash> = (0..n)
        .map(|i| scene(i as u64 * GRID_MS, splitmix64(&mut st) | 1))
        .collect();
    let mask = spread_mask(n, density);
    let b: Vec<SceneHash> = a
        .iter()
        .zip(mask)
        .map(|(s, is_match)| {
            let ph = if is_match {
                s.phash ^ 0b110
            } else {
                splitmix64(&mut st) | 1
            };
            scene(s.timestamp_ms + offset_ms, ph)
        })
        .collect();
    (a, b)
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn scene_count_drift_pair(
    n: usize,
    drift_frac: f64,
    density: f64,
    seed: u64,
) -> (Vec<SceneHash>, Vec<SceneHash>) {
    let mut st = seed;
    let mut a: Vec<SceneHash> = (0..n)
        .map(|i| scene(i as u64 * GRID_MS, splitmix64(&mut st) | 1))
        .collect();
    let mask = spread_mask(n, density);
    let mut b: Vec<SceneHash> = a
        .iter()
        .zip(&mask)
        .map(|(s, &is_match)| {
            let ph = if is_match {
                s.phash ^ 0b110
            } else {
                splitmix64(&mut st) | 1
            };
            scene(s.timestamp_ms, ph)
        })
        .collect();
    let zero_target: &mut [SceneHash] = if drift_frac >= 0.0 { &mut b } else { &mut a };
    let zero_goal = ((n as f64) * drift_frac.abs()).round() as usize;
    let mut zeroed = 0usize;
    for (i, &is_match) in mask.iter().enumerate() {
        if zeroed >= zero_goal {
            break;
        }
        if !is_match {
            zero_target[i].phash = 0;
            zeroed += 1;
        }
    }
    (a, b)
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn dispersed_share_pair(n: usize, shared_frac: f64, seed: u64) -> (Vec<SceneHash>, Vec<SceneHash>) {
    let mut st = seed;
    let a: Vec<SceneHash> = (0..n)
        .map(|i| scene(i as u64 * GRID_MS, splitmix64(&mut st) | 1))
        .collect();
    let half = ((n as f64) * shared_frac / 2.0).round() as usize;
    let b: Vec<SceneHash> = a
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let shared = i < half || i >= n - half;
            let ph = if shared {
                s.phash ^ 0b110
            } else {
                splitmix64(&mut st) | 1
            };
            scene(s.timestamp_ms, ph)
        })
        .collect();
    (a, b)
}

fn multicam_pair(
    n: usize,
    offsets_ms: &[i64],
    weights: &[f64],
    seed: u64,
) -> (Vec<SceneHash>, Vec<SceneHash>) {
    assert_eq!(
        offsets_ms.len(),
        weights.len(),
        "one weight per offset group"
    );
    let mut st = seed;
    let a: Vec<SceneHash> = (0..n)
        .map(|i| scene(i as u64 * GRID_MS, splitmix64(&mut st) | 1))
        .collect();
    let mut b: Vec<SceneHash> = (0..n)
        .map(|i| scene(i as u64 * GRID_MS, splitmix64(&mut st) | 1))
        .collect();
    let grid_i64 = i64::try_from(GRID_MS).unwrap_or(i64::MAX);
    for (offset, &weight) in offsets_ms.iter().zip(weights) {
        let mask = spread_mask(n, weight);
        for (i, is_match) in mask.into_iter().enumerate() {
            if !is_match {
                continue;
            }
            let a_ts = i64::try_from(a[i].timestamp_ms).unwrap_or(i64::MAX);
            let target_bucket = (a_ts + offset).div_euclid(grid_i64);
            if let Ok(j) = usize::try_from(target_bucket) {
                if j < n {
                    b[j].phash = a[i].phash ^ 0b110;
                }
            }
        }
    }
    (a, b)
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn stock_reuse_pair(n: usize, shared_frac: f64, seed: u64) -> (Vec<SceneHash>, Vec<SceneHash>) {
    let mut st = seed;
    let a: Vec<SceneHash> = (0..n)
        .map(|i| scene(i as u64 * GRID_MS, splitmix64(&mut st) | 1))
        .collect();
    let shared_len = ((n as f64) * shared_frac).round() as usize;
    let start = (n - shared_len) / 2;
    let b: Vec<SceneHash> = a
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let shared = i >= start && i < start + shared_len;
            let ph = if shared {
                s.phash ^ 0b110
            } else {
                splitmix64(&mut st) | 1
            };
            scene(s.timestamp_ms, ph)
        })
        .collect();
    (a, b)
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn adjacent_segment_pair(
    n: usize,
    overlap_frac: f64,
    seed: u64,
) -> (Vec<SceneHash>, Vec<SceneHash>) {
    let mut st = seed;
    let a: Vec<SceneHash> = (0..n)
        .map(|i| scene(i as u64 * GRID_MS, splitmix64(&mut st) | 1))
        .collect();
    let overlap_len = ((n as f64) * overlap_frac).round() as usize;
    let b: Vec<SceneHash> = (0..n)
        .map(|i| {
            let ph = if i < overlap_len {
                a[n - overlap_len + i].phash ^ 0b110
            } else {
                splitmix64(&mut st) | 1
            };
            scene(i as u64 * GRID_MS, ph)
        })
        .collect();
    (a, b)
}

fn unrelated_pair(n: usize, seed_a: u64, seed_b: u64) -> (Vec<SceneHash>, Vec<SceneHash>) {
    let mut sa = seed_a;
    let mut sb = seed_b;
    let a: Vec<SceneHash> = (0..n)
        .map(|i| scene(i as u64 * GRID_MS, splitmix64(&mut sa) | 1))
        .collect();
    let b: Vec<SceneHash> = (0..n)
        .map(|i| scene(i as u64 * GRID_MS, splitmix64(&mut sb) | 1))
        .collect();
    (a, b)
}

const P1_DENSITIES: [f64; 4] = [0.15, 0.21, 0.30, 0.50];

fn run_p1_sweep() -> Vec<(f64, WholeFileCandidate)> {
    P1_DENSITIES
        .iter()
        .enumerate()
        .map(|(i, &density)| {
            let (a, b) = density_reencode_pair(N_BASE, density, GRID_MS, 0x5EED_0000 + i as u64);
            (density, measure_one(a, b))
        })
        .collect()
}

const P2_DRIFTS: [f64; 9] = [-0.10, -0.05, -0.03, -0.01, 0.01, 0.03, 0.05, 0.10, 0.15];

fn run_p2_sweep() -> Vec<(f64, WholeFileCandidate)> {
    P2_DRIFTS
        .iter()
        .enumerate()
        .map(|(i, &drift)| {
            let (a, b) = scene_count_drift_pair(N_BASE, drift, 0.25, 0x6EED_0000 + i as u64);
            (drift, measure_one(a, b))
        })
        .collect()
}

const P3_OFFSETS_MS: [u64; 2] = [5_000, 30_000];

fn run_p3_variants() -> Vec<(u64, WholeFileCandidate)> {
    P3_OFFSETS_MS
        .iter()
        .enumerate()
        .map(|(i, &offset_ms)| {
            let (a, b) = density_reencode_pair(N_BASE, 0.25, offset_ms, 0x7EED_0000 + i as u64);
            (offset_ms, measure_one(a, b))
        })
        .collect()
}

const N1_SHARED_FRACS: [f64; 5] = [0.05, 0.10, 0.15, 0.20, 0.25];

fn run_n1_sweep() -> Vec<(f64, WholeFileCandidate)> {
    N1_SHARED_FRACS
        .iter()
        .enumerate()
        .map(|(i, &shared_frac)| {
            let (a, b) = dispersed_share_pair(N_BASE, shared_frac, 0x8EED_0000 + i as u64);
            (shared_frac, measure_one(a, b))
        })
        .collect()
}

fn run_n2() -> WholeFileCandidate {
    let (a, b) = multicam_pair(
        2000,
        &[10_000, -15_000, 30_000],
        &[0.24, 0.20, 0.16],
        0x9EED_0000,
    );
    measure_one(a, b)
}

fn run_n3() -> WholeFileCandidate {
    let (a, b) = stock_reuse_pair(N_BASE, 0.20, 0xAEED_0000);
    measure_one(a, b)
}

fn run_n4() -> WholeFileCandidate {
    let (a, b) = adjacent_segment_pair(N_BASE, 0.20, 0xBEED_0000);
    measure_one(a, b)
}

fn run_n5() -> Vec<WholeFileCandidate> {
    let (a, b) = unrelated_pair(2000, 0xC0FF_EE00, 0xFACE_0FF1);
    measure_all(a, b)
}

#[test]
fn p1_density_sweep_true_reencode() {
    let sweep = run_p1_sweep();
    for (density, c) in &sweep {
        println!("{}", describe(&format!("P1 density={density:.2}"), c));
        assert!(
            c.scene_ratio > 0.999,
            "P1 density={density}: near-equal length"
        );
    }
    let at_030 = sweep
        .iter()
        .find(|(d, _)| (*d - 0.30).abs() < 1e-9)
        .expect("0.30 is in P1_DENSITIES");
    assert!(
        at_030.1.passes_gate,
        "P1 density=0.30 is an unambiguous true re-encode and must pass the gate"
    );
}

#[test]
fn p2_scene_count_drift_sweep() {
    let sweep = run_p2_sweep();
    for (drift, c) in &sweep {
        println!(
            "{}",
            describe(&format!("P2 drift={:+.0}%", drift * 100.0), c)
        );
    }
    assert_eq!(
        sweep.len(),
        P2_DRIFTS.len(),
        "every drift point was measured"
    );
}

#[allow(clippy::cast_possible_wrap)]
const OFFSET_TOLERANCE_MS: i64 = 2 * GRID_MS as i64;

#[test]
fn p3_nonzero_offset_recall() {
    let variants = run_p3_variants();
    for (offset_ms, c) in &variants {
        println!("{}", describe(&format!("P3 offset={offset_ms}ms"), c));
        assert!(
            c.passes_gate,
            "P3 offset={offset_ms}ms: a true re-encode at a nonzero offset must still pass"
        );
        let expected = i64::try_from(*offset_ms).unwrap();
        let diff = (c.offset_ab_ms - expected).abs();
        assert!(
            diff <= OFFSET_TOLERANCE_MS,
            "Hough offset must land within {OFFSET_TOLERANCE_MS}ms of the injected shift \
(offset={offset_ms}ms, recovered={}ms, diff={diff}ms)",
            c.offset_ab_ms
        );
    }
}

#[test]
fn n1_dispersed_share_breach_sweep() {
    let sweep = run_n1_sweep();
    for (shared_frac, c) in &sweep {
        println!(
            "{}",
            describe(&format!("N1 shared={:.0}%", shared_frac * 100.0), c)
        );
        assert!(
            c.scene_ratio > 0.999,
            "N1 shared={shared_frac}: G1 always passes (equal length)"
        );
        assert!(
            c.span_coverage_a > 0.9 && c.span_coverage_b > 0.9,
            "N1 shared={shared_frac}: F2 -- G2 span-coverage cannot see the dispersion"
        );
    }
    let at_05 = sweep
        .iter()
        .find(|(f, _)| (*f - 0.05).abs() < 1e-9)
        .expect("0.05 is in N1_SHARED_FRACS");
    assert!(
        !at_05.1.passes_gate,
        "N1 shared=5%: unambiguously below any sane density floor, must fail"
    );
}

#[test]
fn n2_multicam_inconsistent_offset() {
    let c = run_n2();
    println!("{}", describe("N2 multicam", &c));
    assert!(
        c.scene_ratio > 0.999,
        "N2: near-equal length by construction"
    );
    assert!(
        c.offset_consistency_ab < 0.7,
        "N2: offsets are deliberately scattered across 3 groups, consistency should read low, \
got {}",
        c.offset_consistency_ab
    );
}

#[test]
fn n3_stock_reuse_middle_segment() {
    let c = run_n3();
    println!("{}", describe("N3 stock-reuse (middle 20%)", &c));
    assert!(c.scene_ratio > 0.999, "N3: equal length by construction");
    assert!(
        c.span_coverage_a < 0.5 && c.span_coverage_b < 0.5,
        "N3: a single interior block cannot span the whole file: {} / {}",
        c.span_coverage_a,
        c.span_coverage_b
    );
    assert!(
        !c.passes_gate,
        "N3: G2 (span-coverage) rejects a middle-only shared block"
    );
}

#[test]
fn n4_adjacent_segment_boundary_overlap() {
    let c = run_n4();
    println!("{}", describe("N4 adjacent-segment (20% boundary)", &c));
    assert!(c.scene_ratio > 0.999, "N4: equal length by construction");
    assert!(
        c.span_coverage_a < 0.5 && c.span_coverage_b < 0.5,
        "N4: a boundary-only shared block cannot span either whole file: {} / {}",
        c.span_coverage_a,
        c.span_coverage_b
    );
    assert!(
        !c.passes_gate,
        "N4: G2 (span-coverage) rejects an adjacent-boundary overlap"
    );
}

#[test]
fn n5_unrelated_never_passes() {
    let out = run_n5();
    println!(
        "N5 unrelated: {} candidate(s) measured from incidental LSH collisions",
        out.len()
    );
    for c in &out {
        println!("{}", describe("N5 unrelated", c));
    }
    assert!(
        out.iter().all(|c| !c.passes_gate),
        "N5: two unrelated videos must never clear the structural gate"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn separation_report() {
    let p1 = run_p1_sweep();
    let p2 = run_p2_sweep();
    let p3 = run_p3_variants();
    let n1 = run_n1_sweep();
    let n2 = run_n2();
    let n3 = run_n3();
    let n4 = run_n4();
    let n5 = run_n5();

    println!("\n================ Phase A synthetic separation report ================");
    println!("-- POSITIVES --");
    for (density, c) in &p1 {
        println!("{}", describe(&format!("P1 density={density:.2}"), c));
    }
    for (drift, c) in &p2 {
        println!(
            "{}",
            describe(&format!("P2 drift={:+.0}%", drift * 100.0), c)
        );
    }
    for (offset_ms, c) in &p3 {
        println!("{}", describe(&format!("P3 offset={offset_ms}ms"), c));
    }
    println!("-- NEGATIVES --");
    for (shared_frac, c) in &n1 {
        println!(
            "{}",
            describe(&format!("N1 shared={:.0}%", shared_frac * 100.0), c)
        );
    }
    println!("{}", describe("N2 multicam", &n2));
    println!("{}", describe("N3 stock-reuse", &n3));
    println!("{}", describe("N4 adjacent-segment", &n4));
    for c in &n5 {
        println!("{}", describe("N5 unrelated", c));
    }
    if n5.is_empty() {
        println!("N5 unrelated: 0 candidates measured (no incidental LSH collision at all)");
    }

    let positives_recall_floor = p1
        .iter()
        .filter(|(_, c)| c.passes_gate)
        .map(|(d, _)| *d)
        .fold(f64::INFINITY, f64::min);
    let positives_reject_any = p1.iter().any(|(_, c)| !c.passes_gate);

    let n1_breach_density = n1
        .iter()
        .filter(|(_, c)| c.passes_gate)
        .map(|(f, _)| *f)
        .fold(f64::INFINITY, f64::min);
    let n1_all_reject = n1.iter().all(|(_, c)| !c.passes_gate);

    let p2_g1_breach = p2.iter().find(|(_, c)| !c.passes_gate);

    println!("\n-- SEPARATION SUMMARY --");
    println!(
        "positives recall floor (min P1 density that still passes): {}",
        if positives_recall_floor.is_finite() {
            format!("{positives_recall_floor:.2}")
        } else {
            "NONE -- no tested positive density passed (UNFALSIFIED / recall broken)".to_string()
        }
    );
    println!(
        "P1 sweep rejects at least one density point: {positives_reject_any} \
(informs whether default T_low=0.15 sits exactly at the recall edge)"
    );
    println!(
        "N1 dispersed-share breach density (min shared-frac that WRONGLY passes): {}",
        if n1_all_reject {
            "NONE in tested range 5%-25% (F2 negative never breaches at these densities)"
                .to_string()
        } else {
            format!("{n1_breach_density:.2}")
        }
    );
    if positives_recall_floor.is_finite() && !n1_all_reject {
        let headroom = n1_breach_density - positives_recall_floor;
        println!(
            "F2 headroom (N1 breach density - positives recall floor) = {headroom:.3} \
-- {}",
            if headroom > 0.05 {
                "clean margin on synthetic data"
            } else if headroom >= 0.0 {
                "narrow-to-zero margin: T_low sits right at the F2 breach, exactly the \
'deliberately narrow headroom' the design doc warns about"
            } else {
                "NEGATIVE margin: the dispersed-share negative passes at a LOWER density than \
some true re-encodes reject at -- density alone cannot separate them"
            }
        );
    } else if positives_recall_floor.is_finite() && n1_all_reject {
        println!(
            "F2 headroom: N1 never breaches in the tested 5%-25% range while positives pass \
from {positives_recall_floor:.2} -- clean separation on this synthetic battery."
        );
    }
    let p2_extreme_ratio = p2
        .iter()
        .max_by(|(d1, _), (d2, _)| d1.abs().partial_cmp(&d2.abs()).unwrap())
        .map(|(_, c)| c.scene_ratio);
    println!(
        "P2 (scene-count drift) G1 (scene_ratio_min=0.80) breach in tested +-1/3/5/10%/+15% \
range: {}",
        match p2_g1_breach {
            Some((drift, c)) => {
                format!(
                    "YES at drift={:+.0}% (scene_ratio={:.3})",
                    drift * 100.0,
                    c.scene_ratio
                )
            }
            None => format!(
                "NO -- every tested drift, including the +15% bonus point (measured scene_ratio \
{:.3}), stays at/above G1's 0.80 floor (Phase-A delta; the original 0.90 floor DID breach at \
+15%, see the P2 rows above and 's Phase-A measured-delta doc)",
                p2_extreme_ratio.unwrap_or(f64::NAN)
            ),
        }
    );
    let p1_030_consistency = p1
        .iter()
        .find(|(d, _)| (*d - 0.30).abs() < 1e-9)
        .map_or(f64::NAN, |(_, c)| c.offset_consistency_ab);
    println!(
        "N2 multicam: offset_consistency_ab={:.4} vs. a true re-encode's own reading \
(P1@0.30)={:.4}, passes_gate={} -- {}",
        n2.offset_consistency_ab,
        p1_030_consistency,
        n2.passes_gate,
        if n2.passes_gate {
            "CONFIRMS a scattered-offset multicam negative clears G1+G2+G4 on span+density \
ALONE; G3 is not gated in Phase A, so this is a live false-positive shape a G3 gate would \
be needed to exclude. NOTE: at this corpus scale (thousands of scenes) BOTH readings are \
heavily diluted by incidental LSH band collisions from non-matched scenes (a birthday-paradox \
noise floor -- see OFFSET_TOLERANCE_MS doc), so a raw offset_consistency threshold would need \
noise-floor calibration against corpus size, not a fixed constant, before it could serve as a \
dependable G3 gate"
        } else {
            "did not clear the gate in this construction (G1/G2/G4 rejected it before \
offset-consistency became relevant)"
        }
    );
    println!(
        "N3 stock-reuse passes_gate={} (G2 span-coverage {:.3}/{:.3})",
        n3.passes_gate, n3.span_coverage_a, n3.span_coverage_b
    );
    println!(
        "N4 adjacent-segment passes_gate={} (G2 span-coverage {:.3}/{:.3})",
        n4.passes_gate, n4.span_coverage_a, n4.span_coverage_b
    );
    println!(
        "N5 unrelated: {} candidate(s), any pass = {}",
        n5.len(),
        n5.iter().any(|c| c.passes_gate)
    );
    println!("===============================================================================\n");

    assert!(
        !p1.is_empty() && !n1.is_empty(),
        "the battery must have run"
    );
}
