import { describe, expect, it } from "vitest";
import {
  ACTIVE_POLL_MS,
  ERROR_BACKOFF_BASE_MS,
  ERROR_BACKOFF_MAX_MS,
  ETA_DISPLAY_ALPHA,
  ETA_NULL_HOLD_POLLS,
  ETA_RATE_TAU_SEC,
  EtaEstimator,
  IDLE_POLL_MS,
  ProgressHistory,
  drainBytesPerSec,
  etaFromDrain,
  isDrainStalled,
  drainRateSeries,
  collapseOnScroll,
  completedFraction,
  currentFileLabel,
  errorBackoffMs,
  etaSeconds,
  isAnalyzingPartialClips,
  isScanning,
  nextActivityMs,
  formatRelativeActivity,
  partialCompleted,
  partialFailedTotal,
  partialOutstanding,
  partialSkippedBreakdown,
  partialSkippedTotal,
  partialTotal,
  pollIntervalMs,
  refreshLimit,
  shouldRefreshGroups,
  simulateProgress,
  sparklinePath,
  throughputSeries,
  totalTasks,
  type ProgressSample,
} from "./progress";
import type { ProgressSnapshot } from "./types";

describe("pollIntervalMs (idle-aware, §I3)", () => {
  it("polls fast while scanning and slows to a heartbeat when idle", () => {
    expect(pollIntervalMs(true)).toBe(ACTIVE_POLL_MS);
    expect(pollIntervalMs(false)).toBe(IDLE_POLL_MS);
    expect(IDLE_POLL_MS).toBeGreaterThan(ACTIVE_POLL_MS);
    expect(IDLE_POLL_MS).toBeGreaterThan(0);
  });
});

describe("errorBackoffMs (exponential backoff when daemon unreachable)", () => {
  it("returns the base delay for the first failure", () => {
    expect(errorBackoffMs(1)).toBe(ERROR_BACKOFF_BASE_MS);
  });

  it("doubles on each consecutive failure", () => {
    expect(errorBackoffMs(1)).toBe(2_000);
    expect(errorBackoffMs(2)).toBe(4_000);
    expect(errorBackoffMs(3)).toBe(8_000);
    expect(errorBackoffMs(4)).toBe(16_000);
  });

  it("caps at ERROR_BACKOFF_MAX_MS", () => {
    expect(errorBackoffMs(5)).toBe(ERROR_BACKOFF_MAX_MS);
    expect(errorBackoffMs(100)).toBe(ERROR_BACKOFF_MAX_MS);
  });

  it("returns the base for zero or negative input (no error)", () => {
    expect(errorBackoffMs(0)).toBe(ERROR_BACKOFF_BASE_MS);
    expect(errorBackoffMs(-1)).toBe(ERROR_BACKOFF_BASE_MS);
  });
});

function snap(
  pending: number,
  running: number,
  done: number,
  failed = 0,
): ProgressSnapshot {
  return { pending, running, done, failed };
}

describe("totalTasks / completedFraction / isScanning", () => {
  it("sums every state for the total", () => {
    expect(totalTasks(snap(10, 2, 88, 0))).toBe(100);
  });

  it("counts done AND failed as resolved work", () => {
    expect(completedFraction(snap(0, 0, 80, 20))).toBe(1);
    expect(completedFraction(snap(50, 0, 30, 20))).toBe(0.5);
  });

  it("is 0 (not NaN) for an empty queue", () => {
    expect(completedFraction(snap(0, 0, 0, 0))).toBe(0);
  });

  it("is scanning while anything is pending or running", () => {
    expect(isScanning(snap(1, 0, 5, 0))).toBe(true);
    expect(isScanning(snap(0, 3, 5, 0))).toBe(true);
    expect(isScanning(snap(0, 0, 5, 1))).toBe(false);
  });
});

describe("throughputSeries", () => {
  it("derives done-tasks-per-second between consecutive samples", () => {
    const samples: ProgressSample[] = [
      { timestampMs: 0, snapshot: snap(100, 0, 0) },
      { timestampMs: 1000, snapshot: snap(90, 0, 10) },
      { timestampMs: 3000, snapshot: snap(60, 0, 40) },
    ];
    expect(throughputSeries(samples)).toEqual([10, 15]);
  });

  it("never goes negative when the queue is reset/re-scanned", () => {
    const samples: ProgressSample[] = [
      { timestampMs: 0, snapshot: snap(0, 0, 50) },
      { timestampMs: 1000, snapshot: snap(100, 0, 0) }, 
    ];
    expect(throughputSeries(samples)).toEqual([0]);
  });

  it("measures the queue draining even when the done count is flat", () => {
    const samples: ProgressSample[] = [
      { timestampMs: 0, snapshot: snap(100, 1, 5000) },
      { timestampMs: 1000, snapshot: snap(80, 1, 5000) },
    ];
    expect(throughputSeries(samples)).toEqual([20]);
  });

  it("returns an empty series for fewer than two samples", () => {
    expect(throughputSeries([])).toEqual([]);
    expect(
      throughputSeries([{ timestampMs: 0, snapshot: snap(1, 0, 0) }]),
    ).toEqual([]);
  });

  it("guards against a zero or reversed time delta", () => {
    const samples: ProgressSample[] = [
      { timestampMs: 1000, snapshot: snap(0, 0, 0) },
      { timestampMs: 1000, snapshot: snap(0, 0, 10) }, 
    ];
    expect(throughputSeries(samples)).toEqual([0]);
  });
});

describe("etaSeconds", () => {
  it("estimates remaining time from recent throughput", () => {
    const samples: ProgressSample[] = [
      { timestampMs: 0, snapshot: snap(100, 0, 0) },
      { timestampMs: 1000, snapshot: snap(90, 0, 10) }, 
    ];
    expect(etaSeconds(samples)).toBe(9);
  });

  it("is null when nothing remains or throughput is zero", () => {
    expect(
      etaSeconds([
        { timestampMs: 0, snapshot: snap(0, 0, 100) },
        { timestampMs: 1000, snapshot: snap(0, 0, 100) },
      ]),
    ).toBeNull();
    expect(etaSeconds([{ timestampMs: 0, snapshot: snap(10, 0, 0) }])).toBeNull();
  });

  it("estimates from the queue draining even when done is flat (rescan)", () => {
    const samples: ProgressSample[] = [
      { timestampMs: 0, snapshot: snap(100, 1, 5000) },
      { timestampMs: 2000, snapshot: snap(60, 1, 5000) }, 
    ];
    expect(etaSeconds(samples)).toBe(3);
  });

  it("is null while the queue is net-growing (more work enqueued)", () => {
    const samples: ProgressSample[] = [
      { timestampMs: 0, snapshot: snap(10, 1, 0) },
      { timestampMs: 1000, snapshot: snap(50, 1, 0) }, 
    ];
    expect(etaSeconds(samples)).toBeNull();
  });
});

describe("sparklinePath", () => {
  it("draws an ascending series with higher values nearer the top (smaller y)", () => {
    const path = sparklinePath([0, 10], 100, 40);
    expect(path).toBe("M 0,40 L 100,0");
  });

  it("flattens a constant series to the vertical middle", () => {
    expect(sparklinePath([5, 5, 5], 100, 40)).toBe("M 0,20 L 50,20 L 100,20");
  });

  it("returns an empty path for no data", () => {
    expect(sparklinePath([], 100, 40)).toBe("");
  });

  it("draws a single sample as a flat mid-line point", () => {
    expect(sparklinePath([7], 100, 40)).toBe("M 0,20 L 100,20");
  });
});

describe("ProgressHistory (bounded ring buffer — the 60fps render lock)", () => {
  it("retains only the most recent `capacity` samples regardless of input scale", () => {
    const history = new ProgressHistory(120);
    for (let i = 0; i < 10_000; i += 1) {
      history.push({ timestampMs: i * 100, snapshot: simulateProgress(i, 10_000) });
    }
    expect(history.samples()).toHaveLength(120);
    const kept = history.samples();
    expect(kept[0].timestampMs).toBe((10_000 - 120) * 100);
    expect(kept[kept.length - 1].timestampMs).toBe(9_999 * 100);
  });

  it("exposes the latest snapshot and bounded throughput for the graph", () => {
    const history = new ProgressHistory(8);
    for (let i = 0; i < 50; i += 1) {
      history.push({ timestampMs: i * 1000, snapshot: simulateProgress(i, 200) });
    }
    expect(history.latest()).toEqual(history.samples().at(-1)?.snapshot);
    expect(history.throughput().length).toBe(7);
  });

  it("starts empty with a null latest", () => {
    const history = new ProgressHistory(4);
    expect(history.samples()).toEqual([]);
    expect(history.latest()).toBeNull();
    expect(history.throughput()).toEqual([]);
  });
});

describe("simulateProgress (deterministic mock stream)", () => {
  it("is a pure function of (tick, total) — same input, same output", () => {
    expect(simulateProgress(7, 500)).toEqual(simulateProgress(7, 500));
  });

  it("monotonically resolves work and conserves the total", () => {
    const total = 300;
    let prevDone = -1;
    for (let tick = 0; tick <= 40; tick += 1) {
      const s = simulateProgress(tick, total);
      expect(totalTasks(s)).toBe(total);
      expect(s.done).toBeGreaterThanOrEqual(prevDone);
      prevDone = s.done;
    }
  });

  it("converges to a fully-drained queue", () => {
    const s = simulateProgress(10_000, 300);
    expect(s.pending).toBe(0);
    expect(s.running).toBe(0);
    expect(s.done + s.failed).toBe(300);
    expect(isScanning(s)).toBe(false);
  });
});

describe("collapseOnScroll", () => {
  it("collapses once scrolled past the threshold", () => {
    expect(collapseOnScroll(100, false)).toBe(true);
  });

  it("re-expands at the very top", () => {
    expect(collapseOnScroll(0, true)).toBe(false);
  });

  it("stays expanded inside the dead zone near the top", () => {
    expect(collapseOnScroll(10, false)).toBe(false);
  });

  it("does not flap once collapsed for a small upward scroll", () => {
    expect(collapseOnScroll(10, true)).toBe(true);
  });
});

describe("shouldRefreshGroups (live group-list refresh, )", () => {
  it("refreshes when the group count changed and nothing is selected", () => {
    expect(shouldRefreshGroups(3, 5, false, false)).toBe(true);
  });

  it("refreshes while scanning even when the count is unchanged", () => {
    expect(shouldRefreshGroups(5, 5, true, false)).toBe(true);
  });

  it("does not refresh when idle and the count is unchanged", () => {
    expect(shouldRefreshGroups(5, 5, false, false)).toBe(false);
  });

  it("never replaces the list while a cluster is selected (detail open)", () => {
    expect(shouldRefreshGroups(3, 5, true, true)).toBe(false);
    expect(shouldRefreshGroups(5, 5, true, true)).toBe(false);
    expect(shouldRefreshGroups(3, 10, false, true)).toBe(false);
  });
});

describe("refreshLimit (비파괴 재로드 limit 결정, )", () => {
  it("현재 로드된 항목이 pageSize보다 많으면 그 수만큼 요청해 스크롤 위치를 보존한다", () => {
    expect(refreshLimit(30, 90)).toBe(90);
  });

  it("로드된 항목이 pageSize 이하면 pageSize를 반환해 최소 한 페이지를 보장한다", () => {
    expect(refreshLimit(30, 30)).toBe(30);
    expect(refreshLimit(30, 0)).toBe(30);
    expect(refreshLimit(30, 15)).toBe(30);
  });

  it("선택된 클러스터가 있을 때도 limit 계산은 동일하다 (hasSelection 판정은 shouldRefreshGroups 책임)", () => {
    expect(refreshLimit(30, 60)).toBe(60);
  });
});


function drainSample(
  timestampMs: number,
  pendingBytes: number,
  running = 1,
): ProgressSample {
  return {
    timestampMs,
    snapshot: { pending: running, running, done: 0, failed: 0, pendingBytes },
  };
}

const MiB = 1024 * 1024;
const GiB = 1024 * 1024 * 1024;

describe("drainRateSeries (MB/s sparkline data, )", () => {
  it("derives a continuous trailing-window queue-drain rate", () => {
    const samples = [
      drainSample(0, 300 * MiB),
      drainSample(1000, 300 * MiB),
      drainSample(2000, 200 * MiB),
      drainSample(3000, 200 * MiB),
    ];
    const series = drainRateSeries(samples);
    expect(series).toHaveLength(3); 
    expect(series[0]).toBe(0); 
    expect(series[1]).toBeCloseTo(50 * MiB, 0); 
    expect(series[2]).toBeCloseTo((100 * MiB) / 3, 0); 
  });

  it("uses only the trailing `window` samples, ageing out older drains", () => {
    const samples = [
      drainSample(0, 300 * MiB),
      drainSample(1000, 200 * MiB), 
      drainSample(2000, 200 * MiB), 
    ];
    const series = drainRateSeries(samples, 1);
    expect(series[0]).toBeCloseTo(100 * MiB, 0); 
    expect(series[1]).toBe(0); 
  });

  it("clamps a growing queue (fresh enqueue) to 0, not a negative spike", () => {
    const samples = [drainSample(0, 100 * MiB), drainSample(1000, 500 * MiB)];
    expect(drainRateSeries(samples)).toEqual([0]);
  });

  it("is empty for fewer than two samples", () => {
    expect(drainRateSeries([])).toEqual([]);
    expect(drainRateSeries([drainSample(0, MiB)])).toEqual([]);
  });
});

describe("drainBytesPerSec / etaFromDrain (windowed, )", () => {
  it("measures the windowed drain rate (counts files leaving the queue)", () => {
    const samples = [drainSample(0, 100 * MiB), drainSample(10_000, 0, 0)];
    expect(drainBytesPerSec(samples)).toBeCloseTo(10 * MiB, 0);
  });

  it("stays positive through a stall (earlier completions still in window)", () => {
    const samples = [
      drainSample(0, 200 * MiB),
      drainSample(2000, 100 * MiB),
      drainSample(4000, 100 * MiB),
      drainSample(6000, 100 * MiB),
    ];
    expect(drainBytesPerSec(samples)).toBeCloseTo((100 * MiB) / 6, 0);
  });

  it("is null when the queue is not net-draining or only growing", () => {
    expect(drainBytesPerSec([drainSample(0, 100 * MiB)])).toBeNull();
    const growing = [drainSample(0, 100 * MiB), drainSample(1000, 200 * MiB)];
    expect(drainBytesPerSec(growing)).toBeNull();
  });

  it("ETA = remaining bytes ÷ windowed drain rate (large-file aware)", () => {
    const samples = [drainSample(0, 50 * GiB + 100 * MiB), drainSample(10_000, 50 * GiB)];
    const eta = etaFromDrain(samples);
    expect(eta).toBe(Math.round((50 * GiB) / (10 * MiB)));
    expect(eta).toBeGreaterThan(5000);
  });

  it("ETA is null when nothing is outstanding (scan done)", () => {
    const samples = [drainSample(0, 100 * MiB), drainSample(10_000, 0, 0)];
    expect(etaFromDrain(samples)).toBeNull();
  });
});

describe("isDrainStalled (slow/large-file detection, )", () => {
  it("is true when work is running but pending bytes have not moved", () => {
    const flat = Array.from({ length: 8 }, (_, i) => drainSample(i * 800, 90 * GiB, 1));
    expect(isDrainStalled(flat)).toBe(true);
  });

  it("is false while the queue is actively draining", () => {
    const draining = Array.from({ length: 8 }, (_, i) =>
      drainSample(i * 800, (90 - i) * GiB, 1),
    );
    expect(isDrainStalled(draining)).toBe(false);
  });

  it("is false when nothing is running (idle, not stalled)", () => {
    const idle = Array.from({ length: 8 }, (_, i) => drainSample(i * 800, 0, 0));
    expect(isDrainStalled(idle)).toBe(false);
  });

  it("needs enough samples before judging", () => {
    expect(isDrainStalled([drainSample(0, GiB, 1)])).toBe(false);
  });
});

describe("isAnalyzingPartialClips", () => {
  it("is true when partialPending is non-zero", () => {
    expect(
      isAnalyzingPartialClips({ pending: 0, running: 0, done: 10, failed: 0, partialPending: 3, partialRunning: 0 }),
    ).toBe(true);
  });

  it("is true when partialRunning is non-zero", () => {
    expect(
      isAnalyzingPartialClips({ pending: 0, running: 0, done: 10, failed: 0, partialPending: 0, partialRunning: 2 }),
    ).toBe(true);
  });

  it("is true when both partial counts are non-zero", () => {
    expect(
      isAnalyzingPartialClips({ pending: 0, running: 0, done: 10, failed: 0, partialPending: 5, partialRunning: 1 }),
    ).toBe(true);
  });

  it("is false when both partial counts are zero", () => {
    expect(
      isAnalyzingPartialClips({ pending: 0, running: 0, done: 10, failed: 0, partialPending: 0, partialRunning: 0 }),
    ).toBe(false);
  });

  it("is false when partial fields are omitted (pre-v19 daemon / mock default)", () => {
    expect(
      isAnalyzingPartialClips({ pending: 0, running: 0, done: 10, failed: 0 }),
    ).toBe(false);
  });

  it("is true even when foreground indexing is still running (coexistence)", () => {
    expect(
      isAnalyzingPartialClips({ pending: 5, running: 2, done: 50, failed: 0, partialPending: 3, partialRunning: 1 }),
    ).toBe(true);
  });
});

describe("partialOutstanding", () => {
  it("sums pending and running partial tasks", () => {
    expect(
      partialOutstanding({ pending: 0, running: 0, done: 10, failed: 0, partialPending: 5, partialRunning: 2 }),
    ).toBe(7);
  });

  it("counts pending alone", () => {
    expect(
      partialOutstanding({ pending: 0, running: 0, done: 10, failed: 0, partialPending: 3, partialRunning: 0 }),
    ).toBe(3);
  });

  it("counts running alone", () => {
    expect(
      partialOutstanding({ pending: 0, running: 0, done: 10, failed: 0, partialPending: 0, partialRunning: 4 }),
    ).toBe(4);
  });

  it("is 0 when both partial counts are zero", () => {
    expect(
      partialOutstanding({ pending: 0, running: 0, done: 10, failed: 0, partialPending: 0, partialRunning: 0 }),
    ).toBe(0);
  });

  it("is 0 when partial fields are omitted (pre-v19 daemon / mock default)", () => {
    expect(partialOutstanding({ pending: 0, running: 0, done: 10, failed: 0 })).toBe(0);
  });
});

describe("partialCompleted", () => {
  it("is the partialDone count (the N/M numerator)", () => {
    expect(
      partialCompleted({ pending: 0, running: 0, done: 10, failed: 0, partialDone: 4 }),
    ).toBe(4);
  });

  it("is 0 when partialDone is omitted (pre-v22 daemon / mock default)", () => {
    expect(
      partialCompleted({ pending: 0, running: 0, done: 10, failed: 0, partialPending: 2, partialRunning: 1 }),
    ).toBe(0);
  });
});

describe("partialTotal", () => {
  it("sums done + pending + running (the N/M denominator)", () => {
    expect(
      partialTotal({ pending: 0, running: 0, done: 10, failed: 0, partialDone: 3, partialPending: 5, partialRunning: 2 }),
    ).toBe(10);
  });

  it("counts only outstanding work when partialDone is omitted (pre-v22)", () => {
    expect(
      partialTotal({ pending: 0, running: 0, done: 10, failed: 0, partialPending: 5, partialRunning: 2 }),
    ).toBe(7);
  });

  it("counts done alone once outstanding work drains (N === M)", () => {
    expect(
      partialTotal({ pending: 0, running: 0, done: 10, failed: 0, partialDone: 6, partialPending: 0, partialRunning: 0 }),
    ).toBe(6);
  });

  it("is 0 when every partial field is omitted (pre-v22 daemon / mock default)", () => {
    expect(partialTotal({ pending: 0, running: 0, done: 10, failed: 0 })).toBe(0);
  });

  it("can shrink relative to completed across snapshots (dynamic total, honest fraction)", () => {
    const a = partialCompleted({ pending: 0, running: 0, done: 0, failed: 0, partialDone: 2, partialPending: 0, partialRunning: 0 }) /
      partialTotal({ pending: 0, running: 0, done: 0, failed: 0, partialDone: 2, partialPending: 0, partialRunning: 0 });
    const b = partialCompleted({ pending: 0, running: 0, done: 0, failed: 0, partialDone: 2, partialPending: 6, partialRunning: 0 }) /
      partialTotal({ pending: 0, running: 0, done: 0, failed: 0, partialDone: 2, partialPending: 6, partialRunning: 0 });
    expect(a).toBe(1); 
    expect(b).toBeCloseTo(0.25); 
    expect(b).toBeLessThan(a);
  });
});

describe("partialSkippedTotal", () => {
  it("sums the reason→count map across reasons", () => {
    expect(
      partialSkippedTotal({
        pending: 0, running: 0, done: 10, failed: 0,
        partialSkipped: { "unsupported-codec": 2, "duration-cap": 1 },
      }),
    ).toBe(3);
  });

  it("is 0 when partialSkipped is omitted (pre-v24 daemon / mock default)", () => {
    expect(partialSkippedTotal({ pending: 0, running: 0, done: 10, failed: 0 })).toBe(0);
  });

  it("is 0 for an empty map", () => {
    expect(
      partialSkippedTotal({ pending: 0, running: 0, done: 10, failed: 0, partialSkipped: {} }),
    ).toBe(0);
  });
});

describe("partialFailedTotal", () => {
  it("returns the permanent-reindex-failure count when present", () => {
    expect(
      partialFailedTotal({
        pending: 0, running: 0, done: 10, failed: 1,
        partialFailed: 3,
      }),
    ).toBe(3);
  });

  it("is 0 when partialFailed is omitted (pre-v24 daemon / mock default)", () => {
    expect(partialFailedTotal({ pending: 0, running: 0, done: 10, failed: 0 })).toBe(0);
  });
});

describe("partialSkippedBreakdown", () => {
  it("maps reasons to Korean labels, ordered by descending count", () => {
    expect(
      partialSkippedBreakdown({
        pending: 0, running: 0, done: 10, failed: 0,
        partialSkipped: { "duration-cap": 1, "unsupported-codec": 3 },
      }),
    ).toBe("코덱 미지원 3, 길이 초과 1");
  });

  it("falls back to the raw key for an unknown reason (forward-compat)", () => {
    expect(
      partialSkippedBreakdown({
        pending: 0, running: 0, done: 10, failed: 0,
        partialSkipped: { "some-future-reason": 2 },
      }),
    ).toBe("some-future-reason 2");
  });

  it("maps the /marker reasons to Korean labels", () => {
    expect(
      partialSkippedBreakdown({
        pending: 0, running: 0, done: 10, failed: 0,
        partialSkipped: { "exact-full-dup": 2, "retry-exhausted": 1 },
      }),
    ).toBe("완전 중복 2, 재시도 한도 초과 1");
  });

  it("is empty when nothing was skipped", () => {
    expect(
      partialSkippedBreakdown({ pending: 0, running: 0, done: 10, failed: 0, partialSkipped: {} }),
    ).toBe("");
    expect(
      partialSkippedBreakdown({ pending: 0, running: 0, done: 10, failed: 0 }),
    ).toBe("");
  });
});

describe("simulateProgress partialDone", () => {
  it("reports a partialDone that is the resolved count, never exceeding the total", () => {
    const total = 100;
    for (let tick = 0; tick <= 40; tick += 1) {
      const s = simulateProgress(tick, total);
      const done = partialCompleted(s);
      const sum = partialTotal(s);
      expect(done).toBeGreaterThanOrEqual(0);
      expect(done).toBeLessThanOrEqual(sum);
    }
  });

  it("grows the partial done count as the demo advances", () => {
    const total = 100;
    const early = partialCompleted(simulateProgress(20, total));
    const late = partialCompleted(simulateProgress(35, total));
    expect(late).toBeGreaterThan(early);
  });

  it("emits partialDone:0 for an empty queue", () => {
    expect(simulateProgress(5, 0).partialDone).toBe(0);
  });
});

describe("nextActivityMs", () => {
  it("curDone > prevDone이면 nowMs를 반환한다", () => {
    expect(nextActivityMs(5, 6, null, 1000)).toBe(1000);
  });

  it("curDone === prevDone이면 prevActivityMs를 이월한다", () => {
    expect(nextActivityMs(5, 5, 999, 2000)).toBe(999);
  });

  it("curDone < prevDone(soft-delete로 done 하락)이면 prevActivityMs를 이월한다", () => {
    expect(nextActivityMs(10, 8, 777, 3000)).toBe(777);
  });

  it("prevActivityMs=null + done 증가이면 nowMs를 반환한다", () => {
    expect(nextActivityMs(0, 1, null, 5000)).toBe(5000);
  });

  it("prevDone=null(세션 첫 관찰)이면 done이 양수여도 발화하지 않고 prevActivityMs를 이월한다", () => {
    expect(nextActivityMs(null, 50, null, 6000)).toBeNull();
    expect(nextActivityMs(null, 50, 4242, 6000)).toBe(4242);
  });
});

describe("formatRelativeActivity", () => {
  it("lastActivityMs가 null이면 null을 반환한다", () => {
    expect(formatRelativeActivity(null, 9000)).toBeNull();
  });

  it("deltaSec < 5이면 '방금'을 반환한다", () => {
    expect(formatRelativeActivity(1000, 5000)).toBe("방금"); 
  });

  it("30초 전", () => {
    expect(formatRelativeActivity(0, 30_000)).toBe("30초 전");
  });

  it("90초 → '1분 전'", () => {
    expect(formatRelativeActivity(0, 90_000)).toBe("1분 전");
  });

  it("7200초 → '2시간 전'", () => {
    expect(formatRelativeActivity(0, 7_200_000)).toBe("2시간 전");
  });

  it("2일 전", () => {
    expect(formatRelativeActivity(0, 2 * 86_400_000)).toBe("2일 전");
  });
});

describe("currentFileLabel", () => {
  it("returns null when nothing is in flight", () => {
    expect(currentFileLabel([])).toBeNull();
    expect(currentFileLabel(undefined)).toBeNull();
  });

  it("uses the basename of the single in-flight file and keeps the full path as the title", () => {
    const label = currentFileLabel(["C:/library/영화/대용량 영상.mp4"]);
    expect(label).toEqual({
      name: "대용량 영상.mp4",
      title: "C:/library/영화/대용량 영상.mp4",
      extra: 0,
    });
  });

  it("reports the count of the other files when several decode at once", () => {
    const label = currentFileLabel([
      "/lib/a.mp4",
      "/lib/b.mkv",
      "/lib/c.webm",
    ]);
    expect(label?.name).toBe("a.mp4");
    expect(label?.extra).toBe(2);
  });

  it("handles a bare filename, a trailing slash, and backslash paths", () => {
    expect(currentFileLabel(["movie.mp4"])?.name).toBe("movie.mp4");
    expect(currentFileLabel(["/lib/dir/"])?.name).toBe("dir");
    expect(currentFileLabel(["D:\\videos\\클립 😀.mp4"])?.name).toBe(
      "클립 😀.mp4",
    );
  });
});

describe("EtaEstimator (stable 남은 시간, )", () => {

  function etaSample(
    timestampMs: number,
    pendingBytes: number,
    running = 1,
    pending = 0,
  ): ProgressSample {
    return {
      timestampMs,
      snapshot: { pending, running, done: 0, failed: 0, pendingBytes },
    };
  }

  const DT = ACTIVE_POLL_MS; 
  const DT_SEC = DT / 1000;

  const DECAY = Math.exp(-DT_SEC / ETA_RATE_TAU_SEC);


  function warmUp(
    est: EtaEstimator,
    startMiB: number,
    dropMiB: number,
    count: number,
    t0 = 0,
  ): { t: number; pendingMiB: number } {
    let t = t0;
    let pending = startMiB;
    est.push(etaSample(t, pending * MiB));
    for (let i = 0; i < count; i += 1) {
      t += DT;
      pending -= dropMiB;
      est.push(etaSample(t, pending * MiB));
    }
    return { t, pendingMiB: pending };
  }

  it("(a) big-file stall: holds a smooth, rising, never-null ETA through 70 zero-drain polls", () => {
    const est = new EtaEstimator();
    let { t, pendingMiB } = warmUp(est, 8000, 100, 15); 
    const displayed: number[] = [];
    for (let i = 0; i < 70; i += 1) {
      t += DT;
      est.push(etaSample(t, pendingMiB * MiB, 1));
      const d = est.displayEta();
      expect(d).not.toBeNull(); 
      displayed.push(d as number);
    }
    for (let i = 1; i < displayed.length; i += 1) {
      const prev = displayed[i - 1];
      const cur = displayed[i];
      expect(cur).toBeLessThanOrEqual(prev * 2);
      expect(cur).toBeGreaterThanOrEqual(prev * 0.5);
    }
    expect(displayed[displayed.length - 1]).toBeGreaterThan(displayed[0]);
    expect(displayed[displayed.length - 1]).toBeGreaterThan(0);
  });

  it("(b) burst completion: converges over many polls, never an instant collapse", () => {
    const est = new EtaEstimator();
    const { t: tw, pendingMiB } = warmUp(est, 5120, 100, 12); 
    let t = tw + DT;
    est.push(etaSample(t, pendingMiB * MiB, 1)); 
    const before = est.displayEta() as number;
    t += DT;
    est.push(etaSample(t, 200 * MiB, 1)); 
    const afterBurst = est.displayEta() as number;
    expect(afterBurst).toBeGreaterThan(before * 0.5);
    const series = [afterBurst];
    for (let i = 0; i < 25; i += 1) {
      t += DT;
      est.push(etaSample(t, 200 * MiB, 1));
      series.push(est.displayEta() as number);
    }
    for (let i = 1; i < series.length; i += 1) {
      const prev = series[i - 1];
      const cur = series[i];
      expect(Math.abs(cur - prev)).toBeLessThanOrEqual(Math.max(0.35 * prev, DT_SEC + 1.5));
    }
    expect(series[series.length - 1]).toBeLessThan(before * 0.5);
  });

  it("(c) steady mixed drain: adjacent displayed values change smoothly", () => {
    const est = new EtaEstimator();
    const drops = [50, 300, 20, 500, 80, 200, 40, 350, 60, 150, 30, 420, 90, 70, 260, 45, 310, 25, 190, 55];
    let t = 0;
    let pending = 20_000; 
    est.push(etaSample(t, pending * MiB));
    const displayed: number[] = [];
    for (const drop of drops) {
      t += DT;
      pending -= drop;
      est.push(etaSample(t, pending * MiB));
      const d = est.displayEta();
      if (d !== null) displayed.push(d);
    }
    expect(displayed.length).toBeGreaterThan(5);
    for (let i = 1; i < displayed.length; i += 1) {
      const prev = displayed[i - 1];
      const cur = displayed[i];
      expect(Math.abs(cur - prev)).toBeLessThanOrEqual(Math.max(0.3 * prev, DT_SEC + 2));
    }
  });

  it("(d) completion nulls immediately; a reset restarts fresh with no stale rate", () => {
    const est = new EtaEstimator();
    warmUp(est, 3000, 100, 10); 
    expect(est.rate()).not.toBeNull();
    est.push(etaSample(11 * DT, 0, 0, 0));
    expect(est.displayEta()).toBeNull();
    expect(est.rawEta()).toBeNull();

    est.reset();
    expect(est.rate()).toBeNull();
    expect(est.displayEta()).toBeNull();
    est.push(etaSample(0, 4000 * MiB, 1));
    expect(est.displayEta()).toBeNull();
    est.push(etaSample(DT, 3900 * MiB, 1));
    expect(est.rate()).not.toBeNull();
    expect(est.displayEta()).not.toBeNull();
  });

  it("(e) EWMA: seeds on first drain, decays on zero-drain, survives an enqueue", () => {
    const est = new EtaEstimator();
    est.push(etaSample(0, 1000 * MiB, 1));
    expect(est.rate()).toBeNull();
    est.push(etaSample(DT, 900 * MiB, 1)); 
    const seeded = est.rate() as number;
    expect(seeded).toBeCloseTo((100 * MiB) / DT_SEC, 0);

    let t = DT;
    let rate = seeded;
    for (let i = 0; i < 10; i += 1) {
      t += DT;
      est.push(etaSample(t, 900 * MiB, 1)); 
      rate *= DECAY;
      const r = est.rate() as number;
      expect(r).toBeGreaterThan(0);
      expect(r).toBeCloseTo(rate, 0);
    }

    t += DT;
    est.push(etaSample(t, 3000 * MiB, 1)); 
    rate *= DECAY;
    const afterEnqueue = est.rate() as number;
    expect(afterEnqueue).toBeGreaterThan(0);
    expect(afterEnqueue).toBeCloseTo(rate, 0);
  });

  it("holds a transient raw-null for the bounded window, then goes dark", () => {
    const est = new EtaEstimator();
    warmUp(est, 3000, 100, 10); 
    const before = est.displayEta();
    expect(before).not.toBeNull();
    let t = 11 * DT;
    const held: (number | null)[] = [];
    for (let i = 0; i < ETA_NULL_HOLD_POLLS; i += 1) {
      t += DT;
      est.push(etaSample(t, 0, 1, 1)); 
      held.push(est.displayEta());
    }
    expect(held.every((v) => v !== null)).toBe(true);
    for (let i = 1; i < held.length; i += 1) {
      expect(held[i] as number).toBeLessThanOrEqual(held[i - 1] as number);
    }
    t += DT;
    est.push(etaSample(t, 0, 1, 1));
    expect(est.displayEta()).toBeNull();
  });

  it("uses the configured smoothing constants", () => {
    expect(ETA_RATE_TAU_SEC).toBe(120);
    expect(ETA_DISPLAY_ALPHA).toBeCloseTo(0.15);
    expect(ETA_NULL_HOLD_POLLS).toBe(8);
  });
});

describe("EtaEstimator pause handling", () => {

  function etaSample(
    timestampMs: number,
    pendingBytes: number,
    running = 1,
    pending = 0,
  ): ProgressSample {
    return {
      timestampMs,
      snapshot: { pending, running, done: 0, failed: 0, pendingBytes },
    };
  }

  const DT = ACTIVE_POLL_MS;


  function warmUp(
    est: EtaEstimator,
    startMiB: number,
    dropMiB: number,
    count: number,
  ): { t: number; pendingMiB: number } {
    let t = 0;
    let pending = startMiB;
    est.push(etaSample(t, pending * MiB));
    for (let i = 0; i < count; i += 1) {
      t += DT;
      pending -= dropMiB;
      est.push(etaSample(t, pending * MiB));
    }
    return { t, pendingMiB: pending };
  }

  it("증상 재현(회귀 문서화): paused 플래그 없이 push하면 여전히 표시 ETA가 계속 증가한다", () => {
    const est = new EtaEstimator();
    const { t: t0, pendingMiB } = warmUp(est, 8000, 100, 15); 
    const etaAtPauseStart = est.displayEta() as number;
    expect(etaAtPauseStart).not.toBeNull();

    let t = t0;
    const observed: number[] = [];
    for (let i = 0; i < 60; i += 1) {
      t += DT;
      est.push(etaSample(t, pendingMiB * MiB, 1)); 
      observed.push(est.displayEta() as number);
    }

    const finalEta = observed[observed.length - 1];
    expect(finalEta).toBeGreaterThan(etaAtPauseStart);
  });

  it("AC1: paused=true 동안 표시 ETA는 단조 비증가하며(≥5폴), 값이 고정된다", () => {
    const est = new EtaEstimator();
    const { t: t0, pendingMiB } = warmUp(est, 8000, 100, 15);
    const etaAtPauseStart = est.displayEta() as number;
    expect(etaAtPauseStart).not.toBeNull();

    let t = t0;
    const observed: number[] = [];
    for (let i = 0; i < 8; i += 1) {
      t += DT;
      est.push(etaSample(t, pendingMiB * MiB, 1), { paused: true });
      observed.push(est.displayEta() as number);
    }

    expect(observed.length).toBeGreaterThanOrEqual(5);
    for (let i = 1; i < observed.length; i += 1) {
      expect(observed[i]).toBeLessThanOrEqual(observed[i - 1]);
    }
    for (const v of observed) {
      expect(v).toBe(etaAtPauseStart);
    }
  });

  it("AC2: resume 첫 폴의 ETA는 pause 직전 값 대비 25% 이내로 유지된다", () => {
    const est = new EtaEstimator();
    const { t: t0, pendingMiB } = warmUp(est, 8000, 100, 15);
    const etaPrePause = est.displayEta() as number;
    expect(etaPrePause).not.toBeNull();

    let t = t0;
    for (let i = 0; i < 10; i += 1) {
      t += 20_000; 
      est.push(etaSample(t, pendingMiB * MiB, 1), { paused: true });
    }

    t += DT;
    const resumedPending = pendingMiB - 100;
    est.push(etaSample(t, resumedPending * MiB, 1));
    const etaResume = est.displayEta() as number;

    const relDelta = Math.abs(etaResume - etaPrePause) / Math.max(etaPrePause, 1);
    expect(relDelta).toBeLessThanOrEqual(0.25);
  });

  it("AC3: 비pause stall(paused=false, drained=0)의 기존 감쇠 동작은 회귀 없다", () => {
    const est = new EtaEstimator();
    let { t, pendingMiB } = warmUp(est, 8000, 100, 15);
    const displayed: number[] = [];
    for (let i = 0; i < 70; i += 1) {
      t += DT;
      est.push(etaSample(t, pendingMiB * MiB, 1)); 
      const d = est.displayEta();
      expect(d).not.toBeNull();
      displayed.push(d as number);
    }
    for (let i = 1; i < displayed.length; i += 1) {
      const prev = displayed[i - 1];
      const cur = displayed[i];
      expect(cur).toBeLessThanOrEqual(prev * 2);
      expect(cur).toBeGreaterThanOrEqual(prev * 0.5);
    }
    expect(displayed[displayed.length - 1]).toBeGreaterThan(displayed[0]);
  });
});
