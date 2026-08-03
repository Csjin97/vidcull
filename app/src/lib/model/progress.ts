
import type { ProgressSnapshot } from "./types";


export interface ProgressSample {

  timestampMs: number;

  snapshot: ProgressSnapshot;
}


export function totalTasks(s: ProgressSnapshot): number {
  return s.pending + s.running + s.done + s.failed;
}


export function completedFraction(s: ProgressSnapshot): number {
  const total = totalTasks(s);
  return total === 0 ? 0 : (s.done + s.failed) / total;
}


export function isScanning(s: ProgressSnapshot): boolean {
  return s.pending + s.running > 0;
}


export function isAnalyzingPartialClips(s: ProgressSnapshot): boolean {
  return ((s.partialPending ?? 0) + (s.partialRunning ?? 0)) > 0;
}


export function isFolderScanning(s: ProgressSnapshot): boolean {
  return s.folderScanning ?? false;
}


export function partialOutstanding(s: ProgressSnapshot): number {
  return (s.partialPending ?? 0) + (s.partialRunning ?? 0);
}


export function partialCompleted(s: ProgressSnapshot): number {
  return s.partialDone ?? 0;
}


export function partialTotal(s: ProgressSnapshot): number {
  return (s.partialDone ?? 0) + (s.partialPending ?? 0) + (s.partialRunning ?? 0);
}


export const PARTIAL_SKIP_REASON_LABELS: Record<string, string> = {
  "unsupported-codec": "코덱 미지원",
  "decode-failed": "디코드 실패",
  "duration-cap": "길이 초과",
  unprobeable: "프로브 실패",
  "no-scenes": "장면 없음",
  "exact-full-dup": "완전 중복",
  "retry-exhausted": "재시도 한도 초과",
};


export function partialSkippedTotal(s: ProgressSnapshot): number {
  const m = s.partialSkipped;
  if (!m) return 0;
  let total = 0;
  for (const n of Object.values(m)) total += n;
  return total;
}


export function partialSkippedBreakdown(s: ProgressSnapshot): string {
  const m = s.partialSkipped;
  if (!m) return "";
  return Object.entries(m)
    .filter(([, n]) => n > 0)
    .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
    .map(([reason, n]) => `${PARTIAL_SKIP_REASON_LABELS[reason] ?? reason} ${n}`)
    .join(", ");
}


export function partialFailedTotal(s: ProgressSnapshot): number {
  return s.partialFailed ?? 0;
}


export interface CurrentFileLabel {

  name: string;

  title: string;

  extra: number;
}


export function currentFileLabel(
  paths: readonly string[] | undefined,
): CurrentFileLabel | null {
  if (!paths || paths.length === 0) return null;
  const first = paths[0];
  return { name: basename(first), title: first, extra: paths.length - 1 };
}


function basename(path: string): string {
  const trimmed = path.replace(/[/\\]+$/, "");
  const cut = Math.max(trimmed.lastIndexOf("/"), trimmed.lastIndexOf("\\"));
  return cut >= 0 ? trimmed.slice(cut + 1) : trimmed;
}


export function remaining(s: ProgressSnapshot): number {
  return s.pending + s.running;
}


export function throughputSeries(samples: readonly ProgressSample[]): number[] {
  const series: number[] = [];
  for (let i = 1; i < samples.length; i += 1) {
    const prev = samples[i - 1];
    const cur = samples[i];
    const dt = (cur.timestampMs - prev.timestampMs) / 1000;
    const drained = remaining(prev.snapshot) - remaining(cur.snapshot);
    series.push(dt > 0 && drained > 0 ? drained / dt : 0);
  }
  return series;
}


export function drainRateSeries(
  samples: readonly ProgressSample[],
  window = 8,
): number[] {
  const w = Math.max(1, Math.floor(window));
  const series: number[] = [];
  for (let i = 1; i < samples.length; i += 1) {
    const start = Math.max(0, i - w);
    const dt = (samples[i].timestampMs - samples[start].timestampMs) / 1000;
    const drained =
      (samples[start].snapshot.pendingBytes ?? 0) -
      (samples[i].snapshot.pendingBytes ?? 0);
    series.push(dt > 0 && drained > 0 ? drained / dt : 0);
  }
  return series;
}


export function etaSeconds(samples: readonly ProgressSample[]): number | null {
  if (samples.length < 2) return null;
  const first = samples[0];
  const last = samples[samples.length - 1];
  const remainingLast = remaining(last.snapshot);
  if (remainingLast <= 0) return null; 
  const dtSec = (last.timestampMs - first.timestampMs) / 1000;
  const drained = remaining(first.snapshot) - remainingLast;
  if (dtSec <= 0 || drained <= 0) return null; 
  const rate = drained / dtSec;
  return Math.round(remainingLast / rate);
}


export function drainBytesPerSec(
  samples: readonly ProgressSample[],
): number | null {
  if (samples.length < 2) return null;
  const first = samples[0];
  const last = samples[samples.length - 1];
  const dtSec = (last.timestampMs - first.timestampMs) / 1000;
  const drained =
    (first.snapshot.pendingBytes ?? 0) - (last.snapshot.pendingBytes ?? 0);
  if (dtSec <= 0 || drained <= 0) return null;
  return drained / dtSec;
}


export function etaFromDrain(samples: readonly ProgressSample[]): number | null {
  const last = samples[samples.length - 1];
  const pendingBytes = last?.snapshot.pendingBytes ?? 0;
  if (pendingBytes <= 0) return null;
  const rate = drainBytesPerSec(samples);
  if (rate === null || rate <= 0) return null;
  return Math.round(pendingBytes / rate);
}


export const ETA_RATE_TAU_SEC = 120;


export const ETA_DISPLAY_ALPHA = 0.15;


export const ETA_NULL_HOLD_POLLS = 8;


export class EtaEstimator {

  private ewmaRate: number | null = null;

  private prevTimestampMs: number | null = null;

  private prevPendingBytes: number | null = null;

  private latestPendingBytes = 0;

  private displayValue: number | null = null;

  private nullHolds = 0;


  push(sample: ProgressSample, opts?: { paused?: boolean }): void {
    const t = sample.timestampMs;
    const pending = sample.snapshot.pendingBytes ?? 0;
    const rem = remaining(sample.snapshot);
    const dt =
      this.prevTimestampMs === null
        ? 0
        : Math.max(0, (t - this.prevTimestampMs) / 1000);

    if (opts?.paused) {
      this.prevTimestampMs = t;
      this.prevPendingBytes = pending;
      this.latestPendingBytes = pending;
      return;
    }

    if (this.prevTimestampMs !== null && dt > 0 && this.prevPendingBytes !== null) {
      const drained = Math.max(0, this.prevPendingBytes - pending);
      const instRate = drained / dt;
      if (this.ewmaRate === null) {
        if (drained > 0) this.ewmaRate = instRate;
      } else {
        const alpha = 1 - Math.exp(-dt / ETA_RATE_TAU_SEC);
        this.ewmaRate += alpha * (instRate - this.ewmaRate);
      }
    }

    this.prevTimestampMs = t;
    this.prevPendingBytes = pending;
    this.latestPendingBytes = pending;

    const raw = this.rawEta();
    if (rem <= 0) {
      this.displayValue = null;
      this.nullHolds = 0;
      return;
    }
    if (raw === null) {
      if (this.displayValue !== null && this.nullHolds < ETA_NULL_HOLD_POLLS) {
        this.nullHolds += 1;
        this.displayValue = Math.max(0, this.displayValue - dt);
      } else {
        this.displayValue = null;
        this.nullHolds = 0;
      }
      return;
    }
    this.nullHolds = 0;
    if (this.displayValue === null) {
      this.displayValue = raw; 
    } else {
      const expected = this.displayValue - dt; 
      this.displayValue = Math.max(0, expected + ETA_DISPLAY_ALPHA * (raw - expected));
    }
  }


  rate(): number | null {
    return this.ewmaRate;
  }


  rawEta(): number | null {
    if (this.latestPendingBytes <= 0) return null;
    if (this.ewmaRate === null || this.ewmaRate <= 0) return null;
    return this.latestPendingBytes / this.ewmaRate;
  }


  displayEta(): number | null {
    return this.displayValue === null ? null : Math.round(this.displayValue);
  }


  reset(): void {
    this.ewmaRate = null;
    this.prevTimestampMs = null;
    this.prevPendingBytes = null;
    this.latestPendingBytes = 0;
    this.displayValue = null;
    this.nullHolds = 0;
  }
}


export function isDrainStalled(
  samples: readonly ProgressSample[],
  lookback = 6,
): boolean {
  if (samples.length < lookback + 1) return false;
  const window = samples.slice(samples.length - (lookback + 1));
  const last = window[window.length - 1].snapshot;
  if (last.running <= 0) return false; 
  const newest = last.pendingBytes ?? 0;
  return window.every((s) => (s.snapshot.pendingBytes ?? 0) === newest);
}

function round2(n: number): number {
  return Math.round(n * 100) / 100;
}


export function sparklinePath(
  values: readonly number[],
  width: number,
  height: number,
): string {
  if (values.length === 0) return "";

  const mid = round2(height / 2);
  if (values.length === 1) {
    return `M 0,${mid} L ${round2(width)},${mid}`;
  }

  let min = values[0];
  let max = values[0];
  for (const v of values) {
    if (v < min) min = v;
    if (v > max) max = v;
  }
  const span = max - min;
  const lastIndex = values.length - 1;

  const points = values.map((v, i) => {
    const x = round2((i / lastIndex) * width);
    const y = span === 0 ? mid : round2(height - ((v - min) / span) * height);
    return `${x},${y}`;
  });

  return `M ${points[0]} ${points
    .slice(1)
    .map((p) => `L ${p}`)
    .join(" ")}`;
}


export class ProgressHistory {
  private readonly capacity: number;
  private buffer: ProgressSample[] = [];

  constructor(capacity: number) {
    this.capacity = Math.max(1, Math.floor(capacity));
  }


  push(sample: ProgressSample): void {
    this.buffer.push(sample);
    if (this.buffer.length > this.capacity) {
      this.buffer.splice(0, this.buffer.length - this.capacity);
    }
  }


  samples(): ProgressSample[] {
    return [...this.buffer];
  }


  latest(): ProgressSnapshot | null {
    return this.buffer.length === 0
      ? null
      : this.buffer[this.buffer.length - 1].snapshot;
  }

  clear(): void {
    this.buffer = [];
  }


  throughput(): number[] {
    return throughputSeries(this.buffer);
  }


  bytesThroughput(): number[] {
    return drainRateSeries(this.buffer);
  }
}


const SIMULATION_TICKS = 30;

const SIMULATION_WORKERS = 4;

const SIMULATION_FAIL_RATE = 0.02;


export function simulateProgress(tick: number, total: number): ProgressSnapshot {
  if (total <= 0) {
    return {
      pending: 0,
      running: 0,
      done: 0,
      failed: 0,
      cpuUsagePermille: 0,
      rssBytes: 0,
      throughputBytesPerSec: 0,
      pendingBytes: 0,
      currentFiles: [],
      partialPending: 0,
      partialRunning: 0,
      partialDone: 0,
      partialSkipped: {},
      partialFailed: 0,
      folderScanning: false,
      scanDiscovered: 0,
    };
  }

  const rate = total / SIMULATION_TICKS;
  const resolved = Math.min(total, Math.floor(Math.max(0, tick) * rate));
  const failed = Math.floor(resolved * SIMULATION_FAIL_RATE);
  const done = resolved - failed;

  const remaining = total - resolved;
  const running = remaining > 0 ? Math.min(remaining, SIMULATION_WORKERS) : 0;
  const pending = remaining - running;

  const active = running > 0;
  const AVG_FILE_BYTES = 700 * 1024 * 1024;

  const PARTIAL_TOTAL = Math.ceil(total * 0.3);
  const partialStartTick = Math.floor(SIMULATION_TICKS * 0.6);
  const partialRate = PARTIAL_TOTAL / (SIMULATION_TICKS * 0.5);
  const partialElapsed = Math.max(0, tick - partialStartTick);
  const partialResolved = Math.min(
    PARTIAL_TOTAL,
    Math.floor(partialElapsed * partialRate),
  );
  const partialRemaining = PARTIAL_TOTAL - partialResolved;
  const partialRunning = partialRemaining > 0 ? Math.min(partialRemaining, 2) : 0;
  const partialPending = partialRemaining - partialRunning;

  return {
    pending,
    running,
    done,
    failed,
    cpuUsagePermille: active ? Math.min(1000, running * 450) : 0,
    rssBytes: active ? 180 * 1024 * 1024 : 90 * 1024 * 1024,
    throughputBytesPerSec: active ? running * 12 * 1024 * 1024 : 0,
    pendingBytes: (pending + running) * AVG_FILE_BYTES,
    currentFiles: Array.from(
      { length: running },
      (_, i) => `/library/videos/clip_${resolved + i + 1}.mp4`,
    ),
    partialPending,
    partialRunning,
    partialDone: partialResolved,
    partialSkipped:
      partialResolved > 0 ? { "unsupported-codec": Math.min(2, PARTIAL_TOTAL) } : {},
    partialFailed: resolved > 0 ? 1 : 0,
    folderScanning: false,
    scanDiscovered: 0,
  };
}


export const ACTIVE_POLL_MS = 800;

export const IDLE_POLL_MS = 5000;


export function pollIntervalMs(scanning: boolean): number {
  return scanning ? ACTIVE_POLL_MS : IDLE_POLL_MS;
}


export const ERROR_BACKOFF_MAX_MS = 30_000;

export const ERROR_BACKOFF_BASE_MS = 2_000;


export function errorBackoffMs(consecutiveErrors: number): number {
  if (consecutiveErrors <= 0) return ERROR_BACKOFF_BASE_MS;
  return Math.min(
    ERROR_BACKOFF_BASE_MS * 2 ** (consecutiveErrors - 1),
    ERROR_BACKOFF_MAX_MS,
  );
}


export function refreshLimit(pageSize: number, currentLength: number): number {
  return Math.max(pageSize, currentLength);
}


export function shouldRefreshGroups(
  prevTotal: number,
  newTotal: number,
  scanning: boolean,
  hasSelection: boolean,
): boolean {
  if (hasSelection) return false;
  return newTotal !== prevTotal || scanning;
}


export function collapseOnScroll(
  scrollTop: number,
  collapsed: boolean,
): boolean {
  const COLLAPSE_AT = 24;
  const EXPAND_AT = 4;
  if (!collapsed && scrollTop > COLLAPSE_AT) return true;
  if (collapsed && scrollTop <= EXPAND_AT) return false;
  return collapsed;
}


export function nextActivityMs(
  prevDone: number | null,
  curDone: number,
  prevActivityMs: number | null,
  nowMs: number,
): number | null {
  if (prevDone === null) return prevActivityMs; 
  return curDone > prevDone ? nowMs : prevActivityMs;
}


export function formatRelativeActivity(
  lastActivityMs: number | null,
  nowMs: number,
): string | null {
  if (lastActivityMs === null) return null;
  const deltaSec = Math.max(0, Math.floor((nowMs - lastActivityMs) / 1000));
  if (deltaSec < 5) return "방금";
  if (deltaSec < 60) return `${deltaSec}초 전`;
  if (deltaSec < 3600) return `${Math.floor(deltaSec / 60)}분 전`;
  if (deltaSec < 86400) return `${Math.floor(deltaSec / 3600)}시간 전`;
  return `${Math.floor(deltaSec / 86400)}일 전`;
}
/**
 * @file progress.ts
 * @brief 인덱싱 진행률과 ETA 계산
 *
 * [변경 이력 (Changelog)]
 * - 2026-08-03 : UI 초기화 시 진행 이력 제거 기능 추가
 */
