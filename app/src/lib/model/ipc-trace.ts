

export interface IpcTraceSample {

  cmd: string;

  durationMs: number;
}


export interface IpcTraceCmdStats {
  cmd: string;
  count: number;
  meanMs: number;
  maxMs: number;
}


export class IpcTraceBuffer {
  private readonly capacity: number;
  private buffer: IpcTraceSample[] = [];

  constructor(capacity: number) {
    this.capacity = Math.max(1, Math.floor(capacity));
  }


  record(cmd: string, durationMs: number): void {
    if (!Number.isFinite(durationMs) || durationMs < 0) return;
    this.buffer.push({ cmd, durationMs });
    if (this.buffer.length > this.capacity) {
      this.buffer.splice(0, this.buffer.length - this.capacity);
    }
  }


  samples(): IpcTraceSample[] {
    return [...this.buffer];
  }


  clear(): void {
    this.buffer = [];
  }


  statsByCmd(): IpcTraceCmdStats[] {
    const byCmd = new Map<string, { total: number; count: number; max: number }>();
    for (const s of this.buffer) {
      const entry = byCmd.get(s.cmd) ?? { total: 0, count: 0, max: 0 };
      entry.total += s.durationMs;
      entry.count += 1;
      entry.max = Math.max(entry.max, s.durationMs);
      byCmd.set(s.cmd, entry);
    }
    return [...byCmd.entries()]
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([cmd, { total, count, max }]) => ({
        cmd,
        count,
        meanMs: total / count,
        maxMs: max,
      }));
  }


  summary(): string {
    return this.statsByCmd()
      .map(
        ({ cmd, count, meanMs, maxMs }) =>
          `${cmd}=${meanMs.toFixed(1)}ms(n=${count},max=${maxMs.toFixed(1)})`,
      )
      .join(" ");
  }
}
