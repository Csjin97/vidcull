import { describe, expect, it } from "vitest";
import { IpcTraceBuffer } from "./ipc-trace";

describe("IpcTraceBuffer", () => {
  it("starts empty: no samples, empty summary, empty stats", () => {
    const buf = new IpcTraceBuffer(8);
    expect(buf.samples()).toEqual([]);
    expect(buf.statsByCmd()).toEqual([]);
    expect(buf.summary()).toBe("");
  });

  it("records samples tagged by command", () => {
    const buf = new IpcTraceBuffer(8);
    buf.record("progress", 5);
    buf.record("thumbnail", 200);
    expect(buf.samples()).toEqual([
      { cmd: "progress", durationMs: 5 },
      { cmd: "thumbnail", durationMs: 200 },
    ]);
  });

  it("ignores non-finite and negative durations", () => {
    const buf = new IpcTraceBuffer(8);
    buf.record("progress", 10);
    buf.record("progress", Number.NaN);
    buf.record("progress", Number.POSITIVE_INFINITY);
    buf.record("progress", -1);
    expect(buf.samples()).toEqual([{ cmd: "progress", durationMs: 10 }]);
  });

  it("is a bounded ring: evicts oldest past capacity", () => {
    const buf = new IpcTraceBuffer(3);
    for (let i = 1; i <= 5; i += 1) buf.record("progress", i);
    expect(buf.samples().map((s) => s.durationMs)).toEqual([3, 4, 5]);
  });

  it("clear() drops the window", () => {
    const buf = new IpcTraceBuffer(8);
    buf.record("progress", 10);
    buf.clear();
    expect(buf.samples()).toEqual([]);
    expect(buf.summary()).toBe("");
  });

  it("aggregates mean/max/count per command, command-ascending", () => {
    const buf = new IpcTraceBuffer(16);
    buf.record("thumbnail", 10);
    buf.record("thumbnail", 30);
    buf.record("cluster_detail", 5);
    expect(buf.statsByCmd()).toEqual([
      { cmd: "cluster_detail", count: 1, meanMs: 5, maxMs: 5 },
      { cmd: "thumbnail", count: 2, meanMs: 20, maxMs: 30 },
    ]);
  });

  it("summary() renders a compact, space-joined per-command line", () => {
    const buf = new IpcTraceBuffer(16);
    buf.record("progress", 5);
    buf.record("thumbnail", 10);
    buf.record("thumbnail", 30);
    expect(buf.summary()).toBe("progress=5.0ms(n=1,max=5.0) thumbnail=20.0ms(n=2,max=30.0)");
  });
});
