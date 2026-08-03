import { render, screen } from "@testing-library/svelte";
import { describe, expect, it } from "vitest";
import OverlapTimeline from "./OverlapTimeline.svelte";
import type { ClipOverlap } from "$lib/model/types";

function overlap(
  clipFileId: number,
  startMs: number,
  endMs: number,
  matched = 9,
  total = 10,
): ClipOverlap {
  return {
    clipFileId,
    sourceFileId: 1,
    matchedScenes: matched,
    clipScenes: total,
    startMs,
    endMs,
    clipStartMs: startMs,
    clipEndMs: endMs,
  };
}

describe("OverlapTimeline", () => {
  it("renders one bar per overlapping clip", () => {
    render(OverlapTimeline, {
      props: {
        sourceDurationMs: 600_000,
        overlaps: [
          overlap(2, 0, 120_000),
          overlap(3, 200_000, 320_000),
          overlap(4, 100_000, 250_000),
        ],
      },
    });
    expect(screen.getAllByTestId("overlap-bar")).toHaveLength(3);
  });

  it("positions a bar by its fractional start/width and coverage opacity", () => {
    render(OverlapTimeline, {
      props: {
        sourceDurationMs: 10_000,
        overlaps: [overlap(2, 2500, 7500, 10, 10)], 
      },
    });
    const bar = screen.getByTestId("overlap-bar");
    expect(bar.getAttribute("style")).toContain("left: 25%");
    expect(bar.getAttribute("style")).toContain("width: 50%");
    expect(bar.getAttribute("style")).toContain("opacity: 1");
  });

  it("shows an empty notice when nothing overlaps the source", () => {
    render(OverlapTimeline, {
      props: { sourceDurationMs: 600_000, overlaps: [] },
    });
    expect(screen.queryByTestId("overlap-bar")).toBeNull();
    expect(
      screen.getByText("이 원본과 겹치는 부분 클립이 없습니다."),
    ).toBeInTheDocument();
  });

  it("caps rendered bars at scale so the DOM stays bounded (60fps lock)", () => {
    const many: ClipOverlap[] = [];
    for (let i = 0; i < 5_000; i += 1) {
      many.push(overlap(i + 2, i * 100, i * 100 + 5_000, (i % 10) + 1, 10));
    }
    render(OverlapTimeline, {
      props: { sourceDurationMs: 600_000, overlaps: many },
    });
    expect(screen.getAllByTestId("overlap-bar").length).toBe(200);
  });

  it("renders a time-range row for each overlap (startMs=12000 → '0:12', endMs=275000 → '4:35')", () => {
    render(OverlapTimeline, {
      props: {
        sourceDurationMs: 600_000,
        overlaps: [overlap(5, 12_000, 275_000, 8, 10)],
      },
    });
    const rows = screen.getAllByTestId("overlap-range");
    expect(rows).toHaveLength(1);
    expect(rows[0]).toHaveTextContent("0:12");
    expect(rows[0]).toHaveTextContent("4:35");
    expect(rows[0]).toHaveTextContent("80%");
  });

  it("renders range rows sorted by startMs when multiple overlaps given", () => {
    render(OverlapTimeline, {
      props: {
        sourceDurationMs: 600_000,
        overlaps: [
          overlap(10, 300_000, 360_000), 
          overlap(11, 60_000, 120_000),  
        ],
      },
    });
    const rows = screen.getAllByTestId("overlap-range");
    expect(rows).toHaveLength(2);
    expect(rows[0]).toHaveTextContent("1:00");
    expect(rows[1]).toHaveTextContent("5:00");
  });

  it("shows no range rows when overlaps is empty", () => {
    render(OverlapTimeline, {
      props: { sourceDurationMs: 600_000, overlaps: [] },
    });
    expect(screen.queryByTestId("overlap-range")).toBeNull();
  });

  it("bar title includes time range and coverage %", () => {
    render(OverlapTimeline, {
      props: {
        sourceDurationMs: 600_000,
        overlaps: [overlap(7, 30_000, 90_000, 10, 10)],
      },
    });
    const bar = screen.getByTestId("overlap-bar");
    expect(bar.getAttribute("title")).toContain("0:30");
    expect(bar.getAttribute("title")).toContain("1:30");
    expect(bar.getAttribute("title")).toContain("100%");
  });
});

describe("OverlapTimeline notice prop", () => {
  const EMPTY_MSG = "이 원본과 겹치는 부분 클립이 없습니다.";

  it("renders the notice caption when segments are present [D3]", () => {
    render(OverlapTimeline, {
      props: {
        sourceDurationMs: 600_000,
        overlaps: [overlap(2, 0, 600_000, 10, 10)], 
        notice: "전체 구간 유사 (추정 · 겹침 데이터 없음)",
      },
    });
    expect(screen.getByTestId("overlap-bar")).toBeInTheDocument();
    expect(screen.getByTestId("overlap-notice")).toHaveTextContent(
      "전체 구간 유사 (추정 · 겹침 데이터 없음)",
    );
  });

  it("renders the notice EVEN when there are no segments (above the empty guard) [D3]", () => {
    render(OverlapTimeline, {
      props: {
        sourceDurationMs: 0,
        overlaps: [],
        notice: "겹침 구간 데이터 없음 — 전체 구간 임시 표시",
      },
    });
    expect(screen.queryByTestId("overlap-bar")).toBeNull();
    expect(screen.getByTestId("overlap-notice")).toHaveTextContent(
      "겹침 구간 데이터 없음 — 전체 구간 임시 표시",
    );
  });

  it("suppresses the empty message when a notice is present [D3]", () => {
    render(OverlapTimeline, {
      props: {
        sourceDurationMs: 600_000,
        overlaps: [], 
        notice: "전체 구간 유사 (추정 · 겹침 데이터 없음)",
      },
    });
    expect(screen.getByTestId("overlap-notice")).toBeInTheDocument();
    expect(screen.queryByText(EMPTY_MSG)).not.toBeInTheDocument();
  });

  it("shows the empty message (and no notice) when overlaps are empty and notice is absent [guard]", () => {
    render(OverlapTimeline, {
      props: { sourceDurationMs: 600_000, overlaps: [] },
    });
    expect(screen.queryByTestId("overlap-notice")).toBeNull();
    expect(screen.getByText(EMPTY_MSG)).toBeInTheDocument();
  });
});
