import { describe, expect, it } from "vitest";
import { computeWindow, isNearEnd } from "./window";

describe("computeWindow", () => {
  const base = { rowHeight: 100, viewportHeight: 500, count: 1000, overscan: 2 };

  it("renders the top rows at scrollTop 0 with overscan below only", () => {
    const w = computeWindow({ ...base, scrollTop: 0 });
    expect(w.startIndex).toBe(0); 
    expect(w.endIndex).toBe(7); 
    expect(w.offsetY).toBe(0);
    expect(w.totalHeight).toBe(100_000);
  });

  it("windows around a mid-list scroll position with overscan both sides", () => {
    const w = computeWindow({ ...base, scrollTop: 10_000 }); 
    expect(w.startIndex).toBe(98); 
    expect(w.endIndex).toBe(107); 
    expect(w.offsetY).toBe(9_800); 
  });

  it("clamps the end index at the row count", () => {
    const w = computeWindow({ ...base, scrollTop: 100_000 });
    expect(w.endIndex).toBe(1000);
    expect(w.startIndex).toBeLessThan(w.endIndex);
  });

  it("returns an empty window for an empty list", () => {
    const w = computeWindow({ ...base, count: 0, scrollTop: 0 });
    expect(w).toEqual({ startIndex: 0, endIndex: 0, offsetY: 0, totalHeight: 0 });
  });

  it("guards against a zero row height", () => {
    const w = computeWindow({ ...base, rowHeight: 0, scrollTop: 0 });
    expect(w.endIndex).toBe(0);
  });
});

describe("isNearEnd", () => {
  const base = { rowHeight: 100, viewportHeight: 500, count: 50 };

  it("is false near the top", () => {
    expect(isNearEnd({ ...base, scrollTop: 0 }, 6)).toBe(false);
  });

  it("is true within the threshold of the end", () => {
    expect(isNearEnd({ ...base, scrollTop: 4_000 }, 6)).toBe(true);
  });
});
