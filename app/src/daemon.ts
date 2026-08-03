
import {
  daemonGateBlockMessage,
  daemonVersionMismatchLabel,
} from "./lib/model/delete-messages";


export const EXPECTED_PROTOCOL_VERSION = 28;

export type PingResult =
  | { ok: true; protocolVersion: number; compatible: boolean }
  | { ok: false; error: string };


export function interpretPong(protocolVersion: number): PingResult {
  return {
    ok: true,
    protocolVersion,
    compatible: protocolVersion === EXPECTED_PROTOCOL_VERSION,
  };
}


export function statusLabel(result: PingResult): string {
  if (!result.ok) {
    return "daemon: offline";
  }
  return result.compatible
    ? `daemon: online (v${result.protocolVersion})`
    : daemonVersionMismatchLabel(result.protocolVersion);
}


export function statusClass(result: PingResult): "status--ok" | "status--bad" {
  return result.ok && result.compatible ? "status--ok" : "status--bad";
}


const GATE_EXEMPT_COMMANDS = new Set([
  "ping_daemon",
  "reveal_in_folder",
  "open_folder",
  "pick_folder",
]);


export function gateCommand(cmd: string, last: PingResult): string | null {
  if (!last.ok || last.compatible || GATE_EXEMPT_COMMANDS.has(cmd)) {
    return null;
  }
  return daemonGateBlockMessage(cmd, last.protocolVersion, EXPECTED_PROTOCOL_VERSION);
}
/**
 * @file daemon.ts
 * @brief 데몬 연결 상태와 프로토콜 호환성 판정
 *
 * [변경 이력 (Changelog)]
 * - 2026-08-03 : 클러스터 배치 조회 프로토콜 v28 반영
 */
