import { fireEvent, render, screen } from "@testing-library/svelte";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { IpcValidationError } from "../ipc/validate";
import { errorStore } from "../stores/errorStore.svelte";
import ErrorToast from "./ErrorToast.svelte";

const writeText = vi.fn().mockResolvedValue(undefined);
Object.defineProperty(navigator, "clipboard", {
  value: { writeText },
  writable: true,
  configurable: true,
});

beforeEach(() => {
  errorStore.dismissAll();
  writeText.mockClear();
});

describe("ErrorToast", () => {

  it("renders nothing when the error store is empty", () => {
    const { container } = render(ErrorToast);
    expect(container.querySelector(".toast-stack")).toBeNull();
  });

  it("renders a Korean error message from the store", () => {
    errorStore.reportError(new Error("파일 처리 오류가 발생했습니다."));
    render(ErrorToast);
    expect(
      screen.getByText("파일 처리 오류가 발생했습니다."),
    ).toBeInTheDocument();
  });

  it("renders multiple toasts for multiple entries", () => {
    errorStore.reportError(new Error("오류 A"));
    errorStore.reportError(new Error("오류 B"));
    render(ErrorToast);
    expect(screen.getByText("오류 A")).toBeInTheDocument();
    expect(screen.getByText("오류 B")).toBeInTheDocument();
  });


  it("has role='alert' and aria-live='assertive' on the container", () => {
    errorStore.reportError(new Error("접근성 오류"));
    render(ErrorToast);
    const alert = screen.getByRole("alert");
    expect(alert).toHaveAttribute("aria-live", "assertive");
  });


  it("applies toast--error class for error severity", () => {
    errorStore.reportError(new Error("심각한 오류"));
    const { container } = render(ErrorToast);
    expect(container.querySelector(".toast--error")).not.toBeNull();
  });

  it("applies toast--warning class for warning severity (IpcValidationError)", () => {
    errorStore.reportError(
      new IpcValidationError("get_settings", "scan_folders", null),
    );
    const { container } = render(ErrorToast);
    expect(container.querySelector(".toast--warning")).not.toBeNull();
  });


  it("shows ×N badge when the same error was reported multiple times", () => {
    errorStore.reportError(new Error("반복 오류"));
    errorStore.reportError(new Error("반복 오류"));
    errorStore.reportError(new Error("반복 오류"));
    render(ErrorToast);
    expect(screen.getByText("×3")).toBeInTheDocument();
  });

  it("does not show a count badge when count is 1", () => {
    errorStore.reportError(new Error("단발 오류"));
    render(ErrorToast);
    expect(screen.queryByText(/^×\d/)).toBeNull();
  });


  it("removes the toast from the DOM when the dismiss button is clicked", async () => {
    errorStore.reportError(new Error("닫을 오류"));
    render(ErrorToast);
    expect(screen.getByText("닫을 오류")).toBeInTheDocument();

    const dismissBtn = screen.getByLabelText("오류 알림 닫기");
    await fireEvent.click(dismissBtn);

    expect(screen.queryByText("닫을 오류")).not.toBeInTheDocument();
  });

  it("only dismisses the targeted toast when multiple exist", async () => {
    errorStore.reportError(new Error("오류 1"));
    errorStore.reportError(new Error("오류 2"));
    render(ErrorToast);

    const [firstDismiss] = screen.getAllByLabelText("오류 알림 닫기");
    await fireEvent.click(firstDismiss);

    expect(screen.queryByText("오류 1")).not.toBeInTheDocument();
    expect(screen.getByText("오류 2")).toBeInTheDocument();
  });


  it("calls navigator.clipboard.writeText with the error message when '로그 복사' is clicked", async () => {
    errorStore.reportError(new Error("복사할 오류 메시지"));
    render(ErrorToast);

    const copyBtn = screen.getByLabelText("오류 메시지 클립보드에 복사");
    await fireEvent.click(copyBtn);

    expect(writeText).toHaveBeenCalledOnce();
    expect(writeText).toHaveBeenCalledWith("복사할 오류 메시지");
  });


  it("does not expose raw Windows file paths in the rendered UI", () => {
    errorStore.reportError(
      new Error("처리 실패: C:\\Users\\user\\videos\\clip.mp4"),
    );
    render(ErrorToast);
    const alert = screen.getByRole("alert");
    expect(alert.textContent).not.toContain("C:\\Users\\user\\");
    expect(alert.textContent).toContain("clip.mp4");
  });

  it("does not expose raw POSIX file paths in the rendered UI", () => {
    errorStore.reportError(
      new Error("오류: /home/user/videos/test.mp4"),
    );
    render(ErrorToast);
    const alert = screen.getByRole("alert");
    expect(alert.textContent).not.toContain("/home/user/");
    expect(alert.textContent).toContain("test.mp4");
  });
});
