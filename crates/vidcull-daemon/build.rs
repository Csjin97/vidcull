#![allow(missing_docs)]

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/packed-refs");
    if let Some(ref_path) = head_ref_path() {
        println!("cargo:rerun-if-changed={ref_path}");
    }

    let epoch_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or_else(|_| "0".to_string(), |d| d.as_secs().to_string());

    let sha = git_short_sha().unwrap_or_else(|| "unknown".to_string());
    let stamp = format!("{sha} {epoch_secs}");
    println!("cargo:rustc-env=VIDCULL_BUILD_STAMP={stamp}");
}

fn head_ref_path() -> Option<String> {
    let head = std::fs::read_to_string("../../.git/HEAD").ok()?;
    let target = head.strip_prefix("ref: ")?.trim();
    let path = format!("../../.git/{target}");
    std::path::Path::new(&path).exists().then_some(path)
}

fn git_short_sha() -> Option<String> {
    let sha_out = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !sha_out.status.success() {
        return None;
    }
    let sha = std::str::from_utf8(&sha_out.stdout)
        .ok()?
        .trim()
        .to_string();
    if sha.is_empty() {
        return None;
    }

    let dirty_out = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()?;
    let is_dirty = dirty_out.status.success() && !dirty_out.stdout.is_empty();

    if is_dirty {
        Some(format!("{sha}-dirty"))
    } else {
        Some(sha)
    }
}
