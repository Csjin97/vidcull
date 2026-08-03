import { fireEvent, render, screen } from "@testing-library/svelte";
import { describe, expect, it } from "vitest";
import FailureAccordion from "./FailureAccordion.svelte";
import type { FailedTask } from "$lib/model/types";

const tasks: FailedTask[] = [
  {
    taskId: 1002,
    path: "C:/videos/corrupt-finale.mkv",
    reason: "decode error: invalid NAL unit",
    attempts: 3,
  },
  {
    taskId: 1001,
    path: "C:/videos/archive/old-codec.avi",
    reason: "no decoder for codec 'mpeg2video'",
    attempts: 1,
  },
];

describe("FailureAccordion", () => {
  it("renders nothing when there are no failures", () => {
    const { container } = render(FailureAccordion, { props: { tasks: [] } });
    expect(container.querySelector(".failures")).toBeNull();
  });

  it("shows the failure count collapsed and hides the rows until expanded", () => {
    const { container } = render(FailureAccordion, { props: { tasks } });
    expect(screen.getByText("실패 2건")).toBeInTheDocument();
    expect(
      screen.queryByText("decode error: invalid NAL unit"),
    ).not.toBeInTheDocument();
    const toggle = container.querySelector(".failures__toggle");
    expect(toggle).toHaveAttribute("aria-expanded", "false");
  });

  it("reveals each failed file with its reason and attempts on expand", async () => {
    let open = false;
    const { container, rerender } = render(FailureAccordion, {
      props: {
        tasks,
        open,
        ontoggle: () => {
          open = !open;
          rerender({ tasks, open });
        },
      },
    });
    const toggle = container.querySelector(".failures__toggle")!;
    await fireEvent.click(toggle);
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByText("corrupt-finale.mkv")).toBeInTheDocument();
    expect(screen.getByText("old-codec.avi")).toBeInTheDocument();
    expect(screen.getByText("3회 시도")).toBeInTheDocument();

    const detailBtns = screen.getAllByText("상세보기");
    await fireEvent.click(detailBtns[0]);
    expect(screen.getByText("전체 에러 로그")).toBeInTheDocument();
    expect(screen.getAllByText("decode error: invalid NAL unit")).toHaveLength(2);
  });

  it("labels a lost-payload row instead of showing an empty name", async () => {
    let open = false;
    const { container, rerender } = render(FailureAccordion, {
      props: {
        tasks: [{ taskId: 9, path: "", reason: "task failed", attempts: 1 }],
        open,
        ontoggle: () => {
          open = !open;
          rerender({
            tasks: [{ taskId: 9, path: "", reason: "task failed", attempts: 1 }],
            open,
          });
        },
      },
    });
    const toggle = container.querySelector(".failures__toggle")!;
    await fireEvent.click(toggle);
    expect(screen.getByText("(알 수 없는 경로)")).toBeInTheDocument();
  });
});
