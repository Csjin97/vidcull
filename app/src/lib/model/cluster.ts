
import type {
  ClusterMember,
  ContentCluster,
  DuplicateGroup,
  FileEntry,
  TrustLevel,
} from "./types";
import { TRUST_ORDER } from "./types";

const STRENGTH: Record<TrustLevel, number> = {
  EXACT: 3,
  VERY_LIKELY: 2,
  POSSIBLE: 1,
};


function isTransitive(trust: TrustLevel): boolean {
  return trust === "EXACT" || trust === "VERY_LIKELY";
}


function stronger(a: TrustLevel, b: TrustLevel): TrustLevel {
  return STRENGTH[b] > STRENGTH[a] ? b : a;
}


class DisjointSet {
  private parent = new Map<number, number>();

  add(id: number): void {
    if (!this.parent.has(id)) this.parent.set(id, id);
  }

  find(id: number): number {
    let root = id;
    while (this.parent.get(root) !== root) root = this.parent.get(root) as number;
    let cur = id;
    while (this.parent.get(cur) !== root) {
      const next = this.parent.get(cur) as number;
      this.parent.set(cur, root);
      cur = next;
    }
    return root;
  }

  union(a: number, b: number): void {
    const ra = this.find(a);
    const rb = this.find(b);
    if (ra !== rb) this.parent.set(ra, rb);
  }
}


export function clusterGroups(
  groups: readonly DuplicateGroup[],
): ContentCluster[] {
  const fileById = new Map<number, FileEntry>();
  for (const group of groups) {
    for (const member of group.members) fileById.set(member.fileId, member);
  }

  const transitive = groups.filter((g) => isTransitive(g.trust));
  const dsu = new DisjointSet();
  for (const group of transitive) {
    for (const member of group.members) dsu.add(member.fileId);
    const ids = group.members.map((m) => m.fileId);
    for (let i = 1; i < ids.length; i += 1) dsu.union(ids[0], ids[i]);
  }

  const memberTrust = new Map<number, TrustLevel>();
  const memberGroups = new Map<number, Array<{ groupId: number; trust: TrustLevel }>>();
  for (const group of transitive) {
    for (const member of group.members) {
      const cur = memberTrust.get(member.fileId);
      memberTrust.set(member.fileId, cur ? stronger(cur, group.trust) : group.trust);
      const list = memberGroups.get(member.fileId) ?? [];
      list.push({ groupId: group.groupId, trust: group.trust });
      memberGroups.set(member.fileId, list);
    }
  }

  const membersByRoot = new Map<number, number[]>();
  for (const id of memberTrust.keys()) {
    const root = dsu.find(id);
    const list = membersByRoot.get(root) ?? [];
    list.push(id);
    membersByRoot.set(root, list);
  }
  const groupsByRoot = new Map<number, DuplicateGroup[]>();
  for (const group of transitive) {
    const root = dsu.find(group.members[0].fileId);
    const list = groupsByRoot.get(root) ?? [];
    list.push(group);
    groupsByRoot.set(root, list);
  }

  const clusters: ContentCluster[] = [];

  for (const [root, ids] of membersByRoot) {
    ids.sort((a, b) => a - b);
    const contributing = groupsByRoot.get(root) ?? [];
    const repTrust = ids
      .reduce<TrustLevel>((acc, id) => {
        const t = memberTrust.get(id);
        return t ? stronger(acc, t) : acc;
      }, "POSSIBLE");
    const members = ids
      .map((id) => toMember(fileById, memberGroups, id))
      .filter((m): m is ClusterMember => m !== null);
    clusters.push({
      clusterId: contributing.length
        ? Math.min(...contributing.map((g) => g.groupId))
        : ids[0],
      representativeTrust: repTrust,
      bestFileId: representativeBest(contributing, repTrust),
      members,
    });
  }

  for (const group of groups.filter((g) => !isTransitive(g.trust))) {
    const members: ClusterMember[] = [...group.members]
      .sort((a, b) => a.fileId - b.fileId)
      .map((file) => ({ ...file, trust: group.trust, groupId: group.groupId }));
    clusters.push({
      clusterId: group.groupId,
      representativeTrust: group.trust,
      bestFileId: group.bestFileId,
      members,
    });
  }

  clusters.sort((a, b) => a.clusterId - b.clusterId);
  return clusters;
}


function toMember(
  fileById: Map<number, FileEntry>,
  memberGroups: Map<number, Array<{ groupId: number; trust: TrustLevel }>>,
  id: number,
): ClusterMember | null {
  const file = fileById.get(id);
  if (file === undefined) {
    console.warn(`[cluster] toMember: fileById missing entry for id=${id}`);
    return null;
  }
  const trust = pickTrust(memberGroups.get(id) ?? []);
  const candidates = memberGroups.get(id) ?? [];
  const atTrust = candidates
    .filter((c) => c.trust === trust)
    .map((c) => c.groupId);
  const any = candidates.map((c) => c.groupId);
  const pool = atTrust.length ? atTrust : any;
  if (pool.length === 0) {
    console.warn(`[cluster] toMember: no candidate groups for id=${id}`);
    return null;
  }
  const groupId = Math.min(...pool);
  return { ...file, trust, groupId };
}


function pickTrust(groups: Array<{ trust: TrustLevel }>): TrustLevel {
  return groups.reduce<TrustLevel>(
    (acc, g) => stronger(acc, g.trust),
    "POSSIBLE",
  );
}


function representativeBest(
  groups: readonly DuplicateGroup[],
  repTrust: TrustLevel,
): number | null {
  const rep = groups
    .filter((g) => g.trust === repTrust && g.bestFileId !== null)
    .sort((a, b) => a.groupId - b.groupId)[0];
  return rep ? rep.bestFileId : null;
}


export function memberTrustLevels(cluster: ContentCluster): TrustLevel[] {
  return TRUST_ORDER.filter((t) => cluster.members.some((m) => m.trust === t));
}


export function clusterAsGroup(cluster: ContentCluster): DuplicateGroup {
  return {
    groupId: cluster.clusterId,
    trust: cluster.representativeTrust,
    bestFileId: cluster.bestFileId,
    members: cluster.members,
  };
}


export function clusterOverlapGroupIds(cluster: ContentCluster): number[] {
  return [...new Set(cluster.members.map((m) => m.groupId))];
}


export interface ClusterIntroOutroPartition {

  visible: ContentCluster[];

  hidden: ContentCluster[];
}

export function partitionClustersByIntroOutro(
  clusters: readonly ContentCluster[],
): ClusterIntroOutroPartition {
  const visible: ContentCluster[] = [];
  const hidden: ContentCluster[] = [];
  for (const c of clusters) {
    if (c.introOutro === true) {
      hidden.push(c);
    } else {
      visible.push(c);
    }
  }
  return { visible, hidden };
}


export function shouldShowIntroOutroBar(hiddenCount: number): boolean {
  return hiddenCount > 0;
}


function fileEntryEqual(a: FileEntry, b: FileEntry): boolean {
  return (
    a.fileId === b.fileId &&
    a.path === b.path &&
    a.sizeBytes === b.sizeBytes &&
    a.width === b.width &&
    a.height === b.height &&
    a.durationMs === b.durationMs &&
    a.bitrateBps === b.bitrateBps &&
    a.codec === b.codec &&
    a.container === b.container &&
    a.thumbnailUrl === b.thumbnailUrl
  );
}


function clusterMemberEqual(a: ClusterMember, b: ClusterMember): boolean {
  return a.trust === b.trust && a.groupId === b.groupId && fileEntryEqual(a, b);
}


export function clustersEqual(
  a: readonly ContentCluster[],
  b: readonly ContentCluster[],
): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i += 1) {
    const ca = a[i];
    const cb = b[i];
    if (
      ca.clusterId !== cb.clusterId ||
      ca.representativeTrust !== cb.representativeTrust ||
      ca.bestFileId !== cb.bestFileId ||
      ca.introOutro !== cb.introOutro ||
      ca.members.length !== cb.members.length
    ) {
      return false;
    }
    for (let j = 0; j < ca.members.length; j += 1) {
      if (!clusterMemberEqual(ca.members[j], cb.members[j])) return false;
    }
  }
  return true;
}
