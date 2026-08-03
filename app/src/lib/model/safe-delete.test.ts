import { describe, expect, it } from "vitest";
import {
  assessClusterDeletion,
  assessDeletion,
  clusterSubGroups,
  defaultClusterSelection,
  defaultSelection,
  planBulkDeletion,
} from "./safe-delete";
import type {
  ClusterMember,
  ContentCluster,
  DuplicateGroup,
  FileEntry,
  TrustLevel,
} from "./types";

function file(fileId: number, partial: Partial<FileEntry> = {}): FileEntry {
  return {
    fileId,
    path: `/v/${fileId}.mp4`,
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

function sampleGroup(): DuplicateGroup {
  return {
    groupId: 42,
    trust: "VERY_LIKELY",
    bestFileId: 1,
    members: [
      file(1, { width: 3840, height: 2160, sizeBytes: 4_000_000 }),
      file(2, { width: 1920, height: 1080, sizeBytes: 2_000_000 }),
      file(3, { width: 1280, height: 720, sizeBytes: 1_000_000 }),
    ],
  };
}

describe("defaultSelection", () => {
  it("pre-selects every member except the best copy", () => {
    const sel = defaultSelection(sampleGroup());
    expect([...sel].sort()).toEqual([2, 3]);
  });
});

describe("assessDeletion — safe path", () => {
  it("approves deleting the non-best duplicates and computes reclaimed space", () => {
    const g = sampleGroup();
    const a = assessDeletion(g, new Set([2, 3]), "trash");
    expect(a.canProceed).toBe(true);
    expect(a.issues).toEqual([]);
    expect(a.requiresExtraConfirm).toBe(false);
    expect(a.plan).not.toBeNull();
    expect(a.plan?.toDelete.map((f) => f.fileId)).toEqual([2, 3]);
    expect(a.plan?.kept.map((f) => f.fileId)).toEqual([1]);
    expect(a.plan?.reclaimedBytes).toBe(3_000_000);
    expect(a.plan?.mode).toBe("trash");
  });
});

describe("assessDeletion — accidental-deletion guards", () => {
  it("blocks an empty selection", () => {
    const a = assessDeletion(sampleGroup(), new Set(), "trash");
    expect(a.canProceed).toBe(false);
    expect(a.plan).toBeNull();
    expect(a.issues.map((i) => i.code)).toContain("NONE_SELECTED");
  });

  it("blocks deleting every copy in the group", () => {
    const a = assessDeletion(sampleGroup(), new Set([1, 2, 3]), "trash");
    expect(a.canProceed).toBe(false);
    expect(a.plan).toBeNull();
    const err = a.issues.find((i) => i.code === "DELETE_ALL");
    expect(err?.level).toBe("error");
  });

  it("rejects ids that do not belong to the group", () => {
    const a = assessDeletion(sampleGroup(), new Set([2, 777]), "trash");
    expect(a.canProceed).toBe(false);
    expect(a.issues.map((i) => i.code)).toContain("UNKNOWN_MEMBER");
  });
});

describe("assessDeletion — deliberate but loud paths", () => {
  it("warns (does not block) when the best copy is selected, and forces extra confirm", () => {
    const a = assessDeletion(sampleGroup(), new Set([1, 2]), "trash");
    expect(a.canProceed).toBe(true); 
    const warn = a.issues.find((i) => i.code === "DELETE_BEST");
    expect(warn?.level).toBe("warning"); 
    expect(a.requiresExtraConfirm).toBe(true); 
  });

  it("always requires an extra confirm for permanent deletion", () => {
    const a = assessDeletion(sampleGroup(), new Set([2, 3]), "permanent");
    expect(a.canProceed).toBe(true);
    expect(a.requiresExtraConfirm).toBe(true);
    expect(a.issues.map((i) => i.code)).toContain("PERMANENT");
  });

  it("stacks permanent + best-copy warnings", () => {
    const a = assessDeletion(sampleGroup(), new Set([1, 2]), "permanent");
    const codes = a.issues.map((i) => i.code);
    expect(codes).toContain("DELETE_BEST");
    expect(codes).toContain("PERMANENT");
    expect(a.requiresExtraConfirm).toBe(true);
  });
});


function clusterMember(
  fileId: number,
  trust: TrustLevel,
  groupId: number,
  partial: Partial<FileEntry> = {},
): ClusterMember {
  return { ...file(fileId, partial), trust, groupId };
}

function sampleCluster(): ContentCluster {
  return {
    clusterId: 1,
    representativeTrust: "EXACT",
    bestFileId: 1,
    members: [
      clusterMember(1, "EXACT", 10, { width: 3840, height: 2160, sizeBytes: 4_000_000 }),
      clusterMember(2, "EXACT", 10, { width: 1920, height: 1080, sizeBytes: 2_000_000 }),
      clusterMember(3, "VERY_LIKELY", 11, { width: 1920, height: 1080, sizeBytes: 2_000_000 }),
      clusterMember(4, "VERY_LIKELY", 11, { width: 1280, height: 720, sizeBytes: 1_000_000 }),
    ],
  };
}

describe("clusterSubGroups", () => {
  it("partitions members by routing group, keeping the rep best with its group", () => {
    const groups = clusterSubGroups(sampleCluster());
    expect(groups.map((g) => g.groupId)).toEqual([10, 11]);
    expect(groups[0].bestFileId).toBe(1); 
    expect(groups[0].members.map((m) => m.fileId)).toEqual([1, 2]);
    expect(groups[1].bestFileId).toBeNull(); 
    expect(groups[1].members.map((m) => m.fileId)).toEqual([3, 4]);
  });
});

describe("defaultClusterSelection", () => {
  it("spares EACH sub-group's best, not just the representative", () => {
    const sel = defaultClusterSelection(sampleCluster());
    expect([...sel].sort((a, b) => a - b)).toEqual([2, 4]);
  });
});

describe("assessClusterDeletion (§L2b)", () => {
  it("approves deleting each sub-group's non-best and sums reclaimed bytes", () => {
    const a = assessClusterDeletion(sampleCluster(), new Set([2, 4]), "trash");
    expect(a.canProceed).toBe(true);
    expect(a.issues).toEqual([]);
    expect(a.plan?.toDelete.map((f) => f.fileId).sort((x, y) => x - y)).toEqual([
      2, 4,
    ]);
    expect(a.plan?.reclaimedBytes).toBe(3_000_000); 
  });

  it("warns when a NON-representative sub-group's best is selected (the §L2b fix)", () => {
    const a = assessClusterDeletion(sampleCluster(), new Set([2, 3]), "trash");
    expect(a.canProceed).toBe(true); 
    expect(a.issues.map((i) => i.code)).toContain("DELETE_BEST"); 
    expect(a.requiresExtraConfirm).toBe(true);
  });

  it("blocks emptying any one sub-group even when others are fine", () => {
    const a = assessClusterDeletion(sampleCluster(), new Set([3, 4]), "trash");
    expect(a.canProceed).toBe(false);
    expect(a.plan).toBeNull();
    expect(a.issues.map((i) => i.code)).toContain("DELETE_ALL");
  });

  it("rejects ids outside the cluster", () => {
    const a = assessClusterDeletion(sampleCluster(), new Set([2, 999]), "trash");
    expect(a.canProceed).toBe(false);
    expect(a.issues.map((i) => i.code)).toContain("UNKNOWN_MEMBER");
  });

  it("dedupes the permanent warning across sub-groups and gates an extra confirm", () => {
    const a = assessClusterDeletion(sampleCluster(), new Set([2, 4]), "permanent");
    expect(a.canProceed).toBe(true);
    expect(a.issues.filter((i) => i.code === "PERMANENT")).toHaveLength(1);
    expect(a.requiresExtraConfirm).toBe(true);
  });

  it("blocks an empty selection", () => {
    const a = assessClusterDeletion(sampleCluster(), new Set(), "trash");
    expect(a.canProceed).toBe(false);
    expect(a.issues.map((i) => i.code)).toContain("NONE_SELECTED");
  });
});

function otherCluster(): ContentCluster {
  return {
    clusterId: 2,
    representativeTrust: "POSSIBLE",
    bestFileId: 10,
    members: [
      clusterMember(10, "POSSIBLE", 20, { sizeBytes: 5_000_000 }),
      clusterMember(11, "POSSIBLE", 20, { sizeBytes: 3_000_000 }),
    ],
  };
}

function singletonCluster(): ContentCluster {
  return {
    clusterId: 3,
    representativeTrust: "POSSIBLE",
    bestFileId: null,
    members: [clusterMember(99, "POSSIBLE", 900)],
  };
}

describe("planBulkDeletion", () => {
  it("applies the default (keep-best) selection across every selected cluster", () => {
    const plan = planBulkDeletion([sampleCluster(), otherCluster()], "trash");
    expect(plan.clusterCount).toBe(2);
    expect(plan.skippedClusterIds).toEqual([]);
    expect(plan.perCluster.map((p) => p.cluster.clusterId)).toEqual([1, 2]);
    expect([...plan.perCluster[0].selected].sort((a, b) => a - b)).toEqual([
      2, 4,
    ]);
    expect([...plan.perCluster[1].selected]).toEqual([11]);
    expect(plan.toDelete.map((f) => f.fileId).sort((a, b) => a - b)).toEqual([
      2, 4, 11,
    ]);
    expect(plan.reclaimedBytes).toBe(3_000_000 + 3_000_000);
  });

  it("skips a cluster with nothing safely deletable instead of blocking the rest", () => {
    const plan = planBulkDeletion(
      [sampleCluster(), singletonCluster()],
      "trash",
    );
    expect(plan.skippedClusterIds).toEqual([3]);
    expect(plan.perCluster.map((p) => p.cluster.clusterId)).toEqual([1]);
    expect(plan.toDelete.map((f) => f.fileId).sort((a, b) => a - b)).toEqual([
      2, 4,
    ]);
  });

  it("returns an empty plan for an empty cluster list", () => {
    const plan = planBulkDeletion([], "trash");
    expect(plan.clusterCount).toBe(0);
    expect(plan.perCluster).toEqual([]);
    expect(plan.toDelete).toEqual([]);
    expect(plan.reclaimedBytes).toBe(0);
  });
});
