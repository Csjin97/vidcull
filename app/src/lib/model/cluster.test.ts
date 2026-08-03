import { describe, expect, it } from "vitest";
import {
  clusterAsGroup,
  clusterGroups,
  clusterOverlapGroupIds,
  clustersEqual,
  memberTrustLevels,
  partitionClustersByIntroOutro,
  shouldShowIntroOutroBar,
} from "./cluster";
import type {
  ClusterMember,
  ContentCluster,
  DuplicateGroup,
  FileEntry,
  TrustLevel,
} from "./types";

function file(fileId: number): FileEntry {
  return {
    fileId,
    path: `/v/${fileId}.mp4`,
    sizeBytes: fileId * 1000,
    width: 1920,
    height: 1080,
    durationMs: 60_000,
    bitrateBps: 5_000_000,
    codec: "h264",
    container: "mp4",
    thumbnailUrl: null,
  };
}

function group(
  groupId: number,
  trust: TrustLevel,
  ids: number[],
  bestFileId: number | null = null,
): DuplicateGroup {
  return { groupId, trust, bestFileId, members: ids.map(file) };
}

const memberIds = (cluster: { members: { fileId: number }[] }): number[] =>
  cluster.members.map((m) => m.fileId);
const trustOf = (cluster: { members: { fileId: number; trust: TrustLevel }[] }, id: number) =>
  cluster.members.find((m) => m.fileId === id)?.trust;

describe("clusterGroups", () => {
  it("merges EXACT and VERY_LIKELY groups that share a member", () => {
    const clusters = clusterGroups([
      group(1, "EXACT", [1, 2], 1),
      group(2, "VERY_LIKELY", [2, 3], 2),
    ]);
    expect(clusters).toHaveLength(1);
    expect(memberIds(clusters[0])).toEqual([1, 2, 3]);
    expect(clusters[0].representativeTrust).toBe("EXACT");
    expect(trustOf(clusters[0], 1)).toBe("EXACT");
    expect(trustOf(clusters[0], 2)).toBe("EXACT"); 
    expect(trustOf(clusters[0], 3)).toBe("VERY_LIKELY");
    expect(clusters[0].bestFileId).toBe(1); 
  });

  it("keeps disjoint groups separate (false-positive guard)", () => {
    const clusters = clusterGroups([
      group(1, "EXACT", [1, 2]),
      group(2, "EXACT", [3, 4]),
    ]);
    expect(clusters).toHaveLength(2);
    expect(memberIds(clusters[0])).toEqual([1, 2]);
    expect(memberIds(clusters[1])).toEqual([3, 4]);
  });

  it("keeps a POSSIBLE (partial-clip) group as its own cluster", () => {
    const clusters = clusterGroups([
      group(1, "VERY_LIKELY", [1, 2]),
      group(2, "POSSIBLE", [2, 3]),
    ]);
    expect(clusters).toHaveLength(2);
    const whole = clusters.find((c) => c.representativeTrust === "VERY_LIKELY");
    const clip = clusters.find((c) => c.representativeTrust === "POSSIBLE");
    expect(memberIds(whole!)).toEqual([1, 2]);
    expect(memberIds(clip!)).toEqual([2, 3]);
  });

  it("routes each member's delete through the group at its own trust", () => {
    const clusters = clusterGroups([
      group(10, "EXACT", [1, 2], 1),
      group(11, "VERY_LIKELY", [2, 3], 2),
    ]);
    const byId = (id: number) => clusters[0].members.find((m) => m.fileId === id)!;
    expect(byId(1).groupId).toBe(10); 
    expect(byId(2).groupId).toBe(10); 
    expect(byId(3).groupId).toBe(11); 
  });

  it("orders clusters by smallest contributing group id, members ascending", () => {
    const clusters = clusterGroups([
      group(1, "EXACT", [10, 11]),
      group(2, "EXACT", [1, 2]),
    ]);
    expect(clusters[0].clusterId).toBe(1); 
    expect(memberIds(clusters[0])).toEqual([10, 11]);
    expect(clusters[1].clusterId).toBe(2); 
    expect(memberIds(clusters[1])).toEqual([1, 2]); 
  });

  it("gives a transitive cluster and a POSSIBLE clip a distinct id when they share a smallest member", () => {
    const clusters = clusterGroups([
      group(9, "EXACT", [1, 2, 3], 1), 
      group(5, "POSSIBLE", [1, 4]), 
    ]);
    const transitive = clusters.find((c) => c.representativeTrust === "EXACT")!;
    const possible = clusters.find((c) => c.representativeTrust === "POSSIBLE")!;
    expect(Math.min(...memberIds(transitive))).toBe(1);
    expect(Math.min(...memberIds(possible))).toBe(1);
    expect(transitive.clusterId).not.toBe(possible.clusterId);
    expect(transitive.clusterId).toBe(9); 
    expect(possible.clusterId).toBe(5); 
  });

  it("is deterministic across runs", () => {
    const input = [
      group(2, "VERY_LIKELY", [3, 7]),
      group(1, "EXACT", [1, 3]),
      group(9, "POSSIBLE", [7, 20]),
    ];
    expect(clusterGroups(input)).toEqual(clusterGroups(input));
  });
});

describe("memberTrustLevels", () => {
  it("returns the distinct member trust levels, strongest-first", () => {
    const [cluster] = clusterGroups([
      group(1, "EXACT", [1, 2], 1),
      group(2, "VERY_LIKELY", [2, 3], 2),
    ]);
    expect(memberTrustLevels(cluster)).toEqual(["EXACT", "VERY_LIKELY"]);
  });
});

describe("clusterAsGroup", () => {
  it("projects a cluster onto the DuplicateGroup shape", () => {
    const [cluster] = clusterGroups([group(5, "EXACT", [3, 4], 3)]);
    const projected = clusterAsGroup(cluster);
    expect(projected.groupId).toBe(cluster.clusterId);
    expect(projected.trust).toBe("EXACT");
    expect(projected.members).toBe(cluster.members);
  });
});

describe("clusterOverlapGroupIds", () => {
  function member(fileId: number, groupId: number): ClusterMember {
    return { ...file(fileId), trust: "POSSIBLE", groupId };
  }

  it("returns every unique member routing group, not just the first", () => {
    const cluster: ContentCluster = {
      clusterId: 61,
      representativeTrust: "POSSIBLE",
      bestFileId: null,
      members: [
        member(4, 61),
        member(6, 61),
        member(7, 62),
        member(8, 63),
      ],
    };
    expect([...clusterOverlapGroupIds(cluster)].sort((a, b) => a - b)).toEqual([
      61, 62, 63,
    ]);
  });

  it("dedups a whole-file cluster to its single routing group", () => {
    const cluster: ContentCluster = {
      clusterId: 10,
      representativeTrust: "EXACT",
      bestFileId: 1,
      members: [member(1, 10), member(2, 10), member(3, 10)],
    };
    expect(clusterOverlapGroupIds(cluster)).toEqual([10]);
  });
});

describe("clusterGroups — undefined / Infinity guards", () => {
  it("does not produce an incomplete member when a file id is missing from fileById (Map miss guard)", () => {
    const clusters = clusterGroups([group(1, "EXACT", [10, 20], 10)]);
    expect(clusters).toHaveLength(1);
    const members = clusters[0].members;
    for (const m of members) {
      expect(typeof m.path).toBe("string");
      expect(m.path).not.toBe("undefined");
      expect(typeof m.groupId).toBe("number");
      expect(Number.isFinite(m.groupId)).toBe(true);
    }
  });

  it("does not propagate Infinity as groupId when candidate pool is non-empty", () => {
    const clusters = clusterGroups([
      group(10, "EXACT", [1, 2], 1),
      group(11, "VERY_LIKELY", [2, 3], 2),
    ]);
    for (const cluster of clusters) {
      for (const m of cluster.members) {
        expect(Number.isFinite(m.groupId)).toBe(true);
        expect(m.groupId).not.toBe(Infinity);
      }
    }
  });

  it("repTrust reduce never reads undefined from memberTrust (Map miss guard)", () => {
    const clusters = clusterGroups([group(1, "EXACT", [5, 6, 7], 5)]);
    expect(clusters[0].representativeTrust).toBe("EXACT");
  });
});

describe("partitionClustersByIntroOutro", () => {
  function member(fileId: number, groupId: number): ClusterMember {
    return { ...file(fileId), trust: "POSSIBLE", groupId };
  }

  function cluster(clusterId: number, introOutro?: boolean): ContentCluster {
    return {
      clusterId,
      representativeTrust: "POSSIBLE",
      bestFileId: null,
      members: [member(1, clusterId), member(2, clusterId)],
      introOutro,
    };
  }

  it("puts introOutro:true clusters in `hidden`, everything else in `visible`", () => {
    const tagged = cluster(1, true);
    const explicitFalse = cluster(2, false);
    const omitted = cluster(3); 
    const { visible, hidden } = partitionClustersByIntroOutro([
      tagged,
      explicitFalse,
      omitted,
    ]);
    expect(hidden).toEqual([tagged]);
    expect(visible).toEqual([explicitFalse, omitted]);
  });

  it("never hides a cluster merely because the flag is undefined [recall-safe]", () => {
    const omitted = cluster(1);
    const { visible, hidden } = partitionClustersByIntroOutro([omitted]);
    expect(visible).toEqual([omitted]);
    expect(hidden).toEqual([]);
  });

  it("returns everything visible when no cluster is tagged", () => {
    const a = cluster(1);
    const b = cluster(2, false);
    const { visible, hidden } = partitionClustersByIntroOutro([a, b]);
    expect(visible).toEqual([a, b]);
    expect(hidden).toEqual([]);
  });

  it("returns everything hidden when every cluster is tagged", () => {
    const a = cluster(1, true);
    const b = cluster(2, true);
    const { visible, hidden } = partitionClustersByIntroOutro([a, b]);
    expect(visible).toEqual([]);
    expect(hidden).toEqual([a, b]);
  });

  it("is a no-op on an empty list", () => {
    expect(partitionClustersByIntroOutro([])).toEqual({
      visible: [],
      hidden: [],
    });
  });
});

describe("shouldShowIntroOutroBar", () => {
  it("is false when nothing is hidden (N=0) — no toggle/count clutter", () => {
    expect(shouldShowIntroOutroBar(0)).toBe(false);
  });

  it("is true for any positive hidden count", () => {
    expect(shouldShowIntroOutroBar(1)).toBe(true);
    expect(shouldShowIntroOutroBar(42)).toBe(true);
  });
});

describe("clustersEqual", () => {
  function member(fileId: number, groupId: number): ClusterMember {
    return { ...file(fileId), trust: "POSSIBLE", groupId };
  }

  function cluster(clusterId: number, memberIds: number[] = [1, 2]): ContentCluster {
    return {
      clusterId,
      representativeTrust: "POSSIBLE",
      bestFileId: null,
      members: memberIds.map((id) => member(id, clusterId)),
    };
  }

  it("is true for two empty lists", () => {
    expect(clustersEqual([], [])).toBe(true);
  });

  it("is true for value-identical pages built from independent object graphs", () => {
    const a = [cluster(1, [10, 11]), cluster(2, [20])];
    const b = [cluster(1, [10, 11]), cluster(2, [20])];
    expect(a[0]).not.toBe(b[0]);
    expect(a[0].members[0]).not.toBe(b[0].members[0]);
    expect(clustersEqual(a, b)).toBe(true);
  });

  it("is false when the page length differs", () => {
    expect(clustersEqual([cluster(1)], [])).toBe(false);
    expect(clustersEqual([cluster(1)], [cluster(1), cluster(2)])).toBe(false);
  });

  it("is false when a cluster's id, trust, best copy, or intro/outro flag differs", () => {
    const base = cluster(1);
    expect(clustersEqual([base], [{ ...base, clusterId: 2 }])).toBe(false);
    expect(
      clustersEqual([base], [{ ...base, representativeTrust: "EXACT" }]),
    ).toBe(false);
    expect(clustersEqual([base], [{ ...base, bestFileId: 1 }])).toBe(false);
    expect(clustersEqual([base], [{ ...base, introOutro: true }])).toBe(false);
  });

  it("is false when member order differs (a real reorder must not be swallowed)", () => {
    const a = [cluster(1, [10, 11])];
    const b = [cluster(1, [11, 10])];
    expect(clustersEqual(a, b)).toBe(false);
  });

  it("is false when a member field changes (e.g. a thumbnail resolves)", () => {
    const a = [cluster(1, [10])];
    const withThumb = {
      ...a[0],
      members: [{ ...a[0].members[0], thumbnailUrl: "data:image/jpeg;base64,x" }],
    };
    expect(clustersEqual(a, [withThumb])).toBe(false);
  });
});
