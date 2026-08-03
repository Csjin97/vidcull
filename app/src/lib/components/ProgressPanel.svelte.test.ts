import { render, screen } from "@testing-library/svelte";
import { describe, expect, it } from "vitest";
import ProgressPanel from "./ProgressPanel.svelte";
import type { ProgressSnapshot } from "$lib/model/types";

function snap(
  pending: number,
  running: number,
  done: number,
  failed = 0,
): ProgressSnapshot {
  return { pending, running, done, failed };
}

describe("ProgressPanel", () => {
  it("shows the live state, percent and per-state counts while scanning", () => {
    render(ProgressPanel, {
      props: {
        snapshot: snap(40, 4, 56, 0), 
        throughput: [5, 8, 6],
        etaSeconds: 12,
        reclaimableBytes: 5 * 1024 * 1024,
      },
    });
    expect(screen.getByText("인덱싱 진행 중")).toBeInTheDocument();
    expect(screen.getByText("56%")).toBeInTheDocument();
    expect(screen.getByText("남은 시간 12초")).toBeInTheDocument();
    expect(screen.getByText("40")).toBeInTheDocument(); 
    expect(screen.getByText("56")).toBeInTheDocument(); 
  });

  it("reports the progressbar aria value from the resolved fraction", () => {
    render(ProgressPanel, {
      props: {
        snapshot: snap(0, 0, 80, 20), 
        throughput: [],
        etaSeconds: null,
        reclaimableBytes: 0,
      },
    });
    const bar = screen.getByRole("progressbar");
    expect(bar).toHaveAttribute("aria-valuenow", "100");
    expect(screen.getByText("인덱싱 완료")).toBeInTheDocument();
    expect(screen.getByText("남은 시간 —")).toBeInTheDocument();
  });

  it("shows an idle state and the reclaimable space for an empty queue", () => {
    render(ProgressPanel, {
      props: {
        snapshot: null,
        throughput: [],
        etaSeconds: null,
        reclaimableBytes: 2 * 1024 * 1024 * 1024,
      },
    });
    expect(screen.getByText("대기 중")).toBeInTheDocument();
    expect(screen.getByText("0%")).toBeInTheDocument();
    expect(screen.getByText("2.0 GB")).toBeInTheDocument();
    expect(screen.getByText("정리 시 확보 가능")).toBeInTheDocument();
  });

  it("always renders the sparkline graph", () => {
    render(ProgressPanel, {
      props: {
        snapshot: snap(10, 0, 0),
        throughput: [],
        etaSeconds: null,
        reclaimableBytes: 0,
      },
    });
    expect(screen.getByTestId("sparkline")).toBeInTheDocument();
  });

  it("shows the daemon CPU%, RSS and the smoothed processing speed", () => {
    render(ProgressPanel, {
      props: {
        snapshot: {
          pending: 40,
          running: 4,
          done: 56,
          failed: 0,
          cpuUsagePermille: 375, 
          rssBytes: 180 * 1024 * 1024, 
          throughputBytesPerSec: 12 * 1024 * 1024,
        },
        throughput: [5, 8, 6],
        etaSeconds: 12,
        speedBytesPerSec: 12 * 1024 * 1024, 
        speedKnown: true,
        reclaimableBytes: 0,
      },
    });
    expect(screen.getByTestId("metric-cpu")).toHaveTextContent("37.5%");
    expect(screen.getByTestId("metric-rss")).toHaveTextContent("180.0 MB");
    expect(screen.getByTestId("metric-throughput")).toHaveTextContent(
      "12.0 MB/s",
    );
  });

  it("reads '측정 중' for speed and ETA while scanning before the first rate", () => {
    render(ProgressPanel, {
      props: {
        snapshot: snap(40, 4, 0, 0), 
        throughput: [],
        etaSeconds: null, 
        speedBytesPerSec: 0,
        speedKnown: false,
        reclaimableBytes: 0,
      },
    });
    expect(screen.getByTestId("metric-throughput")).toHaveTextContent("측정 중");
    expect(screen.getByText("남은 시간 측정 중")).toBeInTheDocument();
  });

  it("surfaces the stall note when indexing is stuck on a slow file", () => {
    render(ProgressPanel, {
      props: {
        snapshot: snap(40, 4, 56, 0),
        throughput: [5, 8, 6],
        etaSeconds: 1200,
        speedBytesPerSec: 4 * 1024 * 1024,
        speedKnown: true,
        stalled: true,
        reclaimableBytes: 0,
      },
    });
    expect(screen.getByTestId("progress-stall")).toHaveTextContent(
      "지연 파일 처리 중",
    );
  });

  it("hides the stall note when draining normally", () => {
    render(ProgressPanel, {
      props: {
        snapshot: snap(40, 4, 56, 0),
        throughput: [5, 8, 6],
        etaSeconds: 1200,
        speedBytesPerSec: 4 * 1024 * 1024,
        speedKnown: true,
        stalled: false,
        reclaimableBytes: 0,
      },
    });
    expect(screen.queryByTestId("progress-stall")).not.toBeInTheDocument();
  });

  it("formats a multi-hour ETA for a large remaining workload", () => {
    render(ProgressPanel, {
      props: {
        snapshot: snap(40, 4, 56, 0),
        throughput: [5, 8, 6],
        etaSeconds: 2 * 3600 + 15 * 60, 
        speedBytesPerSec: 8 * 1024 * 1024,
        speedKnown: true,
        reclaimableBytes: 0,
      },
    });
    expect(screen.getByText("남은 시간 2시간 15분")).toBeInTheDocument();
  });

  it("dashes the speed metric when idle and reads 0% CPU", () => {
    render(ProgressPanel, {
      props: {
        snapshot: snap(0, 0, 100, 0), 
        throughput: [],
        etaSeconds: null,
        speedBytesPerSec: 0,
        speedKnown: false,
        reclaimableBytes: 0,
      },
    });
    expect(screen.getByTestId("metric-cpu")).toHaveTextContent("0.0%");
    expect(screen.getByTestId("metric-throughput")).toHaveTextContent("—");
  });

  it("hides the detailed body when collapsed but keeps the status + bar", () => {
    render(ProgressPanel, {
      props: {
        snapshot: snap(40, 4, 56, 0),
        throughput: [5, 8, 6],
        etaSeconds: 12,
        reclaimableBytes: 0,
        collapsed: true,
      },
    });
    expect(screen.getByText("인덱싱 진행 중")).toBeInTheDocument();
    expect(screen.getByRole("progressbar")).toBeInTheDocument();
    expect(screen.queryByTestId("sparkline")).not.toBeInTheDocument();
    expect(screen.queryByText("처리 속도")).not.toBeInTheDocument();
  });

  it("toggles via the collapse button", async () => {
    const calls: number[] = [];
    render(ProgressPanel, {
      props: {
        snapshot: snap(10, 0, 0),
        throughput: [],
        etaSeconds: null,
        reclaimableBytes: 0,
        ontoggle: () => calls.push(1),
      },
    });
    const toggle = screen.getByTestId("progress-toggle");
    toggle.click();
    expect(calls).toHaveLength(1);
    expect(toggle).toHaveAttribute("aria-expanded", "true");
  });

  it("shows the file being decoded and a '외 N개' overflow", () => {
    render(ProgressPanel, {
      props: {
        snapshot: {
          pending: 40,
          running: 2,
          done: 56,
          failed: 0,
          currentFiles: ["/lib/영화/현재 처리.mkv", "/lib/b.mp4"],
        },
        throughput: [5, 8, 6],
        etaSeconds: 12,
        reclaimableBytes: 0,
      },
    });
    const current = screen.getByTestId("progress-current");
    expect(current).toHaveTextContent("현재 처리.mkv");
    expect(current).toHaveTextContent("외 1개");
    expect(current).toHaveAttribute("title", "/lib/영화/현재 처리.mkv");
  });

  it("hides the current-file line when nothing is running", () => {
    render(ProgressPanel, {
      props: {
        snapshot: snap(0, 0, 100, 0), 
        throughput: [],
        etaSeconds: null,
        reclaimableBytes: 0,
      },
    });
    expect(screen.queryByTestId("progress-current")).not.toBeInTheDocument();
  });

  it("keeps the current-file line visible when collapsed, with no overflow for one file", () => {
    render(ProgressPanel, {
      props: {
        snapshot: {
          pending: 10,
          running: 1,
          done: 0,
          failed: 0,
          currentFiles: ["/lib/solo.mp4"],
        },
        throughput: [],
        etaSeconds: null,
        reclaimableBytes: 0,
        collapsed: true,
      },
    });
    const current = screen.getByTestId("progress-current");
    expect(current).toHaveTextContent("solo.mp4");
    expect(current).not.toHaveTextContent("외");
  });

  it("falls back to '남은 N개' when a pre-v22 daemon omits partialDone", () => {
    render(ProgressPanel, {
      props: {
        snapshot: {
          pending: 0,
          running: 0,
          done: 100,
          failed: 0,
          partialPending: 5,
          partialRunning: 2,
        },
        throughput: [],
        etaSeconds: null,
        reclaimableBytes: 0,
      },
    });
    expect(screen.getByText("인덱싱 완료")).toBeInTheDocument();
    const partial = screen.getByTestId("progress-partial");
    expect(partial).toBeInTheDocument();
    expect(partial).toHaveTextContent("부분클립 분석 중");
    expect(partial).toHaveTextContent("남은 7개");
    expect(partial).not.toHaveTextContent("/");
    const seg = screen.getByTestId("progress-partial-seg");
    expect(seg).toBeInTheDocument();
    expect(seg.getAttribute("data-partial-fill")).toBe("stripes");
  });

  it("shows '부분클립 검사 N/M' when a v22 daemon reports partialDone", () => {
    render(ProgressPanel, {
      props: {
        snapshot: {
          pending: 0,
          running: 0,
          done: 100,
          failed: 0,
          partialDone: 3,
          partialRunning: 2,
          partialPending: 5,
        },
        throughput: [],
        etaSeconds: null,
        reclaimableBytes: 0,
      },
    });
    const partial = screen.getByTestId("progress-partial");
    expect(partial).toBeInTheDocument();
    expect(partial).toHaveTextContent("부분클립 검사 3/10");
    expect(partial).not.toHaveTextContent("남은");
  });

  it("paints the partial bar segment width to the completed ratio (v22)", () => {
    render(ProgressPanel, {
      props: {
        snapshot: {
          pending: 0,
          running: 0,
          done: 100,
          failed: 0,
          partialDone: 3,
          partialRunning: 2,
          partialPending: 5,
        },
        throughput: [],
        etaSeconds: null,
        reclaimableBytes: 0,
      },
    });
    const seg = screen.getByTestId("progress-partial-seg");
    expect(seg.getAttribute("data-partial-fill")).toBe("ratio");
    expect(seg.getAttribute("style")).toContain("width: 30%");
  });

  it("distinguishes v22 partialDone:0 ('0/M') from a pre-v22 omit", () => {
    render(ProgressPanel, {
      props: {
        snapshot: {
          pending: 0,
          running: 0,
          done: 100,
          failed: 0,
          partialDone: 0,
          partialRunning: 1,
          partialPending: 4,
        },
        throughput: [],
        etaSeconds: null,
        reclaimableBytes: 0,
      },
    });
    const partial = screen.getByTestId("progress-partial");
    expect(partial).toHaveTextContent("부분클립 검사 0/5");
    expect(partial).not.toHaveTextContent("남은");
  });

  it("hides the partial bar segment when no partial work is outstanding", () => {
    render(ProgressPanel, {
      props: {
        snapshot: {
          pending: 4,
          running: 1,
          done: 20,
          failed: 0,
          partialPending: 0,
          partialRunning: 0,
        },
        throughput: [],
        etaSeconds: 10,
        reclaimableBytes: 0,
      },
    });
    expect(
      screen.queryByTestId("progress-partial-seg"),
    ).not.toBeInTheDocument();
  });

  it("shows '부분클립 분석 중…' when foreground is still scanning (coexistence)", () => {
    render(ProgressPanel, {
      props: {
        snapshot: {
          pending: 10,
          running: 2,
          done: 50,
          failed: 0,
          partialPending: 3,
          partialRunning: 1,
        },
        throughput: [],
        etaSeconds: 30,
        reclaimableBytes: 0,
      },
    });
    expect(screen.getByText("인덱싱 진행 중")).toBeInTheDocument();
    expect(screen.getByTestId("progress-partial")).toBeInTheDocument();
  });

  it("hides '부분클립 분석 중…' when both partial counts are zero", () => {
    render(ProgressPanel, {
      props: {
        snapshot: {
          pending: 0,
          running: 0,
          done: 100,
          failed: 0,
          partialPending: 0,
          partialRunning: 0,
        },
        throughput: [],
        etaSeconds: null,
        reclaimableBytes: 0,
      },
    });
    expect(screen.queryByTestId("progress-partial")).not.toBeInTheDocument();
  });

  it("hides '부분클립 분석 중…' when partial fields are omitted (pre-v19 daemon)", () => {
    render(ProgressPanel, {
      props: {
        snapshot: snap(0, 0, 100, 0),
        throughput: [],
        etaSeconds: null,
        reclaimableBytes: 0,
      },
    });
    expect(screen.queryByTestId("progress-partial")).not.toBeInTheDocument();
  });

  it("shows '부분클립 분석 중…' when collapsed (status line still visible)", () => {
    render(ProgressPanel, {
      props: {
        snapshot: {
          pending: 0,
          running: 0,
          done: 100,
          failed: 0,
          partialPending: 2,
          partialRunning: 0,
        },
        throughput: [],
        etaSeconds: null,
        reclaimableBytes: 0,
        collapsed: true,
      },
    });
    expect(screen.getByTestId("progress-partial")).toBeInTheDocument();
  });

  it("shows '부분클립 제외 N개' with a reason breakdown when files were skipped", () => {
    render(ProgressPanel, {
      props: {
        snapshot: {
          pending: 0,
          running: 0,
          done: 100,
          failed: 0,
          partialPending: 0,
          partialRunning: 0,
          partialDone: 8,
          partialSkipped: { "unsupported-codec": 2, "duration-cap": 1 },
        },
        throughput: [],
        etaSeconds: null,
        reclaimableBytes: 0,
      },
    });
    const excluded = screen.getByTestId("progress-partial-excluded");
    expect(excluded).toBeInTheDocument();
    expect(excluded).toHaveTextContent("부분클립 제외 3개");
    expect(excluded).toHaveTextContent("코덱 미지원 2");
    expect(excluded).toHaveTextContent("길이 초과 1");
  });

  it("hides the '제외' line when nothing was skipped (empty / omitted map)", () => {
    render(ProgressPanel, {
      props: {
        snapshot: {
          pending: 0,
          running: 0,
          done: 100,
          failed: 0,
          partialSkipped: {},
        },
        throughput: [],
        etaSeconds: null,
        reclaimableBytes: 0,
      },
    });
    expect(
      screen.queryByTestId("progress-partial-excluded"),
    ).not.toBeInTheDocument();
  });

  it("shows a distinct '실패/검증 필요 N개' failure line when reindex permanently failed", () => {
    render(ProgressPanel, {
      props: {
        snapshot: {
          pending: 0,
          running: 0,
          done: 100,
          failed: 0,
          partialSkipped: { "unsupported-codec": 1 },
          partialFailed: 2,
        },
        throughput: [],
        etaSeconds: null,
        reclaimableBytes: 0,
      },
    });
    const failed = screen.getByTestId("progress-partial-failed");
    expect(failed).toBeInTheDocument();
    expect(failed).toHaveTextContent("실패/검증 필요 2개");
    const excluded = screen.getByTestId("progress-partial-excluded");
    expect(excluded).toBeInTheDocument();
    expect(excluded).not.toHaveTextContent("실패/검증 필요");
  });

  it("hides the '실패/검증 필요' line when nothing permanently failed", () => {
    render(ProgressPanel, {
      props: {
        snapshot: {
          pending: 0,
          running: 0,
          done: 100,
          failed: 0,
          partialFailed: 0,
        },
        throughput: [],
        etaSeconds: null,
        reclaimableBytes: 0,
      },
    });
    expect(
      screen.queryByTestId("progress-partial-failed"),
    ).not.toBeInTheDocument();
  });

  it("AC4: shows '일시정지' in place of the ETA when paused while still scanning", () => {
    render(ProgressPanel, {
      props: {
        snapshot: snap(40, 4, 56, 0), 
        throughput: [5, 8, 6],
        etaSeconds: 52,
        reclaimableBytes: 0,
        paused: true,
      },
    });
    expect(screen.getByTestId("progress-eta")).toHaveTextContent("일시정지");
    expect(screen.getByTestId("progress-eta")).not.toHaveTextContent("남은 시간");
  });

  it("AC4: shows the normal ETA text when not paused", () => {
    render(ProgressPanel, {
      props: {
        snapshot: snap(40, 4, 56, 0),
        throughput: [5, 8, 6],
        etaSeconds: 52,
        reclaimableBytes: 0,
        paused: false,
      },
    });
    expect(screen.getByTestId("progress-eta")).toHaveTextContent("남은 시간 52초");
  });

  it("AC4: ignores paused when nothing is scanning (idle queue keeps '—')", () => {
    render(ProgressPanel, {
      props: {
        snapshot: snap(0, 0, 100, 0), 
        throughput: [],
        etaSeconds: null,
        reclaimableBytes: 0,
        paused: true,
      },
    });
    expect(screen.getByTestId("progress-eta")).toHaveTextContent("남은 시간 —");
    expect(screen.getByTestId("progress-eta")).not.toHaveTextContent("일시정지");
  });
});
