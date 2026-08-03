
import { describe, expect, it } from "vitest";
import {
  resolveTimelineMode,
  WHOLE_FILE_RATIO,
  type TimelineMode,
} from "./timeline-visibility";
import type { TrustLevel } from "./types";

describe("resolveTimelineMode — 카테고리(trust) 기반 가시성 truth table", () => {
  it("완전동일(EXACT) — 타임라인 없음 [D1]", () => {
    for (const hasOverlaps of [false, true]) {
      for (const ratio of [1.0, 0.25]) {
        expect(resolveTimelineMode("EXACT", hasOverlaps, ratio)).toEqual({
          kind: "hidden",
        });
      }
    }
  });

  it("재인코딩(VERY_LIKELY) — 타임라인 없음 [D1]", () => {
    for (const hasOverlaps of [false, true]) {
      for (const ratio of [1.0, 0.25]) {
        expect(resolveTimelineMode("VERY_LIKELY", hasOverlaps, ratio)).toEqual({
          kind: "hidden",
        });
      }
    }
  });

  it("EXACT|VERY_LIKELY + overlaps — hidden 유지 [가드]", () => {
    expect(resolveTimelineMode("EXACT", true, 1.0)).toEqual({ kind: "hidden" });
    expect(resolveTimelineMode("VERY_LIKELY", true, 1.0)).toEqual({
      kind: "hidden",
    });
  });

  it("추정(POSSIBLE) + 겹침데이터 — 부분클립 바 [D3]", () => {
    expect(resolveTimelineMode("POSSIBLE", true, 1.0)).toEqual({
      kind: "partial",
    });
    expect(resolveTimelineMode("POSSIBLE", true, 0.25)).toEqual({
      kind: "partial",
    });
  });

  it("추정 + 데이터없음 + 길이비≈1.0 — full-span + 안내(전체 구간 유사) [D3]", () => {
    expect(resolveTimelineMode("POSSIBLE", false, 1.0)).toEqual({
      kind: "full-span",
      notice: "whole-file-est",
    });
    expect(resolveTimelineMode("POSSIBLE", false, WHOLE_FILE_RATIO)).toEqual({
      kind: "full-span",
      notice: "whole-file-est",
    });
  });

  it("추정 + 데이터없음 + 길이비 낮음 — full-span + 안내(데이터 없음) [D3]", () => {
    expect(resolveTimelineMode("POSSIBLE", false, 0.25)).toEqual({
      kind: "full-span",
      notice: "missing-overlap",
    });
  });

  it("추정 + 데이터없음 — 길이비와 무관하게 안내는 항상 present [(a) hole 차단]", () => {
    const hi = resolveTimelineMode("POSSIBLE", false, 1.0);
    const lo = resolveTimelineMode("POSSIBLE", false, 0.25);
    expect(hi.kind).toBe("full-span");
    expect(lo.kind).toBe("full-span");
    expect(hi.kind === "full-span" && hi.notice).toBeTruthy();
    expect(lo.kind === "full-span" && lo.notice).toBeTruthy();
  });

  it("타임라인이 보이는 trust는 POSSIBLE 하나뿐 [회귀가드]", () => {
    const trusts: TrustLevel[] = ["EXACT", "VERY_LIKELY", "POSSIBLE"];
    const visible = new Set<TrustLevel>();
    for (const trust of trusts) {
      for (const hasOverlaps of [false, true]) {
        for (const ratio of [1.0, 0.25]) {
          const mode: TimelineMode = resolveTimelineMode(
            trust,
            hasOverlaps,
            ratio,
          );
          if (mode.kind !== "hidden") visible.add(trust);
        }
      }
    }
    expect([...visible]).toEqual(["POSSIBLE"]);
  });
});
