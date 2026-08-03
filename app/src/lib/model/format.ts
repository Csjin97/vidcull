
import type { FileEntry, TrustLevel } from "./types";


export function fileName(path: string): string {
  const normalised = path.replace(/\\/g, "/");
  const idx = normalised.lastIndexOf("/");
  return idx === -1 ? normalised : normalised.slice(idx + 1);
}


export function parentDir(path: string): string {
  const normalised = path.replace(/\\/g, "/");
  const idx = normalised.lastIndexOf("/");
  return idx === -1 ? "" : normalised.slice(0, idx);
}

const BYTE_UNITS = ["B", "KB", "MB", "GB", "TB", "PB"] as const;


export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) {
    return "0 B";
  }
  const exp = Math.min(
    BYTE_UNITS.length - 1,
    Math.floor(Math.log(bytes) / Math.log(1024)),
  );
  const value = bytes / 1024 ** exp;
  const text = exp === 0 ? String(value) : value.toFixed(1);
  return `${text} ${BYTE_UNITS[exp]}`;
}


export function formatDuration(durationMs: number): string {
  const totalSeconds = Math.max(0, Math.round(durationMs / 1000));
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;
  const pad = (n: number): string => String(n).padStart(2, "0");
  return hours > 0
    ? `${hours}:${pad(minutes)}:${pad(seconds)}`
    : `${minutes}:${pad(seconds)}`;
}


export function formatResolution(width: number, height: number): string {
  return `${width}×${height}`;
}


export function resolutionLabel(width: number, height: number): string {
  const longEdge = Math.max(width, height);
  const shortEdge = Math.min(width, height);
  if (longEdge >= 3840) return "4K";
  if (longEdge >= 2560) return "1440p";
  if (shortEdge >= 1080) return "1080p";
  if (shortEdge >= 720) return "720p";
  if (shortEdge >= 480) return "480p";
  return `${shortEdge}p`;
}


export function formatBitrate(bitrateBps: number): string {
  if (!Number.isFinite(bitrateBps) || bitrateBps <= 0) {
    return "—";
  }
  return `${(bitrateBps / 1_000_000).toFixed(1)} Mbps`;
}


export function trustLabel(trust: TrustLevel): string {
  switch (trust) {
    case "EXACT":
      return "완전 동일";
    case "VERY_LIKELY":
      return "유사 (재인코딩)";
    case "POSSIBLE":
      return "유사 (추정)";
  }
}


export function trustShortLabel(trust: TrustLevel): string {
  switch (trust) {
    case "EXACT":
      return "완전 동일";
    case "VERY_LIKELY":
      return "재인코딩";
    case "POSSIBLE":
      return "추정";
  }
}


export function trustClass(trust: TrustLevel): string {
  switch (trust) {
    case "EXACT":
      return "exact";
    case "VERY_LIKELY":
      return "very-likely";
    case "POSSIBLE":
      return "possible";
  }
}


export function specLine(file: FileEntry): string {
  return [
    resolutionLabel(file.width, file.height),
    formatDuration(file.durationMs),
    formatBytes(file.sizeBytes),
    file.codec,
  ].join(" · ");
}
