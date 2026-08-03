

export type TrustLevel = "EXACT" | "VERY_LIKELY" | "POSSIBLE";


export interface FileEntry {

  fileId: number;

  path: string;

  sizeBytes: number;

  width: number;

  height: number;

  durationMs: number;

  bitrateBps: number;

  codec: string;

  container: string;

  thumbnailUrl: string | null;
}


export interface DuplicateGroup {

  groupId: number;

  trust: TrustLevel;

  bestFileId: number | null;

  members: FileEntry[];

  introOutro?: boolean;
}


export const TRUST_ORDER: readonly TrustLevel[] = [
  "EXACT",
  "VERY_LIKELY",
  "POSSIBLE",
];


export interface ClusterMember extends FileEntry {

  trust: TrustLevel;

  groupId: number;
}


export interface ContentCluster {

  clusterId: number;

  representativeTrust: TrustLevel;

  bestFileId: number | null;

  members: ClusterMember[];

  introOutro?: boolean;
}


export interface ProgressSnapshot {

  pending: number;

  running: number;

  done: number;

  failed: number;

  cpuUsagePermille?: number;

  rssBytes?: number;

  throughputBytesPerSec?: number;

  pendingBytes?: number;

  currentFiles?: string[];

  partialPending?: number;

  partialRunning?: number;

  partialDone?: number;

  partialSkipped?: Record<string, number>;

  partialFailed?: number;

  folderScanning?: boolean;

  scanDiscovered?: number;

  groupsRevision?: number;
}


export interface FailedTask {

  taskId: number;

  path: string;

  reason: string;

  attempts: number;
}


export interface GroupRole {

  groupId: number;

  trust: TrustLevel;

  isBest: boolean;
}


export interface CrossGroupConflict {

  fileId: number;

  path: string;

  memberships: GroupRole[];
}


export interface ClipOverlap {

  clipFileId: number;

  sourceFileId: number;

  matchedScenes: number;

  clipScenes: number;

  startMs: number;

  endMs: number;

  clipStartMs: number;

  clipEndMs: number;

  introOutro?: boolean;
}
