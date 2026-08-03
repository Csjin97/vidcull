import { render, screen, waitFor } from "@testing-library/svelte";
import { afterEach, describe, expect, it, vi } from "vitest";
import Thumbnail, { clearThumbnailCache } from "./Thumbnail.svelte";

// Every test below reuses fileId: 1 against the module-level fetch cache —
// clear it between tests so they stay independent of each other.
afterEach(() => {
  clearThumbnailCache();
});

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((res) => {
    resolve = res;
  });
  return { promise, resolve };
}

describe("Thumbnail lazy-fetch loading state", () => {
  it("shows a pending indicator (not the bare placeholder) while fetchThumbnail is in flight", async () => {
    const { promise } = deferred<string | null>();
    const fetchThumbnail = vi.fn(() => promise);
    const { container } = render(Thumbnail, {
      props: { src: null, alt: "video.mp4", fileId: 1, fetchThumbnail },
    });

    await waitFor(() => expect(screen.getByText("준비 중")).toBeInTheDocument());
    expect(screen.queryByText("▶")).not.toBeInTheDocument();
    expect(container.querySelector("img")).not.toBeInTheDocument();
  });

  it("falls back to the bare placeholder once the fetch resolves with no thumbnail", async () => {
    const { promise, resolve } = deferred<string | null>();
    const fetchThumbnail = vi.fn(() => promise);
    render(Thumbnail, {
      props: { src: null, alt: "video.mp4", fileId: 1, fetchThumbnail },
    });

    await waitFor(() => expect(screen.getByText("준비 중")).toBeInTheDocument());
    resolve(null);

    await waitFor(() => expect(screen.getByText("▶")).toBeInTheDocument());
    expect(screen.queryByText("준비 중")).not.toBeInTheDocument();
  });

  it("renders the resolved thumbnail as an <img>, with no placeholder left", async () => {
    const { promise, resolve } = deferred<string | null>();
    const fetchThumbnail = vi.fn(() => promise);
    const { container } = render(Thumbnail, {
      props: { src: null, alt: "video.mp4", fileId: 1, fetchThumbnail },
    });

    await waitFor(() => expect(screen.getByText("준비 중")).toBeInTheDocument());
    resolve("data:image/svg+xml;utf8,<svg/>");

    await waitFor(() => expect(container.querySelector("img")).toBeInTheDocument());
    expect(screen.queryByText("준비 중")).not.toBeInTheDocument();
    expect(screen.queryByText("▶")).not.toBeInTheDocument();
    expect(container.querySelector(".thumb__placeholder")).not.toBeInTheDocument();
  });

  it("renders the eagerly-known thumbnail immediately when `src` is given (no fetch)", () => {
    const fetchThumbnail = vi.fn();
    const { container } = render(Thumbnail, {
      props: { src: "data:image/svg+xml;utf8,<svg/>", alt: "video.mp4" },
    });

    expect(container.querySelector("img")).toBeInTheDocument();
    expect(fetchThumbnail).not.toHaveBeenCalled();
    expect(screen.queryByText("준비 중")).not.toBeInTheDocument();
  });
});
