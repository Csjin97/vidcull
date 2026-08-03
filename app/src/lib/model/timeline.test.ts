import { describe, expect, it } from "vitest";
import {
  coverageFraction,
  layoutOverlaps,
  overlapFraction,
  partitionByIntroOutro,
} from "./timeline";
import type { ClipOverlap } from "./types";

function overlap(
  clipFileId: number,
  startMs: number,
  endMs: number,
  matched = 8,
  total = 10,
): ClipOverlap {
  return {
    clipFileId,
    sourceFileId: 1,
    matchedScenes: matched,
    clipScenes: total,
    startMs,
    endMs,
    clipStartMs: startMs,
    clipEndMs: endMs,
  };
}

describe("coverageFraction", () => {
  it("is matchedScenes / clipScenes clamped to [0,1]", () => {
    expect(coverageFraction(overlap(2, 0, 100, 5, 10))).toBe(0.5);
    expect(coverageFraction(overlap(2, 0, 100, 12, 10))).toBe(1);
  });

  it("is 0 (not NaN) when the clip has no scenes", () => {
    expect(coverageFraction(overlap(2, 0, 100, 0, 0))).toBe(0);
  });
});

describe("overlapFraction", () => {
  it("maps a sub-range onto [0,1] of the source duration", () => {
    expect(overlapFraction(overlap(2, 2500, 7500), 10_000)).toEqual({
      start: 0.25,
      end: 0.75,
    });
  });

  it("clamps a clip that runs past the source bounds", () => {
    expect(overlapFraction(overlap(2, -500, 12_000), 10_000)).toEqual({
      start: 0,
      end: 1,
    });
  });

  it("returns a zero-width range at origin for a non-positive duration", () => {
    expect(overlapFraction(overlap(2, 0, 100), 0)).toEqual({ start: 0, end: 0 });
  });
});

describe("layoutOverlaps", () => {
  it("packs non-overlapping clips onto a single lane", () => {
    const segments = layoutOverlaps(10_000, [
      overlap(2, 0, 2000),
      overlap(3, 5000, 7000),
    ]);
    expect(segments.map((s) => s.lane)).toEqual([0, 0]);
  });

  it("pushes a temporally-overlapping clip onto a second lane", () => {
    const segments = layoutOverlaps(10_000, [
      overlap(2, 0, 6000),
      overlap(3, 3000, 9000), 
    ]);
    expect(segments.map((s) => s.lane)).toEqual([0, 1]);
  });

  it("orders segments by start then carries fraction + coverage through", () => {
    const segments = layoutOverlaps(10_000, [
      overlap(3, 6000, 8000, 9, 10),
      overlap(2, 1000, 3000, 5, 10),
    ]);
    expect(segments.map((s) => s.clipFileId)).toEqual([2, 3]);
    expect(segments[0]).toMatchObject({
      clipFileId: 2,
      startFraction: 0.1,
      endFraction: 0.3,
      coverage: 0.5,
      lane: 0,
    });
  });

  it("returns nothing for an empty input or a non-positive duration", () => {
    expect(layoutOverlaps(10_000, [])).toEqual([]);
    expect(layoutOverlaps(0, [overlap(2, 0, 100)])).toEqual([]);
  });

  it("caps the rendered segments to keep the DOM bounded at scale (60fps lock)", () => {
    const many: ClipOverlap[] = [];
    for (let i = 0; i < 10_000; i += 1) {
      const coverage = (i % 10) + 1; 
      many.push(overlap(i + 2, i, i + 50, coverage, 10));
    }
    const segments = layoutOverlaps(20_000, many, 200);
    expect(segments.length).toBe(200);
    expect(segments.every((s) => s.coverage === 1)).toBe(true);
  });

  it("carries introOutro through to the laid-out segment", () => {
    const tagged = { ...overlap(2, 0, 2000), introOutro: true };
    const untagged = { ...overlap(3, 5000, 7000), introOutro: false };
    const omitted = overlap(4, 8000, 9000); 
    const segments = layoutOverlaps(10_000, [tagged, untagged, omitted]);
    const byId = new Map(segments.map((s) => [s.clipFileId, s]));
    expect(byId.get(2)?.introOutro).toBe(true);
    expect(byId.get(3)?.introOutro).toBe(false);
    expect(byId.get(4)?.introOutro).toBeUndefined();
  });
});

describe("partitionByIntroOutro", () => {
  it("puts introOutro:true overlaps in `hidden`, everything else in `shown`", () => {
    const tagged = { ...overlap(2, 0, 2000), introOutro: true };
    const explicitFalse = { ...overlap(3, 0, 2000), introOutro: false };
    const omitted = overlap(4, 0, 2000); 
    const { shown, hidden } = partitionByIntroOutro([
      tagged,
      explicitFalse,
      omitted,
    ]);
    expect(hidden).toEqual([tagged]);
    expect(shown).toEqual([explicitFalse, omitted]);
  });

  it("never hides an overlap merely because the flag is undefined [recall-safe]", () => {
    const omitted = overlap(2, 0, 2000);
    const { shown, hidden } = partitionByIntroOutro([omitted]);
    expect(shown).toEqual([omitted]);
    expect(hidden).toEqual([]);
  });

  it("returns everything shown for an all-clear group (no tagged overlaps)", () => {
    const a = overlap(2, 0, 2000);
    const b = { ...overlap(3, 3000, 4000), introOutro: false };
    const { shown, hidden } = partitionByIntroOutro([a, b]);
    expect(shown).toEqual([a, b]);
    expect(hidden).toEqual([]);
  });

  it("returns everything hidden when every overlap is tagged", () => {
    const a = { ...overlap(2, 0, 2000), introOutro: true };
    const b = { ...overlap(3, 3000, 4000), introOutro: true };
    const { shown, hidden } = partitionByIntroOutro([a, b]);
    expect(shown).toEqual([]);
    expect(hidden).toEqual([a, b]);
  });

  it("is a no-op on an empty list", () => {
    expect(partitionByIntroOutro([])).toEqual({ shown: [], hidden: [] });
  });
});
