

export const FRAME_BUDGET_MS_60FPS = 1000 / 60;


export interface FrameTimingStats {

  frames: number;

  meanMs: number;

  p95Ms: number;

  maxMs: number;

  droppedFrames: number;

  droppedFraction: number;
}


export class FrameTimingBuffer {
  private readonly capacity: number;
  private buffer: number[] = [];

  constructor(capacity: number) {
    this.capacity = Math.max(1, Math.floor(capacity));
  }


  record(durationMs: number): void {
    if (!Number.isFinite(durationMs) || durationMs < 0) return;
    this.buffer.push(durationMs);
    if (this.buffer.length > this.capacity) {
      this.buffer.splice(0, this.buffer.length - this.capacity);
    }
  }


  samples(): number[] {
    return [...this.buffer];
  }


  clear(): void {
    this.buffer = [];
  }


  stats(budgetMs: number = FRAME_BUDGET_MS_60FPS): FrameTimingStats {
    const frames = this.buffer.length;
    if (frames === 0) {
      return {
        frames: 0,
        meanMs: 0,
        p95Ms: 0,
        maxMs: 0,
        droppedFrames: 0,
        droppedFraction: 0,
      };
    }

    let sum = 0;
    let max = 0;
    let dropped = 0;
    for (const d of this.buffer) {
      sum += d;
      if (d > max) max = d;
      if (d > budgetMs) dropped += 1;
    }

    return {
      frames,
      meanMs: sum / frames,
      p95Ms: percentile(this.buffer, 0.95),
      maxMs: max,
      droppedFrames: dropped,
      droppedFraction: dropped / frames,
    };
  }
}


export function percentile(values: readonly number[], q: number): number {
  if (values.length === 0) return 0;
  const sorted = [...values].sort((a, b) => a - b);
  const clamped = Math.min(1, Math.max(0, q));
  const rank = Math.ceil(clamped * sorted.length);
  const index = Math.min(sorted.length - 1, Math.max(0, rank - 1));
  return sorted[index];
}


export interface FrameClock {

  requestAnimationFrame: (cb: (timestampMs: number) => void) => number;

  cancelAnimationFrame: (handle: number) => void;
}


export function browserFrameClock(): FrameClock | null {
  if (typeof globalThis.requestAnimationFrame !== "function") return null;
  return {
    requestAnimationFrame: globalThis.requestAnimationFrame.bind(globalThis),
    cancelAnimationFrame: globalThis.cancelAnimationFrame.bind(globalThis),
  };
}


export function sampleFrames(
  buffer: FrameTimingBuffer,
  clock: FrameClock | null = browserFrameClock(),
): () => void {
  if (clock === null) {
    return () => {};
  }

  let handle: number | null = null;
  let previous: number | null = null;
  let stopped = false;

  const onFrame = (timestampMs: number): void => {
    if (stopped) return;
    if (previous !== null) {
      buffer.record(timestampMs - previous);
    }
    previous = timestampMs;
    handle = clock.requestAnimationFrame(onFrame);
  };

  handle = clock.requestAnimationFrame(onFrame);

  return () => {
    stopped = true;
    if (handle !== null) {
      clock.cancelAnimationFrame(handle);
      handle = null;
    }
  };
}
