
import { invokeSafe } from "../ipc/tauri";
import {
  validateProgressSnapshot,
  validateGroupStats,
  validateClusterStats,
  validateGroupSummaryArray,
  validateFileDetailArray,
  validateClusterSummaryArray,
  validateThumbnail,
  validateDeleteResult,
  validateUndoResult,
  validateClipOverlapArray,
  validateFailedTaskArray,
  validateCrossGroupConflictArray,
} from "../ipc/validate";
import type { DataSource, DeleteOutcome, GroupPageQuery, UndoOutcome } from "./datasource";
import {
  DELETE_REJECTED_FALLBACK,
  wireRejectMessage,
} from "../model/delete-messages";
import type { DeleteMode } from "../model/safe-delete";
import type {
  ClipOverlap,
  ClusterMember,
  ContentCluster,
  CrossGroupConflict,
  DuplicateGroup,
  FailedTask,
  FileEntry,
  GroupRole,
  ProgressSnapshot,
  TrustLevel,
} from "../model/types";



interface WireFileDetail {
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


interface WireClusterMemberDetail {
  file: WireFileDetail;
  trust: string;
  group_id: number;
}


type WireTrust = "Exact" | "VeryLikely" | "Possible";

function mapTrust(trust: string): TrustLevel {
  switch (trust) {
    case "Exact":
      return "EXACT";
    case "VeryLikely":
      return "VERY_LIKELY";
    case "Possible":
      return "POSSIBLE";
    default:
      throw new Error(`[tauri-datasource] 알 수 없는 TrustLevel 값: '${trust}'`);
  }
}


interface WireCrossGroupConflict {
  file_id: number;
  path: string;
  memberships: Array<{ group_id: number; trust: string; is_best: boolean }>;
}


function toWireTrust(trust: TrustLevel): WireTrust {
  switch (trust) {
    case "EXACT":
      return "Exact";
    case "VERY_LIKELY":
      return "VeryLikely";
    case "POSSIBLE":
      return "Possible";
  }
}


function toFileEntry(detail: WireFileDetail): FileEntry {
  return {
    fileId: detail.file_id,
    path: detail.path,
    sizeBytes: detail.size_bytes,
    width: detail.width ?? 0,
    height: detail.height ?? 0,
    durationMs: detail.duration_ms ?? 0,
    bitrateBps: detail.bitrate_bps ?? 0,
    codec: detail.codec ?? "",
    container: detail.container ?? "",
    thumbnailUrl: detail.thumbnail ?? null,
  };
}


function toClusterMember(detail: WireClusterMemberDetail): ClusterMember {
  return {
    ...toFileEntry(detail.file),
    trust: mapTrust(detail.trust),
    groupId: detail.group_id,
  };
}


function toCrossGroupConflict(
  conflict: WireCrossGroupConflict,
): CrossGroupConflict {
  return {
    fileId: conflict.file_id,
    path: conflict.path,
    memberships: conflict.memberships.map(
      (m): GroupRole => ({
        groupId: m.group_id,
        trust: mapTrust(m.trust),
        isBest: m.is_best,
      }),
    ),
  };
}

export class TauriDataSource implements DataSource {
  async countGroups(trust?: TrustLevel): Promise<number> {
    const raw = await invokeSafe<unknown>("group_stats", {
      trust: trust ? toWireTrust(trust) : null,
    });
    const stats = validateGroupStats("group_stats", raw);
    return stats.group_count;
  }

  async listGroups(query: GroupPageQuery): Promise<DuplicateGroup[]> {
    const rawSummaries = await invokeSafe<unknown>("list_groups", {
      trust: query.trust ? toWireTrust(query.trust) : null,
      limit: query.limit,
      offset: query.offset,
    });
    const summaries = validateGroupSummaryArray("list_groups", rawSummaries);
    const detailed = await Promise.all(
      summaries.map(async (summary) => {
        const rawMembers = await invokeSafe<unknown>(
          "list_group_detail",
          { groupId: summary.group_id },
        );
        const members = validateFileDetailArray("list_group_detail", rawMembers);
        return {
          groupId: summary.group_id,
          trust: mapTrust(summary.trust as WireTrust),
          bestFileId: summary.best_file_id,
          members: members.map(toFileEntry),
          introOutro: summary.intro_outro,
        } satisfies DuplicateGroup;
      }),
    );
    return detailed;
  }

  async reclaimableBytes(): Promise<number> {
    const raw = await invokeSafe<unknown>("group_stats", { trust: null });
    const stats = validateGroupStats("group_stats", raw);
    return stats.reclaimable_bytes;
  }

  async countClusters(trust?: TrustLevel): Promise<number> {
    const raw = await invokeSafe<unknown>("cluster_stats", {
      trust: trust ? toWireTrust(trust) : null,
    });
    const stats = validateClusterStats("cluster_stats", raw);
    return stats.cluster_count;
  }
/**
 * @file tauri-datasource.ts
 * @brief Tauri IPC 응답을 UI 모델로 변환하는 데이터소스
 *
 * [변경 이력 (Changelog)]
 * - 2026-08-03 : 클러스터 N+1 상세 조회 제거
 */
  async listClusters(query: GroupPageQuery): Promise<ContentCluster[]> {
    const rawSummaries = await invokeSafe<unknown>("list_clusters", {
      trust: query.trust ? toWireTrust(query.trust) : null,
      limit: query.limit,
      offset: query.offset,
    });
    const summaries = validateClusterSummaryArray("list_clusters", rawSummaries);
    return summaries
      .map((summary) => ({
          clusterId: summary.cluster_id,
          representativeTrust: mapTrust(
            summary.representative_trust as WireTrust,
          ),
          bestFileId: summary.best_file_id,
          members: summary.members.map(toClusterMember),
          introOutro: summary.intro_outro,
        }) satisfies ContentCluster)
      .filter((cluster) => cluster.members.length > 0);
  }

  async fetchThumbnail(fileId: number): Promise<string | null> {
    const raw = await invokeSafe<unknown>("thumbnail", { fileId });
    return validateThumbnail("thumbnail", raw);
  }

  async clusterReclaimableBytes(): Promise<number> {
    const raw = await invokeSafe<unknown>("cluster_stats", { trust: null });
    const stats = validateClusterStats("cluster_stats", raw);
    return stats.reclaimable_bytes;
  }

  async deleteFiles(
    groupId: number,
    fileIds: readonly number[],
    mode: DeleteMode,
    confirmBest = false,
  ): Promise<DeleteOutcome> {
    const raw = await invokeSafe<unknown>("delete_files", {
      groupId,
      fileIds: [...fileIds],
      mode,
      confirmBest,
    });
    const result = validateDeleteResult("delete_files", raw);
    const detail =
      result.detail === "" && result.reject_code !== null
        ? (wireRejectMessage(result.reject_code) ?? DELETE_REJECTED_FALLBACK)
        : result.detail;
    return {
      ok: result.ok,
      removedFileIds: result.removed_file_ids,
      reclaimedBytes: result.reclaimed_bytes,
      detail,
      rejectCode: result.reject_code,
    };
  }

  async progress(): Promise<ProgressSnapshot> {
    const raw = await invokeSafe<unknown>("daemon_progress");
    const wire = validateProgressSnapshot("daemon_progress", raw);
    return {
      pending: wire.pending,
      running: wire.running,
      done: wire.done,
      failed: wire.failed,
      cpuUsagePermille: wire.cpu_usage_permille ?? 0,
      rssBytes: wire.rss_bytes ?? 0,
      throughputBytesPerSec: wire.throughput_bytes_per_sec ?? 0,
      pendingBytes: wire.pending_bytes ?? 0,
      currentFiles: wire.current_files ?? [],
      partialPending: wire.partial_pending ?? 0,
      partialRunning: wire.partial_running ?? 0,
      partialDone: wire.partial_done ?? 0,
      partialSkipped: wire.partial_skipped ?? {},
      partialFailed: wire.partial_failed ?? 0,
      folderScanning: wire.folder_scanning ?? false,
      scanDiscovered: wire.scan_discovered ?? 0,
      groupsRevision: wire.groups_revision ?? 0,
    };
  }

  async partialOverlaps(groupId: number): Promise<ClipOverlap[]> {
    const raw = await invokeSafe<unknown>("partial_overlaps", { groupId });
    const overlaps = validateClipOverlapArray("partial_overlaps", raw);
    return overlaps.map((o) => ({
      clipFileId: o.clip_file_id,
      sourceFileId: o.source_file_id,
      matchedScenes: o.matched_scenes,
      clipScenes: o.clip_scenes,
      startMs: o.start_ms,
      endMs: o.end_ms,
      clipStartMs: o.clip_start_ms,
      clipEndMs: o.clip_end_ms,
      introOutro: o.intro_outro,
    }));
  }

  async failedTasks(limit: number): Promise<FailedTask[]> {
    const raw = await invokeSafe<unknown>("failed_tasks", { limit });
    const tasks = validateFailedTaskArray("failed_tasks", raw);
    return tasks.map((t) => ({
      taskId: t.task_id,
      path: t.path,
      reason: t.reason,
      attempts: t.attempts,
    }));
  }

  async crossGroupConflicts(groupId: number): Promise<CrossGroupConflict[]> {
    const raw = await invokeSafe<unknown>("cross_group_conflicts", { groupId });
    const conflicts = validateCrossGroupConflictArray(
      "cross_group_conflicts",
      raw,
    );
    return conflicts.map(toCrossGroupConflict);
  }

  async undoLastDelete(): Promise<UndoOutcome> {
    const raw = await invokeSafe<unknown>("undo_last_delete");
    const result = validateUndoResult("undo_last_delete", raw);
    return {
      ok: result.ok,
      groupId: result.group_id,
      restoredFileIds: result.restored_file_ids,
      missingPaths: result.missing_paths,
      detail: result.detail,
    };
  }
}
