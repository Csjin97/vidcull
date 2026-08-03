
import { fireEvent, render, screen } from "@testing-library/svelte";
import { beforeEach, describe, expect, it, vi } from "vitest";
import BoundaryHarness from "./boundary-test-harness.fixture.svelte";

vi.mock("$lib/stores/errorStore.svelte", () => ({
  errorStore: {
    reportError: vi.fn(),
    dismiss: vi.fn(),
    dismissAll: vi.fn(),
    errors: [],
  },
}));
import { errorStore } from "$lib/stores/errorStore.svelte";

beforeEach(() => {
  vi.mocked(errorStore.reportError).mockClear();
});

describe("svelte:boundary — onerror / errorStore integration (+layout.svelte pattern)", () => {
  it("renders child content normally when no error is thrown", () => {
    render(BoundaryHarness, { props: { shouldThrow: false } });
    expect(screen.getByTestId("boundary-content")).toBeInTheDocument();
    expect(screen.queryByTestId("boundary-fallback")).not.toBeInTheDocument();
    expect(errorStore.reportError).not.toHaveBeenCalled();
  });

  it("shows the failed fallback and hides normal content when a child throws", () => {
    render(BoundaryHarness, { props: { shouldThrow: true } });
    expect(screen.getByTestId("boundary-fallback")).toBeInTheDocument();
    expect(screen.queryByTestId("boundary-content")).not.toBeInTheDocument();
  });

  it("calls errorStore.reportError exactly once when the boundary catches a render error", () => {
    render(BoundaryHarness, { props: { shouldThrow: true } });
    expect(errorStore.reportError).toHaveBeenCalledTimes(1);
    expect(errorStore.reportError).toHaveBeenCalledWith(expect.any(Error));
  });

  it("does not double-report — onerror fires once, failed snippet body does not call reportError", () => {
    render(BoundaryHarness, { props: { shouldThrow: true } });
    expect(errorStore.reportError).toHaveBeenCalledTimes(1);
  });

  it("exposes a 다시 시도 reset button in the failed fallback", async () => {
    render(BoundaryHarness, { props: { shouldThrow: true } });
    const resetBtn = screen.getByRole("button", { name: "다시 시도" });
    expect(resetBtn).toBeInTheDocument();
    await fireEvent.click(resetBtn);
  });
});
