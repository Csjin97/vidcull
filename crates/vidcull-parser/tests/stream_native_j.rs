mod common;

use std::path::{Path, PathBuf};

use common::binaries_or_skip;
use vidcull_parser::fallback::DecodePath;
use vidcull_parser::fallback::concurrency::DecodeConcurrency;
use vidcull_parser::{
    Cancel, probe_and_decode_sparse_budgets, probe_and_decode_sparse_budgets_streaming,
};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn streaming_native_equals_buffered_native_byte_identical() {
    let test_name = "streaming_native_equals_buffered_native_byte_identical";

    let path = fixture("black_320x180_30fps_1s.mp4");
    if !path.exists() {
        eprintln!("SKIP {test_name}: fixture {} not found", path.display());
        return;
    }

    let Some(bins) = binaries_or_skip(test_name) else {
        return;
    };

    let native_budget = 8usize;
    let fallback_budget = 8usize;

    let serial_conc = DecodeConcurrency::serial();
    let buffered =
        probe_and_decode_sparse_budgets(&bins, &path, native_budget, fallback_budget, &serial_conc)
            .expect("buffered decode must succeed");

    assert_eq!(
        buffered.decode_path,
        DecodePath::Native,
        "fixture must take the native decode path; got {:?}",
        buffered.decode_path
    );
    assert!(
        !buffered.frames.is_empty(),
        "buffered decode produced no frames"
    );

    for cap in [1usize, 2, 4] {
        let conc = DecodeConcurrency::new(cap);
        let mut streamed = Vec::new();
        let (_meta, decode_path) = probe_and_decode_sparse_budgets_streaming(
            &bins,
            &path,
            native_budget,
            fallback_budget,
            &conc,
            Cancel::default(),
            |f| {
                streamed.push(f.clone());
                Ok(())
            },
        )
        .unwrap_or_else(|e| panic!("streaming decode cap={cap} failed: {e}"));

        assert_eq!(
            decode_path,
            DecodePath::Native,
            "cap={cap}: streaming must take native path; got {decode_path:?}"
        );
        assert_eq!(
            streamed.len(),
            buffered.frames.len(),
            "cap={cap}: streamed frame count {} != buffered {}",
            streamed.len(),
            buffered.frames.len()
        );
        assert_eq!(
            streamed, buffered.frames,
            "cap={cap}: §J violated — streamed frames differ from buffered frames"
        );
    }
}

fn real_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("fixtures")
        .join("real")
        .join(name)
}

#[test]
fn streaming_native_multi_grid_point_byte_identical() {
    let test_name = "streaming_native_multi_grid_point_byte_identical";

    let path = real_fixture("gvh834_h264.mp4");
    if !path.exists() {
        eprintln!(
            "SKIP {test_name}: real fixture {} not found",
            path.display()
        );
        return;
    }
    let Some(bins) = binaries_or_skip(test_name) else {
        return;
    };

    let native_budget = 16usize;
    let fallback_budget = 16usize;

    let serial_conc = DecodeConcurrency::serial();
    let buffered =
        probe_and_decode_sparse_budgets(&bins, &path, native_budget, fallback_budget, &serial_conc)
            .expect("buffered decode must succeed");

    assert_eq!(
        buffered.decode_path,
        DecodePath::Native,
        "fixture must take the native path; got {:?}",
        buffered.decode_path
    );
    assert!(
        buffered.frames.len() > 1,
        "expected a multi-grid-point fixture; got {} frame(s)",
        buffered.frames.len()
    );

    for cap in [1usize, 4] {
        let conc = DecodeConcurrency::new(cap);
        let mut streamed = Vec::new();
        let (_meta, decode_path) = probe_and_decode_sparse_budgets_streaming(
            &bins,
            &path,
            native_budget,
            fallback_budget,
            &conc,
            Cancel::default(),
            |f| {
                streamed.push(f.clone());
                Ok(())
            },
        )
        .unwrap_or_else(|e| panic!("streaming decode cap={cap} failed: {e}"));

        assert_eq!(
            decode_path,
            DecodePath::Native,
            "cap={cap}: streaming must take native path; got {decode_path:?}"
        );
        assert_eq!(
            streamed, buffered.frames,
            "cap={cap}: §J violated — multi-point streamed frames differ from buffered"
        );
    }
}
