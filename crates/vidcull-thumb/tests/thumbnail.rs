use std::cell::Cell;

use vidcull_thumb::{GrayView, ThumbnailCache, ThumbnailOptions, encode_thumbnail};

fn gradient(width: u32, height: u32) -> Vec<u8> {
    let mut pixels = Vec::with_capacity(width as usize * height as usize);
    for y in 0..height {
        for x in 0..width {
            pixels.push(u8::try_from((x + y) % 256).unwrap_or(0));
        }
    }
    pixels
}

#[test]
fn encode_produces_decodable_jpeg_at_downscaled_dimensions() {
    let pixels = gradient(640, 360);
    let view = GrayView {
        width: 640,
        height: 360,
        pixels: &pixels,
    };
    let jpeg = encode_thumbnail(view, ThumbnailOptions::default()).expect("encode");

    assert_eq!(&jpeg[..3], &[0xFF, 0xD8, 0xFF], "missing JPEG magic");

    let decoded = image::load_from_memory(&jpeg).expect("decode jpeg");
    assert_eq!((decoded.width(), decoded.height()), (320, 180));
}

#[test]
fn small_frame_is_not_upscaled() {
    let pixels = gradient(100, 50);
    let view = GrayView {
        width: 100,
        height: 50,
        pixels: &pixels,
    };
    let jpeg = encode_thumbnail(view, ThumbnailOptions::default()).expect("encode");
    let decoded = image::load_from_memory(&jpeg).expect("decode jpeg");
    assert_eq!((decoded.width(), decoded.height()), (100, 50));
}

#[test]
fn encode_rejects_dimension_mismatch() {
    let view = GrayView {
        width: 10,
        height: 10,
        pixels: &[0u8; 50],
    };
    let err = encode_thumbnail(view, ThumbnailOptions::default()).expect_err("must reject");
    assert!(matches!(err, vidcull_core::Error::Parse(_)), "got {err:?}");
}

#[test]
fn encode_rejects_zero_dimension_frame() {
    let view = GrayView {
        width: 0,
        height: 10,
        pixels: &[],
    };
    let err = encode_thumbnail(view, ThumbnailOptions::default()).expect_err("must reject");
    assert!(
        matches!(err, vidcull_core::Error::Unsupported(_)),
        "got {err:?}"
    );
}

#[test]
fn cache_stores_then_serves_without_reencoding() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = ThumbnailCache::new(dir.path());
    let key = "abcd1234";

    let pixels = gradient(64, 36);
    let encode_calls = Cell::new(0u32);
    let encode = || {
        encode_calls.set(encode_calls.get() + 1);
        encode_thumbnail(
            GrayView {
                width: 64,
                height: 36,
                pixels: &pixels,
            },
            ThumbnailOptions::default(),
        )
    };

    let first = cache.load_or_store(key, 0, encode).expect("store");
    assert!(!first.from_cache, "first call must be a miss");
    assert_eq!(encode_calls.get(), 1);
    assert!(
        dir.path().join("abcd1234_0_v2.jpg").is_file(),
        "store must leave a file"
    );

    let second = cache
        .load_or_store(key, 0, || panic!("encoder must not run on a cache hit"))
        .expect("hit");
    assert!(second.from_cache, "second call must be a hit");
    assert_eq!(second.bytes, first.bytes, "hit must return identical bytes");
}

#[test]
fn cache_creates_root_directory_on_first_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let nested = dir.path().join("does").join("not").join("exist");
    let cache = ThumbnailCache::new(&nested);

    let pixels = gradient(32, 18);
    let stored = cache
        .load_or_store("00ff", 2, || {
            encode_thumbnail(
                GrayView {
                    width: 32,
                    height: 18,
                    pixels: &pixels,
                },
                ThumbnailOptions::default(),
            )
        })
        .expect("store into a fresh directory");
    assert!(!stored.from_cache);
    assert!(nested.join("00ff_2_v2.jpg").is_file());
}

#[test]
fn cache_rejects_a_non_hex_key_before_encoding() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = ThumbnailCache::new(dir.path());
    let err = cache
        .load_or_store("../escape", 0, || panic!("must not encode on a bad key"))
        .expect_err("bad key must error");
    assert!(matches!(err, vidcull_core::Error::Parse(_)), "got {err:?}");
}
