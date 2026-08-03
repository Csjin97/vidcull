#![doc(html_no_source)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use vidcull_core::{Error, Result};

#[derive(Debug, Clone, Copy)]
pub struct GrayView<'a> {
    pub width: u32,
    pub height: u32,
    pub pixels: &'a [u8],
}

#[derive(Debug, Clone, Copy)]
pub struct ThumbnailOptions {
    pub max_dim: u32,
    pub jpeg_quality: u8,
}

impl Default for ThumbnailOptions {
    fn default() -> Self {
        Self {
            max_dim: 320,
            jpeg_quality: 75,
        }
    }
}

pub fn encode_thumbnail(view: GrayView<'_>, opts: ThumbnailOptions) -> Result<Vec<u8>> {
    if view.width == 0 || view.height == 0 {
        return Err(Error::Unsupported(
            "thumbnail: refusing to encode a zero-dimension frame".to_owned(),
        ));
    }
    let expected = view.width as usize * view.height as usize;
    if view.pixels.len() != expected {
        return Err(Error::Parse(format!(
            "thumbnail: pixel buffer has {} bytes, expected {}×{}={expected}",
            view.pixels.len(),
            view.width,
            view.height,
        )));
    }

    let source = image::GrayImage::from_raw(view.width, view.height, view.pixels.to_vec())
        .ok_or_else(|| Error::Parse("thumbnail: image buffer rejected by encoder".to_owned()))?;

    let (target_w, target_h) = fit_within(view.width, view.height, opts.max_dim);
    let scaled = if (target_w, target_h) == (view.width, view.height) {
        source
    } else {
        image::imageops::resize(
            &source,
            target_w,
            target_h,
            image::imageops::FilterType::Triangle,
        )
    };

    let mut out = Vec::new();
    let mut encoder =
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, opts.jpeg_quality);
    encoder
        .encode(
            scaled.as_raw(),
            target_w,
            target_h,
            image::ExtendedColorType::L8,
        )
        .map_err(|err| Error::Decode(format!("thumbnail: jpeg encode failed: {err}")))?;
    Ok(out)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "scale < 1 so the rounded result is < the original dimension, far inside u32"
)]
fn fit_within(width: u32, height: u32, max_dim: u32) -> (u32, u32) {
    let longest = width.max(height);
    if max_dim == 0 || longest <= max_dim {
        return (width, height);
    }
    let scale = f64::from(max_dim) / f64::from(longest);
    let scaled_w = ((f64::from(width) * scale).round() as u32).max(1);
    let scaled_h = ((f64::from(height) * scale).round() as u32).max(1);
    (scaled_w, scaled_h)
}

#[must_use]
pub fn to_data_uri(jpeg: &[u8]) -> String {
    format!("data:image/jpeg;base64,{}", base64_encode(jpeg))
}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(BASE64_ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
        out.push(BASE64_ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            BASE64_ALPHABET[((triple >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            BASE64_ALPHABET[(triple & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedThumbnail {
    pub bytes: Vec<u8>,
    pub from_cache: bool,
}

#[derive(Debug, Clone)]
pub struct ThumbnailCache {
    root: PathBuf,
}

impl ThumbnailCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path_for(&self, content_hex: &str, frame_index: u32) -> PathBuf {
        self.root
            .join(format!("{content_hex}_{frame_index}_v2.jpg"))
    }

    pub fn load_or_store<F>(
        &self,
        content_hex: &str,
        frame_index: u32,
        encode: F,
    ) -> Result<CachedThumbnail>
    where
        F: FnOnce() -> Result<Vec<u8>>,
    {
        validate_hex_key(content_hex)?;
        let path = self.path_for(content_hex, frame_index);
        if let Some(bytes) = read_if_exists(&path)? {
            return Ok(CachedThumbnail {
                bytes,
                from_cache: true,
            });
        }
        let bytes = encode()?;
        write_atomic(&path, &bytes)?;
        Ok(CachedThumbnail {
            bytes,
            from_cache: false,
        })
    }
}

fn validate_hex_key(key: &str) -> Result<()> {
    if key.is_empty()
        || !key
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(Error::Parse(format!(
            "thumbnail: cache key {key:?} is not lowercase hex"
        )));
    }
    Ok(())
}

fn read_if_exists(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(Error::Io(err)),
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let pid = std::process::id();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut name = path
        .file_name()
        .map(std::ffi::OsString::from)
        .unwrap_or_default();
    name.push(format!(".{pid}.{seq}.tmp"));
    let mut temp = path.to_path_buf();
    temp.set_file_name(name);

    std::fs::write(&temp, bytes)?;

    if let Err(e) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(e.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_within_leaves_small_frames_untouched() {
        assert_eq!(fit_within(320, 180, 320), (320, 180));
        assert_eq!(fit_within(100, 50, 320), (100, 50));
        assert_eq!(fit_within(4000, 2000, 0), (4000, 2000));
    }

    #[test]
    fn fit_within_scales_the_longest_side_preserving_aspect() {
        assert_eq!(fit_within(3840, 2160, 320), (320, 180));
        assert_eq!(fit_within(1920, 1080, 320), (320, 180));
        assert_eq!(fit_within(1080, 1920, 320), (180, 320));
        assert_eq!(fit_within(10_000, 1, 320), (320, 1));
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn data_uri_has_the_jpeg_prefix() {
        let uri = to_data_uri(&[0xFF, 0xD8, 0xFF]);
        assert!(uri.starts_with("data:image/jpeg;base64,"), "{uri}");
        assert!(uri.ends_with("/9j/"), "{uri}");
    }

    #[test]
    fn hex_key_validation_blocks_traversal() {
        assert!(validate_hex_key("deadbeef").is_ok());
        assert!(validate_hex_key("").is_err());
        assert!(validate_hex_key("../etc").is_err());
        assert!(validate_hex_key("DEADBEEF").is_err());
        assert!(validate_hex_key("dead_beef").is_err());
    }

    #[test]
    fn write_atomic_concurrent_writes_no_torn_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("thumb.jpg");

        let payload_a = vec![0xAAu8; 128];
        let payload_b = vec![0xBBu8; 128];

        let target_a = target.clone();
        let pa = payload_a.clone();
        let handle_a = std::thread::spawn(move || write_atomic(&target_a, &pa));

        let target_b = target.clone();
        let pb = payload_b.clone();
        let handle_b = std::thread::spawn(move || write_atomic(&target_b, &pb));

        handle_a
            .join()
            .expect("thread A panicked")
            .expect("write A failed");
        handle_b
            .join()
            .expect("thread B panicked")
            .expect("write B failed");

        let on_disk = std::fs::read(&target).expect("target missing after concurrent writes");
        assert!(
            on_disk == payload_a || on_disk == payload_b,
            "final file is neither payload_a nor payload_b — torn write detected"
        );

        let orphans: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(
            orphans.is_empty(),
            "orphan .tmp files left behind: {orphans:?}"
        );
    }

    #[test]
    fn write_atomic_cleans_up_temp_on_rename_failure() {
        let dir = tempfile::tempdir().expect("tempdir");

        let subdir = dir.path().join("sub");
        std::fs::create_dir_all(&subdir).expect("create subdir");
        let target = subdir.join("thumb.jpg");

        write_atomic(&target, b"ok").expect("first write must succeed");

        std::fs::remove_file(&target).expect("remove first file");
        std::fs::create_dir_all(&target).expect("create dir at target path");

        let result = write_atomic(&target, b"should fail");
        assert!(result.is_err(), "expected rename to fail");

        let orphans: Vec<_> = std::fs::read_dir(&subdir)
            .expect("read_dir")
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(
            orphans.is_empty(),
            "orphan .tmp files left after rename failure: {orphans:?}"
        );
    }
}
