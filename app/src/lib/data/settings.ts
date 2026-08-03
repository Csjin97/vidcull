
import { invokeSafe } from "../ipc/tauri";
import { validateWireSettings } from "../ipc/validate";


export type CpuThrottle = "full" | "balanced" | "eco";


export const CPU_THROTTLE_OPTIONS: ReadonlyArray<{
  value: CpuThrottle;
  label: string;
  hint: string;
}> = [
  { value: "full", label: "최대 성능 (Full)", hint: "병렬 디코딩으로 최대 속도 인덱싱 — 제한 없음" },
  { value: "balanced", label: "균형", hint: "작업 사이에 잠깐 쉼 — CPU 절약" },
  { value: "eco", label: "절약", hint: "작업 사이에 길게 쉼 — 최소 CPU" },
];

function isCpuThrottle(v: unknown): v is CpuThrottle {
  return v === "full" || v === "balanced" || v === "eco";
}


export type BestCopyMode = "archival" | "space_saving" | "max_quality" | "min_size" | "compatible" | "max_resolution";


export const BEST_COPY_MODE_OPTIONS: ReadonlyArray<{
  value: BestCopyMode;
  label: string;
  hint: string;
}> = [
  { value: "archival", label: "원본 예상", hint: "코덱 변환 여부와 상관없이 진짜 원본에 가장 가까울 것으로 예상되는 파일 우선 선택" },
  { value: "space_saving", label: "화질 대비 고효율", hint: "시각적 화질이 비슷하다면 고효율 코덱(HEVC/AV1)의 소용량 사본을 우선 보존" },
  { value: "max_quality", label: "최고 화질 우선", hint: "인코더 태그나 변환 여부에 관계없이 절대적 스펙(해상도/비트레이트)이 높은 사본을 보존" },
  { value: "min_size", label: "최소 용량 우선", hint: "화질이나 스펙에 상관없이 오로지 파일 크기가 가장 작은 사본을 보존하여 공간 극대화" },
  { value: "compatible", label: "범용 호환 우선", hint: "다양한 기기에서 즉시 재생이 가능한 H.264 코덱 및 MP4 컨테이너 사본 우선 보존" },
  { value: "max_resolution", label: "최고 해상도 우선", hint: "비트레이트나 코덱 효율보다 오로지 화면 해상도(가로x세로)가 가장 큰 사본을 우선 보존" },
];

function isBestCopyMode(v: unknown): v is BestCopyMode {
  return v === "archival" || v === "space_saving" || v === "max_quality" || v === "min_size" || v === "compatible" || v === "max_resolution";
}


interface WireSettings {
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


export interface Settings {

  scanFolders: string[];

  backgroundEnabled: boolean;

  autoIndex: boolean;

  excludeRules: string[];

  runOnBoot: boolean;

  cpuThrottle: CpuThrottle;

  bestCopyMode: BestCopyMode;

  workerCount: number | null;

  cpuCores: number;

  partialClipsEnabled: boolean;

  indexingEnabled: boolean;
}


export function defaultSettings(): Settings {
  return {
    scanFolders: [],
    backgroundEnabled: true,
    autoIndex: true,
    excludeRules: [],
    runOnBoot: false,
    cpuThrottle: "full",
    bestCopyMode: "archival",
    workerCount: null,
    cpuCores: 1,
    partialClipsEnabled: true,
    indexingEnabled: true,
  };
}


export const SETTINGS_STORAGE_KEY = "vidcull.settings";

function fromWire(w: WireSettings): Settings {
  return {
    scanFolders: w.scan_folders,
    backgroundEnabled: w.background_enabled,
    autoIndex: w.auto_index,
    excludeRules: w.exclude_rules,
    runOnBoot: w.run_on_boot,
    cpuThrottle: isCpuThrottle(w.cpu_throttle) ? w.cpu_throttle : "full",
    bestCopyMode: isBestCopyMode(w.best_copy_mode) ? w.best_copy_mode : "archival",
    workerCount:
      typeof w.idle_worker_count === "number" ? w.idle_worker_count : null,
    cpuCores: typeof w.cpu_cores === "number" && w.cpu_cores >= 1 ? w.cpu_cores : 1,
    partialClipsEnabled:
      typeof w.partial_clips_enabled === "boolean" ? w.partial_clips_enabled : false,
    indexingEnabled:
      typeof w.indexing_enabled === "boolean" ? w.indexing_enabled : true,
  };
}

function toWire(s: Settings): WireSettings {
  return {
    scan_folders: s.scanFolders,
    background_enabled: s.backgroundEnabled,
    auto_index: s.autoIndex,
    exclude_rules: s.excludeRules,
    run_on_boot: s.runOnBoot,
    cpu_throttle: s.cpuThrottle,
    best_copy_mode: s.bestCopyMode,
    idle_worker_count: s.workerCount,
    cpu_cores: s.cpuCores,
    partial_clips_enabled: s.partialClipsEnabled,
    indexing_enabled: s.indexingEnabled,
  };
}


function inTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}


export function coerceSettings(value: unknown): Settings {
  const d = defaultSettings();
  if (typeof value !== "object" || value === null) {
    return d;
  }
  const v = value as Record<string, unknown>;
  return {
    scanFolders: Array.isArray(v.scanFolders) ? v.scanFolders : d.scanFolders,
    backgroundEnabled:
      typeof v.backgroundEnabled === "boolean"
        ? v.backgroundEnabled
        : d.backgroundEnabled,
    autoIndex: typeof v.autoIndex === "boolean" ? v.autoIndex : d.autoIndex,
    excludeRules: Array.isArray(v.excludeRules) ? v.excludeRules : d.excludeRules,
    runOnBoot: typeof v.runOnBoot === "boolean" ? v.runOnBoot : d.runOnBoot,
    cpuThrottle: isCpuThrottle(v.cpuThrottle) ? v.cpuThrottle : d.cpuThrottle,
    bestCopyMode: isBestCopyMode(v.bestCopyMode) ? v.bestCopyMode : d.bestCopyMode,
    workerCount:
      typeof v.workerCount === "number" || v.workerCount === null
        ? (v.workerCount as number | null)
        : d.workerCount,
    cpuCores:
      typeof v.cpuCores === "number" && v.cpuCores >= 1 ? v.cpuCores : d.cpuCores,
    partialClipsEnabled:
      typeof v.partialClipsEnabled === "boolean"
        ? v.partialClipsEnabled
        : d.partialClipsEnabled,
    indexingEnabled:
      typeof v.indexingEnabled === "boolean"
        ? v.indexingEnabled
        : d.indexingEnabled,
  };
}


export async function loadSettings(): Promise<Settings> {
  if (inTauri()) {
    const raw = await invokeSafe<unknown>("get_settings");
    const wire = validateWireSettings("get_settings", raw);
    return fromWire(wire);
  }
  if (typeof localStorage === "undefined") {
    return defaultSettings();
  }
  const raw = localStorage.getItem(SETTINGS_STORAGE_KEY);
  if (raw === null) {
    return defaultSettings();
  }
  try {
    return coerceSettings(JSON.parse(raw));
  } catch {
    return defaultSettings();
  }
}


export async function saveSettings(settings: Settings): Promise<Settings> {
  if (inTauri()) {
    const raw = await invokeSafe<unknown>("set_settings", {
      settings: toWire(settings),
    });
    const stored = validateWireSettings("set_settings", raw);
    return fromWire(stored);
  }
  if (typeof localStorage !== "undefined") {
    localStorage.setItem(SETTINGS_STORAGE_KEY, JSON.stringify(settings));
  }
  return settings;
}


export async function setIndexingEnabled(enabled: boolean): Promise<Settings> {
  const fresh = await loadSettings();
  return saveSettings({ ...fresh, indexingEnabled: enabled });
}


export async function addScanFolder(path: string): Promise<Settings> {
  const normalized = path.trim().replace(/\\/g, "/");
  const fresh = await loadSettings();
  if (!normalized || fresh.scanFolders.includes(normalized)) {
    return fresh;
  }
  return saveSettings({
    ...fresh,
    scanFolders: [...fresh.scanFolders, normalized],
  });
}


export async function removeScanFolder(path: string): Promise<Settings> {
  const fresh = await loadSettings();
  return saveSettings({
    ...fresh,
    scanFolders: fresh.scanFolders.filter((f) => f !== path),
  });
}
