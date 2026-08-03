
import { rejectMessage, warnMessage } from "./delete-messages";
import { resolveBestFileId } from "./best-copy";
import type {
  ClusterMember,
  ContentCluster,
  DuplicateGroup,
  FileEntry,
} from "./types";


export type DeleteMode = "trash" | "permanent";


export interface DeletionIssue {
  level: "error" | "warning";

  code:
    | "NONE_SELECTED"
    | "DELETE_ALL"
    | "UNKNOWN_MEMBER"
    | "DELETE_BEST"
    | "PERMANENT";

  message: string;
}


export interface DeletionPlan {
  mode: DeleteMode;
  groupId: number;
  toDelete: FileEntry[];
  kept: FileEntry[];

  reclaimedBytes: number;
}


export interface DeletionAssessment {

  plan: DeletionPlan | null;
  issues: DeletionIssue[];

  canProceed: boolean;

  requiresExtraConfirm: boolean;
}


export function defaultSelection(group: DuplicateGroup): Set<number> {
  const best = resolveBestFileId(group);
  return new Set(
    group.members.filter((m) => m.fileId !== best).map((m) => m.fileId),
  );
}


export function assessDeletion(
  group: DuplicateGroup,
  selected: ReadonlySet<number>,
  mode: DeleteMode,
): DeletionAssessment {
  const issues: DeletionIssue[] = [];
  const memberIds = new Set(group.members.map((m) => m.fileId));

  for (const id of selected) {
    if (!memberIds.has(id)) {
      issues.push({
        level: "error",
        code: "UNKNOWN_MEMBER",
        message: rejectMessage("UNKNOWN_MEMBER"),
      });
      break;
    }
  }

  if (selected.size === 0) {
    issues.push({
      level: "error",
      code: "NONE_SELECTED",
      message: rejectMessage("NONE_SELECTED"),
    });
  }

  if (selected.size > 0 && selected.size >= group.members.length) {
    issues.push({
      level: "error",
      code: "DELETE_ALL",
      message: rejectMessage("DELETE_ALL"),
    });
  }

  const best = resolveBestFileId(group);
  if (best !== null && selected.has(best)) {
    issues.push({
      level: "warning",
      code: "DELETE_BEST",
      message: warnMessage("DELETE_BEST"),
    });
  }

  if (mode === "permanent") {
    issues.push({
      level: "warning",
      code: "PERMANENT",
      message: warnMessage("PERMANENT"),
    });
  }

  const hasError = issues.some((i) => i.level === "error");
  const canProceed = !hasError && selected.size > 0;

  const toDelete = group.members.filter((m) => selected.has(m.fileId));
  const kept = group.members.filter((m) => !selected.has(m.fileId));
  const plan: DeletionPlan | null = canProceed
    ? {
        mode,
        groupId: group.groupId,
        toDelete,
        kept,
        reclaimedBytes: toDelete.reduce((sum, f) => sum + f.sizeBytes, 0),
      }
    : null;

  const requiresExtraConfirm =
    canProceed &&
    (mode === "permanent" ||
      issues.some((i) => i.code === "DELETE_BEST"));

  return { plan, issues, canProceed, requiresExtraConfirm };
}



export function clusterSubGroups(cluster: ContentCluster): DuplicateGroup[] {
  const byGroup = new Map<number, ClusterMember[]>();
  for (const member of cluster.members) {
    const list = byGroup.get(member.groupId) ?? [];
    list.push(member);
    byGroup.set(member.groupId, list);
  }
  const groups: DuplicateGroup[] = [];
  for (const [groupId, members] of byGroup) {
    const carriesRepBest =
      cluster.bestFileId !== null &&
      members.some((m) => m.fileId === cluster.bestFileId);
    groups.push({
      groupId,
      trust: members[0].trust,
      bestFileId: carriesRepBest ? cluster.bestFileId : null,
      members,
    });
  }
  groups.sort((a, b) => a.groupId - b.groupId);
  return groups;
}


export function defaultClusterSelection(cluster: ContentCluster): Set<number> {
  const keep = new Set<number>();
  for (const group of clusterSubGroups(cluster)) {
    const best = resolveBestFileId(group);
    if (best !== null) keep.add(best);
  }
  return new Set(
    cluster.members.filter((m) => !keep.has(m.fileId)).map((m) => m.fileId),
  );
}


export interface BulkDeletionPlan {

  clusterCount: number;

  skippedClusterIds: number[];

  toDelete: FileEntry[];
  kept: FileEntry[];

  reclaimedBytes: number;

  perCluster: Array<{ cluster: ContentCluster; selected: Set<number> }>;
}


// Bulk cleanup always applies the same "keep best, delete the rest" default
// per cluster (matching defaultClusterSelection) rather than exposing
// per-file selection — clusters where no best copy could be determined
// (assessClusterDeletion would reject as DELETE_ALL) are skipped rather than
// blocking the whole batch, since one ambiguous cluster shouldn't stop
// cleanup of everything else the user selected.
export function planBulkDeletion(
  clusters: readonly ContentCluster[],
  mode: DeleteMode,
): BulkDeletionPlan {
  const skippedClusterIds: number[] = [];
  const toDelete: FileEntry[] = [];
  const kept: FileEntry[] = [];
  const perCluster: Array<{ cluster: ContentCluster; selected: Set<number> }> =
    [];
  let reclaimedBytes = 0;

  for (const cluster of clusters) {
    const selected = defaultClusterSelection(cluster);
    const assessment = assessClusterDeletion(cluster, selected, mode);
    if (assessment.canProceed && assessment.plan) {
      toDelete.push(...assessment.plan.toDelete);
      kept.push(...assessment.plan.kept);
      reclaimedBytes += assessment.plan.reclaimedBytes;
      perCluster.push({ cluster, selected });
    } else {
      skippedClusterIds.push(cluster.clusterId);
    }
  }

  return {
    clusterCount: clusters.length,
    skippedClusterIds,
    toDelete,
    kept,
    reclaimedBytes,
    perCluster,
  };
}


function dedupeIssues(issues: readonly DeletionIssue[]): DeletionIssue[] {
  const seen = new Set<DeletionIssue["code"]>();
  const out: DeletionIssue[] = [];
  for (const issue of issues) {
    if (!seen.has(issue.code)) {
      seen.add(issue.code);
      out.push(issue);
    }
  }
  return out;
}


export function assessClusterDeletion(
  cluster: ContentCluster,
  selected: ReadonlySet<number>,
  mode: DeleteMode,
): DeletionAssessment {
  const issues: DeletionIssue[] = [];

  const clusterIds = new Set(cluster.members.map((m) => m.fileId));
  for (const id of selected) {
    if (!clusterIds.has(id)) {
      issues.push({
        level: "error",
        code: "UNKNOWN_MEMBER",
        message: rejectMessage("UNKNOWN_MEMBER"),
      });
      break;
    }
  }

  const toDelete: FileEntry[] = [];
  const kept: FileEntry[] = [];
  let reclaimedBytes = 0;
  let totalSelected = 0;
  let blocked = false;
  let extraConfirm = false;

  for (const group of clusterSubGroups(cluster)) {
    const groupIds = new Set(group.members.map((m) => m.fileId));
    const groupSelected = new Set(
      [...selected].filter((id) => groupIds.has(id)),
    );
    if (groupSelected.size === 0) {
      kept.push(...group.members);
      continue;
    }
    totalSelected += groupSelected.size;
    const sub = assessDeletion(group, groupSelected, mode);
    issues.push(...sub.issues);
    if (sub.canProceed && sub.plan) {
      toDelete.push(...sub.plan.toDelete);
      kept.push(...sub.plan.kept);
      reclaimedBytes += sub.plan.reclaimedBytes;
      extraConfirm = extraConfirm || sub.requiresExtraConfirm;
    } else {
      blocked = true;
      kept.push(...group.members.filter((m) => !groupSelected.has(m.fileId)));
    }
  }

  if (totalSelected === 0) {
    issues.push({
      level: "error",
      code: "NONE_SELECTED",
      message: rejectMessage("NONE_SELECTED"),
    });
  }

  const deduped = dedupeIssues(issues);
  const hasError = deduped.some((i) => i.level === "error");
  const canProceed = !hasError && totalSelected > 0 && !blocked;
  const plan: DeletionPlan | null = canProceed
    ? { mode, groupId: cluster.clusterId, toDelete, kept, reclaimedBytes }
    : null;
  const requiresExtraConfirm =
    canProceed && (mode === "permanent" || extraConfirm);

  return { plan, issues: deduped, canProceed, requiresExtraConfirm };
}
