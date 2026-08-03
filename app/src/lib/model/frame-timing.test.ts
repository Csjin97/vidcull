import { describe, expect, it } from "vitest";
import {
  FRAME_BUDGET_MS_60FPS,
  FrameTimingBuffer,
  type FrameClock,
  percentile,
  sampleFrames,
} from "./frame-timing";

describe("percentile (nearest-rank)", () => {
  it("returns 0 for empty input", () => {
    expect(percentile([], 0.95)).toBe(0);
  });

  it("picks the nearest-rank value and is order-independent", () => {
    const values = [5, 1, 4, 2, 3];
    expect(percentile(values, 1)).toBe(5); 
    expect(percentile(values, 0)).toBe(1); 
    expect(percentile([10, 20, 30, 40], 0.95)).toBe(40);
  });

  it("clamps q into [0,1]", () => {
    expect(percentile([1, 2, 3], 5)).toBe(3);
    expect(percentile([1, 2, 3], -5)).toBe(1);
  });
});

describe("FrameTimingBuffer", () => {
  it("reports all-zero stats for an empty window (never NaN)", () => {
    const stats = new FrameTimingBuffer(8).stats();
    expect(stats).toEqual({
      frames: 0,
      meanMs: 0,
      p95Ms: 0,
      maxMs: 0,
      droppedFrames: 0,
      droppedFraction: 0,
    });
  });

  it("computes mean, max, p95 and dropped frames against the budget", () => {
    const buf = new FrameTimingBuffer(16);
    for (const d of [10, 12, 8, 40]) buf.record(d);
    const stats = buf.stats();
    expect(stats.frames).toBe(4);
    expect(stats.meanMs).toBeCloseTo((10 + 12 + 8 + 40) / 4);
    expect(stats.maxMs).toBe(40);
    expect(stats.droppedFrames).toBe(1);
    expect(stats.droppedFraction).toBeCloseTo(0.25);
    expect(stats.p95Ms).toBe(40);
  });

  it("honours a custom budget", () => {
    const buf = new FrameTimingBuffer(8);
    for (const d of [10, 20, 30]) buf.record(d);
    expect(buf.stats(25).droppedFrames).toBe(1);
    expect(buf.stats(5).droppedFrames).toBe(3);
  });

  it("ignores non-finite and negative samples", () => {
    const buf = new FrameTimingBuffer(8);
    buf.record(16);
    buf.record(Number.NaN);
    buf.record(Number.POSITIVE_INFINITY);
    buf.record(-5);
    expect(buf.samples()).toEqual([16]);
  });

  it("is a bounded ring: evicts oldest past capacity", () => {
    const buf = new FrameTimingBuffer(3);
    for (const d of [1, 2, 3, 4, 5]) buf.record(d);
    expect(buf.samples()).toEqual([3, 4, 5]);
  });

  it("clear() drops the window", () => {
    const buf = new FrameTimingBuffer(4);
    buf.record(10);
    buf.clear();
    expect(buf.samples()).toEqual([]);
    expect(buf.stats().frames).toBe(0);
  });
});

describe("sampleFrames", () => {
  it("no-ops with a null clock (no DOM) and returns a safe stop()", () => {
    const buf = new FrameTimingBuffer(8);
    const stop = sampleFrames(buf, null);
    expect(buf.samples()).toEqual([]);
    expect(() => stop()).not.toThrow();
  });

  it("records inter-frame deltas, skipping the baseline frame", () => {
    const pending: ((t: number) => void)[] = [];
    const clock: FrameClock = {
      requestAnimationFrame: (cb) => {
        pending.push(cb);
        return pending.length; 
      },
      cancelAnimationFrame: () => {},
    };

    const buf = new FrameTimingBuffer(16);
    const stop = sampleFrames(buf, clock);

    const fire = (t: number): void => {
      const cb = pending.shift();
      expect(cb).toBeDefined();
      cb?.(t);
    };

    fire(100); 
    fire(116); 
    fire(150); 
    stop();

    expect(buf.samples()).toEqual([16, 34]);
    expect(buf.stats().droppedFrames).toBe(1);
  });

  it("stop() halts further recording", () => {
    const pending: ((t: number) => void)[] = [];
    const clock: FrameClock = {
      requestAnimationFrame: (cb) => {
        pending.push(cb);
        return pending.length;
      },
      cancelAnimationFrame: () => {},
    };
    const buf = new FrameTimingBuffer(8);
    const stop = sampleFrames(buf, clock);

    const next = pending.shift();
    next?.(0); 
    stop();
    const stale = pending.shift();
    stale?.(16);
    expect(buf.samples()).toEqual([]);
  });
});

describe("FRAME_BUDGET_MS_60FPS", () => {
  it("is ~16.67ms", () => {
    expect(FRAME_BUDGET_MS_60FPS).toBeCloseTo(16.667, 2);
  });
});
