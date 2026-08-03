
import type { ClipOverlap } from "./types";


export interface TimelineSegment {

  clipFileId: number;

  startFraction: number;

  endFraction: number;

  startMs: number;

  endMs: number;

  coverage: number;

  lane: number;

  introOutro?: boolean;
}


export const DEFAULT_MAX_SEGMENTS = 200;

function clamp01(n: number): number {
  if (n < 0) return 0;
  if (n > 1) return 1;
  return n;
}


export interface IntroOutroPartition {

  shown: ClipOverlap[];

  hidden: ClipOverlap[];
}

export function partitionByIntroOutro(
  overlaps: readonly ClipOverlap[],
): IntroOutroPartition {
  const shown: ClipOverlap[] = [];
  const hidden: ClipOverlap[] = [];
  for (const o of overlaps) {
    if (o.introOutro === true) {
      hidden.push(o);
    } else {
      shown.push(o);
    }
  }
  return { shown, hidden };
}


export function coverageFraction(o: ClipOverlap): number {
  if (o.clipScenes <= 0) return 0;
  return clamp01(o.matchedScenes / o.clipScenes);
}


export function overlapFraction(
  o: ClipOverlap,
  sourceDurationMs: number,
): { start: number; end: number } {
  if (sourceDurationMs <= 0) return { start: 0, end: 0 };
  const start = clamp01(o.startMs / sourceDurationMs);
  const end = clamp01(o.endMs / sourceDurationMs);
  return { start, end: Math.max(start, end) };
}


export function layoutOverlaps(
  sourceDurationMs: number,
  overlaps: readonly ClipOverlap[],
  maxSegments: number = DEFAULT_MAX_SEGMENTS,
): TimelineSegment[] {
  if (sourceDurationMs <= 0 || overlaps.length === 0) return [];

  const ranked = [...overlaps].sort((a, b) => {
    const byCoverage = coverageFraction(b) - coverageFraction(a);
    if (byCoverage !== 0) return byCoverage;
    return a.clipFileId - b.clipFileId;
  });
  const capped = ranked.slice(0, Math.max(0, Math.floor(maxSegments)));

  const ordered = capped
    .map((o) => {
      const { start, end } = overlapFraction(o, sourceDurationMs);
      return {
        clipFileId: o.clipFileId,
        startFraction: start,
        endFraction: end,
        startMs: o.startMs,
        endMs: o.endMs,
        coverage: coverageFraction(o),
        introOutro: o.introOutro,
      };
    })
    .sort((a, b) => {
      if (a.startFraction !== b.startFraction) {
        return a.startFraction - b.startFraction;
      }
      if (a.endFraction !== b.endFraction) return a.endFraction - b.endFraction;
      return a.clipFileId - b.clipFileId;
    });

  const laneEnds: number[] = [];
  return ordered.map((seg) => {
    let lane = laneEnds.findIndex((end) => end <= seg.startFraction);
    if (lane === -1) {
      lane = laneEnds.length;
      laneEnds.push(seg.endFraction);
    } else {
      laneEnds[lane] = seg.endFraction;
    }
    return { ...seg, lane };
  });
}


export function laneCount(segments: readonly TimelineSegment[]): number {
  let max = -1;
  for (const s of segments) {
    if (s.lane > max) max = s.lane;
  }
  return max + 1;
}
