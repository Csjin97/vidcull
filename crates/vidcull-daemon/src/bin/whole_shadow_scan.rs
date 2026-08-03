use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::process::ExitCode;

use rusqlite::{Connection, OpenFlags};
use vidcull_core::types::FileId;
use vidcull_db::repo::FingerprintsRepo;
use vidcull_fingerprint::format::decode_tier2;
use vidcull_fingerprint::tier2::Tier2Fingerprint;
use vidcull_matcher::whole::{WholeFileCandidate, WholeFileParams, scan_whole_file_candidates};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut db_path: Option<String> = None;
    let mut names = false;
    let mut all = false;
    for arg in &args {
        match arg.as_str() {
            "--names" => names = true,
            "--all" => all = true,
            "-h" | "--help" => {
                print_help();
                return ExitCode::SUCCESS;
            }
            other if db_path.is_none() && !other.starts_with('-') => {
                db_path = Some(other.to_owned());
            }
            other => {
                eprintln!("error: unrecognised argument '{other}'\n");
                print_help();
                return ExitCode::FAILURE;
            }
        }
    }

    let Some(db_path) = db_path else {
        eprintln!("error: missing required <db-path>\n");
        print_help();
        return ExitCode::FAILURE;
    };

    match run(Path::new(&db_path), names, all) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn print_help() {
    println!(
        "whole_shadow_scan -- Phase A offline whole-file re-encode measurement\n\
         \n\
         USAGE:\n\
         \x20\x20\x20\x20whole_shadow_scan <db-path> [--names] [--all]\n\
         \n\
         Opens <db-path> READ-ONLY (SQLITE_OPEN_READ_ONLY) and never writes to it.\n\
         Run this against a COPY of your database (scripts/whole-shadow-measure.ps1\n\
         does this for you), not the live file the daemon holds open.\n\
         \n\
         OPTIONS:\n\
         \x20\x20--names   resolve file ids to their stored path (local diagnostic only)\n\
         \x20\x20--all     print every measured candidate, not only gate-passers\n\
         \x20\x20-h, --help  print this help text"
    );
}

fn immutable_uri(path: &Path) -> String {
    let forward = path.to_string_lossy().replace('\\', "/");
    let mut out = String::with_capacity(forward.len() + 24);
    out.push_str("file:");
    out.push_str(if forward.starts_with('/') {
        "//"
    } else {
        "///"
    });
    for byte in forward.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
                out.push(char::from(byte));
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out.push_str("?immutable=1");
    out
}

fn run(db_path: &Path, names: bool, all: bool) -> Result<(), Box<dyn std::error::Error>> {
    if !db_path.is_file() {
        return Err(format!("database file not found: {}", db_path.display()).into());
    }
    let conn = Connection::open_with_flags(
        immutable_uri(db_path),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;

    let corpus = load_corpus(&conn)?;
    let corpus_files = corpus.len();
    let path_by_id = if names {
        Some(load_paths(&conn)?)
    } else {
        None
    };

    let candidates = scan_whole_file_candidates(&corpus, WholeFileParams::default());
    print_report(corpus_files, &candidates, path_by_id.as_ref(), all);
    Ok(())
}

fn load_corpus(
    conn: &Connection,
) -> Result<Vec<(FileId, Tier2Fingerprint)>, Box<dyn std::error::Error>> {
    let rows = FingerprintsRepo::new(conn).list_active_tier2()?;
    let mut corpus = Vec::with_capacity(rows.len());
    for (file_id, blob) in rows {
        match decode_tier2(&blob) {
            Ok(fp) => corpus.push((file_id, fp)),
            Err(err) => eprintln!(
                "warn: file_id={} tier2 decode failed, excluded from scan: {err}",
                file_id.0
            ),
        }
    }
    Ok(corpus)
}

fn load_paths(conn: &Connection) -> Result<BTreeMap<i64, String>, Box<dyn std::error::Error>> {
    let mut stmt = conn.prepare("SELECT id, path FROM files")?;
    let rows = stmt
        .query_map([], |row| {
            let id: i64 = row.get(0)?;
            let path: String = row.get(1)?;
            Ok((id, path))
        })?
        .collect::<rusqlite::Result<BTreeMap<i64, String>>>()?;
    Ok(rows)
}

fn format_id(id: FileId, path_by_id: Option<&BTreeMap<i64, String>>) -> String {
    match path_by_id.and_then(|m| m.get(&id.0)) {
        Some(path) => format!("{} [{path}]", id.0),
        None => id.0.to_string(),
    }
}

fn print_report(
    corpus_files: usize,
    candidates: &[WholeFileCandidate],
    path_by_id: Option<&BTreeMap<i64, String>>,
    all: bool,
) {
    println!(
        "[whole-shadow] corpus_files={corpus_files} candidates_total={}",
        candidates.len()
    );
    for c in candidates {
        if !all && !c.passes_gate {
            continue;
        }
        println!(
            "[whole-shadow] candidate a={} b={} scene_ratio={:.4} span_coverage_a={:.4} \
             span_coverage_b={:.4} coverage_ab={:.4} coverage_ba={:.4} offset_ab_ms={} \
             offset_ba_ms={} offset_consistency_ab={:.4} offset_consistency_ba={:.4} \
             passes_gate={}",
            format_id(c.a, path_by_id),
            format_id(c.b, path_by_id),
            c.scene_ratio,
            c.span_coverage_a,
            c.span_coverage_b,
            c.coverage_ab,
            c.coverage_ba,
            c.offset_ab_ms,
            c.offset_ba_ms,
            c.offset_consistency_ab,
            c.offset_consistency_ba,
            c.passes_gate,
        );
    }

    let gate_pass_density: Vec<f64> = candidates
        .iter()
        .filter(|c| c.passes_gate)
        .map(whole_file_density)
        .collect();
    let scene_ratio_min = WholeFileParams::default().scene_ratio_min;
    let near_equal_nonpass_density: Vec<f64> = candidates
        .iter()
        .filter(|c| !c.passes_gate && c.scene_ratio >= scene_ratio_min)
        .map(whole_file_density)
        .collect();
    let (gate_pass_density_min, gate_pass_density_max) = min_max(&gate_pass_density);
    let (near_equal_nonpass_density_min, near_equal_nonpass_density_max) =
        min_max(&near_equal_nonpass_density);

    println!(
        "[whole-shadow] summary corpus_files={corpus_files} candidates_total={} \
         gate_pass_count={} gate_pass_density_min={gate_pass_density_min:.4} \
         gate_pass_density_max={gate_pass_density_max:.4} near_equal_nonpass_count={} \
         near_equal_nonpass_density_min={near_equal_nonpass_density_min:.4} \
         near_equal_nonpass_density_max={near_equal_nonpass_density_max:.4}",
        candidates.len(),
        gate_pass_density.len(),
        near_equal_nonpass_density.len(),
    );
}

fn whole_file_density(c: &WholeFileCandidate) -> f64 {
    c.coverage_ab.min(c.coverage_ba)
}

fn min_max(values: &[f64]) -> (f64, f64) {
    if values.is_empty() {
        return (0.0, 0.0);
    }
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    (min, max)
}
