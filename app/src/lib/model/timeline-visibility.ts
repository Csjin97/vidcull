
import type { TrustLevel } from "./types";

export type TimelineMode =
  | { kind: "hidden" }
  | { kind: "partial" }
  | { kind: "full-span"; notice: "whole-file-est" | "missing-overlap" };


export const WHOLE_FILE_RATIO = 0.9;

export function resolveTimelineMode(
  trust: TrustLevel,
  hasOverlaps: boolean,
  lengthRatio: number, 
): TimelineMode {
  if (trust !== "POSSIBLE") return { kind: "hidden" };
  if (hasOverlaps) return { kind: "partial" };
  return {
    kind: "full-span",
    notice: lengthRatio >= WHOLE_FILE_RATIO ? "whole-file-est" : "missing-overlap",
  };
}
