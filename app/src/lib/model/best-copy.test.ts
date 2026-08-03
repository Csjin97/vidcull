import { describe, expect, it } from "vitest";
import {
  clientBestFileId,
  compareQuality,
  isBest,
  membersByQuality,
  resolveBestFileId,
} from "./best-copy";
import type { DuplicateGroup, FileEntry } from "./types";

function file(partial: Partial<FileEntry> & { fileId: number }): FileEntry {
  return {
    path: `/v/${partial.fileId}.mp4`,
    sizeBytes: 1_000_000,
    width: 1920,
    height: 1080,
    durationMs: 60_000,
    bitrateBps: 5_000_000,
    codec: "h264",
    container: "mp4",
    thumbnailUrl: null,
    ...partial,
  };
}

function group(members: FileEntry[], bestFileId: number | null): DuplicateGroup {
  return { groupId: 1, trust: "VERY_LIKELY", bestFileId, members };
}

describe("compareQuality", () => {
  it("prefers higher resolution first", () => {
    const lo = file({ fileId: 1, width: 1280, height: 720 });
    const hi = file({ fileId: 2, width: 3840, height: 2160 });
    expect(compareQuality(hi, lo)).toBeLessThan(0);
    expect([lo, hi].sort(compareQuality)[0]).toBe(hi);
  });

  it("breaks a resolution tie on bitrate, then size, then fileId", () => {
    const a = file({ fileId: 5, bitrateBps: 8_000_000, sizeBytes: 10 });
    const b = file({ fileId: 6, bitrateBps: 4_000_000, sizeBytes: 99 });
    expect(compareQuality(a, b)).toBeLessThan(0); 

    const c = file({ fileId: 7, bitrateBps: 4_000_000, sizeBytes: 200 });
    const d = file({ fileId: 8, bitrateBps: 4_000_000, sizeBytes: 100 });
    expect(compareQuality(c, d)).toBeLessThan(0); 

    const e = file({ fileId: 3, bitrateBps: 4_000_000, sizeBytes: 100 });
    const f = file({ fileId: 9, bitrateBps: 4_000_000, sizeBytes: 100 });
    expect(compareQuality(e, f)).toBeLessThan(0); 
  });

  it("applies codec efficiency boost in space_saving mode, but not in archival mode", () => {
    const a = file({ fileId: 1, bitrateBps: 8_000_000, codec: "h264" });
    const b = file({ fileId: 2, bitrateBps: 6_000_000, codec: "h265" });

    expect(compareQuality(a, b, "archival")).toBeLessThan(0);

    expect(compareQuality(b, a, "space_saving")).toBeLessThan(0);
  });
});

describe("resolveBestFileId", () => {
  it("uses the server pick when it names a present member", () => {
    const g = group([file({ fileId: 1 }), file({ fileId: 2 })], 2);
    expect(resolveBestFileId(g)).toBe(2);
    expect(isBest(g, 2)).toBe(true);
    expect(isBest(g, 1)).toBe(false);
  });

  it("falls back to the client heuristic when the server has not picked", () => {
    const lo = file({ fileId: 1, width: 1280, height: 720 });
    const hi = file({ fileId: 2, width: 3840, height: 2160 });
    const g = group([lo, hi], null);
    expect(resolveBestFileId(g)).toBe(2);
  });

  it("ignores a stale server pick that is no longer a member", () => {
    const g = group([file({ fileId: 1 }), file({ fileId: 2 })], 999);
    expect(resolveBestFileId(g)).toBe(1);
  });

  it("returns null for an empty group", () => {
    expect(clientBestFileId([])).toBeNull();
    expect(resolveBestFileId(group([], null))).toBeNull();
  });
});

describe("membersByQuality", () => {
  it("orders members best-first without mutating the group", () => {
    const lo = file({ fileId: 1, width: 1280, height: 720 });
    const hi = file({ fileId: 2, width: 3840, height: 2160 });
    const g = group([lo, hi], null);
    expect(membersByQuality(g).map((m) => m.fileId)).toEqual([2, 1]);
    expect(g.members.map((m) => m.fileId)).toEqual([1, 2]); 
  });
});
