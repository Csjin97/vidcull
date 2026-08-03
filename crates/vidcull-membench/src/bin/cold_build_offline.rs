use std::collections::BTreeSet;
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use vidcull_db::open_file;
use vidcull_matcher::partial::AnchorParams;
use vidcull_matcher::partial::durable::{PartialClipIndex, rebuild_partial_clip_groups_durable};

const NOW_STAMP: i64 = 1_700_000_000;

fn run(db_path: &str) -> vidcull_core::Result<()> {
    let mut db = open_file(Path::new(db_path))?;
    let mut index = PartialClipIndex::new(AnchorParams::default());
    println!("== offline partial-clip cold-build driver ==");
    println!("db: {db_path}");
    let t = Instant::now();
    let outcome =
        rebuild_partial_clip_groups_durable(&mut index, &mut db, NOW_STAMP, &BTreeSet::new())?;
    println!(
        "done in {:.2?}: {} POSSIBLE groups, {} members, {} edges (skipped_short {})",
        t.elapsed(),
        outcome.groups_created,
        outcome.members_added,
        outcome.edges_added,
        outcome.skipped_short,
    );
    println!(
        "re-run on the same db to resume an interrupted build (byte-identical to a full plan)"
    );
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let Some(db_path) = args.get(1) else {
        eprintln!("usage: cold_build_offline <db_path>");
        return ExitCode::FAILURE;
    };
    match run(db_path) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("cold_build_offline failed: {e}");
            ExitCode::FAILURE
        }
    }
}
