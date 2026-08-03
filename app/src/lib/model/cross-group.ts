
import type { CrossGroupConflict, GroupRole, TrustLevel } from "./types";


const TRUST_LABEL: Record<TrustLevel, string> = {
  EXACT: "완전 일치",
  VERY_LIKELY: "매우 유사",
  POSSIBLE: "부분 일치",
};


export interface ConflictSummary {

  fileId: number;

  path: string;

  keptIn: GroupRole[];

  candidateIn: GroupRole[];

  keptElsewhere: GroupRole[];

  dangerousHere: boolean;

  message: string;
}


export function summarizeConflict(
  conflict: CrossGroupConflict,
  viewingGroupId: number,
): ConflictSummary {
  const keptIn = conflict.memberships.filter((m) => m.isBest);
  const candidateIn = conflict.memberships.filter((m) => !m.isBest);
  const keptElsewhere = keptIn.filter((m) => m.groupId !== viewingGroupId);
  const dangerousHere = keptElsewhere.length > 0;

  const message = dangerousHere
    ? `이 파일은 ${describeGroups(keptElsewhere)}에서 보존되는 최적 사본입니다. ` +
      `여기서 삭제하면 해당 그룹의 사본도 함께 사라집니다.`
    : `이 파일은 ${describeGroups(conflict.memberships)}에 함께 속해 있습니다.`;

  return {
    fileId: conflict.fileId,
    path: conflict.path,
    keptIn,
    candidateIn,
    keptElsewhere,
    dangerousHere,
    message,
  };
}


export function isDangerousToDeleteHere(
  conflicts: readonly CrossGroupConflict[],
  fileId: number,
  viewingGroupId: number,
): boolean {
  const conflict = conflicts.find((c) => c.fileId === fileId);
  return conflict
    ? summarizeConflict(conflict, viewingGroupId).dangerousHere
    : false;
}


function describeGroups(roles: readonly GroupRole[]): string {
  return roles
    .map((r) => `그룹 ${r.groupId}(${TRUST_LABEL[r.trust]})`)
    .join(", ");
}
