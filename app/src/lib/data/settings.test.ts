import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const invokeSafe = vi.fn();
vi.mock("../ipc/tauri", () => ({
  invokeSafe: (...args: unknown[]) => invokeSafe(...args),
}));

const {
  loadSettings,
  saveSettings,
  setIndexingEnabled,
  addScanFolder,
  removeScanFolder,
  defaultSettings,
  coerceSettings,
  SETTINGS_STORAGE_KEY,
} = await import("./settings");

beforeEach(() => {
  invokeSafe.mockReset();
  localStorage.clear();
});

afterEach(() => {
  delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__;
});

describe("defaultSettings", () => {
  it("matches the daemon defaults (background+autoIndex on, partial clips on, no folders, no boot)", () => {
    expect(defaultSettings()).toEqual({
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
    });
  });
});

describe("coerceSettings", () => {
  it("fills missing fields from defaults", () => {
    expect(coerceSettings({ scanFolders: ["C:/v"] })).toEqual({
      ...defaultSettings(),
      scanFolders: ["C:/v"],
    });
  });

  it("returns defaults for non-object input", () => {
    expect(coerceSettings(null)).toEqual(defaultSettings());
    expect(coerceSettings("nope")).toEqual(defaultSettings());
  });

  it("keeps a valid cpuThrottle and rejects an invalid one", () => {
    expect(coerceSettings({ cpuThrottle: "eco" }).cpuThrottle).toBe("eco");
    expect(coerceSettings({ cpuThrottle: "turbo" }).cpuThrottle).toBe("full");
  });
});

describe("localStorage fallback (browser / vitest)", () => {
  it("loads defaults when nothing is stored", async () => {
    expect(await loadSettings()).toEqual(defaultSettings());
    expect(invokeSafe).not.toHaveBeenCalled();
  });

  it("round-trips a saved value through localStorage", async () => {
    const settings = {
      scanFolders: ["C:/videos", "D:/raw"],
      backgroundEnabled: false,
      autoIndex: true,
      excludeRules: ["node_modules"],
      runOnBoot: true,
      cpuThrottle: "eco" as const,
      bestCopyMode: "space_saving" as const,
      workerCount: 3,
      cpuCores: 8,
      partialClipsEnabled: true,
      indexingEnabled: false,
    };
    const stored = await saveSettings(settings);
    expect(stored).toEqual(settings);
    expect(localStorage.getItem(SETTINGS_STORAGE_KEY)).not.toBeNull();
    expect(await loadSettings()).toEqual(settings);
    expect(invokeSafe).not.toHaveBeenCalled();
  });

  it("recovers from a corrupt stored blob", async () => {
    localStorage.setItem(SETTINGS_STORAGE_KEY, "{not valid json");
    expect(await loadSettings()).toEqual(defaultSettings());
  });
});

describe("Tauri path", () => {
  beforeEach(() => {
    (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {};
  });

  it("loadSettings reads via get_settings and maps snake_case", async () => {
    invokeSafe.mockResolvedValueOnce({
      scan_folders: ["C:/a"],
      background_enabled: true,
      auto_index: false,
      exclude_rules: [".trash"],
      run_on_boot: true,
      cpu_throttle: "balanced",
      best_copy_mode: "space_saving",
      idle_worker_count: 4,
      cpu_cores: 12,
      partial_clips_enabled: true,
    });
    expect(await loadSettings()).toEqual({
      scanFolders: ["C:/a"],
      backgroundEnabled: true,
      autoIndex: false,
      excludeRules: [".trash"],
      runOnBoot: true,
      cpuThrottle: "balanced",
      bestCopyMode: "space_saving",
      workerCount: 4,
      cpuCores: 12,
      partialClipsEnabled: true,
      indexingEnabled: true,
    });
    expect(invokeSafe).toHaveBeenCalledWith("get_settings");
  });

  it("maps a pre-v13 daemon (no worker fields) to auto / 1 core", async () => {
    invokeSafe.mockResolvedValueOnce({
      scan_folders: [],
      background_enabled: true,
      auto_index: true,
      exclude_rules: [],
      run_on_boot: false,
      cpu_throttle: "full",
      best_copy_mode: "archival",
    });
    const s = await loadSettings();
    expect(s.workerCount).toBeNull();
    expect(s.cpuCores).toBe(1);
  });

  it("saveSettings sends snake_case wire payload and maps the echo back", async () => {
    invokeSafe.mockResolvedValueOnce({
      scan_folders: ["C:/a"],
      background_enabled: false,
      auto_index: true,
      exclude_rules: [],
      run_on_boot: false,
      cpu_throttle: "full",
      best_copy_mode: "archival",
    });
    const result = await saveSettings({
      scanFolders: ["C:/a"],
      backgroundEnabled: false,
      autoIndex: true,
      excludeRules: [],
      runOnBoot: false,
      cpuThrottle: "eco",
      bestCopyMode: "archival",
      workerCount: 2,
      cpuCores: 8,
      partialClipsEnabled: true,
      indexingEnabled: true,
    });
    expect(invokeSafe).toHaveBeenCalledWith("set_settings", {
      settings: {
        scan_folders: ["C:/a"],
        background_enabled: false,
        auto_index: true,
        exclude_rules: [],
        run_on_boot: false,
        cpu_throttle: "eco",
        best_copy_mode: "archival",
        idle_worker_count: 2,
        cpu_cores: 8,
        partial_clips_enabled: true,
        indexing_enabled: true,
      },
    });
    expect(result.scanFolders).toEqual(["C:/a"]);
  });

  it("setIndexingEnabled re-reads live settings so a pause never clobbers scan_folders", async () => {
    invokeSafe.mockResolvedValueOnce({
      scan_folders: ["C:/library"],
      background_enabled: true,
      auto_index: true,
      exclude_rules: [],
      run_on_boot: false,
      cpu_throttle: "full",
      best_copy_mode: "archival",
      idle_worker_count: null,
      cpu_cores: 8,
      partial_clips_enabled: true,
      indexing_enabled: true,
    });
    invokeSafe.mockResolvedValueOnce({
      scan_folders: ["C:/library"],
      background_enabled: true,
      auto_index: true,
      exclude_rules: [],
      run_on_boot: false,
      cpu_throttle: "full",
      best_copy_mode: "archival",
      idle_worker_count: null,
      cpu_cores: 8,
      partial_clips_enabled: true,
      indexing_enabled: false,
    });

    const result = await setIndexingEnabled(false);

    expect(invokeSafe).toHaveBeenNthCalledWith(1, "get_settings");
    expect(invokeSafe).toHaveBeenNthCalledWith(2, "set_settings", {
      settings: expect.objectContaining({
        scan_folders: ["C:/library"],
        indexing_enabled: false,
      }),
    });
    expect(result.indexingEnabled).toBe(false);
  });

  it("addScanFolder appends onto the daemon's fresh list, never dropping existing folders", async () => {
    invokeSafe.mockResolvedValueOnce({
      scan_folders: ["C:/videos"],
      background_enabled: true,
      auto_index: true,
      exclude_rules: [],
      run_on_boot: false,
      cpu_throttle: "full",
      best_copy_mode: "archival",
      idle_worker_count: null,
      cpu_cores: 8,
      partial_clips_enabled: true,
      indexing_enabled: true,
    });
    invokeSafe.mockResolvedValueOnce({
      scan_folders: ["C:/videos", "E:/"],
      background_enabled: true,
      auto_index: true,
      exclude_rules: [],
      run_on_boot: false,
      cpu_throttle: "full",
      best_copy_mode: "archival",
      idle_worker_count: null,
      cpu_cores: 8,
      partial_clips_enabled: true,
      indexing_enabled: true,
    });

    const result = await addScanFolder("E:/");

    expect(invokeSafe).toHaveBeenNthCalledWith(1, "get_settings");
    expect(invokeSafe).toHaveBeenNthCalledWith(2, "set_settings", {
      settings: expect.objectContaining({
        scan_folders: ["C:/videos", "E:/"],
      }),
    });
    expect(result.scanFolders).toEqual([
      "C:/videos",
      "E:/",
    ]);
  });

  it("addScanFolder normalises backslashes and is a no-op for an already-watched folder", async () => {
    invokeSafe.mockResolvedValueOnce({
      scan_folders: ["C:/a"],
      background_enabled: true,
      auto_index: true,
      exclude_rules: [],
      run_on_boot: false,
      cpu_throttle: "full",
      best_copy_mode: "archival",
      indexing_enabled: true,
    });
    invokeSafe.mockResolvedValueOnce({
      scan_folders: ["C:/a", "E:/videos"],
      background_enabled: true,
      auto_index: true,
      exclude_rules: [],
      run_on_boot: false,
      cpu_throttle: "full",
      best_copy_mode: "archival",
      indexing_enabled: true,
    });
    await addScanFolder("E:\\videos");
    expect(invokeSafe).toHaveBeenNthCalledWith(2, "set_settings", {
      settings: expect.objectContaining({ scan_folders: ["C:/a", "E:/videos"] }),
    });

    invokeSafe.mockReset();
    invokeSafe.mockResolvedValueOnce({
      scan_folders: ["C:/a"],
      background_enabled: true,
      auto_index: true,
      exclude_rules: [],
      run_on_boot: false,
      cpu_throttle: "full",
      best_copy_mode: "archival",
      indexing_enabled: true,
    });
    const result = await addScanFolder("C:/a");
    expect(invokeSafe).toHaveBeenCalledTimes(1);
    expect(invokeSafe).toHaveBeenCalledWith("get_settings");
    expect(result.scanFolders).toEqual(["C:/a"]);
  });

  it("removeScanFolder drops only the target, keeping the rest", async () => {
    invokeSafe.mockResolvedValueOnce({
      scan_folders: ["C:/a", "D:/b"],
      background_enabled: true,
      auto_index: true,
      exclude_rules: [],
      run_on_boot: false,
      cpu_throttle: "full",
      best_copy_mode: "archival",
      indexing_enabled: true,
    });
    invokeSafe.mockResolvedValueOnce({
      scan_folders: ["D:/b"],
      background_enabled: true,
      auto_index: true,
      exclude_rules: [],
      run_on_boot: false,
      cpu_throttle: "full",
      best_copy_mode: "archival",
      indexing_enabled: true,
    });
    const result = await removeScanFolder("C:/a");
    expect(invokeSafe).toHaveBeenNthCalledWith(1, "get_settings");
    expect(invokeSafe).toHaveBeenNthCalledWith(2, "set_settings", {
      settings: expect.objectContaining({ scan_folders: ["D:/b"] }),
    });
    expect(result.scanFolders).toEqual(["D:/b"]);
  });
});
