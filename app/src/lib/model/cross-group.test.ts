import { describe, expect, it } from "vitest";

import { isDangerousToDeleteHere, summarizeConflict } from "./cross-group";
import type { CrossGroupConflict } from "./types";


const conflict: CrossGroupConflict = {
  fileId: 42,
  path: "/v/shared.mp4",
  memberships: [
    { groupId: 1, trust: "EXACT", isBest: true },
    { groupId: 9, trust: "POSSIBLE", isBest: false },
  ],
};

describe("summarizeConflict", () => {
  it("partitions kept vs candidate roles", () => {
    const s = summarizeConflict(conflict, 9);
    expect(s.keptIn.map((r) => r.groupId)).toEqual([1]);
    expect(s.candidateIn.map((r) => r.groupId)).toEqual([9]);
    expect(s.fileId).toBe(42);
    expect(s.path).toBe("/v/shared.mp4");
  });

  it("flags danger when viewing the group where the file is only a candidate", () => {
    const s = summarizeConflict(conflict, 9);
    expect(s.dangerousHere).toBe(true);
    expect(s.keptElsewhere.map((r) => r.groupId)).toEqual([1]);
    expect(s.message).toContain("그룹 1(완전 일치)");
    expect(s.message).toContain("삭제하면");
  });

  it("is not dangerous from the group that itself keeps the file", () => {
    const s = summarizeConflict(conflict, 1);
    expect(s.dangerousHere).toBe(false);
    expect(s.keptElsewhere).toEqual([]);
  });

  it("treats kept-in-two-groups as dangerous from either when one is elsewhere", () => {
    const keptInBoth: CrossGroupConflict = {
      fileId: 7,
      path: "/v/twin.mp4",
      memberships: [
        { groupId: 2, trust: "EXACT", isBest: true },
        { groupId: 3, trust: "VERY_LIKELY", isBest: true },
      ],
    };
    expect(summarizeConflict(keptInBoth, 2).dangerousHere).toBe(true);
    expect(summarizeConflict(keptInBoth, 3).dangerousHere).toBe(true);
  });
});

describe("isDangerousToDeleteHere", () => {
  it("is true for a conflicted file kept by another group", () => {
    expect(isDangerousToDeleteHere([conflict], 42, 9)).toBe(true);
  });

  it("is false for the group that keeps the file itself", () => {
    expect(isDangerousToDeleteHere([conflict], 42, 1)).toBe(false);
  });

  it("is false for a file with no recorded conflict", () => {
    expect(isDangerousToDeleteHere([conflict], 999, 9)).toBe(false);
  });
});
