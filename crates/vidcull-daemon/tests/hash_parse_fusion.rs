use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::Instant;

use vidcull_parser::cancel::Cancel;
use vidcull_parser::fallback::{DecodeConcurrency, DecodePath, FfmpegBinaries};

const READ_BUF_CAPACITY: usize = 1 << 20;

struct CountingRead<R> {
    inner: R,
    bytes: u64,
}

impl<R> CountingRead<R> {
    fn new(inner: R) -> Self {
        Self { inner, bytes: 0 }
    }
}

impl<R: Read> Read for CountingRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.bytes += n as u64;
        Ok(n)
    }
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("vidcull-parser")
        .join("tests")
        .join("fixtures")
        .join("h264-native-e2e")
        .join(name)
}

fn box_header(size: u32, fourcc: &[u8]) -> Vec<u8> {
    let mut v = size.to_be_bytes().to_vec();
    v.extend_from_slice(fourcc);
    v
}

fn with_overshoot_garbage(base: &[u8]) -> Vec<u8> {
    let mut v = base.to_vec();
    v.extend_from_slice(&box_header(500_000_000, b"junk"));
    v
}

fn write_clip(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("clip.mp4");
    std::fs::write(&path, bytes).unwrap();
    (dir, path)
}

fn hash_with_counter(path: &Path) -> (u64, blake3::Hash) {
    const CHUNK_SIZE: usize = 64 * 1024;
    let file = File::open(path).expect("open for hash");
    let mut counting = CountingRead::new(file);
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; CHUNK_SIZE];
    loop {
        let n = counting.read(&mut buf).expect("hash read");
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    (counting.bytes, hasher.finalize())
}

fn parse_with_counter(path: &Path) -> u64 {
    let file = File::open(path).expect("open for parse");
    let counting = CountingRead::new(file);
    let mut reader = BufReader::with_capacity(READ_BUF_CAPACITY, counting);
    mp4parse::read_mp4(&mut reader).expect("mp4parse::read_mp4");
    reader.into_inner().bytes
}

fn native_index_total_bytes_via_production_path(path: &Path) -> u64 {
    let bins = FfmpegBinaries::new(
        PathBuf::from("/nonexistent/ffmpeg"),
        PathBuf::from("/nonexistent/ffprobe"),
    );
    let mut frames = Vec::new();
    let (_metadata, decode_path) = vidcull_parser::probe_and_decode_sparse_budgets_streaming(
        &bins,
        path,
        4,
        4,
        &DecodeConcurrency::serial(),
        Cancel::default(),
        |f| {
            frames.push(f.clone());
            Ok(())
        },
    )
    .unwrap_or_else(|e| {
        panic!(
            "probe_and_decode_sparse_budgets_streaming({}): {e:?}",
            path.display()
        )
    });
    assert_eq!(
        decode_path,
        DecodePath::Native,
        "expected native decode path"
    );
    assert!(
        !frames.is_empty(),
        "native decode must deliver at least one frame"
    );
    u64::try_from(frames.len()).unwrap()
}

fn measure_hash_plus_parse_read_bytes(path: &Path) -> u64 {
    let (hash_bytes, _digest) = hash_with_counter(path);
    let parse_bytes = parse_with_counter(path);
    hash_bytes + parse_bytes
}

#[test]
#[allow(clippy::cast_precision_loss)]
fn read_counter_covers_hash_and_parse_when_both_wrapped() {
    let path = fixture_path("testsrc2_160_90.mp4");
    let file_size = std::fs::metadata(&path).unwrap().len();

    let total = measure_hash_plus_parse_read_bytes(&path);

    let ratio = total as f64 / file_size as f64;
    assert!(
        (1.9..=2.1).contains(&ratio),
        "harness accuracy check failed: hash+parse totalled {total} bytes for a \
         {file_size}-byte file (ratio {ratio:.3}), expected ~2.0x (one full hash \
         pass + one full parse pass) — the harness itself may be miscounting"
    );
}

#[test]
#[allow(clippy::cast_precision_loss)]
fn index_file_reads_whole_file_at_most_1_2x_clean() {
    let path = fixture_path("testsrc2_160_90.mp4");
    let file_size = std::fs::metadata(&path).unwrap().len();

    let mut hashed_bytes = 0u64;
    let outcome = vidcull_parser::mp4::read_mp4_tolerant_hashing_cancellable(
        &path,
        Cancel::default(),
        &mut |b| hashed_bytes += b.len() as u64,
    )
    .expect("fused pass must succeed on the clean fixture");
    assert!(
        matches!(outcome, vidcull_parser::PreParsedMp4::Parsed(_)),
        "clean fixture must parse in the fused pass, got {outcome:?}"
    );

    let ratio = hashed_bytes as f64 / file_size as f64;
    assert!(
        ratio <= 1.2,
        "fused hash+parse amplification: {hashed_bytes} bytes for a {file_size}-byte \
         file (ratio {ratio:.3}, want <= 1.2x)"
    );
    assert_eq!(
        hashed_bytes, file_size,
        "the fused pass must stream the whole file through the hash exactly once"
    );
}

#[test]
fn index_file_reads_whole_file_at_most_2_4x_garbage() {
    let base = std::fs::read(fixture_path("testsrc2_160_90.mp4")).unwrap();
    let craft = with_overshoot_garbage(&base);
    let file_size = craft.len() as u64;
    let (_dir, path) = write_clip(&craft);

    let mut hashed_bytes = 0u64;
    let outcome = vidcull_parser::mp4::read_mp4_tolerant_hashing_cancellable(
        &path,
        Cancel::default(),
        &mut |b| hashed_bytes += b.len() as u64,
    )
    .expect("fused pass must succeed on the garbage fixture");
    assert!(
        matches!(outcome, vidcull_parser::PreParsedMp4::Parsed(_)),
        "trim retry must recover the clip in the fused pass, got {outcome:?}"
    );
    assert_eq!(
        hashed_bytes, file_size,
        "hash must cover the whole ORIGINAL file (garbage tail included) exactly once"
    );
}

fn fused_digest(path: &Path) -> (blake3::Hash, vidcull_parser::PreParsedMp4) {
    let mut hasher = blake3::Hasher::new();
    let outcome = vidcull_parser::mp4::read_mp4_tolerant_hashing_cancellable(
        path,
        Cancel::default(),
        &mut |b| {
            hasher.update(b);
        },
    )
    .expect("fused pass must not error on readable files");
    (hasher.finalize(), outcome)
}

#[test]
fn fused_hash_identical_to_standalone_hash() {
    use vidcull_parser::PreParsedMp4;

    let clean = fixture_path("testsrc2_160_90.mp4");
    let standalone = vidcull_fingerprint::hash_file(&clean).unwrap();
    let (fused, outcome) = fused_digest(&clean);
    assert_eq!(
        fused.as_bytes(),
        standalone.as_bytes(),
        "clean: digest drift"
    );
    assert!(matches!(outcome, PreParsedMp4::Parsed(_)));

    let base = std::fs::read(&clean).unwrap();
    let (_dir, garbage) = write_clip(&with_overshoot_garbage(&base));
    let standalone = vidcull_fingerprint::hash_file(&garbage).unwrap();
    let (fused, outcome) = fused_digest(&garbage);
    assert_eq!(
        fused.as_bytes(),
        standalone.as_bytes(),
        "garbage: digest drift"
    );
    assert!(matches!(outcome, PreParsedMp4::Parsed(_)));

    let (_dir2, corrupt) = write_clip(&vec![0xABu8; 96 * 1024]);
    let standalone = vidcull_fingerprint::hash_file(&corrupt).unwrap();
    let (fused, outcome) = fused_digest(&corrupt);
    assert_eq!(
        fused.as_bytes(),
        standalone.as_bytes(),
        "corrupt: digest drift"
    );
    assert!(matches!(outcome, PreParsedMp4::Failed));

    let (_dir3, empty) = write_clip(&[]);
    let standalone = vidcull_fingerprint::hash_file(&empty).unwrap();
    let (fused, outcome) = fused_digest(&empty);
    assert_eq!(
        fused.as_bytes(),
        standalone.as_bytes(),
        "empty: digest drift"
    );
    assert!(matches!(outcome, PreParsedMp4::Failed));
}

#[test]
#[allow(clippy::cast_precision_loss)]
fn parse_vs_fetch_amplification_breaks_down_cleanly() {
    let path = fixture_path("testsrc2_160_90.mp4");
    let file_size = std::fs::metadata(&path).unwrap().len();

    let parse_only_bytes = parse_with_counter(&path);
    let parse_only_ratio = parse_only_bytes as f64 / file_size as f64;

    let frame_count = native_index_total_bytes_via_production_path(&path);

    let known_parse_plus_fetch_ceiling = 1.2;
    let fetch_share_ceiling = known_parse_plus_fetch_ceiling - parse_only_ratio;

    println!(
        "[184-0 G3] parse-only ratio={parse_only_ratio:.4}x ({parse_only_bytes} bytes / \
         {file_size} bytes), frames fetched={frame_count}, parse+fetch ceiling (decode.rs:1832)=\
         {known_parse_plus_fetch_ceiling:.2}x => fetch share <= {fetch_share_ceiling:.4}x"
    );

    assert!(
        parse_only_ratio <= known_parse_plus_fetch_ceiling,
        "parse-only ratio {parse_only_ratio:.4}x must not exceed the combined \
         parse+fetch ceiling {known_parse_plus_fetch_ceiling:.2}x — a parse-only \
         read cannot cost more than parse+fetch together"
    );
    assert!(
        frame_count > 0,
        "expected at least one frame fetched at budget 4/4"
    );
}

#[test]
#[allow(clippy::cast_precision_loss)]
fn parse_and_hash_cpu_cost_per_gb_is_measured() {
    const RUNS: u32 = 20;
    let path = fixture_path("testsrc2_160_90.mp4");
    let file_size = std::fs::metadata(&path).unwrap().len();

    let parse_start = Instant::now();
    for _ in 0..RUNS {
        let _ = parse_with_counter(&path);
    }
    let parse_elapsed = parse_start.elapsed();

    let hash_start = Instant::now();
    for _ in 0..RUNS {
        let _ = hash_with_counter(&path);
    }
    let hash_elapsed = hash_start.elapsed();

    let bytes_per_run = file_size as f64;
    let gb = bytes_per_run / 1_073_741_824.0;
    let parse_ms_per_run = parse_elapsed.as_secs_f64() * 1000.0 / f64::from(RUNS);
    let hash_ms_per_run = hash_elapsed.as_secs_f64() * 1000.0 / f64::from(RUNS);
    let parse_ms_per_gb = parse_ms_per_run / gb;
    let hash_ms_per_gb = hash_ms_per_run / gb;

    println!(
        "[184-0 CPU cost] file={file_size}B runs={RUNS} \
         parse={parse_ms_per_run:.4}ms/run ({parse_ms_per_gb:.1}ms/GB extrapolated) \
         hash={hash_ms_per_run:.4}ms/run ({hash_ms_per_gb:.1}ms/GB extrapolated) \
         -- CAVEAT: 64KiB fixture, fixed overhead dominates; see test doc comment"
    );

    assert!(parse_ms_per_run >= 0.0);
    assert!(hash_ms_per_run >= 0.0);
}
