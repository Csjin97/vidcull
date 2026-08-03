import { fireEvent, render, screen } from "@testing-library/svelte";
import { describe, expect, it, vi } from "vitest";
import ClusterCard from "./ClusterCard.svelte";
import { trustLabel } from "$lib/model/format";
import type { ClusterMember, ContentCluster } from "$lib/model/types";
import type { TrustLevel } from "$lib/model/types";

function member(
  fileId: number,
  trust: TrustLevel,
  groupId: number,
  w = 1920,
  h = 1080,
): ClusterMember {
  return {
    fileId,
    path: `/v/${fileId}.mp4`,
    sizeBytes: fileId * 1024 * 1024,
    width: w,
    height: h,
    durationMs: 60_000,
    bitrateBps: 5_000_000,
    codec: "h264",
    container: "mp4",
    thumbnailUrl: `data:image/svg+xml;utf8,<svg/>`,
    trust,
    groupId,
  };
}

const mixed: ContentCluster = {
  clusterId: 1,
  representativeTrust: "EXACT",
  bestFileId: 1,
  members: [
    member(1, "EXACT", 10, 3840, 2160),
    member(2, "EXACT", 10, 1920, 1080),
    member(3, "VERY_LIKELY", 11, 1280, 720),
  ],
};

describe("ClusterCard render", () => {
  it("빈 멤버 목록에서도 width 오류 없이 갱신 상태를 표시한다", () => {
    render(ClusterCard, {
      props: {
        cluster: {
          clusterId: 99,
          representativeTrust: "EXACT",
          bestFileId: null,
          members: [],
        },
      },
    });
    expect(screen.getByText("파일 정보 갱신 중")).toBeInTheDocument();
  });

  it("shows a badge for EVERY distinct member trust level simultaneously", () => {
    render(ClusterCard, { props: { cluster: mixed } });
    expect(screen.getByText(trustLabel("EXACT"))).toBeInTheDocument();
    expect(screen.getByText(trustLabel("VERY_LIKELY"))).toBeInTheDocument();
    expect(screen.queryByText(trustLabel("POSSIBLE"))).not.toBeInTheDocument();
  });

  it("renders file count and reclaimable size", () => {
    render(ClusterCard, { props: { cluster: mixed } });
    expect(screen.getByText("3개 파일")).toBeInTheDocument();
    expect(screen.getByText("정리 시 회수 5.0 MB")).toBeInTheDocument();
  });

  it("renders one thumbnail per member (capped at four)", () => {
    render(ClusterCard, { props: { cluster: mixed } });
    expect(screen.getAllByRole("img")).toHaveLength(3);
  });

  it("fires onselect with the cluster on click", async () => {
    const onselect = vi.fn();
    render(ClusterCard, { props: { cluster: mixed, onselect } });
    await fireEvent.click(screen.getByRole("button"));
    expect(onselect).toHaveBeenCalledWith(mixed);
  });

  it("shows a single badge for a uniform POSSIBLE cluster", () => {
    const clip: ContentCluster = {
      clusterId: 5,
      representativeTrust: "POSSIBLE",
      bestFileId: null,
      members: [member(5, "POSSIBLE", 20), member(6, "POSSIBLE", 20)],
    };
    render(ClusterCard, { props: { cluster: clip } });
    expect(screen.getByText(trustLabel("POSSIBLE"))).toBeInTheDocument();
    expect(screen.queryByText(trustLabel("EXACT"))).not.toBeInTheDocument();
  });
});

describe("ClusterCard intro/outro suspicion badge", () => {
  function possibleCluster(introOutro?: boolean): ContentCluster {
    return {
      clusterId: 7,
      representativeTrust: "POSSIBLE",
      bestFileId: null,
      members: [member(7, "POSSIBLE", 30), member(8, "POSSIBLE", 30)],
      introOutro,
    };
  }

  it("shows the badge when introOutro is true", () => {
    render(ClusterCard, { props: { cluster: possibleCluster(true) } });
    expect(screen.getByText("인트로/아웃트로 의심")).toBeInTheDocument();
  });

  it("does not show the badge when introOutro is false", () => {
    render(ClusterCard, { props: { cluster: possibleCluster(false) } });
    expect(screen.queryByText("인트로/아웃트로 의심")).not.toBeInTheDocument();
  });

  it("does not show the badge when introOutro is undefined (older daemon/mock)", () => {
    render(ClusterCard, { props: { cluster: possibleCluster() } });
    expect(screen.queryByText("인트로/아웃트로 의심")).not.toBeInTheDocument();
  });
});
