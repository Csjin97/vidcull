
import { IpcValidationError } from "../ipc/validate";

export type Severity = "error" | "warning" | "info";

export interface NormalizedError {
  message: string;
  severity: Severity;
}

const PATH_RE =
  /[A-Za-z]:[\\\/][^\s"',;)[\]]*|\\\\[^\s"',;)[\]]+|\/(?:home|Users|mnt|var|tmp|opt|data|srv)\/[^\s"',;)[\]]*/g;

function basename(path: string): string {
  const i = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
  return i >= 0 ? path.slice(i + 1) : path;
}

function redactPaths(msg: string): string {
  return msg.replace(PATH_RE, (match) => {
    const name = basename(match);
    return name.length > 0 ? name : "[경로]";
  });
}

export function normalizeError(input: unknown): NormalizedError {
  if (
    typeof PromiseRejectionEvent !== "undefined" &&
    input instanceof PromiseRejectionEvent
  ) {
    return normalizeError(input.reason);
  }

  if (input instanceof IpcValidationError) {
    return {
      message: `IPC 응답 검증 오류: 필드 '${input.field}' 형식이 올바르지 않습니다.`,
      severity: "warning",
    };
  }

  if (input == null) {
    return { message: "알 수 없는 오류", severity: "error" };
  }

  if (input instanceof Error) {
    return { message: redactPaths(input.message), severity: "error" };
  }

  if (typeof input === "string") {
    const trimmed = input.trim();
    return {
      message: trimmed.length > 0 ? redactPaths(trimmed) : "알 수 없는 오류",
      severity: "error",
    };
  }

  return { message: redactPaths(String(input)), severity: "error" };
}
