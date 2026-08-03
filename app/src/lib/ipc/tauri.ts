
import { gateCommand, interpretPong, type PingResult } from "../../daemon";
import { IpcTraceBuffer } from "../model/ipc-trace";

function inTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

let lastPing: PingResult = { ok: false, error: "연결 확인 전" };



export let frontendTraceEnabled = false;
let frontendTraceChecked = false;


export const ipcTraceBuffer = new IpcTraceBuffer(60);


function ensureFrontendTraceChecked(): void {
  if (frontendTraceChecked || !inTauri()) return;
  frontendTraceChecked = true;
  void (async () => {
    try {
      const { invoke } = await import("@tauri-apps/api/core");
      frontendTraceEnabled = await invoke<boolean>("frontend_trace_enabled");
    } catch {
      frontendTraceEnabled = false;
    }
  })();
}


export async function invokeSafe<T>(
  cmd: string,
  args?: Record<string, unknown>,
): Promise<T> {
  if (!inTauri()) {
    throw new Error("Tauri 런타임 밖에서는 데몬에 연결할 수 없습니다.");
  }
  const blocked = gateCommand(cmd, lastPing);
  if (blocked !== null) {
    throw new Error(blocked);
  }
  const { invoke } = await import("@tauri-apps/api/core");
  if (!frontendTraceEnabled) {
    ensureFrontendTraceChecked();
    return invoke<T>(cmd, args);
  }
  const startedAt = performance.now();
  try {
    return await invoke<T>(cmd, args);
  } finally {
    const durationMs = performance.now() - startedAt;
    ipcTraceBuffer.record(cmd, durationMs);
    console.debug(`[ipc-trace] ${cmd} ${durationMs.toFixed(1)}ms`);
  }
}


export async function pingDaemon(): Promise<PingResult> {
  try {
    const version = await invokeSafe<number>("ping_daemon");
    lastPing = interpretPong(version);
  } catch (err) {
    lastPing = { ok: false, error: String(err) };
  }
  return lastPing;
}


export async function revealInFolder(path: string): Promise<void> {
  await invokeSafe<null>("reveal_in_folder", { path });
}


export async function openFolder(path: string): Promise<void> {
  await invokeSafe<null>("open_folder", { path });
}


export async function pickFolder(): Promise<string | null> {
  return invokeSafe<string | null>("pick_folder");
}


export async function rescanDirectory(path: string): Promise<string> {
  return invokeSafe<string>("rescan_directory", { path });
}


export async function forceRescanDirectory(path: string): Promise<string> {
  return invokeSafe<string>("force_rescan_directory", { path });
}


export async function setLogLevel(level: string): Promise<string> {
  return invokeSafe<string>("set_log_level", { level });
}


export async function exportDiagnostics(dest: string): Promise<string> {
  return invokeSafe<string>("export_diagnostics", { dest });
}
