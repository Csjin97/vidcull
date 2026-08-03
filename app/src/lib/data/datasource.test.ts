import { describe, expect, it } from "vitest";
import { MockDataSource, reclaimableAcross } from "./datasource";
import { makeMockGroups } from "./mock-data";
import type { DuplicateGroup, FileEntry } from "../model/types";

function file(fileId: number, sizeBytes: number, best = false): FileEntry {
  return {
    fileId,
    path: `/v/${fileId}.mp4`,
    sizeBytes,
    width: best ? 3840 : 1280,
    height: best ? 2160 : 720,
    durationMs: 60_000,
    bitrateBps: 5_000_000,
    codec: "h264",
    container: "mp4",
    thumbnailUrl: null,
  };
}

function group(groupId: number, members: FileEntry[]): DuplicateGroup {
  return { groupId, trust: "VERY_LIKELY", bestFileId: members[0].fileId, members };
}

describe("makeMockGroups determinism", () => {
  it("produces identical output for the same seed", () => {
    expect(makeMockGroups(20, 123)).toEqual(makeMockGroups(20, 123));
  });

  it("differs for a different seed", () => {
    expect(makeMockGroups(20, 1)).not.toEqual(makeMockGroups(20, 2));
  });

  it("every group has ≥2 members and a thumbnail per file", () => {
    for (const g of makeMockGroups(30)) {
      expect(g.members.length).toBeGreaterThanOrEqual(2);
      for (const m of g.members) {
        expect(m.thumbnailUrl).toMatch(/^data:image\/svg\+xml/);
      }
    }
  });
});

describe("reclaimableAcross", () => {
  it("sums every member beyond the best, per group", () => {
    const groups = [
      group(1, [file(1, 100), file(2, 40), file(3, 10)]),
      group(2, [file(4, 50), file(5, 20)]),
    ];
    expect(reclaimableAcross(groups)).toBe(70);
  });

  it("uses bestFileId to keep the chosen copy, even if it is smaller than other members", () => {
    const g = group(1, [file(1, 40), file(2, 100)]);
    g.bestFileId = 1;
    expect(reclaimableAcross([g])).toBe(100);
  });
});

describe("MockDataSource paging & filtering", () => {
  it("pages and filters by trust", async () => {
    const ds = new MockDataSource(makeMockGroups(30));
    const firstPage = await ds.listGroups({ limit: 10, offset: 0 });
    const secondPage = await ds.listGroups({ limit: 10, offset: 10 });
    expect(firstPage).toHaveLength(10);
    expect(secondPage).toHaveLength(10);
    expect(firstPage[0].groupId).not.toBe(secondPage[0].groupId);

    const exactCount = await ds.countGroups("EXACT");
    const exact = await ds.listGroups({ trust: "EXACT", limit: 100, offset: 0 });
    expect(exact.every((g) => g.trust === "EXACT")).toBe(true);
    expect(exact).toHaveLength(exactCount);
  });
});

describe("MockDataSource deleteFiles re-validates safety", () => {
  it("removes the selected duplicates and reports reclaimed bytes", async () => {
    const ds = new MockDataSource([
      group(1, [file(1, 100, true), file(2, 40), file(3, 10)]),
    ]);
    const out = await ds.deleteFiles(1, [2, 3], "trash");
    expect(out.ok).toBe(true);
    expect(out.removedFileIds.sort()).toEqual([2, 3]);
    expect(out.reclaimedBytes).toBe(50);

    expect(await ds.countGroups()).toBe(0);
  });

  it("refuses a delete-all request even if the UI sends one", async () => {
    const ds = new MockDataSource([
      group(1, [file(1, 100, true), file(2, 40)]),
    ]);
    const out = await ds.deleteFiles(1, [1, 2], "trash");
    expect(out.ok).toBe(false);
    expect(out.removedFileIds).toEqual([]);
    expect(await ds.countGroups()).toBe(1); 
  });

  it("reports a not-found group", async () => {
    const ds = new MockDataSource([]);
    const out = await ds.deleteFiles(99, [1], "trash");
    expect(out.ok).toBe(false);
    expect(out.detail).toContain("99");
  });
});

describe("MockDataSource undoLastDelete", () => {
  it("restores groups and members after a delete", async () => {
    const ds = new MockDataSource([
      group(1, [file(1, 100, true), file(2, 40), file(3, 10)]),
    ]);
    await ds.deleteFiles(1, [2, 3], "trash");
    expect(await ds.countGroups()).toBe(0); 

    const out = await ds.undoLastDelete();
    expect(out.ok).toBe(true);
    expect(out.restoredFileIds.sort()).toEqual([2, 3]);
    expect(out.missingPaths).toEqual([]);
    expect(out.groupId).toBe(1);

    expect(await ds.countGroups()).toBe(1);
    const [restored] = await ds.listGroups({ limit: 10, offset: 0 });
    expect(restored.members.map((m) => m.fileId).sort()).toEqual([1, 2, 3]);
  });

  it("returns ok:false when the undo stack is empty", async () => {
    const ds = new MockDataSource([
      group(1, [file(1, 100, true), file(2, 40)]),
    ]);
    const out = await ds.undoLastDelete();
    expect(out.ok).toBe(false);
    expect(out.groupId).toBeNull();
    expect(out.restoredFileIds).toEqual([]);
    expect(out.detail).toContain("없습니다");
  });

  it("two deletes then two undos restores in reverse order", async () => {
    const ds = new MockDataSource([
      group(1, [file(1, 100, true), file(2, 40), file(3, 10)]),
    ]);
    await ds.deleteFiles(1, [2], "trash");
    await ds.deleteFiles(1, [3], "trash");

    const out1 = await ds.undoLastDelete();
    expect(out1.ok).toBe(true);
    expect(out1.restoredFileIds).toEqual([3]);
    let [g] = await ds.listGroups({ limit: 10, offset: 0 });
    expect(g.members.map((m) => m.fileId).sort()).toEqual([1, 3]);

    const out2 = await ds.undoLastDelete();
    expect(out2.ok).toBe(true);
    expect(out2.restoredFileIds).toEqual([2]);
    [g] = await ds.listGroups({ limit: 10, offset: 0 });
    expect(g.members.map((m) => m.fileId).sort()).toEqual([1, 2, 3]);

    const out3 = await ds.undoLastDelete();
    expect(out3.ok).toBe(false);
  });
});

describe("MockDataSource crossGroupConflicts mirrors the daemon rule", () => {
  const shared = file(1, 100, true);
  const conflictedGroups: DuplicateGroup[] = [
    group(1, [shared, file(2, 40)]), 
    group(2, [file(3, 200), shared]), 
  ];

  it("reports a file kept here but a candidate in another group", async () => {
    const ds = new MockDataSource(conflictedGroups);
    const conflicts = await ds.crossGroupConflicts(1);
    expect(conflicts).toHaveLength(1);
    expect(conflicts[0].fileId).toBe(1);
    expect(conflicts[0].memberships).toEqual([
      { groupId: 1, trust: "VERY_LIKELY", isBest: true },
      { groupId: 2, trust: "VERY_LIKELY", isBest: false },
    ]);
  });

  it("reports the shared file from whichever group is being viewed", async () => {
    const ds = new MockDataSource(conflictedGroups);
    const conflicts = await ds.crossGroupConflicts(2);
    expect(conflicts.map((c) => c.fileId)).toEqual([1]);
  });

  it("returns nothing when no member spans groups", async () => {
    const ds = new MockDataSource([group(1, [file(1, 100, true), file(2, 40)])]);
    expect(await ds.crossGroupConflicts(1)).toEqual([]);
  });

  it("ignores benign multi-membership with no kept/candidate split", async () => {
    const benign: DuplicateGroup[] = [
      group(1, [file(2, 200), file(1, 100)]),
      group(2, [file(3, 300), file(1, 100)]),
    ];
    const ds = new MockDataSource(benign);
    expect(await ds.crossGroupConflicts(1)).toEqual([]);
  });

  it("returns an empty list for an unknown group", async () => {
    const ds = new MockDataSource(conflictedGroups);
    expect(await ds.crossGroupConflicts(999)).toEqual([]);
  });
});
