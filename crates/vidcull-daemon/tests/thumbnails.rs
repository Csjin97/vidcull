use std::path::Path;
use std::sync::Arc;

use tempfile::tempdir;
use vidcull_core::types::{Blake3Hash, NormalizedPath};
use vidcull_daemon::thumbnails::ThumbnailProvider;
use vidcull_daemon::{
    ChangeKind, ChangeTask, Daemon, DaemonConfig, IndexingHandler, enqueue_changes,
};
use vidcull_db::repo::FilesRepo;
use vidcull_synth::{FfmpegBinaries, render_source};
use vidcull_thumb::ThumbnailCache;

const NOW: i64 = 1_700_000_000;

fn now() -> i64 {
    NOW
}

#[test]
fn returns_none_without_a_content_hash() {
    let dir = tempdir().unwrap();
    let provider = ThumbnailProvider::new(dir.path().to_path_buf(), None);
    assert!(provider.data_uri(Path::new("clip.mp4"), None).is_none());
}

#[test]
fn returns_none_on_a_cache_miss_without_a_decoder() {
    let dir = tempdir().unwrap();
    let provider = ThumbnailProvider::new(dir.path().to_path_buf(), None);
    let hash = Blake3Hash::from_bytes([3u8; 32]);
    assert!(
        provider
            .data_uri(Path::new("clip.mp4"), Some(&hash))
            .is_none()
    );
}

#[test]
fn serves_a_cache_hit_without_a_decoder() {
    let dir = tempdir().unwrap();
    let hash = Blake3Hash::from_bytes([7u8; 32]);
    let hex = hash.to_hex();
    ThumbnailCache::new(dir.path().to_path_buf())
        .load_or_store(&hex, 0, || Ok(vec![0xFF, 0xD8, 0xFF, 0xD9]))
        .unwrap();

    let provider = ThumbnailProvider::new(dir.path().to_path_buf(), None);
    let uri = provider
        .data_uri(Path::new("clip.mp4"), Some(&hash))
        .expect("a cache hit serves a preview even without a decoder");
    assert!(uri.starts_with("data:image/jpeg;base64,"));
}

#[test]
fn indexing_caches_the_thumbnail_so_display_needs_no_cold_decode() {
    let Ok(bins) = FfmpegBinaries::resolve() else {
        eprintln!(
            "SKIP indexing_caches_the_thumbnail_so_display_needs_no_cold_decode: ffmpeg not resolvable"
        );
        return;
    };
    let tmp = tempdir().expect("tempdir");
    let dir = tmp.path();
    let db_path = dir.join("index.db");
    let cache_dir = dir.join("thumbs");

    let clip =
        render_source(&bins, dir, "clip", "testsrc", 2000, 320, 180, 30, 6).expect("render clip");

    {
        let mut db = vidcull_db::open_file(&db_path).expect("open db");
        let n = enqueue_changes(
            &mut db,
            &[ChangeTask {
                path: NormalizedPath::new(&clip),
                change: ChangeKind::Upsert,
                size_bytes: 0,
            }],
            "scan",
            0,
            NOW,
        )
        .expect("enqueue");
        assert_eq!(n, 1, "the clip is enqueued");
    }

    let provider = Arc::new(ThumbnailProvider::new(cache_dir.clone(), None));
    let handler_db = vidcull_db::open_file(&db_path).expect("open handler db");
    let mut handler =
        IndexingHandler::new(handler_db, bins, now).with_thumbnails(Arc::clone(&provider));
    let worker_db = vidcull_db::open_file(&db_path).expect("open worker db");
    let daemon = Daemon::new(DaemonConfig::default());
    while daemon
        .step(&worker_db, &mut handler, NOW)
        .expect("step")
        .is_some()
    {}

    let verify = vidcull_db::open_file(&db_path).expect("reopen db");
    let active = FilesRepo::new(verify.conn()).list_active().expect("list");
    assert_eq!(active.len(), 1, "the clip is indexed");
    let hash = active[0]
        .content_hash
        .expect("the indexed file has a content hash");

    let display = ThumbnailProvider::new(cache_dir, None);
    let uri = display.data_uri(&clip, Some(&hash));
    assert!(
        uri.is_some(),
        "indexing-time L1 tee must have cached the thumbnail: a no-decoder provider \
         returns Some only on a cache hit"
    );
    assert!(
        uri.expect("cache hit")
            .starts_with("data:image/jpeg;base64,")
    );
}
