use vidcull_db::repo::{FilesRepo, FingerprintsRepo};
use vidcull_fingerprint::format::decode_tier1;
use vidcull_fingerprint::hamming_distance;

fn main() {
    let db_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "vidcull.db".into());
    let db = vidcull_db::open_file(std::path::Path::new(&db_path)).expect("open db");
    let files = FilesRepo::new(db.conn());
    let fps = FingerprintsRepo::new(db.conn());

    let rows = fps.list_active_tier1().expect("list tier1");
    let mut entries: Vec<(i64, String, u64)> = Vec::new();
    for (file_id, blob) in rows {
        let t1 = decode_tier1(&blob).expect("decode tier1");
        let name = files
            .get(file_id)
            .ok()
            .flatten()
            .map(|r| {
                let p = r.path.as_str();
                p.rsplit('/')
                    .next()
                    .unwrap_or(p)
                    .chars()
                    .take(34)
                    .collect::<String>()
            })
            .unwrap_or_default();
        entries.push((file_id.0, name, t1.global_phash));
    }
    entries.sort_by_key(|e| e.0);

    println!("== files ({}) ==", entries.len());
    for (id, name, ph) in &entries {
        println!("  id={id:<3} phash={ph:016x}  {name}");
    }
    println!("\n== pairwise Hamming (near-dup threshold = 6) ==");
    for i in 0..entries.len() {
        for j in (i + 1)..entries.len() {
            let d = hamming_distance(entries[i].2, entries[j].2);
            let mark = if d <= 6 { "  <= MATCH" } else { "" };
            println!(
                "  id{:<3} vs id{:<3}: {:>2} bits{}   ({} | {})",
                entries[i].0, entries[j].0, d, mark, entries[i].1, entries[j].1
            );
        }
    }
}
