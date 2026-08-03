import { fireEvent, render, screen } from "@testing-library/svelte";
import { describe, expect, it, vi } from "vitest";
import GroupCard from "./GroupCard.svelte";
import { trustLabel } from "$lib/model/format";
import type { DuplicateGroup, FileEntry } from "$lib/model/types";

function file(fileId: number, w: number, h: number): FileEntry {
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
  };
}

const group: DuplicateGroup = {
  groupId: 7,
  trust: "VERY_LIKELY",
  bestFileId: 1,
  members: [file(1, 3840, 2160), file(2, 1920, 1080), file(3, 1280, 720)],
};

describe("GroupCard render", () => {
  it("renders the trust label, file count and reclaimable size", () => {
    render(GroupCard, { props: { group } });
    expect(screen.getByText(trustLabel("VERY_LIKELY"))).toBeInTheDocument();
    expect(screen.getByText("3개 파일")).toBeInTheDocument();
    expect(screen.getByText("정리 시 회수 5.0 MB")).toBeInTheDocument();
  });

  it("renders one thumbnail per member (capped at four)", () => {
    render(GroupCard, { props: { group } });
    expect(screen.getAllByRole("img")).toHaveLength(3);
  });

  it("fires onselect with the group on click", async () => {
    const onselect = vi.fn();
    render(GroupCard, { props: { group, onselect } });
    await fireEvent.click(screen.getByRole("button"));
    expect(onselect).toHaveBeenCalledWith(group);
  });
});
