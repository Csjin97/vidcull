import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeSafe = vi.fn();
vi.mock("../ipc/tauri", () => ({
  invokeSafe: (...args: unknown[]) => invokeSafe(...args),
}));

const { TauriDataSource } = await import("./tauri-datasource");

beforeEach(() => {
  invokeSafe.mockReset();
});

describe("TauriDataSource.progress", () => {
  it("maps the queue counts and defaults absent metrics to 0 (pre-v11)", async () => {
    invokeSafe.mockResolvedValueOnce({
      pending: 5,
      running: 1,
      done: 40,
      failed: 2,
    });
    const ds = new TauriDataSource();
    expect(await ds.progress()).toEqual({
      pending: 5,
      running: 1,
      done: 40,
      failed: 2,
      cpuUsagePermille: 0,
      rssBytes: 0,
      throughputBytesPerSec: 0,
      pendingBytes: 0,
      currentFiles: [],
      partialPending: 0,
      partialRunning: 0,
      partialDone: 0,
      partialSkipped: {},
      partialFailed: 0,
      folderScanning: false,
      scanDiscovered: 0,
      groupsRevision: 0,
    });
    expect(invokeSafe).toHaveBeenCalledWith("daemon_progress");
  });

  it("maps the system-metrics fields from snake_case to camelCase", async () => {
    invokeSafe.mockResolvedValueOnce({
      pending: 0,
      running: 2,
      done: 100,
      failed: 0,
      cpu_usage_permille: 375,
      rss_bytes: 188_743_680,
      throughput_bytes_per_sec: 25_165_824,
      pending_bytes: 1_073_741_824,
      current_files: ["/lib/a.mp4", "/lib/b.mkv"],
    });
    const ds = new TauriDataSource();
    expect(await ds.progress()).toEqual({
      pending: 0,
      running: 2,
      done: 100,
      failed: 0,
      cpuUsagePermille: 375,
      rssBytes: 188_743_680,
      throughputBytesPerSec: 25_165_824,
      pendingBytes: 1_073_741_824,
      currentFiles: ["/lib/a.mp4", "/lib/b.mkv"],
      partialPending: 0,
      partialRunning: 0,
      partialDone: 0,
      partialSkipped: {},
      partialFailed: 0,
      folderScanning: false,
      scanDiscovered: 0,
      groupsRevision: 0,
    });
  });

  it("maps partial_failed through to partialFailed", async () => {
    invokeSafe.mockResolvedValueOnce({
      pending: 0,
      running: 0,
      done: 50,
      failed: 1,
      partial_failed: 3,
    });
    const ds = new TauriDataSource();
    const snap = await ds.progress();
    expect(snap.partialFailed).toBe(3);
    expect(snap.partialSkipped).toEqual({});
  });

  it("maps groups_revision through to groupsRevision, defaulting to 0", async () => {
    invokeSafe.mockResolvedValueOnce({
      pending: 0,
      running: 0,
      done: 50,
      failed: 0,
      groups_revision: 12,
    });
    const ds = new TauriDataSource();
    expect((await ds.progress()).groupsRevision).toBe(12);

    invokeSafe.mockResolvedValueOnce({
      pending: 0,
      running: 0,
      done: 50,
      failed: 0,
    });
    const withoutField = new TauriDataSource();
    expect((await withoutField.progress()).groupsRevision).toBe(0);
  });
});

describe("TauriDataSource.countGroups", () => {
  it("translates the trust enum and returns group_count", async () => {
    invokeSafe.mockResolvedValueOnce({ group_count: 7, reclaimable_bytes: 0 });
    const ds = new TauriDataSource();
    expect(await ds.countGroups("VERY_LIKELY")).toBe(7);
    expect(invokeSafe).toHaveBeenCalledWith("group_stats", {
      trust: "VeryLikely",
    });
  });

  it("sends null trust when unfiltered", async () => {
    invokeSafe.mockResolvedValueOnce({ group_count: 12, reclaimable_bytes: 0 });
    const ds = new TauriDataSource();
    await ds.countGroups();
    expect(invokeSafe).toHaveBeenCalledWith("group_stats", { trust: null });
  });
});

describe("TauriDataSource.reclaimableBytes", () => {
  it("reads the global (unfiltered) reclaimable total", async () => {
    invokeSafe.mockResolvedValueOnce({
      group_count: 3,
      reclaimable_bytes: 123_456,
    });
    const ds = new TauriDataSource();
    expect(await ds.reclaimableBytes()).toBe(123_456);
    expect(invokeSafe).toHaveBeenCalledWith("group_stats", { trust: null });
  });
});

describe("TauriDataSource.listGroups", () => {
  it("composes summaries with per-group member detail", async () => {
    invokeSafe
      .mockResolvedValueOnce([
        {
          group_id: 9,
          trust: "Exact",
          best_file_id: 90,
          member_count: 2,
          intro_outro: false,
        },
      ])
      .mockResolvedValueOnce([
        {
          file_id: 90,
          path: "/v/a.mp4",
          size_bytes: 1_000,
          width: 1920,
          height: 1080,
          duration_ms: 60_000,
          bitrate_bps: 5_000_000,
          codec: "h264",
          container: "mp4",
          is_best: true,
          thumbnail: "data:image/jpeg;base64,/9j/AAAA",
        },
        {
          file_id: 91,
          path: "/v/b.mkv",
          size_bytes: 400,
          width: null,
          height: null,
          duration_ms: null,
          bitrate_bps: null,
          codec: null,
          container: null,
          is_best: false,
          thumbnail: null,
        },
      ]);

    const ds = new TauriDataSource();
    const groups = await ds.listGroups({ limit: 10, offset: 0 });

    expect(groups).toHaveLength(1);
    const g = groups[0];
    expect(g.groupId).toBe(9);
    expect(g.trust).toBe("EXACT");
    expect(g.bestFileId).toBe(90);
    expect(g.members).toHaveLength(2);

    expect(g.members[0]).toEqual({
      fileId: 90,
      path: "/v/a.mp4",
      sizeBytes: 1_000,
      width: 1920,
      height: 1080,
      durationMs: 60_000,
      bitrateBps: 5_000_000,
      codec: "h264",
      container: "mp4",
      thumbnailUrl: "data:image/jpeg;base64,/9j/AAAA",
    });
    expect(g.members[1].width).toBe(0);
    expect(g.members[1].codec).toBe("");
    expect(g.members[1].thumbnailUrl).toBeNull();

    expect(invokeSafe).toHaveBeenNthCalledWith(1, "list_groups", {
      trust: null,
      limit: 10,
      offset: 0,
    });
    expect(invokeSafe).toHaveBeenNthCalledWith(2, "list_group_detail", {
      groupId: 9,
    });
  });

  it("sends the trust filter as a serde variant", async () => {
    invokeSafe.mockResolvedValueOnce([]);
    const ds = new TauriDataSource();
    await ds.listGroups({ trust: "POSSIBLE", limit: 5, offset: 5 });
    expect(invokeSafe).toHaveBeenCalledWith("list_groups", {
      trust: "Possible",
      limit: 5,
      offset: 5,
    });
  });
});

describe("TauriDataSource.listClusters first-paint", () => {
  function memberDetail(fileId: number, isBest: boolean) {
    return {
      file: {
        file_id: fileId,
        path: `/v/${fileId}.mp4`,
        size_bytes: 1_000,
        width: 1920,
        height: 1080,
        duration_ms: 60_000,
        bitrate_bps: 5_000_000,
        codec: "h264",
        container: "mp4",
        is_best: isBest,
        thumbnail: null,
      },
      trust: "Exact",
      group_id: 1,
    };
  }

  it("resolves the cluster list without any per-member thumbnail decode", async () => {
    invokeSafe
      .mockResolvedValueOnce([
        {
          cluster_id: 1,
          representative_trust: "Exact",
          best_file_id: 1,
          member_count: 2,
          member_trust_levels: ["Exact"],
          intro_outro: false,
          members: [memberDetail(1, true), memberDetail(2, false)],
        },
      ]);

    const ds = new TauriDataSource();
    const clusters = await ds.listClusters({ limit: 10, offset: 0 });

    expect(clusters).toHaveLength(1);
    expect(clusters[0].members).toHaveLength(2);
    for (const m of clusters[0].members) {
      expect(m.thumbnailUrl).toBeNull();
    }

    const commands = invokeSafe.mock.calls.map((c) => c[0]);
    expect(commands).toEqual(["list_clusters"]);
    expect(commands).not.toContain("thumbnail");
  });

  it("상세 멤버가 사라진 클러스터를 렌더링 목록에서 제외한다", async () => {
    invokeSafe.mockResolvedValueOnce([
      {
        cluster_id: 9,
        representative_trust: "Exact",
        best_file_id: null,
        member_count: 2,
        member_trust_levels: ["Exact"],
        intro_outro: false,
        members: [],
      },
    ]);

    const ds = new TauriDataSource();
    expect(await ds.listClusters({ limit: 10, offset: 0 })).toEqual([]);
    expect(invokeSafe).toHaveBeenCalledTimes(1);
  });
});

describe("TauriDataSource.fetchThumbnail lazy path", () => {
  it("resolves one member's preview via the dedicated thumbnail RPC", async () => {
    invokeSafe.mockResolvedValueOnce("data:image/jpeg;base64,/9j/LAZY");
    const ds = new TauriDataSource();
    const uri = await ds.fetchThumbnail(42);
    expect(uri).toBe("data:image/jpeg;base64,/9j/LAZY");
    expect(invokeSafe).toHaveBeenCalledWith("thumbnail", { fileId: 42 });
  });

  it("returns null when the daemon can produce no preview", async () => {
    invokeSafe.mockResolvedValueOnce(null);
    const ds = new TauriDataSource();
    expect(await ds.fetchThumbnail(7)).toBeNull();
    expect(invokeSafe).toHaveBeenCalledWith("thumbnail", { fileId: 7 });
  });
});

describe("TauriDataSource.deleteFiles", () => {
  it("maps the wire DeleteResult and forwards mode + confirmBest", async () => {
    invokeSafe.mockResolvedValueOnce({
      ok: true,
      removed_file_ids: [2, 3],
      reclaimed_bytes: 700,
      detail: "2개 파일을 휴지통으로 이동했습니다.",
    });
    const ds = new TauriDataSource();
    const out = await ds.deleteFiles(1, [2, 3], "trash", false);
    expect(out).toEqual({
      ok: true,
      removedFileIds: [2, 3],
      reclaimedBytes: 700,
      detail: "2개 파일을 휴지통으로 이동했습니다.",
      rejectCode: null,
    });
    expect(invokeSafe).toHaveBeenCalledWith("delete_files", {
      groupId: 1,
      fileIds: [2, 3],
      mode: "trash",
      confirmBest: false,
    });
  });

  it("forwards reject_code from a guard rejection (v12+)", async () => {
    invokeSafe.mockResolvedValueOnce({
      ok: false,
      removed_file_ids: [],
      reclaimed_bytes: 0,
      detail: "",
      reject_code: "BEST_UNCONFIRMED",
    });
    const ds = new TauriDataSource();
    const out = await ds.deleteFiles(1, [1], "trash", false);
    expect(out.ok).toBe(false);
    expect(out.rejectCode).toBe("BEST_UNCONFIRMED");
  });

  it("passes permanent mode and the best-copy acknowledgement through", async () => {
    invokeSafe.mockResolvedValueOnce({
      ok: false,
      removed_file_ids: [],
      reclaimed_bytes: 0,
      detail: "거부됨",
    });
    const ds = new TauriDataSource();
    const out = await ds.deleteFiles(9, [1], "permanent", true);
    expect(out.ok).toBe(false);
    expect(out.removedFileIds).toEqual([]);
    expect(invokeSafe).toHaveBeenCalledWith("delete_files", {
      groupId: 9,
      fileIds: [1],
      mode: "permanent",
      confirmBest: true,
    });
  });

  it("defaults confirmBest to false when omitted", async () => {
    invokeSafe.mockResolvedValueOnce({
      ok: true,
      removed_file_ids: [2],
      reclaimed_bytes: 10,
      detail: "",
    });
    const ds = new TauriDataSource();
    await ds.deleteFiles(1, [2], "trash");
    expect(invokeSafe).toHaveBeenCalledWith("delete_files", {
      groupId: 1,
      fileIds: [2],
      mode: "trash",
      confirmBest: false,
    });
  });
});

describe("TauriDataSource.partialOverlaps", () => {
  it("maps wire ClipOverlap rows onto the UI shape", async () => {
    invokeSafe.mockResolvedValueOnce([
      {
        clip_file_id: 43,
        source_file_id: 42,
        matched_scenes: 9,
        clip_scenes: 12,
        start_ms: 30_000,
        end_ms: 90_000,
        clip_start_ms: 0,
        clip_end_ms: 60_000,
        intro_outro: false,
      },
    ]);
    const ds = new TauriDataSource();
    const overlaps = await ds.partialOverlaps(7);
    expect(overlaps).toEqual([
      {
        clipFileId: 43,
        sourceFileId: 42,
        matchedScenes: 9,
        clipScenes: 12,
        startMs: 30_000,
        endMs: 90_000,
        clipStartMs: 0,
        clipEndMs: 60_000,
        introOutro: false,
      },
    ]);
    expect(invokeSafe).toHaveBeenCalledWith("partial_overlaps", { groupId: 7 });
  });

  it("returns an empty list for a non-partial group", async () => {
    invokeSafe.mockResolvedValueOnce([]);
    const ds = new TauriDataSource();
    expect(await ds.partialOverlaps(1)).toEqual([]);
  });
});

describe("TauriDataSource.undoLastDelete", () => {
  it("maps wire UndoResult (snake_case) to camelCase UndoOutcome", async () => {
    invokeSafe.mockResolvedValueOnce({
      ok: true,
      group_id: 7,
      restored_file_ids: [2, 3],
      missing_paths: [],
      detail: "2개 파일을 복원했습니다.",
    });
    const ds = new TauriDataSource();
    const out = await ds.undoLastDelete();
    expect(out).toEqual({
      ok: true,
      groupId: 7,
      restoredFileIds: [2, 3],
      missingPaths: [],
      detail: "2개 파일을 복원했습니다.",
    });
    expect(invokeSafe).toHaveBeenCalledWith("undo_last_delete");
  });

  it("maps ok:false (empty journal) verbatim", async () => {
    invokeSafe.mockResolvedValueOnce({
      ok: false,
      group_id: null,
      restored_file_ids: [],
      missing_paths: [],
      detail: "되돌릴 삭제 내역이 없습니다.",
    });
    const ds = new TauriDataSource();
    const out = await ds.undoLastDelete();
    expect(out.ok).toBe(false);
    expect(out.groupId).toBeNull();
    expect(out.restoredFileIds).toEqual([]);
    expect(invokeSafe).toHaveBeenCalledWith("undo_last_delete");
  });
});

describe("TauriDataSource garbage / transport-failure handling (§M3)", () => {
  it("propagates a transport error so the caller can catch it (no swallow)", async () => {
    invokeSafe.mockRejectedValueOnce(new Error("postcard: unexpected end of input"));
    const ds = new TauriDataSource();
    await expect(ds.progress()).rejects.toThrow("postcard");
  });

  it("rejects with IpcValidationError when the daemon returns a malformed shape", async () => {
    invokeSafe.mockResolvedValueOnce({ unexpected: "shape" });
    const ds = new TauriDataSource();
    await expect(ds.countGroups()).rejects.toThrow("group_count");
  });

  it("rejects from listGroups when the per-group detail call fails mid-page", async () => {
    invokeSafe
      .mockResolvedValueOnce([
        {
          group_id: 1,
          trust: "Exact",
          best_file_id: 1,
          member_count: 2,
          intro_outro: false,
        },
      ])
      .mockRejectedValueOnce(new Error("connection reset"));
    const ds = new TauriDataSource();
    await expect(ds.listGroups({ limit: 10, offset: 0 })).rejects.toThrow(
      "connection reset",
    );
  });
});

describe("TauriDataSource.failedTasks", () => {
  it("maps wire failed-task rows to the UI shape and forwards the limit", async () => {
    invokeSafe.mockResolvedValueOnce([
      {
        task_id: 1002,
        path: "/lib/broken.mkv",
        reason: "decode error",
        attempts: 3,
      },
    ]);
    const ds = new TauriDataSource();
    expect(await ds.failedTasks(50)).toEqual([
      {
        taskId: 1002,
        path: "/lib/broken.mkv",
        reason: "decode error",
        attempts: 3,
      },
    ]);
    expect(invokeSafe).toHaveBeenCalledWith("failed_tasks", { limit: 50 });
  });

  it("returns an empty list when nothing has failed", async () => {
    invokeSafe.mockResolvedValueOnce([]);
    const ds = new TauriDataSource();
    expect(await ds.failedTasks(10)).toEqual([]);
  });
});

describe("TauriDataSource.crossGroupConflicts", () => {
  it("maps wire conflicts (snake_case + trust enum) to the UI shape", async () => {
    invokeSafe.mockResolvedValueOnce([
      {
        file_id: 42,
        path: "/v/shared.mp4",
        memberships: [
          { group_id: 1, trust: "Exact", is_best: true },
          { group_id: 9, trust: "Possible", is_best: false },
        ],
      },
    ]);
    const ds = new TauriDataSource();
    expect(await ds.crossGroupConflicts(1)).toEqual([
      {
        fileId: 42,
        path: "/v/shared.mp4",
        memberships: [
          { groupId: 1, trust: "EXACT", isBest: true },
          { groupId: 9, trust: "POSSIBLE", isBest: false },
        ],
      },
    ]);
    expect(invokeSafe).toHaveBeenCalledWith("cross_group_conflicts", {
      groupId: 1,
    });
  });

  it("returns an empty list when no member is entangled", async () => {
    invokeSafe.mockResolvedValueOnce([]);
    const ds = new TauriDataSource();
    expect(await ds.crossGroupConflicts(5)).toEqual([]);
  });
});
