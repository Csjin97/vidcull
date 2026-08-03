

export type DeleteRejectCode =
  | "NONE_SELECTED"
  | "UNKNOWN_MEMBER"
  | "DELETE_ALL"
  | "BEST_UNCONFIRMED";


export type DeleteWarnCode = "DELETE_BEST" | "PERMANENT";


export type DeleteIssueCode = DeleteRejectCode | DeleteWarnCode;

const REJECT_MESSAGES: Record<DeleteRejectCode, string> = {
  NONE_SELECTED: "삭제할 파일을 선택하세요.",
  UNKNOWN_MEMBER: "선택한 파일이 이 그룹에 속하지 않습니다.",
  DELETE_ALL: "그룹의 모든 사본은 삭제할 수 없습니다. 최소 한 개는 보존하세요.",
  BEST_UNCONFIRMED: "최고 품질 사본을 삭제하려면 명시적 확인이 필요합니다.",
};

const WARN_MESSAGES: Record<DeleteWarnCode, string> = {
  DELETE_BEST: "최고 품질 사본을 삭제하려고 합니다. 정말 진행하시겠습니까?",
  PERMANENT: "영구 삭제는 되돌릴 수 없습니다. 휴지통으로 이동하지 않습니다.",
};


export const DELETE_REJECTED_FALLBACK = "삭제할 수 없습니다.";


export function rejectMessage(code: DeleteRejectCode): string {
  return REJECT_MESSAGES[code];
}


export function warnMessage(code: DeleteWarnCode): string {
  return WARN_MESSAGES[code];
}

function isRejectCode(code: string): code is DeleteRejectCode {
  return code in REJECT_MESSAGES;
}


export function wireRejectMessage(code: string): string | null {
  return isRejectCode(code) ? REJECT_MESSAGES[code] : null;
}


export function daemonVersionMismatchLabel(daemonVersion: number): string {
  return `daemon: 버전 불일치 (v${daemonVersion}) — 앱 업데이트 필요`;
}


export function daemonGateBlockMessage(
  cmd: string,
  daemonVersion: number,
  appVersion: number,
): string {
  return (
    `프로토콜 버전 불일치 (데몬 v${daemonVersion}, 앱 v${appVersion}) — ` +
    `'${cmd}' 요청을 차단했습니다. 앱과 데몬을 같은 버전으로 업데이트하세요.`
  );
}
