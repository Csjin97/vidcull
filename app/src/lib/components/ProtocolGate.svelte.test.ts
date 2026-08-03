import { render, screen } from "@testing-library/svelte";
import { describe, expect, it } from "vitest";
import ProtocolGate from "./ProtocolGate.svelte";
import { EXPECTED_PROTOCOL_VERSION, interpretPong } from "../../daemon";

describe("ProtocolGate", () => {
  it("renders nothing for a compatible daemon", () => {
    const { container } = render(ProtocolGate, {
      props: { ping: interpretPong(EXPECTED_PROTOCOL_VERSION) },
    });
    expect(container.querySelector(".gate")).toBeNull();
  });

  it("renders nothing while the daemon is offline or not yet pinged", () => {
    const { container } = render(ProtocolGate, {
      props: { ping: { ok: false, error: "connection refused" } },
    });
    expect(container.querySelector(".gate")).toBeNull();
  });

  it("blocks the window with versions and guidance on a mismatch", () => {
    render(ProtocolGate, {
      props: { ping: interpretPong(EXPECTED_PROTOCOL_VERSION + 1) },
    });
    const dialog = screen.getByRole("alertdialog");
    expect(dialog).toHaveAttribute("aria-modal", "true");
    expect(screen.getByText("프로토콜 버전 불일치")).toBeInTheDocument();
    expect(
      screen.getByText(`v${EXPECTED_PROTOCOL_VERSION + 1}`),
    ).toBeInTheDocument();
    expect(
      screen.getByText(`v${EXPECTED_PROTOCOL_VERSION}`),
    ).toBeInTheDocument();
    expect(
      screen.getByText(/같은 버전으로 업데이트/),
    ).toBeInTheDocument();
  });

  it("lifts the gate when a later ping reports matching versions", async () => {
    const { container, rerender } = render(ProtocolGate, {
      props: { ping: interpretPong(EXPECTED_PROTOCOL_VERSION + 1) },
    });
    expect(container.querySelector(".gate")).not.toBeNull();
    await rerender({ ping: interpretPong(EXPECTED_PROTOCOL_VERSION) });
    expect(container.querySelector(".gate")).toBeNull();
  });
});
