import { fireEvent, render, screen, waitFor } from "@testing-library/svelte";
import { beforeEach, describe, expect, it, vi } from "vitest";
import CompareView from "./CompareView.svelte";
import type {
  ClipOverlap,
  DuplicateGroup,
  FileEntry,
} from "$lib/model/types";

vi.mock("$lib/stores/errorStore.svelte", () => ({
  errorStore: {
    reportError: vi.fn(),
    dismiss: vi.fn(),
    dismissAll: vi.fn(),
    errors: [],
  },
}));
import { errorStore } from "$lib/stores/errorStore.svelte";
import { introOutroVisibilityStore } from "$lib/stores/introOutroVisibilityStore.svelte";

beforeEach(() => {
  introOutroVisibilityStore.set(false);
});

function file(fileId: number, w: number, h: number, size: number): FileEntry {
  return {
    fileId,
    path: `/v/${fileId}.mp4`,
    sizeBytes: size,
    width: w,
    height: h,
    durationMs: 60_000,
    bitrateBps: 5_000_000,
    codec: "h264",
    container: "mp4",
    thumbnailUrl: null,
  };
}

const group: DuplicateGroup = {
  groupId: 42,
  trust: "VERY_LIKELY",
  bestFileId: 1,
  members: [
    file(1, 3840, 2160, 4_000_000),
    file(2, 1920, 1080, 2_000_000),
    file(3, 1280, 720, 1_000_000),
  ],
};

const noop = (): void => {};
const okExecute = vi.fn(async () => ({
  ok: true,
  removedFileIds: [2, 3],
  reclaimedBytes: 3_000_000,
  detail: "done",
  rejectCode: null,
}));

describe("CompareView safe-delete UX", () => {
  it("highlights the best copy and pre-selects only the non-best members", () => {
    render(CompareView, {
      props: { group, onclose: noop, onexecute: okExecute },
    });
    expect(screen.getByText("최적 사본")).toBeInTheDocument();

    const checkboxes = screen.getAllByRole("checkbox") as HTMLInputElement[];
    expect(checkboxes).toHaveLength(3);
    expect(checkboxes[0].checked).toBe(false); 
    expect(checkboxes[1].checked).toBe(true);
    expect(checkboxes[2].checked).toBe(true);
  });

  it("enables the trash button with the default selection and shows reclaim", () => {
    render(CompareView, {
      props: { group, onclose: noop, onexecute: okExecute },
    });
    const trashBtn = screen.getByRole("button", { name: "휴지통으로 이동" });
    expect(trashBtn).toBeEnabled();
    expect(screen.getByText(/2개 선택 · 회수/)).toBeInTheDocument();
  });

  it("blocks the action and explains why when nothing is selected", async () => {
    render(CompareView, {
      props: { group, onclose: noop, onexecute: okExecute },
    });
    const checkboxes = screen.getAllByRole("checkbox") as HTMLInputElement[];
    await fireEvent.click(checkboxes[1]);
    await fireEvent.click(checkboxes[2]);

    const trashBtn = screen.getByRole("button", { name: "휴지통으로 이동" });
    expect(trashBtn).toBeDisabled();
    expect(screen.getByText("삭제할 파일을 선택하세요.")).toBeInTheDocument();
  });
});

describe("CompareView overlap perspective switch", () => {
  function withOverlaps(): {
    group: DuplicateGroup;
    overlaps: ClipOverlap[];
  } {
    const source = file(1, 1920, 1080, 4_000_000);
    source.durationMs = 600_000;
    const clip = file(2, 1280, 720, 1_000_000);
    clip.durationMs = 150_000;
    return {
      group: {
        groupId: 7,
        trust: "POSSIBLE",
        bestFileId: null,
        members: [source, clip],
      },
      overlaps: [
        {
          clipFileId: 2,
          sourceFileId: 1,
          matchedScenes: 8,
          clipScenes: 10,
          startMs: 60_000, 
          endMs: 210_000, 
          clipStartMs: 0, 
          clipEndMs: 150_000, 
        },
      ],
    };
  }

  it("defaults to the source perspective (source-side range)", () => {
    const { group, overlaps } = withOverlaps();
    render(CompareView, {
      props: { group, overlaps, onclose: noop, onexecute: okExecute },
    });
    const row = screen.getByTestId("overlap-range");
    expect(row).toHaveTextContent("1:00");
    expect(row).toHaveTextContent("3:30");
  });

  it("focuses a member on click and switches to its perspective", async () => {
    const { group, overlaps } = withOverlaps();
    const { container } = render(CompareView, {
      props: { group, overlaps, onclose: noop, onexecute: okExecute },
    });

    const focusButtons = Array.from(
      container.querySelectorAll<HTMLButtonElement>(".member__focus"),
    );
    expect(focusButtons).toHaveLength(2);
    await fireEvent.click(focusButtons[1]);

    const focused = container.querySelector(".member--focused");
    expect(focused).not.toBeNull();

    const row = screen.getByTestId("overlap-range");
    expect(row).toHaveTextContent("0:00");
    expect(row).toHaveTextContent("2:30");
  });
});

describe("CompareView overlap timeline survives a removed source member", () => {
  it("renders the timeline from the surviving member when the source is gone", () => {
    const clip = file(2, 1280, 720, 1_000_000);
    clip.durationMs = 150_000;
    const group: DuplicateGroup = {
      groupId: 9,
      trust: "POSSIBLE",
      bestFileId: null,
      members: [clip], 
    };
    const overlaps: ClipOverlap[] = [
      {
        clipFileId: 2,
        sourceFileId: 1, 
        matchedScenes: 8,
        clipScenes: 10,
        startMs: 60_000,
        endMs: 210_000,
        clipStartMs: 0, 
        clipEndMs: 150_000, 
      },
    ];

    render(CompareView, {
      props: { group, overlaps, onclose: noop, onexecute: okExecute },
    });

    const row = screen.getByTestId("overlap-range");
    expect(row).toHaveTextContent("0:00");
    expect(row).toHaveTextContent("2:30");
  });
});

describe("CompareView timeline visibility", () => {
  function wholeFileGroup(trust: "EXACT" | "VERY_LIKELY"): DuplicateGroup {
    const m1 = file(1, 3840, 2160, 4_000_000);
    const m2 = file(2, 1920, 1080, 2_000_000);
    return { groupId: 100, trust, bestFileId: 1, members: [m1, m2] };
  }

  it("재인코딩/완전동일 그룹 — 타임라인 미마운트 [D1]", () => {
    for (const trust of ["EXACT", "VERY_LIKELY"] as const) {
      const { unmount } = render(CompareView, {
        props: {
          group: wholeFileGroup(trust),
          overlaps: [],
          onclose: noop,
          onexecute: okExecute,
        },
      });
      expect(screen.queryByTestId("overlap-timeline")).not.toBeInTheDocument();
      expect(screen.queryByText("전체 일치")).not.toBeInTheDocument();
      unmount();
    }
  });

  it("추정 whole-file(길이비≈1.0) — full-span 바 + 안내 마운트 [D3]", () => {
    const g: DuplicateGroup = {
      groupId: 101,
      trust: "POSSIBLE",
      bestFileId: null,
      members: [file(1, 1920, 1080, 2_000_000), file(2, 1280, 720, 1_000_000)],
    };
    render(CompareView, {
      props: { group: g, overlaps: [], onclose: noop, onexecute: okExecute },
    });
    expect(screen.getByTestId("overlap-timeline")).toBeInTheDocument();
    expect(screen.getByTestId("overlap-bar")).toBeInTheDocument();
    expect(screen.getByText("전체 일치")).toBeInTheDocument();
    expect(screen.getByTestId("overlap-notice")).toHaveTextContent(
      "전체 구간 유사 (추정 · 겹침 데이터 없음)",
    );
    expect(
      screen.queryByText(/겹치는 부분 클립이 없습니다/),
    ).not.toBeInTheDocument();
  });

  it("추정 + 데이터없음 + 길이비 0.25(비대칭) — 바 + 강한 안내 [D3]", () => {
    const long = file(1, 1920, 1080, 4_000_000);
    long.durationMs = 60_000;
    const short = file(2, 1280, 720, 1_000_000);
    short.durationMs = 15_000;
    const g: DuplicateGroup = {
      groupId: 102,
      trust: "POSSIBLE",
      bestFileId: null,
      members: [long, short],
    };
    render(CompareView, {
      props: { group: g, overlaps: [], onclose: noop, onexecute: okExecute },
    });
    expect(screen.getByTestId("overlap-timeline")).toBeInTheDocument();
    expect(screen.getByTestId("overlap-bar")).toBeInTheDocument();
    expect(screen.getByTestId("overlap-notice")).toHaveTextContent(
      "겹침 구간 데이터 없음 — 전체 구간 임시 표시",
    );
  });

  it("추정 단일멤버(폴더삭제) + 데이터없음 — self-span 바 + 안내 보장 [D3]", () => {
    const clip = file(2, 1280, 720, 1_000_000);
    clip.durationMs = 150_000;
    const g: DuplicateGroup = {
      groupId: 103,
      trust: "POSSIBLE",
      bestFileId: null,
      members: [clip],
    };
    render(CompareView, {
      props: { group: g, overlaps: [], onclose: noop, onexecute: okExecute },
    });
    expect(screen.getByTestId("overlap-timeline")).toBeInTheDocument();
    expect(screen.getByTestId("overlap-bar")).toBeInTheDocument();
    expect(screen.getByTestId("overlap-notice")).toBeInTheDocument();
  });

  it("추정 + durationMs<=0 — 빈메시지 대신 안내 present", () => {
    const a = file(1, 1920, 1080, 2_000_000);
    a.durationMs = 0;
    const b = file(2, 1280, 720, 1_000_000);
    b.durationMs = 0;
    const g: DuplicateGroup = {
      groupId: 104,
      trust: "POSSIBLE",
      bestFileId: null,
      members: [a, b],
    };
    render(CompareView, {
      props: { group: g, overlaps: [], onclose: noop, onexecute: okExecute },
    });
    expect(screen.getByTestId("overlap-timeline")).toBeInTheDocument();
    expect(screen.getByTestId("overlap-notice")).toBeInTheDocument();
    expect(
      screen.queryByText(/겹치는 부분 클립이 없습니다/),
    ).not.toBeInTheDocument();
  });

  it("still renders the partial-clip timeline normally for a POSSIBLE group with overlaps", () => {
    const source = file(1, 1920, 1080, 4_000_000);
    source.durationMs = 600_000;
    const clip = file(2, 1280, 720, 1_000_000);
    clip.durationMs = 150_000;
    const g: DuplicateGroup = {
      groupId: 102,
      trust: "POSSIBLE",
      bestFileId: null,
      members: [source, clip],
    };
    const overlaps: ClipOverlap[] = [
      {
        clipFileId: 2,
        sourceFileId: 1,
        matchedScenes: 8,
        clipScenes: 10,
        startMs: 60_000,
        endMs: 210_000,
        clipStartMs: 0,
        clipEndMs: 150_000,
      },
    ];
    render(CompareView, {
      props: { group: g, overlaps, onclose: noop, onexecute: okExecute },
    });
    expect(screen.getByTestId("overlap-timeline")).toBeInTheDocument();
    expect(screen.getByText("부분 클립 겹침")).toBeInTheDocument();
    const row = screen.getByTestId("overlap-range");
    expect(row).toHaveTextContent("1:00");
    expect(row).toHaveTextContent("3:30");
  });
});

describe("CompareView fan-out cluster perspectives", () => {
  it("renders a timeline bar from each of the four members' perspectives", async () => {
    const clip = file(4, 1280, 720, 1_000_000);
    clip.durationMs = 150_000;
    const copy6 = file(6, 1920, 1080, 4_000_000);
    copy6.durationMs = 600_000;
    const copy7 = file(7, 1920, 1080, 3_500_000);
    copy7.durationMs = 600_000;
    const copy8 = file(8, 1920, 1080, 3_000_000);
    copy8.durationMs = 600_000;
    const fanout: DuplicateGroup = {
      groupId: 61,
      trust: "POSSIBLE",
      bestFileId: null,
      members: [clip, copy6, copy7, copy8],
    };
    const overlap = (source: number): ClipOverlap => ({
      clipFileId: 4,
      sourceFileId: source,
      matchedScenes: 8,
      clipScenes: 10,
      startMs: 60_000,
      endMs: 210_000,
      clipStartMs: 0,
      clipEndMs: 150_000,
    });
    const overlaps: ClipOverlap[] = [overlap(6), overlap(7), overlap(8)];

    const { container } = render(CompareView, {
      props: { group: fanout, overlaps, onclose: noop, onexecute: okExecute },
    });

    expect(screen.getAllByTestId("overlap-bar").length).toBeGreaterThan(0);

    const focusButtons = Array.from(
      container.querySelectorAll<HTMLButtonElement>(".member__focus"),
    );
    expect(focusButtons).toHaveLength(4);
    for (const btn of focusButtons) {
      await fireEvent.click(btn);
      expect(screen.getByTestId("overlap-timeline")).toBeInTheDocument();
      expect(screen.getAllByTestId("overlap-bar").length).toBeGreaterThan(0);
    }
  });
});

describe("CompareView Bug A+B compose — deep-source fan-out", () => {
  it("renders the deep partial bar for the source, not a synthetic full-span", () => {
    const copy6 = file(6, 1920, 1080, 4_000_000);
    copy6.durationMs = 600_000; 
    const copy7 = file(7, 1920, 1080, 3_500_000);
    copy7.durationMs = 600_000;
    const copy8 = file(8, 1920, 1080, 3_000_000);
    copy8.durationMs = 600_000;
    const deep = file(12, 1920, 1080, 40_000_000);
    deep.durationMs = 3_600_000; 
    const fanout: DuplicateGroup = {
      groupId: 65,
      trust: "POSSIBLE",
      bestFileId: null,
      members: [copy6, copy7, copy8, deep],
    };
    const clipInDeep = (clip: number): ClipOverlap => ({
      clipFileId: clip,
      sourceFileId: 12,
      matchedScenes: 9,
      clipScenes: 10,
      startMs: 3_545_000, 
      endMs: 3_555_000, 
      clipStartMs: 0,
      clipEndMs: 10_000,
    });
    const overlaps: ClipOverlap[] = [clipInDeep(6), clipInDeep(7), clipInDeep(8)];

    render(CompareView, {
      props: { group: fanout, overlaps, onclose: noop, onexecute: okExecute },
    });

    expect(screen.getByTestId("overlap-timeline")).toBeInTheDocument();
    expect(screen.getByText("부분 클립 겹침")).toBeInTheDocument();
    expect(screen.queryByTestId("overlap-notice")).toBeNull();
    const bar = screen.getAllByTestId("overlap-bar")[0];
    expect(bar.getAttribute("style")).toContain("left: 98.");
    const rows = screen.getAllByTestId("overlap-range");
    expect(rows.some((r) => r.textContent?.includes("59:05"))).toBe(true);
    expect(rows.some((r) => r.textContent?.includes("59:15"))).toBe(true);
  });
});

describe("CompareView intro/outro overlap default-hide + toggle", () => {
  function sourceClipGroup(): { group: DuplicateGroup; source: FileEntry; clip: FileEntry } {
    const source = file(1, 1920, 1080, 4_000_000);
    source.durationMs = 600_000;
    const clip = file(2, 1280, 720, 1_000_000);
    clip.durationMs = 150_000;
    const group: DuplicateGroup = {
      groupId: 200,
      trust: "POSSIBLE",
      bestFileId: null,
      members: [source, clip],
    };
    return { group, source, clip };
  }

  it("① a group whose ONLY overlap is intro/outro-tagged is hidden by default, with a visible hidden-count", () => {
    const { group } = sourceClipGroup();
    const overlaps: ClipOverlap[] = [
      {
        clipFileId: 2,
        sourceFileId: 1,
        matchedScenes: 3,
        clipScenes: 10,
        startMs: 0,
        endMs: 5_000,
        clipStartMs: 0,
        clipEndMs: 5_000,
        introOutro: true,
      },
    ];
    render(CompareView, {
      props: { group, overlaps, onclose: noop, onexecute: okExecute },
    });
    expect(screen.queryByText("부분 클립 겹침")).not.toBeInTheDocument();
    expect(screen.getByText("전체 일치")).toBeInTheDocument();
    expect(screen.getByTestId("overlap-notice")).toBeInTheDocument();
    expect(screen.getByTestId("intro-outro-hidden-count")).toHaveTextContent(
      "인트로/아웃트로 겹침 1건 숨김",
    );
  });

  it("② toggling reveal shows the group's tagged overlap with a suspicion badge", async () => {
    const { group } = sourceClipGroup();
    const overlaps: ClipOverlap[] = [
      {
        clipFileId: 2,
        sourceFileId: 1,
        matchedScenes: 3,
        clipScenes: 10,
        startMs: 0,
        endMs: 5_000,
        clipStartMs: 0,
        clipEndMs: 5_000,
        introOutro: true,
      },
    ];
    render(CompareView, {
      props: { group, overlaps, onclose: noop, onexecute: okExecute },
    });
    const toggle = screen.getByRole("checkbox", { name: "인트로/아웃트로 겹침 표시" });
    expect((toggle as HTMLInputElement).checked).toBe(false);
    await fireEvent.click(toggle);
    expect((toggle as HTMLInputElement).checked).toBe(true);

    expect(screen.getByTestId("overlap-range")).toBeInTheDocument();
    expect(screen.getByText("부분 클립 겹침")).toBeInTheDocument();
    expect(screen.getByText("인트로/아웃트로 의심")).toBeInTheDocument();
  });

  it("③ an UNTAGGED overlap always shows — the toggle is irrelevant to it", () => {
    const { group } = sourceClipGroup();
    const overlaps: ClipOverlap[] = [
      {
        clipFileId: 2,
        sourceFileId: 1,
        matchedScenes: 8,
        clipScenes: 10,
        startMs: 60_000,
        endMs: 210_000,
        clipStartMs: 0,
        clipEndMs: 150_000,
        introOutro: false,
      },
    ];
    render(CompareView, {
      props: { group, overlaps, onclose: noop, onexecute: okExecute },
    });
    expect(screen.getByTestId("overlap-range")).toBeInTheDocument();
    expect(screen.getByText("부분 클립 겹침")).toBeInTheDocument();
    expect(screen.queryByRole("checkbox", { name: "인트로/아웃트로 겹침 표시" })).not.toBeInTheDocument();
    expect(screen.queryByTestId("intro-outro-hidden-count")).not.toBeInTheDocument();
  });

  it("④ a mixed group (tagged + untagged) shows the untagged overlap by default and reports the tagged one as hidden", () => {
    const clip3 = file(3, 1280, 720, 900_000);
    clip3.durationMs = 120_000;
    const { group, source, clip } = sourceClipGroup();
    group.members = [source, clip, clip3];
    const overlaps: ClipOverlap[] = [
      {
        clipFileId: 2,
        sourceFileId: 1,
        matchedScenes: 3,
        clipScenes: 10,
        startMs: 0,
        endMs: 5_000,
        clipStartMs: 0,
        clipEndMs: 5_000,
        introOutro: true, 
      },
      {
        clipFileId: 3,
        sourceFileId: 1,
        matchedScenes: 9,
        clipScenes: 10,
        startMs: 60_000,
        endMs: 180_000,
        clipStartMs: 0,
        clipEndMs: 120_000,
        introOutro: false, 
      },
    ];
    render(CompareView, {
      props: { group, overlaps, onclose: noop, onexecute: okExecute },
    });
    expect(screen.getByText("부분 클립 겹침")).toBeInTheDocument();
    const rows = screen.getAllByTestId("overlap-range");
    expect(rows.some((r) => r.textContent?.includes("1:00"))).toBe(true);
    expect(rows).toHaveLength(1);
    expect(screen.getByTestId("intro-outro-hidden-count")).toHaveTextContent(
      "인트로/아웃트로 겹침 1건 숨김",
    );
  });

  it("⑤ introOutro undefined (older daemon/mock data) behaves exactly like untagged — always shown, no hidden count", () => {
    const { group } = sourceClipGroup();
    const overlaps: ClipOverlap[] = [
      {
        clipFileId: 2,
        sourceFileId: 1,
        matchedScenes: 8,
        clipScenes: 10,
        startMs: 60_000,
        endMs: 210_000,
        clipStartMs: 0,
        clipEndMs: 150_000,
      },
    ];
    render(CompareView, {
      props: { group, overlaps, onclose: noop, onexecute: okExecute },
    });
    expect(screen.getByTestId("overlap-range")).toBeInTheDocument();
    expect(screen.getByText("부분 클립 겹침")).toBeInTheDocument();
    expect(screen.queryByTestId("intro-outro-hidden-count")).not.toBeInTheDocument();
  });
});

describe("CompareView confirmDelete — single-owner error surface", () => {
  beforeEach(() => {
    vi.mocked(errorStore.reportError).mockClear();
  });

  it("shows inline error when onexecute returns ok:false with a rejectCode (guard rejection, Domain 1)", async () => {
    const failExecute = vi.fn(async () => ({
      ok: false as const,
      removedFileIds: [] as number[],
      reclaimedBytes: 0,
      detail: "보호된 파일입니다.",
      rejectCode: "WOULD_DELETE_BEST",  
    }));

    render(CompareView, { props: { group, onclose: noop, onexecute: failExecute } });

    const footerTrashBtn = screen.getByRole("button", { name: "휴지통으로 이동" });
    await fireEvent.click(footerTrashBtn);

    const trashBtns = await screen.findAllByRole("button", { name: "휴지통으로 이동" });
    const confirmBtn = trashBtns[trashBtns.length - 1];
    await fireEvent.click(confirmBtn);

    await waitFor(() =>
      expect(screen.getByTestId("delete-error")).toBeInTheDocument(),
    );
    expect(screen.getByTestId("delete-error")).toHaveTextContent(
      "보호된 파일입니다.",
    );

    expect(errorStore.reportError).not.toHaveBeenCalled();

    expect(screen.queryByText("처리 중…")).not.toBeInTheDocument();
  });

  it("closes dialog and shows no inline error on ok:true outcome", async () => {
    render(CompareView, { props: { group, onclose: noop, onexecute: okExecute } });

    const footerTrashBtn = screen.getByRole("button", { name: "휴지통으로 이동" });
    await fireEvent.click(footerTrashBtn);

    const trashBtns = await screen.findAllByRole("button", { name: "휴지통으로 이동" });
    const confirmBtn = trashBtns[trashBtns.length - 1];
    await fireEvent.click(confirmBtn);

    await waitFor(() =>
      expect(screen.queryByTestId("delete-error")).not.toBeInTheDocument(),
    );

    expect(errorStore.reportError).not.toHaveBeenCalled();
  });

  it("suppresses inline error when rejectCode is null (execute already toasted — no double surface)", async () => {
    const unexpectedFailExecute = vi.fn(async () => ({
      ok: false as const,
      removedFileIds: [] as number[],
      reclaimedBytes: 0,
      detail: "삭제 중 오류가 발생했습니다.",
      rejectCode: null,  
    }));

    render(CompareView, { props: { group, onclose: noop, onexecute: unexpectedFailExecute } });

    const footerTrashBtn = screen.getByRole("button", { name: "휴지통으로 이동" });
    await fireEvent.click(footerTrashBtn);

    const trashBtns = await screen.findAllByRole("button", { name: "휴지통으로 이동" });
    const confirmBtn = trashBtns[trashBtns.length - 1];
    await fireEvent.click(confirmBtn);

    await waitFor(() =>
      expect(screen.queryByTestId("delete-error")).not.toBeInTheDocument(),
    );
    expect(errorStore.reportError).not.toHaveBeenCalled();
  });
});
