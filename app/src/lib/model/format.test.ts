import { describe, expect, it } from "vitest";
import {
  fileName,
  formatBitrate,
  formatBytes,
  formatDuration,
  formatResolution,
  parentDir,
  resolutionLabel,
  specLine,
  trustClass,
  trustLabel,
  trustShortLabel,
} from "./format";
import type { FileEntry } from "./types";

describe("path helpers", () => {
  it("extracts the basename from forward- and back-slash paths", () => {
    expect(fileName("/media/movies/a.mkv")).toBe("a.mkv");
    expect(fileName("C:\\videos\\clip.mp4")).toBe("clip.mp4");
    expect(fileName("loose.mp4")).toBe("loose.mp4");
  });

  it("extracts the parent directory", () => {
    expect(parentDir("/media/movies/a.mkv")).toBe("/media/movies");
    expect(parentDir("C:\\videos\\clip.mp4")).toBe("C:/videos");
    expect(parentDir("loose.mp4")).toBe("");
  });
});

describe("formatBytes", () => {
  it("scales through binary units", () => {
    expect(formatBytes(0)).toBe("0 B");
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(1024)).toBe("1.0 KB");
    expect(formatBytes(1536)).toBe("1.5 KB");
    expect(formatBytes(1024 ** 3)).toBe("1.0 GB");
    expect(formatBytes(1024 ** 4 * 2)).toBe("2.0 TB");
  });

  it("guards against negative / non-finite input", () => {
    expect(formatBytes(-5)).toBe("0 B");
    expect(formatBytes(Number.NaN)).toBe("0 B");
  });
});

describe("formatDuration", () => {
  it("renders M:SS under an hour and H:MM:SS over", () => {
    expect(formatDuration(0)).toBe("0:00");
    expect(formatDuration(65_000)).toBe("1:05");
    expect(formatDuration(3_661_000)).toBe("1:01:01");
  });
});

describe("resolution", () => {
  it("formats WIDTH×HEIGHT", () => {
    expect(formatResolution(3840, 2160)).toBe("3840×2160");
  });

  it("labels common resolutions", () => {
    expect(resolutionLabel(3840, 2160)).toBe("4K");
    expect(resolutionLabel(2560, 1440)).toBe("1440p");
    expect(resolutionLabel(1920, 1080)).toBe("1080p");
    expect(resolutionLabel(1280, 720)).toBe("720p");
    expect(resolutionLabel(854, 480)).toBe("480p");
    expect(resolutionLabel(640, 360)).toBe("360p");
  });
});

describe("formatBitrate", () => {
  it("renders Mbps with one decimal", () => {
    expect(formatBitrate(8_000_000)).toBe("8.0 Mbps");
    expect(formatBitrate(0)).toBe("—");
  });
});

describe("trust labels", () => {
  it("maps each level to a Korean label and css class", () => {
    expect(trustLabel("EXACT")).toBe("완전 동일");
    expect(trustLabel("VERY_LIKELY")).toBe("유사 (재인코딩)");
    expect(trustLabel("POSSIBLE")).toBe("유사 (추정)");
    expect(trustClass("EXACT")).toBe("exact");
    expect(trustClass("VERY_LIKELY")).toBe("very-likely");
    expect(trustClass("POSSIBLE")).toBe("possible");
  });

  it("drops the 유사 prefix in the compact label", () => {
    expect(trustShortLabel("EXACT")).toBe("완전 동일");
    expect(trustShortLabel("VERY_LIKELY")).toBe("재인코딩");
    expect(trustShortLabel("POSSIBLE")).toBe("추정");
  });
});

describe("specLine", () => {
  it("joins resolution, duration, size and codec", () => {
    const file: FileEntry = {
      fileId: 1,
      path: "/v/a.mp4",
      sizeBytes: 1024 ** 3,
      width: 1920,
      height: 1080,
      durationMs: 65_000,
      bitrateBps: 8_000_000,
      codec: "h264",
      container: "mp4",
      thumbnailUrl: null,
    };
    expect(specLine(file)).toBe("1080p · 1:05 · 1.0 GB · h264");
  });
});
