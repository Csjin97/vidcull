
/**
 * @file datasource.ts
 * @brief UI 데이터소스 계약과 개발용 구현
 *
 * [변경 이력 (Changelog)]
 * - 2026-08-03 : 클러스터 조회 성능 개선에 맞춰 개발용 구현 정비
 */
import { assessDeletion, type DeleteMode } from "../model/safe-delete";
import {
  DELETE_REJECTED_FALLBACK,
  wireRejectMessage,
} from "../model/delete-messages";
import { membersByQuality } from "../model/best-copy";
import { makeMockOverlaps } from "./mock-data";
import { simulateProgress } from "../model/progress";
import { clusterAsGroup, clusterGroups } from "../model/cluster";
import type {
  ClipOverlap,
  ContentCluster,
  CrossGroupConflict,
  DuplicateGroup,
  FailedTask,
  GroupRole,
  ProgressSnapshot,
  TrustLevel,
} from "../model/types";


export interface GroupPageQuery {

  trust?: TrustLevel;

  limit: number;

  offset: number;
}


export interface DeleteOutcome {

  ok: boolean;

  removedFileIds: number[];

  reclaimedBytes: number;

  detail: string;

  rejectCode: string | null;
}


export interface UndoOutcome {

  ok: boolean;

  groupId: number | null;

  restoredFileIds: number[];

  missingPaths: string[];

  detail: string;
}


export interface DataSource {

  countGroups(trust?: TrustLevel): Promise<number>;

  listGroups(query: GroupPageQuery): Promise<DuplicateGroup[]>;

  countClusters(trust?: TrustLevel): Promise<number>;

  listClusters(query: GroupPageQuery): Promise<ContentCluster[]>;

  fetchThumbnail(fileId: number): Promise<string | null>;

  reclaimableBytes(): Promise<number>;

  clusterReclaimableBytes(): Promise<number>;

  deleteFiles(
    groupId: number,
    fileIds: readonly number[],
    mode: DeleteMode,
    confirmBest?: boolean,
  ): Promise<DeleteOutcome>;

  progress(): Promise<ProgressSnapshot>;

  partialOverlaps(groupId: number): Promise<ClipOverlap[]>;

  failedTasks(limit: number): Promise<FailedTask[]>;

  crossGroupConflicts(groupId: number): Promise<CrossGroupConflict[]>;

  undoLastDelete(): Promise<UndoOutcome>;
}


export function reclaimableAcross(groups: readonly DuplicateGroup[]): number {
  let total = 0;
  for (const group of groups) {
    const ordered = membersByQuality(group);
    for (let i = 1; i < ordered.length; i += 1) {
      total += ordered[i].sizeBytes;
    }
  }
  return total;
}


interface UndoEntry {

  groups: DuplicateGroup[];

  removedFileIds: number[];

  groupId: number;
}


export class MockDataSource implements DataSource {
  private groups: DuplicateGroup[];

  private readonly totalTasks: number;

  private progressTick = 0;

  private undoStack: UndoEntry[] = [];

  constructor(groups: DuplicateGroup[]) {
    this.groups = groups;
    const members = groups.reduce((n, g) => n + g.members.length, 0);
    this.totalTasks = Math.max(1, members);
  }

  private filtered(trust?: TrustLevel): DuplicateGroup[] {
    return trust ? this.groups.filter((g) => g.trust === trust) : this.groups;
  }

  countGroups(trust?: TrustLevel): Promise<number> {
    return Promise.resolve(this.filtered(trust).length);
  }

  listGroups(query: GroupPageQuery): Promise<DuplicateGroup[]> {
    const page = this.filtered(query.trust).slice(
      query.offset,
      query.offset + query.limit,
    );
    return Promise.resolve(page);
  }

  reclaimableBytes(): Promise<number> {
    return Promise.resolve(reclaimableAcross(this.groups));
  }


  private clusterList(trust?: TrustLevel): ContentCluster[] {
    const all = clusterGroups(this.groups);
    return trust ? all.filter((c) => c.representativeTrust === trust) : all;
  }

  countClusters(trust?: TrustLevel): Promise<number> {
    return Promise.resolve(this.clusterList(trust).length);
  }

  listClusters(query: GroupPageQuery): Promise<ContentCluster[]> {
    const page = this.clusterList(query.trust).slice(
      query.offset,
      query.offset + query.limit,
    );
    return Promise.resolve(page);
  }

  clusterReclaimableBytes(): Promise<number> {
    return Promise.resolve(
      reclaimableAcross(clusterGroups(this.groups).map(clusterAsGroup)),
    );
  }

  fetchThumbnail(fileId: number): Promise<string | null> {
    for (const group of this.groups) {
      const member = group.members.find((m) => m.fileId === fileId);
      if (member) {
        return Promise.resolve(member.thumbnailUrl);
      }
    }
    return Promise.resolve(null);
  }

  deleteFiles(
    groupId: number,
    fileIds: readonly number[],
    mode: DeleteMode,
    _confirmBest = false,
  ): Promise<DeleteOutcome> {
    const group = this.groups.find((g) => g.groupId === groupId);
    if (!group) {
      return Promise.resolve({
        ok: false,
        removedFileIds: [],
        reclaimedBytes: 0,
        detail: `그룹 ${groupId}을(를) 찾을 수 없습니다.`,
        rejectCode: null,
      });
    }

    const assessment = assessDeletion(group, new Set(fileIds), mode);
    if (!assessment.canProceed || !assessment.plan) {
      const errorIssue = assessment.issues.find((i) => i.level === "error");
      const code = errorIssue?.code ?? null;
      const detail = errorIssue
        ? (wireRejectMessage(errorIssue.code) ?? errorIssue.message)
        : DELETE_REJECTED_FALLBACK;
      return Promise.resolve({
        ok: false,
        removedFileIds: [],
        reclaimedBytes: 0,
        detail,
        rejectCode: code,
      });
    }

    const removed = new Set(assessment.plan.toDelete.map((f) => f.fileId));

    this.undoStack.push({
      groups: structuredClone(this.groups),
      removedFileIds: [...removed],
      groupId,
    });

    group.members = group.members.filter((m) => !removed.has(m.fileId));
    if (group.bestFileId !== null && removed.has(group.bestFileId)) {
      group.bestFileId = null; 
    }
    if (group.members.length < 2) {
      this.groups = this.groups.filter((g) => g.groupId !== groupId);
    }

    return Promise.resolve({
      ok: true,
      removedFileIds: [...removed],
      reclaimedBytes: assessment.plan.reclaimedBytes,
      detail:
        mode === "trash"
          ? `${removed.size}개 파일을 휴지통으로 이동했습니다.`
          : `${removed.size}개 파일을 영구 삭제했습니다.`,
      rejectCode: null,
    });
  }

  progress(): Promise<ProgressSnapshot> {
    const snapshot = simulateProgress(this.progressTick, this.totalTasks);
    this.progressTick += 1;
    return Promise.resolve(snapshot);
  }

  partialOverlaps(groupId: number): Promise<ClipOverlap[]> {
    const group = this.groups.find((g) => g.groupId === groupId);
    return Promise.resolve(group ? makeMockOverlaps(group) : []);
  }

  crossGroupConflicts(groupId: number): Promise<CrossGroupConflict[]> {
    const group = this.groups.find((g) => g.groupId === groupId);
    if (!group) {
      return Promise.resolve([]);
    }
    const conflicts: CrossGroupConflict[] = [];
    for (const member of group.members) {
      const containing = this.groups.filter((g) =>
        g.members.some((m) => m.fileId === member.fileId),
      );
      if (containing.length < 2) {
        continue;
      }
      const memberships: GroupRole[] = containing
        .map((g) => ({
          groupId: g.groupId,
          trust: g.trust,
          isBest: g.bestFileId === member.fileId,
        }))
        .sort((a, b) => a.groupId - b.groupId);
      const anyBest = memberships.some((m) => m.isBest);
      const anyCandidate = memberships.some((m) => !m.isBest);
      if (anyBest && anyCandidate) {
        conflicts.push({
          fileId: member.fileId,
          path: member.path,
          memberships,
        });
      }
    }
    return Promise.resolve(conflicts);
  }

  failedTasks(limit: number): Promise<FailedTask[]> {
    const sample: FailedTask[] = [
      {
        taskId: 1002,
        path: "C:/videos/corrupt-finale.mkv",
        reason: "decode error: invalid NAL unit",
        attempts: 3,
      },
      {
        taskId: 1001,
        path: "C:/videos/archive/old-codec.avi",
        reason: "no decoder for codec 'mpeg2video'",
        attempts: 1,
      },
    ];
    return Promise.resolve(sample.slice(0, Math.max(0, limit)));
  }

  undoLastDelete(): Promise<UndoOutcome> {
    const entry = this.undoStack.pop();
    if (!entry) {
      return Promise.resolve({
        ok: false,
        groupId: null,
        restoredFileIds: [],
        missingPaths: [],
        detail: "되돌릴 삭제 내역이 없습니다.",
      });
    }
    this.groups = entry.groups;
    return Promise.resolve({
      ok: true,
      groupId: entry.groupId,
      restoredFileIds: entry.removedFileIds,
      missingPaths: [],
      detail: `${entry.removedFileIds.length}개 파일을 복원했습니다.`,
    });
  }
}
