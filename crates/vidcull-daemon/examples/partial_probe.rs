#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
#![allow(clippy::too_many_lines)]

use std::collections::HashMap;

use vidcull_db::repo::{FilesRepo, FingerprintsRepo};
use vidcull_fingerprint::format::decode_tier2;
use vidcull_matcher::partial::{AnchorParams, plan_partial_clips};

fn main() {
    let db_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "vidcull.db".into());
    let db = vidcull_db::open_file(std::path::Path::new(&db_path)).expect("open db");
    let files = FilesRepo::new(db.conn());
    let fps = FingerprintsRepo::new(db.conn());

    let mut name: HashMap<i64, String> = HashMap::new();
    for f in files.list_active().expect("files") {
        let p = f.path.as_str().replace('\\', "/");
        let short: String = p
            .rsplit('/')
            .next()
            .unwrap_or(&p)
            .chars()
            .take(26)
            .collect();
        name.insert(f.id.0, short);
    }
    let nm = |id: i64| name.get(&id).cloned().unwrap_or_default();

    let mut corpus = Vec::new();
    for (id, blob) in fps.list_active_tier2().expect("tier2") {
        match decode_tier2(&blob) {
            Ok(fp) => {
                println!(
                    "file {:>2}  scenes={:>5}  {}",
                    id.0,
                    fp.scenes.len(),
                    nm(id.0)
                );
                corpus.push((id, fp));
            }
            Err(e) => eprintln!("file {} tier2 decode err: {e}", id.0),
        }
    }
    println!();

    let scenes_of = |id: i64| -> Vec<(u64, u64)> {
        corpus
            .iter()
            .find(|(fid, _)| fid.0 == id)
            .map(|(_, fp)| {
                fp.scenes
                    .iter()
                    .map(|s| (s.timestamp_ms, s.phash))
                    .collect()
            })
            .unwrap_or_default()
    };
    for id in [1i64, 2, 3, 5, 6] {
        let s = scenes_of(id);
        if s.is_empty() {
            continue;
        }
        let step = if s.len() > 1 {
            s[1].0 as i64 - s[0].0 as i64
        } else {
            0
        };
        println!(
            "file {:>2} scenes={:>4} ts[0]={}ms last={}ms (~{:.1}min) step≈{}ms  {}",
            id,
            s.len(),
            s[0].0,
            s[s.len() - 1].0,
            s[s.len() - 1].0 as f64 / 60_000.0,
            step,
            nm(id)
        );
    }
    println!();

    let best_align = |clip: &[(u64, u64)], src: &[(u64, u64)], t: u32| -> (i64, usize, usize) {
        if clip.is_empty() || src.is_empty() {
            return (0, 0, 0);
        }
        let step = if src.len() > 1 {
            (src[1].0 as i64 - src[0].0 as i64).max(250)
        } else {
            2500
        };
        let (mut best_d, mut best_hits, mut best_win) = (0i64, 0usize, 0usize);
        let span = src[src.len() - 1].0 as i64;
        let mut d = -(clip[clip.len() - 1].0 as i64);
        while d <= span {
            let mut hits = 0usize;
            let mut win = 0usize;
            for &(tc, pc) in clip {
                let target = tc as i64 + d;
                if target < src[0].0 as i64 - step || target > span + step {
                    continue;
                }
                win += 1;
                let nearest = src
                    .iter()
                    .min_by_key(|&&(ts, _)| (ts as i64 - target).abs())
                    .map_or(0, |&(_, ph)| ph);
                if (pc ^ nearest).count_ones() <= t {
                    hits += 1;
                }
            }
            if hits > best_hits {
                best_hits = hits;
                best_d = d;
                best_win = win;
            }
            d += step;
        }
        (best_d, best_hits, best_win)
    };

    let pairs = [(7i64, 4i64), (1, 2), (3, 6), (1, 5)];
    for (clip_id, src_id) in pairs {
        let clip = scenes_of(clip_id);
        let src = scenes_of(src_id);
        for t in [6u32, 8, 10, 12] {
            let (d, hits, win) = best_align(&clip, &src, t);
            println!(
                "align clip {} ⊂ source {}  T={:>2}bit -> best Δ={}ms (~{:.1}min)  hits={}/{} (overlap window scenes={})  {} vs {}",
                clip_id,
                src_id,
                t,
                d,
                d as f64 / 60_000.0,
                hits,
                clip.len(),
                win,
                nm(clip_id),
                nm(src_id)
            );
        }
        println!();
    }

    for &maxd in &[6u32, 8, 10, 12, 16] {
        for &covx in &[600u32, 400, 300, 250, 200, 150, 100] {
            let params = AnchorParams::new(AnchorParams::DEFAULT_BANDS, maxd, covx, 3)
                .expect("valid params");
            let plan = plan_partial_clips(corpus.clone(), params);
            if plan.matches.is_empty() {
                continue;
            }
            println!(
                "== max_distance={maxd}  coverage={covx}/1000  ->  {} match(es) ==",
                plan.matches.len()
            );
            for m in &plan.matches {
                let a = &m.alignment;
                println!(
                    "   clip {:>2}({}) ⊂ source {:>2}({})  matched {}/{}  cov={}  [{}..{}ms]",
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
        }
    }
}
