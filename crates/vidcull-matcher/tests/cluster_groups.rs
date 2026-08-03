use vidcull_core::Result;
use vidcull_core::types::{FileId, NormalizedPath};
use vidcull_db::repo::{DuplicateGroupsRepo, FilesRepo, NewFile, TrustLevel};
use vidcull_db::{Database, open_in_memory};
use vidcull_matcher::cluster::{Cluster, build_clusters};

const T0: i64 = 1_700_000_000;
const MTIME: i64 = 1_700_000_000_000_000_000;

fn fresh_db() -> Database {
    open_in_memory().expect("open in-memory db")
}

fn seed_file(db: &Database, path: &str) -> FileId {
    let new_file = NewFile {
        path: NormalizedPath::new(path),
        size_bytes: 1024,
        mtime_ns: MTIME,
        inode: None,
        content_hash: None,
        codec: None,
        container: None,
        duration: None,
        fps_x1000: None,
        bitrate_bps: None,
        resolution: None,
        first_seen_at: T0,
        last_seen_at: T0,
        ..Default::default()
    };
    FilesRepo::new(db.conn())
        .insert(&new_file)
        .expect("insert file")
}

fn seed_group(db: &Database, trust: TrustLevel, members: &[FileId]) -> i64 {
    let repo = DuplicateGroupsRepo::new(db.conn());
    let gid = repo.create(trust, T0).expect("create group");
    for &m in members {
        repo.add_member(gid, m).expect("add member");
    }
    gid
}

fn member_ids(cluster: &Cluster) -> Vec<i64> {
    cluster.member_ids().into_iter().map(|f| f.0).collect()
}

#[test]
fn build_clusters_merges_exact_and_very_likely_sharing_a_member() -> Result<()> {
    let db = fresh_db();
    let f1 = seed_file(&db, "/v/a.mp4");
    let f2 = seed_file(&db, "/v/b.mp4");
    let f3 = seed_file(&db, "/v/c.mp4");
    let exact = seed_group(&db, TrustLevel::Exact, &[f1, f2]);
    let near = seed_group(&db, TrustLevel::VeryLikely, &[f2, f3]);

    let clusters = build_clusters(&db)?;
    assert_eq!(clusters.len(), 1, "shared member f2 merges the two groups");
    let c = &clusters[0];
    assert_eq!(member_ids(c), vec![f1.0, f2.0, f3.0]);
    assert_eq!(c.representative_trust, TrustLevel::Exact);
    let mut gids = c.group_ids.clone();
    gids.sort_unstable();
    assert_eq!(gids, vec![exact, near]);

    let trust_of = |id: FileId| c.members.iter().find(|m| m.file_id == id).unwrap().trust;
    assert_eq!(trust_of(f1), TrustLevel::Exact);
    assert_eq!(trust_of(f2), TrustLevel::Exact);
    assert_eq!(trust_of(f3), TrustLevel::VeryLikely);
    Ok(())
}

#[test]
fn build_clusters_keeps_disjoint_groups_separate() -> Result<()> {
    let db = fresh_db();
    let f1 = seed_file(&db, "/v/a.mp4");
    let f2 = seed_file(&db, "/v/b.mp4");
    let f3 = seed_file(&db, "/v/c.mp4");
    let f4 = seed_file(&db, "/v/d.mp4");
    seed_group(&db, TrustLevel::Exact, &[f1, f2]);
    seed_group(&db, TrustLevel::VeryLikely, &[f3, f4]);

    let clusters = build_clusters(&db)?;
    assert_eq!(clusters.len(), 2, "no shared member ⇒ no merge (FP guard)");
    assert_eq!(member_ids(&clusters[0]), vec![f1.0, f2.0]);
    assert_eq!(member_ids(&clusters[1]), vec![f3.0, f4.0]);
    Ok(())
}

#[test]
fn build_clusters_keeps_possible_clip_groups_separate() -> Result<()> {
    let db = fresh_db();
    let f1 = seed_file(&db, "/v/whole-a.mp4");
    let f2 = seed_file(&db, "/v/whole-b.mp4");
    let f3 = seed_file(&db, "/v/clip.mp4");
    seed_group(&db, TrustLevel::VeryLikely, &[f1, f2]);
    seed_group(&db, TrustLevel::Possible, &[f2, f3]);

    let clusters = build_clusters(&db)?;
    assert_eq!(clusters.len(), 2);
    let whole = clusters
        .iter()
        .find(|c| c.representative_trust == TrustLevel::VeryLikely)
        .expect("whole-file cluster");
    assert_eq!(member_ids(whole), vec![f1.0, f2.0]);
    let clip = clusters
        .iter()
        .find(|c| c.representative_trust == TrustLevel::Possible)
        .expect("clip cluster");
    assert_eq!(member_ids(clip), vec![f2.0, f3.0]);
    Ok(())
}

#[test]
fn build_clusters_drops_possible_group_duplicating_a_near_pair() -> Result<()> {
    let db = fresh_db();
    let f1 = seed_file(&db, "/v/h264.mp4");
    let f2 = seed_file(&db, "/v/h265.mp4");
    let near = seed_group(&db, TrustLevel::VeryLikely, &[f1, f2]);
    seed_group(&db, TrustLevel::Possible, &[f1, f2]);

    let clusters = build_clusters(&db)?;
    assert_eq!(
        clusters.len(),
        1,
        "near pair already clustered ⇒ no duplicate POSSIBLE cluster"
    );
    let c = &clusters[0];
    assert_eq!(member_ids(c), vec![f1.0, f2.0]);
    assert_eq!(c.representative_trust, TrustLevel::VeryLikely);
    assert_eq!(c.group_ids, vec![near]);
    let possible = clusters
        .iter()
        .filter(|c| c.representative_trust == TrustLevel::Possible)
        .count();
    assert_eq!(
        possible, 0,
        "no standalone POSSIBLE cluster for the near pair"
    );
    Ok(())
}

#[test]
fn build_clusters_keeps_partial_only_clip_group() -> Result<()> {
    let db = fresh_db();
    let f1 = seed_file(&db, "/v/whole-a.mp4");
    let f2 = seed_file(&db, "/v/whole-b.mp4");
    let f3 = seed_file(&db, "/v/clip.mp4");
    seed_group(&db, TrustLevel::VeryLikely, &[f1, f2]);
    seed_group(&db, TrustLevel::Possible, &[f2, f3]);

    let clusters = build_clusters(&db)?;
    assert_eq!(clusters.len(), 2, "partial-only clip cluster is preserved");
    let possible = clusters
        .iter()
        .filter(|c| c.representative_trust == TrustLevel::Possible)
        .count();
    assert_eq!(
        possible, 1,
        "the genuine partial clip stays its own cluster"
    );
    let clip = clusters
        .iter()
        .find(|c| c.representative_trust == TrustLevel::Possible)
        .expect("clip cluster present");
    assert_eq!(member_ids(clip), vec![f2.0, f3.0]);
    Ok(())
}

#[test]
fn build_clusters_is_idempotent() -> Result<()> {
    let db = fresh_db();
    let f1 = seed_file(&db, "/v/a.mp4");
    let f2 = seed_file(&db, "/v/b.mp4");
    let f3 = seed_file(&db, "/v/c.mp4");
    seed_group(&db, TrustLevel::Exact, &[f1, f2]);
    seed_group(&db, TrustLevel::VeryLikely, &[f2, f3]);

    let first = build_clusters(&db)?;
    let second = build_clusters(&db)?;
    assert_eq!(first, second, "read-only projection must be idempotent");
    Ok(())
}

#[test]
fn build_clusters_on_empty_db_is_empty() -> Result<()> {
    let db = fresh_db();
    assert!(build_clusters(&db)?.is_empty());
    Ok(())
}
