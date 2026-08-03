use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use vidcull_core::{Error, Result};
use vidcull_parser::fallback::{
    FfmpegBinaries, RENDER_TIMEOUT_SECS, effective_timeout, run_with_timeout,
};

use crate::plan::{RenderPlan, plan};
use crate::transform::Recipe;

pub fn render(bins: &FfmpegBinaries, render_plan: &RenderPlan) -> Result<PathBuf> {
    if let Some(sidecar) = &render_plan.sidecar_srt {
        fs::write(&sidecar.path, &sidecar.content).map_err(Error::Io)?;
    }
    let output = run_with_timeout(
        Command::new(bins.ffmpeg()).args(&render_plan.args),
        effective_timeout(RENDER_TIMEOUT_SECS),
        "synth",
    )?;
    if !output.status.success() {
        return Err(Error::Decode(format!(
            "ffmpeg synth render failed ({}) for {}",
            output.status,
            render_plan.output.display()
        )));
    }
    Ok(render_plan.output.clone())
}

pub fn render_recipe(
    bins: &FfmpegBinaries,
    recipe: &Recipe,
    seed: u64,
    out_dir: &Path,
) -> Result<PathBuf> {
    let render_plan = plan(recipe, seed, out_dir)?;
    render(bins, &render_plan)
}

pub fn render_testsrc(
    bins: &FfmpegBinaries,
    out_dir: &Path,
    seed: u64,
    duration_ms: u64,
    width: u32,
    height: u32,
) -> Result<PathBuf> {
    let output = out_dir.join(format!("source.{seed:016x}.mp4"));
    let lavfi = format!(
        "testsrc=duration={}:size={width}x{height}:rate=30",
        seconds(duration_ms)
    );
    let mut args: Vec<OsString> = Vec::new();
    for flag in [
        "-v",
        "error",
        "-hide_banner",
        "-nostdin",
        "-y",
        "-fflags",
        "+bitexact",
        "-f",
        "lavfi",
        "-i",
    ] {
        args.push(flag.into());
    }
    args.push(lavfi.into());
    for flag in [
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p",
        "-g",
        "30",
        "-an",
        "-map_metadata",
        "-1",
        "-bitexact",
    ] {
        args.push(flag.into());
    }
    args.push(output.clone().into_os_string());

    let run_output = run_with_timeout(
        Command::new(bins.ffmpeg()).args(&args),
        effective_timeout(RENDER_TIMEOUT_SECS),
        "synth",
    )?;
    if !run_output.status.success() {
        return Err(Error::Decode(format!(
            "ffmpeg testsrc render failed ({})",
            run_output.status
        )));
    }
    Ok(output)
}

#[allow(clippy::too_many_arguments)]
pub fn render_source(
    bins: &FfmpegBinaries,
    out_dir: &Path,
    name: &str,
    pattern: &str,
    duration_ms: u64,
    width: u32,
    height: u32,
    fps: u32,
    gop: u32,
) -> Result<PathBuf> {
    let output = out_dir.join(format!("source.{name}.mp4"));
    let lavfi = format!("{pattern}=size={width}x{height}:rate={fps}");
    let secs = seconds(duration_ms);
    let gop = gop.to_string();
    let fps = fps.to_string();
    let mut args: Vec<OsString> = Vec::new();
    for flag in [
        "-v",
        "error",
        "-hide_banner",
        "-nostdin",
        "-y",
        "-fflags",
        "+bitexact",
        "-f",
        "lavfi",
        "-i",
    ] {
        args.push(flag.into());
    }
    args.push(lavfi.into());
    for flag in [
        "-t",
        &secs,
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p",
        "-r",
        &fps,
        "-g",
        &gop,
        "-keyint_min",
        &gop,
        "-sc_threshold",
        "0",
        "-an",
        "-map_metadata",
        "-1",
        "-bitexact",
    ] {
        args.push(flag.into());
    }
    args.push(output.clone().into_os_string());

    let run_output = run_with_timeout(
        Command::new(bins.ffmpeg()).args(&args),
        effective_timeout(RENDER_TIMEOUT_SECS),
        "synth",
    )?;
    if !run_output.status.success() {
        return Err(Error::Decode(format!(
            "ffmpeg source render failed ({}) for pattern {pattern:?}",
            run_output.status
        )));
    }
    Ok(output)
}

fn seconds(ms: u64) -> String {
    format!("{}.{:03}", ms / 1000, ms % 1000)
}
