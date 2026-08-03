#![allow(clippy::too_many_lines)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::ExitCode;

use vidcull_core::types::FileId;
use vidcull_db::open_file;
use vidcull_db::repo::{FilesRepo, FingerprintsRepo, SimilarityEdgesRepo, TrustLevel};
use vidcull_fingerprint::format::decode_tier2;
use vidcull_fingerprint::tier2::Tier2Fingerprint;
use vidcull_matcher::partial::durable::{
    BlobSource, PartialClipIndex, rebuild_partial_clip_groups_durable,
};
use vidcull_matcher::partial::{
    AnchorParams, PartialClipPlan, partial_clip_params, plan_partial_clips,
};

const NOW_STAMP: i64 = 1_700_000_000;

const DEFAULT_WATCH: [i64; 4] = [3, 4, 6, 8];

fn print_plan(
    label: &str,
    plan: &PartialClipPlan,
    watch: &BTreeSet<i64>,
    nm: &dyn Fn(i64) -> String,
) {
    println!("== {label} ==");
    println!(
        "   counters: matches={} examined={} skipped_short={} \
         dropped_below_coverage={} dropped_single_vote={}",
        plan.matches.len(),
        plan.candidate_offsets_examined,
        plan.skipped_short,
        plan.dropped_below_coverage,
        plan.dropped_single_vote,
    );
    for m in &plan.matches {
        let a = &m.alignment;
        let star = if watch.contains(&m.clip.0) || watch.contains(&a.source.0) {
            "*"
        } else {
            " "
        };
        println!(
            "  {star} clip {:>2}({}) ⊂ source {:>2}({})  matched {}/{}  cov={}  \
             src[{}..{}ms]",
            m.clip.0,
            nm(m.clip.0),
            a.source.0,
            nm(a.source.0),
            a.matched_scenes,
            a.clip_scenes,
            a.coverage_x1000,
            a.start_ms,
            a.end_ms,
        );
    }
    println!();
}

fn run(db_path: &str, watch: &BTreeSet<i64>) -> vidcull_core::Result<()> {
    let mut db = open_file(Path::new(db_path))?;
    println!("== near-miss probe ==");
    println!("db: {db_path}  watch: {watch:?}");
    println!();

    let mut name: BTreeMap<i64, String> = BTreeMap::new();
    for f in FilesRepo::new(db.conn()).list_active()? {
        let p = f.path.as_str().replace('\\', "/");
        let short: String = p
            .rsplit('/')
            .next()
            .unwrap_or(&p)
            .chars()
            .take(30)
            .collect();
        name.insert(f.id.0, short);
    }
    let nm = |id: i64| name.get(&id).cloned().unwrap_or_default();

    let fps = FingerprintsRepo::new(db.conn());
    let mut tier2_scenes: BTreeMap<i64, usize> = BTreeMap::new();
    for (id, blob) in fps.list_active_tier2()? {
        tier2_scenes.insert(id.0, decode_tier2(&blob)?.scenes.len());
    }
    let mut partial_corpus: Vec<(FileId, Tier2Fingerprint)> = Vec::new();
    for (id, blob) in fps.list_active_partial()? {
        partial_corpus.push((id, decode_tier2(&blob)?));
    }
    println!("[files] id  tier2-scenes  partial-scenes  name");
    for (id, t2) in &tier2_scenes {
        let ps = partial_corpus
            .iter()
            .find(|(fid, _)| fid.0 == *id)
            .map_or("-".to_string(), |(_, fp)| fp.scenes.len().to_string());
        let star = if watch.contains(id) { "*" } else { " " };
        println!("  {star} {id:>3}  {t2:>6}  {ps:>6}  {}", nm(*id));
    }
    println!();

    let prod = partial_clip_params();
    println!(
        "production params: bands={} max_distance={} coverage={}/1000 min_scenes={} min_matched={}",
        prod.bands(),
        prod.max_distance(),
        prod.min_coverage_x1000(),
        prod.min_scenes(),
        prod.min_matched(),
    );
    println!();

    let plan_a = plan_partial_clips(partial_corpus.clone(), prod);
    print_plan(
        "run A: in-memory, production params (min_scenes=3)",
        &plan_a,
        watch,
        &nm,
    );

    let relaxed = AnchorParams::new(
        AnchorParams::DEFAULT_BANDS,
        prod.max_distance(),
        prod.min_coverage_x1000(),
        1,
    )?
    .with_min_matched(prod.min_matched());
    let plan_b = plan_partial_clips(partial_corpus.clone(), relaxed);
    print_plan(
        "run B: in-memory, min_scenes=1 (other gates unchanged)",
        &plan_b,
        watch,
        &nm,
    );

    let mut index = PartialClipIndex::new_with_source(partial_clip_params(), BlobSource::Partial);
    let boot =
        rebuild_partial_clip_groups_durable(&mut index, &mut db, NOW_STAMP, &BTreeSet::new())?;
    println!("== run C: durable bootstrap (empty delta) ==");
    println!(
        "   groups_created={} groups_cleared={} members_added={} edges_added={} \
         skipped_short={} dropped_below_coverage={} dropped_single_vote={}",
        boot.groups_created,
        boot.groups_cleared,
        boot.members_added,
        boot.edges_added,
        boot.skipped_short,
        boot.dropped_below_coverage,
        boot.dropped_single_vote,
    );
    let changed: BTreeSet<FileId> = watch.iter().map(|&id| FileId(id)).collect();
    let burst = rebuild_partial_clip_groups_durable(&mut index, &mut db, NOW_STAMP + 1, &changed)?;
    println!("== run C: durable burst (changed = watch ids) ==");
    println!(
        "   groups_created={} groups_cleared={} members_added={} edges_added={} \
         skipped_short={} dropped_below_coverage={} dropped_single_vote={}",
        burst.groups_created,
        burst.groups_cleared,
        burst.members_added,
        burst.edges_added,
        burst.skipped_short,
        burst.dropped_below_coverage,
        burst.dropped_single_vote,
    );
    println!();

    println!("[durable result] POSSIBLE edges touching watch ids:");
    let mut touched = 0usize;
    for e in SimilarityEdgesRepo::new(db.conn()).list_by_trust(TrustLevel::Possible)? {
        if !watch.contains(&e.file_a.0) && !watch.contains(&e.file_b.0) {
            continue;
        }
        touched += 1;
        match &e.partial_span {
            Some(s) => println!(
                "   group {} {}({})-{}({}) score={} matched {}/{}",
                e.group_id,
                e.file_a.0,
                nm(e.file_a.0),
                e.file_b.0,
                nm(e.file_b.0),
                e.score_x1000,
                s.matched_scenes,
                s.clip_scenes,
            ),
            None => println!(
                "   group {} {}({})-{}({}) score={} (no span)",
                e.group_id,
                e.file_a.0,
                nm(e.file_a.0),
                e.file_b.0,
                nm(e.file_b.0),
                e.score_x1000,
            ),
        }
    }
    if touched == 0 {
        println!("   (none)");
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let Some(db_path) = args.get(1) else {
        eprintln!("usage: near_miss_probe <db_copy_path> [watch_id ...]");
        return ExitCode::FAILURE;
    };
    let watch: BTreeSet<i64> = if args.len() > 2 {
        args[2..].iter().filter_map(|a| a.parse().ok()).collect()
    } else {
        DEFAULT_WATCH.into_iter().collect()
    };
    match run(db_path, &watch) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("near_miss_probe failed: {e}");
            ExitCode::FAILURE
        }
    }
}
