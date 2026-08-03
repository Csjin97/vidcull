
import { resolveBestFileId } from "../model/best-copy";
import type {
  ClipOverlap,
  DuplicateGroup,
  FileEntry,
  TrustLevel,
} from "../model/types";


function mulberry32(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a |= 0;
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const TRUST_CYCLE: readonly TrustLevel[] = ["EXACT", "VERY_LIKELY", "POSSIBLE"];
const CODECS = ["h264", "hevc", "av1", "vp9"] as const;
const CONTAINERS = ["mp4", "mkv", "webm", "mov"] as const;
const RESOLUTIONS: ReadonlyArray<[number, number]> = [
  [3840, 2160],
  [2560, 1440],
  [1920, 1080],
  [1280, 720],
  [854, 480],
];
const TRUST_HUE: Record<TrustLevel, number> = {
  EXACT: 138, 
  VERY_LIKELY: 28, 
  POSSIBLE: 0, 
};


function thumbDataUrl(label: string, hue: number, variant: number): string {
  const bg = `hsl(${hue} 18% ${14 + (variant % 3) * 4}%)`;
  const fg = `hsl(${hue} 60% 62%)`;
  const svg =
    `<svg xmlns='http://www.w3.org/2000/svg' width='320' height='180'>` +
    `<rect width='320' height='180' fill='${bg}'/>` +
    `<circle cx='160' cy='90' r='34' fill='none' stroke='${fg}' stroke-width='4'/>` +
    `<polygon points='150,74 150,106 178,90' fill='${fg}'/>` +
    `<text x='12' y='168' fill='${fg}' font-family='sans-serif' font-size='14'>${label}</text>` +
    `</svg>`;
  return `data:image/svg+xml;utf8,${encodeURIComponent(svg)}`;
}


export function makeMockGroups(count: number, seed = 0xa5f3): DuplicateGroup[] {
  const rng = mulberry32(seed);
  const groups: DuplicateGroup[] = [];
  let nextFileId = 1;

  for (let g = 0; g < count; g += 1) {
    const trust = TRUST_CYCLE[g % TRUST_CYCLE.length];
    const memberCount = 2 + Math.floor(rng() * 3); 
    const baseResIdx = Math.floor(rng() * (RESOLUTIONS.length - 1));
    const members: FileEntry[] = [];

    for (let m = 0; m < memberCount; m += 1) {
      const fileId = nextFileId++;
      const resIdx = Math.min(RESOLUTIONS.length - 1, baseResIdx + m);
      const [width, height] = RESOLUTIONS[resIdx];
      const durationMs = 60_000 + Math.floor(rng() * 7_200_000);
      const bitrateBps = Math.round((width * height) / 200) * 1000;
      const sizeBytes = Math.round((bitrateBps / 8) * (durationMs / 1000));
      const codec = CODECS[Math.floor(rng() * CODECS.length)];
      const container = CONTAINERS[Math.floor(rng() * CONTAINERS.length)];
      members.push({
        fileId,
        path: `/library/group-${g + 1}/${width}p_${codec}_${fileId}.${container}`,
        sizeBytes,
        width,
        height,
        durationMs,
        bitrateBps,
        codec,
        container,
        thumbnailUrl: thumbDataUrl(`#${fileId}`, TRUST_HUE[trust], m),
      });
    }

    const group: DuplicateGroup = {
      groupId: g + 1,
      trust,
      bestFileId: null,
      members,
    };
    group.bestFileId = trust === "POSSIBLE" ? null : resolveBestFileId(group);
    groups.push(group);
  }

  return groups;
}


export function makeMockClusterGroups(
  count: number,
  seed = 0xc1a5,
): DuplicateGroup[] {
  const rng = mulberry32(seed);
  const groups: DuplicateGroup[] = [];
  let nextFileId = 1;
  let nextGroupId = 1;

  const member = (
    trust: TrustLevel,
    resIdx: number,
    prefix: string,
  ): FileEntry => {
    const fileId = nextFileId++;
    const [width, height] = RESOLUTIONS[Math.min(RESOLUTIONS.length - 1, resIdx)];
    const durationMs = 60_000 + Math.floor(rng() * 7_200_000);
    const bitrateBps = Math.round((width * height) / 200) * 1000;
    const sizeBytes = Math.round((bitrateBps / 8) * (durationMs / 1000));
    const codec = CODECS[Math.floor(rng() * CODECS.length)];
    const container = CONTAINERS[Math.floor(rng() * CONTAINERS.length)];
    return {
      fileId,
      path: `/library/${prefix}/${width}p_${codec}_${fileId}.${container}`,
      sizeBytes,
      width,
      height,
      durationMs,
      bitrateBps,
      codec,
      container,
      thumbnailUrl: thumbDataUrl(`#${fileId}`, TRUST_HUE[trust], fileId),
    };
  };

  for (let c = 0; c < count; c += 1) {
    const baseRes = Math.floor(rng() * (RESOLUTIONS.length - 2));
    const kind = c % 3;
    if (kind === 0) {
      const a = member("EXACT", baseRes, `cluster-${c + 1}`);
      const b = member("EXACT", baseRes + 1, `cluster-${c + 1}`);
      const cc = member("VERY_LIKELY", baseRes + 2, `cluster-${c + 1}`);
      groups.push({
        groupId: nextGroupId++,
        trust: "EXACT",
        bestFileId: a.fileId,
        members: [a, b],
      });
      groups.push({
        groupId: nextGroupId++,
        trust: "VERY_LIKELY",
        bestFileId: b.fileId,
        members: [b, cc],
      });
    } else if (kind === 1) {
      const n = 2 + Math.floor(rng() * 2);
      const members = Array.from({ length: n }, (_, i) =>
        member("VERY_LIKELY", baseRes + i, `cluster-${c + 1}`),
      );
      groups.push({
        groupId: nextGroupId++,
        trust: "VERY_LIKELY",
        bestFileId: members[0].fileId,
        members,
      });
    } else {
      const n = 2 + Math.floor(rng() * 2);
      const members = Array.from({ length: n }, (_, i) =>
        member("POSSIBLE", baseRes + i, `cluster-${c + 1}`),
      );
      groups.push({
        groupId: nextGroupId++,
        trust: "POSSIBLE",
        bestFileId: null,
        members,
      });
    }
  }

  return groups;
}


export function makeMockOverlaps(group: DuplicateGroup): ClipOverlap[] {
  if (group.trust !== "POSSIBLE" || group.members.length < 2) return [];

  const source = group.members.reduce((a, b) =>
    b.durationMs > a.durationMs ? b : a,
  );
  const rng = mulberry32((group.groupId * 0x9e3779b1) >>> 0);
  const SCENES = 10;

  return group.members
    .filter((m) => m.fileId !== source.fileId)
    .map((clip) => {
      const clipLen = Math.min(source.durationMs, clip.durationMs);
      const maxStart = Math.max(0, source.durationMs - clipLen);
      const startMs = Math.floor(rng() * maxStart);
      const matched = Math.min(SCENES, 6 + Math.floor(rng() * (SCENES - 5)));
      const clipStartMs = Math.floor(rng() * (maxStart > 0 ? maxStart / 4 : 1));
      return {
        clipFileId: clip.fileId,
        sourceFileId: source.fileId,
        matchedScenes: matched,
        clipScenes: SCENES,
        startMs,
        endMs: startMs + clipLen,
        clipStartMs,
        clipEndMs: clipStartMs + clipLen,
        introOutro: false,
      };
    });
}
