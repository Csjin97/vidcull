
import type { DuplicateGroup, FileEntry } from "./types";
import type { BestCopyMode } from "../data/settings";


export function codecEfficiency(codec: string | null): number {
  if (!codec) return 100;
  const c = codec.toLowerCase();
  if (c.includes("av1")) return 350;
  if (c.includes("h265") || c.includes("hevc")) return 300;
  if (c.includes("vp9")) return 260;
  if (c.includes("h264") || c.includes("avc")) return 200;
  return 100;
}


export function effectiveBitrate(file: FileEntry, mode: BestCopyMode): number {
  if (mode === "archival" || mode === "min_size" || mode === "compatible" || mode === "max_resolution") {
    return file.bitrateBps;
  }
  return file.bitrateBps * (codecEfficiency(file.codec) / 100);
}


export function pixelCount(file: FileEntry): number {
  return file.width * file.height;
}


export function compatibilityScore(file: FileEntry): number {
  let score = 0;
  if (file.codec) {
    const c = file.codec.toLowerCase();
    if (c.includes("h264") || c.includes("avc")) {
      score += 5;
    }
  }
  if (file.container) {
    const cont = file.container.toLowerCase();
    if (cont.includes("mp4")) {
      score += 5;
    }
  }
  return score;
}


export function compareQuality(a: FileEntry, b: FileEntry, mode: BestCopyMode = "archival"): number {
  if (mode === "min_size") {
    if (a.sizeBytes !== b.sizeBytes) {
      return a.sizeBytes - b.sizeBytes; 
    }
    return a.fileId - b.fileId;
  }

  const byPixels = pixelCount(b) - pixelCount(a);
  if (byPixels !== 0) return byPixels;

  if (mode === "compatible") {
    const byComp = compatibilityScore(b) - compatibilityScore(a);
    if (byComp !== 0) return byComp;
  }

  const byBitrate = effectiveBitrate(b, mode) - effectiveBitrate(a, mode);
  if (byBitrate !== 0) return byBitrate;
  const bySize = b.sizeBytes - a.sizeBytes;
  if (bySize !== 0) return bySize;
  return a.fileId - b.fileId;
}


export function clientBestFileId(members: readonly FileEntry[], mode: BestCopyMode = "archival"): number | null {
  if (members.length === 0) return null;
  return [...members].sort((a, b) => compareQuality(a, b, mode))[0].fileId;
}


export function resolveBestFileId(
  group: DuplicateGroup,
  mode: BestCopyMode = "archival",
  activeMode: BestCopyMode = "archival"
): number | null {
  if (mode === activeMode && group.bestFileId !== null) {
    if (group.members.some((m) => m.fileId === group.bestFileId)) {
      return group.bestFileId;
    }
  }
  return clientBestFileId(group.members, mode);
}


export function isBest(
  group: DuplicateGroup,
  fileId: number,
  activeMode: BestCopyMode = "archival"
): boolean {
  return resolveBestFileId(group, activeMode, activeMode) === fileId;
}


export function membersByQuality(
  group: DuplicateGroup,
  activeMode: BestCopyMode = "archival"
): FileEntry[] {
  const bestId = resolveBestFileId(group, activeMode, activeMode);
  if (bestId === null) {
    return [...group.members].sort((a, b) => compareQuality(a, b, activeMode));
  }
  const members = [...group.members];
  members.sort((a, b) => {
    if (a.fileId === bestId) return -1;
    if (b.fileId === bestId) return 1;
    return compareQuality(a, b, activeMode);
  });
  return members;
}
