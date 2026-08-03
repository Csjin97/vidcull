

export class IpcValidationError extends Error {
  constructor(
    public readonly command: string,
    public readonly field: string,
    public readonly received: unknown,
  ) {
    super(
      `IPC '${command}' 응답 검증 실패: 필드 '${field}' 가 예상과 다릅니다. ` +
        `(수신값: ${JSON.stringify(received)})`,
    );
    this.name = "IpcValidationError";
  }
}


function expectNumber(cmd: string, field: string, v: unknown): number {
  if (typeof v !== "number") throw new IpcValidationError(cmd, field, v);
  return v;
}

function expectNumberOptional(
  cmd: string,
  field: string,
  v: unknown,
): number | undefined {
  if (v === undefined) return undefined;
  if (typeof v !== "number") throw new IpcValidationError(cmd, field, v);
  return v;
}

function expectNumberOrNull(
  cmd: string,
  field: string,
  v: unknown,
): number | null {
  if (v !== null && typeof v !== "number")
    throw new IpcValidationError(cmd, field, v);
  return v as number | null;
}

function expectNumberOrNullOptional(
  cmd: string,
  field: string,
  v: unknown,
): number | null | undefined {
  if (v === undefined) return undefined;
  return expectNumberOrNull(cmd, field, v);
}

function expectBoolean(cmd: string, field: string, v: unknown): boolean {
  if (typeof v !== "boolean") throw new IpcValidationError(cmd, field, v);
  return v;
}

function expectBooleanOptional(
  cmd: string,
  field: string,
  v: unknown,
): boolean | undefined {
  if (v === undefined) return undefined;
  if (typeof v !== "boolean") throw new IpcValidationError(cmd, field, v);
  return v;
}

function expectString(cmd: string, field: string, v: unknown): string {
  if (typeof v !== "string") throw new IpcValidationError(cmd, field, v);
  return v;
}

function expectStringOrNull(
  cmd: string,
  field: string,
  v: unknown,
): string | null {
  if (v !== null && typeof v !== "string")
    throw new IpcValidationError(cmd, field, v);
  return v as string | null;
}

function expectArray(cmd: string, field: string, v: unknown): unknown[] {
  if (!Array.isArray(v)) throw new IpcValidationError(cmd, field, v);
  return v;
}

function expectStringArray(
  cmd: string,
  field: string,
  v: unknown,
): string[] {
  const arr = expectArray(cmd, field, v);
  arr.forEach((item, i) => {
    if (typeof item !== "string")
      throw new IpcValidationError(cmd, `${field}[${i}]`, item);
  });
  return arr as string[];
}

function expectObject(
  cmd: string,
  field: string,
  v: unknown,
): Record<string, unknown> {
  if (typeof v !== "object" || v === null || Array.isArray(v))
    throw new IpcValidationError(cmd, field, v);
  return v as Record<string, unknown>;
}

function expectNumberRecordOptional(
  cmd: string,
  field: string,
  v: unknown,
): Record<string, number> | undefined {
  if (v === undefined) return undefined;
  const o = expectObject(cmd, field, v);
  const out: Record<string, number> = {};
  for (const [key, value] of Object.entries(o)) {
    out[key] = expectNumber(cmd, `${field}.${key}`, value);
  }
  return out;
}



export interface WireProgressSnapshot {
  pending: number;
  running: number;
  done: number;
  failed: number;
  cpu_usage_permille?: number;
  rss_bytes?: number;
  throughput_bytes_per_sec?: number;
  pending_bytes?: number;
  current_files?: string[];
  partial_pending?: number;
  partial_running?: number;
  partial_done?: number;
  partial_skipped?: Record<string, number>;
  partial_failed?: number;
  folder_scanning?: boolean;
  scan_discovered?: number;
  groups_revision?: number;
}

export function validateProgressSnapshot(
  cmd: string,
  raw: unknown,
): WireProgressSnapshot {
  const o = expectObject(cmd, "(root)", raw);
  return {
    pending: expectNumber(cmd, "pending", o.pending),
    running: expectNumber(cmd, "running", o.running),
    done: expectNumber(cmd, "done", o.done),
    failed: expectNumber(cmd, "failed", o.failed),
    cpu_usage_permille: expectNumberOptional(
      cmd,
      "cpu_usage_permille",
      o.cpu_usage_permille,
    ),
    rss_bytes: expectNumberOptional(cmd, "rss_bytes", o.rss_bytes),
    throughput_bytes_per_sec: expectNumberOptional(
      cmd,
      "throughput_bytes_per_sec",
      o.throughput_bytes_per_sec,
    ),
    pending_bytes: expectNumberOptional(cmd, "pending_bytes", o.pending_bytes),
    current_files:
      o.current_files === undefined
        ? undefined
        : expectStringArray(cmd, "current_files", o.current_files),
    partial_pending: expectNumberOptional(
      cmd,
      "partial_pending",
      o.partial_pending,
    ),
    partial_running: expectNumberOptional(
      cmd,
      "partial_running",
      o.partial_running,
    ),
    partial_done: expectNumberOptional(cmd, "partial_done", o.partial_done),
    partial_skipped: expectNumberRecordOptional(
      cmd,
      "partial_skipped",
      o.partial_skipped,
    ),
    partial_failed: expectNumberOptional(cmd, "partial_failed", o.partial_failed),
    folder_scanning: expectBooleanOptional(
      cmd,
      "folder_scanning",
      o.folder_scanning,
    ),
    scan_discovered: expectNumberOptional(
      cmd,
      "scan_discovered",
      o.scan_discovered,
    ),
    groups_revision: expectNumberOptional(
      cmd,
      "groups_revision",
      o.groups_revision,
    ),
  };
}

export interface WireGroupStats {
  group_count: number;
  reclaimable_bytes: number;
}

export function validateGroupStats(cmd: string, raw: unknown): WireGroupStats {
  const o = expectObject(cmd, "(root)", raw);
  return {
    group_count: expectNumber(cmd, "group_count", o.group_count),
    reclaimable_bytes: expectNumber(
      cmd,
      "reclaimable_bytes",
      o.reclaimable_bytes,
    ),
  };
}

export interface WireClusterStats {
  cluster_count: number;
  reclaimable_bytes: number;
}

export function validateClusterStats(
  cmd: string,
  raw: unknown,
): WireClusterStats {
  const o = expectObject(cmd, "(root)", raw);
  return {
    cluster_count: expectNumber(cmd, "cluster_count", o.cluster_count),
    reclaimable_bytes: expectNumber(
      cmd,
      "reclaimable_bytes",
      o.reclaimable_bytes,
    ),
  };
}


export function validateThumbnail(cmd: string, raw: unknown): string | null {
  return expectStringOrNull(cmd, "(root)", raw);
}

export interface WireGroupSummary {
  group_id: number;
  trust: string;
  best_file_id: number | null;
  member_count: number;
  intro_outro: boolean;
}

export function validateGroupSummaryArray(
  cmd: string,
  raw: unknown,
): WireGroupSummary[] {
  const arr = expectArray(cmd, "(root)", raw);
  return arr.map((item, i) => {
    const o = expectObject(cmd, `[${i}]`, item);
    return {
      group_id: expectNumber(cmd, `[${i}].group_id`, o.group_id),
      trust: expectString(cmd, `[${i}].trust`, o.trust),
      best_file_id: expectNumberOrNull(
        cmd,
        `[${i}].best_file_id`,
        o.best_file_id,
      ),
      member_count: expectNumber(cmd, `[${i}].member_count`, o.member_count),
      intro_outro: expectBoolean(cmd, `[${i}].intro_outro`, o.intro_outro),
    };
  });
}

export interface WireFileDetail {
  file_id: number;
  path: string;
  size_bytes: number;
  width: number | null;
  height: number | null;
  duration_ms: number | null;
  bitrate_bps: number | null;
  codec: string | null;
  container: string | null;
  is_best: boolean;
  thumbnail: string | null;
}

export function validateFileDetailArray(
  cmd: string,
  raw: unknown,
): WireFileDetail[] {
  const arr = expectArray(cmd, "(root)", raw);
  return arr.map((item, i) => {
    const o = expectObject(cmd, `[${i}]`, item);
    return {
      file_id: expectNumber(cmd, `[${i}].file_id`, o.file_id),
      path: expectString(cmd, `[${i}].path`, o.path),
      size_bytes: expectNumber(cmd, `[${i}].size_bytes`, o.size_bytes),
      width: expectNumberOrNull(cmd, `[${i}].width`, o.width),
      height: expectNumberOrNull(cmd, `[${i}].height`, o.height),
      duration_ms: expectNumberOrNull(cmd, `[${i}].duration_ms`, o.duration_ms),
      bitrate_bps: expectNumberOrNull(
        cmd,
        `[${i}].bitrate_bps`,
        o.bitrate_bps,
      ),
      codec: expectStringOrNull(cmd, `[${i}].codec`, o.codec),
      container: expectStringOrNull(cmd, `[${i}].container`, o.container),
      is_best: expectBoolean(cmd, `[${i}].is_best`, o.is_best),
      thumbnail: expectStringOrNull(cmd, `[${i}].thumbnail`, o.thumbnail),
    };
  });
}

export interface WireClusterSummary {
  cluster_id: number;
  representative_trust: string;
  best_file_id: number | null;
  member_count: number;
  member_trust_levels: string[];
  intro_outro: boolean;
  members: WireClusterMemberDetail[];
}

export function validateClusterSummaryArray(
  cmd: string,
  raw: unknown,
): WireClusterSummary[] {
  const arr = expectArray(cmd, "(root)", raw);
  return arr.map((item, i) => {
    const o = expectObject(cmd, `[${i}]`, item);
    return {
      cluster_id: expectNumber(cmd, `[${i}].cluster_id`, o.cluster_id),
      representative_trust: expectString(
        cmd,
        `[${i}].representative_trust`,
        o.representative_trust,
      ),
      best_file_id: expectNumberOrNull(
        cmd,
        `[${i}].best_file_id`,
        o.best_file_id,
      ),
      member_count: expectNumber(cmd, `[${i}].member_count`, o.member_count),
      member_trust_levels: expectStringArray(
        cmd,
        `[${i}].member_trust_levels`,
        o.member_trust_levels,
      ),
      intro_outro: expectBoolean(cmd, `[${i}].intro_outro`, o.intro_outro),
      members: validateClusterMemberDetailArray(
        cmd,
        expectArray(cmd, `[${i}].members`, o.members),
      ),
    };
  });
}

export interface WireClusterMemberDetail {
  file: WireFileDetail;
  trust: string;
  group_id: number;
}

export function validateClusterMemberDetailArray(
  cmd: string,
  raw: unknown,
): WireClusterMemberDetail[] {
  const arr = expectArray(cmd, "(root)", raw);
  return arr.map((item, i) => {
    const o = expectObject(cmd, `[${i}]`, item);
    const fileRaw = expectObject(cmd, `[${i}].file`, o.file);
    return {
      file: validateFileDetailArray(cmd, [fileRaw])[0],
      trust: expectString(cmd, `[${i}].trust`, o.trust),
      group_id: expectNumber(cmd, `[${i}].group_id`, o.group_id),
    };
  });
}

export interface WireDeleteResult {
  ok: boolean;
  removed_file_ids: number[];
  reclaimed_bytes: number;
  detail: string;

  reject_code: string | null;
}

export function validateDeleteResult(
  cmd: string,
  raw: unknown,
): WireDeleteResult {
  const o = expectObject(cmd, "(root)", raw);
  const ids = expectArray(cmd, "removed_file_ids", o.removed_file_ids);
  ids.forEach((id, i) => expectNumber(cmd, `removed_file_ids[${i}]`, id));
  return {
    ok: expectBoolean(cmd, "ok", o.ok),
    removed_file_ids: ids as number[],
    reclaimed_bytes: expectNumber(cmd, "reclaimed_bytes", o.reclaimed_bytes),
    detail: expectString(cmd, "detail", o.detail),
    reject_code:
      o.reject_code === undefined
        ? null
        : expectStringOrNull(cmd, "reject_code", o.reject_code),
  };
}

export interface WireUndoResult {
  ok: boolean;
  group_id: number | null;
  restored_file_ids: number[];
  missing_paths: string[];
  detail: string;
}

export function validateUndoResult(cmd: string, raw: unknown): WireUndoResult {
  const o = expectObject(cmd, "(root)", raw);
  const restoredIds = expectArray(
    cmd,
    "restored_file_ids",
    o.restored_file_ids,
  );
  restoredIds.forEach((id, i) =>
    expectNumber(cmd, `restored_file_ids[${i}]`, id),
  );
  return {
    ok: expectBoolean(cmd, "ok", o.ok),
    group_id: expectNumberOrNull(cmd, "group_id", o.group_id),
    restored_file_ids: restoredIds as number[],
    missing_paths: expectStringArray(cmd, "missing_paths", o.missing_paths),
    detail: expectString(cmd, "detail", o.detail),
  };
}

export interface WireClipOverlap {
  clip_file_id: number;
  source_file_id: number;
  matched_scenes: number;
  clip_scenes: number;
  start_ms: number;
  end_ms: number;
  clip_start_ms: number;
  clip_end_ms: number;
  intro_outro: boolean;
}

export function validateClipOverlapArray(
  cmd: string,
  raw: unknown,
): WireClipOverlap[] {
  const arr = expectArray(cmd, "(root)", raw);
  return arr.map((item, i) => {
    const o = expectObject(cmd, `[${i}]`, item);
    return {
      clip_file_id: expectNumber(cmd, `[${i}].clip_file_id`, o.clip_file_id),
      source_file_id: expectNumber(
        cmd,
        `[${i}].source_file_id`,
        o.source_file_id,
      ),
      matched_scenes: expectNumber(
        cmd,
        `[${i}].matched_scenes`,
        o.matched_scenes,
      ),
      clip_scenes: expectNumber(cmd, `[${i}].clip_scenes`, o.clip_scenes),
      start_ms: expectNumber(cmd, `[${i}].start_ms`, o.start_ms),
      end_ms: expectNumber(cmd, `[${i}].end_ms`, o.end_ms),
      clip_start_ms: expectNumber(cmd, `[${i}].clip_start_ms`, o.clip_start_ms),
      clip_end_ms: expectNumber(cmd, `[${i}].clip_end_ms`, o.clip_end_ms),
      intro_outro: expectBoolean(cmd, `[${i}].intro_outro`, o.intro_outro),
    };
  });
}

export interface WireFailedTask {
  task_id: number;
  path: string;
  reason: string;
  attempts: number;
}

export function validateFailedTaskArray(
  cmd: string,
  raw: unknown,
): WireFailedTask[] {
  const arr = expectArray(cmd, "(root)", raw);
  return arr.map((item, i) => {
    const o = expectObject(cmd, `[${i}]`, item);
    return {
      task_id: expectNumber(cmd, `[${i}].task_id`, o.task_id),
      path: expectString(cmd, `[${i}].path`, o.path),
      reason: expectString(cmd, `[${i}].reason`, o.reason),
      attempts: expectNumber(cmd, `[${i}].attempts`, o.attempts),
    };
  });
}

export interface WireCrossGroupConflict {
  file_id: number;
  path: string;
  memberships: Array<{
    group_id: number;
    trust: string;
    is_best: boolean;
  }>;
}

export function validateCrossGroupConflictArray(
  cmd: string,
  raw: unknown,
): WireCrossGroupConflict[] {
  const arr = expectArray(cmd, "(root)", raw);
  return arr.map((item, i) => {
    const o = expectObject(cmd, `[${i}]`, item);
    const memberships = expectArray(
      cmd,
      `[${i}].memberships`,
      o.memberships,
    ).map((m, j) => {
      const mo = expectObject(cmd, `[${i}].memberships[${j}]`, m);
      return {
        group_id: expectNumber(
          cmd,
          `[${i}].memberships[${j}].group_id`,
          mo.group_id,
        ),
        trust: expectString(
          cmd,
          `[${i}].memberships[${j}].trust`,
          mo.trust,
        ),
        is_best: expectBoolean(
          cmd,
          `[${i}].memberships[${j}].is_best`,
          mo.is_best,
        ),
      };
    });
    return {
      file_id: expectNumber(cmd, `[${i}].file_id`, o.file_id),
      path: expectString(cmd, `[${i}].path`, o.path),
      memberships,
    };
  });
}

export interface WireSettings {
  scan_folders: string[];
  background_enabled: boolean;
  auto_index: boolean;
  exclude_rules: string[];
  run_on_boot: boolean;
  cpu_throttle: string;
  best_copy_mode: string;
  idle_worker_count?: number | null;
  cpu_cores?: number;
  partial_clips_enabled?: boolean;
  indexing_enabled?: boolean;
}

export function validateWireSettings(
  cmd: string,
  raw: unknown,
): WireSettings {
  const o = expectObject(cmd, "(root)", raw);
  return {
    scan_folders: expectStringArray(cmd, "scan_folders", o.scan_folders),
    background_enabled: expectBoolean(
      cmd,
      "background_enabled",
      o.background_enabled,
    ),
    auto_index: expectBoolean(cmd, "auto_index", o.auto_index),
    exclude_rules: expectStringArray(cmd, "exclude_rules", o.exclude_rules),
    run_on_boot: expectBoolean(cmd, "run_on_boot", o.run_on_boot),
    cpu_throttle: expectString(cmd, "cpu_throttle", o.cpu_throttle),
    best_copy_mode: expectString(cmd, "best_copy_mode", o.best_copy_mode),
    idle_worker_count: expectNumberOrNullOptional(
      cmd,
      "idle_worker_count",
      o.idle_worker_count,
    ),
    cpu_cores: expectNumberOptional(cmd, "cpu_cores", o.cpu_cores),
    partial_clips_enabled: expectBooleanOptional(
      cmd,
      "partial_clips_enabled",
      o.partial_clips_enabled,
    ),
    indexing_enabled: expectBooleanOptional(
      cmd,
      "indexing_enabled",
      o.indexing_enabled,
    ),
  };
}
/**
 * @file validate.ts
 * @brief Tauri IPC 응답의 런타임 형식 검증
 *
 * [변경 이력 (Changelog)]
 * - 2026-08-03 : 클러스터 목록의 내장 멤버 상세 검증 추가
 */
